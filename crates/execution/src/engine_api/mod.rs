pub mod capabilities;
pub mod client;
mod json_structures;
pub mod jwt;

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
    PayloadStatus,
};
use async_trait::async_trait;
use color_eyre::eyre;
use ultramarine_types::aliases::{B256, BlockHash};

use crate::engine_api::capabilities::EngineCapabilities;

/// The Engine API trait, defining the interface between Consensus and Execution layers.
#[async_trait]
pub trait EngineApi: Send + Sync {
    async fn forkchoice_updated(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated>;

    async fn get_payload(&self, payload_id: PayloadId) -> eyre::Result<ExecutionPayloadV3>;

    async fn new_payload(
        &self,
        execution_payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_block_hash: BlockHash,
    ) -> eyre::Result<PayloadStatus>;

    async fn exchange_capabilities(&self) -> eyre::Result<EngineCapabilities>;
}
