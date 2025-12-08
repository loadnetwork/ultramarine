//! Shared helpers for in-process integration tests.
//!
//! These helpers intentionally mirror the lightweight harness style used by
//! malachite and snapchain: every test owns its temporary environment, spins
//! real Ultramarine components inside the current Tokio runtime, and only
//! mocks the execution client surface.

pub(crate) mod mocks;

use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
};

use alloy_consensus::{Signed, TxEip4844, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{
    Address as AlloyAddress, B256, Bloom, Bytes as AlloyBytes, FixedBytes, Signature, U256,
};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadId,
};
use bytes::Bytes;
use c_kzg::{Blob as CKzgBlob, KzgSettings};
use color_eyre::Result;
use malachitebft_app_channel::app::types::{LocallyProposedValue, PeerId, core::Round};
use serde::Deserialize;
use ssz::Encode;
use tempfile::TempDir;
use ultramarine_blob_engine::{
    BlobEngineImpl, BlobEngineMetrics, store::rocksdb::RocksDbBlobStore,
};
use ultramarine_consensus::{metrics::DbMetrics, state::State, store::Store};
pub(crate) use ultramarine_test_support::execution_requests::sample_execution_requests_for_height;
use ultramarine_types::{
    address::Address,
    blob::{BYTES_PER_BLOB, Blob, BlobsBundle, KzgCommitment, KzgProof},
    constants::LOAD_EXECUTION_GAS_LIMIT,
    context::LoadContext,
    engine_api::load_prev_randao,
    genesis::Genesis,
    height::Height,
    proposal_part::BlobSidecar,
    signing::{Ed25519Provider, PrivateKey},
    validator_set::{Validator, ValidatorSet},
};

/// Type alias for the production blob engine used in tests.
pub(crate) type TestBlobEngine = BlobEngineImpl<RocksDbBlobStore>;

/// Type alias for the state tied to the production blob engine.
pub(crate) type TestState = State<TestBlobEngine>;

/// Wrapper that keeps temporary directories alive for the duration of a test.
///
/// Each integration test allocates its own storage root so node restarts can be
/// simulated by reusing the same path while dropping and recreating runtime
/// components.  The directory is removed automatically when the guard is
/// dropped at the end of the test.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TestDirs {
    /// Base temporary directory.
    root: TempDir,
    /// Path to the RocksDB state store.
    pub(crate) store_path: PathBuf,
    /// Path to the blob store database.
    pub(crate) blob_store_path: PathBuf,
}

#[allow(dead_code)]
impl TestDirs {
    /// Create a new set of directories under a unique temporary root.
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().expect("create temp dir");
        let store_path = root.path().join("store.db");
        let blob_store_path = root.path().join("blob_store.db");
        Self { root, store_path, blob_store_path }
    }

    /// Expose the root path for convenience.
    pub(crate) fn root(&self) -> &Path {
        self.root.path()
    }
}

/// Deterministic validator keypair used by the harness.
#[derive(Clone, Debug)]
pub(crate) struct ValidatorKey {
    pub validator: Validator,
    key_bytes: [u8; 32],
}

impl ValidatorKey {
    fn new(seed: u8, voting_power: u64) -> Self {
        let key_bytes = [seed; 32];
        let private_key = PrivateKey::from(key_bytes);
        let validator = Validator::new(private_key.public_key(), voting_power);
        Self { validator, key_bytes }
    }

    /// Regenerate the private key from the stored seed.
    pub(crate) fn private_key(&self) -> PrivateKey {
        PrivateKey::from(self.key_bytes)
    }

    /// Shortcut accessor for the validator address.
    pub(crate) fn address(&self) -> Address {
        self.validator.address.clone()
    }
}

/// Build a genesis configuration with a deterministic validator set.
pub(crate) fn make_genesis(validator_count: usize) -> (Genesis, Vec<ValidatorKey>) {
    assert!(validator_count > 0, "at least one validator required");

    let validators: Vec<ValidatorKey> =
        (0..validator_count).map(|i| ValidatorKey::new(i as u8 + 1, 1)).collect();

    let validator_set = ValidatorSet::new(validators.iter().map(|vk| vk.validator.clone()));
    (Genesis { validator_set }, validators)
}

/// Convenience bundle containing state and metrics handles for a node.
#[allow(dead_code)]
pub(crate) struct StateHarness {
    pub state: TestState,
    pub blob_metrics: BlobEngineMetrics,
}

/// Open the consensus + blob stores for a validator and construct a [`State`].
#[allow(dead_code)]
pub(crate) fn build_state(
    dirs: &TestDirs,
    genesis: &Genesis,
    validator: &ValidatorKey,
    start_height: Height,
) -> Result<StateHarness> {
    // Ensure the RocksDB directories exist before opening the stores.
    std::fs::create_dir_all(dirs.root())?;

    let store = Store::open(&dirs.store_path, DbMetrics::new())?;
    let blob_store = RocksDbBlobStore::open(&dirs.blob_store_path)?;
    let blob_metrics = BlobEngineMetrics::new();
    let blob_engine = BlobEngineImpl::new(blob_store, blob_metrics.clone())?;

    let provider = Ed25519Provider::new(validator.private_key());
    let state = State::new(
        genesis.clone(),
        LoadContext::new(),
        provider,
        validator.address(),
        start_height,
        store,
        blob_engine,
        blob_metrics.clone(),
    );

    Ok(StateHarness { state, blob_metrics })
}

/// Open the stores and seed the genesis metadata so the node can process blobs immediately.
///
/// Most integration tests need the state to be hydrated; this helper wraps [`build_state`]
/// and performs the asynchronous initialization steps.
#[allow(dead_code)]
pub(crate) async fn build_seeded_state(
    dirs: &TestDirs,
    genesis: &Genesis,
    validator: &ValidatorKey,
    start_height: Height,
) -> Result<StateHarness> {
    let mut harness = build_state(dirs, genesis, validator, start_height)?;
    harness.state.seed_genesis_blob_metadata().await?;
    harness.state.hydrate_blob_parent_root().await?;
    Ok(harness)
}

/// Propose a value with optional blobs and capture the resulting sidecars.
pub(crate) async fn propose_with_optional_blobs(
    state: &mut TestState,
    height: Height,
    round: Round,
    payload: &ExecutionPayloadV3,
    bundle: Option<&BlobsBundle>,
) -> Result<(LocallyProposedValue<LoadContext>, Bytes, Option<Vec<BlobSidecar>>)> {
    state.current_height = height;
    state.current_round = round;

    let bytes = Bytes::from(payload.as_ssz_bytes());
    let proposed =
        state.propose_value_with_blobs(height, round, bytes.clone(), payload, &[], bundle).await?;

    let sidecars = if let Some(bundle) = bundle {
        let (_header, sidecars) = state.prepare_blob_sidecar_parts(&proposed, Some(bundle))?;
        Some(sidecars)
    } else {
        None
    };

    Ok((proposed, bytes, sidecars))
}

/// Deterministic execution payload used by tests. Parent/child hashes are derived from the
/// requested height so repeated invocations form a consistent chain for availability checks.
pub(crate) fn sample_execution_payload_v3_for_height(
    height: Height,
    bundle: Option<&BlobsBundle>,
) -> ExecutionPayloadV3 {
    let parent_byte =
        if height.as_u64() == 0 { 0u8 } else { height.as_u64().saturating_sub(1) as u8 };
    let block_byte = height.as_u64() as u8;

    let mut payload = ExecutionPayloadV3 {
        blob_gas_used: 0,
        excess_blob_gas: 0,
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: B256::from([parent_byte; 32]),
                fee_recipient: AlloyAddress::from([6u8; 20]),
                state_root: B256::from([3u8; 32]),
                receipts_root: B256::from([4u8; 32]),
                logs_bloom: Bloom::ZERO,
                prev_randao: load_prev_randao(),
                block_number: height.as_u64(), // Fix: block_number should equal height
                gas_limit: LOAD_EXECUTION_GAS_LIMIT,
                gas_used: LOAD_EXECUTION_GAS_LIMIT / 2,
                timestamp: 1_700_000_000 + height.as_u64(),
                extra_data: AlloyBytes::new(),
                base_fee_per_gas: U256::from(1),
                block_hash: B256::from([block_byte; 32]),
                transactions: Vec::new(),
            },
            withdrawals: Vec::new(),
        },
    };

    if let Some(bundle) = bundle {
        let versioned_hashes: Vec<B256> =
            bundle.versioned_hashes().into_iter().map(B256::from).collect();
        if !versioned_hashes.is_empty() {
            let tx_bytes = dummy_blob_tx_bytes(&versioned_hashes);
            payload.payload_inner.payload_inner.transactions = vec![tx_bytes];
            payload.blob_gas_used = versioned_hashes.len() as u64 * 131_072;
        }
    }

    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_payload_uses_constant_prev_randao() {
        let height_one = Height::new(1);
        let height_two = Height::new(2);

        let payload_one = sample_execution_payload_v3_for_height(height_one, None);
        let payload_two = sample_execution_payload_v3_for_height(height_two, None);

        let expected = load_prev_randao();
        assert_eq!(payload_one.payload_inner.payload_inner.prev_randao, expected);
        assert_eq!(payload_two.payload_inner.payload_inner.prev_randao, expected);
    }
}

/// Helper to construct deterministic [`PayloadId`] values.
pub(crate) fn payload_id(byte: u8) -> PayloadId {
    PayloadId::from(FixedBytes::<8>::from([byte; 8]))
}

/// Construct a deterministic blob bundle for tests.
pub(crate) fn sample_blob_bundle(count: usize) -> BlobsBundle {
    fn load_kzg_settings() -> Arc<KzgSettings> {
        static SETTINGS: OnceLock<Arc<KzgSettings>> = OnceLock::new();
        SETTINGS
            .get_or_init(|| Arc::new(load_trusted_setup().expect("load trusted setup for tests")))
            .clone()
    }

    let kzg = load_kzg_settings();

    let mut commitments = Vec::with_capacity(count);
    let mut proofs = Vec::with_capacity(count);
    let mut blobs = Vec::with_capacity(count);

    for i in 0..count {
        let data = vec![i as u8; BYTES_PER_BLOB];
        let ckzg_blob = CKzgBlob::from_bytes(&data).expect("valid blob data");
        let commitment = kzg.blob_to_kzg_commitment(&ckzg_blob).expect("compute commitment");
        let commitment_bytes = commitment.to_bytes();
        let proof =
            kzg.compute_blob_kzg_proof(&ckzg_blob, &commitment_bytes).expect("compute proof");

        commitments.push(KzgCommitment::new(commitment_bytes.into_inner()));
        proofs.push(KzgProof::new(proof.to_bytes().into_inner()));
        blobs.push(Blob::new(AlloyBytes::from(data)).expect("blob"));
    }

    BlobsBundle::new(commitments, proofs, blobs)
}

/// Deterministic peer identifier for harness usage.
pub(crate) fn test_peer_id(index: u8) -> PeerId {
    const PEER_IDS: &[&str] = &[
        "12D3KooWHRyfTBKcjkqjNk5UZarJhzT7rXZYfr4DmaCWJgen62Xk",
        "12D3KooWNr294AH1fviDQxRmQ4K79iFSGoRCWzGspVxPprJUKN47",
        "12D3KooWCc28TYrrXFivwUshyZ8R5HqPMgx4f7AP54iCDLYr7kFR",
    ];
    let idx = (index as usize) % PEER_IDS.len();
    PeerId::from_str(PEER_IDS[idx]).expect("valid peer id")
}

fn load_trusted_setup() -> Result<KzgSettings> {
    const NO_PRECOMPUTE: u64 = 0;

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let setup_path = manifest_dir.join("../blob_engine/src/trusted_setup.json");
    let bytes = std::fs::read(setup_path)?;
    let trusted_setup: TrustedSetup = serde_json::from_slice(&bytes)?;

    KzgSettings::load_trusted_setup(
        &trusted_setup.g1_monomial_bytes(),
        &trusted_setup.g1_lagrange_bytes(),
        &trusted_setup.g2_monomial_bytes(),
        NO_PRECOMPUTE,
    )
    .map_err(|e| color_eyre::eyre::eyre!("failed to load KZG settings: {:?}", e))
}

/// Create a deterministic EIP-4844 transaction encoded as 2718 bytes for embedding in payloads.
fn dummy_blob_tx_bytes(versioned_hashes: &[B256]) -> AlloyBytes {
    let tx = TxEip4844 {
        chain_id: 1,
        nonce: 0,
        gas_limit: 30_000,
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: AlloyAddress::from([9u8; 20]),
        value: U256::from(1u64),
        access_list: Default::default(),
        blob_versioned_hashes: versioned_hashes.to_vec(),
        max_fee_per_blob_gas: 20_000_000_000,
        input: AlloyBytes::new(),
    };

    let signature = Signature::test_signature();
    let signed = Signed::new_unhashed(tx, signature);
    let envelope: TxEnvelope = TxEnvelope::from(signed);
    AlloyBytes::from(envelope.encoded_2718())
}

#[derive(Debug, Clone, Deserialize)]
struct TrustedSetup {
    g1_monomial: Vec<G1Point>,
    g1_lagrange: Vec<G1Point>,
    g2_monomial: Vec<G2Point>,
}

impl TrustedSetup {
    fn g1_monomial_bytes(&self) -> Vec<u8> {
        self.g1_monomial.iter().flat_map(|p| p.0).collect()
    }

    fn g1_lagrange_bytes(&self) -> Vec<u8> {
        self.g1_lagrange.iter().flat_map(|p| p.0).collect()
    }

    fn g2_monomial_bytes(&self) -> Vec<u8> {
        self.g2_monomial.iter().flat_map(|p| p.0).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
struct G1Point(#[serde(with = "hex_serde")] [u8; 48]);

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
struct G2Point(#[serde(with = "hex_serde")] [u8; 96]);

mod hex_serde {
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != N {
            return Err(serde::de::Error::custom(format!(
                "Expected {} bytes, got {}",
                N,
                bytes.len()
            )));
        }
        let mut array = [0u8; N];
        array.copy_from_slice(&bytes);
        Ok(array)
    }
}

// ============================================================================
// Common Test Patterns (Phase 3 Refactoring)
// ============================================================================

/// Test constants for deterministic test identifiers.
///
/// Using named constants instead of magic numbers improves readability and
/// prevents ID collisions between tests.
#[allow(dead_code)]
pub(crate) mod test_constants {
    /// Payload IDs - use distinct ranges for different test categories
    pub(crate) const PAYLOAD_ID_ROUNDTRIP: u8 = 1;
    pub(crate) const PAYLOAD_ID_SYNC: u8 = 10;
    pub(crate) const PAYLOAD_ID_PRUNING: u8 = 20;

    /// Peer IDs for multi-validator scenarios
    pub(crate) const PEER_ID_PROPOSER: u8 = 0;
    pub(crate) const PEER_ID_FOLLOWER_1: u8 = 1;
    pub(crate) const PEER_ID_FOLLOWER_2: u8 = 2;

    /// Timing constants for async operation polling
    pub(crate) const BLOB_VERIFICATION_TIMEOUT_MS: u64 = 5000;
    pub(crate) const ASYNC_OP_POLL_INTERVAL_MS: u64 = 50;
}
