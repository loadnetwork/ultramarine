// TODO: Implement an actor-based client as an alternative to the Mutex-based approach.
// This could be more flexible and help to avoid deadlocks in more complex scenarios.
#![allow(missing_docs)]
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use color_eyre::eyre;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use super::{JsonRpcRequest, JsonRpcResponse, Transport};

// Align with HTTP transport defaults; block building or EL load can exceed 3s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct IpcTransport {
    path: PathBuf,
}

impl IpcTransport {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf() }
    }

    async fn connect(&self) -> eyre::Result<UnixStream> {
        let stream_future = UnixStream::connect(&self.path);
        let stream = tokio::time::timeout(REQUEST_TIMEOUT, stream_future).await??;
        Ok(stream)
    }
}

#[async_trait]
impl Transport for IpcTransport {
    async fn send(&self, req: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
        // Establish a fresh connection per request and half-close after write so the server
        // can terminate its side, allowing us to read a single full JSON response to EOF.
        let mut stream = self.connect().await?;

        let req_bytes = serde_json::to_vec(req)?;
        tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&req_bytes)).await??;
        // Signal end-of-request; many JSON-RPC IPC servers respond and then close.
        tokio::time::timeout(REQUEST_TIMEOUT, stream.shutdown()).await??;

        let mut resp_bytes = Vec::new();
        tokio::time::timeout(REQUEST_TIMEOUT, stream.read_to_end(&mut resp_bytes)).await??;

        serde_json::from_slice(&resp_bytes).map_err(|e| e.into())
    }
}
