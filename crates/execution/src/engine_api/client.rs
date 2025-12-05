#![allow(missing_docs)]
use std::{fmt, sync::Arc};

use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4, ExecutionPayloadV3, ForkchoiceState,
    ForkchoiceUpdated, PayloadAttributes, PayloadId, PayloadStatus,
};
use async_trait::async_trait;
use color_eyre::eyre;
use serde::{Serialize, de::DeserializeOwned};
use ultramarine_types::{
    aliases::{B256, BlockHash, Bytes},
    // Phase 1b: Import BlobsBundle for get_payload_with_blobs()
    blob::BlobsBundle,
    engine_api::JsonExecutionPayloadV3,
};

use super::{EngineApi, ExecutionPayloadResult};
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

impl fmt::Debug for EngineApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineApiClient").field("transport", &"<dyn Transport>").finish()
    }
}

#[async_trait]
impl EngineApi for EngineApiClient {
    /// Notify that a fork choice has been updated, to set the head of the chain
    /// - head_block_hash: The block hash of the head of the chain
    /// - safe_block_hash: The block hash of the most recent "safe" block (can be same as head)
    /// - finalized_block_hash: The block hash of the highest finalized block (can be 0x0 for
    ///   genesis)
    async fn forkchoice_updated(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        self.request(ENGINE_FORKCHOICE_UPDATED_V3, (state, payload_attributes)).await
    }

    async fn get_payload(&self, payload_id: PayloadId) -> eyre::Result<ExecutionPayloadResult> {
        let ExecutionPayloadEnvelopeV4 { envelope_inner, execution_requests } =
            self.request(ENGINE_GET_PAYLOAD_V4, (payload_id,)).await?;
        let execution_requests: Vec<Bytes> = execution_requests.into_iter().collect();
        Ok(ExecutionPayloadResult { payload: envelope_inner.execution_payload, execution_requests })
    }

    async fn get_payload_with_blobs(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<(ExecutionPayloadResult, Option<BlobsBundle>)> {
        let ExecutionPayloadEnvelopeV4 { envelope_inner, execution_requests } =
            self.request(ENGINE_GET_PAYLOAD_V4, (payload_id,)).await?;

        let execution_requests: Vec<Bytes> = execution_requests.into_iter().collect();
        let ExecutionPayloadEnvelopeV3 {
            execution_payload,
            block_value: _,
            blobs_bundle,
            should_override_builder: _,
        } = envelope_inner;

        let payload = execution_payload;

        let blob_bundle = if blobs_bundle.blobs.is_empty() {
            None
        } else {
            let bundle = BlobsBundle::try_from(blobs_bundle).map_err(|e| {
                eyre::eyre!("Failed to convert blob bundle from Engine API response: {}", e)
            })?;
            bundle
                .validate()
                .map_err(|e| eyre::eyre!("Invalid blob bundle from execution layer: {}", e))?;
            Some(bundle)
        };

        Ok((ExecutionPayloadResult { payload, execution_requests }, blob_bundle))
    }

    async fn new_payload(
        &self,
        execution_payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_block_hash: BlockHash,
        execution_requests: Vec<Bytes>,
    ) -> eyre::Result<PayloadStatus> {
        let payload = JsonExecutionPayloadV3::from(execution_payload);
        let params =
            serde_json::json!([payload, versioned_hashes, parent_block_hash, execution_requests]);
        self.request(ENGINE_NEW_PAYLOAD_V4, params).await
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
