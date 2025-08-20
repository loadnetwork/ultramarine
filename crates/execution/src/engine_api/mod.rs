// crates/execution/src/engine_api/mod.rs

pub mod capabilities;
pub mod client;
pub mod jwt;

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
};
use async_trait::async_trait;
use color_eyre::eyre;

/// The Engine API trait, defining the interface between Consensus and Execution layers.
#[async_trait]
pub trait EngineApi: Send + Sync {
    async fn forkchoice_updated(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated>;

    async fn get_payload(&self, payload_id: PayloadId) -> eyre::Result<ExecutionPayloadV3>;
}
