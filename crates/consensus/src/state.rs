//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like
//! RPC.

use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
};

use bytes::Bytes;
use color_eyre::eyre;
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
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use ultramarine_blob_engine::{BlobEngine, BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
use ultramarine_types::{
    address::Address,
    // Phase 3: Import blob types for streaming
    aliases::B256,
    blob::BlobsBundle,
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
    signing::{Ed25519Provider, Signature},
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

    // For stats
    pub txs_count: u64,
    pub chain_bytes: u64,
    pub start_time: Instant,
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
    /// * Other parameters remain the same
    pub fn new(
        genesis: Genesis,
        ctx: LoadContext,
        signing_provider: Ed25519Provider,
        address: Address,
        height: Height,
        store: Store,
        blob_engine: E,
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
            peers: HashSet::new(),

            latest_block: None,
            blob_rounds: HashMap::new(),
            last_blob_sidecar_root: B256::ZERO,
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

    /// Phase 4: Hydrate blob parent root from latest decided BlobMetadata (Layer 2)
    ///
    /// This MUST be called on startup to restore the parent_root cache from Layer 2 metadata.
    /// The cache is updated ONLY when metadata becomes canonical (at commit).
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
            info!("No BlobMetadata found, starting with zero parent root");
            self.last_blob_sidecar_root = B256::ZERO;
        }

        Ok(())
    }

    /// Phase 4: Legacy method - kept for backward compatibility during migration
    ///
    /// DEPRECATED: Use `hydrate_blob_parent_root()` instead.
    /// This method loads from old BlobSidecar headers and will be removed in Phase 4 cleanup.
    #[deprecated(
        since = "0.4.0",
        note = "Use hydrate_blob_parent_root() with BlobMetadata instead"
    )]
    pub async fn hydrate_blob_sidecar_root(&mut self) -> eyre::Result<()> {
        if let Some((_, header)) =
            self.store.get_latest_blob_sidecar_header().await.map_err(|e| eyre::Report::new(e))?
        {
            self.last_blob_sidecar_root = header.message.hash_tree_root();
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

    pub async fn persist_blob_sidecar_header(
        &self,
        height: Height,
        header: &SignedBeaconBlockHeader,
    ) -> eyre::Result<()> {
        self.store.put_blob_sidecar_header(height, header).await.map_err(|e| eyre::Report::new(e))
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

                let inclusion_proof = generate_kzg_commitment_inclusion_proof(&commitments, index)
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
            self.blob_rounds.entry(parts.height).or_insert_with(HashSet::new).insert(round_i64);
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
                .map(|validator| TreeHash256::from_slice(&validator.address.into_inner()))
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

        // LAYER 3: Mark blobs as decided in blob engine
        // CRITICAL: This MUST succeed to maintain data availability guarantee.
        // If blob promotion fails, we cannot finalize the block because the execution layer
        // would be unable to retrieve blobs for import, breaking the DA layer.
        let round_i64 = certificate.round.as_i64();
        self.blob_engine.mark_decided(certificate.height, round_i64).await.map_err(|e| {
            error!(
                height = %certificate.height,
                round = %certificate.round,
                error = %e,
                "CRITICAL: Failed to mark blobs as decided - aborting commit to preserve DA"
            );
            eyre::eyre!(
                "Cannot finalize block at height {} round {} without blob availability: {}",
                certificate.height,
                certificate.round,
                e
            )
        })?;

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

        // Prune the store, keep the last 5 heights
        let retain_height = Height::new(certificate.height.as_u64().saturating_sub(5));
        self.store.prune(retain_height).await?;

        // Clean up orphaned blobs from failed rounds before advancing height
        // Any blobs that were stored but not marked as decided are orphaned and should be removed
        if let Some(rounds) = self.blob_rounds.get(&certificate.height) {
            for &round in rounds.iter() {
                // Skip the decided round
                if round != round_i64 {
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
        // The prune_archived_before() call removes blobs that have been archived before the given
        // height
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
        let parent_blob_root = if height.as_u64() == 0 {
            B256::ZERO
        } else {
            self.last_blob_sidecar_root
        };

        let blob_metadata = if commitments.is_empty() {
            // Blobless block - still need metadata for parent-root chaining
            BlobMetadata::blobless(height, parent_blob_root, &header, proposer_index)
        } else {
            // Blobbed block
            BlobMetadata::new(height, parent_blob_root, commitments, header, proposer_index)
        };

        self.store
            .put_blob_metadata_undecided(height, round, &blob_metadata)
            .await
            .map_err(|e| {
                error!(
                    height = %height,
                    round = %round,
                    error = %e,
                    "Failed to store undecided BlobMetadata for proposal"
                );
                eyre::eyre!("Cannot propose without storing BlobMetadata: {}", e)
            })?;

        debug!(
            height = %height,
            round = %round,
            blob_count = blob_metadata.blob_count(),
            "Stored undecided BlobMetadata for proposal"
        );

        Ok(LocallyProposedValue::new(proposal.height, proposal.round, proposal.value))
    }

    fn stream_id(&mut self) -> StreamId {
        let mut bytes = Vec::with_capacity(size_of::<u64>() + size_of::<u32>());
        bytes.extend_from_slice(&self.current_height.as_u64().to_be_bytes());
        bytes.extend_from_slice(&self.current_round.as_u32().unwrap().to_be_bytes());
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
        let parts = self.make_proposal_parts(value, data, blob_sidecars, proposer_address);

        let stream_id = self.stream_id();

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
                hasher.update(&sidecar.index.to_be_bytes());
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

            let signed_header = self
                .verify_blob_sidecars(parts.height, &parts.proposer, metadata, &blob_sidecars)
                .await?;

            let round_i64 = parts.round.as_i64();

            self.blob_engine
                .verify_and_store(parts.height, round_i64, &blob_sidecars)
                .await
                .map_err(|e| format!("Blob engine error: {}", e))?;

            self.persist_blob_sidecar_header(parts.height, &signed_header)
                .await
                .map_err(|e| format!("Failed to store blob sidecar header: {}", e))?;

            // Phase 4: Store BlobMetadata (Layer 2) as undecided after verification
            let proposer_index = self.validator_index(&parts.proposer);
            let parent_blob_root = if parts.height.as_u64() == 0 {
                B256::ZERO
            } else {
                self.last_blob_sidecar_root
            };

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

        if !has_blobs {
            if let Some(metadata) = metadata_opt.as_ref() {
                let body = BeaconBlockBodyMinimal::from_ultramarine_data(
                    metadata.blob_kzg_commitments.clone(),
                    &metadata.execution_payload_header,
                );
                let body_root = body.compute_body_root();
                let header_message = self
                    .build_sidecar_header_message(
                        parts.height,
                        &parts.proposer,
                        metadata,
                        body_root,
                    )
                    .map_err(|e| format!("Failed to build blob sidecar header: {}", e))?;

                let signed_header =
                    SignedBeaconBlockHeader::new(header_message, Signature::from_bytes([0u8; 64]));

                self.persist_blob_sidecar_header(parts.height, &signed_header)
                    .await
                    .map_err(|e| format!("Failed to store blob sidecar header: {}", e))?;

                // Phase 4: Store blobless BlobMetadata (Layer 2) for parent-root chaining
                let proposer_index = self.validator_index(&parts.proposer);
                let parent_blob_root = if parts.height.as_u64() == 0 {
                    B256::ZERO
                } else {
                    self.last_blob_sidecar_root
                };

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
            match self.store.get_blob_sidecar_header(prev_height).await {
                Ok(Some(prev_header)) => prev_header.message.hash_tree_root(),
                Ok(None) => {
                    return Err(format!(
                        "Missing blob sidecar header for parent height {}",
                        prev_height.as_u64()
                    ));
                }
                Err(e) => {
                    return Err(format!(
                        "Failed to load blob sidecar header for parent height {}: {}",
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

            if !verify_kzg_commitment_inclusion_proof(
                &sidecar.kzg_commitment,
                &sidecar.kzg_commitment_inclusion_proof,
                index,
                signed_header.message.body_root,
            ) {
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
                    hasher.update(&sidecar.index.to_be_bytes());
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

/// Decodes a Value from its byte representation using ProtobufCodec
pub fn decode_value(bytes: Bytes) -> Value {
    ProtobufCodec.decode(bytes).expect("panic during protobuf velue decode")
}
