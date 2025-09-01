// TODO: IN PROGRESS
//! A mock transport for testing the Engine API client.

use std::collections::HashMap;

use async_trait::async_trait;
use color_eyre::eyre::{self, eyre};
use serde_json::Value;
use tokio::sync::Mutex;

use super::{JsonRpcRequest, JsonRpcResponse, Transport};

/// A mock transport that can be programmed with expected responses for testing.
#[derive(Debug, Default)]
pub struct MockTransport {
    // A map of method names to the JSON value that should be returned.
    // The value can be a success (Ok) or an error (Err) to test both paths.
    responses: Mutex<HashMap<String, eyre::Result<Value>>>,
}

impl MockTransport {
    /// Creates a new, empty mock transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a response to the mock's repertoire.
    ///
    /// When a request with the given `method` is received, the mock will
    /// return the provided `response`.
    pub async fn push_response(&self, method: String, response: eyre::Result<Value>) {
        self.responses.lock().await.insert(method, response);
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send(&self, request: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
        let response = self.responses.lock().await.remove(&request.method);

        match response {
            // If a response was programmed for this method, return it.
            Some(Ok(result)) => Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            }),
            // If an error was programmed, return that.
            Some(Err(e)) => Err(e),
            // If no response was programmed for this method, it's an unexpected call.
            None => Err(eyre!(
                "MockTransport: received unexpected call to method '''{}'''",
                request.method
            )),
        }
    }
}
