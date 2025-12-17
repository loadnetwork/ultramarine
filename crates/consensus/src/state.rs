//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like
//! RPC.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    convert::TryFrom,
    sync::Arc,
};

use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadStatus};
use bytes::Bytes;
use color_eyre::eyre;
use ethereum_hashing::hash32_concat;
use fixed_bytes::Hash256 as FixedHash;
use malachitebft_app_channel::app::{
    streaming::{StreamContent, StreamId, StreamMessage},
    types::{
        LocallyProposedValue, PeerId, ProposedValue,
        codec::Codec,
        core::{CommitCertificate, Round, ValidatorSet as ValidatorSetTrait, Validity},
    },
};
use rand::{Rng, SeedableRng, rngs::StdRng};
use sha2::{Digest as Sha2Digest, Sha256 as Sha2Sha256};
use ssz::Decode;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use tree_hash::TreeHash;
use ultramarine_blob_engine::{
    BlobEngine, BlobEngineError, BlobEngineImpl, store::rocksdb::RocksDbBlobStore,
};
use ultramarine_execution::notifier::ExecutionNotifier;
use ultramarine_types::{
    address::Address,
    // Phase 3: Import blob types for streaming
    aliases::{B256, Block, BlockHash, Bytes as AlloyBytes},
    archive::{ArchiveJob, ArchiveNotice, ArchiveRecord, BlobArchivalStatus},
    blob::{BlobsBundle, KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK, kzg_to_versioned_hash},
    // Phase 4: Import three-layer metadata types
    blob_metadata::BlobMetadata,
    codec::proto::ProtobufCodec,
    consensus_block_metadata::ConsensusBlockMetadata,
    context::LoadContext,
    engine_api::{ExecutionBlock, ExecutionPayloadHeader, load_prev_randao},
    ethereum_compat::{
        BeaconBlockBodyMinimal, BeaconBlockHeader, SignedBeaconBlockHeader,
        generate_kzg_commitment_inclusion_proof, verify_kzg_commitment_inclusion_proof,
    },
    genesis::Genesis,
    height::Height,
    proposal_part::{BlobSidecar, ProposalData, ProposalFin, ProposalInit, ProposalPart},
    signing::Ed25519Provider,
    sync::SyncedValuePackage,
    validator_set::ValidatorSet,
    value::Value,
    value_metadata::ValueMetadata,
};

use crate::{
    store::{DecidedValue, Store},
    streaming::{PartStreamsMap, ProposalParts},
};

const MAX_EXECUTION_REQUESTS: usize = 64;

/// Size of randomly generated blocks in bytes
#[allow(dead_code)]
const BLOCK_SIZE: usize = 10 * 1024 * 1024; // 10 MiB

/// Size of chunks in which the data is split for streaming
const CHUNK_SIZE: usize = 128 * 1024; // 128 KiB

const BLOB_KZG_COMMITMENTS_INDEX: usize = 11;
const KZG_COMMITMENT_INCLUSION_PROOF_DEPTH: usize = 17;

/// Represents the internal state of the application node
/// Contains information about current height, round, proposals and blocks
///
/// Generic over `BlobEngine` to allow different blob storage backends.
/// Defaults to `BlobEngineImpl<RocksDbBlobStore>` for production use.
pub struct State<E = BlobEngineImpl<RocksDbBlobStore>>
where
    E: BlobEngine,
{
    #[allow(dead_code)]
    ctx: LoadContext,
    genesis: Genesis,
    signing_provider: Ed25519Provider,
    address: Address,
    store: Store,
    stream_nonce: u32,
    streams_map: PartStreamsMap,
    #[allow(dead_code)]
    rng: StdRng,
    /// Shared blob engine for verification and storage.
    /// Wrapped in Arc to allow sharing with the archiver worker.
    blob_engine: Arc<E>,
    pub(crate) blob_metrics: ultramarine_blob_engine::BlobEngineMetrics,
    pub(crate) archive_metrics: crate::archive_metrics::ArchiveMetrics,

    pub current_height: Height,
    pub current_round: Round,
    pub current_proposer: Option<Address>,
    pub peers: HashSet<PeerId>,

    pub latest_block: Option<ExecutionBlock>,

    // Track rounds with blobs for cleanup
    // Key: height, Value: set of rounds that have blobs
    blob_rounds: HashMap<Height, HashSet<i64>>,

    // Hash tree root of the latest decided blob sidecar header (used as parent_root for proposals)
    last_blob_sidecar_root: B256,
    last_blob_sidecar_height: Height,

    /// Retention window (in heights) for decided blobs.
    blob_retention_window: u64,

    /// Heights with blobs that are still awaiting archive notices.
    pending_archive_heights: BTreeSet<Height>,
    /// Heights that have fully archived blobs but are waiting for finalization before pruning.
    pending_prune_heights: BTreeSet<Height>,
    /// Highest height finalized/committed locally.
    ///
    /// NOTE: In V0, "finalized" equals "decided" - there is no additional finality delay.
    /// This means pruning happens immediately after a height is decided AND all blobs
    /// at that height have verified archive notices. Future versions may add a finality
    /// lag (e.g., 2/3 acks, on-chain anchoring) before allowing prune.
    latest_finalized_height: Height,
    /// Whether the archiver worker is enabled.
    /// When true, build_archive_job() produces jobs for the archiver worker after commit.
    /// Archive notices are ONLY generated after real uploads - there is no placeholder path.
    archiver_enabled: bool,

    // For stats
    pub txs_count: u64,
    pub chain_bytes: u64,
    pub start_time: Instant,
}

/// Summary of the effects triggered while finalizing a decided certificate.
#[derive(Debug)]
pub struct DecidedOutcome {
    /// Execution-layer view of the finalized block.
    pub execution_block: ExecutionBlock,
    /// Payload status returned by the execution layer.
    pub payload_status: PayloadStatus,
    /// Number of transactions in the execution payload.
    pub tx_count: usize,
    /// Size in bytes of the SSZ-encoded execution payload.
    pub block_bytes: usize,
    /// Number of blob sidecars imported for the block.
    pub blob_count: usize,
    /// Archive notices generated by this node (if proposer) ready for broadcast.
    /// When archiver worker is used, this will be empty and `archive_job` will be set instead.
    pub archive_notices: Vec<ArchiveNotice>,
    /// Archive job for the archiver worker (if proposer and has blobs).
    /// When present, the app should send this to the archiver instead of using archive_notices.
    pub archive_job: Option<ArchiveJob>,
}

/// Represents errors that can occur during the verification of a proposal's signature.
#[derive(Debug)]
enum SignatureVerificationError {
    /// Indicates that the `Fin` part of the proposal is missing.
    MissingFinPart,

    /// Indicates that the proposer was not found in the validator set.
    ProposerNotFound,

    /// Indicates that the signature in the `Fin` part is invalid.
    InvalidSignature,
}

// Make up a seed for the rng based on our address in
// order for each node to likely propose different values at
// each round.
fn seed_from_address(address: &Address) -> u64 {
    address.into_inner().chunks(8).fold(0u64, |acc, chunk| {
        let term =
            chunk.iter().fold(0u64, |acc, &x| acc.wrapping_shl(8).wrapping_add(u64::from(x)));
        acc.wrapping_add(term)
    })
}

impl<E> State<E>
where
    E: BlobEngine,
{
    /// Creates a new State instance with the given validator address and starting height
    ///
    /// # Arguments
    ///
    /// * `blob_engine` - The blob engine for verification and storage (wrapped in Arc for sharing)
    /// * `blob_metrics` - Metrics for blob operations (cloneable, shared via Arc)
    /// * `archive_metrics` - Metrics for archive/prune operations
    /// * Other parameters remain the same
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        genesis: Genesis,
        ctx: LoadContext,
        signing_provider: Ed25519Provider,
        address: Address,
        height: Height,
        store: Store,
        blob_engine: Arc<E>,
        blob_metrics: ultramarine_blob_engine::BlobEngineMetrics,
        archive_metrics: crate::archive_metrics::ArchiveMetrics,
    ) -> Self {
        Self {
            genesis,
            ctx,
            signing_provider,
            current_height: height,
            current_round: Round::new(0),
            current_proposer: None,
            address,
            store,
            stream_nonce: 0,
            streams_map: PartStreamsMap::new(),
            rng: StdRng::seed_from_u64(seed_from_address(&address)),
            blob_engine,
            blob_metrics,
            archive_metrics,
            peers: HashSet::new(),

            latest_block: None,
            blob_rounds: HashMap::new(),
            last_blob_sidecar_root: B256::ZERO,
            last_blob_sidecar_height: Height::new(0),
            blob_retention_window: BLOB_RETENTION_WINDOW,
            pending_archive_heights: BTreeSet::new(),
            pending_prune_heights: BTreeSet::new(),
            latest_finalized_height: Height::new(0),
            archiver_enabled: false,
            txs_count: 0,
            chain_bytes: 0,
            start_time: Instant::now(),
        }
    }

    /// Enable archiver worker mode.
    pub fn set_archiver_enabled(&mut self, enabled: bool) {
        self.archiver_enabled = enabled;
    }

    /// Returns the earliest height available in the state.
    /// If no values have been decided yet (empty store), returns the current height.
    pub async fn get_earliest_height(&self) -> Height {
        self.store.min_decided_value_height().await.unwrap_or(self.current_height)
    }

    /// Returns the latest (maximum) decided height from the store.
    /// Returns None if no heights have been decided yet.
    pub async fn get_latest_decided_height(&self) -> Option<Height> {
        self.store.max_decided_value_height().await
    }

    /// Returns a reference to the blob engine for blob operations
    pub fn blob_engine(&self) -> &E {
        &self.blob_engine
    }

    /// Returns an Arc clone of the blob engine for sharing with other components.
    /// Used to pass the blob engine to the archiver worker.
    pub fn blob_engine_shared(&self) -> Arc<E> {
        Arc::clone(&self.blob_engine)
    }

    pub async fn load_archive_notices(&self, height: Height) -> eyre::Result<Vec<ArchiveNotice>> {
        let records = self.store.archive_records(height).await?;
        Ok(records.into_iter().map(ArchiveRecord::into_notice).collect())
    }

    /// Retrieve blobs for a height, respecting the serving contract.
    ///
    /// This method checks the archival/pruned status before returning blobs:
    /// - **Pending**: Blobs are served normally from local storage.
    /// - **Archived (pre-prune)**: Blobs are still served in V0.
    /// - **Pruned**: Returns `BlobsPruned` error with locators for archive retrieval.
    ///
    /// Use this method instead of calling `blob_engine().get_for_import()` directly
    /// when you need to respect the serving contract (e.g., for `engine_getBlobsV1`).
    pub async fn get_blobs_with_status_check(
        &self,
        height: Height,
    ) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        // Check if blobs have been pruned.
        if let Ok(Some(metadata)) = self.store.get_blob_metadata(height).await &&
            metadata.is_pruned()
        {
            // Collect locators from archive records
            let locators: Vec<String> = (0..usize::from(metadata.blob_count()))
                .filter_map(|idx| {
                    if let BlobArchivalStatus::Archived(record) = metadata.archival_status(idx) {
                        Some(record.body.locator.clone())
                    } else {
                        None
                    }
                })
                .collect();

            self.archive_metrics.record_served_archived(locators.len());

            return Err(BlobEngineError::BlobsPruned {
                height,
                blob_count: locators.len(),
                locators,
            });
        }

        // Not pruned - serve from local storage
        match self.blob_engine.get_for_import(height).await {
            Ok(sidecars) => {
                self.archive_metrics.record_served(sidecars.len());
                Ok(sidecars)
            }
            Err(err) => Err(err),
        }
    }

    /// Retrieve undecided blobs for a specific round, ensuring they have not been pruned.
    pub async fn get_undecided_blobs_with_status_check(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        if let Ok(Some(metadata)) = self.store.get_blob_metadata(height).await &&
            metadata.is_pruned() &&
            metadata.blob_count() > 0
        {
            let locators: Vec<String> = (0..usize::from(metadata.blob_count()))
                .filter_map(|idx| {
                    if let BlobArchivalStatus::Archived(record) = metadata.archival_status(idx) {
                        Some(record.body.locator.clone())
                    } else {
                        None
                    }
                })
                .collect();

            self.archive_metrics.record_served_archived(locators.len());

            return Err(BlobEngineError::BlobsPruned {
                height,
                blob_count: locators.len(),
                locators,
            });
        }

        let blobs = self.blob_engine.get_undecided_blobs(height, round.as_i64()).await?;
        self.archive_metrics.record_served(blobs.len());
        Ok(blobs)
    }

    pub async fn rehydrate_pending_prunes(&mut self) -> eyre::Result<()> {
        if let Some(latest) = self.store.max_decided_value_height().await {
            self.latest_finalized_height = latest;
        }
        self.pending_prune_heights.clear();
        self.pending_archive_heights.clear();
        let heights = self.store.decided_heights().await?;
        for height in heights {
            let Some(mut metadata) = self.store.get_blob_metadata(height).await? else {
                continue;
            };
            if metadata.blob_count() == 0 || metadata.is_pruned() {
                continue;
            }
            let all_archived = (0..usize::from(metadata.blob_count())).all(|idx| {
                matches!(metadata.archival_status(idx), BlobArchivalStatus::Archived(_))
            });
            if !all_archived {
                // Only proposers upload in V0, so only the proposer needs to track pending
                // archival work. Followers may be missing notices but have no duty to upload.
                if self.archiver_enabled {
                    let proposer_hint = metadata.proposer_index_hint();
                    if self.proposer_address_for_height(height, proposer_hint).await? ==
                        Some(self.address)
                    {
                        self.pending_archive_heights.insert(height);
                    }
                }
                continue;
            }
            if self.is_height_finalized(height) {
                self.prune_archived_height(height, &mut metadata).await?;
            } else {
                self.pending_prune_heights.insert(height);
            }
        }
        Ok(())
    }

    fn is_height_finalized(&self, height: Height) -> bool {
        // "Finality" in V0 is "decided". During normal operation, `current_height` is always the
        // next height to propose, so any height `< current_height` is decided locally.
        //
        // On restarts, we also track `latest_finalized_height` via store hydration / WAL replay.
        // Use the max of both signals to avoid foot-guns where one lags behind the other.
        let by_progress = self.current_height.decrement().unwrap_or(Height::new(0));
        let effective = by_progress.max(self.latest_finalized_height);
        height <= effective
    }

    async fn flush_pending_prunes(&mut self) -> eyre::Result<()> {
        if self.pending_prune_heights.is_empty() {
            return Ok(());
        }
        let by_progress = self.current_height.decrement().unwrap_or(Height::new(0));
        let finalized = by_progress.max(self.latest_finalized_height);
        let ready: Vec<_> =
            self.pending_prune_heights.iter().copied().filter(|h| *h <= finalized).collect();
        for height in ready {
            self.pending_prune_heights.remove(&height);
            if let Some(mut metadata) = self.store.get_blob_metadata(height).await? {
                if metadata.blob_count() == 0 {
                    continue;
                }
                let all_archived = (0..usize::from(metadata.blob_count())).all(|idx| {
                    matches!(metadata.archival_status(idx), BlobArchivalStatus::Archived(_))
                });
                if all_archived {
                    self.prune_archived_height(height, &mut metadata).await?;
                } else {
                    // Notices missing after restart; re-queue until complete.
                    self.pending_prune_heights.insert(height);
                }
            }
        }
        Ok(())
    }

    /// Returns the cached parent root used for subsequent blob proposals.
    pub fn blob_parent_root(&self) -> B256 {
        self.last_blob_sidecar_root
    }

    /// Override the blob retention window (useful for tests exercising pruning paths).
    pub fn set_blob_retention_window_for_testing(&mut self, window: u64) {
        self.blob_retention_window = window.max(1);
    }

    /// Record a blob sync failure (e.g., verification or storage error during sync)
    ///
    /// This is a convenience method that wraps the internal metrics handle,
    /// maintaining encapsulation and allowing State to control its instrumentation surface.
    pub fn record_sync_failure(&self) {
        self.blob_metrics.record_sync_failure();
    }

    /// Phase 4: Hydrate blob parent root from latest decided BlobMetadata (Layer 2)
    ///
    /// This MUST be called on startup to restore the parent_root cache from Layer 2 metadata.
    /// The cache is updated ONLY when metadata becomes canonical (at commit).
    ///
    /// # Genesis Seeding
    ///
    /// If the blob metadata store is empty (e.g., after `make clean-net-ipc`), this method
    /// automatically seeds a genesis entry at height 0. This satisfies the invariant that
    /// every blob proposal must have a parent metadata entry.
    ///
    /// **Important**: The genesis metadata values (gas limit, timestamps) are arbitrary
    /// placeholders chosen for determinism, NOT derived from Reth's actual genesis block.
    /// See [`BlobMetadata::genesis()`] for details on why this is safe.
    ///
    /// # Cache Discipline
    ///
    /// - **Startup**: Load from `get_latest_blob_metadata()` (decided table)
    /// - **Finalization**: Update in `commit()` after `mark_blob_metadata_decided()`
    /// - **Never**: Update during propose/receive flows (failed rounds cannot corrupt cache)
    pub async fn hydrate_blob_parent_root(&mut self) -> eyre::Result<()> {
        if let Some((height, metadata)) =
            self.store.get_latest_blob_metadata().await.map_err(|e| eyre::Report::new(e))?
        {
            let header = metadata.to_beacon_header();
            self.last_blob_sidecar_root = header.hash_tree_root();
            self.last_blob_sidecar_height = height;
            info!(
                height = %height,
                parent_root = ?self.last_blob_sidecar_root,
                "Hydrated blob parent root from BlobMetadata"
            );
        } else {
            // Store is empty - seed genesis blob metadata for height 0
            // This is required for the first blob proposal at height 1 to pass
            // the parent metadata lookup check
            info!("No BlobMetadata found, seeding genesis (height 0)");
            let genesis_metadata = BlobMetadata::genesis();
            self.store.seed_genesis_blob_metadata().await.map_err(|e| eyre::Report::new(e))?;
            let genesis_root = genesis_metadata.to_beacon_header().hash_tree_root();
            self.last_blob_sidecar_root = genesis_root;
            self.last_blob_sidecar_height = Height::new(0);
            info!(
                parent_root = ?self.last_blob_sidecar_root,
                "Seeded genesis BlobMetadata at height 0"
            );
        }

        Ok(())
    }

    /// Phase 4: Cleanup stale undecided blob metadata on startup
    ///
    /// Removes orphaned `(height, round)` entries that were left behind due to:
    /// - Node crashes before commit
    /// - Timeouts/failed rounds
    /// - Any entry from heights less than current_height
    ///
    /// # Recovery Guarantee
    ///
    /// This ensures bounded storage growth after crashes by removing all undecided
    /// metadata below the current height. Safe because:
    /// - If height H was decided, its metadata is in BLOB_METADATA_DECIDED_TABLE
    /// - If height H was not decided, we'll re-propose at that height
    /// - No valid restream can reference heights < current_height
    pub async fn cleanup_stale_blob_metadata(&self) -> eyre::Result<()> {
        let before_height = self.current_height;
        let stale_entries = self
            .store
            .get_all_undecided_blob_metadata_before(before_height)
            .await
            .map_err(|e| eyre::Report::new(e))?;

        if stale_entries.is_empty() {
            debug!("No stale undecided blob metadata to clean up");
            return Ok(());
        }

        info!(
            count = stale_entries.len(),
            before_height = %before_height,
            "Cleaning up stale undecided blob metadata"
        );

        let mut deleted = 0;
        let mut failed = 0;

        for (height, round) in stale_entries {
            match self.store.delete_blob_metadata_undecided(height, round).await {
                Ok(()) => {
                    deleted += 1;
                    debug!(
                        height = %height,
                        round = %round,
                        "Deleted stale undecided blob metadata"
                    );
                }
                Err(e) => {
                    failed += 1;
                    warn!(
                        height = %height,
                        round = %round,
                        error = %e,
                        "Failed to delete stale undecided blob metadata"
                    );
                    // Continue cleanup even if one fails
                }
            }
        }

        info!(
            deleted = deleted,
            failed = failed,
            "Finished cleaning up stale undecided blob metadata"
        );

        Ok(())
    }

    /// Recover pending archive jobs on startup.
    ///
    /// Scans decided heights for blobs that have not been archived yet and returns
    /// archive jobs that should be enqueued to the worker. This handles the case where
    /// the process crashed after commit but before the archiver worker processed the jobs.
    ///
    /// **Important**: Only returns jobs for heights where THIS NODE was the proposer.
    /// Per the spec, only proposers upload blobs; followers verify and store notices.
    ///
    /// Returns a list of archive jobs for heights with pending blobs where we were proposer.
    pub async fn recover_pending_archive_jobs(&self) -> eyre::Result<Vec<ArchiveJob>> {
        let mut jobs = Vec::new();
        let decided_heights = self.store.decided_heights().await.unwrap_or_default();

        for height in decided_heights {
            let metadata = match self.store.get_blob_metadata(height).await? {
                Some(meta) => meta,
                None => continue,
            };

            // Skip if no blobs or already pruned
            if metadata.blob_count() == 0 || metadata.is_pruned() {
                continue;
            }

            // Check if all blobs are archived
            let all_archived = (0..usize::from(metadata.blob_count()))
                .all(|idx| metadata.archival_status(idx).is_archived());

            if all_archived {
                continue;
            }

            // Determine the decided round for this height.
            //
            // Prefer consensus metadata (kept forever) so recovery still works even if the
            // decided-value retention window has pruned historical decided values.
            let round = if let Some(consensus_meta) =
                self.store.get_consensus_block_metadata(height).await?
            {
                consensus_meta.round
            } else if let Some(decided_value) = self.store.get_decided_value(height).await? {
                decided_value.certificate.round
            } else {
                continue;
            };

            // **Critical**: Only recover jobs for heights where WE were the proposer.
            // This matches the check in commit() - only proposers upload blobs.
            //
            // NOTE: We use BlobMetadata.proposer_index_hint instead of load_undecided_proposal
            // because undecided proposals are pruned by commit() and won't survive past the
            // retention window. BlobMetadata survives longer and stores the proposer index.
            let is_local_proposer = metadata
                .proposer_index_hint()
                .and_then(|idx| self.genesis.validator_set.get_by_index(idx as usize))
                .map(|v| v.address == self.address)
                .unwrap_or(false);

            if !is_local_proposer {
                continue;
            }

            if let Some(job) = self.build_archive_job(height, round).await? {
                info!(
                    height = %height,
                    blob_count = job.blob_indices.len(),
                    "Recovered pending archive job from restart (we were proposer)"
                );
                jobs.push(job);
            }
        }

        if !jobs.is_empty() {
            info!(count = jobs.len(), "Recovered {} pending archive jobs from restart", jobs.len());
        }

        Ok(jobs)
    }

    /// Helper for tests and tooling: seed the blob metadata store with genesis entry.
    pub async fn seed_genesis_blob_metadata(&self) -> eyre::Result<()> {
        self.store.seed_genesis_blob_metadata().await.map_err(|e| eyre::Report::new(e))
    }

    /// Helper for tests and tooling: persist undecided block data for the given round.
    pub async fn store_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        data: Bytes,
        execution_requests: Vec<AlloyBytes>,
    ) -> eyre::Result<()> {
        self.store
            .store_undecided_block_data(height, round, data, execution_requests)
            .await
            .map_err(|e| eyre::Report::new(e))
    }

    /// Helper for tests and tooling: persist undecided blob metadata entry.
    pub async fn put_blob_metadata_undecided(
        &self,
        height: Height,
        round: Round,
        metadata: &BlobMetadata,
    ) -> eyre::Result<()> {
        self.store
            .put_blob_metadata_undecided(height, round, metadata)
            .await
            .map_err(|e| eyre::Report::new(e))
    }

    /// Helper for tests and tooling: fetch decided blob metadata for a height.
    pub async fn get_blob_metadata(&self, height: Height) -> eyre::Result<Option<BlobMetadata>> {
        self.store.get_blob_metadata(height).await.map_err(|e| eyre::Report::new(e))
    }

    fn validator_index(&self, address: &Address) -> Option<u64> {
        self.genesis
            .validator_set
            .validators
            .iter()
            .position(|v| &v.address == address)
            .map(|idx| idx as u64)
    }

    async fn proposer_address_for_height(
        &self,
        height: Height,
        proposer_hint: Option<u64>,
    ) -> eyre::Result<Option<Address>> {
        if let Some(index) = proposer_hint &&
            let Some(validator) = self.genesis.validator_set.get_by_index(index as usize)
        {
            return Ok(Some(validator.address));
        }

        match self.store.get_consensus_block_metadata(height).await {
            Ok(Some(metadata)) => Ok(Some(metadata.proposer)),
            Ok(None) => Ok(None),
            Err(e) => {
                Err(eyre::eyre!("Failed to load consensus metadata for height {}: {}", height, e))
            }
        }
    }

    fn build_sidecar_header_message(
        &self,
        height: Height,
        proposer: &Address,
        metadata: &ValueMetadata,
        body_root: B256,
    ) -> Result<BeaconBlockHeader, String> {
        let parent_root =
            if height.as_u64() == 0 { B256::ZERO } else { self.last_blob_sidecar_root };

        let proposer_index = self
            .validator_index(proposer)
            .ok_or_else(|| format!("Proposer {} not found in validator set", proposer))?;

        Ok(BeaconBlockHeader::new(
            height.as_u64(),
            proposer_index,
            parent_root,
            metadata.execution_payload_header.state_root,
            body_root,
        ))
    }

    pub fn prepare_blob_sidecar_parts(
        &self,
        value: &LocallyProposedValue<LoadContext>,
        bundle: Option<&BlobsBundle>,
    ) -> eyre::Result<(SignedBeaconBlockHeader, Vec<BlobSidecar>)> {
        let metadata = &value.value.metadata;

        let expected = metadata.blob_kzg_commitments.len();
        if let Some(bundle) = bundle {
            if expected != bundle.commitments.len() ||
                expected != bundle.blobs.len() ||
                expected != bundle.proofs.len()
            {
                return Err(eyre::eyre!(
                    "Blob bundle length mismatch: metadata={} commitments={} blobs={} proofs={}",
                    expected,
                    bundle.commitments.len(),
                    bundle.blobs.len(),
                    bundle.proofs.len()
                ));
            }
        } else if expected != 0 {
            return Err(eyre::eyre!(
                "Metadata contains {} commitments but no blob bundle was provided",
                expected
            ));
        }

        let commitments = metadata.blob_kzg_commitments.clone();
        let body = BeaconBlockBodyMinimal::from_ultramarine_data(
            commitments.clone(),
            &metadata.execution_payload_header,
        );
        let body_root = body.compute_body_root();

        let header_message = self
            .build_sidecar_header_message(value.height, &self.address, metadata, body_root)
            .map_err(|e| eyre::eyre!(e))?;

        let signing_root = header_message.hash_tree_root();
        let signature = self.signing_provider.sign(signing_root.as_slice());
        let signed_header = SignedBeaconBlockHeader::new(header_message, signature);

        let mut sidecars = Vec::new();
        if let Some(bundle) = bundle {
            sidecars.reserve(bundle.blobs.len());
            for (index, ((blob, commitment), proof)) in
                bundle.blobs.iter().zip(&bundle.commitments).zip(&bundle.proofs).enumerate()
            {
                if metadata.blob_kzg_commitments[index] != *commitment {
                    return Err(eyre::eyre!("Blob commitment mismatch at index {}", index));
                }

                let inclusion_proof = generate_kzg_commitment_inclusion_proof(
                    &commitments,
                    index,
                    &body,
                )
                .map_err(|e| {
                    eyre::eyre!("Failed to create inclusion proof for blob {}: {}", index, e)
                })?;

                let index_u16 = u16::try_from(index)
                    .map_err(|_| eyre::eyre!("Blob index {} exceeds u16::MAX", index))?;

                let sidecar = BlobSidecar::new(
                    index_u16,
                    blob.clone(),
                    *commitment,
                    *proof,
                    signed_header.clone(),
                    inclusion_proof,
                );
                sidecars.push(sidecar);
            }
        }

        Ok((signed_header, sidecars))
    }

    fn build_beacon_header_from_blob_metadata(
        &self,
        metadata: &BlobMetadata,
        proposer: &Address,
    ) -> Result<BeaconBlockHeader, String> {
        let proposer_index = metadata
            .proposer_index_hint
            .or_else(|| self.validator_index(proposer))
            .ok_or_else(|| format!("Proposer {} not found in validator set", proposer))?;

        let body_root = BeaconBlockBodyMinimal::from_ultramarine_data(
            metadata.blob_kzg_commitments.clone(),
            &metadata.execution_payload_header,
        )
        .compute_body_root();

        Ok(BeaconBlockHeader::new(
            metadata.height.as_u64(),
            proposer_index,
            metadata.parent_blob_root,
            metadata.execution_payload_header.state_root,
            body_root,
        ))
    }

    pub fn rebuild_blob_sidecars_for_restream(
        &self,
        metadata: &BlobMetadata,
        proposer: &Address,
        blobs: &[BlobSidecar],
    ) -> eyre::Result<Vec<BlobSidecar>> {
        let expected_count = usize::from(metadata.blob_count);
        if expected_count != blobs.len() {
            return Err(eyre::eyre!(
                "Blob metadata count ({}) does not match stored blobs ({})",
                expected_count,
                blobs.len()
            ));
        }

        let header_message = self
            .build_beacon_header_from_blob_metadata(metadata, proposer)
            .map_err(|e| eyre::eyre!(e))?;

        let signing_root = header_message.hash_tree_root();
        let signature = self.signing_provider.sign(signing_root.as_slice());
        let signed_header = SignedBeaconBlockHeader::new(header_message, signature);

        let commitments = metadata.blob_kzg_commitments.clone();
        let body = BeaconBlockBodyMinimal::from_ultramarine_data(
            commitments.clone(),
            &metadata.execution_payload_header,
        );
        let mut rebuilt = Vec::with_capacity(blobs.len());

        for (index, original_sidecar) in blobs.iter().enumerate() {
            let commitment = commitments
                .get(index)
                .ok_or_else(|| eyre::eyre!("Missing commitment for blob index {}", index))?;

            if original_sidecar.kzg_commitment != *commitment {
                return Err(eyre::eyre!("Commitment mismatch at index {} during restream", index));
            }

            let inclusion_proof =
                generate_kzg_commitment_inclusion_proof(&commitments, index, &body).map_err(
                    |e| eyre::eyre!("Failed to create inclusion proof for blob {}: {}", index, e),
                )?;

            let index_u16 = u16::try_from(index)
                .map_err(|_| eyre::eyre!("Blob index {} exceeds u16::MAX", index))?;

            let rebuilt_sidecar = BlobSidecar::new(
                index_u16,
                original_sidecar.blob.clone(),
                *commitment,
                original_sidecar.kzg_proof,
                signed_header.clone(),
                inclusion_proof,
            );

            rebuilt.push(rebuilt_sidecar);
        }

        // Record metrics: successful restream rebuild from storage
        self.blob_metrics.record_restream_rebuild();

        Ok(rebuilt)
    }

    /// Processes and adds a new proposal to the state if it's valid
    /// Returns Some(ProposedValue) if the proposal was accepted, None otherwise
    pub async fn received_proposal_part(
        &mut self,
        from: PeerId,
        part: StreamMessage<ProposalPart>,
    ) -> eyre::Result<Option<ProposedValue<LoadContext>>> {
        let sequence = part.sequence;

        // Check if we have a full proposal - for now we are assuming that the network layer will
        // stop spam/DOS
        let Some(parts) = self.streams_map.insert(from, part) else {
            return Ok(None);
        };

        // Check if the proposal is outdated
        if parts.height < self.current_height {
            debug!(
                height = %self.current_height,
                round = %self.current_round,
                part.height = %parts.height,
                part.round = %parts.round,
                part.sequence = %sequence,
                "Received outdated proposal part, ignoring"
            );

            return Ok(None);
        }

        if let Err(e) = self.verify_proposal_signature(&parts) {
            error!(
                height = %self.current_height,
                round = %self.current_round,
                error = ?e,
                "Received proposal with invalid signature, ignoring"
            );

            return Ok(None);
        }

        // Re-assemble the proposal from its parts with KZG verification and storage
        let (value, data, execution_requests, has_blobs) = match self
            .assemble_and_store_blobs(parts.clone())
            .await
        {
            Ok((value, data, execution_requests, has_blobs)) => {
                (value, data, execution_requests, has_blobs)
            }
            Err(e) => {
                error!(
                    height = %self.current_height,
                    round = %self.current_round,
                    error = %e,
                    "Received proposal with invalid blob KZG proofs or storage failure, rejecting"
                );
                return Ok(None);
            }
        };

        // Track blob rounds for cleanup
        if has_blobs {
            let round_i64 = parts.round.as_i64();
            self.blob_rounds.entry(parts.height).or_default().insert(round_i64);
        }

        // Log first 32 bytes of proposal data and total size
        if data.len() >= 32 {
            info!(
                "Proposal data[0..32]: {}, total_size: {} bytes, id: {:x}",
                hex::encode(&data[..32]),
                data.len(),
                value.value.id().as_u64()
            );
        }

        // Store the proposal and its data
        self.store.store_undecided_proposal(value.clone()).await?;

        // DIAGNOSTIC: Capture data length before move
        let data_len = data.len();

        self.store_undecided_proposal_data(
            parts.height,
            parts.round,
            data.clone(),
            execution_requests.clone(),
        )
        .await?;

        // DIAGNOSTIC: Confirm follower stored block data
        info!(
            "[DIAG] ✅ Follower stored block data: height={}, round={}, size={} bytes, address={}",
            parts.height,
            parts.round,
            data_len,
            self.validator_address()
        );

        Ok(Some(value))
    }

    pub async fn handle_archive_notice(&mut self, notice: ArchiveNotice) -> eyre::Result<()> {
        let height = notice.body.height;
        let blob_index = usize::from(notice.body.blob_index);

        let validator = self
            .get_validator_set()
            .validators
            .iter()
            .find(|v| v.address == notice.body.archived_by)
            .ok_or_else(|| {
                eyre::eyre!("ArchiveNotice from unknown validator {}", notice.body.archived_by)
            })?;

        if !notice.verify(&validator.public_key) {
            return Err(eyre::eyre!("Invalid ArchiveNotice signature for height {}", height));
        }

        let mut metadata = self.store.get_blob_metadata(height).await?.ok_or_else(|| {
            eyre::eyre!("Missing BlobMetadata for ArchiveNotice at height {}", height)
        })?;

        if blob_index >= usize::from(metadata.blob_count()) {
            return Err(eyre::eyre!(
                "ArchiveNotice blob_index {} exceeds blob_count {}",
                blob_index,
                metadata.blob_count()
            ));
        }

        let proposer_hint = metadata.proposer_index_hint();
        let expected_proposer = self.proposer_address_for_height(height, proposer_hint).await?;
        if let Some(expected) = expected_proposer {
            if notice.body.archived_by != expected {
                self.archive_metrics.record_receipt_mismatch();
                return Err(eyre::eyre!(
                    "ArchiveNotice archived_by {} does not match proposer {} at height {}",
                    notice.body.archived_by,
                    expected,
                    height
                ));
            }
        } else {
            warn!(
                height = %height,
                "Missing proposer hint/metadata; skipping proposer-only notice enforcement"
            );
        }

        if metadata.blob_kzg_commitments()[blob_index] != notice.body.kzg_commitment {
            return Err(eyre::eyre!(
                "ArchiveNotice commitment mismatch at height {} index {}",
                height,
                blob_index
            ));
        }

        if metadata.blob_keccak_hashes()[blob_index] != notice.body.blob_keccak {
            return Err(eyre::eyre!(
                "ArchiveNotice blob hash mismatch at height {} index {}",
                height,
                blob_index
            ));
        }

        if let BlobArchivalStatus::Archived(existing) = metadata.archival_status(blob_index) {
            if existing.body.provider_id != notice.body.provider_id ||
                existing.body.locator != notice.body.locator ||
                existing.body.blob_keccak != notice.body.blob_keccak
            {
                // Record receipt mismatch metric
                self.archive_metrics.record_receipt_mismatch();
                return Err(eyre::eyre!(
                    "Conflicting ArchiveNotice for height {} index {}",
                    height,
                    blob_index
                ));
            } else {
                debug!(
                    height = %height,
                    index = %blob_index,
                    "ArchiveNotice already recorded for blob"
                );
                return Ok(());
            }
        }

        let record = ArchiveRecord::from_notice(notice.clone());
        self.store.insert_archive_record(height, notice.body.blob_index, record.clone()).await?;
        metadata.set_archive_record(record);
        self.store.update_blob_metadata(height, metadata.clone()).await?;

        tracing::info!(
            height = %height,
            index = %blob_index,
            provider_id = %notice.body.provider_id,
            locator = %notice.body.locator,
            "✅ ArchiveNotice verified and recorded"
        );

        let all_archived = (0..usize::from(metadata.blob_count()))
            .all(|idx| matches!(metadata.archival_status(idx), BlobArchivalStatus::Archived(_)));

        if all_archived && metadata.blob_count() > 0 {
            self.pending_archive_heights.remove(&height);
            if self.is_height_finalized(height) {
                self.prune_archived_height(height, &mut metadata).await?;
            } else {
                self.pending_prune_heights.insert(height);
            }
        }

        Ok(())
    }

    async fn prune_archived_height(
        &mut self,
        height: Height,
        metadata: &mut BlobMetadata,
    ) -> eyre::Result<()> {
        if metadata.is_pruned() || metadata.blob_count() == 0 {
            return Ok(());
        }

        let blob_count = usize::from(metadata.blob_count());
        let indices: Vec<u16> = (0..blob_count).map(|idx| idx as u16).collect();

        // Estimate bytes (each blob is ~131KB)
        let estimated_bytes = blob_count * ultramarine_types::blob::BYTES_PER_BLOB;

        self.blob_engine
            .mark_archived(height, &indices)
            .await
            .map_err(|e| eyre::eyre!("Failed to prune blobs for height {}: {}", height, e))?;

        metadata.mark_pruned();
        self.store.update_blob_metadata(height, metadata.clone()).await?;

        // Record prune metrics
        self.archive_metrics.record_pruned(blob_count, estimated_bytes);

        info!(height = %height, blob_count = %blob_count, "Pruned blobs after archival");
        self.pending_archive_heights.remove(&height);
        self.pending_prune_heights.remove(&height);
        Ok(())
    }

    /// Record notice propagation duration for metrics.
    /// Call this after successfully broadcasting an archive notice to peers.
    pub fn observe_notice_propagation(&self, duration: std::time::Duration) {
        self.archive_metrics.observe_notice_propagation(duration);
    }

    /// Build an ArchiveJob for the archiver worker.
    /// This collects blob indices, commitments and keccak hashes for blobs that
    /// haven't been archived yet at the given height.
    pub async fn build_archive_job(
        &self,
        height: Height,
        round: Round,
    ) -> eyre::Result<Option<ArchiveJob>> {
        let metadata = match self.store.get_blob_metadata(height).await? {
            Some(meta) => meta,
            None => return Ok(None),
        };

        if metadata.blob_count() == 0 {
            return Ok(None);
        }

        let archived = self.store.archived_blob_indexes(height).await?;
        let archived_set: HashSet<u16> = archived.into_iter().collect();

        let blobs = self.blob_engine.get_for_import(height).await.map_err(|e| {
            eyre::eyre!("Failed to load decided blobs for archival at height {}: {}", height, e)
        })?;

        let mut blob_indices = Vec::new();
        let mut commitments = Vec::new();
        let mut blob_keccaks = Vec::new();
        let mut versioned_hashes = Vec::new();

        for sidecar in blobs {
            if archived_set.contains(&sidecar.index) {
                continue;
            }
            blob_indices.push(sidecar.index);
            commitments.push(sidecar.kzg_commitment);
            blob_keccaks.push(sidecar.blob_keccak());
            let vh = kzg_to_versioned_hash(sidecar.kzg_commitment.as_bytes());
            versioned_hashes.push(B256::from(vh));
        }

        if blob_indices.is_empty() {
            return Ok(None);
        }

        Ok(Some(ArchiveJob {
            height,
            round,
            blob_indices,
            commitments,
            blob_keccaks,
            versioned_hashes,
        }))
    }

    pub async fn store_undecided_proposal_data(
        &mut self,
        height: Height,
        round: Round,
        data: Bytes,
        execution_requests: Vec<AlloyBytes>,
    ) -> eyre::Result<()> {
        self.store
            .store_undecided_block_data(height, round, data, execution_requests)
            .await
            .map_err(|e| eyre::Report::new(e))
    }

    /// Store execution payload data for a synced value at arbitrary height/round
    ///
    /// This is used by ProcessSyncedValue to store execution payloads received
    /// during state sync, which may be for different heights/rounds than current.
    pub async fn store_synced_block_data(
        &mut self,
        height: Height,
        round: Round,
        data: Bytes,
        execution_requests: Vec<AlloyBytes>,
    ) -> eyre::Result<()> {
        Self::validate_execution_requests(&execution_requests)
            .map_err(|err| eyre::eyre!("Invalid execution requests: {}", err))?;

        self.store_undecided_proposal_data(height, round, data, execution_requests).await
    }

    /// Process a [`SyncedValuePackage`] received during state sync.
    ///
    /// Returns the `ProposedValue` that should be forwarded to consensus if
    /// processing succeeds, or `Ok(None)` if validation fails and the package
    /// must be rejected.
    pub async fn process_synced_package(
        &mut self,
        height: Height,
        round: Round,
        proposer: Address,
        package: SyncedValuePackage,
    ) -> eyre::Result<Option<ProposedValue<LoadContext>>> {
        info!(height = %height, round = %round, "🔵 SYNC: process_synced_package called");
        match package {
            SyncedValuePackage::Full {
                value,
                execution_payload_ssz,
                blob_sidecars,
                execution_requests,
                archive_notices,
            } => {
                info!(
                    height = %height,
                    round = %round,
                    payload_size = execution_payload_ssz.len(),
                    blob_count = blob_sidecars.len(),
                    "🔵 SYNC: Processing FULL synced value package"
                );

                let value_metadata = value.metadata.clone();

                self.store_synced_block_data(
                    height,
                    round,
                    execution_payload_ssz,
                    execution_requests,
                )
                .await?;

                if !blob_sidecars.is_empty() {
                    let round_i64 = round.as_i64();

                    if let Err(e) =
                        self.blob_engine.verify_and_store(height, round_i64, &blob_sidecars).await
                    {
                        error!(
                            height = %height,
                            round = %round,
                            error = %e,
                            "Failed to verify/store blobs during sync"
                        );
                        self.record_sync_failure();
                        return Ok(None);
                    }

                    if value_metadata.blob_kzg_commitments.len() != blob_sidecars.len() {
                        error!(
                            height = %height,
                            round = %round,
                            metadata_count = value_metadata.blob_kzg_commitments.len(),
                            sidecar_count = blob_sidecars.len(),
                            "Commitment count mismatch between metadata and sidecars"
                        );
                        self.blob_engine.drop_round(height, round_i64).await.ok();
                        self.record_sync_failure();
                        return Ok(None);
                    }

                    let mut mismatch = false;
                    for sidecar in &blob_sidecars {
                        let index = usize::from(sidecar.index);
                        if index >= value_metadata.blob_kzg_commitments.len() ||
                            value_metadata.blob_kzg_commitments[index] != sidecar.kzg_commitment
                        {
                            error!(
                                height = %height,
                                round = %round,
                                blob_index = %index,
                                "Commitment mismatch: metadata does not match verified sidecar"
                            );
                            mismatch = true;
                            break;
                        }
                    }

                    if mismatch {
                        self.blob_engine.drop_round(height, round_i64).await.ok();
                        self.store.delete_blob_metadata_undecided(height, round).await.ok();
                        self.record_sync_failure();
                        return Ok(None);
                    }

                    if let Err(e) = self
                        .verify_blob_sidecars(height, &proposer, &value_metadata, &blob_sidecars)
                        .await
                    {
                        error!(
                            height = %height,
                            round = %round,
                            error = %e,
                            "Blob sidecar verification failed during sync"
                        );
                        self.blob_engine.drop_round(height, round_i64).await.ok();
                        self.record_sync_failure();
                        return Ok(None);
                    }

                    if let Err(e) = self.blob_engine.mark_decided(height, round_i64).await {
                        error!(
                            height = %height,
                            round = %round,
                            error = %e,
                            "Failed to mark synced blobs as decided"
                        );
                        // Continue despite the error – blobs are verified and stored.
                    }
                }

                let parent_blob_root =
                    if height.as_u64() == 0 { B256::ZERO } else { self.blob_parent_root() };

                let proposer_index = self
                    .get_validator_set()
                    .validators
                    .iter()
                    .position(|v| v.address == proposer)
                    .map(|idx| idx as u64);

                let blob_keccak_hashes: Vec<B256> =
                    blob_sidecars.iter().map(|sidecar| sidecar.blob_keccak()).collect();

                let blob_metadata = if value_metadata.blob_kzg_commitments.is_empty() {
                    BlobMetadata::blobless(
                        height,
                        parent_blob_root,
                        &value_metadata.execution_payload_header,
                        proposer_index,
                    )
                } else {
                    BlobMetadata::new(
                        height,
                        parent_blob_root,
                        value_metadata.blob_kzg_commitments.clone(),
                        blob_keccak_hashes,
                        value_metadata.execution_payload_header.clone(),
                        proposer_index,
                    )
                };

                self.put_blob_metadata_undecided(height, round, &blob_metadata).await?;
                self.store.mark_blob_metadata_decided(height, round).await?;
                let header = blob_metadata.to_beacon_header();
                let new_root = header.hash_tree_root();
                info!(
                    height = %height,
                    old_cache_height = %self.last_blob_sidecar_height,
                    old_cache_root = ?self.last_blob_sidecar_root,
                    new_cache_root = ?new_root,
                    "✅ VALUESYNC: Updated blob parent root cache from synced value"
                );
                self.last_blob_sidecar_root = new_root;
                self.last_blob_sidecar_height = height;

                let proposed_value = ProposedValue {
                    height,
                    round,
                    valid_round: Round::Nil,
                    proposer,
                    value,
                    validity: Validity::Valid,
                };

                self.store_synced_proposal(proposed_value.clone()).await?;

                for notice in archive_notices {
                    self.handle_archive_notice(notice).await?;
                }

                Ok(Some(proposed_value))
            }
            SyncedValuePackage::MetadataOnly { value: _, archive_notices } => {
                // MetadataOnly is received when blobs have been pruned on the sending peer.
                // We process archive notices so we know where to fetch blobs from external
                // archives, but we cannot complete the sync from this peer alone.
                warn!(
                    height = %height,
                    round = %round,
                    notice_count = archive_notices.len(),
                    "Received MetadataOnly sync package (blobs pruned on peer), processing archive notices"
                );

                // Process archive notices so we have the locators stored
                for notice in archive_notices {
                    self.handle_archive_notice(notice).await?;
                }

                // Return None - the syncing peer cannot import this height from us,
                // but they now have archive locators to fetch blobs externally.
                // Malachite will try another peer or the EL will sync independently.
                Ok(None)
            }
        }
    }

    pub async fn process_decided_certificate<Notify>(
        &mut self,
        certificate: &CommitCertificate<LoadContext>,
        execution_payload_ssz: Bytes,
        notifier: &mut Notify,
    ) -> eyre::Result<DecidedOutcome>
    where
        Notify: ExecutionNotifier,
    {
        if execution_payload_ssz.is_empty() {
            return Err(eyre::eyre!(
                "Empty execution payload bytes for decided certificate at height {} round {}",
                certificate.height,
                certificate.round
            ));
        }

        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&execution_payload_ssz)
            .map_err(|e| eyre::eyre!("Failed to decode execution payload: {:?}", e))?;

        let block: Block = execution_payload
            .clone()
            .try_into_block()
            .map_err(|e| eyre::eyre!("Failed to convert execution payload into Block: {}", e))?;

        let height = certificate.height;
        let round = certificate.round;
        let round_i64 = round.as_i64();

        let mut versioned_hashes: Vec<BlockHash> =
            block.body.blob_versioned_hashes_iter().copied().collect();

        let execution_requests =
            self.get_execution_requests(height, round).await.unwrap_or_default();

        if versioned_hashes.is_empty() &&
            let Some(proposal) = self.load_undecided_proposal(height, round).await?
        {
            let commitments = proposal.value.metadata.blob_kzg_commitments.clone();
            versioned_hashes = versioned_hashes_from_commitments(&commitments);
        }

        // CRITICAL: Validate BEFORE promoting to ensure atomicity.
        // If validation fails, blobs remain in undecided state and can be retried.
        let mut blob_count = 0usize;
        let mut blobs_already_decided = false;
        if !versioned_hashes.is_empty() {
            debug!(
                height = %height,
                round = %round,
                blob_hashes = versioned_hashes.len(),
                "Validating blobs before promotion to decided state"
            );

            // Step 1: Get blobs - try undecided first (normal path), then decided (sync path)
            let blobs = match self.blob_engine.get_undecided_blobs(height, round_i64).await {
                Ok(undecided_blobs) if !undecided_blobs.is_empty() => undecided_blobs,
                _ => {
                    // Blobs not in undecided state - check if they're already decided (sync path)
                    let decided_blobs =
                        self.blob_engine.get_for_import(height).await.map_err(|e| {
                            eyre::eyre!("Failed to retrieve blobs at height {}: {}", height, e)
                        })?;
                    if !decided_blobs.is_empty() {
                        blobs_already_decided = true;
                    }
                    decided_blobs
                }
            };

            // Step 2: Validate blob count
            if blobs.len() != versioned_hashes.len() {
                return Err(eyre::eyre!(
                    "Blob count mismatch at height {} round {}: blob_engine has {} blobs, but block expects {}",
                    height,
                    round,
                    blobs.len(),
                    versioned_hashes.len()
                ));
            }

            // Step 3: Validate versioned hashes
            let computed_hashes = versioned_hashes_from_sidecars(&blobs);

            if computed_hashes != versioned_hashes {
                return Err(eyre::eyre!(
                    "Versioned hash mismatch at height {} round {}: computed from stored commitments != hashes in execution payload",
                    height,
                    round
                ));
            }

            blob_count = computed_hashes.len();

            debug!(
                height = %height,
                round = %round,
                blob_count = blob_count,
                already_decided = blobs_already_decided,
                "Validation passed"
            );
        }

        // Step 4: Promote blobs to decided state (or update gauge to 0 for blobless blocks)
        // Skip promotion if blobs are already decided (sync path)
        if !blobs_already_decided {
            debug!(
                height = %height,
                round = %round,
                blob_count = blob_count,
                "Promoting blobs to decided state"
            );

            if let Err(e) = self.blob_engine.mark_decided(height, round_i64).await {
                return Err(eyre::eyre!(
                    "Failed to promote blobs to decided state at height {} round {}: {}",
                    height,
                    round,
                    e
                ));
            }
        } else {
            debug!(
                height = %height,
                round = %round,
                blob_count = blob_count,
                "Blobs already in decided state (sync path)"
            );
        }

        let payload_status = notifier
            .notify_new_block(
                execution_payload.clone(),
                execution_requests.clone(),
                versioned_hashes.clone(),
            )
            .await
            .map_err(|e| eyre::eyre!("Execution layer new_payload failed: {}", e))?;
        if payload_status.is_invalid() {
            return Err(eyre::eyre!("Invalid payload status: {}", payload_status.status));
        }

        let payload_inner = &execution_payload.payload_inner.payload_inner;
        let block_hash = payload_inner.block_hash;
        let block_number = payload_inner.block_number;
        let parent_block_hash = payload_inner.parent_hash;

        // Enforce Load Network's constant prev_randao contract (0x01, Arbitrum pattern).
        // The CL always sends this constant in PayloadAttributes (client.rs:190,359),
        // so any payload with a different value indicates either:
        // 1. EL bug (not respecting the attribute we sent)
        // 2. Network attack (malicious payload propagated)
        // 3. Development error (test fixture inconsistency)
        // In all cases, rejecting at consensus ensures network integrity.
        // See: FINAL_PLAN.md "Engine API Contract" for rationale.
        let expected_prev_randao = load_prev_randao();
        if payload_inner.prev_randao != expected_prev_randao {
            return Err(eyre::eyre!(
                "prev_randao mismatch at height {} round {}: expected {:?}, got {:?}",
                height,
                round,
                expected_prev_randao,
                payload_inner.prev_randao
            ));
        }
        let tx_count = payload_inner.transactions.len();

        let expected_parent = self.latest_block.as_ref().map(|block| block.block_hash);
        if let Some(expected_parent_hash) = expected_parent &&
            expected_parent_hash != parent_block_hash
        {
            return Err(eyre::eyre!(
                "Parent hash mismatch at height {}: expected {:?} but payload declares {:?}",
                height,
                expected_parent_hash,
                parent_block_hash
            ));
        }

        let _latest_valid_hash = notifier
            .set_latest_forkchoice_state(block_hash)
            .await
            .map_err(|e| eyre::eyre!("Failed to update forkchoice: {}", e))?;

        debug!(
            height = %height,
            ?block_hash,
            ?parent_block_hash,
            block_number,
            txs = tx_count,
            blobs = blob_count,
            "Finalizing decided certificate"
        );

        self.txs_count += tx_count as u64;
        self.chain_bytes += execution_payload_ssz.len() as u64;

        let elapsed_time = self.start_time.elapsed();
        info!(
            "👉 stats at height {}: #txs={}, txs/s={:.2}, chain_bytes={}, bytes/s={:.2}",
            height,
            self.txs_count,
            self.txs_count as f64 / elapsed_time.as_secs_f64(),
            self.chain_bytes,
            self.chain_bytes as f64 / elapsed_time.as_secs_f64(),
        );

        debug!("🦄 Block at height {} contains {} transactions", height, tx_count);

        self.commit(certificate.clone()).await?;

        let execution_block = ExecutionBlock {
            block_hash,
            block_number,
            parent_hash: parent_block_hash,
            timestamp: execution_payload.timestamp(),
            prev_randao: expected_prev_randao,
        };
        self.latest_block = Some(execution_block);

        let archive_notices = Vec::new();

        // Build archive job for archiver worker (when enabled, this will be used instead of
        // notices) Check if we were the proposer by loading the proposal
        let archive_job = if blob_count > 0 {
            let is_local_proposer = match self.load_undecided_proposal(height, round).await {
                Ok(Some(proposal)) => proposal.proposer == self.address,
                _ => false,
            };
            if is_local_proposer {
                self.build_archive_job(height, round).await.unwrap_or(None)
            } else {
                None
            }
        } else {
            None
        };

        if blob_count > 0 && self.archiver_enabled && archive_job.is_some() {
            self.pending_archive_heights.insert(height);
        }

        Ok(DecidedOutcome {
            execution_block,
            payload_status,
            tx_count,
            block_bytes: execution_payload_ssz.len(),
            blob_count,
            archive_notices,
            archive_job,
        })
    }

    /// Store a synced proposal value
    ///
    /// This is used by ProcessSyncedValue to store the ProposedValue received
    /// during state sync. The proposal MUST be stored before consensus decides,
    /// otherwise the commit() method will fail when it tries to retrieve the
    /// undecided proposal.
    pub async fn store_synced_proposal(
        &mut self,
        value: ProposedValue<LoadContext>,
    ) -> eyre::Result<()> {
        self.store.store_undecided_proposal(value).await.map_err(|e| eyre::Report::new(e))
    }

    /// Retrieves a decided block at the given height
    pub async fn get_decided_value(&self, height: Height) -> Option<DecidedValue> {
        self.store.get_decided_value(height).await.ok().flatten()
    }

    /// Retrieves a decided block data at the given height
    pub async fn get_block_data(&self, height: Height, round: Round) -> Option<Bytes> {
        self.store.get_block_data(height, round).await.ok().flatten()
    }

    pub async fn get_execution_requests(
        &self,
        height: Height,
        round: Round,
    ) -> Option<Vec<AlloyBytes>> {
        self.store.get_execution_requests(height, round).await.ok().flatten()
    }

    /// Fetch blob metadata for a specific (height, round), falling back to decided metadata.
    ///
    /// This supports restreaming and diagnostics by guaranteeing we can rebuild blob headers
    /// even after the round has advanced.
    pub async fn load_blob_metadata_for_round(
        &self,
        height: Height,
        round: Round,
    ) -> eyre::Result<Option<BlobMetadata>> {
        if let Some(metadata) = self
            .store
            .get_blob_metadata_undecided(height, round)
            .await
            .map_err(|e| eyre::Report::new(e))?
        {
            return Ok(Some(metadata));
        }

        self.store.get_blob_metadata(height).await.map_err(|e| eyre::Report::new(e))
    }

    /// Returns the validator address associated with this state.
    pub fn validator_address(&self) -> &Address {
        &self.address
    }

    /// Retrieves an undecided proposal for restreaming or diagnostics.
    pub async fn load_undecided_proposal(
        &self,
        height: Height,
        round: Round,
    ) -> eyre::Result<Option<ProposedValue<LoadContext>>> {
        self.store.get_undecided_proposal(height, round).await.map_err(|e| eyre::Report::new(e))
    }

    /// Commits a value with the given certificate, updating internal state
    /// and moving to the next height
    pub async fn commit(
        &mut self,
        certificate: CommitCertificate<LoadContext>,
    ) -> eyre::Result<()> {
        info!(
            height = %certificate.height,
            round = %certificate.round,
            "Looking for certificate"
        );

        let proposal =
            self.store.get_undecided_proposal(certificate.height, certificate.round).await;

        let (proposal, is_idempotent_replay) = match proposal {
            Ok(Some(proposal)) => (proposal, false),
            Ok(None) => {
                // Undecided proposal missing: if the decided value already exists (e.g., WAL
                // replay), we're in idempotent mode. We still need to advance current_height
                // and clean up state, but we skip re-creating metadata that already exists.
                match self.store.get_decided_value(certificate.height).await {
                    Ok(Some(decided_value)) => {
                        info!(
                            height = %certificate.height,
                            round = %certificate.round,
                            "Decided value already present; entering idempotent commit mode (WAL replay)"
                        );
                        // In idempotent mode, metadata/blobs should already be decided, so we
                        // just need to advance height. We create a minimal ProposedValue to
                        // satisfy the borrow checker, but we'll skip metadata operations.
                        let minimal_proposal = ProposedValue {
                            height: certificate.height,
                            round: certificate.round,
                            valid_round: Round::Nil,
                            proposer: Address::new([0u8; 20]), /* Placeholder, not used in
                                                                * idempotent path */
                            value: decided_value.value,
                            validity: Validity::Valid,
                        };
                        (minimal_proposal, true)
                    }
                    Ok(None) => {
                        return Err(eyre::eyre!(
                            "Cannot commit height {} round {}: neither undecided proposal nor decided value found",
                            certificate.height,
                            certificate.round
                        ));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        };

        // Only perform persistence and metadata operations on first commit (not WAL replay)
        if !is_idempotent_replay {
            self.store.store_decided_value(&certificate, proposal.value.clone()).await?;

            // Phase 4: Three-layer metadata promotion (Layer 1 → Layer 2 → Layer 3)
            // This follows the architectural principle: Consensus → Ethereum → Blobs

            // LAYER 1: Store ConsensusBlockMetadata (pure BFT consensus state)
            // Build from certificate and proposal data

            // Compute validator set hash using SSZ tree-hash for deterministic ordering
            let validator_set_hash = {
                use tree_hash::{Hash256 as TreeHash256, merkle_root};

                let leaves: Vec<TreeHash256> = self
                    .genesis
                    .validator_set
                    .validators
                    .iter()
                    .map(|validator| {
                        let address_bytes = validator.address.into_inner();
                        let mut padded = [0u8; 32];
                        padded[..address_bytes.len()].copy_from_slice(&address_bytes);
                        TreeHash256::from_slice(&padded)
                    })
                    .collect();

                if leaves.is_empty() {
                    B256::ZERO
                } else {
                    let mut leaf_bytes = Vec::with_capacity(leaves.len() * 32);
                    for hash in &leaves {
                        leaf_bytes.extend_from_slice(hash.as_ref());
                    }
                    let root = merkle_root(&leaf_bytes, leaves.len());
                    B256::from_slice(root.as_ref())
                }
            };

            let consensus_metadata = ConsensusBlockMetadata::new(
                certificate.height,
                certificate.round,
                proposal.proposer,
                proposal.value.metadata.execution_payload_header.timestamp,
                validator_set_hash,
                proposal.value.metadata.execution_payload_header.block_hash,
                proposal.value.metadata.execution_payload_header.gas_limit,
                proposal.value.metadata.execution_payload_header.gas_used,
            );

            self.store
                .put_consensus_block_metadata(certificate.height, &consensus_metadata)
                .await
                .map_err(|e| {
                    error!(
                        height = %certificate.height,
                        round = %certificate.round,
                        error = %e,
                        "Failed to store ConsensusBlockMetadata"
                    );
                    eyre::eyre!(
                        "Cannot finalize block at height {} without consensus metadata: {}",
                        certificate.height,
                        e
                    )
                })?;

            // LAYER 2: Promote BlobMetadata from undecided → decided
            // This MUST happen before blob engine promotion to maintain data consistency.
            // If this fails, we abort the commit because the Ethereum compatibility layer is
            // broken.
            self.store
                .mark_blob_metadata_decided(certificate.height, certificate.round)
                .await
                .map_err(|e| {
                    error!(
                        height = %certificate.height,
                        round = %certificate.round,
                        error = %e,
                        "CRITICAL: Failed to promote BlobMetadata - aborting commit"
                    );
                    eyre::eyre!(
                        "Cannot finalize block at height {} round {} without BlobMetadata: {}",
                        certificate.height,
                        certificate.round,
                        e
                    )
                })?;

            let metadata = self
                .store
                .get_blob_metadata(certificate.height)
                .await
                .map_err(|e| {
                    error!(
                        height = %certificate.height,
                        error = %e,
                        "CRITICAL: Failed to load BlobMetadata after promotion"
                    );
                    eyre::eyre!("Failed to load BlobMetadata: {}", e)
                })?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Promoted BlobMetadata missing for height {}",
                        certificate.height.as_u64()
                    )
                })?;

            let header = metadata.to_beacon_header();
            let new_root = header.hash_tree_root();
            info!(
                height = %certificate.height,
                old_root = ?self.last_blob_sidecar_root,
                new_root = ?new_root,
                "Updated blob parent root from decided BlobMetadata"
            );
            self.last_blob_sidecar_root = new_root;
            self.last_blob_sidecar_height = certificate.height;

            // NOTE: Archive notices are ONLY generated by the archiver worker after real uploads.
            // There is no synchronous/placeholder path - pruning waits indefinitely until
            // the archiver worker successfully uploads blobs and produces valid notices.
            // See PHASE6_ARCHIVE_PRUNE_FINAL.md: "If no valid notice ever arrives, pruning waits
            // indefinitely"

            // NOTE: Blob promotion to decided state is handled by process_decided_certificate
            // before validation and EL notification, not here in commit().
            // This ensures proper ordering: validate first, promote only on success.

            // Track round for cleanup of orphaned blobs
            let round_i64 = certificate.round.as_i64();

            // Store block data for decided value
            let block_data =
                self.store.get_block_data(certificate.height, certificate.round).await?;
            let execution_requests =
                self.store.get_execution_requests(certificate.height, certificate.round).await?;

            // Log first 32 bytes of block data with JNT prefix
            if let Some(data) = &block_data &&
                data.len() >= 32
            {
                info!("Committed block_data[0..32]: {}", hex::encode(&data[..32]));
            }

            if let (Some(data), Some(requests)) = (block_data, execution_requests) {
                self.store.store_decided_block_data(certificate.height, data, requests).await?;
            }

            // Phase 4: Cache update moved above (after BlobMetadata promotion)
            // Old blob sidecar header loading removed - we now use BlobMetadata

            // Prune the store, keeping recent decided heights within the retention window
            let window = self.blob_retention_window.max(1);
            let retain_floor = certificate.height.as_u64().saturating_sub(window - 1);
            let mut retain_height = Height::new(retain_floor);
            if let Some(&pending_height) = self.pending_archive_heights.iter().next() &&
                pending_height < retain_height
            {
                tracing::debug!(
                    retain = %retain_height,
                    pending = %pending_height,
                    "Adjusting prune floor to protect pending archive height"
                );
                retain_height = pending_height;
            }
            self.store.prune(retain_height).await?;

            // Clean up orphaned blobs from failed rounds before advancing height
            // Any blobs that were stored but not marked as decided are orphaned and should be
            // removed
            if let Some(rounds) = self.blob_rounds.get(&certificate.height) {
                for &round in rounds.iter() {
                    // Skip the decided round
                    if round != round_i64 {
                        let Some(round_u32) = u32::try_from(round).ok() else {
                            warn!(
                                height = %certificate.height,
                                round = round,
                                "Invalid negative round encountered during metadata cleanup"
                            );
                            continue;
                        };

                        if let Err(e) = self
                            .store
                            .delete_blob_metadata_undecided(
                                certificate.height,
                                Round::new(round_u32),
                            )
                            .await
                        {
                            warn!(
                                height = %certificate.height,
                                round = round,
                                error = %e,
                                "Failed to delete undecided BlobMetadata for failed round"
                            );
                        }

                        if let Err(e) = self.blob_engine.drop_round(certificate.height, round).await
                        {
                            error!(
                                height = %certificate.height,
                                round = round,
                                error = %e,
                                "Failed to drop orphaned blobs for failed round"
                            );
                            // Don't fail commit - this is cleanup
                        } else {
                            debug!(
                                height = %certificate.height,
                                round = round,
                                "Dropped orphaned blobs for failed round"
                            );
                        }
                    }
                }
            }
            // Remove blob tracking for current height (we're advancing)
            self.blob_rounds.remove(&certificate.height);
        } // End of !is_idempotent_replay block

        if is_idempotent_replay {
            let round = certificate.round;
            let round_i64 = round.as_i64();

            if let Err(e) = self.blob_engine.mark_decided(certificate.height, round_i64).await {
                warn!(
                    height = %certificate.height,
                    round = %round,
                    error = %e,
                    "Failed to mark blobs decided during WAL replay"
                );
            }

            let decided_metadata = match self.store.get_blob_metadata(certificate.height).await {
                Ok(Some(metadata)) => Some(metadata),
                Ok(None) => {
                    match self.store.get_blob_metadata_undecided(certificate.height, round).await {
                        Ok(Some(metadata)) => {
                            if let Err(e) = self
                                .store
                                .mark_blob_metadata_decided(certificate.height, round)
                                .await
                            {
                                warn!(
                                    height = %certificate.height,
                                    round = %round,
                                    error = %e,
                                    "Failed to promote BlobMetadata during WAL replay"
                                );
                            }
                            Some(metadata)
                        }
                        Ok(None) => {
                            warn!(
                                height = %certificate.height,
                                round = %round,
                                "Idempotent replay could not find decided or undecided BlobMetadata"
                            );
                            None
                        }
                        Err(e) => {
                            warn!(
                                height = %certificate.height,
                                round = %round,
                                error = %e,
                                "Failed to load undecided BlobMetadata during WAL replay"
                            );
                            None
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        height = %certificate.height,
                        error = %e,
                        "Failed to load decided BlobMetadata during WAL replay"
                    );
                    None
                }
            };

            if let Some(metadata) = decided_metadata {
                let header = metadata.to_beacon_header();
                let new_root = header.hash_tree_root();
                info!(
                    height = %certificate.height,
                    round = %round,
                    old_cache_height = %self.last_blob_sidecar_height,
                    old_cache_root = ?self.last_blob_sidecar_root,
                    new_cache_height = %certificate.height,
                    new_cache_root = ?new_root,
                    "🔄 WAL REPLAY: Updated blob parent cache"
                );
                self.last_blob_sidecar_root = new_root;
                self.last_blob_sidecar_height = certificate.height;
            } else {
                warn!(
                    height = %certificate.height,
                    round = %round,
                    current_cache_height = %self.last_blob_sidecar_height,
                    current_cache_root = ?self.last_blob_sidecar_root,
                    "⚠️  WAL REPLAY: Could not update cache - no metadata found"
                );
            }

            // Log final cache state after WAL replay completes
            info!(
                height = %certificate.height,
                cache_height = %self.last_blob_sidecar_height,
                cache_root = ?self.last_blob_sidecar_root,
                "✅ WAL REPLAY COMPLETE: Final cache state"
            );
        }

        // Move to next height (always execute, even in idempotent replay)

        if certificate.height > self.latest_finalized_height {
            self.latest_finalized_height = certificate.height;
        }
        self.flush_pending_prunes().await?;

        // Move to next height.
        //
        // Use the committed height as the source of truth to avoid drift when `commit()` is
        // invoked while `current_height` is temporarily out of sync (e.g., tests, restream, or
        // WAL replay edge cases). Keep monotonicity by taking the max.
        let next_height = certificate.height.increment();
        self.current_height = self.current_height.max(next_height);
        self.current_round = Round::new(0);

        Ok(())
    }

    #[allow(dead_code)]
    pub fn make_block(&mut self) -> Bytes {
        let mut random_bytes = vec![0u8; BLOCK_SIZE];
        self.rng.fill(&mut random_bytes[..]);
        Bytes::from(random_bytes)
    }

    // TODO: LOAD: here could be added blobs, right? or...
    /// Creates a new proposal value for the given height
    /// Returns either a previously built proposal or creates a new one
    pub async fn propose_value(
        &mut self,
        height: Height,
        round: Round,
        data: Bytes,
    ) -> eyre::Result<LocallyProposedValue<LoadContext>> {
        assert_eq!(height, self.current_height);
        assert_eq!(round, self.current_round);

        // We create a new value.
        // TODO: Phase 2 - Update this to use actual ExecutionPayloadHeader and blobs metadata
        // For now, using deprecated from_bytes for backward compatibility
        #[allow(deprecated)]
        let value = Value::from_bytes(data);

        let proposal = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: self.address, // We are the proposer
            value,
            validity: Validity::Valid, // Our proposals are de facto valid
        };

        // Insert the new proposal into the undecided proposals.
        self.store.store_undecided_proposal(proposal.clone()).await?;

        Ok(LocallyProposedValue::new(proposal.height, proposal.round, proposal.value))
    }

    /// Phase 3: Creates a proposal value with proper metadata and blob commitments
    ///
    /// This method replaces the deprecated `propose_value()` pattern and correctly creates
    /// a Value with ValueMetadata containing:
    /// - ExecutionPayloadHeader (lightweight, ~516 bytes)
    /// - KZG commitments from blobs (48 bytes × blob_count)
    ///
    /// This ensures consensus messages stay small (~2KB) while blob data streams separately.
    ///
    /// Phase 4: Now also stores BlobMetadata (Layer 2) as undecided for future promotion.
    pub async fn propose_value_with_blobs(
        &mut self,
        height: Height,
        round: Round,
        _data: Bytes, // Still stored separately for SSZ reconstruction
        execution_payload: &alloy_rpc_types_engine::ExecutionPayloadV3,
        execution_requests: &[AlloyBytes],
        blobs_bundle: Option<&BlobsBundle>,
    ) -> eyre::Result<LocallyProposedValue<LoadContext>> {
        assert_eq!(height, self.current_height);
        assert_eq!(round, self.current_round);

        Self::validate_execution_requests(execution_requests)
            .map_err(|err| eyre::eyre!("Invalid execution requests: {}", err))?;

        // Phase 2: Extract lightweight header from execution payload
        let requests_hash = Some(ExecutionPayloadHeader::compute_requests_hash(execution_requests));
        let header = ExecutionPayloadHeader::from_payload(execution_payload, requests_hash);

        // Phase 2: Extract KZG commitments from blobs bundle
        let commitments = blobs_bundle.map(|bundle| bundle.commitments.clone()).unwrap_or_default();
        let blob_keccak_hashes =
            blobs_bundle.map(|bundle| bundle.blob_keccak_hashes()).unwrap_or_default();
        let has_blobs = !commitments.is_empty();

        // Phase 2: Create ValueMetadata (~2KB) instead of embedding full data
        let metadata = ValueMetadata::new(header.clone(), commitments.clone());

        // Phase 2: Create Value with metadata (NOT from_bytes!)
        let value = Value::new(metadata);

        let proposal = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: self.address, // We are the proposer
            value,
            validity: Validity::Valid, // Our proposals are de facto valid
        };

        // Insert the new proposal into the undecided proposals.
        self.store.store_undecided_proposal(proposal.clone()).await?;

        // Phase 4: Build and store BlobMetadata (Layer 2) as undecided
        // This MUST be stored before commit can promote it to decided
        let proposer_index = self.validator_index(&self.address);
        let parent_blob_root =
            if height.as_u64() == 0 { B256::ZERO } else { self.last_blob_sidecar_root };

        let blob_metadata = if commitments.is_empty() {
            // Blobless block - still need metadata for parent-root chaining
            BlobMetadata::blobless(height, parent_blob_root, &header, proposer_index)
        } else {
            // Blobbed block
            BlobMetadata::new(
                height,
                parent_blob_root,
                commitments,
                blob_keccak_hashes,
                header,
                proposer_index,
            )
        };

        self.store.put_blob_metadata_undecided(height, round, &blob_metadata).await.map_err(
            |e| {
                error!(
                    height = %height,
                    round = %round,
                    error = %e,
                    "Failed to store undecided BlobMetadata for proposal"
                );
                eyre::eyre!("Cannot propose without storing BlobMetadata: {}", e)
            },
        )?;

        // Track blob rounds for cleanup (proposer path)
        // This mirrors the tracking in received_proposal_part() for followers
        if has_blobs {
            let round_i64 = round.as_i64();
            self.blob_rounds.entry(height).or_default().insert(round_i64);
        }

        debug!(
            height = %height,
            round = %round,
            blob_count = blob_metadata.blob_count(),
            "Stored undecided BlobMetadata for proposal"
        );

        Ok(LocallyProposedValue::new(proposal.height, proposal.round, proposal.value))
    }

    fn stream_id(&mut self, height: Height, round: Round) -> StreamId {
        let mut bytes = Vec::with_capacity(size_of::<u64>() + size_of::<u32>());
        bytes.extend_from_slice(&height.as_u64().to_be_bytes());
        bytes.extend_from_slice(&round.as_u32().unwrap().to_be_bytes());
        bytes.extend_from_slice(&self.stream_nonce.to_be_bytes());
        self.stream_nonce += 1;
        StreamId::new(bytes.into())
    }

    /// Creates a stream message containing a proposal part.
    /// Updates internal sequence number and current proposal.
    ///
    /// Phase 3: Now accepts optional BlobsBundle for streaming blob sidecars
    ///
    /// # Arguments
    ///
    /// * `proposer` - Optional proposer address. If None, uses self.address (for our own
    ///   proposals). If Some, uses the provided address (for restreaming others' proposals).
    pub fn stream_proposal(
        &mut self,
        value: LocallyProposedValue<LoadContext>,
        data: Bytes,
        blob_sidecars: Option<&[BlobSidecar]>,
        execution_requests: &[AlloyBytes],
        proposer: Option<Address>, // For RestreamProposal support
    ) -> impl Iterator<Item = StreamMessage<ProposalPart>> {
        // Use provided proposer (for restreaming) or default to self.address (for our own
        // proposals)
        let proposer_address = proposer.unwrap_or(self.address);
        let proposal_height = value.height;
        let proposal_round = value.round;
        let parts = self.make_proposal_parts(
            value,
            data,
            blob_sidecars,
            execution_requests,
            proposer_address,
        );

        let stream_id = self.stream_id(proposal_height, proposal_round);

        let mut msgs = Vec::with_capacity(parts.len() + 1);
        let mut sequence = 0;

        for part in parts {
            let msg = StreamMessage::new(stream_id.clone(), sequence, StreamContent::Data(part));
            sequence += 1;
            msgs.push(msg);
        }

        msgs.push(StreamMessage::new(stream_id, sequence, StreamContent::Fin));
        msgs.into_iter()
    }

    /// Builds a single stream message carrying an [`ArchiveNotice`].
    pub fn stream_archive_notice(&mut self, notice: ArchiveNotice) -> StreamMessage<ProposalPart> {
        let height = notice.body.height;
        let round = notice.body.round;
        let stream_id = self.stream_id(height, round);
        StreamMessage::new(stream_id, 0, StreamContent::Data(ProposalPart::ArchiveNotice(notice)))
    }

    /// Phase 3: Updated to stream blobs as BlobSidecar parts
    fn make_proposal_parts(
        &self,
        value: LocallyProposedValue<LoadContext>,
        data: Bytes,
        blob_sidecars: Option<&[BlobSidecar]>,
        execution_requests: &[AlloyBytes],
        proposer: Address, // Explicit proposer (for restreaming)
    ) -> Vec<ProposalPart> {
        let mut hasher = sha3::Keccak256::new();
        let mut parts = Vec::new();

        // Init
        {
            parts.push(ProposalPart::Init(ProposalInit::new(
                value.height,
                value.round,
                proposer, // Use provided proposer, not self.address
            )));

            hasher.update(value.height.as_u64().to_be_bytes().as_slice());
            hasher.update(value.round.as_i64().to_be_bytes().as_slice());
        }

        // Data (execution payload)
        {
            let has_requests = !execution_requests.is_empty();
            let mut attached_requests = !has_requests;
            for chunk in data.chunks(CHUNK_SIZE) {
                let chunk_bytes = Bytes::copy_from_slice(chunk);
                let chunk_data = if !attached_requests {
                    attached_requests = true;
                    ProposalData::with_execution_requests(chunk_bytes, execution_requests.to_vec())
                } else {
                    ProposalData::new(chunk_bytes)
                };
                if !chunk_data.execution_requests.is_empty() {
                    Self::hash_execution_requests(&mut hasher, &chunk_data.execution_requests);
                }
                hasher.update(chunk_data.bytes.as_ref());
                parts.push(ProposalPart::Data(chunk_data));
            }
            if !attached_requests {
                let chunk_data = ProposalData::with_execution_requests(
                    Bytes::new(),
                    execution_requests.to_vec(),
                );
                if !chunk_data.execution_requests.is_empty() {
                    Self::hash_execution_requests(&mut hasher, &chunk_data.execution_requests);
                }
                hasher.update(chunk_data.bytes.as_ref());
                parts.push(ProposalPart::Data(chunk_data));
            }
        }

        // Blob sidecars (Phase 3: Stream blobs separately)
        if let Some(sidecars) = blob_sidecars {
            for sidecar in sidecars {
                parts.push(ProposalPart::BlobSidecar(sidecar.clone()));

                // Include blob data in signature hash
                hasher.update(sidecar.index.to_be_bytes());
                hasher.update(sidecar.blob.data());
                hasher.update(sidecar.kzg_commitment.as_bytes());
                hasher.update(sidecar.kzg_proof.as_bytes());
            }
        }

        // Fin (signature over all parts)
        {
            let hash = hasher.finalize().to_vec();
            let signature = self.signing_provider.sign(&hash);
            parts.push(ProposalPart::Fin(ProposalFin::new(signature)));
        }

        parts
    }

    /// Returns the set of validators.
    pub fn get_validator_set(&self) -> &ValidatorSet {
        &self.genesis.validator_set
    }

    /// Assemble value from proposal parts and store verified blobs
    ///
    /// This method:
    /// 1. Extracts execution payload and blob sidecars from parts
    /// 2. Verifies and stores blobs using the blob engine
    /// 3. Creates Value with metadata
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - KZG verification fails
    /// - Blob storage fails
    /// - Payload parsing fails
    async fn assemble_and_store_blobs(
        &self,
        parts: ProposalParts,
    ) -> Result<(ProposedValue<LoadContext>, Bytes, Vec<AlloyBytes>, bool), String> {
        // Extract execution payload data
        let total_size: usize =
            parts.parts.iter().filter_map(|part| part.as_data()).map(|data| data.bytes.len()).sum();

        let mut execution_requests: Vec<AlloyBytes> = Vec::new();
        for part in parts.parts.iter().filter_map(|part| part.as_data()) {
            if !part.execution_requests.is_empty() {
                if execution_requests.is_empty() {
                    execution_requests = part.execution_requests.clone();
                } else if execution_requests != part.execution_requests {
                    return Err("Mismatched execution requests across proposal chunks".into());
                }
            }
        }

        Self::validate_execution_requests(&execution_requests)
            .map_err(|err| format!("Invalid execution requests: {}", err))?;

        let mut data = Vec::with_capacity(total_size);
        for part in parts.parts.iter().filter_map(|part| part.as_data()) {
            data.extend_from_slice(&part.bytes);
        }
        let data = Bytes::from(data);

        // Extract blob sidecars
        let blob_sidecars: Vec<_> =
            parts.parts.iter().filter_map(|part| part.as_blob_sidecar()).cloned().collect();
        let has_blobs = !blob_sidecars.is_empty();

        use alloy_rpc_types_engine::ExecutionPayloadV3;
        use ssz::Decode;

        let mut metadata_opt = None;

        let value = if !data.is_empty() {
            match ExecutionPayloadV3::from_ssz_bytes(&data) {
                Ok(execution_payload) => {
                    let requests_hash =
                        Some(ExecutionPayloadHeader::compute_requests_hash(&execution_requests));
                    let header =
                        ExecutionPayloadHeader::from_payload(&execution_payload, requests_hash);
                    let commitments = if has_blobs {
                        blob_sidecars.iter().map(|sidecar| sidecar.kzg_commitment).collect()
                    } else {
                        Vec::new()
                    };
                    let metadata = ValueMetadata::new(header, commitments);
                    metadata_opt = Some(metadata.clone());
                    Value::new(metadata)
                }
                Err(e) => {
                    if has_blobs {
                        return Err(format!(
                            "Failed to decode execution payload with blobs: {:?}",
                            e
                        ));
                    }
                    debug!("Failed to parse ExecutionPayloadV3 from SSZ: {:?}", e);
                    #[allow(deprecated)]
                    Value::from_bytes(data.clone())
                }
            }
        } else {
            if has_blobs {
                return Err("Received blob sidecars without execution payload data".to_string());
            }
            #[allow(deprecated)]
            Value::from_bytes(data.clone())
        };

        if has_blobs {
            let metadata = metadata_opt
                .as_ref()
                .ok_or_else(|| "Missing metadata for blob proposal".to_string())?;

            debug!(
                "Extracted {} blob sidecars from proposal at height {}, round {}",
                blob_sidecars.len(),
                parts.height,
                parts.round
            );

            let _signed_header = self
                .verify_blob_sidecars(parts.height, &parts.proposer, metadata, &blob_sidecars)
                .await?;

            let round_i64 = parts.round.as_i64();

            self.blob_engine
                .verify_and_store(parts.height, round_i64, &blob_sidecars)
                .await
                .map_err(|e| format!("Blob engine error: {}", e))?;

            // Phase 4: Store BlobMetadata (Layer 2) as undecided after verification
            let proposer_index = self.validator_index(&parts.proposer);
            let parent_blob_root =
                if parts.height.as_u64() == 0 { B256::ZERO } else { self.last_blob_sidecar_root };
            let blob_keccak_hashes: Vec<B256> =
                blob_sidecars.iter().map(|sidecar| sidecar.blob_keccak()).collect();

            let blob_metadata = BlobMetadata::new(
                parts.height,
                parent_blob_root,
                metadata.blob_kzg_commitments.clone(),
                blob_keccak_hashes,
                metadata.execution_payload_header.clone(),
                proposer_index,
            );

            self.store
                .put_blob_metadata_undecided(parts.height, parts.round, &blob_metadata)
                .await
                .map_err(|e| format!("Failed to store undecided BlobMetadata: {}", e))?;

            info!(
                "✅ Verified and stored {} blobs for height {}, round {} (BlobMetadata stored)",
                blob_sidecars.len(),
                parts.height,
                parts.round
            );
        }

        if !has_blobs && let Some(metadata) = metadata_opt.as_ref() {
            // Store blobless BlobMetadata (Layer 2) for parent-root chaining
            let proposer_index = self.validator_index(&parts.proposer);
            let parent_blob_root =
                if parts.height.as_u64() == 0 { B256::ZERO } else { self.last_blob_sidecar_root };

            let blob_metadata = BlobMetadata::blobless(
                parts.height,
                parent_blob_root,
                &metadata.execution_payload_header,
                proposer_index,
            );

            self.store
                .put_blob_metadata_undecided(parts.height, parts.round, &blob_metadata)
                .await
                .map_err(|e| format!("Failed to store blobless BlobMetadata: {}", e))?;

            debug!(
                "Stored blobless BlobMetadata for height {}, round {}",
                parts.height, parts.round
            );
        }

        let proposed_value = ProposedValue {
            height: parts.height,
            round: parts.round,
            valid_round: Round::Nil,
            proposer: parts.proposer,
            value,
            validity: Validity::Valid,
        };
        Ok((proposed_value, data, execution_requests, has_blobs))
    }

    async fn verify_blob_sidecars(
        &self,
        height: Height,
        proposer: &Address,
        metadata: &ValueMetadata,
        sidecars: &[BlobSidecar],
    ) -> Result<SignedBeaconBlockHeader, String> {
        if sidecars.is_empty() {
            return Err("verify_blob_sidecars called with empty sidecars".to_string());
        }

        let signed_header = sidecars[0].signed_block_header.clone();
        for sidecar in sidecars.iter().skip(1) {
            if sidecar.signed_block_header != signed_header {
                return Err("Inconsistent signed headers across blob sidecars".to_string());
            }
        }

        if signed_header.message.slot != height.as_u64() {
            return Err(format!(
                "Signed header slot {} does not match proposal height {}",
                signed_header.message.slot,
                height.as_u64()
            ));
        }

        let proposer_index = self
            .validator_index(proposer)
            .ok_or_else(|| format!("Proposer {} not found in validator set", proposer))?;

        if signed_header.message.proposer_index != proposer_index {
            return Err(format!(
                "Signed header proposer_index {} does not match expected {}",
                signed_header.message.proposer_index, proposer_index
            ));
        }

        let validator = self
            .get_validator_set()
            .get_by_address(proposer)
            .ok_or_else(|| format!("Proposer {} not found in validator set", proposer))?;

        if !signed_header.verify_signature(&validator.public_key) {
            return Err("Invalid beacon header signature".to_string());
        }

        let commitments = &metadata.blob_kzg_commitments;
        if commitments.len() != sidecars.len() {
            return Err(format!(
                "Metadata reports {} blobs but received {} sidecars",
                commitments.len(),
                sidecars.len()
            ));
        }

        let body = BeaconBlockBodyMinimal::from_ultramarine_data(
            commitments.clone(),
            &metadata.execution_payload_header,
        );
        let computed_body_root = body.compute_body_root();

        if signed_header.message.body_root != computed_body_root {
            return Err("Beacon header body_root mismatch".to_string());
        }

        info!(
            height = %height,
            current_cache_height = %self.last_blob_sidecar_height,
            current_cache_root = ?self.last_blob_sidecar_root,
            proposal_parent_root = ?signed_header.message.parent_root,
            "🔍 VALIDATION: Checking proposal parent root - Current cache state"
        );

        let expected_parent_root = if height.as_u64() == 0 {
            B256::ZERO
        } else {
            let prev_height = Height::new(height.as_u64() - 1);
            match self.store.get_blob_metadata(prev_height).await {
                Ok(Some(prev_metadata)) => {
                    let parent_root = prev_metadata.to_beacon_header().hash_tree_root();
                    info!(
                        height = %height,
                        parent_height = %prev_height,
                        parent_root = ?parent_root,
                        "✅ VALIDATION: Loaded parent metadata from store"
                    );
                    parent_root
                }
                Ok(None) => {
                    if prev_height == self.last_blob_sidecar_height {
                        warn!(
                            height = %height,
                            parent_height = %prev_height,
                            cached_root = ?self.last_blob_sidecar_root,
                            "⚠️  VALIDATION: Missing decided BlobMetadata for parent height {}; using cached blob root",
                            prev_height.as_u64()
                        );
                        self.last_blob_sidecar_root
                    } else {
                        error!(
                            height = %height,
                            parent_height = %prev_height,
                            cache_height = %self.last_blob_sidecar_height,
                            cache_root = ?self.last_blob_sidecar_root,
                            "❌ VALIDATION: Cache mismatch - cache points to height {} but need height {}",
                            self.last_blob_sidecar_height.as_u64(),
                            prev_height.as_u64()
                        );
                        return Err(format!(
                            "Missing decided BlobMetadata for parent height {}",
                            prev_height.as_u64()
                        ));
                    }
                }
                Err(e) => {
                    error!(
                        height = %height,
                        parent_height = %prev_height,
                        error = %e,
                        "❌ VALIDATION: Failed to load parent BlobMetadata"
                    );
                    return Err(format!(
                        "Failed to load BlobMetadata for parent height {}: {}",
                        prev_height.as_u64(),
                        e
                    ));
                }
            }
        };

        if signed_header.message.parent_root != expected_parent_root {
            return Err("Beacon header parent_root mismatch".to_string());
        }

        for sidecar in sidecars {
            let index = usize::from(sidecar.index);
            if index >= commitments.len() {
                return Err(format!(
                    "Blob index {} out of range {}",
                    sidecar.index,
                    commitments.len()
                ));
            }

            if commitments[index] != sidecar.kzg_commitment {
                return Err(format!("Commitment mismatch for blob index {}", sidecar.index));
            }

            let proof = &sidecar.kzg_commitment_inclusion_proof;

            let (commitments_root, reconstructed_body_root) =
                match reconstruct_roots_from_proof(&sidecar.kzg_commitment, proof, index) {
                    Ok(values) => {
                        let (commitments_root, reconstructed_body_root) = values;
                        debug!(
                            blob_index = sidecar.index,
                            commitments_root = ?commitments_root,
                            reconstructed_body_root = ?reconstructed_body_root,
                            header_body_root = ?signed_header.message.body_root,
                            "Evaluated blob inclusion proof"
                        );
                        (commitments_root, reconstructed_body_root)
                    }
                    Err(e) => {
                        error!(
                            blob_index = sidecar.index,
                            error = %e,
                            "Malformed blob inclusion proof"
                        );
                        return Err(format!(
                            "Invalid KZG inclusion proof for blob index {}",
                            sidecar.index
                        ));
                    }
                };

            if proof !=
                &generate_kzg_commitment_inclusion_proof(commitments, index, &body).map_err(
                    |e| {
                        format!(
                            "Failed to recompute inclusion proof for blob {}: {}",
                            sidecar.index, e
                        )
                    },
                )?
            {
                warn!(
                    blob_index = sidecar.index,
                    "Non-canonical inclusion proof received; recomputing locally"
                );
            }

            if !verify_kzg_commitment_inclusion_proof(
                &sidecar.kzg_commitment,
                proof,
                index,
                signed_header.message.body_root,
            ) {
                error!(
                    blob_index = sidecar.index,
                    commitments_root = ?commitments_root,
                    reconstructed_body_root = ?reconstructed_body_root,
                    header_body_root = ?signed_header.message.body_root,
                    commitments_len = commitments.len(),
                    "KZG inclusion proof failed verification"
                );
                return Err(format!("Invalid KZG inclusion proof for blob index {}", sidecar.index));
            }
        }

        Ok(signed_header)
    }

    fn hash_execution_requests(hasher: &mut sha3::Keccak256, execution_requests: &[AlloyBytes]) {
        if execution_requests.is_empty() {
            return;
        }

        hasher.update((execution_requests.len() as u32).to_be_bytes());
        for request in execution_requests {
            hasher.update((request.len() as u32).to_be_bytes());
            hasher.update(request.as_ref());
        }
    }

    fn validate_execution_requests(requests: &[AlloyBytes]) -> Result<(), String> {
        if requests.len() > MAX_EXECUTION_REQUESTS {
            return Err(format!(
                "execution request count {} exceeds limit of {}",
                requests.len(),
                MAX_EXECUTION_REQUESTS
            ));
        }

        let mut prev_type: Option<u8> = None;
        for (idx, request) in requests.iter().enumerate() {
            if request.len() <= 1 {
                return Err(format!("execution request {} must include type byte and payload", idx));
            }

            let request_type = request[0];
            if let Some(prev) = prev_type &&
                request_type <= prev
            {
                return Err(format!(
                    "execution requests must be strictly increasing by type (index {}, prev {}, current {})",
                    idx, prev, request_type
                ));
            }
            prev_type = Some(request_type);
        }

        Ok(())
    }

    /// Verifies the signature of the proposal.
    /// Returns `Ok(())` if the signature is valid, or an appropriate `SignatureVerificationError`.
    fn verify_proposal_signature(
        &self,
        parts: &ProposalParts,
    ) -> Result<(), SignatureVerificationError> {
        let mut hasher = sha3::Keccak256::new();
        let mut signature = None;

        // Recreate the hash and extract the signature during traversal
        for part in &parts.parts {
            match part {
                ProposalPart::Init(init) => {
                    hasher.update(init.height.as_u64().to_be_bytes());
                    hasher.update(init.round.as_i64().to_be_bytes());
                }
                ProposalPart::Data(data) => {
                    Self::hash_execution_requests(&mut hasher, &data.execution_requests);
                    hasher.update(data.bytes.as_ref());
                }
                // Phase 3: Include blob sidecars in signature hash
                ProposalPart::BlobSidecar(sidecar) => {
                    hasher.update(sidecar.index.to_be_bytes());
                    hasher.update(sidecar.blob.data());
                    hasher.update(sidecar.kzg_commitment.as_bytes());
                    hasher.update(sidecar.kzg_proof.as_bytes());
                }
                ProposalPart::Fin(fin) => {
                    signature = Some(&fin.signature);
                }
                ProposalPart::ArchiveNotice(_) => {
                    return Err(SignatureVerificationError::InvalidSignature);
                }
            }
        }

        let hash = hasher.finalize();
        let signature = signature.ok_or(SignatureVerificationError::MissingFinPart)?;

        // Retrieve the public key of the proposer
        let public_key =
            self.get_validator_set().get_by_address(&parts.proposer).map(|v| v.public_key);

        let public_key = public_key.ok_or(SignatureVerificationError::ProposerNotFound)?;

        // Verify the signature
        if !self.signing_provider.verify(&hash, signature, &public_key) {
            return Err(SignatureVerificationError::InvalidSignature);
        }

        Ok(())
    }
}

fn tree_hash_to_fixed(hash: tree_hash::Hash256) -> FixedHash {
    FixedHash::from_slice(hash.as_ref())
}

fn b256_to_fixed(value: &B256) -> FixedHash {
    FixedHash::from_slice(value.as_slice())
}

fn fixed_to_b256(hash: &FixedHash) -> B256 {
    B256::from_slice(hash.as_slice())
}

fn merkle_root_from_branch(
    mut value: FixedHash,
    branch: &[FixedHash],
    mut index: usize,
) -> FixedHash {
    for sibling in branch {
        if index & 1 == 1 {
            value = FixedHash::from_slice(&hash32_concat(sibling.as_slice(), value.as_slice()));
        } else {
            value = FixedHash::from_slice(&hash32_concat(value.as_slice(), sibling.as_slice()));
        }
        index >>= 1;
    }
    value
}

fn reconstruct_roots_from_proof(
    commitment: &KzgCommitment,
    proof: &[B256],
    index: usize,
) -> Result<(B256, B256), String> {
    let commitments_depth = MAX_BLOB_COMMITMENTS_PER_BLOCK.next_power_of_two().ilog2() as usize;
    let commitments_branch_length = commitments_depth + 1;

    if proof.len() != KZG_COMMITMENT_INCLUSION_PROOF_DEPTH ||
        commitments_branch_length > KZG_COMMITMENT_INCLUSION_PROOF_DEPTH ||
        index >= MAX_BLOB_COMMITMENTS_PER_BLOCK
    {
        return Err(format!(
            "Proof metadata invalid: len={}, branch_len={}, index={}",
            proof.len(),
            commitments_branch_length,
            index
        ));
    }

    let (commitment_branch, body_branch) = proof.split_at(commitments_branch_length);
    let commitment_branch_fixed: Vec<FixedHash> =
        commitment_branch.iter().map(b256_to_fixed).collect();
    let body_branch_fixed: Vec<FixedHash> = body_branch.iter().map(b256_to_fixed).collect();

    if commitment_branch_fixed.len() != commitments_branch_length ||
        commitment_branch_fixed.len() + body_branch_fixed.len() !=
            KZG_COMMITMENT_INCLUSION_PROOF_DEPTH
    {
        return Err(format!(
            "Proof branch sizing invalid: commitment_branch={}, body_branch={}, total={}",
            commitment_branch_fixed.len(),
            body_branch_fixed.len(),
            proof.len()
        ));
    }

    let leaf = tree_hash_to_fixed(TreeHash::tree_hash_root(commitment));
    let commitments_root = merkle_root_from_branch(leaf, &commitment_branch_fixed, index);
    let reconstructed_body_root =
        merkle_root_from_branch(commitments_root, &body_branch_fixed, BLOB_KZG_COMMITMENTS_INDEX);

    Ok((fixed_to_b256(&commitments_root), fixed_to_b256(&reconstructed_body_root)))
}

#[cfg(test)]
mod tests;

/// Decodes a Value from its byte representation using ProtobufCodec
pub fn decode_value(bytes: Bytes) -> Value {
    ProtobufCodec.decode(bytes).expect("panic during protobuf velue decode")
}

pub const BLOB_RETENTION_WINDOW: u64 = 4_096;

fn versioned_hashes_from_commitments(commitments: &[KzgCommitment]) -> Vec<BlockHash> {
    commitments
        .iter()
        .map(|commitment| {
            let mut hash = Sha2Sha256::digest(commitment.as_bytes());
            hash[0] = 0x01; // EIP-4844 versioned hash prefix
            BlockHash::from_slice(&hash)
        })
        .collect()
}

fn versioned_hashes_from_sidecars(sidecars: &[BlobSidecar]) -> Vec<BlockHash> {
    sidecars
        .iter()
        .map(|sidecar| {
            let mut hash = Sha2Sha256::digest(sidecar.kzg_commitment.as_bytes());
            hash[0] = 0x01; // EIP-4844 versioned hash prefix
            BlockHash::from_slice(&hash)
        })
        .collect()
}
