use std::sync::Arc;

use color_eyre::eyre;

use crate::{
    config::{EngineApiEndpoint, ExecutionConfig},
    engine_api::{EngineApi, client::EngineApiClient},
    eth_rpc::{EthRpc, alloy_impl::AlloyEthRpc},
    transport::{http::HttpTransport, ipc::IpcTransport},
};

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
}
