use std::{sync::Arc, time::Duration};

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
    PayloadStatus,
};
use async_trait::async_trait;
use color_eyre::eyre;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use ultramarine_types::aliases::{B256, BlockHash};

use super::EngineApi;
use crate::{
    engine_api::{EngineCapabilities, capabilities::*},
    transport::{JsonRpcRequest, Transport},
};

pub struct EngineApiClient {
    transport: Arc<dyn Transport>,
}
impl EngineApiClient {
    pub fn new(transport: impl Transport + 'static) -> Self {
        Self { transport: Arc::new(transport) }
    }

    async fn request<P, R>(&self, method: &str, params: P) -> eyre::Result<R>
    where
        P: Serialize + Send,
        R: DeserializeOwned,
    {
        let req = JsonRpcRequest::new(method, serde_json::to_value(params)?);

        let resp = self.transport.send(&req).await?;

        if let Some(err) = resp.error {
            return Err(eyre::eyre!("JSON-RPC error (code {}): {}", err.code, err.message));
        }

        let res =
            resp.result.ok_or_else(|| eyre::eyre!("Missing result field in JSON-RPC response"))?;

        Ok(serde_json::from_value(res)?)
    }
}

#[async_trait]
impl EngineApi for EngineApiClient {
    async fn forkchoice_updated(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        unimplemented!()
    }

    async fn get_payload(&self, payload_id: PayloadId) -> eyre::Result<ExecutionPayloadV3> {
        unimplemented!()
    }

    async fn new_payload(
        &self,
        execution_payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_block_hash: BlockHash,
    ) -> eyre::Result<PayloadStatus> {
        unimplemented!()
    }

    async fn exchange_capabilities(&self) -> eyre::Result<EngineCapabilities> {
        let resp_string: Vec<String> = self
            .request(ENGINE_EXCHANGE_CAPABILITIES, (ULTRAMARINE_CAPABILITIES.to_vec(),))
            .await?;

        let el_capabilities =
            EngineCapabilities::from_response_strings(resp_string.into_iter().collect());

        Ok(el_capabilities)
    }
}
