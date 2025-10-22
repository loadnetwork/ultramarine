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
        height: Height,
        index: u8,
        #[source]
        source: BlobVerificationError,
    },

    /// Storage layer error
    #[error("Storage error: {0}")]
    Storage(#[from] BlobStoreError),

    /// Blob not found
    #[error("Blob not found: height={height}, round={round}, index={index}")]
    BlobNotFound { height: Height, round: i64, index: u8 },

    /// No blobs found for height
    #[error("No blobs found for height {0}")]
    NoBlobs(Height),

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
}

/// Re-export KZG verification errors from consensus
///
/// This type comes from the blob_verifier module in consensus crate.
/// We re-export it here so users of blob_engine don't need to depend on consensus.
pub use crate::verifier::BlobVerificationError;
