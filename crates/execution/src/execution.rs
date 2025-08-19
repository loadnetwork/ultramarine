use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceUpdated, PayloadAttributes, PayloadStatus, PayloadStatusEnum,
};
use color_eyre::eyre::{self, Ok};
use tracing::debug;
use ultramarine_types::{address::Address, aliase::BlockHash, aliases::B256};

use crate::{engine::EngineApiClient, eth_rpc::EthereumRpcClient, json_structures::ExecutionBlock};

pub struct ExecutionApi {
    pub engine: Engine,
    pub rpc: EthRpc,
}

impl ExecutionApi {
    pub fn new(engine: EngineApiClient, rpc: EthereumRpcClient) -> Self {
        Self { engine, rpc }
    }
}
