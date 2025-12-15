/// ! High-level blob engine orchestration
use async_trait::async_trait;
use tracing::{debug, info, warn};
use ultramarine_types::{blob::BYTES_PER_BLOB, height::Height, proposal_part::BlobSidecar};

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
/// use ultramarine_blob_engine::{
///     BlobEngine, BlobEngineImpl, BlobEngineMetrics, store::rocksdb::RocksDbBlobStore,
/// };
/// use ultramarine_types::height::Height;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create store and engine
/// let store = RocksDbBlobStore::open("./blob_data")?;
/// let metrics = BlobEngineMetrics::new();
/// let engine = BlobEngineImpl::new(store, metrics)?;
/// #
/// # let height = Height::new(100);
/// # let round = 1;
/// # let sidecars = vec![];
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
    async fn mark_archived(&self, height: Height, indices: &[u16]) -> Result<(), BlobEngineError>;

    /// Prune all decided blobs before a given height.
    ///
    /// **⚠️ WARNING**: This method performs height-only deletion without checking
    /// archive status. Callers MUST ensure all blobs at heights being pruned have
    /// been successfully archived before calling this method. For production use,
    /// prefer `State::prune_archived_height` which performs proper archive verification.
    ///
    /// This method is intended for CLI/manual cleanup operations where the caller
    /// has already verified archive status through `BlobMetadata`.
    ///
    /// # Arguments
    ///
    /// * `height` - Delete all blobs with height < this value
    ///
    /// # Returns
    ///
    /// Number of blobs deleted
    ///
    /// # Safety
    ///
    /// Calling this method without verifying archive status will result in
    /// permanent data loss for un-archived blobs.
    async fn prune_archived_before(&self, height: Height) -> Result<usize, BlobEngineError>;

    /// Retrieve undecided blobs for a specific (height, round)
    ///
    /// Used for restreaming proposals where we need to send the original blob sidecars.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height
    /// * `round` - Consensus round
    ///
    /// # Returns
    ///
    /// Vector of blob sidecars, sorted by index
    async fn get_undecided_blobs(
        &self,
        height: Height,
        round: i64,
    ) -> Result<Vec<BlobSidecar>, BlobEngineError>;
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
    metrics: crate::metrics::BlobEngineMetrics,
}

impl<S> BlobEngineImpl<S>
where
    S: BlobStore,
{
    /// Create a new blob engine with the given storage backend and metrics
    ///
    /// Initializes the KZG verifier with the Ethereum mainnet trusted setup.
    ///
    /// # Arguments
    ///
    /// * `store` - Storage backend (e.g., RocksDbBlobStore)
    /// * `metrics` - Prometheus metrics for observability
    ///
    /// # Errors
    ///
    /// Returns an error if the trusted setup cannot be loaded.
    pub fn new(
        store: S,
        metrics: crate::metrics::BlobEngineMetrics,
    ) -> Result<Self, BlobEngineError> {
        let verifier = BlobVerifier::new().map_err(|e| {
            BlobEngineError::InvalidConfig(format!("Failed to initialize KZG verifier: {}", e))
        })?;

        Ok(Self { verifier, store, metrics })
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

        // Step 1: Verify KZG proofs (security gate) - with timing
        debug!(
            height = height.as_u64(),
            round = round,
            count = sidecars.len(),
            "Verifying KZG proofs"
        );

        let start = std::time::Instant::now();
        let sidecar_refs: Vec<&BlobSidecar> = sidecars.iter().collect();
        let verification_result = self.verifier.verify_blob_sidecars_batch(&sidecar_refs);
        let duration = start.elapsed();

        // Record verification metrics
        self.metrics.observe_verification_time(duration);

        verification_result.map_err(|verification_error| {
            // Record failure
            self.metrics.record_verifications_failure(sidecars.len());

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

        // Record successful verification
        self.metrics.record_verifications_success(sidecars.len());

        info!(
            height = height.as_u64(),
            round = round,
            count = sidecars.len(),
            duration_ms = duration.as_millis(),
            "✅ KZG verification passed"
        );

        // Step 2: Store verified blobs
        let stored_count = self.store.put_undecided_blobs(height, round, sidecars).await?;

        // Calculate total bytes
        let total_bytes: usize = sidecars.iter().map(|s| s.blob.size()).sum();

        // Update storage metrics
        self.metrics.add_undecided_storage(total_bytes, stored_count);

        info!(
            height = height.as_u64(),
            round = round,
            count = stored_count,
            bytes = total_bytes,
            "Stored verified blobs"
        );

        Ok(())
    }

    async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
        let (blob_count, total_bytes) = self.store.mark_decided(height, round).await?;

        if blob_count == 0 {
            // No blobs were moved in this call. This can happen if:
            // 1. The block legitimately had zero blobs.
            // 2. mark_decided() was invoked twice (e.g. proposer + commit path).
            //
            // In both cases we want the gauge to reflect the number of blobs currently
            // available for import at this height, but we must not mutate the storage
            // or counters again.
            let decided = self.store.get_decided_blobs(height).await?;
            let decided_count = decided.len();
            self.metrics.set_blobs_per_block(decided_count);
            debug!(
                height = height.as_u64(),
                round = round,
                "mark_decided called with no pending blobs; gauge updated to decided count {decided_count}"
            );
            return Ok(());
        }

        // Update metrics: promote blobs from undecided to decided
        self.metrics.promote_blobs(total_bytes, blob_count);

        // Set gauge for blobs in this finalized block
        self.metrics.set_blobs_per_block(blob_count);

        info!(
            height = height.as_u64(),
            round = round,
            count = blob_count,
            bytes = total_bytes,
            "Marked blobs as decided"
        );

        Ok(())
    }

    async fn get_for_import(&self, height: Height) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        let blobs = self.store.get_decided_blobs(height).await?;

        debug!(height = height.as_u64(), count = blobs.len(), "Retrieved blobs for import");

        Ok(blobs)
    }

    async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
        let (blob_count, total_bytes) = self.store.drop_round(height, round).await?;

        // Update metrics: drop undecided blobs
        self.metrics.drop_blobs(total_bytes, blob_count);

        debug!(
            height = height.as_u64(),
            round = round,
            count = blob_count,
            bytes = total_bytes,
            "Dropped blobs for round"
        );

        Ok(())
    }

    async fn mark_archived(&self, height: Height, indices: &[u16]) -> Result<(), BlobEngineError> {
        let blob_count = indices.len();
        let total_bytes = blob_count * BYTES_PER_BLOB;

        self.store.delete_archived(height, indices).await?;

        // Update metrics: prune decided blobs
        self.metrics.prune_blobs(total_bytes, blob_count);

        debug!(
            height = height.as_u64(),
            count = blob_count,
            bytes = total_bytes,
            "Marked blobs as archived"
        );

        Ok(())
    }

    async fn prune_archived_before(&self, height: Height) -> Result<usize, BlobEngineError> {
        // WARNING: This method does NOT check archive status - caller must verify
        warn!(
            before_height = height.as_u64(),
            "⚠️  prune_archived_before called - ensure all blobs are archived first!"
        );

        let count = self.store.prune_before(height).await?;

        // Estimate bytes (each blob is BYTES_PER_BLOB)
        // TODO: Update store API to return actual bytes if needed
        let total_bytes = count * BYTES_PER_BLOB;

        // Update metrics: prune decided blobs
        self.metrics.prune_blobs(total_bytes, count);

        info!(
            before_height = height.as_u64(),
            count = count,
            bytes = total_bytes,
            "Pruned archived blobs"
        );

        Ok(count)
    }

    async fn get_undecided_blobs(
        &self,
        height: Height,
        round: i64,
    ) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        let blobs = self.store.get_undecided_blobs(height, round).await?;

        debug!(
            height = height.as_u64(),
            round = round,
            count = blobs.len(),
            "Retrieved undecided blobs"
        );

        Ok(blobs)
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

    fn create_test_blob(index: u16) -> BlobSidecar {
        let byte = index as u8;
        let blob_data = vec![byte; 131_072];
        let blob = Blob::new(Bytes::from(blob_data)).unwrap();
        let commitment = KzgCommitment([byte; 48]);
        let proof = KzgProof([byte; 48]);
        BlobSidecar::from_bundle_item(index, blob, commitment, proof)
    }

    #[tokio::test]
    async fn test_engine_initialization() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();
        let metrics = crate::metrics::BlobEngineMetrics::new();
        let engine = BlobEngineImpl::new(store, metrics);

        assert!(engine.is_ok(), "Failed to initialize blob engine");
    }

    #[tokio::test]
    #[ignore] // Requires valid KZG proofs
    async fn test_verify_and_store_flow() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();
        let metrics = crate::metrics::BlobEngineMetrics::new();
        let engine = BlobEngineImpl::new(store, metrics).unwrap();

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
        let metrics = crate::metrics::BlobEngineMetrics::new();
        let engine = BlobEngineImpl::new(store, metrics).unwrap();

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

    #[tokio::test]
    async fn storing_and_deciding_with_different_rounds_loses_blobs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        let height = Height::new(123);
        let proposed_round = 0;
        let commit_round = 3;
        let blobs = vec![create_test_blob(0)];

        store.put_undecided_blobs(height, proposed_round, &blobs).await.unwrap();

        let metrics = crate::metrics::BlobEngineMetrics::new();
        let engine = BlobEngineImpl::new(store, metrics).unwrap();

        engine.mark_decided(height, commit_round).await.unwrap();

        let decided = engine.get_for_import(height).await.unwrap();
        assert_eq!(
            decided.len(),
            blobs.len(),
            "Blobs stored under round {} were not promoted when mark_decided was called with round {}",
            proposed_round,
            commit_round
        );
    }

    #[tokio::test]
    async fn mark_decided_is_idempotent_for_metrics() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        let height = Height::new(777);
        let round = 4;
        let blobs = vec![create_test_blob(0)];

        store.put_undecided_blobs(height, round, &blobs).await.unwrap();

        let metrics = crate::metrics::BlobEngineMetrics::new();
        let engine = BlobEngineImpl::new(store, metrics.clone()).unwrap();

        // First promotion updates metrics and gauge.
        engine.mark_decided(height, round).await.unwrap();
        let first_snapshot = metrics.snapshot();
        assert_eq!(first_snapshot.blobs_per_block, 1);
        assert_eq!(first_snapshot.lifecycle_promoted, 1);

        // Second promotion should be a no-op for counters/gauges.
        engine.mark_decided(height, round).await.unwrap();
        let second_snapshot = metrics.snapshot();
        assert_eq!(
            second_snapshot.blobs_per_block, 1,
            "Gauge should remain stable after idempotent promotion"
        );
        assert_eq!(
            second_snapshot.lifecycle_promoted, 1,
            "Promotion counter should not increment on duplicate call"
        );
    }
}
