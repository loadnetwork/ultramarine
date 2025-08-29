// crates/execution/src/transport/ipc.rs

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use color_eyre::eyre;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use super::{JsonRpcRequest, JsonRpcResponse, Transport};

pub struct IpcTransport {
    path: PathBuf,
}

impl IpcTransport {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf() }
    }
}

#[async_trait]
impl Transport for IpcTransport {
    async fn send(&self, req: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
        let mut stream = UnixStream::connect(&self.path).await?;
        let req_bytes = serde_json::to_vec(req)?;
        stream.write_all(&req_bytes).await?;
        let mut resp_bytes = Vec::new();
        stream.read_to_end(&mut resp_bytes).await?;
        Ok(serde_json::from_slice(&resp_bytes)?)
    }
}
