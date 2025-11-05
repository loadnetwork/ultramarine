//! Shared helpers for in-process integration tests.
//!
//! These helpers intentionally mirror the lightweight harness style used by
//! malachite and snapchain: every test owns its temporary environment, spins
//! real Ultramarine components inside the current Tokio runtime, and only
//! mocks the execution client surface.

pub mod mocks;

use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, OnceLock},
};

use alloy_primitives::{
    Address as AlloyAddress, B256, Bloom, Bytes as AlloyBytes, FixedBytes, U256,
};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadId,
};
use c_kzg::{Blob as CKzgBlob, KzgSettings};
use color_eyre::Result;
use malachitebft_app_channel::app::types::PeerId;
use serde::Deserialize;
use tempfile::TempDir;
use ultramarine_blob_engine::{
    BlobEngineImpl, BlobEngineMetrics, store::rocksdb::RocksDbBlobStore,
};
use ultramarine_consensus::{metrics::DbMetrics, state::State, store::Store};
use ultramarine_types::{
    address::Address,
    blob::{BYTES_PER_BLOB, Blob, BlobsBundle, KzgCommitment, KzgProof},
    context::LoadContext,
    genesis::Genesis,
    height::Height,
    signing::{Ed25519Provider, PrivateKey},
    validator_set::{Validator, ValidatorSet},
};

/// Type alias for the production blob engine used in tests.
pub type TestBlobEngine = BlobEngineImpl<RocksDbBlobStore>;

/// Type alias for the state tied to the production blob engine.
pub type TestState = State<TestBlobEngine>;

/// Wrapper that keeps temporary directories alive for the duration of a test.
///
/// Each integration test allocates its own storage root so node restarts can be
/// simulated by reusing the same path while dropping and recreating runtime
/// components.  The directory is removed automatically when the guard is
/// dropped at the end of the test.
#[derive(Debug)]
pub(crate) struct TestDirs {
    /// Base temporary directory.
    root: TempDir,
    /// Path to the RocksDB state store.
    pub(crate) store_path: PathBuf,
    /// Path to the blob store database.
    pub(crate) blob_store_path: PathBuf,
}

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
pub(crate) struct StateHarness {
    pub state: TestState,
    pub blob_metrics: BlobEngineMetrics,
}

/// Open the consensus + blob stores for a validator and construct a [`State`].
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

/// Deterministic execution payload used by tests. Parent/child hashes are derived from the
/// requested height so repeated invocations form a consistent chain for availability checks.
pub(crate) fn sample_execution_payload_v3_for_height(height: Height) -> ExecutionPayloadV3 {
    let parent_byte = if height.as_u64() == 0 { 0u8 } else { height.as_u64() as u8 };
    let block_byte = parent_byte.wrapping_add(1);

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
                prev_randao: B256::from([5u8; 32]),
                block_number: height.as_u64() + 1,
                gas_limit: 30_000_000,
                gas_used: 15_000_000,
                timestamp: 1_700_000_000 + height.as_u64(),
                extra_data: AlloyBytes::new(),
                base_fee_per_gas: U256::from(1),
                block_hash: B256::from([block_byte; 32]),
                transactions: Vec::new(),
            },
            withdrawals: Vec::new(),
        },
    };

    payload
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

    pub fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
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
