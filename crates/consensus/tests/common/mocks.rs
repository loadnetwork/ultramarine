//! Test doubles for external dependencies used by integration tests.
//!
//! These mocks allow the harness to exercise consensus logic without
//! maintaining live execution-layer connections.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadId, PayloadStatus, PayloadStatusEnum};
use async_trait::async_trait;
use color_eyre::Result;
use ultramarine_execution::{
    engine_api::{EngineApi, capabilities::EngineCapabilities},
    notifier::ExecutionNotifier,
};
use ultramarine_types::{aliases::BlockHash, blob::BlobsBundle};

/// Lightweight Engine API mock that records invocations and returns canned responses.
///
/// The integration tests currently focus on consensus + blob flows, so the mock keeps the
/// interface minimal. Methods can be extended as coverage grows.
#[derive(Default, Clone)]
pub(crate) struct MockEngineApi {
    pub forkchoice_updates: Arc<Mutex<Vec<BlockHash>>>,
    payloads: Arc<Mutex<HashMap<PayloadId, (ExecutionPayloadV3, Option<BlobsBundle>)>>>,
}

impl MockEngineApi {
    /// Register a payload/bundle pair to be returned by `get_payload_with_blobs`.
    pub(crate) fn with_payload(
        self,
        payload_id: PayloadId,
        payload: ExecutionPayloadV3,
        bundle: Option<BlobsBundle>,
    ) -> Self {
        self.payloads.lock().unwrap().insert(payload_id, (payload, bundle));
        self
    }
}

#[async_trait]
impl EngineApi for MockEngineApi {
    async fn exchange_capabilities(&self) -> Result<EngineCapabilities> {
        Ok(EngineCapabilities {
            new_payload_v1: false,
            new_payload_v2: false,
            new_payload_v3: true,
            new_payload_v4: false,
            forkchoice_updated_v1: false,
            forkchoice_updated_v2: false,
            forkchoice_updated_v3: true,
            get_payload_bodies_by_hash_v1: false,
            get_payload_bodies_by_range_v1: false,
            get_payload_v1: false,
            get_payload_v2: false,
            get_payload_v3: true,
            get_payload_v4: false,
            get_client_version_v1: false,
            get_blobs_v1: false,
        })
    }

    async fn forkchoice_updated(
        &self,
        state: alloy_rpc_types_engine::ForkchoiceState,
        _payload_attributes: Option<alloy_rpc_types_engine::PayloadAttributes>,
    ) -> Result<alloy_rpc_types_engine::ForkchoiceUpdated> {
        self.forkchoice_updates.lock().unwrap().push(state.head_block_hash);
        Ok(alloy_rpc_types_engine::ForkchoiceUpdated {
            payload_status: PayloadStatus::new(
                PayloadStatusEnum::Valid,
                Some(state.head_block_hash),
            ),
            payload_id: None,
        })
    }

    async fn get_payload(&self, payload_id: PayloadId) -> Result<ExecutionPayloadV3> {
        let guard = self.payloads.lock().unwrap();
        guard
            .get(&payload_id)
            .cloned()
            .map(|(payload, _)| payload)
            .ok_or_else(|| color_eyre::eyre::eyre!("mock payload not registered"))
    }

    async fn get_payload_with_blobs(
        &self,
        payload_id: PayloadId,
    ) -> Result<(ExecutionPayloadV3, Option<BlobsBundle>)> {
        let guard = self.payloads.lock().unwrap();
        guard
            .get(&payload_id)
            .cloned()
            .ok_or_else(|| color_eyre::eyre::eyre!("mock payload not registered"))
    }

    async fn new_payload(
        &self,
        _execution_payload: ExecutionPayloadV3,
        _versioned_hashes: Vec<B256>,
        _parent_block_hash: BlockHash,
    ) -> Result<PayloadStatus> {
        Ok(PayloadStatus::from_status(PayloadStatusEnum::Valid))
    }
}

/// Mock `ExecutionNotifier` used by integration tests.
///
/// Records payload submissions and forkchoice updates while allowing the caller to
/// configure the payload status or force failures.
#[derive(Clone)]
pub(crate) struct MockExecutionNotifier {
    pub new_block_calls: Arc<Mutex<Vec<(ExecutionPayloadV3, Vec<BlockHash>)>>>,
    pub forkchoice_calls: Arc<Mutex<Vec<BlockHash>>>,
    payload_status: Arc<Mutex<PayloadStatus>>,
}

impl Default for MockExecutionNotifier {
    fn default() -> Self {
        Self {
            new_block_calls: Arc::new(Mutex::new(Vec::new())),
            forkchoice_calls: Arc::new(Mutex::new(Vec::new())),
            payload_status: Arc::new(Mutex::new(PayloadStatus::from_status(
                PayloadStatusEnum::Valid,
            ))),
        }
    }
}

impl MockExecutionNotifier {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Configure the payload status returned by `notify_new_block`.
    pub(crate) fn with_payload_status(self, status: PayloadStatus) -> Self {
        *self.payload_status.lock().unwrap() = status;
        self
    }
}

#[async_trait]
impl ExecutionNotifier for MockExecutionNotifier {
    async fn notify_new_block(
        &mut self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<BlockHash>,
    ) -> color_eyre::Result<PayloadStatus> {
        self.new_block_calls.lock().unwrap().push((payload, versioned_hashes));

        Ok(self.payload_status.lock().unwrap().clone())
    }

    async fn set_latest_forkchoice_state(
        &mut self,
        block_hash: BlockHash,
    ) -> color_eyre::Result<BlockHash> {
        self.forkchoice_calls.lock().unwrap().push(block_hash);

        Ok(block_hash)
    }
}
