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
    /// Max attempts for `engine_forkchoiceUpdatedV3` calls that include payload attributes.
    ///
    /// This is used during block production (proposal) and is intentionally bounded so the
    /// consensus round timer stays in control (Tendermint/Malachite-style).
    ///
    /// If unset, defaults are applied by `ExecutionClient`.
    pub forkchoice_with_attrs_max_attempts: Option<usize>,
    /// Delay in milliseconds between retry attempts for `forkchoiceUpdated` with attributes.
    ///
    /// If unset, defaults are applied by `ExecutionClient`.
    pub forkchoice_with_attrs_delay_ms: Option<u64>,
    /// Delay in milliseconds before calling `get_payload` after `forkchoice_updated` returns.
    ///
    /// This gives the EL builder time to poll the transaction pool and include transactions
    /// in the payload. The builder works incrementally - each `interval` (e.g., 25ms) it
    /// rebuilds the payload with more transactions. Without this delay, getPayload may
    /// return an empty or nearly-empty payload.
    ///
    /// Recommended: 500-800ms for high-throughput scenarios.
    /// If unset, defaults to 0 (no delay).
    pub get_payload_delay_ms: Option<u64>,
}
