// crates/execution/src/client.rs

// This file defines the main `ExecutionClient`, which acts as the public
// entry point to the entire crate.

// --- REFERENCE IMPLEMENTATION ---
// use crate::engine_api::{client::EngineApiClient, EngineApi};
// use crate::eth_rpc::{alloy_impl::AlloyEthRpc, EthRpc};
// use crate::transport::{HttpTransport, IpcTransport};
// use color_eyre::eyre;
// use std::path::Path;
// use std::sync::Arc;
// use url::Url;
//
// pub struct ExecutionClient {
// pub engine: Arc<dyn EngineApi>,
// pub eth: Arc<dyn EthRpc>,
// }
//
// impl ExecutionClient {
// pub fn with_ipc(ipc_path: impl AsRef<Path>) -> Self {
// let engine_transport = IpcTransport::new(ipc_path);
// let engine = Arc::new(EngineApiClient::new(engine_transport));
// let eth = Arc::new(AlloyEthRpc);
// Self { engine, eth }
// }
//
// pub fn with_http(url: Url, jwt_secret: Option<[u8; 32]>) -> eyre::Result<Self> {
// let mut engine_transport = HttpTransport::new(url.clone());
// if let Some(secret) = jwt_secret {
// engine_transport = engine_transport.with_jwt(secret);
// }
// let engine = Arc::new(EngineApiClient::new(engine_transport));
// let eth = Arc::new(AlloyEthRpc);
// Ok(Self { engine, eth })
// }
//
// pub fn engine(&self) -> &dyn EngineApi {
// self.engine.as_ref()
// }
//
// pub fn eth(&self) -> &dyn EthRpc {
// self.eth.as_ref()
// }
// }
