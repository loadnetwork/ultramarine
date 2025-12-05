use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadStatus};
use async_trait::async_trait;
use ultramarine_types::aliases::{BlockHash, Bytes};

/// Notifies the execution layer about newly finalized payloads and forkchoice updates.
#[async_trait]
pub trait ExecutionNotifier: Send {
    async fn notify_new_block(
        &mut self,
        payload: ExecutionPayloadV3,
        execution_requests: Vec<Bytes>,
        versioned_hashes: Vec<BlockHash>,
    ) -> color_eyre::Result<PayloadStatus>;

    async fn set_latest_forkchoice_state(
        &mut self,
        block_hash: BlockHash,
    ) -> color_eyre::Result<BlockHash>;
}
