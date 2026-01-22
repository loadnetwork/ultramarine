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
    constants::LOAD_MIN_BLOCK_TIME_SECS,
    engine_api::load_prev_randao,
};

use crate::{
    config::{EngineApiEndpoint, ExecutionConfig},
    engine_api::{EngineApi, ExecutionPayloadResult, client::EngineApiClient},
    error::ExecutionError,
    eth_rpc::{EthRpc, alloy_impl::AlloyEthRpc},
    transport::{http::HttpTransport, ipc::IpcTransport},
};

// Pluggable time source to keep tests deterministic without global state.
trait TimeProvider: Send + Sync {
    fn now_secs(&self) -> u64;
}

#[derive(Debug, Default)]
struct SystemTimeProvider;

impl TimeProvider for SystemTimeProvider {
    fn now_secs(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

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
    /// Injected clock for deterministic testing and controlled timestamping.
    time_provider: Arc<dyn TimeProvider>,
    forkchoice_with_attrs_max_attempts: usize,
    forkchoice_with_attrs_delay: Duration,
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
                let transport = HttpTransport::new(url)?.with_jwt(config.jwt_secret);
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

        let forkchoice_with_attrs_max_attempts =
            config.forkchoice_with_attrs_max_attempts.unwrap_or(10).max(1);
        let forkchoice_with_attrs_delay =
            Duration::from_millis(config.forkchoice_with_attrs_delay_ms.unwrap_or(200));

        Ok(Self {
            engine: engine_client,
            eth: eth_client,
            time_provider: Arc::new(SystemTimeProvider),
            forkchoice_with_attrs_max_attempts,
            forkchoice_with_attrs_delay,
        })
    }

    pub fn engine(&self) -> &dyn EngineApi {
        self.engine.as_ref()
    }

    pub fn eth(&self) -> &dyn EthRpc {
        self.eth.as_ref()
    }

    fn current_unix_time(&self) -> u64 {
        self.time_provider.now_secs()
    }

    /// Compute next block timestamp. Must be called AFTER throttling ensures now >= parent +
    /// min_block_time.
    fn next_block_timestamp(&self, parent_timestamp: u64) -> u64 {
        std::cmp::max(self.current_unix_time(), parent_timestamp + LOAD_MIN_BLOCK_TIME_SECS)
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
            PayloadStatusEnum::Syncing => {
                // Engine API spec: `engine_forkchoiceUpdatedV3` can return SYNCING with
                // `latestValidHash: null` if `headBlockHash` references an unknown payload or one
                // that can't be validated because requisite data is missing.
                //
                // We cannot treat this as success (forkchoice may not have been applied), so we
                // return an error that callers can treat as transient backpressure.
                if payload_status.latest_valid_hash.is_some() {
                    tracing::warn!(
                        head_block_hash = ?head_block_hash,
                        latest_valid_hash = ?payload_status.latest_valid_hash,
                        "engine_forkchoiceUpdatedV3 returned SYNCING with non-null latestValidHash"
                    );
                }
                tracing::warn!(
                    head_block_hash = ?head_block_hash,
                    "forkchoiceUpdated returned SYNCING; EL not ready"
                );
                Err(eyre::Report::new(ExecutionError::SyncingForkchoice { head: head_block_hash }))
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

        // Compute expected timestamp ONCE, use for request AND validation
        let expected_timestamp = self.next_block_timestamp(latest_block.timestamp);

        let payload_attributes = PayloadAttributes {
            // Unix timestamp for when the payload is expected to be executed.
            // Wall-clock aligned: max(now, parent + LOAD_MIN_BLOCK_TIME_SECS)
            timestamp: expected_timestamp,

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

        let ForkchoiceUpdated { payload_status, payload_id } = self
            .forkchoice_updated_with_attributes_retry(forkchoice_state, payload_attributes)
            .await?;

        tracing::debug!(
            status = ?payload_status.status,
            latest_valid_hash = ?payload_status.latest_valid_hash,
            has_payload_id = %payload_id.is_some(),
            head = ?block_hash,
            "forkchoiceUpdated (with attributes) response"
        );

        // Note: Engine API can return SYNCING with latestValidHash=null while it is not ready.
        // We handle that via retry in `forkchoice_updated_with_attributes_retry`.

        match payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted => {
                let Some(payload_id) = payload_id else {
                    tracing::error!(
                        status = ?payload_status.status,
                        "forkchoiceUpdated status requires payloadId but payload_id is None after attributes"
                    );
                    return Err(eyre::eyre!(
                        "Engine API spec violation: forkchoiceUpdated with payload attributes must include payloadId (status={})",
                        payload_status.status
                    ));
                };
                if payload_status.status == PayloadStatusEnum::Accepted {
                    tracing::warn!(
                        head = ?block_hash,
                        latest_valid_hash = ?payload_status.latest_valid_hash,
                        "forkchoiceUpdated returned ACCEPTED with payload attributes; proceeding using payloadId"
                    );
                }
                // See how payload is constructed: https://github.com/ethereum/consensus-specs/blob/v1.1.5/specs/merge/validator.md#block-proposal
                let payload_result = self.engine.get_payload(payload_id).await?;
                let payload_inner = &payload_result.payload.payload_inner.payload_inner;
                // Safety: ensure the payload we got is actually built on top of the requested head
                // with OUR expected timestamp. This protects against EL quirks.
                if payload_inner.parent_hash != block_hash ||
                    payload_inner.block_number != latest_block.block_number + 1 ||
                    payload_inner.timestamp != expected_timestamp
                {
                    return Err(eyre::Report::new(ExecutionError::BuiltPayloadMismatch {
                        head: block_hash,
                        parent: payload_inner.parent_hash,
                        block_number: payload_inner.block_number,
                        timestamp: payload_inner.timestamp,
                    }));
                }
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
    /// let header = ExecutionPayloadHeader::from_payload(&payload, None)?;
    ///
    /// // Extract commitments for ValueMetadata
    /// let commitments = maybe_bundle.map(|b| b.commitments).unwrap_or_else(Vec::new);
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

        // Compute expected timestamp ONCE, use for request AND validation
        let expected_timestamp = self.next_block_timestamp(latest_block.timestamp);

        let payload_attributes = PayloadAttributes {
            // Unix timestamp for when the payload is expected to be executed.
            // Wall-clock aligned: max(now, parent + LOAD_MIN_BLOCK_TIME_SECS)
            timestamp: expected_timestamp,

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
        let ForkchoiceUpdated { payload_status, payload_id } = self
            .forkchoice_updated_with_attributes_retry(forkchoice_state, payload_attributes)
            .await?;

        tracing::debug!(
            status = ?payload_status.status,
            latest_valid_hash = ?payload_status.latest_valid_hash,
            has_payload_id = %payload_id.is_some(),
            head = ?block_hash,
            "forkchoiceUpdated (with attributes) response for blob block"
        );

        // Note: Engine API can return SYNCING with latestValidHash=null while it is not ready.
        // We handle that via retry in `forkchoice_updated_with_attributes_retry`.

        match payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Accepted => {
                let Some(payload_id) = payload_id else {
                    tracing::error!(
                        status = ?payload_status.status,
                        "forkchoiceUpdated status requires payloadId but payload_id is None after attributes"
                    );
                    return Err(eyre::eyre!(
                        "Engine API spec violation: forkchoiceUpdated with payload attributes must include payloadId (status={})",
                        payload_status.status
                    ));
                };
                if payload_status.status == PayloadStatusEnum::Accepted {
                    tracing::warn!(
                        head = ?block_hash,
                        latest_valid_hash = ?payload_status.latest_valid_hash,
                        "forkchoiceUpdated returned ACCEPTED with payload attributes; proceeding using payloadId"
                    );
                }

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
                let has_blob_gas = payload_result.payload.blob_gas_used > 0;
                if has_blob_gas && blob_count == 0 {
                    return Err(eyre::eyre!(
                        "Engine API spec violation: payload has blob_gas_used={} but blobs bundle is empty",
                        payload_result.payload.blob_gas_used
                    ));
                }
                if !has_blob_gas && blob_count > 0 {
                    return Err(eyre::eyre!(
                        "Engine API spec violation: blobs bundle has {} blobs but payload blob_gas_used=0",
                        blob_count
                    ));
                }
                // Safety: ensure the payload we got is actually built on top of the requested head
                // with OUR expected timestamp. This protects against EL quirks.
                if payload_inner.parent_hash != block_hash ||
                    payload_inner.block_number != latest_block.block_number + 1 ||
                    payload_inner.timestamp != expected_timestamp
                {
                    return Err(eyre::Report::new(ExecutionError::BuiltPayloadMismatch {
                        head: block_hash,
                        parent: payload_inner.parent_hash,
                        block_number: payload_inner.block_number,
                        timestamp: payload_inner.timestamp,
                    }));
                }
                tracing::info!(
                    block_hash = ?payload_inner.block_hash,
                    parent_hash = ?payload_inner.parent_hash,
                    block_number = payload_inner.block_number,
                    timestamp = payload_inner.timestamp,
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

    async fn forkchoice_updated_with_attributes_retry(
        &self,
        forkchoice_state: ForkchoiceState,
        payload_attributes: PayloadAttributes,
    ) -> eyre::Result<ForkchoiceUpdated> {
        let head = forkchoice_state.head_block_hash;
        // Keep retries short: the consensus round timer should stay in control (Tendermint-style).
        // This just smooths transient "EL restarting" windows.
        let delay = self.forkchoice_with_attrs_delay;
        let max_attempts = self.forkchoice_with_attrs_max_attempts;
        let mut attempts = 0usize;

        loop {
            attempts += 1;
            let res = self
                .engine
                .forkchoice_updated(forkchoice_state, Some(payload_attributes.clone()))
                .await?;

            match res.payload_status.status {
                PayloadStatusEnum::Valid => {
                    if res.payload_status.latest_valid_hash != Some(head) {
                        return Err(eyre::eyre!(
                            "engine_forkchoiceUpdatedV3 returned latestValidHash={:?} not matching head={:?}",
                            res.payload_status.latest_valid_hash,
                            head
                        ));
                    }
                    // The spec allows returning `payloadId: null` if the EL does not begin a build
                    // process (e.g. head is a VALID ancestor, or EL is temporarily unable to
                    // build). Treat this as transient backpressure and retry briefly.
                    if res.payload_id.is_none() {
                        if attempts >= max_attempts {
                            return Err(eyre::Report::new(ExecutionError::NoPayloadIdForBuild {
                                head,
                            }));
                        }
                        tracing::warn!(
                            head = ?head,
                            "engine_forkchoiceUpdatedV3 returned VALID but payloadId is null; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok(res);
                }
                PayloadStatusEnum::Syncing => {
                    // Engine API spec: SYNCING commonly returns latestValidHash=null and
                    // payloadId=null. See `execution-apis/src/engine/paris.md`
                    // (forkchoiceUpdated spec point 8).
                    if res.payload_id.is_some() {
                        return Err(eyre::eyre!(
                            "Engine API spec violation: engine_forkchoiceUpdatedV3 returned SYNCING with non-null payloadId (head={:?})",
                            head
                        ));
                    }
                    if res.payload_status.latest_valid_hash.is_some() {
                        tracing::warn!(
                            head = ?head,
                            latest_valid_hash = ?res.payload_status.latest_valid_hash,
                            "engine_forkchoiceUpdatedV3 returned SYNCING with non-null latestValidHash"
                        );
                    }
                    if attempts >= max_attempts {
                        return Err(eyre::eyre!(
                            "engine_forkchoiceUpdatedV3 still SYNCING after {} attempts (head={:?})",
                            attempts,
                            head
                        ));
                    }
                    tracing::warn!(
                        head = ?head,
                        latest_valid_hash = ?res.payload_status.latest_valid_hash,
                        "engine_forkchoiceUpdatedV3 returned SYNCING; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                PayloadStatusEnum::Accepted => {
                    // `ACCEPTED` is not listed as a valid response for forkchoiceUpdated in the
                    // spec. Be liberal:
                    // - If the EL provides a payloadId, proceed so proposers can keep producing
                    //   blocks.
                    // - Otherwise retry briefly, then fail so consensus can progress to nil.
                    if res.payload_status.latest_valid_hash.is_some() {
                        tracing::warn!(
                            head = ?head,
                            latest_valid_hash = ?res.payload_status.latest_valid_hash,
                            "engine_forkchoiceUpdatedV3 returned ACCEPTED with non-null latestValidHash"
                        );
                    }
                    if res.payload_id.is_some() {
                        tracing::warn!(
                            head = ?head,
                            latest_valid_hash = ?res.payload_status.latest_valid_hash,
                            "engine_forkchoiceUpdatedV3 returned ACCEPTED with payloadId; proceeding"
                        );
                        return Ok(res);
                    }
                    if attempts >= max_attempts {
                        return Err(eyre::eyre!(
                            "engine_forkchoiceUpdatedV3 stuck in ACCEPTED after {} attempts (head={:?})",
                            attempts,
                            head
                        ));
                    }
                    tracing::warn!(
                        head = ?head,
                        latest_valid_hash = ?res.payload_status.latest_valid_hash,
                        "engine_forkchoiceUpdatedV3 returned ACCEPTED without payloadId; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                status => {
                    return Err(eyre::eyre!(
                        "engine_forkchoiceUpdatedV3 returned non-VALID status: {}",
                        status
                    ));
                }
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_eips::eip7685::Requests;
    use alloy_primitives::{
        Address as AlloyAddress, B256 as AB256, Bloom, Bytes as AlloyBytes, FixedBytes, U256,
    };
    use alloy_rpc_types::{BlockNumberOrTag, Filter, Log, SyncStatus};
    use alloy_rpc_types_engine::{
        BlobsBundleV1, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4, ExecutionPayloadV1,
        ExecutionPayloadV2, PayloadId,
    };
    use alloy_rpc_types_txpool::{TxpoolInspect, TxpoolStatus};
    use serde_json::json;

    use super::*;
    use crate::transport::{JsonRpcRequest, JsonRpcResponse, Transport};

    struct NoopEthRpc;

    #[async_trait]
    impl EthRpc for NoopEthRpc {
        async fn get_chain_id(&self) -> eyre::Result<String> {
            Err(eyre::eyre!("not implemented"))
        }
        async fn syncing(&self) -> eyre::Result<SyncStatus> {
            Err(eyre::eyre!("not implemented"))
        }
        async fn get_logs(&self, _filter: &Filter) -> eyre::Result<Vec<Log>> {
            Err(eyre::eyre!("not implemented"))
        }
        async fn get_block_by_number(
            &self,
            _block_number: BlockNumberOrTag,
            _full_transactions: bool,
        ) -> eyre::Result<Option<ultramarine_types::engine_api::ExecutionBlock>> {
            Err(eyre::eyre!("not implemented"))
        }
        async fn txpool_status(&self) -> eyre::Result<TxpoolStatus> {
            Err(eyre::eyre!("not implemented"))
        }
        async fn txpool_inspect(&self) -> eyre::Result<TxpoolInspect> {
            Err(eyre::eyre!("not implemented"))
        }
    }

    const TEST_NOW: u64 = 1_700_000_001;

    struct FixedTimeProvider {
        time: u64,
    }

    impl TimeProvider for FixedTimeProvider {
        fn now_secs(&self) -> u64 {
            self.time
        }
    }

    #[derive(Clone)]
    struct ScriptedEngineApi {
        responses: Arc<Mutex<Vec<ForkchoiceUpdated>>>,
        payloads: Arc<Mutex<Vec<ExecutionPayloadResult>>>,
    }

    impl ScriptedEngineApi {
        fn new(responses: Vec<ForkchoiceUpdated>, payload: ExecutionPayloadResult) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().rev().collect())),
                payloads: Arc::new(Mutex::new(vec![payload])),
            }
        }
    }

    #[async_trait]
    impl EngineApi for ScriptedEngineApi {
        async fn forkchoice_updated(
            &self,
            _state: ForkchoiceState,
            _payload_attributes: Option<PayloadAttributes>,
        ) -> eyre::Result<ForkchoiceUpdated> {
            let mut guard = self.responses.lock().unwrap();
            guard
                .pop()
                .ok_or_else(|| eyre::eyre!("scripted forkchoice_updated responses exhausted"))
        }

        async fn get_payload(
            &self,
            _payload_id: PayloadId,
        ) -> eyre::Result<ExecutionPayloadResult> {
            self.payloads
                .lock()
                .unwrap()
                .last()
                .cloned()
                .ok_or_else(|| eyre::eyre!("scripted payload missing"))
        }

        async fn get_payload_with_blobs(
            &self,
            payload_id: PayloadId,
        ) -> eyre::Result<(ExecutionPayloadResult, Option<BlobsBundle>)> {
            Ok((self.get_payload(payload_id).await?, None))
        }

        async fn new_payload(
            &self,
            _execution_payload: ExecutionPayloadV3,
            _versioned_hashes: Vec<B256>,
            _parent_block_hash: BlockHash,
            _execution_requests: Vec<Bytes>,
        ) -> eyre::Result<PayloadStatus> {
            Ok(PayloadStatus::from_status(PayloadStatusEnum::Valid))
        }

        async fn exchange_capabilities(
            &self,
        ) -> eyre::Result<crate::engine_api::capabilities::EngineCapabilities> {
            Err(eyre::eyre!("not implemented"))
        }
    }

    fn sample_payload_result(
        block_hash: AB256,
        parent_hash: AB256,
        block_number: u64,
    ) -> ExecutionPayloadResult {
        let payload = ExecutionPayloadV3 {
            blob_gas_used: 0,
            excess_blob_gas: 0,
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash,
                    fee_recipient: AlloyAddress::from([6u8; 20]),
                    state_root: AB256::from([3u8; 32]),
                    receipts_root: AB256::from([4u8; 32]),
                    logs_bloom: Bloom::ZERO,
                    prev_randao: load_prev_randao(),
                    block_number,
                    gas_limit: ultramarine_types::constants::LOAD_EXECUTION_GAS_LIMIT,
                    gas_used: ultramarine_types::constants::LOAD_EXECUTION_GAS_LIMIT / 2,
                    timestamp: 1_700_000_000 + block_number,
                    extra_data: AlloyBytes::new(),
                    base_fee_per_gas: U256::from(1),
                    block_hash,
                    transactions: Vec::new(),
                },
                withdrawals: Vec::new(),
            },
        };
        ExecutionPayloadResult { payload, execution_requests: Vec::new() }
    }

    fn make_latest_block(
        block_hash: AB256,
        number: u64,
        parent_hash: AB256,
        timestamp: u64,
    ) -> ultramarine_types::engine_api::ExecutionBlock {
        ultramarine_types::engine_api::ExecutionBlock {
            block_hash,
            block_number: number,
            parent_hash,
            timestamp,
            prev_randao: load_prev_randao(),
        }
    }

    #[derive(Clone)]
    struct StaticTransport {
        result: serde_json::Value,
    }

    #[async_trait]
    impl Transport for StaticTransport {
        async fn send(&self, req: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: Some(self.result.clone()),
                error: None,
                id: req.id,
            })
        }
    }

    #[tokio::test]
    async fn accepted_with_payload_id_proceeds_to_get_payload() {
        let head = AB256::from([9u8; 32]);
        let payload_id = PayloadId::from(FixedBytes::<8>::from([1u8; 8]));
        let payload_result = sample_payload_result(AB256::from([7u8; 32]), head, 2);

        let engine = ScriptedEngineApi::new(
            vec![ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Accepted, None),
                payload_id: Some(payload_id),
            }],
            payload_result.clone(),
        );

        let client = ExecutionClient {
            engine: Arc::new(engine),
            eth: Arc::new(NoopEthRpc),
            time_provider: Arc::new(FixedTimeProvider { time: TEST_NOW }),
            forkchoice_with_attrs_max_attempts: 1,
            forkchoice_with_attrs_delay: Duration::from_millis(0),
        };

        let latest_block = make_latest_block(head, 1, AB256::from([8u8; 32]), 1_700_000_001);
        let res = client.generate_block(&latest_block).await.unwrap();
        assert_eq!(
            res.payload.payload_inner.payload_inner.parent_hash, head,
            "payload returned by get_payload should be used"
        );
    }

    #[tokio::test]
    async fn syncing_retries_then_recovers() {
        let head = AB256::from([9u8; 32]);
        let payload_id = PayloadId::from(FixedBytes::<8>::from([2u8; 8]));
        let payload_result = sample_payload_result(AB256::from([7u8; 32]), head, 2);

        let mut scripted = Vec::new();
        for _ in 0..3 {
            scripted.push(ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Syncing, None),
                payload_id: None,
            });
        }
        scripted.push(ForkchoiceUpdated {
            payload_status: PayloadStatus::new(PayloadStatusEnum::Valid, Some(head)),
            payload_id: Some(payload_id),
        });

        let engine = ScriptedEngineApi::new(scripted, payload_result.clone());

        let client = ExecutionClient {
            engine: Arc::new(engine),
            eth: Arc::new(NoopEthRpc),
            time_provider: Arc::new(FixedTimeProvider { time: TEST_NOW }),
            forkchoice_with_attrs_max_attempts: 10,
            forkchoice_with_attrs_delay: Duration::from_millis(1),
        };

        let latest_block = make_latest_block(head, 1, AB256::from([8u8; 32]), 1_700_000_001);
        let res = client.generate_block(&latest_block).await.unwrap();
        assert_eq!(res.payload.payload_inner.payload_inner.parent_hash, head);
    }

    #[tokio::test]
    async fn syncing_with_payload_id_is_error() {
        let head = AB256::from([9u8; 32]);
        let payload_id = PayloadId::from(FixedBytes::<8>::from([5u8; 8]));
        let payload_result = sample_payload_result(AB256::from([7u8; 32]), head, 2);

        let engine = ScriptedEngineApi::new(
            vec![ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Syncing, None),
                payload_id: Some(payload_id),
            }],
            payload_result,
        );

        let client = ExecutionClient {
            engine: Arc::new(engine),
            eth: Arc::new(NoopEthRpc),
            time_provider: Arc::new(FixedTimeProvider { time: TEST_NOW }),
            forkchoice_with_attrs_max_attempts: 1,
            forkchoice_with_attrs_delay: Duration::from_millis(0),
        };

        let latest_block = make_latest_block(head, 1, AB256::from([8u8; 32]), 1_700_000_001);
        let err = client.generate_block(&latest_block).await.unwrap_err();
        assert!(
            err.to_string().contains("SYNCING with non-null payloadId"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_payload_built_on_wrong_parent() {
        let head = AB256::from([9u8; 32]);
        let payload_id = PayloadId::from(FixedBytes::<8>::from([3u8; 8]));
        // Return a payload that claims a different parent than the requested head.
        let payload_result =
            sample_payload_result(AB256::from([7u8; 32]), AB256::from([8u8; 32]), 2);

        let engine = ScriptedEngineApi::new(
            vec![ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Accepted, None),
                payload_id: Some(payload_id),
            }],
            payload_result,
        );

        let client = ExecutionClient {
            engine: Arc::new(engine),
            eth: Arc::new(NoopEthRpc),
            time_provider: Arc::new(FixedTimeProvider { time: TEST_NOW }),
            forkchoice_with_attrs_max_attempts: 1,
            forkchoice_with_attrs_delay: Duration::from_millis(0),
        };

        let latest_block = make_latest_block(head, 1, AB256::from([1u8; 32]), 1_700_000_001);
        let err = client.generate_block(&latest_block).await.unwrap_err();
        let mismatch = err.downcast_ref::<ExecutionError>();
        assert!(
            matches!(mismatch, Some(ExecutionError::BuiltPayloadMismatch { head: h, .. }) if *h == head),
            "expected typed BuiltPayloadMismatch error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn valid_without_payload_id_retries_then_succeeds() {
        let head = AB256::from([9u8; 32]);
        let payload_id = PayloadId::from(FixedBytes::<8>::from([4u8; 8]));
        let payload_result = sample_payload_result(AB256::from([7u8; 32]), head, 2);

        let scripted = vec![
            ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Valid, Some(head)),
                payload_id: None,
            },
            ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Valid, Some(head)),
                payload_id: Some(payload_id),
            },
        ];

        let engine = ScriptedEngineApi::new(scripted, payload_result.clone());
        let client = ExecutionClient {
            engine: Arc::new(engine),
            eth: Arc::new(NoopEthRpc),
            time_provider: Arc::new(FixedTimeProvider { time: TEST_NOW }),
            forkchoice_with_attrs_max_attempts: 5,
            forkchoice_with_attrs_delay: Duration::from_millis(1),
        };

        let latest_block = make_latest_block(head, 1, AB256::from([8u8; 32]), 1_700_000_001);
        let res = client.generate_block(&latest_block).await.unwrap();
        assert_eq!(res.payload.payload_inner.payload_inner.parent_hash, head);
    }

    #[tokio::test]
    async fn valid_without_payload_id_exhausts_retry_budget() {
        let head = AB256::from([9u8; 32]);
        let payload_result = sample_payload_result(AB256::from([7u8; 32]), head, 2);

        let scripted = vec![
            ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Valid, Some(head)),
                payload_id: None,
            },
            ForkchoiceUpdated {
                payload_status: PayloadStatus::new(PayloadStatusEnum::Valid, Some(head)),
                payload_id: None,
            },
        ];

        let engine = ScriptedEngineApi::new(scripted, payload_result.clone());
        let client = ExecutionClient {
            engine: Arc::new(engine),
            eth: Arc::new(NoopEthRpc),
            time_provider: Arc::new(FixedTimeProvider { time: TEST_NOW }),
            forkchoice_with_attrs_max_attempts: 2,
            forkchoice_with_attrs_delay: Duration::from_millis(0),
        };

        let latest_block = make_latest_block(head, 1, AB256::from([8u8; 32]), 1_700_000_001);
        let err = client.generate_block(&latest_block).await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<ExecutionError>(),
                Some(ExecutionError::NoPayloadIdForBuild { head: h }) if *h == head
            ),
            "expected NoPayloadIdForBuild, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn get_payload_accepts_extra_fields() {
        let payload_result =
            sample_payload_result(AB256::from([7u8; 32]), AB256::from([6u8; 32]), 2);

        let envelope_v3 = ExecutionPayloadEnvelopeV3 {
            execution_payload: payload_result.payload.clone(),
            block_value: U256::ZERO,
            blobs_bundle: BlobsBundleV1 { commitments: vec![], proofs: vec![], blobs: vec![] },
            should_override_builder: false,
        };

        let envelope_v4 = ExecutionPayloadEnvelopeV4 {
            envelope_inner: envelope_v3,
            execution_requests: Requests::new(Vec::new()),
        };

        let mut value = serde_json::to_value(&envelope_v4).expect("serialize envelope");
        if let Some(obj) = value.as_object_mut() {
            obj.insert("extraField".to_string(), json!("0x01"));
        }
        if let Some(payload_obj) =
            value.get_mut("executionPayload").and_then(|inner| inner.as_object_mut())
        {
            payload_obj.insert("extraPayloadField".to_string(), json!("0x02"));
        }

        let transport = StaticTransport { result: value };
        let client = EngineApiClient::new(transport);
        let payload_id = PayloadId::from(FixedBytes::<8>::from([9u8; 8]));
        let result = client.get_payload(payload_id).await.expect("getPayload with extras");

        assert_eq!(
            result.payload.payload_inner.payload_inner.block_number,
            payload_result.payload.payload_inner.payload_inner.block_number
        );
    }
}
