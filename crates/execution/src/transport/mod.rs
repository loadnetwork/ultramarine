// crates/execution/src/transport/mod.rs
#![allow(missing_docs)]

pub mod http;
pub mod ipc;
mod mock;

use async_trait::async_trait;
use color_eyre::eyre;
use serde::{Deserialize, Serialize};

/// A generic transport for sending JSON-RPC requests.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Sends a JSON-RPC request and returns the response.
    async fn send(&self, req: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse>;
}

/// Represents a JSON-RPC request object.
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: serde_json::Value,
    pub id: u64,
}

impl JsonRpcRequest {
    pub fn new(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0", method: method.into(), params, id: 1 }
    }
}

/// Represents a JSON-RPC response object.
#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
    pub id: u64,
}

/// Represents a JSON-RPC error object.
#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}
