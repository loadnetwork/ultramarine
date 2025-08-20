// crates/execution/src/transport/ipc.rs

// This file will contain the IPC transport implementation.

// --- REFERENCE IMPLEMENTATION ---
// use super::{JsonRpcRequest, JsonRpcResponse, Transport};
// use async_trait::async_trait;
// use color_eyre::eyre::{self, eyre};
// use std::path::{Path, PathBuf};
// use tokio::io::{AsyncReadExt, AsyncWriteExt};
// use tokio::net::UnixStream;
//
// pub struct IpcTransport {
// path: PathBuf,
// }
//
// impl IpcTransport {
// pub fn new(path: impl AsRef<Path>) -> Self {
// Self {
// path: path.as_ref().to_path_buf(),
// }
// }
// }
//
// #[async_trait]
// impl Transport for IpcTransport {
// async fn send(&self, request: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
// let mut stream = UnixStream::connect(&self.path).await.map_err(|e| eyre!(e))?;
// let request_bytes = serde_json::to_vec(request).map_err(|e| eyre!(e))?;
// stream.write_all(&request_bytes).await.map_err(|e| eyre!(e))?;
// let mut response_bytes = Vec::new();
// stream.read_to_end(&mut response_bytes).await.map_err(|e| eyre!(e))?;
// serde_json::from_slice(&response_bytes).map_err(|e| eyre!(e))
// }
// }
