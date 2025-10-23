/// ! High-level blob engine orchestration
use async_trait::async_trait;
use tracing::{debug, info};
use ultramarine_types::{height::Height, proposal_part::BlobSidecar};

use crate::{error::BlobEngineError, store::BlobStore, verifier::BlobVerifier};

/// Blob lifecycle management: verification, storage, and archival
///
/// BlobEngine coordinates all blob-related operations:
/// - Cryptographic verification (KZG proofs)
/// - Persistent storage (undecided → decided → archived)
/// - Lifecycle management (pruning, cleanup)
///
/// ## Usage
///
/// ```no_run
/// use ultramarine_blob_engine::{BlobEngine, BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create store and engine
/// let store = RocksDbBlobStore::open("./blob_data")?;
/// let engine = BlobEngineImpl::new(store)?;
///
/// // Verify and store blobs from a proposal
/// engine.verify_and_store(height, round, &sidecars).await?;
///
/// // When block is decided
/// engine.mark_decided(height, round).await?;
///
/// // Retrieve for execution layer
/// let blobs = engine.get_for_import(height).await?;
/// # Ok(())
/// # }
/// ```
#[async_trait]
pub trait BlobEngine: Send + Sync {
    /// Verify KZG proofs and store blobs for an undecided proposal
    ///
    /// This method:
    /// 1. Verifies all KZG proofs using batch verification (fast)
    /// 2. Stores verified blobs in undecided state
    ///
    /// If verification fails, no blobs are stored and an error is returned.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `round` - Consensus round
    /// * `sidecars` - Blob sidecars to verify and store
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - KZG verification fails (invalid proofs)
    /// - Storage operation fails
    async fn verify_and_store(
        &self,
        height: Height,
        round: i64,
        sidecars: &[BlobSidecar],
    ) -> Result<(), BlobEngineError>;

    /// Promote blobs from undecided to decided state
    ///
    /// Called when a block is finalized by consensus.
    /// Moves blobs from (height, round) key to height-only key.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `round` - The round that was decided
    async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError>;

    /// Retrieve decided blobs for execution layer import
    ///
    /// Returns all blobs for a finalized height, sorted by index.
    /// Used when submitting the block to the execution layer.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height to retrieve
    ///
    /// # Returns
    ///
    /// Vector of blob sidecars, sorted by index (0, 1, 2, ...)
    async fn get_for_import(&self, height: Height) -> Result<Vec<BlobSidecar>, BlobEngineError>;

    /// Remove all blobs for a failed or timed-out round
    ///
    /// Called when a round fails or is superseded by a later round.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `round` - Round to drop
    async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError>;

    /// Delete blobs after successful archival
    ///
    /// Called by the archiver after blobs have been persisted to
    /// long-term storage (e.g., S3, IPFS).
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `indices` - Blob indices to delete
    async fn mark_archived(&self, height: Height, indices: &[u8]) -> Result<(), BlobEngineError>;

    /// Prune all decided blobs before a given height
    ///
    /// Used for cleanup after finalization or archival.
    ///
    /// # Arguments
    ///
    /// * `height` - Delete all blobs with height < this value
    ///
    /// # Returns
    ///
    /// Number of blobs deleted
    async fn prune_archived_before(&self, height: Height) -> Result<usize, BlobEngineError>;
}

/// Default blob engine implementation
///
/// Uses RocksDB for storage and c-kzg for verification.
/// Generic over storage backend to allow testing with in-memory stores.
pub struct BlobEngineImpl<S>
where
    S: BlobStore,
{
    verifier: BlobVerifier,
    store: S,
}

impl<S> BlobEngineImpl<S>
where
    S: BlobStore,
{
    /// Create a new blob engine with the given storage backend
    ///
    /// Initializes the KZG verifier with the Ethereum mainnet trusted setup.
    ///
    /// # Arguments
    ///
    /// * `store` - Storage backend (e.g., RocksDbBlobStore)
    ///
    /// # Errors
    ///
    /// Returns an error if the trusted setup cannot be loaded.
    pub fn new(store: S) -> Result<Self, BlobEngineError> {
        let verifier = BlobVerifier::new().map_err(|e| {
            BlobEngineError::InvalidConfig(format!("Failed to initialize KZG verifier: {}", e))
        })?;

        Ok(Self { verifier, store })
    }
}

#[async_trait]
impl<S> BlobEngine for BlobEngineImpl<S>
where
    S: BlobStore,
{
    async fn verify_and_store(
        &self,
        height: Height,
        round: i64,
        sidecars: &[BlobSidecar],
    ) -> Result<(), BlobEngineError> {
        if sidecars.is_empty() {
            debug!("No blobs to verify and store");
            return Ok(());
        }

        // Step 1: Verify KZG proofs (security gate)
        debug!(
            height = height.as_u64(),
            round = round,
            count = sidecars.len(),
            "Verifying KZG proofs"
        );

        let sidecar_refs: Vec<&BlobSidecar> = sidecars.iter().collect();
        self.verifier.verify_blob_sidecars_batch(&sidecar_refs).map_err(|verification_error| {
            // Extract the actual failing blob index from the verification error
            // This is critical for debugging - we need to report which blob failed,
            // not just assume it's the first one
            use crate::verifier::BlobVerificationError;
            let index = match &verification_error {
                // InvalidProofValue contains the actual failing blob index
                BlobVerificationError::InvalidProofValue(idx) => *idx,
                // For other errors that don't contain index info, use first blob as hint
                _ => sidecars.first().map(|s| s.index).unwrap_or(0),
            };
            BlobEngineError::VerificationFailed { height, index, source: verification_error }
        })?;

        info!(
            height = height.as_u64(),
            round = round,
            count = sidecars.len(),
            "✅ KZG verification passed"
        );

        // Step 2: Store verified blobs
        self.store.put_undecided_blobs(height, round, sidecars).await?;

        info!(
            height = height.as_u64(),
            round = round,
            count = sidecars.len(),
            "Stored verified blobs"
        );

        Ok(())
    }

    async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
        self.store.mark_decided(height, round).await?;

        info!(height = height.as_u64(), round = round, "Marked blobs as decided");

        Ok(())
    }

    async fn get_for_import(&self, height: Height) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        let blobs = self.store.get_decided_blobs(height).await?;

        debug!(height = height.as_u64(), count = blobs.len(), "Retrieved blobs for import");

        Ok(blobs)
    }

    async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
        self.store.drop_round(height, round).await?;

        debug!(height = height.as_u64(), round = round, "Dropped blobs for round");

        Ok(())
    }

    async fn mark_archived(&self, height: Height, indices: &[u8]) -> Result<(), BlobEngineError> {
        self.store.delete_archived(height, indices).await?;

        debug!(height = height.as_u64(), count = indices.len(), "Marked blobs as archived");

        Ok(())
    }

    async fn prune_archived_before(&self, height: Height) -> Result<usize, BlobEngineError> {
        let count = self.store.prune_before(height).await?;

        info!(before_height = height.as_u64(), count = count, "Pruned archived blobs");

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use ultramarine_types::{
        aliases::Bytes,
        blob::{Blob, KzgCommitment, KzgProof},
    };

    use super::*;
    use crate::store::rocksdb::RocksDbBlobStore;

    fn create_test_blob(index: u8) -> BlobSidecar {
        let blob_data = vec![index; 131_072];
        let blob = Blob::new(Bytes::from(blob_data)).unwrap();
        let commitment = KzgCommitment([index; 48]);
        let proof = KzgProof([index; 48]);
        BlobSidecar::new(index, blob, commitment, proof)
    }

    #[tokio::test]
    async fn test_engine_initialization() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();
        let engine = BlobEngineImpl::new(store);

        assert!(engine.is_ok(), "Failed to initialize blob engine");
    }

    #[tokio::test]
    #[ignore] // Requires valid KZG proofs
    async fn test_verify_and_store_flow() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();
        let engine = BlobEngineImpl::new(store).unwrap();

        let height = Height::new(100);
        let round = 1;

        // These are dummy blobs - would fail real KZG verification
        let blobs = vec![create_test_blob(0), create_test_blob(1)];

        // This will fail with invalid proof since we're using dummy data
        let result = engine.verify_and_store(height, round, &blobs).await;
        assert!(result.is_err(), "Dummy blobs should fail verification");
    }

    #[tokio::test]
    async fn test_lifecycle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        let height = Height::new(100);
        let round = 1;
        let blobs = vec![create_test_blob(0)];

        // Store directly (bypassing verification for test)
        store.put_undecided_blobs(height, round, &blobs).await.unwrap();

        // Initialize engine
        let engine = BlobEngineImpl::new(store).unwrap();

        // Mark as decided
        engine.mark_decided(height, round).await.unwrap();

        // Retrieve for import
        let imported = engine.get_for_import(height).await.unwrap();
        assert_eq!(imported.len(), 1);

        // Archive and prune
        engine.mark_archived(height, &[0]).await.unwrap();
        let imported_after = engine.get_for_import(height).await.unwrap();
        assert_eq!(imported_after.len(), 0);
    }
}
