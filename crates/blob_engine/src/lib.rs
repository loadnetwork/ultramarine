//! Blob engine for EIP-4844 blob sidecar lifecycle management
//!
//! This crate provides a complete blob lifecycle system:
//! - **Verification**: KZG proof verification using c-kzg
//! - **Storage**: Persistent storage with RocksDB backend
//! - **Lifecycle**: State transitions (undecided → decided → archived)
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                      BlobEngine                         │
//! │  (High-level orchestration & verification)              │
//! └──────────────────┬──────────────────────────────────────┘
//!                    │
//!          ┌─────────┴──────────┐
//!          │                    │
//!    ┌─────▼──────┐     ┌──────▼────────┐
//!    │ BlobStore  │     │ BlobVerifier  │
//!    │  (trait)   │     │  (KZG crypto) │
//!    └─────┬──────┘     └───────────────┘
//!          │
//!    ┌─────▼────────────┐
//!    │ RocksDbBlobStore │
//!    │ (persistence)    │
//!    └──────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use ultramarine_blob_engine::{BlobEngine, BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
//! use ultramarine_types::height::Height;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize storage and engine
//! let store = RocksDbBlobStore::open("./blob_data")?;
//! let metrics = ultramarine_blob_engine::BlobEngineMetrics::new();
//! let engine = BlobEngineImpl::new(store, metrics)?;
//! #
//! # let height = Height::new(100);
//! # let round = 1;
//! # let sidecars = vec![];
//! # let old_height = Height::new(50);
//!
//! // During proposal handling:
//! // 1. Verify and store blobs
//! engine.verify_and_store(height, round, &sidecars).await?;
//!
//! // 2. When block is decided:
//! engine.mark_decided(height, round).await?;
//!
//! // 3. Retrieve for execution layer:
//! let blobs = engine.get_for_import(height).await?;
//!
//! // 4. After archival:
//! engine.mark_archived(height, &[0, 1, 2]).await?;
//!
//! // 5. Cleanup old blobs:
//! let pruned = engine.prune_archived_before(old_height).await?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

/// High-level blob engine orchestration.
pub mod engine;
/// Error types for the blob engine.
pub mod error;
/// Prometheus metrics for blob lifecycle operations.
pub mod metrics;
/// Persistent storage backends and blob storage abstractions.
pub mod store;
mod verifier;

// Re-export main types
pub use engine::{BlobEngine, BlobEngineImpl};
pub use error::{BlobEngineError, BlobStoreError, BlobVerificationError};
pub use metrics::BlobEngineMetrics;
pub use store::{BlobKey, BlobStore};
