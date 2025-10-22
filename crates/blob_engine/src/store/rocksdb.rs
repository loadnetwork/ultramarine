/// ! RocksDB implementation of BlobStore
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rocksdb::{ColumnFamilyDescriptor, DB, Options};
use tracing::{debug, warn};
use ultramarine_types::{height::Height, proposal_part::BlobSidecar};

use super::{BlobKey, BlobStore};
use crate::error::BlobStoreError;

/// Column family for undecided blobs (includes round in key)
const CF_UNDECIDED: &str = "undecided_blobs";

/// Column family for decided blobs (height-only keys)
const CF_DECIDED: &str = "decided_blobs";

/// RocksDB-backed blob storage
///
/// Uses two column families:
/// - `undecided_blobs`: Stores blobs from proposals that haven't been decided yet
/// - `decided_blobs`: Stores blobs from finalized blocks
///
/// This separation allows efficient cleanup of failed rounds while
/// preserving decided blobs for execution layer handoff and archival.
#[derive(Clone)]
pub struct RocksDbBlobStore {
    db: Arc<DB>,
}

impl RocksDbBlobStore {
    /// Open a RocksDB blob store at the given path
    ///
    /// Creates the database and column families if they don't exist.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the RocksDB directory
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or created.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BlobStoreError> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        // Define column families
        let cf_undecided = ColumnFamilyDescriptor::new(CF_UNDECIDED, Options::default());
        let cf_decided = ColumnFamilyDescriptor::new(CF_DECIDED, Options::default());

        let db = DB::open_cf_descriptors(&db_opts, path, vec![cf_undecided, cf_decided])?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Get column family handle, returning error if not found
    fn cf_handle(&self, name: &str) -> Result<&rocksdb::ColumnFamily, BlobStoreError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(name.to_string()))
    }

    /// Serialize a blob sidecar using bincode
    fn serialize_blob(blob: &BlobSidecar) -> Result<Vec<u8>, BlobStoreError> {
        bincode::serialize(blob)
            .map_err(|e| BlobStoreError::Serialization(format!("Failed to serialize blob: {}", e)))
    }

    /// Deserialize a blob sidecar using bincode
    fn deserialize_blob(bytes: &[u8]) -> Result<BlobSidecar, BlobStoreError> {
        bincode::deserialize(bytes).map_err(|e| {
            BlobStoreError::Deserialization(format!("Failed to deserialize blob: {}", e))
        })
    }

    /// Create a key prefix for scanning all blobs at a given height+round
    fn undecided_prefix(height: Height, round: i64) -> Vec<u8> {
        let mut prefix = Vec::with_capacity(16);
        prefix.extend_from_slice(&height.as_u64().to_be_bytes());
        prefix.extend_from_slice(&round.to_be_bytes());
        prefix
    }

    /// Create a key prefix for scanning all decided blobs at a given height
    fn decided_prefix(height: Height) -> Vec<u8> {
        height.as_u64().to_be_bytes().to_vec()
    }
}

#[async_trait]
impl BlobStore for RocksDbBlobStore {
    async fn put_undecided_blobs(
        &self,
        height: Height,
        round: i64,
        blobs: &[BlobSidecar],
    ) -> Result<(), BlobStoreError> {
        let db = self.db.clone();
        let blobs = blobs.to_vec();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_UNDECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_UNDECIDED.to_string()))?;

            for blob in &blobs {
                let key = BlobKey::new(height, round, blob.index).to_undecided_key();
                let value = Self::serialize_blob(blob)?;
                db.put_cf(cf, key, value)?;
            }

            debug!(
                height = height.as_u64(),
                round = round,
                count = blobs.len(),
                "Stored undecided blobs"
            );

            Ok(())
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }

    async fn get_undecided_blobs(
        &self,
        height: Height,
        round: i64,
    ) -> Result<Vec<BlobSidecar>, BlobStoreError> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_UNDECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_UNDECIDED.to_string()))?;

            let prefix = Self::undecided_prefix(height, round);
            let mut blobs = Vec::new();

            let iter = db.prefix_iterator_cf(cf, &prefix);
            for item in iter {
                let (key_bytes, value_bytes) = item?;

                // Verify the key actually matches our prefix (safety check)
                if !key_bytes.starts_with(&prefix) {
                    break;
                }

                let blob = Self::deserialize_blob(&value_bytes)?;
                blobs.push(blob);
            }

            // Sort by index for deterministic ordering
            blobs.sort_by_key(|b| b.index);

            debug!(
                height = height.as_u64(),
                round = round,
                count = blobs.len(),
                "Retrieved undecided blobs"
            );

            Ok(blobs)
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }

    async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobStoreError> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || {
            let cf_undecided = db
                .cf_handle(CF_UNDECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_UNDECIDED.to_string()))?;
            let cf_decided = db
                .cf_handle(CF_DECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_DECIDED.to_string()))?;

            // Read all undecided blobs for this height+round
            // IMPORTANT: Collect all items before deleting to avoid iterator invalidation.
            // RocksDB iterators become invalid when you mutate the CF during iteration.
            // See: https://github.com/facebook/rocksdb/wiki/Iterator
            let prefix = Self::undecided_prefix(height, round);
            let iter = db.prefix_iterator_cf(cf_undecided, &prefix);

            let mut items_to_move = Vec::new();
            for item in iter {
                let (key_bytes, value_bytes) = item?;

                if !key_bytes.starts_with(&prefix) {
                    break;
                }

                items_to_move.push((key_bytes.to_vec(), value_bytes.to_vec()));
            }

            // Now process items after iteration completes
            let mut moved_count = 0;
            for (key_bytes, value_bytes) in items_to_move {
                // Parse key to get blob index
                if let Some(blob_key) = BlobKey::from_undecided_key(&key_bytes) {
                    // Write to decided CF with height-only key
                    let decided_key = blob_key.to_decided_key();
                    db.put_cf(cf_decided, decided_key, value_bytes)?;

                    // Delete from undecided CF (safe now - iteration complete)
                    db.delete_cf(cf_undecided, key_bytes)?;

                    moved_count += 1;
                } else {
                    warn!("Failed to parse undecided key, skipping");
                }
            }

            debug!(
                height = height.as_u64(),
                round = round,
                count = moved_count,
                "Marked blobs as decided"
            );

            Ok(())
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }

    async fn get_decided_blobs(&self, height: Height) -> Result<Vec<BlobSidecar>, BlobStoreError> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_DECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_DECIDED.to_string()))?;

            let prefix = Self::decided_prefix(height);
            let mut blobs = Vec::new();

            let iter = db.prefix_iterator_cf(cf, &prefix);
            for item in iter {
                let (key_bytes, value_bytes) = item?;

                if !key_bytes.starts_with(&prefix) {
                    break;
                }

                let blob = Self::deserialize_blob(&value_bytes)?;
                blobs.push(blob);
            }

            // Sort by index for deterministic ordering
            blobs.sort_by_key(|b| b.index);

            debug!(height = height.as_u64(), count = blobs.len(), "Retrieved decided blobs");

            Ok(blobs)
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }

    async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobStoreError> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_UNDECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_UNDECIDED.to_string()))?;

            // IMPORTANT: Collect keys before deleting to avoid iterator invalidation.
            // RocksDB iterators become invalid when you mutate the CF during iteration.
            let prefix = Self::undecided_prefix(height, round);
            let iter = db.prefix_iterator_cf(cf, &prefix);

            let mut keys_to_delete = Vec::new();
            for item in iter {
                let (key_bytes, _) = item?;

                if !key_bytes.starts_with(&prefix) {
                    break;
                }

                keys_to_delete.push(key_bytes.to_vec());
            }

            // Delete after iteration completes (safe now)
            let deleted_count = keys_to_delete.len();
            for key in keys_to_delete {
                db.delete_cf(cf, &key)?;
            }

            debug!(
                height = height.as_u64(),
                round = round,
                count = deleted_count,
                "Dropped blobs for round"
            );

            Ok(())
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }

    async fn delete_archived(&self, height: Height, indices: &[u8]) -> Result<(), BlobStoreError> {
        let db = self.db.clone();
        let indices = indices.to_vec();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_DECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_DECIDED.to_string()))?;

            for index in &indices {
                let key = BlobKey::new(height, 0, *index).to_decided_key();
                db.delete_cf(cf, key)?;
            }

            debug!(height = height.as_u64(), count = indices.len(), "Deleted archived blobs");

            Ok(())
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }

    async fn prune_before(&self, height: Height) -> Result<usize, BlobStoreError> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_DECIDED)
                .ok_or_else(|| BlobStoreError::ColumnFamilyNotFound(CF_DECIDED.to_string()))?;

            // IMPORTANT: Collect keys before deleting to avoid iterator invalidation.
            // RocksDB iterators become invalid when you mutate the CF during iteration.
            let mut keys_to_delete = Vec::new();

            // Iterate through all decided blobs
            let iter = db.iterator_cf(cf, rocksdb::IteratorMode::Start);
            for item in iter {
                let (key_bytes, _) = item?;

                // Parse height from key (first 8 bytes)
                if key_bytes.len() >= 8 {
                    let blob_height_bytes: [u8; 8] = key_bytes[0..8].try_into().unwrap();
                    let blob_height = u64::from_be_bytes(blob_height_bytes);

                    if blob_height < height.as_u64() {
                        keys_to_delete.push(key_bytes.to_vec());
                    }
                }
            }

            // Delete after iteration completes (safe now)
            let deleted_count = keys_to_delete.len();
            for key in keys_to_delete {
                db.delete_cf(cf, &key)?;
            }

            debug!(before_height = height.as_u64(), count = deleted_count, "Pruned decided blobs");

            Ok(deleted_count)
        })
        .await
        .map_err(BlobStoreError::TaskJoin)?
    }
}

#[cfg(test)]
mod tests {
    use ultramarine_types::{
        aliases::Bytes,
        blob::{Blob, KzgCommitment, KzgProof},
    };

    use super::*;

    fn create_test_blob(index: u8) -> BlobSidecar {
        let blob_data = vec![index; 131_072];
        let blob = Blob::new(Bytes::from(blob_data)).unwrap();
        let commitment = KzgCommitment([index; 48]);
        let proof = KzgProof([index; 48]);
        BlobSidecar::new(index, blob, commitment, proof)
    }

    #[tokio::test]
    async fn test_roundtrip_undecided() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        let height = Height::new(100);
        let round = 1;
        let blobs = vec![create_test_blob(0), create_test_blob(1)];

        // Store
        store.put_undecided_blobs(height, round, &blobs).await.unwrap();

        // Retrieve
        let retrieved = store.get_undecided_blobs(height, round).await.unwrap();

        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].index, 0);
        assert_eq!(retrieved[1].index, 1);
    }

    #[tokio::test]
    async fn test_mark_decided() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        let height = Height::new(100);
        let round = 1;
        let blobs = vec![create_test_blob(0), create_test_blob(1)];

        // Store undecided
        store.put_undecided_blobs(height, round, &blobs).await.unwrap();

        // Mark decided
        store.mark_decided(height, round).await.unwrap();

        // Should be in decided, not undecided
        let undecided = store.get_undecided_blobs(height, round).await.unwrap();
        assert_eq!(undecided.len(), 0);

        let decided = store.get_decided_blobs(height).await.unwrap();
        assert_eq!(decided.len(), 2);
    }

    #[tokio::test]
    async fn test_drop_round() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        let height = Height::new(100);
        let round1 = 1;
        let round2 = 2;

        // Store blobs for two rounds
        store.put_undecided_blobs(height, round1, &[create_test_blob(0)]).await.unwrap();
        store.put_undecided_blobs(height, round2, &[create_test_blob(1)]).await.unwrap();

        // Drop round 1
        store.drop_round(height, round1).await.unwrap();

        // Round 1 should be gone, round 2 should remain
        let round1_blobs = store.get_undecided_blobs(height, round1).await.unwrap();
        assert_eq!(round1_blobs.len(), 0);

        let round2_blobs = store.get_undecided_blobs(height, round2).await.unwrap();
        assert_eq!(round2_blobs.len(), 1);
    }

    #[tokio::test]
    async fn test_prune_before() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        // Create decided blobs at different heights
        for h in 90..110 {
            let height = Height::new(h);
            store.put_undecided_blobs(height, 0, &[create_test_blob(0)]).await.unwrap();
            store.mark_decided(height, 0).await.unwrap();
        }

        // Prune before height 100
        let pruned = store.prune_before(Height::new(100)).await.unwrap();
        assert_eq!(pruned, 10); // Heights 90-99

        // Height 100+ should still exist
        let remaining = store.get_decided_blobs(Height::new(105)).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    /// Comprehensive integration test for the full blob lifecycle.
    ///
    /// This test exercises the complete flow: store → mark_decided → drop_round → prune,
    /// with multiple blobs to ensure the iterator fixes work correctly.
    ///
    /// Regression test for iterator-while-deleting bugs:
    /// - mark_decided must move ALL blobs, not skip some due to iterator invalidation
    /// - drop_round must delete ALL blobs in a round
    /// - prune_before must delete ALL old blobs
    #[tokio::test]
    async fn test_full_lifecycle_integration() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = RocksDbBlobStore::open(temp_dir.path()).unwrap();

        // Setup: Create blobs for multiple heights and rounds
        // Height 100: round 1 (will be decided), round 2 (will be dropped)
        // Height 101: round 1 (will be decided), round 2 (will be dropped)
        // Height 102: round 1 (will be decided)

        let height_100 = Height::new(100);
        let height_101 = Height::new(101);
        let height_102 = Height::new(102);

        // Store 6 blobs per round to stress-test the iterator fix
        let blobs_round_1: Vec<_> = (0..6).map(create_test_blob).collect();
        let blobs_round_2: Vec<_> = (0..6).map(|i| create_test_blob(i + 10)).collect();

        // Height 100
        store.put_undecided_blobs(height_100, 1, &blobs_round_1).await.unwrap();
        store.put_undecided_blobs(height_100, 2, &blobs_round_2).await.unwrap();

        // Height 101
        store.put_undecided_blobs(height_101, 1, &blobs_round_1).await.unwrap();
        store.put_undecided_blobs(height_101, 2, &blobs_round_2).await.unwrap();

        // Height 102
        store.put_undecided_blobs(height_102, 1, &blobs_round_1).await.unwrap();

        // Verify all blobs stored (22 total: 6+6+6+6+6)
        assert_eq!(store.get_undecided_blobs(height_100, 1).await.unwrap().len(), 6);
        assert_eq!(store.get_undecided_blobs(height_100, 2).await.unwrap().len(), 6);
        assert_eq!(store.get_undecided_blobs(height_101, 1).await.unwrap().len(), 6);
        assert_eq!(store.get_undecided_blobs(height_101, 2).await.unwrap().len(), 6);
        assert_eq!(store.get_undecided_blobs(height_102, 1).await.unwrap().len(), 6);

        // Step 1: Mark height 100 round 1 as decided
        // This tests mark_decided with 6 blobs (iterator fix critical here)
        store.mark_decided(height_100, 1).await.unwrap();

        // Verify ALL 6 blobs moved to decided (not just some due to iterator bug)
        let decided_100 = store.get_decided_blobs(height_100).await.unwrap();
        assert_eq!(decided_100.len(), 6, "mark_decided should move ALL blobs");
        assert_eq!(store.get_undecided_blobs(height_100, 1).await.unwrap().len(), 0);

        // Round 2 should still be in undecided
        assert_eq!(store.get_undecided_blobs(height_100, 2).await.unwrap().len(), 6);

        // Step 2: Drop height 100 round 2 (failed round cleanup)
        // This tests drop_round with 6 blobs (iterator fix critical here)
        store.drop_round(height_100, 2).await.unwrap();

        // Verify ALL 6 blobs deleted (not just some due to iterator bug)
        assert_eq!(
            store.get_undecided_blobs(height_100, 2).await.unwrap().len(),
            0,
            "drop_round should delete ALL blobs"
        );

        // Step 3: Mark height 101 and 102 as decided
        store.mark_decided(height_101, 1).await.unwrap();
        store.mark_decided(height_102, 1).await.unwrap();

        // Drop their round 2s
        store.drop_round(height_101, 2).await.unwrap();
        // Height 102 had no round 2, this should be safe
        store.drop_round(height_102, 2).await.unwrap();

        // Verify decided blobs exist
        assert_eq!(store.get_decided_blobs(height_100).await.unwrap().len(), 6);
        assert_eq!(store.get_decided_blobs(height_101).await.unwrap().len(), 6);
        assert_eq!(store.get_decided_blobs(height_102).await.unwrap().len(), 6);

        // Step 4: Prune blobs before height 102
        // This tests prune_before with 12 blobs (iterator fix critical here)
        let pruned = store.prune_before(height_102).await.unwrap();
        assert_eq!(pruned, 12, "prune_before should delete ALL old blobs (6+6)");

        // Verify height 100 and 101 blobs are gone
        assert_eq!(store.get_decided_blobs(height_100).await.unwrap().len(), 0);
        assert_eq!(store.get_decided_blobs(height_101).await.unwrap().len(), 0);

        // Verify height 102 blobs remain
        assert_eq!(store.get_decided_blobs(height_102).await.unwrap().len(), 6);

        // Final verification: no undecided blobs left
        assert_eq!(store.get_undecided_blobs(height_100, 1).await.unwrap().len(), 0);
        assert_eq!(store.get_undecided_blobs(height_100, 2).await.unwrap().len(), 0);
        assert_eq!(store.get_undecided_blobs(height_101, 1).await.unwrap().len(), 0);
        assert_eq!(store.get_undecided_blobs(height_101, 2).await.unwrap().len(), 0);
        assert_eq!(store.get_undecided_blobs(height_102, 1).await.unwrap().len(), 0);
    }
}
