// TODO(IPC transport improvements)
// - Use a robust framed protocol instead of ad‑hoc newline framing. Prefer
//   tokio_util::codec::LinesCodec for LF‑delimited servers (reth), and consider a length‑delimited
//   codec fallback for servers that do not emit trailing newlines.
// - Add connection reuse/pooling with backoff and jittered retries. Today we open a fresh
//   UnixStream per request; pooling lowers latency and reduces socket churn. Validate response ids
//   when multiplexing and guard against head‑of‑line blocking.
// - Make timeouts configurable (env/CLI/config). Different ELs and workloads can need longer
//   request timeouts; a static 10s may be too short/long.
// - Improve error mapping: surface clear diagnostics for ENOENT (socket missing), ECONNREFUSED (EL
//   not ready), ETIMEDOUT, and JSON decode errors with truncated frames.
// - Add metrics: request latency histogram, error counters by class, connect failures, pool stats.
//   Emit tracing spans and downgrade raw request/response byte logs to debug/trace to avoid noisy
//   logs and large payloads at info level.
// - Cross‑platform support: add Windows named pipes (\\.\pipe\...) via a platform abstraction to
//   broaden compatibility beyond Unix sockets.
// - Support batch requests and notifications where useful, and generate unique ids per request
//   (monotonic or random) to be future‑proof for pooled connections.
// - Enforce sane limits: maximum frame size and read budget to avoid memory blowups on malformed or
//   unexpectedly large responses.
// - Health‑checks: optional lightweight call (e.g., engine_exchangeCapabilities) to validate
//   connectivity and capability negotiation upon startup.
// - Testing: add unit tests with a mock Unix socket server and integration tests against a local EL
//   to validate framing, timeouts, and error paths.
#![allow(missing_docs)]
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use color_eyre::eyre;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tracing::info;

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
        info!("Connecting to IPC socket at {:?}", &self.path);
        let stream_future = UnixStream::connect(&self.path);
        let stream = tokio::time::timeout(REQUEST_TIMEOUT, stream_future).await??;
        info!("Successfully connected to IPC socket");
        Ok(stream)
    }
}

#[async_trait]
impl Transport for IpcTransport {
    async fn send(&self, req: &JsonRpcRequest) -> eyre::Result<JsonRpcResponse> {
        info!("Sending IPC request");
        // Establish a fresh connection per request and half-close after write so the server
        // can terminate its side, allowing us to read a single full JSON response to EOF.
        let mut stream = self.connect().await?;
        info!("IPC stream connected");

        // Quick fix: many Engine API IPC servers (e.g., reth) expect newline-delimited
        // JSON-RPC frames and keep the connection open for multiple requests. Without
        // a trailing newline the server may never parse the request, and reading to EOF
        // can block forever since EOF is not sent per response.
        //
        // TODO: Replace with a robust framed transport (e.g., tokio_util::codec::LinesCodec
        // or a length-delimited codec) and optional connection pooling to support
        // multiple requests per connection.
        let mut req_bytes = serde_json::to_vec(req)?;
        req_bytes.push(b'\n');
        info!("Request bytes: {}", String::from_utf8_lossy(&req_bytes));
        tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&req_bytes)).await??;
        tokio::time::timeout(REQUEST_TIMEOUT, stream.flush()).await??;
        info!("Request bytes sent");

        // Read a single newline-delimited JSON response frame.
        let mut reader = BufReader::new(stream);
        let mut resp_bytes = Vec::new();
        tokio::time::timeout(REQUEST_TIMEOUT, reader.read_until(b'\n', &mut resp_bytes)).await??;
        info!("Response bytes received: {}", String::from_utf8_lossy(&resp_bytes));

        serde_json::from_slice(&resp_bytes).map_err(|e| e.into())
    }
}
