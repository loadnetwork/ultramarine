#![allow(missing_docs)]
use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadStatus,
    PayloadStatusEnum,
};
use async_trait::async_trait;
use color_eyre::eyre;
use tracing::{debug, info};
use ultramarine_types::{
    address::Address,
    aliases::{B256, BlockHash, Bytes},
    blob::BlobsBundle,
    engine_api::load_prev_randao,
};

use crate::{
    config::{EngineApiEndpoint, ExecutionConfig},
    engine_api::{EngineApi, ExecutionPayloadResult, client::EngineApiClient},
    eth_rpc::{EthRpc, alloy_impl::AlloyEthRpc},
    transport::{http::HttpTransport, ipc::IpcTransport},
};

// TODO: USE GENERICS instead of dyn

/// The main client for interacting with an execution layer node.
///
/// This client encapsulates both the Engine API client (for consensus-critical
/// operations) and a standard Eth1 RPC client (for all other interactions).
/// It is created from a single, flexible `ExecutionConfig`.
#[derive(Clone)]
pub struct ExecutionClient {
    /// The Engine API client, used for block production and fork choice.
    pub engine: Arc<dyn EngineApi>,
    /// The standard Eth1 JSON-RPC client, used for things like fetching logs.
    pub eth: Arc<dyn EthRpc>,
}

impl fmt::Debug for ExecutionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionClient")
            .field("engine", &"<dyn EngineApi>")
            .field("eth", &"<dyn EthRpc>")
            .finish()
    }
}

impl ExecutionClient {
    pub fn as_notifier(&self) -> ExecutionClientNotifier<'_> {
        ExecutionClientNotifier { client: self }
    }
    /// Creates a new `ExecutionClient` from the given configuration.
    ///
    /// This function is async because it needs to establish a connection
    /// to the execution node to initialize the EthRpc1 client.
    pub async fn new(config: ExecutionConfig) -> eyre::Result<Self> {
        info!("Creating new ExecutionClient");
        // 1. Create the Engine API client using its specific endpoint from the config.
        let engine_client: Arc<dyn EngineApi> = match config.engine_api_endpoint {
            EngineApiEndpoint::Http(url) => {
                info!("Using HTTP transport for Engine API");
                let transport = HttpTransport::new(url).with_jwt(config.jwt_secret);
                Arc::new(EngineApiClient::new(transport))
            }
            EngineApiEndpoint::Ipc(path) => {
                info!("Using IPC transport for Engine API");
                let transport = IpcTransport::new(path);
                Arc::new(EngineApiClient::new(transport))
            }
        };

        // 2. Create the standard Eth1 RPC client using its dedicated HTTP URL from the config.
        info!("Creating Eth1 RPC client");
        let eth_client: Arc<dyn EthRpc> = {
            let rpc_client = AlloyEthRpc::new(config.eth1_rpc_url);
            Arc::new(rpc_client)
        };
        info!("ExecutionClient created");

        Ok(Self { engine: engine_client, eth: eth_client })
    }

    pub fn engine(&self) -> &dyn EngineApi {
        self.engine.as_ref()
    }

    pub fn eth(&self) -> &dyn EthRpc {
        self.eth.as_ref()
    }

    pub async fn check_capabilities(&self) -> eyre::Result<()> {
        match self.engine.exchange_capabilities().await {
            Ok(cap) => {
                if !cap.forkchoice_updated_v3 || !cap.get_payload_v4 || !cap.new_payload_v4 {
                    tracing::error!(
                        ?cap,
                        "Execution client missing required Engine API capabilities"
                    );
                    return Err(eyre::eyre!("Execution client lacks required Engine API methods"));
                }
                tracing::info!("Execution client capabilities verified: OK");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to exchange Engine API capabilities: {}", e);
                Err(e)
            }
        }
    }

    pub async fn set_latest_forkchoice_state(
        &self,
        head_block_hash: BlockHash,
    ) -> eyre::Result<BlockHash> {
        debug!("🟠 set_latest_forkchoice_state: {:?}", head_block_hash);

        let forkchoice_state = ForkchoiceState {
            head_block_hash,
            finalized_block_hash: head_block_hash,
            safe_block_hash: head_block_hash,
        };

        let ForkchoiceUpdated { payload_status, payload_id } =
            self.engine.forkchoice_updated(forkchoice_state, None).await?;
        if payload_id.is_some() {
            return Err(eyre::eyre!(
                "engine_forkchoiceUpdatedV3 returned unexpected payloadId in a state update (no attributes)"
            ));
        }

        debug!("➡️ payload_status (state update): {:?}", payload_status);

        match payload_status.status {
            PayloadStatusEnum::Valid => {
                if payload_status.latest_valid_hash != Some(head_block_hash) {
                    tracing::warn!(
                        latest_valid_hash = ?payload_status.latest_valid_hash,
                        head_block_hash = ?head_block_hash,
                        "VALID status but latest_valid_hash does not match head"
                    );
                }
                payload_status.latest_valid_hash.ok_or_else(|| {
                    eyre::eyre!("Engine API spec violation: VALID status without latestValidHash")
                })
            }
            PayloadStatusEnum::Syncing if payload_status.latest_valid_hash.is_none() => {
                // From the Engine API spec:
                // 8. Client software MUST respond to this method call in the following way:
                //   * {payloadStatus: {status: SYNCING, latestValidHash: null,
                //   * validationError: null}, payloadId: null} if forkchoiceState.headBlockHash
                //     references an unknown payload or a payload that can't be validated because
                //     requisite data for the validation is missing
                tracing::warn!(
                    head_block_hash = ?head_block_hash,
                    "forkchoiceUpdated returned SYNCING with latest_valid_hash = None; EL not ready"
                );
                Err(eyre::eyre!(
                    "headBlockHash={:?} references an unknown payload or a payload that can't be validated",
                    head_block_hash
                ))
            }
            status => {
                tracing::error!(
                    ?status,
                    "forkchoiceUpdated state update returned non-VALID status"
                );
                Err(eyre::eyre!("Invalid payload status: {}", status))
            }
        }
    }

    pub async fn generate_block(
        &self,
        latest_block: &ultramarine_types::engine_api::ExecutionBlock,
    ) -> eyre::Result<ExecutionPayloadResult> {
        debug!("🟠 generate_block on top of {:?}", latest_block);
        let block_hash = latest_block.block_hash;
        let payload_attributes = PayloadAttributes {
            // Unix timestamp for when the payload is expected to be executed.
            // It should be greater than that of forkchoiceState.headBlockHash.
            timestamp: latest_block.timestamp + 1,

            // Load fixes PREVRANDAO to the canonical constant (Arbitrum-style) so no
            // application assumes entropy from it; the consensus doc captures this contract.
            prev_randao: load_prev_randao(),

            // CRITICAL TODO: This is a placeholder address. In a production environment,
            // this MUST be replaced with a user-configurable address to ensure
            // the validator operator receives their earned transaction fees (tips).
            suggested_fee_recipient: Address::repeat_byte(42).to_alloy_address(),

            // Cannot be None in V3.
            withdrawals: Some(vec![]),

            // Cannot be None in V3.
            parent_beacon_block_root: Some(block_hash),
        };

        let forkchoice_state = ForkchoiceState {
            head_block_hash: block_hash,
            finalized_block_hash: block_hash,
            safe_block_hash: block_hash,
        };

        let ForkchoiceUpdated { payload_status, payload_id } =
            self.engine.forkchoice_updated(forkchoice_state, Some(payload_attributes)).await?;

        tracing::debug!(
            status = ?payload_status.status,
            latest_valid_hash = ?payload_status.latest_valid_hash,
            has_payload_id = %payload_id.is_some(),
            head = ?block_hash,
            "forkchoiceUpdated (with attributes) response"
        );

        if payload_status.latest_valid_hash != Some(block_hash) {
            tracing::error!(
                latest_valid_hash = ?payload_status.latest_valid_hash,
                head = ?block_hash,
                "engine_forkchoiceUpdatedV3 returned mismatched latest_valid_hash"
            );
            return Err(eyre::eyre!(
                "engine_forkchoiceUpdatedV3 returned latestValidHash={:?} not matching head={:?}",
                payload_status.latest_valid_hash,
                block_hash
            ));
        }

        match payload_status.status {
            PayloadStatusEnum::Valid => {
                let Some(payload_id) = payload_id else {
                    tracing::error!("VALID status but payload_id is None after attributes");
                    return Err(eyre::eyre!(
                        "Engine API spec violation: VALID status with payload attributes must include payloadId"
                    ));
                };
                // See how payload is constructed: https://github.com/ethereum/consensus-specs/blob/v1.1.5/specs/merge/validator.md#block-proposal
                let payload_result = self.engine.get_payload(payload_id).await?;
                let payload_inner = &payload_result.payload.payload_inner.payload_inner;
                tracing::info!(
                    block_hash = ?payload_inner.block_hash,
                    parent_hash = ?payload_inner.parent_hash,
                    block_number = payload_inner.block_number,
                    txs = payload_inner.transactions.len(),
                    execution_requests = payload_result.execution_requests.len(),
                    "Received execution payload from EL"
                );
                Ok(payload_result)
            }
            // TODO: A production-ready client must handle all possible statuses gracefully.
            // This is critical for node stability and to prevent incorrect block proposals.
            //
            // In a Tendermint-based system (like Malachite), the Host application
            // would need to handle these statuses from the execution client:
            // - `SYNCING`: The EL is catching up. The Host should pause or skip block proposals for
            //   this round and wait for the EL to become synced. This prevents proposing on a
            //   non-canonical chain.
            // - `ACCEPTED`: The payload is valid but the EL is not yet treating it as the canonical
            //   head (e.g., due to a re-org). The Host should likely wait and not send a PRECOMMIT
            //   for the current proposal until the status becomes VALID in a subsequent
            //   forkchoiceUpdated call.
            // - `INVALID` / `INVALID_BLOCK_HASH`: This indicates a critical desynchronization or a
            //   bug. The Host must treat this as a fatal error for the round, halt consensus to
            //   avoid propagating a bad block, and alert an operator immediately.
            //
            // Additionally, the CRITICAL TODO for `suggested_fee_recipient` in this function
            // must be addressed before any real-world use to ensure transaction fees are
            // collected.
            status => {
                tracing::error!(
                    ?status,
                    "forkchoiceUpdated (with attributes) returned non-VALID status"
                );
                Err(eyre::eyre!("Invalid payload status: {}", status))
            }
        }
    }

    /// Generate a new execution block with blob bundle (Phase 1b - EIP-4844 integration)
    ///
    /// This method is similar to `generate_block()` but also retrieves and parses
    /// the blob bundle from the Engine API v3 response. The blob bundle contains:
    /// - KZG commitments (48 bytes each)
    /// - KZG proofs (48 bytes each)
    /// - Blob data (131,072 bytes each)
    ///
    /// ## Returns
    ///
    /// - `Ok((payload, Some(bundle)))` if the EL included blobs in the block
    /// - `Ok((payload, None))` if the EL produced a block without blobs
    /// - `Err(...)` on any execution layer error or invalid response
    ///
    /// ## Usage in Consensus (Phase 2)
    ///
    /// When proposing a block, the consensus layer will:
    /// 1. Call this method to get the payload + blob bundle
    /// 2. Extract lightweight `ExecutionPayloadHeader` from the payload
    /// 3. Create `ValueMetadata` with header + commitments (for voting)
    /// 4. Stream full payload via `ProposalPart::Data`
    /// 5. Stream blobs via `ProposalPart::BlobSidecar` (Phase 3)
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let (payload, maybe_bundle) = client.generate_block_with_blobs(&latest_block).await?;
    ///
    /// // Extract header for consensus voting
    /// let header = ExecutionPayloadHeader::from_payload(&payload);
    ///
    /// // Extract commitments for ValueMetadata
    /// let commitments = maybe_bundle.map(|b| b.commitments).unwrap_or_default();
    ///
    /// // Create metadata for consensus
    /// let metadata = ValueMetadata::new(header, commitments);
    /// ```
    ///
    /// ## Engine API v3 Response Format
    ///
    /// The `getPayloadV3` response includes a `blobsBundle` field:
    ///
    /// ```json
    /// {
    ///   "executionPayload": { ... },
    ///   "blockValue": "0x...",
    ///   "blobsBundle": {
    ///     "commitments": ["0x...", "0x..."],  // KZG commitments (hex, 48 bytes each)
    ///     "proofs": ["0x...", "0x..."],        // KZG proofs (hex, 48 bytes each)
    ///     "blobs": ["0x...", "0x..."]          // Blob data (hex, 131072 bytes each)
    ///   },
    ///   "shouldOverrideBuilder": false
    /// }
    /// ```
    ///
    /// If no blob transactions were included, `blobsBundle` may be absent or have empty arrays.
    ///
    /// ## Implementation
    ///
    /// This method uses `EngineApi::get_payload_with_blobs()` which calls `getPayloadV3`
    /// and parses the full response including the blob bundle. The blob bundle is then
    /// converted from Alloy's `BlobsBundleV1` to our internal `BlobsBundle` type and
    /// validated for structure correctness.
    pub async fn generate_block_with_blobs(
        &self,
        latest_block: &ultramarine_types::engine_api::ExecutionBlock,
    ) -> eyre::Result<(ExecutionPayloadResult, Option<BlobsBundle>)> {
        debug!("🟠 generate_block_with_blobs on top of {:?}", latest_block);

        let block_hash = latest_block.block_hash;
        let payload_attributes = PayloadAttributes {
            // Unix timestamp for when the payload is expected to be executed.
            // It should be greater than that of forkchoiceState.headBlockHash.
            timestamp: latest_block.timestamp + 1,

            // Load fixes PREVRANDAO to the canonical constant (Arbitrum-style) so no
            // application assumes entropy from it; the consensus doc captures this contract.
            prev_randao: load_prev_randao(),

            // CRITICAL TODO: This is a placeholder address. In a production environment,
            // this MUST be replaced with a user-configurable address to ensure
            // the validator operator receives their earned transaction fees (tips).
            suggested_fee_recipient: Address::repeat_byte(42).to_alloy_address(),

            // Cannot be None in V3.
            withdrawals: Some(vec![]),

            // Cannot be None in V3.
            parent_beacon_block_root: Some(block_hash),
        };

        let forkchoice_state = ForkchoiceState {
            head_block_hash: block_hash,
            finalized_block_hash: block_hash,
            safe_block_hash: block_hash,
        };

        // Step 1: Call forkchoiceUpdatedV3 to start block production
        let ForkchoiceUpdated { payload_status, payload_id } =
            self.engine.forkchoice_updated(forkchoice_state, Some(payload_attributes)).await?;

        tracing::debug!(
            status = ?payload_status.status,
            latest_valid_hash = ?payload_status.latest_valid_hash,
            has_payload_id = %payload_id.is_some(),
            head = ?block_hash,
            "forkchoiceUpdated (with attributes) response for blob block"
        );

        if payload_status.latest_valid_hash != Some(block_hash) {
            tracing::error!(
                latest_valid_hash = ?payload_status.latest_valid_hash,
                head = ?block_hash,
                "engine_forkchoiceUpdatedV3 returned mismatched latest_valid_hash"
            );
            return Err(eyre::eyre!(
                "engine_forkchoiceUpdatedV3 returned latestValidHash={:?} not matching head={:?}",
                payload_status.latest_valid_hash,
                block_hash
            ));
        }

        match payload_status.status {
            PayloadStatusEnum::Valid => {
                let Some(payload_id) = payload_id else {
                    tracing::error!("VALID status but payload_id is None after attributes");
                    return Err(eyre::eyre!(
                        "Engine API spec violation: VALID status with payload attributes must include payloadId"
                    ));
                };

                // Step 2: Call getPayloadV3 to retrieve the block and blob bundle
                //
                // This uses the new get_payload_with_blobs() method which:
                // 1. Calls getPayloadV3 via Engine API
                // 2. Parses the full ExecutionPayloadEnvelopeV3 including blobs_bundle
                // 3. Converts Alloy's BlobsBundleV1 to our BlobsBundle type
                // 4. Validates the bundle structure
                let (payload_result, blob_bundle) =
                    self.engine.get_payload_with_blobs(payload_id).await?;

                let blob_count = blob_bundle.as_ref().map(|b| b.len()).unwrap_or(0);
                let payload_inner = &payload_result.payload.payload_inner.payload_inner;
                tracing::info!(
                    block_hash = ?payload_inner.block_hash,
                    parent_hash = ?payload_inner.parent_hash,
                    block_number = payload_inner.block_number,
                    txs = payload_inner.transactions.len(),
                    blob_gas_used = payload_result.payload.blob_gas_used,
                    excess_blob_gas = payload_result.payload.excess_blob_gas,
                    blob_count = blob_count,
                    execution_requests = payload_result.execution_requests.len(),
                    "Received execution payload with blobs from EL"
                );

                Ok((payload_result, blob_bundle))
            }
            // TODO: A production-ready client must handle all possible statuses gracefully.
            // See comments in generate_block() for full status handling requirements.
            status => {
                tracing::error!(
                    ?status,
                    "forkchoiceUpdated (with attributes) returned non-VALID status"
                );
                Err(eyre::eyre!("Invalid payload status: {}", status))
            }
        }
    }

    pub async fn notify_new_block(
        &self,
        execution_payload: ExecutionPayloadV3,
        execution_requests: Vec<Bytes>,
        versioned_hashes: Vec<B256>,
    ) -> eyre::Result<PayloadStatus> {
        let parent_block_hash = execution_payload.payload_inner.payload_inner.parent_hash;
        tracing::debug!(
            parent = ?parent_block_hash,
            block = ?execution_payload.payload_inner.payload_inner.block_hash,
            number = execution_payload.payload_inner.payload_inner.block_number,
            "Submitting new payload to EL"
        );
        self.engine
            .new_payload(execution_payload, versioned_hashes, parent_block_hash, execution_requests)
            .await
    }

    /// Returns the duration since the unix epoch.
    fn _timestamp_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs()
    }
}

/// Adapter that exposes `ExecutionClient` functionality via the `ExecutionNotifier` trait.
#[derive(Debug)]
pub struct ExecutionClientNotifier<'a> {
    client: &'a ExecutionClient,
}

#[async_trait]
impl<'a> crate::notifier::ExecutionNotifier for ExecutionClientNotifier<'a> {
    async fn notify_new_block(
        &mut self,
        payload: ExecutionPayloadV3,
        execution_requests: Vec<Bytes>,
        versioned_hashes: Vec<BlockHash>,
    ) -> color_eyre::Result<PayloadStatus> {
        self.client.notify_new_block(payload, execution_requests, versioned_hashes).await
    }

    async fn set_latest_forkchoice_state(
        &mut self,
        block_hash: BlockHash,
    ) -> color_eyre::Result<BlockHash> {
        self.client.set_latest_forkchoice_state(block_hash).await
    }
}
