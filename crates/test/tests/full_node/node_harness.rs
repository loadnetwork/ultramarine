//! Tier 1 full-node harness.
//!
//! This harness boots real Ultramarine validators (Malachite channel service,
//! WAL, libp2p transport) and drives blobbed proposals end-to-end using a tiny
//! Engine API stub. Unlike the Tier 0 tests, nothing calls `State` APIs
//! directly—the nodes run exactly the same application loop as production.

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    future::Future,
    net::{SocketAddr, TcpListener as StdTcpListener},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, Once},
    time::Duration,
};

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var_os("ULTRAMARINE_FULL_NODE_DEBUG").is_some() {
            eprintln!("[full_node] {}", format!($($arg)*));
        }
    };
}

fn record_event(log: &Arc<Mutex<Vec<String>>>, entry: String) {
    let mut guard = log.lock().unwrap();
    if guard.len() >= 10 {
        guard.remove(0);
    }
    guard.push(entry);
}

use alloy_consensus::Blob as AlloyBlob;
use alloy_eips::eip4844::Bytes48;
use alloy_primitives::{B256, U256, hex};
use alloy_rpc_types_engine::{
    BlobsBundleV1, ExecutionPayloadEnvelopeV3, ExecutionPayloadV3, ForkchoiceState,
    PayloadAttributes,
};
use color_eyre::{
    Result,
    eyre::{self, WrapErr},
};
use malachitebft_app::{
    engine::wal::log_entries,
    node::{Node, NodeHandle},
    wal::Log as WalLog,
};
use malachitebft_app_channel::app::events::RxEvent;
use malachitebft_config::{
    BootstrapProtocol, LoggingConfig, RuntimeConfig, Selector, TransportProtocol,
};
use malachitebft_engine::util::events::Event;
use multiaddr::Multiaddr;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serial_test::serial;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream},
    sync::Mutex as TokioMutex,
    task::JoinHandle,
    time::{sleep, timeout},
};
use ultramarine_blob_engine::{
    BlobEngine, BlobEngineImpl, metrics::BlobEngineMetrics, store::rocksdb::RocksDbBlobStore,
};
use ultramarine_cli::{config::Config, new::generate_config};
use ultramarine_consensus::{metrics::DbMetrics, store::Store};
use ultramarine_node::node::{App, Handle};
use ultramarine_types::{
    blob::{BYTES_PER_BLOB, BlobsBundle},
    codec::proto::ProtobufCodec,
    context::LoadContext,
    engine_api::{ExecutionBlock, JsonExecutionPayloadV3},
    height::Height,
};

#[path = "../common/mod.rs"]
mod blob_common;

use blob_common::{make_genesis, sample_blob_bundle, sample_execution_payload_v3_for_height};
use tracing_subscriber::EnvFilter;

type NodeConfigHook = Arc<dyn Fn(usize, &mut Config) + Send + Sync>;

#[derive(Clone)]
struct HarnessConfig {
    node_count: usize,
    start_height: Option<Height>,
    node_config_hook: Option<NodeConfigHook>,
}

impl HarnessConfig {
    fn apply_node_config(&self, index: usize, config: &mut Config) {
        if let Some(hook) = &self.node_config_hook {
            (hook)(index, config);
        }
    }
}

type ScenarioFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

struct FullNodeTestBuilder {
    node_count: usize,
    start_height: Option<Height>,
    node_config_hook: Option<NodeConfigHook>,
}

impl Default for FullNodeTestBuilder {
    fn default() -> Self {
        Self { node_count: 3, start_height: Some(Height::new(1)), node_config_hook: None }
    }
}

impl FullNodeTestBuilder {
    fn new() -> Self {
        Self::default()
    }

    fn node_count(mut self, count: usize) -> Self {
        self.node_count = count;
        self
    }

    fn start_height(mut self, height: Option<Height>) -> Self {
        self.start_height = height;
        self
    }

    fn with_node_config<F>(mut self, hook: F) -> Self
    where
        F: Fn(usize, &mut Config) + Send + Sync + 'static,
    {
        self.node_config_hook = Some(Arc::new(hook));
        self
    }

    async fn run<F>(self, scenario: F) -> Result<()>
    where
        F: for<'network> FnOnce(&'network mut NetworkHarness) -> ScenarioFuture<'network>,
    {
        let config = HarnessConfig {
            node_count: self.node_count,
            start_height: self.start_height,
            node_config_hook: self.node_config_hook.clone(),
        };

        let mut network = NetworkHarness::start(&config).await?;
        let scenario_result = scenario(&mut network).await;
        let shutdown_result = network.shutdown().await;

        match scenario_result {
            Ok(_) => shutdown_result,
            Err(err) => {
                shutdown_result?;
                Err(err)
            }
        }
    }
}

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn install_color_eyre_once() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let _ = color_eyre::install();
        let filter = build_env_filter();
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    });
}

fn build_env_filter() -> EnvFilter {
    let mut filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    if std::env::var_os("ULTRAMARINE_FULL_NODE_KEEP_P2P_ERRORS").is_some() {
        return filter;
    }

    if let Ok(directive) =
        "informalsystems_malachitebft_network=off".parse::<tracing_subscriber::filter::Directive>()
    {
        filter = filter.add_directive(directive);
    }

    filter
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_blob_quorum_roundtrip() -> Result<()> {
    install_color_eyre_once();
    FullNodeTestBuilder::new()
        .node_count(3)
        .run(|network| {
            Box::pin(async move {
                tokio::try_join!(
                    network.wait_for_height(0, Height::new(2)),
                    network.wait_for_height(1, Height::new(2)),
                    network.wait_for_height(2, Height::new(2))
                )?;
                sleep(Duration::from_millis(200)).await;
                network.assert_blobs(0, Height::new(1), 1).await?;
                network.assert_blobs(1, Height::new(1), 1).await?;
                network.assert_blobs(2, Height::new(1), 1).await?;
                network.assert_blobs(0, Height::new(2), 1).await?;
                network.assert_blobs(1, Height::new(2), 1).await?;
                network.assert_blobs(2, Height::new(2), 1).await?;
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_validator_restart_recovers() -> Result<()> {
    install_color_eyre_once();
    FullNodeTestBuilder::new()
        .node_count(3)
        .run(|network| {
            Box::pin(async move {
                network.wait_all(Height::new(1)).await?;
                sleep(Duration::from_millis(200)).await;

                network.restart_node(0).await?;
                network.wait_for_height(1, Height::new(2)).await?;
                network.wait_for_height(2, Height::new(2)).await?;
                network.wait_for_height(0, Height::new(2)).await?;

                // Verify blob metadata for both heights survived the restart
                network.assert_blobs(0, Height::new(1), 1).await?;
                network.assert_blobs(1, Height::new(1), 1).await?;
                network.assert_blobs(2, Height::new(1), 1).await?;
                network.assert_blobs(0, Height::new(2), 1).await?;
                network.assert_blobs(1, Height::new(2), 1).await?;
                network.assert_blobs(2, Height::new(2), 1).await?;
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_restart_mid_height() -> Result<()> {
    install_color_eyre_once();
    FullNodeTestBuilder::new()
        .node_count(3)
        .run(|network| {
            Box::pin(async move {
                // Drive height 1 to completion.
                network.wait_all(Height::new(1)).await?;

                // Let proposer for height 2 start streaming, then restart node 1 mid-round.
                network.wait_for_height(0, Height::new(2)).await?;
                sleep(Duration::from_millis(100)).await;
                network.restart_node(1).await?;

                // All nodes should eventually reach height 3.
                network.wait_for_height(0, Height::new(3)).await?;
                network.wait_for_height(1, Height::new(3)).await?;
                network.wait_for_height(2, Height::new(3)).await?;

                for node in 0..3 {
                    network.assert_blobs(node, Height::new(1), 1).await?;
                    network.assert_blobs(node, Height::new(2), 1).await?;
                    network.assert_blobs(node, Height::new(3), 1).await?;
                }

                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_new_node_sync() -> Result<()> {
    install_color_eyre_once();
    FullNodeTestBuilder::new()
        .node_count(4)
        .run(|network| {
            Box::pin(async move {
                // Take validator 3 offline before height 1 so it misses the first two heights.
                network.stop_node(3).await?;

                tokio::try_join!(
                    network.wait_for_height(0, Height::new(2)),
                    network.wait_for_height(1, Height::new(2)),
                    network.wait_for_height(2, Height::new(2))
                )?;

                // Bring the node back online and ensure ValueSync catches it up.
                network.start_node(3).await?;
                network.wait_for_height(3, Height::new(2)).await?;

                network.assert_blobs(3, Height::new(1), 1).await?;
                network.assert_blobs(3, Height::new(2), 1).await?;
                Ok(())
            })
        })
        .await
}

/// Representation of a node running inside the multi-node harness.
struct NodeProcess {
    handle: Option<Handle>,
    home: TempDir,
    engine_stub: Option<EngineRpcStub>,
    event_rx: TokioMutex<RxEvent<LoadContext>>,
    config: Config,
    genesis_path: PathBuf,
    key_path: PathBuf,
    jwt_path: PathBuf,
    start_height: Option<Height>,
    running: bool,
}

impl NodeProcess {
    async fn start(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        let new_stub = EngineRpcStub::start().await?;
        let app = App {
            config: self.config.clone(),
            home_dir: self.home.path().to_path_buf(),
            genesis_file: self.genesis_path.clone(),
            private_key_file: self.key_path.clone(),
            start_height: self.start_height,
            engine_http_url: Some(new_stub.url()),
            engine_ipc_path: None,
            eth1_rpc_url: Some(new_stub.url()),
            jwt_path: Some(self.jwt_path.clone()),
        };
        let handle = app.start().await?;
        self.event_rx = TokioMutex::new(handle.tx_event.subscribe());
        self.handle = Some(handle);
        self.engine_stub = Some(new_stub);
        self.running = true;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if !self.running {
            return Ok(());
        }
        if let Some(handle) = self.handle.take() {
            handle.kill(None).await?;
        }
        sleep(Duration::from_millis(200)).await;
        if let Some(stub) = self.engine_stub.take() {
            stub.shutdown().await;
        }
        self.running = false;
        Ok(())
    }

    async fn restart(&mut self) -> Result<()> {
        self.stop().await?;
        self.start().await
    }
}

/// Harness that manages multiple Ultramarine nodes connected over real libp2p transport.
struct NetworkHarness {
    nodes: Vec<NodeProcess>,
    is_shutdown: bool,
}

impl NetworkHarness {
    async fn start(config: &HarnessConfig) -> Result<Self> {
        eyre::ensure!(config.node_count > 0, "at least one node required");
        let (genesis, validator_keys) = make_genesis(config.node_count);
        let addresses: Vec<NodeAddrs> = (0..config.node_count).map(|_| NodeAddrs::new()).collect();
        let mut nodes = Vec::with_capacity(config.node_count);

        for index in 0..config.node_count {
            let engine_stub = EngineRpcStub::start().await?;
            let home = TempDir::new().wrap_err("create node tempdir")?;
            let validator = &validator_keys[index];
            let genesis_path = write_json(home.path().join("genesis.json"), &genesis)?;
            let key_path =
                write_json(home.path().join("validator_key.json"), &validator.private_key())?;
            let jwt_path = write_jwt(home.path().join("jwt.hex"))?;

            let mut node_config = generate_config(
                index,
                config.node_count,
                RuntimeConfig::default(),
                false,
                BootstrapProtocol::Kademlia,
                Selector::default(),
                0,
                0,
                1_000,
                TransportProtocol::Tcp,
                LoggingConfig::default(),
            );
            node_config.metrics.enabled = false;

            // Tune ValueSync for faster catch-up during restarts
            node_config.sync.status_update_interval = Duration::from_secs(1);
            node_config.sync.request_timeout = Duration::from_secs(5);
            node_config.sync.parallel_requests = 3;

            node_config.consensus.p2p.listen_addr = addresses[index].consensus.clone();
            node_config.consensus.p2p.persistent_peers =
                peer_multiaddrs(&addresses, index, |addr| &addr.consensus);
            node_config.consensus.p2p.discovery.enabled = false;
            node_config.mempool.p2p.listen_addr = addresses[index].mempool.clone();
            node_config.mempool.p2p.persistent_peers =
                peer_multiaddrs(&addresses, index, |addr| &addr.mempool);
            node_config.mempool.p2p.discovery.enabled = false;

            config.apply_node_config(index, &mut node_config);

            let stored_config = node_config.clone();

            let stored_genesis = genesis_path.clone();
            let stored_key = key_path.clone();
            let stored_jwt = jwt_path.clone();

            // Let the node handle genesis initialization naturally through App::start()
            // which calls State::hydrate_blob_parent_root() to seed genesis metadata if needed
            let app = App {
                config: node_config,
                home_dir: home.path().to_path_buf(),
                genesis_file: genesis_path,
                private_key_file: key_path,
                start_height: config.start_height,
                engine_http_url: Some(engine_stub.url()),
                engine_ipc_path: None,
                eth1_rpc_url: Some(engine_stub.url()),
                jwt_path: Some(jwt_path),
            };

            let handle = app.start().await?;
            let event_rx = TokioMutex::new(handle.tx_event.subscribe());
            nodes.push(NodeProcess {
                handle: Some(handle),
                home,
                engine_stub: Some(engine_stub),
                event_rx,
                config: stored_config,
                genesis_path: stored_genesis,
                key_path: stored_key,
                jwt_path: stored_jwt,
                start_height: config.start_height,
                running: true,
            });
        }

        // Give nodes time to establish p2p connections via persistent peers
        // before consensus begins. Without this, late-starting nodes miss proposals.
        tokio::time::sleep(Duration::from_millis(500)).await;

        Ok(Self { nodes, is_shutdown: false })
    }

    async fn wait_for_height(&self, node_index: usize, target_height: Height) -> Result<()> {
        let node =
            self.nodes.get(node_index).ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        let mut rx = node.event_rx.lock().await;
        let events_log = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&events_log);
        debug_log!("node {} waiting for height {}", node_index, target_height);
        let wait = async move {
            loop {
                match rx.recv().await {
                    Ok(Event::Decided(cert)) if cert.height == target_height => {
                        debug_log!("node {} decided height {}", node_index, cert.height);
                        break Ok(());
                    }
                    Ok(event) => {
                        debug_log!("node {} event {}", node_index, event);
                        record_event(&log_clone, format!("{event}"));
                    }
                    Err(e) => break Err(eyre::eyre!("event channel closed: {e}")),
                }
            }
        };
        match timeout(TEST_TIMEOUT, wait).await {
            Ok(res) => res,
            Err(_) => {
                let snapshot = {
                    let log = events_log.lock().unwrap();
                    log.join(" | ")
                };
                dump_node_state(node_index, node).await;
                Err(eyre::eyre!(
                    "node {node_index} timed out waiting for height {target_height}; last events: {}",
                    snapshot
                ))
            }
        }
    }

    async fn wait_all(&self, height: Height) -> Result<()> {
        for idx in 0..self.nodes.len() {
            self.wait_for_height(idx, height).await?;
        }
        Ok(())
    }

    async fn restart_node(&mut self, node_index: usize) -> Result<()> {
        let node = self
            .nodes
            .get_mut(node_index)
            .ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        node.restart().await
    }

    async fn stop_node(&mut self, node_index: usize) -> Result<()> {
        let node = self
            .nodes
            .get_mut(node_index)
            .ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        node.stop().await
    }

    async fn start_node(&mut self, node_index: usize) -> Result<()> {
        let node = self
            .nodes
            .get_mut(node_index)
            .ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        node.start().await
    }

    async fn shutdown(&mut self) -> Result<()> {
        if self.is_shutdown {
            return Ok(());
        }
        for node in &mut self.nodes {
            node.stop().await?;
        }
        self.is_shutdown = true;
        Ok(())
    }

    async fn assert_blobs(
        &mut self,
        node_index: usize,
        height: Height,
        expected: usize,
    ) -> Result<()> {
        if !self.is_shutdown {
            self.shutdown().await?;
            // Give RocksDB time to release file locks before reopening for inspection.
            sleep(Duration::from_millis(100)).await;
        }
        let node =
            self.nodes.get(node_index).ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        let blob_store = RocksDbBlobStore::open(node.home.path().join("blob_store.db"))
            .wrap_err("open blob store")?;
        let metrics = BlobEngineMetrics::new();
        let engine = BlobEngineImpl::new(blob_store, metrics)?;
        let blobs = engine.get_for_import(height).await?;
        assert_eq!(
            blobs.len(),
            expected,
            "expected {expected} blobs at height {height}, found {}",
            blobs.len()
        );
        Ok(())
    }
}

#[derive(Clone)]
struct NodeAddrs {
    consensus: Multiaddr,
    mempool: Multiaddr,
}

impl NodeAddrs {
    fn new() -> Self {
        Self { consensus: local_multiaddr(), mempool: local_multiaddr() }
    }
}

fn peer_multiaddrs<'a, F>(addrs: &'a [NodeAddrs], index: usize, f: F) -> Vec<Multiaddr>
where
    F: Fn(&'a NodeAddrs) -> &'a Multiaddr,
{
    addrs.iter().enumerate().filter(|(i, _)| *i != index).map(|(_, addr)| f(addr).clone()).collect()
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<PathBuf> {
    serde_json::to_writer(File::create(&path)?, value)?;
    Ok(path)
}

fn write_jwt(path: PathBuf) -> Result<PathBuf> {
    std::fs::write(&path, [0u8; 32])?;
    Ok(path)
}

fn local_multiaddr() -> Multiaddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind temp port");
    let port = listener.local_addr().unwrap().port() as usize;
    drop(listener);
    TransportProtocol::Tcp.multiaddr("127.0.0.1", port)
}

/// Minimal Engine + Eth RPC stub that satisfies the calls Ultramarine makes during tests.
struct EngineRpcStub {
    addr: SocketAddr,
    state: Arc<TokioMutex<StubState>>,
    handle: JoinHandle<()>,
}

impl EngineRpcStub {
    async fn start() -> Result<Self> {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.wrap_err("bind engine stub")?;
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(TokioMutex::new(StubState::default()));
        let accept_state = Arc::clone(&state);
        debug_log!("engine stub listening on {}", addr);

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&accept_state);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, state).await {
                                tracing::error!(%e, "engine stub connection error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(%e, "engine stub listener error");
                        break;
                    }
                }
            }
        });

        Ok(Self { addr, state, handle })
    }

    fn url(&self) -> url::Url {
        url::Url::parse(&format!("http://{}", self.addr)).expect("valid stub url")
    }

    async fn shutdown(&self) {
        self.handle.abort();
    }
}

struct StubState {
    latest_block: ExecutionBlock,
    next_payload_id: u64,
    pending: HashMap<[u8; 8], (ExecutionPayloadV3, Option<BlobsBundle>)>,
}

impl Default for StubState {
    fn default() -> Self {
        Self {
            latest_block: default_execution_block(),
            next_payload_id: 0,
            pending: HashMap::new(),
        }
    }
}

fn default_execution_block() -> ExecutionBlock {
    ExecutionBlock {
        block_hash: B256::ZERO,
        block_number: 0,
        parent_hash: B256::ZERO,
        timestamp: 0,
        prev_randao: B256::from([1u8; 32]),
    }
}

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    id: Value,
}

async fn handle_connection(
    mut stream: TokioTcpStream,
    state: Arc<TokioMutex<StubState>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(eyre::eyre!("unexpected EOF before headers"));
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buffer) {
            let content_length = parse_content_length(&buffer[..pos])?;
            let mut body = buffer[pos..].to_vec();
            while body.len() < content_length {
                let mut more = vec![0u8; content_length - body.len()];
                let n = stream.read(&mut more).await?;
                if n == 0 {
                    return Err(eyre::eyre!("unexpected EOF reading body"));
                }
                body.extend_from_slice(&more[..n]);
            }
            let body = body.into_iter().take(content_length).collect::<Vec<_>>();
            let rpc: RpcRequest = serde_json::from_slice(&body)?;
            let reply = match handle_rpc(&rpc, state).await {
                Ok(result) => build_success(&rpc.id, result),
                Err(e) => build_error(&rpc.id, -32000, e.to_string()),
            };
            let payload = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await?;
            return Ok(());
        }
        if buffer.len() > 64 * 1024 {
            return Err(eyre::eyre!("request too large"));
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n").map(|idx| idx + 4)
}

fn parse_content_length(headers: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(headers)?;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            let len = value.trim().parse::<usize>()?;
            return Ok(len);
        }
    }
    Err(eyre::eyre!("missing Content-Length header"))
}

async fn handle_rpc(req: &RpcRequest, state: Arc<TokioMutex<StubState>>) -> Result<Value> {
    debug_log!("rpc {}", req.method);
    match req.method.as_str() {
        "engine_exchangeCapabilities" => {
            Ok(json!(["engine_newPayloadV3", "engine_forkchoiceUpdatedV3", "engine_getPayloadV3"]))
        }
        "engine_forkchoiceUpdatedV3" => handle_forkchoice(req, state).await,
        "engine_getPayloadV3" => handle_get_payload(req, state).await,
        "engine_newPayloadV3" => handle_new_payload(req, state).await,
        "eth_getBlockByNumber" => handle_get_block(state).await,
        "eth_blockNumber" => {
            let state = state.lock().await;
            Ok(json!(format_hex_u64(state.latest_block.block_number)))
        }
        "eth_chainId" => Ok(json!("0x1")),
        "eth_syncing" => Ok(json!(false)),
        "txpool_status" => Ok(json!({"pending": "0x0", "queued": "0x0"})),
        "txpool_inspect" => Ok(json!({})),
        other => Err(eyre::eyre!("unsupported RPC method {other}")),
    }
}

async fn handle_forkchoice(req: &RpcRequest, state: Arc<TokioMutex<StubState>>) -> Result<Value> {
    let params = expect_params_array(&req.params)?;
    let (forkchoice, payload_attrs): (ForkchoiceState, Option<PayloadAttributes>) =
        serde_json::from_value(Value::Array(params)).wrap_err("parse forkchoice params")?;

    let mut guard = state.lock().await;

    // Update latest block from forkchoice head if we don't have it yet
    // This allows the stub to track the correct height even after restart
    if guard.latest_block.block_number == 0 && forkchoice.head_block_hash != B256::ZERO {
        // Non-zero head hash means we're not at genesis - infer block number from timestamp
        debug_log!("engine_forkchoiceUpdatedV3: updating latest_block from forkchoice head");
    }

    if let Some(_attrs) = payload_attrs {
        // Generate payload for next height (latest + 1)
        let next_height = Height::new(guard.latest_block.block_number + 1);

        debug_log!("engine_forkchoiceUpdatedV3: generating payload for height {}", next_height);

        let raw_bundle = sample_blob_bundle(1);
        let payload = sample_execution_payload_v3_for_height(next_height, Some(&raw_bundle));
        let bundle = Some(raw_bundle);
        let payload_id = guard.next_payload_id;
        guard.next_payload_id += 1;
        guard.pending.insert(payload_id.to_be_bytes(), (payload.clone(), bundle.clone()));

        Ok(json!({
            "payloadStatus": {
                "status": "VALID",
                "latestValidHash": format_b256(forkchoice.head_block_hash),
                "validationError": Value::Null
            },
            "payloadId": format_payload_id(payload_id),
        }))
    } else {
        Ok(json!({
            "payloadStatus": {
                "status": "VALID",
                "latestValidHash": format_b256(forkchoice.head_block_hash),
                "validationError": Value::Null
            },
            "payloadId": Value::Null
        }))
    }
}

async fn handle_get_payload(req: &RpcRequest, state: Arc<TokioMutex<StubState>>) -> Result<Value> {
    let params = expect_params_array(&req.params)?;
    let payload_id_hex =
        params.get(0).and_then(Value::as_str).ok_or_else(|| eyre::eyre!("missing payload id"))?;
    let id_bytes = parse_payload_id(payload_id_hex)?;

    let mut guard = state.lock().await;
    let (payload, bundle) =
        guard.pending.remove(&id_bytes).ok_or_else(|| eyre::eyre!("unknown payload id"))?;

    let envelope = ExecutionPayloadEnvelopeV3 {
        execution_payload: payload,
        block_value: U256::ZERO,
        blobs_bundle: convert_bundle(bundle.as_ref()),
        should_override_builder: false,
    };

    Ok(serde_json::to_value(&envelope)?)
}

async fn handle_new_payload(req: &RpcRequest, state: Arc<TokioMutex<StubState>>) -> Result<Value> {
    let params = expect_params_array(&req.params)?;
    let payload: JsonExecutionPayloadV3 =
        serde_json::from_value(params[0].clone()).wrap_err("decode payload")?;

    let mut guard = state.lock().await;
    guard.latest_block = ExecutionBlock {
        block_hash: payload.block_hash,
        block_number: payload.block_number,
        parent_hash: payload.parent_hash,
        timestamp: payload.timestamp,
        prev_randao: payload.prev_randao,
    };

    Ok(json!({
        "status": "VALID",
        "latestValidHash": format_b256(payload.block_hash),
        "validationError": Value::Null
    }))
}

async fn handle_get_block(state: Arc<TokioMutex<StubState>>) -> Result<Value> {
    let guard = state.lock().await;
    let block = &guard.latest_block;
    Ok(json!({
        "number": format_hex_u64(block.block_number),
        "hash": format_b256(block.block_hash),
        "parentHash": format_b256(block.parent_hash),
        "mixHash": format_b256(block.prev_randao),
        "timestamp": format_hex_u64(block.timestamp),
        "nonce": format_zero_bytes(8),
        "sha3Uncles": format_b256(B256::ZERO),
        "logsBloom": format_zero_bytes(256),
        "transactionsRoot": format_b256(B256::ZERO),
        "stateRoot": format_b256(B256::ZERO),
        "receiptsRoot": format_b256(B256::ZERO),
        "miner": zero_address(),
        "difficulty": "0x0",
        "totalDifficulty": "0x0",
        "extraData": "0x",
        "size": "0x0",
        "gasLimit": "0x0",
        "gasUsed": "0x0",
        "transactions": [],
        "uncles": [],
        "baseFeePerGas": "0x0",
        "withdrawals": [],
        "withdrawalsRoot": format_b256(B256::ZERO),
    }))
}

fn expect_params_array(params: &Value) -> Result<Vec<Value>> {
    match params {
        Value::Array(values) => Ok(values.clone()),
        Value::Null => Ok(Vec::new()),
        _ => Err(eyre::eyre!("params must be array")),
    }
}

fn parse_payload_id(value: &str) -> Result<[u8; 8]> {
    let bytes = hex::decode(value.trim_start_matches("0x"))?;
    Ok(bytes.as_slice().try_into().map_err(|_| eyre::eyre!("payload id must be 8 bytes"))?)
}

fn format_payload_id(id: u64) -> String {
    format!("0x{:016x}", id)
}

fn format_b256(value: B256) -> String {
    format!("{value:#066x}")
}

fn format_hex_u64(value: u64) -> String {
    format!("0x{:x}", value)
}

fn format_zero_bytes(bytes: usize) -> String {
    format!("0x{}", "00".repeat(bytes))
}

fn zero_address() -> String {
    format!("0x{:040}", 0)
}

fn convert_bundle(bundle: Option<&BlobsBundle>) -> BlobsBundleV1 {
    match bundle {
        Some(bundle) => {
            let commitments = bundle.commitments.iter().map(|c| Bytes48::from(c.0)).collect();
            let proofs = bundle.proofs.iter().map(|p| Bytes48::from(p.0)).collect();
            let blobs = bundle
                .blobs
                .iter()
                .map(|blob| {
                    let mut data = [0u8; BYTES_PER_BLOB];
                    data.copy_from_slice(blob.data());
                    AlloyBlob::from(data)
                })
                .collect();
            BlobsBundleV1 { commitments, proofs, blobs }
        }
        None => BlobsBundleV1 { commitments: vec![], proofs: vec![], blobs: vec![] },
    }
}

fn build_success(id: &Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    })
}

async fn dump_node_state(node_index: usize, node: &NodeProcess) {
    if let Err(e) = async {
        let store_snapshot = tempfile::tempdir().wrap_err("create store snapshot dir")?;
        let store_snapshot_path = store_snapshot.path().join("store.db");
        copy_dir_recursive(&node.home.path().join("store.db"), &store_snapshot_path)
            .wrap_err("snapshot store db")?;
        let store = Store::open(store_snapshot_path, DbMetrics::new())?;
        let latest_decided = store.max_decided_value_height().await;
        let undecided = store
            .get_all_undecided_blob_metadata_before(Height::new(u64::MAX))
            .await
            .unwrap_or_default();

        tracing::warn!(
            node = node_index,
            ?latest_decided,
            undecided_rounds = ?undecided,
            "Timeout diagnostics: store snapshot"
        );
        Ok::<(), color_eyre::Report>(())
    }
    .await
    {
        tracing::warn!(
            node = node_index,
            error = %e,
            "Timeout diagnostics: failed to capture store snapshot"
        );
    }

    let wal_path = node.home.path().join("wal/consensus.wal");
    if wal_path.exists() {
        match WalLog::open(&wal_path) {
            Ok(mut log) => match log_entries::<LoadContext, _>(&mut log, &ProtobufCodec) {
                Ok(entries) => {
                    let mut tail = VecDeque::new();
                    for (idx, entry) in entries.enumerate() {
                        let item = match entry {
                            Ok(e) => format!("#{idx}: {e:?}"),
                            Err(e) => format!("#{idx}: Err({e})"),
                        };
                        if tail.len() == 5 {
                            tail.pop_front();
                        }
                        tail.push_back(item);
                    }
                    tracing::warn!(
                        node = node_index,
                        wal_entries = tail.len(),
                        wal_tail = ?tail,
                        "Timeout diagnostics: WAL snapshot"
                    );
                }
                Err(e) => tracing::warn!(
                    node = node_index,
                    error = %e,
                    "Timeout diagnostics: failed to iterate WAL"
                ),
            },
            Err(e) => tracing::warn!(
                node = node_index,
                path = %wal_path.display(),
                error = %e,
                "Timeout diagnostics: failed to open WAL"
            ),
        }
    } else {
        tracing::warn!(
            node = node_index,
            path = %wal_path.display(),
            "Timeout diagnostics: WAL file missing"
        );
    }
}

fn build_error(id: &Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message },
        "id": id,
    })
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_file() {
        fs::create_dir_all(dst)?;
        let file_name = src.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "source file missing name")
        })?;
        fs::copy(src, dst.join(file_name))?;
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
