use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address as AlloyAddress, B256, Bytes as AlloyBytes};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadStatus, PayloadStatusEnum,
};
use alloy_rpc_types_eth::Withdrawal;
use async_trait::async_trait;
use malachitebft_app_channel::app::types::LocallyProposedValue;
use ssz::Encode;
use tempfile::{TempDir, tempdir};
use ultramarine_blob_engine::error::BlobEngineError;
use ultramarine_execution::{error::ExecutionError, notifier::ExecutionNotifier};
use ultramarine_types::{
    address::Address,
    blob::{BYTES_PER_BLOB, Blob, BlobsBundle, KzgCommitment, KzgProof},
    blob_metadata::BlobMetadata,
    constants::LOAD_EXECUTION_GAS_LIMIT,
    engine_api::{ExecutionPayloadHeader, load_prev_randao},
    genesis::Genesis,
    height::Height,
    signing::{Ed25519Provider, PrivateKey},
    validator_set::{Validator, ValidatorSet},
    value_metadata::ValueMetadata,
};

use super::*;
use crate::{
    metrics::DbMetrics,
    state::{BlobSidecar, State},
    store::Store,
};

pub fn sample_blob_metadata(height: Height, parent_blob_root: B256) -> BlobMetadata {
    let commitments = vec![KzgCommitment::new([42u8; 48])];
    let blob_hashes = commitments.iter().map(|_| B256::ZERO).collect();
    BlobMetadata::new(
        height,
        parent_blob_root,
        commitments,
        blob_hashes,
        sample_execution_payload_header(),
        Some(0),
    )
}

pub fn sample_blob_bundle(count: usize) -> BlobsBundle {
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

pub fn sample_execution_payload_header() -> ExecutionPayloadHeader {
    ExecutionPayloadHeader {
        block_hash: B256::from([1u8; 32]),
        parent_hash: B256::from([2u8; 32]),
        state_root: B256::from([3u8; 32]),
        receipts_root: B256::from([4u8; 32]),
        logs_bloom: alloy_primitives::Bloom::ZERO,
        block_number: 1,
        gas_limit: LOAD_EXECUTION_GAS_LIMIT,
        gas_used: LOAD_EXECUTION_GAS_LIMIT / 2,
        timestamp: 1_700_000_000,
        base_fee_per_gas: alloy_primitives::U256::from(1),
        blob_gas_used: 0,
        excess_blob_gas: 0,
        prev_randao: load_prev_randao(),
        fee_recipient: Address::new([6u8; 20]),
        extra_data: AlloyBytes::new(),
        transactions_root: B256::from([7u8; 32]),
        withdrawals_root: B256::from([8u8; 32]),
        requests_hash: None,
    }
}

pub fn sample_execution_payload_v3() -> ExecutionPayloadV3 {
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
                prev_randao: load_prev_randao(),
                block_number: 1,
                gas_limit: LOAD_EXECUTION_GAS_LIMIT,
                gas_used: LOAD_EXECUTION_GAS_LIMIT / 2,
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

pub fn sample_value_metadata(count: usize) -> ValueMetadata {
    let header = sample_execution_payload_header();
    let commitments = (0..count).map(|i| KzgCommitment::new([i as u8; 48])).collect();
    ValueMetadata::new(header, commitments)
}

pub async fn propose_blobbed_value(
    state: &mut State<MockBlobEngine>,
    height: Height,
    round: Round,
    blob_count: usize,
) -> (LocallyProposedValue<LoadContext>, BlobMetadata, Vec<BlobSidecar>, BlobsBundle, NetworkBytes)
{
    let payload = sample_execution_payload_v3();
    let bundle = sample_blob_bundle(blob_count);
    let payload_bytes = NetworkBytes::from(payload.as_ssz_bytes());

    let proposed = state
        .propose_value_with_blobs(
            height,
            round,
            payload_bytes.clone(),
            &payload,
            &[],
            Some(&bundle),
        )
        .await
        .expect("propose value with blobs");

    let metadata = state
        .store
        .get_blob_metadata_undecided(height, round)
        .await
        .expect("load undecided metadata")
        .expect("metadata stored");

    let (_signed_header, sidecars) =
        state.prepare_blob_sidecar_parts(&proposed, Some(&bundle)).await.expect("sidecars");

    (proposed, metadata, sidecars, bundle, payload_bytes)
}

pub async fn seed_decided_blob_metadata(
    state: &mut State<MockBlobEngine>,
    height: Height,
    parent_root: B256,
) -> eyre::Result<B256> {
    let header = sample_execution_payload_header();
    let metadata = BlobMetadata::blobless(height, parent_root, &header, Some(0));
    state.store.put_blob_metadata_undecided(height, Round::new(0), &metadata).await?;
    state.store.mark_blob_metadata_decided(height, Round::new(0)).await?;
    Ok(metadata.to_beacon_header().hash_tree_root())
}

pub fn build_state(
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
    let address = validator.address;

    let blob_metrics = ultramarine_blob_engine::BlobEngineMetrics::new();
    let archive_metrics = crate::archive_metrics::ArchiveMetrics::new();
    let state = State::new(
        genesis,
        LoadContext::new(),
        provider,
        address,
        start_height,
        store,
        Arc::new(mock_engine.clone()),
        blob_metrics,
        archive_metrics,
    );

    (state, tmp)
}

#[derive(Default, Clone)]
pub struct MockBlobEngine {
    inner: Arc<Mutex<MockBlobEngineState>>,
}

#[derive(Default)]
struct MockBlobEngineState {
    verify_calls: Vec<(Height, i64, usize)>,
    mark_decided_calls: Vec<(Height, i64)>,
    drop_calls: Vec<(Height, i64)>,
    undecided_blobs: HashMap<(Height, i64), Vec<BlobSidecar>>,
    decided_blobs: HashMap<Height, Vec<BlobSidecar>>,
}

impl MockBlobEngine {
    pub fn verify_calls(&self) -> Vec<(Height, i64, usize)> {
        self.inner.lock().unwrap().verify_calls.clone()
    }

    pub fn mark_decided_calls(&self) -> Vec<(Height, i64)> {
        self.inner.lock().unwrap().mark_decided_calls.clone()
    }

    pub fn drop_calls(&self) -> Vec<(Height, i64)> {
        self.inner.lock().unwrap().drop_calls.clone()
    }
}

#[async_trait]
impl ultramarine_blob_engine::BlobEngine for MockBlobEngine {
    async fn verify_and_store(
        &self,
        height: Height,
        round: i64,
        sidecars: &[BlobSidecar],
    ) -> Result<(), BlobEngineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.verify_calls.push((height, round, sidecars.len()));
        inner.undecided_blobs.insert((height, round), sidecars.to_vec());
        Ok(())
    }

    async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.mark_decided_calls.push((height, round));

        if let Some(sidecars) = inner.undecided_blobs.remove(&(height, round)) {
            inner.decided_blobs.insert(height, sidecars);
        }
        Ok(())
    }

    async fn get_for_import(&self, height: Height) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.decided_blobs.get(&height).cloned().unwrap_or_default())
    }

    async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
        let mut inner = self.inner.lock().unwrap();
        inner.drop_calls.push((height, round));
        inner.undecided_blobs.remove(&(height, round));
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
        height: Height,
        round: i64,
    ) -> Result<Vec<BlobSidecar>, BlobEngineError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.undecided_blobs.get(&(height, round)).cloned().unwrap_or_default())
    }
}

#[derive(Clone)]
pub struct MockExecutionNotifier {
    inner: Arc<Mutex<MockExecutionNotifierState>>,
}

impl Default for MockExecutionNotifier {
    fn default() -> Self {
        Self { inner: Arc::new(Mutex::new(MockExecutionNotifierState::default())) }
    }
}

struct MockExecutionNotifierState {
    new_block_calls: Vec<(ExecutionPayloadV3, Vec<AlloyBytes>, Vec<BlockHash>)>,
    forkchoice_calls: Vec<BlockHash>,
    payload_status: PayloadStatus,
    payload_statuses: VecDeque<PayloadStatus>,
    forkchoice_errors: VecDeque<ExecutionError>,
}

impl Default for MockExecutionNotifierState {
    fn default() -> Self {
        Self {
            new_block_calls: Vec::new(),
            forkchoice_calls: Vec::new(),
            payload_status: PayloadStatus::from_status(PayloadStatusEnum::Valid),
            payload_statuses: VecDeque::new(),
            forkchoice_errors: VecDeque::new(),
        }
    }
}

impl MockExecutionNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_payload_statuses(statuses: Vec<PayloadStatus>) -> Self {
        let inner = MockExecutionNotifierState {
            payload_statuses: statuses.into(),
            ..MockExecutionNotifierState::default()
        };
        Self { inner: Arc::new(Mutex::new(inner)) }
    }

    #[allow(dead_code)]
    pub fn with_forkchoice_errors(errors: Vec<ExecutionError>) -> Self {
        let inner = MockExecutionNotifierState {
            forkchoice_errors: errors.into(),
            ..MockExecutionNotifierState::default()
        };
        Self { inner: Arc::new(Mutex::new(inner)) }
    }
}

#[async_trait]
impl ExecutionNotifier for MockExecutionNotifier {
    async fn notify_new_block(
        &mut self,
        payload: ExecutionPayloadV3,
        execution_requests: Vec<AlloyBytes>,
        versioned_hashes: Vec<BlockHash>,
    ) -> color_eyre::Result<PayloadStatus> {
        let mut inner = self.inner.lock().unwrap();
        inner.new_block_calls.push((payload, execution_requests, versioned_hashes));
        if let Some(status) = inner.payload_statuses.pop_front() {
            return Ok(status);
        }
        Ok(inner.payload_status.clone())
    }

    async fn set_latest_forkchoice_state(
        &mut self,
        block_hash: BlockHash,
    ) -> color_eyre::Result<BlockHash> {
        let mut inner = self.inner.lock().unwrap();
        inner.forkchoice_calls.push(block_hash);
        if let Some(error) = inner.forkchoice_errors.pop_front() {
            return Err(color_eyre::Report::new(error));
        }
        Ok(block_hash)
    }
}
