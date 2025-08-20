// crates/execution/src/engine_api/client.rs

// This file will contain the implementation of the Engine API client.

// --- REFERENCE IMPLEMENTATION ---
// use super::EngineApi;
// use crate::transport::Transport;
// use alloy_rpc_types_engine::{
// ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
// };
// use async_trait::async_trait;
// use color_eyre::eyre;
// use std::sync::Arc;
//
// pub struct EngineApiClient {
// transport: Arc<dyn Transport>,
// }
//
// impl EngineApiClient {
// pub fn new(transport: impl Transport + 'static) -> Self {
// Self {
// transport: Arc::new(transport),
// }
// }
// }
//
// #[async_trait]
// impl EngineApi for EngineApiClient {
// async fn forkchoice_updated(
// &self,
// _state: ForkchoiceState,
// _payload_attributes: Option<PayloadAttributes>,
// ) -> eyre::Result<ForkchoiceUpdated> {
// unimplemented!();
// }
//
// async fn get_payload(&self, _payload_id: PayloadId) -> eyre::Result<ExecutionPayloadV3> {
// unimplemented!();
// }
// }
