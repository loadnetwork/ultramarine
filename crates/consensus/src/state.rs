//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like
//! RPC.

use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
};

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
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use tree_hash::TreeHash;
use ultramarine_blob_engine::{BlobEngine, BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
use ultramarine_types::{
    address::Address,
    // Phase 3: Import blob types for streaming
    aliases::B256,
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
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address as AlloyAddress, B256, Bytes as AlloyBytes};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use alloy_rpc_types_eth::Withdrawal;
    use async_trait::async_trait;
    use bytes::Bytes as NetworkBytes;
    use tempfile::{TempDir, tempdir};
    use ultramarine_blob_engine::error::BlobEngineError;
    use ultramarine_types::{
        address::Address,
        aliases::Bytes as BlobBytes,
        blob::{BYTES_PER_BLOB, Blob, BlobsBundle, KzgCommitment, KzgProof},
        blob_metadata::BlobMetadata,
        engine_api::ExecutionPayloadHeader,
        genesis::Genesis,
        signing::{Ed25519Provider, PrivateKey},
        validator_set::{Validator, ValidatorSet},
        value::Value,
        value_metadata::ValueMetadata,
    };

    use super::*;
    use crate::metrics::DbMetrics;

    #[derive(Default, Clone)]
    struct MockBlobEngine {
        inner: Arc<Mutex<MockBlobEngineState>>,
    }

    fn sample_blob_metadata(height: Height, parent_blob_root: B256) -> BlobMetadata {
        BlobMetadata::new(
            height,
            parent_blob_root,
            vec![KzgCommitment::new([42u8; 48])],
            sample_execution_payload_header(),
            Some(0),
        )
    }

    #[derive(Default)]
    struct MockBlobEngineState {
        verify_calls: Vec<(Height, i64, usize)>,
        mark_decided_calls: Vec<(Height, i64)>,
        drop_calls: Vec<(Height, i64)>,
    }

    impl MockBlobEngine {
        fn verify_calls(&self) -> Vec<(Height, i64, usize)> {
            self.inner.lock().unwrap().verify_calls.clone()
        }

        fn mark_decided_calls(&self) -> Vec<(Height, i64)> {
            self.inner.lock().unwrap().mark_decided_calls.clone()
        }

        fn drop_calls(&self) -> Vec<(Height, i64)> {
            self.inner.lock().unwrap().drop_calls.clone()
        }
    }

    #[async_trait]
    impl BlobEngine for MockBlobEngine {
        async fn verify_and_store(
            &self,
            height: Height,
            round: i64,
            sidecars: &[BlobSidecar],
        ) -> Result<(), BlobEngineError> {
            self.inner.lock().unwrap().verify_calls.push((height, round, sidecars.len()));
            Ok(())
        }

        async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
            self.inner.lock().unwrap().mark_decided_calls.push((height, round));
            Ok(())
        }

        async fn get_for_import(
            &self,
            _height: Height,
        ) -> Result<Vec<BlobSidecar>, BlobEngineError> {
            Ok(Vec::new())
        }

        async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
            self.inner.lock().unwrap().drop_calls.push((height, round));
            Ok(())
        }

        async fn mark_archived(
            &self,
            _height: Height,
            _indices: &[u16],
        ) -> Result<(), BlobEngineError> {
            Ok(())
        }

        async fn prune_archived_before(&self, _height: Height) -> Result<usize, BlobEngineError> {
            Ok(0)
        }

        async fn get_undecided_blobs(
            &self,
            _height: Height,
            _round: i64,
        ) -> Result<Vec<BlobSidecar>, BlobEngineError> {
            Ok(Vec::new())
        }
    }

    fn sample_blob_bundle(count: usize) -> BlobsBundle {
        let mut commitments = Vec::with_capacity(count);
        let mut proofs = Vec::with_capacity(count);
        let mut blobs = Vec::with_capacity(count);

        for i in 0..count {
            commitments.push(KzgCommitment::new([i as u8; 48]));
            proofs.push(KzgProof::new([i as u8; 48]));
            let data = vec![i as u8; BYTES_PER_BLOB];
            blobs.push(Blob::new(AlloyBytes::from(data)).expect("blob"));
        }

        BlobsBundle::new(commitments, proofs, blobs)
    }

    fn sample_execution_payload_header() -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            block_hash: B256::from([1u8; 32]),
            parent_hash: B256::from([2u8; 32]),
            state_root: B256::from([3u8; 32]),
            receipts_root: B256::from([4u8; 32]),
            logs_bloom: alloy_primitives::Bloom::ZERO,
            block_number: 1,
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            timestamp: 1_700_000_000,
            base_fee_per_gas: alloy_primitives::U256::from(1),
            blob_gas_used: 0,
            excess_blob_gas: 0,
            prev_randao: B256::from([5u8; 32]),
            fee_recipient: Address::new([6u8; 20]),
            extra_data: AlloyBytes::new(),
            transactions_root: B256::from([7u8; 32]),
            withdrawals_root: B256::from([8u8; 32]),
        }
    }

    fn sample_execution_payload_v3() -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            blob_gas_used: 0,
            excess_blob_gas: 0,
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::from([2u8; 32]),
                    fee_recipient: AlloyAddress::from([6u8; 20]),
                    state_root: B256::from([3u8; 32]),
                    receipts_root: B256::from([4u8; 32]),
                    logs_bloom: alloy_primitives::Bloom::ZERO,
                    prev_randao: B256::from([5u8; 32]),
                    block_number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 15_000_000,
                    timestamp: 1_700_000_000,
                    extra_data: AlloyBytes::new(),
                    base_fee_per_gas: alloy_primitives::U256::from(1),
                    block_hash: B256::from([1u8; 32]),
                    transactions: Vec::new(),
                },
                withdrawals: Vec::<Withdrawal>::new(),
            },
        }
    }

    fn sample_value_metadata(count: usize) -> ValueMetadata {
        let header = sample_execution_payload_header();
        let commitments = (0..count).map(|i| KzgCommitment::new([i as u8; 48])).collect();
        ValueMetadata::new(header, commitments)
    }

    fn build_state(
        mock_engine: MockBlobEngine,
        start_height: Height,
    ) -> (State<MockBlobEngine>, TempDir) {
        let tmp = tempdir().expect("tempdir");
        let store = Store::open(tmp.path().join("store.db"), DbMetrics::new()).expect("store");

        let private_key = PrivateKey::from([42u8; 32]);
        let public_key = private_key.public_key();
        let validator = Validator::new(public_key, 1);
        let validator_set = ValidatorSet::new(vec![validator.clone()]);
        let genesis = Genesis { validator_set };
        let provider = Ed25519Provider::new(private_key);
        let address = validator.address.clone();

        let state = State::new(
            genesis,
            LoadContext::new(),
            provider,
            address,
            start_height,
            store,
            mock_engine.clone(),
        );

        (state, tmp)
    }

    #[tokio::test]
    async fn hydrate_blob_parent_root_uses_decided_metadata() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

        let metadata = sample_blob_metadata(Height::new(1), B256::from([9u8; 32]));
        state
            .store
            .put_blob_metadata_undecided(Height::new(1), Round::new(0), &metadata)
            .await
            .expect("insert metadata");
        state
            .store
            .mark_blob_metadata_decided(Height::new(1), Round::new(0))
            .await
            .expect("promote metadata");

        state.last_blob_sidecar_root = B256::ZERO;
        state.hydrate_blob_parent_root().await.expect("hydrate");

        let expected = metadata.to_beacon_header().hash_tree_root();
        assert_eq!(state.last_blob_sidecar_root, expected);
    }

    #[tokio::test]
    async fn hydrate_blob_parent_root_seeds_genesis_with_correct_root() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

        let expected = BlobMetadata::genesis().to_beacon_header().hash_tree_root();
        state.hydrate_blob_parent_root().await.expect("hydrate");

        assert_eq!(state.last_blob_sidecar_root, expected);
    }

    #[tokio::test]
    async fn verify_blob_sidecars_roundtrip_canonical_proof() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(0));
        state.store.seed_genesis_blob_metadata().await.expect("seed genesis metadata");
        state.hydrate_blob_parent_root().await.expect("hydrate parent root");

        let height = Height::new(1);
        let round = Round::new(0);

        let commitments = vec![KzgCommitment::new([1u8; 48]), KzgCommitment::new([2u8; 48])];
        let blobs = vec![
            Blob::new(BlobBytes::from(NetworkBytes::from(vec![0u8; BYTES_PER_BLOB])))
                .expect("blob0"),
            Blob::new(BlobBytes::from(NetworkBytes::from(vec![1u8; BYTES_PER_BLOB])))
                .expect("blob1"),
        ];
        let proofs = vec![KzgProof::new([3u8; 48]), KzgProof::new([4u8; 48])];

        let bundle = BlobsBundle::new(commitments.clone(), proofs, blobs);
        bundle.validate().expect("bundle valid");

        let metadata = ValueMetadata::new(sample_execution_payload_header(), commitments.clone());
        let value = Value::new(metadata.clone());
        let locally_proposed = LocallyProposedValue::new(height, round, value);

        let (_signed_header, sidecars) = state
            .prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))
            .expect("prepare sidecars");
        assert_eq!(sidecars.len(), commitments.len());

        state
            .verify_blob_sidecars(height, &state.address, &metadata, &sidecars)
            .await
            .expect("canonical proofs pass");

        let mut tampered = sidecars.clone();
        tampered[0].kzg_commitment_inclusion_proof[0] = B256::from([0xFFu8; 32]);

        let err = state
            .verify_blob_sidecars(height, &state.address, &metadata, &tampered)
            .await
            .expect_err("tampered proof rejected");
        assert!(
            err.contains("Invalid KZG inclusion proof"),
            "expected inclusion proof failure, got {err}"
        );
    }

    #[tokio::test]
    async fn cleanup_stale_blob_metadata_removes_lower_entries() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(3));
        state.current_height = Height::new(4);

        let metadata_low = sample_blob_metadata(Height::new(2), B256::from([10u8; 32]));
        state
            .store
            .put_blob_metadata_undecided(Height::new(2), Round::new(0), &metadata_low)
            .await
            .expect("insert");

        let metadata_current = sample_blob_metadata(Height::new(4), B256::from([11u8; 32]));
        state
            .store
            .put_blob_metadata_undecided(Height::new(4), Round::new(1), &metadata_current)
            .await
            .expect("insert");

        state.cleanup_stale_blob_metadata().await.expect("cleanup");

        assert!(
            state
                .store
                .get_blob_metadata_undecided(Height::new(2), Round::new(0))
                .await
                .expect("get")
                .is_none()
        );
        assert!(
            state
                .store
                .get_blob_metadata_undecided(Height::new(4), Round::new(1))
                .await
                .expect("get")
                .is_some()
        );
    }

    #[tokio::test]
    async fn load_blob_metadata_for_round_falls_back_to_decided() {
        let mock_engine = MockBlobEngine::default();
        let (state, _tmp) = build_state(mock_engine, Height::new(1));
        let height = Height::new(1);

        let metadata = sample_blob_metadata(height, B256::ZERO);
        state
            .store
            .put_blob_metadata_undecided(height, Round::new(0), &metadata)
            .await
            .expect("insert undecided metadata");
        state
            .store
            .mark_blob_metadata_decided(height, Round::new(0))
            .await
            .expect("promote metadata");

        assert!(
            state
                .store
                .get_blob_metadata_undecided(height, Round::new(0))
                .await
                .expect("get undecided")
                .is_none()
        );

        let loaded = state
            .load_blob_metadata_for_round(height, Round::new(1))
            .await
            .expect("load metadata")
            .expect("metadata fallback");

        assert_eq!(loaded.height(), metadata.height());
        assert_eq!(loaded.parent_blob_root(), metadata.parent_blob_root());
        assert_eq!(loaded.blob_kzg_commitments(), metadata.blob_kzg_commitments());
    }

    #[tokio::test]
    async fn propose_value_with_blobs_stores_blob_metadata() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(1));

        let payload = sample_execution_payload_v3();
        let expected_header = ExecutionPayloadHeader::from_payload(&payload);
        let bundle = sample_blob_bundle(1);
        let metadata_before = state
            .store
            .get_blob_metadata_undecided(Height::new(1), Round::new(0))
            .await
            .expect("get");
        assert!(metadata_before.is_none());
        assert_eq!(state.last_blob_sidecar_root, B256::ZERO);

        state
            .propose_value_with_blobs(
                Height::new(1),
                Round::new(0),
                NetworkBytes::new(),
                &payload,
                Some(&bundle),
            )
            .await
            .expect("propose");

        let stored = state
            .store
            .get_blob_metadata_undecided(Height::new(1), Round::new(0))
            .await
            .expect("get")
            .expect("metadata");

        assert_eq!(stored.height(), Height::new(1));
        assert_eq!(stored.blob_count(), 1);
        assert_eq!(stored.parent_blob_root(), B256::ZERO);
        assert_eq!(stored.blob_kzg_commitments(), bundle.commitments.as_slice());
        assert_eq!(stored.execution_payload_header(), &expected_header);
        assert_eq!(stored.proposer_index_hint(), Some(0));
        assert_eq!(state.last_blob_sidecar_root, B256::ZERO);
    }

    #[tokio::test]
    async fn propose_blobless_value_uses_parent_root_hint() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(2));
        let parent_root = B256::from([7u8; 32]);
        state.last_blob_sidecar_root = parent_root;

        let payload = sample_execution_payload_v3();
        let expected_header = ExecutionPayloadHeader::from_payload(&payload);

        state
            .propose_value_with_blobs(
                Height::new(2),
                Round::new(0),
                NetworkBytes::new(),
                &payload,
                None,
            )
            .await
            .expect("propose blobless");

        let stored = state
            .store
            .get_blob_metadata_undecided(Height::new(2), Round::new(0))
            .await
            .expect("get")
            .expect("metadata");

        assert_eq!(stored.height(), Height::new(2));
        assert_eq!(stored.blob_count(), 0);
        assert_eq!(stored.parent_blob_root(), parent_root);
        assert_eq!(stored.execution_payload_header(), &expected_header);
        assert_eq!(stored.proposer_index_hint(), Some(0));
        assert_eq!(state.last_blob_sidecar_root, parent_root);
    }

    #[tokio::test]
    async fn commit_promotes_metadata_and_updates_parent_root() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
        let height = Height::new(1);
        let round = Round::new(0);

        let metadata = sample_blob_metadata(height, B256::ZERO);
        state
            .store
            .put_blob_metadata_undecided(height, round, &metadata)
            .await
            .expect("insert metadata");

        let value_metadata = sample_value_metadata(metadata.blob_count() as usize);
        let value = Value::new(value_metadata.clone());
        let proposal = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: state.address.clone(),
            value: value.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
        state
            .store
            .store_undecided_block_data(height, round, NetworkBytes::from_static(b"block"))
            .await
            .expect("store block bytes");

        let certificate = CommitCertificate {
            height,
            round,
            value_id: proposal.value.id(),
            commit_signatures: Vec::new(),
        };

        state.commit(certificate.clone()).await.expect("commit");

        assert_eq!(state.last_blob_sidecar_root, metadata.to_beacon_header().hash_tree_root());
        assert!(
            state.store.get_blob_metadata_undecided(height, round).await.expect("get").is_none()
        );
        assert!(state.store.get_blob_metadata(height).await.expect("get").is_some());

        let proposer = state.address.clone();
        let consensus = state
            .store
            .get_consensus_block_metadata(height)
            .await
            .expect("load consensus")
            .expect("consensus metadata");
        assert_eq!(consensus.height(), height);
        assert_eq!(consensus.round(), round);
        assert_eq!(consensus.proposer(), &proposer);
        assert_eq!(
            consensus.execution_block_hash(),
            value_metadata.execution_payload_header.block_hash
        );
        assert_eq!(consensus.gas_limit(), value_metadata.execution_payload_header.gas_limit);
        assert_eq!(consensus.gas_used(), value_metadata.execution_payload_header.gas_used);
        let mut expected_validator_root = [0u8; 32];
        let proposer_bytes = proposer.into_inner();
        expected_validator_root[..proposer_bytes.len()].copy_from_slice(&proposer_bytes);
        assert_eq!(consensus.validator_set_hash(), B256::from(expected_validator_root));

        let calls = mock_engine.mark_decided_calls();
        assert_eq!(calls, vec![(height, round.as_i64())]);
        assert!(mock_engine.verify_calls().is_empty());
    }

    #[tokio::test]
    async fn commit_promotes_blobless_metadata_updates_parent_root() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(2));
        let height = Height::new(2);
        let round = Round::new(0);

        let previous_root = B256::from([3u8; 32]);
        state.last_blob_sidecar_root = previous_root;

        let header = sample_execution_payload_header();
        let metadata = BlobMetadata::blobless(height, previous_root, &header, Some(0));
        state
            .store
            .put_blob_metadata_undecided(height, round, &metadata)
            .await
            .expect("insert blobless metadata");

        let value_metadata = ValueMetadata::new(header.clone(), Vec::new());
        let value = Value::new(value_metadata.clone());
        let proposer = state.address.clone();
        let proposal = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: proposer.clone(),
            value: value.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
        state
            .store
            .store_undecided_block_data(height, round, NetworkBytes::from_static(b"block"))
            .await
            .expect("store block bytes");

        let certificate = CommitCertificate {
            height,
            round,
            value_id: proposal.value.id(),
            commit_signatures: Vec::new(),
        };

        state.commit(certificate).await.expect("commit blobless");

        let expected_root = metadata.to_beacon_header().hash_tree_root();
        assert_eq!(state.last_blob_sidecar_root, expected_root);
        assert!(
            state.store.get_blob_metadata_undecided(height, round).await.expect("get").is_none()
        );

        let decided =
            state.store.get_blob_metadata(height).await.expect("load decided").expect("metadata");
        assert_eq!(decided.blob_count(), 0);
        assert_eq!(decided.parent_blob_root(), previous_root);

        let consensus = state
            .store
            .get_consensus_block_metadata(height)
            .await
            .expect("load consensus")
            .expect("consensus metadata");
        assert_eq!(consensus.height(), height);
        assert_eq!(consensus.round(), round);
        assert_eq!(consensus.proposer(), &proposer);
        assert_eq!(
            consensus.execution_block_hash(),
            value_metadata.execution_payload_header.block_hash
        );
        assert_eq!(consensus.gas_limit(), value_metadata.execution_payload_header.gas_limit);
        assert_eq!(consensus.gas_used(), value_metadata.execution_payload_header.gas_used);
        let mut expected_validator_root = [0u8; 32];
        let proposer_bytes = proposer.into_inner();
        expected_validator_root[..proposer_bytes.len()].copy_from_slice(&proposer_bytes);
        assert_eq!(consensus.validator_set_hash(), B256::from(expected_validator_root));

        let calls = mock_engine.mark_decided_calls();
        assert_eq!(calls, vec![(height, round.as_i64())]);
        assert!(mock_engine.verify_calls().is_empty());
    }

    #[tokio::test]
    async fn rebuild_blob_sidecars_for_restream_reconstructs_headers() {
        let mock_engine = MockBlobEngine::default();
        let (state, _tmp) = build_state(mock_engine, Height::new(1));

        let height = Height::new(1);
        let round = Round::new(0);
        let payload = sample_execution_payload_v3();
        let header = ExecutionPayloadHeader::from_payload(&payload);
        let bundle = sample_blob_bundle(1);

        let value_metadata = ValueMetadata::new(header.clone(), bundle.commitments.clone());
        let value = Value::new(value_metadata.clone());
        let locally_proposed = LocallyProposedValue::new(height, round, value);

        let (_signed_header, sidecars) =
            state.prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle)).expect("prepare");

        let blob_metadata = BlobMetadata::new(
            height,
            B256::ZERO,
            bundle.commitments.clone(),
            header.clone(),
            Some(0),
        );

        let rebuilt = state
            .rebuild_blob_sidecars_for_restream(
                &blob_metadata,
                state.validator_address(),
                &sidecars,
            )
            .expect("rebuild");

        assert_eq!(rebuilt.len(), sidecars.len());

        for rebuilt_sidecar in &rebuilt {
            assert_eq!(
                rebuilt_sidecar.signed_block_header.message.parent_root,
                blob_metadata.parent_blob_root()
            );
            assert_eq!(
                rebuilt_sidecar.signed_block_header.message.slot,
                blob_metadata.height().as_u64()
            );
            assert_eq!(
                rebuilt_sidecar.signed_block_header.message.proposer_index,
                blob_metadata.proposer_index_hint().expect("proposer index hint")
            );
            assert!(
                !rebuilt_sidecar.kzg_commitment_inclusion_proof.is_empty(),
                "inclusion proof should be populated"
            );
        }
    }

    #[tokio::test]
    async fn commit_cleans_failed_round_blob_metadata() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

        let height = Height::new(1);
        let decided_round = Round::new(0);
        let losing_round = Round::new(1);

        let metadata_decided = sample_blob_metadata(height, B256::ZERO);
        let metadata_losing = sample_blob_metadata(height, B256::from([9u8; 32]));

        state
            .store
            .put_blob_metadata_undecided(height, decided_round, &metadata_decided)
            .await
            .expect("insert decided metadata");
        state
            .store
            .put_blob_metadata_undecided(height, losing_round, &metadata_losing)
            .await
            .expect("insert losing metadata");

        state
            .store
            .store_undecided_block_data(height, decided_round, NetworkBytes::from_static(b"block"))
            .await
            .expect("store block bytes");

        state
            .blob_rounds
            .entry(height)
            .or_insert_with(HashSet::new)
            .extend([decided_round.as_i64(), losing_round.as_i64()]);

        let value_metadata = sample_value_metadata(metadata_decided.blob_count() as usize);
        let value = Value::new(value_metadata.clone());
        let proposer = state.address.clone();

        let proposal = ProposedValue {
            height,
            round: decided_round,
            valid_round: Round::Nil,
            proposer: proposer.clone(),
            value: value.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");

        let certificate = CommitCertificate {
            height,
            round: decided_round,
            value_id: proposal.value.id(),
            commit_signatures: Vec::new(),
        };

        state.commit(certificate).await.expect("commit");

        assert!(
            state
                .store
                .get_blob_metadata_undecided(height, losing_round)
                .await
                .expect("load losing metadata")
                .is_none()
        );

        assert_eq!(mock_engine.drop_calls(), vec![(height, losing_round.as_i64())]);
    }

    #[tokio::test]
    async fn multi_round_proposal_isolation_and_commit() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
        let height = Height::new(1);

        // Propose at round 0
        state.current_height = height;
        state.current_round = Round::new(0);
        let payload_r0 = sample_execution_payload_v3();
        let bundle_r0 = sample_blob_bundle(2);
        state
            .propose_value_with_blobs(
                height,
                Round::new(0),
                NetworkBytes::new(),
                &payload_r0,
                Some(&bundle_r0),
            )
            .await
            .expect("propose round 0");

        // Propose at round 1 (timeout on round 0)
        state.current_round = Round::new(1);
        let payload_r1 = sample_execution_payload_v3();
        let bundle_r1 = sample_blob_bundle(3);
        state
            .propose_value_with_blobs(
                height,
                Round::new(1),
                NetworkBytes::new(),
                &payload_r1,
                Some(&bundle_r1),
            )
            .await
            .expect("propose round 1");

        // Both rounds should have undecided metadata
        let meta_r0 = state
            .store
            .get_blob_metadata_undecided(height, Round::new(0))
            .await
            .expect("get r0")
            .expect("metadata r0");
        let meta_r1 = state
            .store
            .get_blob_metadata_undecided(height, Round::new(1))
            .await
            .expect("get r1")
            .expect("metadata r1");

        assert_eq!(meta_r0.blob_count(), 2);
        assert_eq!(meta_r1.blob_count(), 3);

        // Commit round 1 (round 0 timed out)
        let value_metadata_r1 = sample_value_metadata(3);
        let value = Value::new(value_metadata_r1.clone());
        let proposal = ProposedValue {
            height,
            round: Round::new(1),
            valid_round: Round::Nil,
            proposer: state.address.clone(),
            value: value.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
        state
            .store
            .store_undecided_block_data(height, Round::new(1), NetworkBytes::from_static(b"block"))
            .await
            .expect("store block bytes");

        let certificate = CommitCertificate {
            height,
            round: Round::new(1),
            value_id: proposal.value.id(),
            commit_signatures: Vec::new(),
        };

        state.commit(certificate).await.expect("commit r1");

        // Round 1 should be promoted to decided
        let decided = state
            .store
            .get_blob_metadata(height)
            .await
            .expect("get decided")
            .expect("decided metadata");
        assert_eq!(decided.blob_count(), 3);

        // Round 1 undecided should be deleted
        assert!(
            state
                .store
                .get_blob_metadata_undecided(height, Round::new(1))
                .await
                .expect("get r1")
                .is_none()
        );

        // Round 0 should still exist (not cleaned by commit)
        assert!(
            state
                .store
                .get_blob_metadata_undecided(height, Round::new(0))
                .await
                .expect("get r0")
                .is_some(),
            "Round 0 metadata should survive commit of round 1"
        );

        // Cache should be updated from round 1 metadata
        assert_eq!(state.last_blob_sidecar_root, decided.to_beacon_header().hash_tree_root());
    }

    #[tokio::test]
    async fn propose_at_height_zero_uses_zero_parent_blobbed() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

        // Sanity check initial state
        assert_eq!(state.current_height, Height::new(0));
        assert_eq!(state.last_blob_sidecar_root, B256::ZERO);

        let payload = sample_execution_payload_v3();
        let bundle = sample_blob_bundle(1);

        state
            .propose_value_with_blobs(
                Height::new(0),
                Round::new(0),
                NetworkBytes::new(),
                &payload,
                Some(&bundle),
            )
            .await
            .expect("propose height 0");

        let stored = state
            .store
            .get_blob_metadata_undecided(Height::new(0), Round::new(0))
            .await
            .expect("get")
            .expect("metadata");

        assert_eq!(stored.height(), Height::new(0));
        assert_eq!(stored.parent_blob_root(), B256::ZERO, "Height 0 must use ZERO parent");
        assert_eq!(stored.blob_count(), 1);
        // Cache unchanged (only updated at commit)
        assert_eq!(state.last_blob_sidecar_root, B256::ZERO);
    }

    #[tokio::test]
    async fn propose_at_height_zero_uses_zero_parent_blobless() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine, Height::new(0));

        let payload = sample_execution_payload_v3();

        state
            .propose_value_with_blobs(
                Height::new(0),
                Round::new(0),
                NetworkBytes::new(),
                &payload,
                None, // blobless
            )
            .await
            .expect("propose blobless height 0");

        let stored = state
            .store
            .get_blob_metadata_undecided(Height::new(0), Round::new(0))
            .await
            .expect("get")
            .expect("metadata");

        assert_eq!(stored.height(), Height::new(0));
        assert_eq!(stored.parent_blob_root(), B256::ZERO, "Height 0 must use ZERO parent");
        assert_eq!(stored.blob_count(), 0);
        assert_eq!(state.last_blob_sidecar_root, B256::ZERO);
    }

    #[tokio::test]
    async fn commit_fails_fast_if_blob_metadata_missing() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));
        let height = Height::new(1);
        let round = Round::new(0);

        // Store proposal and block data but NOT BlobMetadata
        let value_metadata = sample_value_metadata(2);
        let value = Value::new(value_metadata.clone());
        let proposal = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: state.address.clone(),
            value: value.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal.clone()).await.expect("store proposal");
        state
            .store
            .store_undecided_block_data(height, round, NetworkBytes::from_static(b"block"))
            .await
            .expect("store block bytes");

        let certificate = CommitCertificate {
            height,
            round,
            value_id: proposal.value.id(),
            commit_signatures: Vec::new(),
        };

        // Commit should fail fast with MissingBlobMetadata error
        let result = state.commit(certificate).await;
        assert!(result.is_err(), "Commit should fail when BlobMetadata is missing");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("BlobMetadata") || err_msg.contains("not found"),
            "Error should mention BlobMetadata: {}",
            err_msg
        );

        // Cache should remain unchanged
        assert_eq!(state.last_blob_sidecar_root, B256::ZERO);

        // Blob engine should NOT have been called
        assert!(mock_engine.mark_decided_calls().is_empty());
    }

    #[tokio::test]
    async fn parent_root_chain_continuity_across_mixed_blocks() {
        let mock_engine = MockBlobEngine::default();
        let (mut state, _tmp) = build_state(mock_engine.clone(), Height::new(1));

        // Height 1: Blobbed block
        let payload_h1 = sample_execution_payload_v3();
        let bundle_h1 = sample_blob_bundle(2);
        state
            .propose_value_with_blobs(
                Height::new(1),
                Round::new(0),
                NetworkBytes::new(),
                &payload_h1,
                Some(&bundle_h1),
            )
            .await
            .expect("propose h1");

        let meta_h1 = state
            .store
            .get_blob_metadata_undecided(Height::new(1), Round::new(0))
            .await
            .expect("get h1")
            .expect("metadata h1");

        // Commit height 1
        let value_meta_h1 = sample_value_metadata(2);
        let value_h1 = Value::new(value_meta_h1.clone());
        let proposal_h1 = ProposedValue {
            height: Height::new(1),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: state.address.clone(),
            value: value_h1.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal_h1.clone()).await.expect("store p1");
        state
            .store
            .store_undecided_block_data(
                Height::new(1),
                Round::new(0),
                NetworkBytes::from_static(b"b1"),
            )
            .await
            .expect("store b1");

        let cert_h1 = CommitCertificate {
            height: Height::new(1),
            round: Round::new(0),
            value_id: proposal_h1.value.id(),
            commit_signatures: Vec::new(),
        };

        state.commit(cert_h1).await.expect("commit h1");

        let parent_after_h1 = state.last_blob_sidecar_root;
        assert_ne!(parent_after_h1, B256::ZERO, "Cache should be updated after commit");
        assert_eq!(parent_after_h1, meta_h1.to_beacon_header().hash_tree_root());

        // Height 2: Blobless block (should maintain chain)
        state.current_height = Height::new(2);
        let payload_h2 = sample_execution_payload_v3();
        state
            .propose_value_with_blobs(
                Height::new(2),
                Round::new(0),
                NetworkBytes::new(),
                &payload_h2,
                None, // blobless
            )
            .await
            .expect("propose h2 blobless");

        let meta_h2 = state
            .store
            .get_blob_metadata_undecided(Height::new(2), Round::new(0))
            .await
            .expect("get h2")
            .expect("metadata h2");

        assert_eq!(meta_h2.blob_count(), 0);
        assert_eq!(meta_h2.parent_blob_root(), parent_after_h1, "Blobless block must chain to h1");

        // Commit height 2
        let value_meta_h2 =
            ValueMetadata::new(ExecutionPayloadHeader::from_payload(&payload_h2), Vec::new());
        let value_h2 = Value::new(value_meta_h2.clone());
        let proposal_h2 = ProposedValue {
            height: Height::new(2),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: state.address.clone(),
            value: value_h2.clone(),
            validity: Validity::Valid,
        };

        state.store.store_undecided_proposal(proposal_h2.clone()).await.expect("store p2");
        state
            .store
            .store_undecided_block_data(
                Height::new(2),
                Round::new(0),
                NetworkBytes::from_static(b"b2"),
            )
            .await
            .expect("store b2");

        let cert_h2 = CommitCertificate {
            height: Height::new(2),
            round: Round::new(0),
            value_id: proposal_h2.value.id(),
            commit_signatures: Vec::new(),
        };

        state.commit(cert_h2).await.expect("commit h2 blobless");

        let parent_after_h2 = state.last_blob_sidecar_root;
        assert_ne!(parent_after_h2, parent_after_h1, "Cache should update even for blobless");
        assert_eq!(parent_after_h2, meta_h2.to_beacon_header().hash_tree_root());

        // Height 3: Another blobbed block (should chain to h2)
        state.current_height = Height::new(3);
        let payload_h3 = sample_execution_payload_v3();
        let bundle_h3 = sample_blob_bundle(1);
        state
            .propose_value_with_blobs(
                Height::new(3),
                Round::new(0),
                NetworkBytes::new(),
                &payload_h3,
                Some(&bundle_h3),
            )
            .await
            .expect("propose h3");

        let meta_h3 = state
            .store
            .get_blob_metadata_undecided(Height::new(3), Round::new(0))
            .await
            .expect("get h3")
            .expect("metadata h3");

        assert_eq!(meta_h3.blob_count(), 1);
        assert_eq!(
            meta_h3.parent_blob_root(),
            parent_after_h2,
            "Height 3 must chain to blobless h2"
        );

        // Verify full chain: h1 → h2 (blobless) → h3
        let decided_h1 =
            state.store.get_blob_metadata(Height::new(1)).await.expect("d1").expect("m1");
        let decided_h2 =
            state.store.get_blob_metadata(Height::new(2)).await.expect("d2").expect("m2");

        assert_eq!(decided_h1.parent_blob_root(), B256::ZERO);
        assert_eq!(decided_h2.parent_blob_root(), decided_h1.to_beacon_header().hash_tree_root());
        assert_eq!(meta_h3.parent_blob_root(), decided_h2.to_beacon_header().hash_tree_root());
    }
}

/// Decodes a Value from its byte representation using ProtobufCodec
pub fn decode_value(bytes: Bytes) -> Value {
    ProtobufCodec.decode(bytes).expect("panic during protobuf velue decode")
}
