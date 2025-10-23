//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like
//! RPC.

use std::collections::{HashMap, HashSet};

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
use tracing::{debug, error, info};
use ultramarine_blob_engine::{BlobEngine, BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
use ultramarine_types::{
    address::Address,
    // Phase 3: Import blob types for streaming
    blob::BlobsBundle,
    codec::proto::ProtobufCodec,
    context::LoadContext,
    engine_api::{ExecutionBlock, ExecutionPayloadHeader},
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
        self.store
            .get_undecided_proposal(height, round)
            .await
            .map_err(|e| eyre::Report::new(e))
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

        self.store.store_decided_value(&certificate, proposal.value).await?;

        // Mark blobs as decided in blob engine
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
        let metadata = ValueMetadata::new(header, commitments);

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
        blobs_bundle: Option<BlobsBundle>,
        proposer: Option<Address>, // For RestreamProposal support
    ) -> impl Iterator<Item = StreamMessage<ProposalPart>> {
        // Use provided proposer (for restreaming) or default to self.address (for our own
        // proposals)
        let proposer_address = proposer.unwrap_or(self.address);
        let parts = self.make_proposal_parts(value, data, blobs_bundle, proposer_address);

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
        blobs_bundle: Option<BlobsBundle>,
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
        if let Some(bundle) = blobs_bundle {
            for (index, ((blob, commitment), proof)) in
                bundle.blobs.iter().zip(&bundle.commitments).zip(&bundle.proofs).enumerate()
            {
                let sidecar =
                    BlobSidecar::from_bundle_item(index as u8, blob.clone(), *commitment, *proof);
                parts.push(ProposalPart::BlobSidecar(sidecar));

                // Include blob data in signature hash
                hasher.update(&[index as u8]);
                hasher.update(blob.data());
                hasher.update(commitment.as_bytes());
                hasher.update(proof.as_bytes());
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

        // Verify and store blobs
        if !blob_sidecars.is_empty() {
            debug!(
                "Extracted {} blob sidecars from proposal at height {}, round {}",
                blob_sidecars.len(),
                parts.height,
                parts.round
            );

            // Convert round to i64
            let round_i64 = parts.round.as_i64();

            // Verify and store blobs atomically
            self.blob_engine
                .verify_and_store(parts.height, round_i64, &blob_sidecars)
                .await
                .map_err(|e| format!("Blob engine error: {}", e))?;

            info!(
                "✅ Verified and stored {} blobs for height {}, round {}",
                blob_sidecars.len(),
                parts.height,
                parts.round
            );
        }

        // Parse execution payload and create Value
        let value = if !data.is_empty() && !blob_sidecars.is_empty() {
            use alloy_rpc_types_engine::ExecutionPayloadV3;
            use ssz::Decode;

            match ExecutionPayloadV3::from_ssz_bytes(&data) {
                Ok(execution_payload) => {
                    let header = ExecutionPayloadHeader::from_payload(&execution_payload);
                    let commitments: Vec<_> =
                        blob_sidecars.iter().map(|sidecar| sidecar.kzg_commitment).collect();

                    let metadata = ValueMetadata::new(header, commitments);
                    Value::new(metadata)
                }
                Err(e) => {
                    debug!("Failed to parse ExecutionPayloadV3 from SSZ: {:?}", e);
                    #[allow(deprecated)]
                    Value::from_bytes(data.clone())
                }
            }
        } else if !data.is_empty() {
            use alloy_rpc_types_engine::ExecutionPayloadV3;
            use ssz::Decode;

            match ExecutionPayloadV3::from_ssz_bytes(&data) {
                Ok(execution_payload) => {
                    let header = ExecutionPayloadHeader::from_payload(&execution_payload);
                    let metadata = ValueMetadata::new(header, vec![]);
                    Value::new(metadata)
                }
                Err(_) =>
                {
                    #[allow(deprecated)]
                    Value::from_bytes(data.clone())
                }
            }
        } else {
            #[allow(deprecated)]
            Value::from_bytes(data.clone())
        };

        let proposed_value = ProposedValue {
            height: parts.height,
            round: parts.round,
            valid_round: Round::Nil,
            proposer: parts.proposer,
            value,
            validity: Validity::Valid,
        };

        let has_blobs = !blob_sidecars.is_empty();
        Ok((proposed_value, data, has_blobs))
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
                    hasher.update(&[sidecar.index]);
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
