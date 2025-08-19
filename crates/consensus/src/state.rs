//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like
//! RPC.

use std::collections::HashSet;

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
// TODO: use malachitebft_eth_engine::json_structures::ExecutionBlock;
use rand::{Rng, SeedableRng, rngs::StdRng};
use sha3::Digest;
use tokio::time::Instant;
use tracing::{debug, error, info};
use ultramarine_types::{
    address::Address,
    codec::proto::ProtobufCodec,
    context::LoadContext,
    genesis::Genesis,
    height::Height,
    proposal_part::{ProposalData, ProposalFin, ProposalInit, ProposalPart},
    signing::Ed25519Provider,
    validator_set::ValidatorSet,
    value::Value,
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
pub struct State {
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

    pub current_height: Height,
    pub current_round: Round,
    pub current_proposer: Option<Address>,
    pub peers: HashSet<PeerId>,

    // TODO: pub latest_block: Option<ExecutionBlock>,

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

impl State {
    /// Creates a new State instance with the given validator address and starting height
    pub fn new(
        genesis: Genesis,
        ctx: LoadContext,
        signing_provider: Ed25519Provider,
        address: Address,
        height: Height,
        store: Store,
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
            peers: HashSet::new(),

            // TODO: latest_block: None,
            txs_count: 0,
            chain_bytes: 0,
            start_time: Instant::now(),
        }
    }

    /// Returns the earliest height available in the state
    pub async fn get_earliest_height(&self) -> Height {
        self.store.min_decided_value_height().await.unwrap_or_default()
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

        // Re-assemble the proposal from its parts
        let (value, data) = assemble_value_from_parts(parts);

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

    /// Retrieves a decided block at the given height
    pub async fn get_decided_value(&self, height: Height) -> Option<DecidedValue> {
        self.store.get_decided_value(height).await.ok().flatten()
    }

    /// Retrieves a decided block data at the given height
    pub async fn get_block_data(&self, height: Height, round: Round) -> Option<Bytes> {
        self.store.get_block_data(height, round).await.ok().flatten()
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

        // Store block data for decided value
        let block_data = self.store.get_block_data(certificate.height, certificate.round).await?;

        // Log first 32 bytes of block data with JNT prefix
        if let Some(data) = &block_data {
            if data.len() >= 32 {
                info!("Committed block_data[0..32]: {}", hex::encode(&data[..32]));
            }
        }

        if let Some(data) = block_data {
            self.store.store_decided_block_data(certificate.height, data).await?;
        }

        // Prune the store, keep the last 5 heights
        let retain_height = Height::new(certificate.height.as_u64().saturating_sub(5));
        self.store.prune(retain_height).await?;

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
        let value = Value::new(data);

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
    pub fn stream_proposal(
        &mut self,
        value: LocallyProposedValue<LoadContext>,
        data: Bytes,
    ) -> impl Iterator<Item = StreamMessage<ProposalPart>> {
        let parts = self.make_proposal_parts(value, data);

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

    fn make_proposal_parts(
        &self,
        value: LocallyProposedValue<LoadContext>,
        data: Bytes,
    ) -> Vec<ProposalPart> {
        let mut hasher = sha3::Keccak256::new();
        let mut parts = Vec::new();

        // Init
        {
            parts.push(ProposalPart::Init(ProposalInit::new(
                value.height,
                value.round,
                self.address,
            )));

            hasher.update(value.height.as_u64().to_be_bytes().as_slice());
            hasher.update(value.round.as_i64().to_be_bytes().as_slice());
        }

        // Data
        {
            for chunk in data.chunks(CHUNK_SIZE) {
                let chunk_data = ProposalData::new(Bytes::copy_from_slice(chunk));
                parts.push(ProposalPart::Data(chunk_data));
                hasher.update(chunk);
            }
        }

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

/// Re-assemble a [`ProposedValue`] from its [`ProposalParts`].
///
/// This is done by multiplying all the factors in the parts.
fn assemble_value_from_parts(parts: ProposalParts) -> (ProposedValue<LoadContext>, Bytes) {
    // Calculate total size and allocate buffer
    let total_size: usize =
        parts.parts.iter().filter_map(|part| part.as_data()).map(|data| data.bytes.len()).sum();

    let mut data = Vec::with_capacity(total_size);
    // Concatenate all chunks
    for part in parts.parts.iter().filter_map(|part| part.as_data()) {
        data.extend_from_slice(&part.bytes);
    }

    // Convert the concatenated data vector into Bytes
    let data = Bytes::from(data);

    let proposed_value = ProposedValue {
        height: parts.height,
        round: parts.round,
        valid_round: Round::Nil,
        proposer: parts.proposer,
        value: Value::new(data.clone()),
        validity: Validity::Valid,
    };

    (proposed_value, data)
}

/// Decodes a Value from its byte representation using ProtobufCodec
pub fn decode_value(bytes: Bytes) -> Value {
    ProtobufCodec.decode(bytes).unwrap()
}
