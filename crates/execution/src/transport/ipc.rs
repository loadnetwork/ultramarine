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
    sync::Mutex,
};

use super::{JsonRpcRequest, JsonRpcResponse, Transport};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

pub struct IpcTransport {
    path: PathBuf,
    connection: Mutex<Option<UnixStream>>,
}

impl IpcTransport {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf(), connection: Mutex::new(None) }
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
        let mut conn_guard = self.connection.lock().await;

        if conn_guard.is_none() {
            *conn_guard = Some(self.connect().await?);
        }

        if let Some(stream) = conn_guard.as_mut() {
            let req_bytes = serde_json::to_vec(req)?;

            if let Err(e) = stream.write_all(&req_bytes).await {
                // Connection is broken, reset it and retry next time.
                *conn_guard = None;
                return Err(e.into())
            }

            let mut resp_bytes = Vec::new();
            let read_future = stream.read_to_end(&mut resp_bytes);

            if let Err(e) = tokio::time::timeout(REQUEST_TIMEOUT, read_future).await? {
                // Connection is broken, reset it and retry next time.
                *conn_guard = None;
                return Err(e.into())
            }

            serde_json::from_slice(&resp_bytes).map_err(|e| e.into())
        } else {
            // This should not happen, as we just created the connection.
            Err(eyre::eyre!("Failed to get IPC connection"))
        }
    }
}
