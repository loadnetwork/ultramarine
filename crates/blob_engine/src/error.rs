/// ! Error types for blob engine operations
use thiserror::Error;
use ultramarine_types::height::Height;

/// Errors that can occur during blob storage operations
#[derive(Debug, Error)]
pub enum BlobStoreError {
    /// RocksDB error
    #[error("RocksDB error: {0}")]
    RocksDb(#[from] rocksdb::Error),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Deserialization error
    #[error("Deserialization error: {0}")]
    Deserialization(String),

    /// Column family not found
    #[error("Column family not found: {0}")]
    ColumnFamilyNotFound(String),

    /// Task join error
    #[error("Failed to join spawned task: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

/// Errors that can occur during blob engine operations
#[derive(Debug, Error)]
pub enum BlobEngineError {
    /// KZG verification failed
    #[error("Blob verification failed at height {height}, index {index}: {source}")]
    VerificationFailed {
        /// Height at which verification failed.
        height: Height,
        /// Blob index within the height.
        index: u16,
        #[source]
        /// Underlying verification error.
        source: BlobVerificationError,
    },

    /// Storage layer error
    #[error("Storage error: {0}")]
    Storage(#[from] BlobStoreError),

    /// Blob not found
    #[error("Blob not found: height={height}, round={round}, index={index}")]
    BlobNotFound {
        /// Height of the missing blob.
        height: Height,
        /// Round of the missing blob.
        round: i64,
        /// Blob index within the round.
        index: u16,
    },

    /// No blobs found for height
    #[error("No blobs found for height {0}")]
    NoBlobs(Height),

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// Blobs have been pruned after archival
    ///
    /// This error indicates that blobs for the requested height were archived
    /// and subsequently pruned from local storage. Callers should use the
    /// provided locators to retrieve blobs from the archive provider.
    #[error("Blobs pruned at height {height}: archived to {blob_count} locators")]
    BlobsPruned {
        /// Height at which blobs were pruned
        height: Height,
        /// Number of blobs that were archived
        blob_count: usize,
        /// Archive locators for each blob (indexed by blob_index)
        locators: Vec<String>,
    },
}

/// Re-export KZG verification errors from consensus
///
/// This type comes from the blob_verifier module in consensus crate.
/// We re-export it here so users of blob_engine don't need to depend on consensus.
pub use crate::verifier::BlobVerificationError;
