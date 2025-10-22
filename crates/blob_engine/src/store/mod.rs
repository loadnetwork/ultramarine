/// ! Blob storage abstraction and implementations
use async_trait::async_trait;
use ultramarine_types::{height::Height, proposal_part::BlobSidecar};

use crate::error::BlobStoreError;

pub mod rocksdb;

/// Key for identifying a blob in storage
///
/// Blobs are uniquely identified by (height, round, index).
/// During consensus, multiple rounds may propose different blobs for the same height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobKey {
    pub height: Height,
    pub round: i64, // Round can be negative (Nil = -1)
    pub index: u8,
}

impl BlobKey {
    /// Create a new blob key
    pub fn new(height: Height, round: i64, index: u8) -> Self {
        Self { height, round, index }
    }

    /// Encode key for undecided blobs: [height: u64 BE][round: i64 BE][index: u8]
    pub fn to_undecided_key(&self) -> Vec<u8> {
        let mut key = Vec::with_capacity(17);
        key.extend_from_slice(&self.height.as_u64().to_be_bytes());
        key.extend_from_slice(&self.round.to_be_bytes());
        key.push(self.index);
        key
    }

    /// Encode key for decided blobs: [height: u64 BE][index: u8]
    pub fn to_decided_key(&self) -> Vec<u8> {
        let mut key = Vec::with_capacity(9);
        key.extend_from_slice(&self.height.as_u64().to_be_bytes());
        key.push(self.index);
        key
    }

    /// Decode key from undecided format
    pub fn from_undecided_key(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 17 {
            return None;
        }

        let height = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
        let round = i64::from_be_bytes(bytes[8..16].try_into().ok()?);
        let index = bytes[16];

        Some(Self { height: Height::new(height), round, index })
    }

    /// Decode key from decided format
    pub fn from_decided_key(bytes: &[u8], round: i64) -> Option<Self> {
        if bytes.len() != 9 {
            return None;
        }

        let height = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
        let index = bytes[8];

        Some(Self { height: Height::new(height), round, index })
    }
}

/// Persistent storage for blob sidecars
///
/// This trait abstracts over different storage backends (RocksDB, in-memory, etc.).
/// All methods are async to allow backends to use spawn_blocking or native async I/O.
#[async_trait]
pub trait BlobStore: Send + Sync + Clone {
    /// Store blobs for an undecided proposal
    ///
    /// These blobs are associated with a specific (height, round) and will be:
    /// - Promoted to decided when the block is finalized
    /// - Deleted when the round fails or times out
    async fn put_undecided_blobs(
        &self,
        height: Height,
        round: i64,
        blobs: &[BlobSidecar],
    ) -> Result<(), BlobStoreError>;

    /// Get all undecided blobs for a specific (height, round)
    async fn get_undecided_blobs(
        &self,
        height: Height,
        round: i64,
    ) -> Result<Vec<BlobSidecar>, BlobStoreError>;

    /// Promote blobs from undecided to decided state
    ///
    /// This is called when a block is finalized. Blobs are moved from
    /// the undecided storage (keyed by height+round) to decided storage
    /// (keyed by height only).
    async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobStoreError>;

    /// Get all decided blobs for a height
    ///
    /// This is used when submitting blocks to the execution layer.
    async fn get_decided_blobs(&self, height: Height) -> Result<Vec<BlobSidecar>, BlobStoreError>;

    /// Delete all blobs for a specific round
    ///
    /// Called when a round fails, times out, or is superseded.
    async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobStoreError>;

    /// Delete specific blobs after successful archival
    ///
    /// Called by the archiver after blobs have been persisted to long-term storage.
    async fn delete_archived(&self, height: Height, indices: &[u8]) -> Result<(), BlobStoreError>;

    /// Prune all decided blobs before a given height
    ///
    /// Returns the number of blobs deleted.
    async fn prune_before(&self, height: Height) -> Result<usize, BlobStoreError>;
}
