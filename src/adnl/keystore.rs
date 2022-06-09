use std::collections::hash_map;
use std::sync::Arc;

use anyhow::Result;
use everscale_crypto::ed25519;

use crate::utils::*;

/// Tagged keystore for ADNL keys
#[derive(Default)]
pub struct Keystore {
    keys: FxHashMap<AdnlNodeIdShort, Arc<StoredAdnlNodeKey>>,
    tags: FxHashMap<usize, AdnlNodeIdShort>,
}

impl Keystore {
    pub fn builder() -> KeystoreBuilder {
        KeystoreBuilder::default()
    }

    pub fn key_by_id(
        &self,
        id: &AdnlNodeIdShort,
    ) -> Result<&Arc<StoredAdnlNodeKey>, KeystoreError> {
        if let Some(key) = self.keys.get(id) {
            Ok(key)
        } else {
            Err(KeystoreError::KeyIdNotFound(*id))
        }
    }

    pub fn key_by_tag(&self, tag: usize) -> Result<&Arc<StoredAdnlNodeKey>, KeystoreError> {
        if let Some(id) = self.tags.get(&tag) {
            self.key_by_id(id)
        } else {
            Err(KeystoreError::KeyTagNotFound(tag))
        }
    }

    #[inline(always)]
    pub fn keys(&self) -> &FxHashMap<AdnlNodeIdShort, Arc<StoredAdnlNodeKey>> {
        &self.keys
    }

    pub fn add_key(&mut self, key: [u8; 32], tag: usize) -> Result<AdnlNodeIdShort, KeystoreError> {
        let secret_key = ed25519::SecretKey::from_bytes(key);
        let (full_id, short_id) = secret_key.compute_node_ids();

        match self.tags.entry(tag) {
            hash_map::Entry::Vacant(entry) => {
                entry.insert(short_id);
                match self.keys.entry(short_id) {
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(Arc::new(StoredAdnlNodeKey::from_id_and_private_key(
                            short_id,
                            full_id,
                            &secret_key,
                        )));
                        Ok(short_id)
                    }
                    hash_map::Entry::Occupied(_) => Err(KeystoreError::DuplicatedKey(tag)),
                }
            }
            hash_map::Entry::Occupied(entry) => {
                if entry.get() == &short_id {
                    Ok(short_id)
                } else {
                    Err(KeystoreError::DuplicatedKeyTag(tag))
                }
            }
        }
    }
}

#[derive(Default)]
pub struct KeystoreBuilder {
    keystore: Keystore,
}

impl KeystoreBuilder {
    pub fn build(self) -> Keystore {
        self.keystore
    }

    pub fn with_tagged_key(mut self, key: [u8; 32], tag: usize) -> Result<Self, KeystoreError> {
        self.keystore.add_key(key, tag)?;
        Ok(self)
    }

    /// Creates new keystore from tagged secret keys
    pub fn with_tagged_keys<I>(mut self, keys: I) -> Result<Self, KeystoreError>
    where
        I: IntoIterator<Item = ([u8; 32], usize)>,
    {
        for (key, tag) in keys {
            self.keystore.add_key(key, tag)?;
        }
        Ok(self)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum KeystoreError {
    #[error("Duplicated key tag {0}")]
    DuplicatedKeyTag(usize),
    #[error("Duplicated secret key {0}")]
    DuplicatedKey(usize),
    #[error("Key is not found: {0}")]
    KeyIdNotFound(AdnlNodeIdShort),
    #[error("Key tag not found: {0}")]
    KeyTagNotFound(usize),
    #[error("Unexpected key")]
    UnexpectedKey,
}