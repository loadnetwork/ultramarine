pub mod capabilities;
pub mod client;
pub mod jwt;

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
    PayloadStatus,
};
use async_trait::async_trait;
use color_eyre::eyre;
use ultramarine_types::{
    aliases::Bytes,
    aliases::{B256, BlockHash},
    // Phase 1b: Import BlobsBundle for get_payload_with_blobs()
    blob::BlobsBundle,
};

use crate::engine_api::capabilities::EngineCapabilities;

/// Execution payload plus Prague execution requests.
#[derive(Clone, Debug)]
pub struct ExecutionPayloadResult {
    pub payload: ExecutionPayloadV3,
    pub execution_requests: Vec<Bytes>,
}

/// The Engine API trait, defining the interface between Consensus and Execution layers.
#[async_trait]
pub trait EngineApi: Send + Sync {
    async fn forkchoice_updated(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated>;

    async fn get_payload(&self, payload_id: PayloadId) -> eyre::Result<ExecutionPayloadResult>;

    /// Get execution payload with blob bundle (Phase 1b - EIP-4844 integration)
    ///
    /// This method retrieves both the execution payload and the associated blob bundle
    /// from the `getPayloadV3` Engine API call. The blob bundle contains:
    /// - KZG commitments (48 bytes each)
    /// - KZG proofs (48 bytes each)
    /// - Blob data (131,072 bytes each)
    ///
    /// ## Returns
    ///
    /// - `Ok((payload, Some(bundle)))` if blobs are included
    /// - `Ok((payload, None))` if no blobs (empty bundle or absent)
    /// - `Err(...)` on RPC error or parsing failure
    ///
    /// ## Usage
    ///
    /// This is used by `ExecutionClient::generate_block_with_blobs()` when the
    /// consensus layer needs to propose a block that may contain blob transactions.
    async fn get_payload_with_blobs(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<(ExecutionPayloadResult, Option<BlobsBundle>)>;

    async fn new_payload(
        &self,
        execution_payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_block_hash: BlockHash,
        execution_requests: Vec<Bytes>,
    ) -> eyre::Result<PayloadStatus>;

    async fn exchange_capabilities(&self) -> eyre::Result<EngineCapabilities>;
}
