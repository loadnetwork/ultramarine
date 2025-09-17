#![allow(missing_docs)]
use std::path::PathBuf;

use url::Url;

/// Defines the endpoint for the Engine API, which can be HTTP or IPC.
#[derive(Debug, Clone)]
pub enum EngineApiEndpoint {
    Http(Url),
    Ipc(PathBuf),
}

/// Holds all necessary parameters to connect to an execution node.
/// This new config supports separate endpoints for the Engine API and the
/// standard Eth1 RPC, which is a more robust and flexible pattern.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// The endpoint for the Engine API (can be HTTP or IPC).
    pub engine_api_endpoint: EngineApiEndpoint,
    /// The URL for the standard Eth1 JSON-RPC endpoint (must be HTTP).
    pub eth1_rpc_url: Url,
    /// The JWT secret for authenticating the Engine API connection.
    pub jwt_secret: [u8; 32],
}
