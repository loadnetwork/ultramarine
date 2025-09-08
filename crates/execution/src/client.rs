use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadStatus,
    PayloadStatusEnum,
};
use color_eyre::eyre;
use tracing::debug;
use ultramarine_types::{
    address::Address,
    aliases::{B256, BlockHash},
};

use crate::{
    config::{EngineApiEndpoint, ExecutionConfig},
    engine_api::{self, EngineApi, client::EngineApiClient},
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

impl ExecutionClient {
    /// Creates a new `ExecutionClient` from the given configuration.
    ///
    /// This function is async because it needs to establish a connection
    /// to the execution node to initialize the EthRpc1 client.
    pub async fn new(config: ExecutionConfig) -> eyre::Result<Self> {
        // 1. Craete the Engine API client using its specific endpoint from the config.
        let engine_client: Arc<dyn EngineApi> = match config.engine_api_endpoint {
            EngineApiEndpoint::Http(url) => {
                let transport = HttpTransport::new(url).with_jwt(config.jwt_secret);
                Arc::new(EngineApiClient::new(transport))
            }
            EngineApiEndpoint::Ipc(path) => {
                let transport = IpcTransport::new(path);
                Arc::new(EngineApiClient::new(transport))
            }
        };

        // 2. Craete the standard Eth1 RPC client using its dedicated HTTP Url from the config.
        let eth_client: Arc<dyn EthRpc> = {
            let rpc_client = AlloyEthRpc::new(config.eth1_rpc_url);
            Arc::new(rpc_client)
        };

        Ok(Self { engine: engine_client, eth: eth_client })
    }

    pub fn engine(&self) -> &dyn EngineApi {
        self.engine.as_ref()
    }

    pub fn eth(&self) -> &dyn EthRpc {
        self.eth.as_ref()
    }

    pub async fn check_capabilities(&self) -> eyre::Result<()> {
        let cap = self.engine.exchange_capabilities().await?;
        if !cap.forkchoice_updated_v3 || !cap.get_payload_v3 || !cap.new_payload_v3 {
            return Err(eyre::eyre!("Engine does not required methods!"))
        }
        Ok(())
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
        assert!(payload_id.is_none(), "Payload ID should be None!");

        debug!("➡️ payload_status: {:?}", payload_status);

        match payload_status.status {
            PayloadStatusEnum::Valid => Ok(payload_status
                .latest_valid_hash
                .expect("Engine API spec violation: VALID status must have a latestValidHash")),
            PayloadStatusEnum::Syncing if payload_status.latest_valid_hash.is_none() => {
                // From the Engine API spec:
                // 8. Client software MUST respond to this method call in the following way:
                //   * {payloadStatus: {status: SYNCING, latestValidHash: null,
                //   * validationError: null}, payloadId: null} if forkchoiceState.headBlockHash
                //     references an unknown payload or a payload that can't be validated because
                //     requisite data for the validation is missing
                Err(eyre::eyre!(
                    "headBlockHash={:?} references an unknown payload or a payload that can't be validated",
                    head_block_hash
                ))
            }
            status => Err(eyre::eyre!("Invalid payload status: {}", status)),
        }
    }

    pub async fn generate_block(
        &self,
        latest_block: &ultramarine_types::engine_api::ExecutionBlock,
    ) -> eyre::Result<ExecutionPayloadV3> {
        debug!("🟠 generate_block on top of {:?}", latest_block);
        let block_hash = latest_block.block_hash;
        let payload_attributes = PayloadAttributes {
            // Unix timestamp for when the payload is expected to be executed.
            // It should be greater than that of forkchoiceState.headBlockHash.
            timestamp: latest_block.timestamp + 1,

            // TODO: This is a placeholder value. In a real consensus client, this value
            // must be generated according to the consensus protocol's specifications.
            //
            // In Ethereum PoS, this is the RANDAO mix from the Beacon Chain state, used for
            // proposer selection.
            //
            // In a Tendermint-based system (like Malachite), proposer selection is
            // deterministic (round-robin). The Host application would be responsible for
            // generating a deterministic value here, such as a hash of the previous
            // block's signatures, to satisfy the EVM's block header format.
            prev_randao: latest_block.prev_randao,

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

        assert_eq!(payload_status.latest_valid_hash, Some(block_hash));

        match payload_status.status {
            PayloadStatusEnum::Valid => {
                assert!(payload_id.is_some(), "Payload ID should be Some!");
                let payload_id = payload_id.expect("Engine API spec violation: VALID status with payload attributes must have a payloadId");
                // See how payload is constructed: https://github.com/ethereum/consensus-specs/blob/v1.1.5/specs/merge/validator.md#block-proposal
                Ok(self.engine.get_payload(payload_id).await?)
            }
            // TODO: A production client must handle all possible statuses gracefully.
            //
            // In a Tendermint-based system (like Malachite), the Host application
            // would need to handle these statuses from the execution client:
            // - `SYNCING`: The Host should pause or skip the block proposal for this round and wait
            //   for the execution client to catch up.
            // - `INVALID` / `INVALID_BLOCK_HASH`: This indicates a critical desynchronization. The
            //   Host must treat this as a fatal error for the round, halt consensus, and alert an
            //   operator.
            status => Err(eyre::eyre!("Invalid payload status: {}", status)),
        }
    }

    pub async fn notify_new_block(
        &self,
        execution_payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
    ) -> eyre::Result<PayloadStatus> {
        let parent_block_hash = execution_payload.payload_inner.payload_inner.parent_hash;
        self.engine.new_payload(execution_payload, versioned_hashes, parent_block_hash).await
    }

    /// Returns the duration since the unix epoch.
    fn _timestamp_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs()
    }
}
