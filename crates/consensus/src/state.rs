//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like
//! RPC.

use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
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
        core::{CommitCertificate, Round, Validity},
    },
};
use rand::{Rng, SeedableRng, rngs::StdRng};
use sha3::Digest;
use ssz::Decode;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use tree_hash::TreeHash;
use ultramarine_blob_engine::{BlobEngine, BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
use ultramarine_execution::notifier::ExecutionNotifier;
use ultramarine_types::{
    address::Address,
    // Phase 3: Import blob types for streaming
    aliases::{B256, Block, BlockHash},
    blob::{BlobsBundle, KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK},
    // Phase 4: Import three-layer metadata types
    blob_metadata::BlobMetadata,
    codec::proto::ProtobufCodec,
    consensus_block_metadata::ConsensusBlockMetadata,
    context::LoadContext,
    engine_api::{ExecutionBlock, ExecutionPayloadHeader},
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
    blob_engine: E,
    pub(crate) blob_metrics: ultramarine_blob_engine::BlobEngineMetrics,

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

    /// Retention window (in heights) for decided blobs.
    blob_retention_window: u64,

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
    /// * `blob_engine` - The blob engine for verification and storage
    /// * `blob_metrics` - Metrics for blob operations (cloneable, shared via Arc)
    /// * Other parameters remain the same
    pub fn new(
        genesis: Genesis,
        ctx: LoadContext,
        signing_provider: Ed25519Provider,
        address: Address,
        height: Height,
        store: Store,
        blob_engine: E,
        blob_metrics: ultramarine_blob_engine::BlobEngineMetrics,
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
            peers: HashSet::new(),

            latest_block: None,
            blob_rounds: HashMap::new(),
            last_blob_sidecar_root: B256::ZERO,
            blob_retention_window: BLOB_RETENTION_WINDOW,
            txs_count: 0,
            chain_bytes: 0,
            start_time: Instant::now(),
        }
    }

    /// Returns the earliest height available in the state
    pub async fn get_earliest_height(&self) -> Height {
        self.store.min_decided_value_height().await.unwrap_or_default()
    }

    /// Returns a reference to the blob engine for blob operations
    pub fn blob_engine(&self) -> &E {
        &self.blob_engine
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
    ) -> eyre::Result<()> {
        self.store
            .store_undecided_block_data(height, round, data)
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
        let (value, data, has_blobs) = match self.assemble_and_store_blobs(parts.clone()).await {
            Ok((value, data, has_blobs)) => (value, data, has_blobs),
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
        self.store_undecided_proposal_data(data).await?;

        Ok(Some(value))
    }

    pub async fn store_undecided_proposal_data(&mut self, data: Bytes) -> eyre::Result<()> {
        self.store
            .store_undecided_block_data(self.current_height, self.current_round, data)
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
    ) -> eyre::Result<()> {
        self.store
            .store_undecided_block_data(height, round, data)
            .await
            .map_err(|e| eyre::Report::new(e))
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
        match package {
            SyncedValuePackage::Full { value, execution_payload_ssz, blob_sidecars } => {
                info!(
                    height = %height,
                    round = %round,
                    payload_size = execution_payload_ssz.len(),
                    blob_count = blob_sidecars.len(),
                    "Processing synced value package"
                );

                let value_metadata = value.metadata.clone();

                self.store_synced_block_data(height, round, execution_payload_ssz).await?;

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
                        value_metadata.execution_payload_header.clone(),
                        proposer_index,
                    )
                };

                self.put_blob_metadata_undecided(height, round, &blob_metadata).await?;

                let proposed_value = ProposedValue {
                    height,
                    round,
                    valid_round: Round::Nil,
                    proposer,
                    value,
                    validity: Validity::Valid,
                };

                self.store_synced_proposal(proposed_value.clone()).await?;

                Ok(Some(proposed_value))
            }
            SyncedValuePackage::MetadataOnly { value: _ } => {
                error!(
                    height = %height,
                    round = %round,
                    "Received metadata-only sync package during pre-v0"
                );
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

        if versioned_hashes.is_empty() {
            if let Some(proposal) = self.load_undecided_proposal(height, round).await? {
                let commitments = proposal.value.metadata.blob_kzg_commitments.clone();
                if !commitments.is_empty() {
                    use sha2::{Digest as Sha2Digest, Sha256 as Sha2Sha256};
                    versioned_hashes = commitments
                        .iter()
                        .map(|commitment| {
                            let mut hash = Sha2Sha256::digest(commitment.as_bytes());
                            hash[0] = 0x01;
                            BlockHash::from_slice(&hash)
                        })
                        .collect();
                }
            }
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
            use sha2::{Digest, Sha256};
            let computed_hashes: Vec<BlockHash> = blobs
                .iter()
                .map(|sidecar| {
                    let mut hash = Sha256::digest(sidecar.kzg_commitment.as_bytes());
                    hash[0] = 0x01;
                    BlockHash::from_slice(&hash)
                })
                .collect();

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
            .notify_new_block(execution_payload.clone(), versioned_hashes.clone())
            .await
            .map_err(|e| eyre::eyre!("Execution layer new_payload failed: {}", e))?;
        if payload_status.is_invalid() {
            return Err(eyre::eyre!("Invalid payload status: {}", payload_status.status));
        }

        let payload_inner = &execution_payload.payload_inner.payload_inner;
        let block_hash = payload_inner.block_hash;
        let block_number = payload_inner.block_number;
        let parent_block_hash = payload_inner.parent_hash;
        let prev_randao = payload_inner.prev_randao;
        let tx_count = payload_inner.transactions.len();

        let expected_parent = self.latest_block.as_ref().map(|block| block.block_hash);
        if let Some(expected_parent_hash) = expected_parent {
            if expected_parent_hash != parent_block_hash {
                return Err(eyre::eyre!(
                    "Parent hash mismatch at height {}: expected {:?} but payload declares {:?}",
                    height,
                    expected_parent_hash,
                    parent_block_hash
                ));
            }
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
            prev_randao,
        };
        self.latest_block = Some(execution_block.clone());

        Ok(DecidedOutcome {
            execution_block,
            payload_status,
            tx_count,
            block_bytes: execution_payload_ssz.len(),
            blob_count,
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

        let proposal = match proposal {
            Ok(Some(proposal)) => proposal,
            Ok(None) => {
                error!(
                    height = %certificate.height,
                    round = %certificate.round,
                    "Trying to commit a value that is not decided"
                );

                return Ok(()); // FIXME: Return an actual error and handle in caller
            }
            Err(e) => return Err(e.into()),
        };

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
        // If this fails, we abort the commit because the Ethereum compatibility layer is broken.
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

        // NOTE: Blob promotion to decided state is handled by process_decided_certificate
        // before validation and EL notification, not here in commit().
        // This ensures proper ordering: validate first, promote only on success.

        // Track round for cleanup of orphaned blobs
        let round_i64 = certificate.round.as_i64();

        // Store block data for decided value
        let block_data = self.store.get_block_data(certificate.height, certificate.round).await?;

        // Log first 32 bytes of block data with JNT prefix
        if let Some(data) = &block_data &&
            data.len() >= 32
        {
            info!("Committed block_data[0..32]: {}", hex::encode(&data[..32]));
        }

        if let Some(data) = block_data {
            self.store.store_decided_block_data(certificate.height, data).await?;
        }

        // Phase 4: Cache update moved above (after BlobMetadata promotion)
        // Old blob sidecar header loading removed - we now use BlobMetadata

        // Prune the store, keeping recent decided heights within the retention window
        let window = self.blob_retention_window.max(1);
        let retain_floor = certificate.height.as_u64().saturating_sub(window - 1);
        let retain_height = Height::new(retain_floor);
        self.store.prune(retain_height).await?;

        // Clean up orphaned blobs from failed rounds before advancing height
        // Any blobs that were stored but not marked as decided are orphaned and should be removed
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
                        .delete_blob_metadata_undecided(certificate.height, Round::new(round_u32))
                        .await
                    {
                        warn!(
                            height = %certificate.height,
                            round = round,
                            error = %e,
                            "Failed to delete undecided BlobMetadata for failed round"
                        );
                    }

                    if let Err(e) = self.blob_engine.drop_round(certificate.height, round).await {
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

        // Prune blob engine - keep the same retention policy (last 5 heights)
        // NOTE: This is a basic, non-configurable implementation for Phase 5 testing.
        // Phase 6 will add configurable retention policies (CLI flags, multiple strategies, etc.)
        // For now, blobs older than 5 heights are permanently deleted from RocksDB.
        match self.blob_engine.prune_archived_before(retain_height).await {
            Ok(count) if count > 0 => {
                debug!("Pruned {} blobs before height {}", count, retain_height.as_u64());
            }
            Ok(_) => {} // No blobs to prune
            Err(e) => {
                error!(
                    error = %e,
                    height = %retain_height,
                    "Failed to prune blobs from blob engine"
                );
                // Don't fail the commit if blob pruning fails - just log the error
            }
        }

        // Also clean up blob round tracking for old heights
        self.blob_rounds.retain(|h, _| *h >= retain_height);

        // Move to next height
        self.current_height = self.current_height.increment();
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
        blobs_bundle: Option<&BlobsBundle>,
    ) -> eyre::Result<LocallyProposedValue<LoadContext>> {
        assert_eq!(height, self.current_height);
        assert_eq!(round, self.current_round);

        // Phase 2: Extract lightweight header from execution payload
        let header = ExecutionPayloadHeader::from_payload(execution_payload);

        // Phase 2: Extract KZG commitments from blobs bundle
        let commitments = blobs_bundle.map(|bundle| bundle.commitments.clone()).unwrap_or_default();
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
            BlobMetadata::new(height, parent_blob_root, commitments, header, proposer_index)
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
        proposer: Option<Address>, // For RestreamProposal support
    ) -> impl Iterator<Item = StreamMessage<ProposalPart>> {
        // Use provided proposer (for restreaming) or default to self.address (for our own
        // proposals)
        let proposer_address = proposer.unwrap_or(self.address);
        let proposal_height = value.height;
        let proposal_round = value.round;
        let parts = self.make_proposal_parts(value, data, blob_sidecars, proposer_address);

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

    /// Phase 3: Updated to stream blobs as BlobSidecar parts
    fn make_proposal_parts(
        &self,
        value: LocallyProposedValue<LoadContext>,
        data: Bytes,
        blob_sidecars: Option<&[BlobSidecar]>,
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
            for chunk in data.chunks(CHUNK_SIZE) {
                let chunk_data = ProposalData::new(Bytes::copy_from_slice(chunk));
                parts.push(ProposalPart::Data(chunk_data));
                hasher.update(chunk);
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
    ) -> Result<(ProposedValue<LoadContext>, Bytes, bool), String> {
        // Extract execution payload data
        let total_size: usize =
            parts.parts.iter().filter_map(|part| part.as_data()).map(|data| data.bytes.len()).sum();

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
                    let header = ExecutionPayloadHeader::from_payload(&execution_payload);
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

            let blob_metadata = BlobMetadata::new(
                parts.height,
                parent_blob_root,
                metadata.blob_kzg_commitments.clone(),
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
        Ok((proposed_value, data, has_blobs))
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

        let expected_parent_root = if height.as_u64() == 0 {
            B256::ZERO
        } else {
            let prev_height = Height::new(height.as_u64() - 1);
            match self.store.get_blob_metadata(prev_height).await {
                Ok(Some(prev_metadata)) => prev_metadata.to_beacon_header().hash_tree_root(),
                Ok(None) => {
                    return Err(format!(
                        "Missing decided BlobMetadata for parent height {}",
                        prev_height.as_u64()
                    ));
                }
                Err(e) => {
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
