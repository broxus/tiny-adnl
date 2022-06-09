use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::transfers_cache::*;
use crate::adnl::AdnlNode;
use crate::proto;
use crate::subscriber::*;
use crate::utils::*;

/// RLDP node configuration
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RldpNodeOptions {
    /// Max allowed RLDP answer size in bytes. Query will be rejected
    /// if answer is bigger.
    ///
    /// Default: `10485760` (10 MB)
    pub max_answer_size: u32,

    /// Max parallel RLDP queries per peer.
    ///
    /// Default: `16`
    pub max_peer_queries: usize,

    /// Min RLDP query timeout.
    ///
    /// Default: `500` ms
    pub query_min_timeout_ms: u64,

    /// Max RLDP query timeout
    ///
    /// Default: `10000` ms
    pub query_max_timeout_ms: u64,

    /// Number of FEC messages to send in group. There will be a short delay between them.
    ///
    /// Default: `20`
    pub query_wave_len: u32,

    /// Interval between FEC broadcast waves.
    ///
    /// Default: `10` ms
    pub query_wave_interval_ms: u64,

    /// Whether requests will be compressed.
    ///
    /// Default: `false`
    pub force_compression: bool,
}

impl Default for RldpNodeOptions {
    fn default() -> Self {
        Self {
            max_answer_size: 10 * 1024 * 1024,
            max_peer_queries: 16,
            query_min_timeout_ms: 500,
            query_max_timeout_ms: 10000,
            query_wave_len: 20,
            query_wave_interval_ms: 10,
            force_compression: false,
        }
    }
}

/// Reliable UDP transport layer
pub struct RldpNode {
    /// Underlying ADNL node
    adnl: Arc<AdnlNode>,
    /// Parallel requests limiter
    semaphores: FxDashMap<AdnlNodeIdShort, Arc<Semaphore>>,
    /// Transfers handler
    transfers: Arc<TransfersCache>,
    /// Configuration
    options: RldpNodeOptions,
}

impl RldpNode {
    /// Create new RLDP node on top of the given ADNL node
    pub fn new(
        adnl: Arc<AdnlNode>,
        subscribers: Vec<Arc<dyn QuerySubscriber>>,
        options: RldpNodeOptions,
    ) -> Result<Arc<Self>> {
        let transfers = Arc::new(TransfersCache::new(subscribers, options));

        adnl.add_message_subscriber(transfers.clone())?;

        Ok(Arc::new(Self {
            adnl,
            semaphores: Default::default(),
            transfers,
            options,
        }))
    }

    /// Underlying ADNL node
    #[inline(always)]
    pub fn adnl(&self) -> &Arc<AdnlNode> {
        &self.adnl
    }

    #[inline(always)]
    pub fn options(&self) -> &RldpNodeOptions {
        &self.options
    }

    pub fn metrics(&self) -> RldpNodeMetrics {
        RldpNodeMetrics {
            peer_count: self.semaphores.len(),
            transfers_cache_len: self.transfers.len(),
        }
    }

    /// Clears semaphores table
    pub fn gc(&self) {
        let max_permits = self.options.max_peer_queries;
        self.semaphores
            .retain(|_, semaphore| semaphore.available_permits() < max_permits);
    }

    #[tracing::instrument(level = "debug", name = "rldp_query", skip(self, data))]
    pub async fn query(
        &self,
        local_id: &AdnlNodeIdShort,
        peer_id: &AdnlNodeIdShort,
        data: Vec<u8>,
        roundtrip: Option<u64>,
    ) -> Result<(Option<Vec<u8>>, u64)> {
        let (query_id, query) = self.make_query(data);

        let peer = self
            .semaphores
            .entry(*peer_id)
            .or_insert_with(|| Arc::new(Semaphore::new(self.options.max_peer_queries)))
            .value()
            .clone();

        let result = {
            let _permit = peer.acquire().await.ok();
            self.transfers
                .query(&self.adnl, local_id, peer_id, query, roundtrip)
                .await
        };

        match result? {
            (Some(answer), roundtrip) => match tl_proto::deserialize(&answer) {
                Ok(proto::rldp::Message::Answer {
                    query_id: answer_id,
                    data,
                }) if answer_id == &query_id => Ok((
                    Some(compression::decompress(data).unwrap_or_else(|| data.to_vec())),
                    roundtrip,
                )),
                Ok(proto::rldp::Message::Answer { .. }) => {
                    Err(RldpNodeError::QueryIdMismatch.into())
                }
                Ok(proto::rldp::Message::Message { .. }) => {
                    Err(RldpNodeError::UnexpectedAnswer("RldpMessageView::Message").into())
                }
                Ok(proto::rldp::Message::Query { .. }) => {
                    Err(RldpNodeError::UnexpectedAnswer("RldpMessageView::Query").into())
                }
                Err(e) => Err(RldpNodeError::InvalidPacketContent(e).into()),
            },
            (None, roundtrip) => Ok((None, roundtrip)),
        }
    }

    fn make_query(&self, mut data: Vec<u8>) -> ([u8; 32], Vec<u8>) {
        use rand::Rng;

        if self.options.force_compression {
            if let Err(e) = compression::compress(&mut data) {
                tracing::warn!("Failed to compress RLDP query: {e:?}");
            }
        }

        // TODO: compress data inplace

        let query_id = rand::thread_rng().gen();
        let data = proto::rldp::Message::Query {
            query_id: &query_id,
            max_answer_size: self.options.max_answer_size as u64,
            timeout: now() + self.options.query_max_timeout_ms as u32 / 1000,
            data: &data,
        };
        (query_id, tl_proto::serialize(data))
    }
}

#[async_trait::async_trait]
impl MessageSubscriber for TransfersCache {
    async fn try_consume_custom<'a>(
        &self,
        ctx: SubscriberContext<'a>,
        constructor: u32,
        data: &'a [u8],
    ) -> Result<bool> {
        if constructor != proto::rldp::MessagePart::TL_ID_MESSAGE_PART
            && constructor != proto::rldp::MessagePart::TL_ID_CONFIRM
            && constructor != proto::rldp::MessagePart::TL_ID_COMPLETE
        {
            return Ok(false);
        }

        let message_part = tl_proto::deserialize(data)?;
        self.handle_message(ctx.adnl, ctx.local_id, ctx.peer_id, message_part)
            .await?;

        Ok(true)
    }
}

/// Instant RLDP node metrics
#[derive(Debug, Copy, Clone)]
pub struct RldpNodeMetrics {
    pub peer_count: usize,
    pub transfers_cache_len: usize,
}

#[derive(thiserror::Error, Debug)]
enum RldpNodeError {
    #[error("Unexpected answer: {0}")]
    UnexpectedAnswer(&'static str),
    #[error("Invalid packet content: {0:?}")]
    InvalidPacketContent(tl_proto::TlError),
    #[error("Unknown query id")]
    QueryIdMismatch,
}