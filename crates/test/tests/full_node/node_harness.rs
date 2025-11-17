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
    io::IsTerminal,
    net::{SocketAddr, TcpListener as StdTcpListener},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, Once},
    time::{Duration, Instant},
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
use bytes::Bytes;
use color_eyre::{
    Result,
    eyre::{self, WrapErr},
};
use malachitebft_app::{
    engine::wal::log_entries,
    node::{Node, NodeHandle},
    wal::Log as WalLog,
};
use malachitebft_app_channel::app::{
    events::RxEvent,
    streaming::StreamMessage,
    types::{
        LocallyProposedValue,
        core::{CommitCertificate, Round},
    },
};
use malachitebft_config::{
    BootstrapProtocol, LoggingConfig, RuntimeConfig, Selector, TransportProtocol,
};
use malachitebft_engine::util::events::Event;
use multiaddr::Multiaddr;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serial_test::serial;
use ssz::Encode;
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
use ultramarine_consensus::{metrics::DbMetrics, state::State, store::Store};
use ultramarine_node::node::{App, Handle};
use ultramarine_types::{
    address::Address,
    blob::{BYTES_PER_BLOB, BlobsBundle, KzgCommitment},
    blob_metadata::BlobMetadata,
    codec::proto::ProtobufCodec,
    context::LoadContext,
    engine_api::{ExecutionBlock, ExecutionPayloadHeader, JsonExecutionPayloadV3},
    genesis::Genesis,
    height::Height,
    signing::{Ed25519Provider, PrivateKey},
    sync::SyncedValuePackage,
    value::Value as StateValue,
    value_metadata::ValueMetadata,
};

#[path = "../common/mod.rs"]
mod blob_common;

use blob_common::{
    make_genesis,
    mocks::{MockEngineApi, MockExecutionNotifier},
    payload_id, propose_with_optional_blobs, sample_blob_bundle,
    sample_execution_payload_v3_for_height, test_peer_id,
};
use tracing_subscriber::{EnvFilter, filter::Directive};
use ultramarine_execution::EngineApi;

type NodeConfigHook = Arc<dyn Fn(usize, &mut Config) + Send + Sync>;
type PayloadPlan = Arc<dyn Fn(Height) -> usize + Send + Sync>;

#[derive(Clone)]
struct HarnessConfig {
    node_count: usize,
    start_height: Option<Height>,
    node_config_hook: Option<NodeConfigHook>,
    payload_plan: Option<PayloadPlan>,
}

impl HarnessConfig {
    fn apply_node_config(&self, index: usize, config: &mut Config) {
        if let Some(hook) = &self.node_config_hook {
            (hook)(index, config);
        }
    }

    fn payload_plan(&self) -> Option<PayloadPlan> {
        self.payload_plan.clone()
    }
}

type ScenarioFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

struct FullNodeTestBuilder {
    node_count: usize,
    start_height: Option<Height>,
    node_config_hook: Option<NodeConfigHook>,
    payload_plan: Option<PayloadPlan>,
}

impl Default for FullNodeTestBuilder {
    fn default() -> Self {
        Self { node_count: 3, start_height: None, node_config_hook: None, payload_plan: None }
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

    #[allow(dead_code)]
    fn with_node_config<F>(mut self, hook: F) -> Self
    where
        F: Fn(usize, &mut Config) + Send + Sync + 'static,
    {
        self.node_config_hook = Some(Arc::new(hook));
        self
    }

    fn with_payload_plan<F>(mut self, plan: F) -> Self
    where
        F: Fn(Height) -> usize + Send + Sync + 'static,
    {
        self.payload_plan = Some(Arc::new(plan));
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
            payload_plan: self.payload_plan.clone(),
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

/// Installs color-eyre and the tracing subscriber used by integration tests.
///
/// Defaults:
/// - Logs are captured unless a test fails (`with_test_writer`).
/// - `ultramarine` modules log at `info`, everything else at `warn`.
/// - `ULTRAMARINE_FULL_NODE_KEEP_P2P_ERRORS=1` re-enables libp2p/malachite network logs.
/// - `RUST_LOG` overrides everything (e.g., `RUST_LOG=debug cargo test`).
fn init_test_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = color_eyre::install();
        let filter = build_test_filter();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .with_target(false)
            .with_thread_ids(false)
            .with_ansi(stream_supports_ansi());
        let _ = subscriber.try_init();
    });
}

fn build_test_filter() -> EnvFilter {
    if let Ok(value) = std::env::var("RUST_LOG") {
        return EnvFilter::try_new(value).unwrap_or_else(|_| default_test_filter());
    }

    default_test_filter()
}

fn default_test_filter() -> EnvFilter {
    let mut filter = EnvFilter::new("warn,ultramarine=info");

    if std::env::var_os("ULTRAMARINE_FULL_NODE_KEEP_P2P_ERRORS").is_none() {
        if let Ok(directive) = "informalsystems_malachitebft_network=off".parse::<Directive>() {
            filter = filter.add_directive(directive);
        }
    }

    if let Ok(directive) = "libp2p=warn".parse::<Directive>() {
        filter = filter.add_directive(directive);
    }

    filter
}

fn stream_supports_ansi() -> bool {
    std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_blob_quorum_roundtrip() -> Result<()> {
    init_test_logging();
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
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(4)
        .run(|network| {
            Box::pin(async move {
                // Stop node 0 IMMEDIATELY before it can participate in any consensus.
                // This ensures it has NO WAL entries for height 1 or 2, forcing pure ValueSync.
                // Eliminates the race condition where node 0 might partially enter height 2.
                network.stop_node(0).await?;

                // Let nodes 1-3 decide height 1 and advance to height 2 (quorum = 3/4)
                tokio::try_join!(
                    network.wait_for_height(1, Height::new(2)),
                    network.wait_for_height(2, Height::new(2)),
                    network.wait_for_height(3, Height::new(2))
                )?;

                // Bring node 0 back online - it MUST ValueSync heights 1 and 2 from peers.
                // This exercises the actual ValueSync cache update path in
                // process_synced_package (state.rs:844-856).
                network.start_node(0).await?;

                // Wait for all nodes to reach height 3.
                // For node 0 to reach height 3, it must have successfully ValueSync'd heights 1 and 2,
                // which means the blob parent root cache was properly updated.
                // If the cache wasn't updated, validation of height 3 proposals would fail.
                tokio::try_join!(
                    network.wait_for_height(0, Height::new(3)),
                    network.wait_for_height(1, Height::new(3)),
                    network.wait_for_height(2, Height::new(3)),
                    network.wait_for_height(3, Height::new(3))
                )?;

                // Verify blob metadata for all heights
                for node in 0..4 {
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
async fn full_node_restart_mid_height() -> Result<()> {
    init_test_logging();
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
                tokio::try_join!(
                    network.wait_for_height(0, Height::new(3)),
                    network.wait_for_height(1, Height::new(3)),
                    network.wait_for_height(2, Height::new(3))
                )?;

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
    init_test_logging();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_multi_height_valuesync_restart() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(4)
        .with_payload_plan(|height: Height| match height.as_u64() {
            1 => 1,
            2 => 0,
            3 => 2,
            _ => 1,
        })
        .run(|network| {
            Box::pin(async move {
                // Keep validator 3 offline so it misses the first three heights.
                network.stop_node(3).await?;
                tokio::try_join!(
                    network.wait_for_height(0, Height::new(3)),
                    network.wait_for_height(1, Height::new(3)),
                    network.wait_for_height(2, Height::new(3))
                )?;

                // Bring it back online and wait for ValueSync to replay the missing heights.
                network.start_node(3).await?;
                network.wait_for_height(3, Height::new(3)).await?;

                // Freeze the cluster before inspecting persistent state to avoid newer heights.
                network.shutdown().await?;

                // After the cluster shuts down, verify mixed blob metadata survived the restart.
                network.assert_blob_metadata(3, Height::new(1), 1).await?;
                network.assert_blob_metadata(3, Height::new(2), 0).await?;
                network.assert_blob_metadata(3, Height::new(3), 2).await?;
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_restart_multi_height_rebuilds() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let mut node_state = {
                    let node = network.node_ref(0)?;
                    open_state_ready(node).await?
                };

                let scenarios = [
                    (Height::new(0), Some(sample_blob_bundle(1))),
                    (Height::new(1), None),
                    (Height::new(2), Some(sample_blob_bundle(2))),
                ];

                for (height, maybe_bundle) in scenarios.into_iter() {
                    let payload =
                        sample_execution_payload_v3_for_height(height, maybe_bundle.as_ref());
                    let (proposed, bytes, sidecars) = propose_with_optional_blobs(
                        &mut node_state,
                        height,
                        Round::new(0),
                        &payload,
                        maybe_bundle.as_ref(),
                    )
                    .await?;

                    if let Some(ref bundle_sidecars) = sidecars {
                        node_state
                            .blob_engine()
                            .verify_and_store(height, 0, bundle_sidecars)
                            .await?;
                    }

                    node_state
                        .store_undecided_block_data(height, Round::new(0), bytes.clone())
                        .await?;

                    let certificate = CommitCertificate {
                        height,
                        round: Round::new(0),
                        value_id: proposed.value.id(),
                        commit_signatures: Vec::new(),
                    };
                    let mut notifier = MockExecutionNotifier::default();
                    node_state
                        .process_decided_certificate(&certificate, bytes, &mut notifier)
                        .await?;
                }

                drop(node_state);
                network.shutdown().await?;
                network.assert_rebuilds_sidecars(0, Height::new(0), 1).await?;
                network.assert_rebuilds_sidecars(0, Height::new(1), 0).await?;
                network.assert_rebuilds_sidecars(0, Height::new(2), 2).await?;
                network.assert_parent_root_matches(0, Height::new(2)).await?;
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_restream_multiple_rounds_cleanup() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(2)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                network.stop_node(1).await?;

                let (mut proposer, _) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };
                let (mut follower, follower_metrics) = {
                    let node = network.node_ref(1)?;
                    open_state_ready_with_metrics(node).await?
                };

                let height = Height::new(0);
                let rounds = [Round::new(0), Round::new(1)];
                let payload_ids = [payload_id(6), payload_id(7)];

                let raw_bundles = [sample_blob_bundle(1), sample_blob_bundle(1)];
                let raw_payloads = [
                    sample_execution_payload_v3_for_height(height, Some(&raw_bundles[0])),
                    sample_execution_payload_v3_for_height(height, Some(&raw_bundles[1])),
                ];

                let mut mock_engine = MockEngineApi::default();
                for ((payload_id, payload), bundle) in
                    payload_ids.iter().zip(raw_payloads.iter()).zip(raw_bundles.iter())
                {
                    mock_engine = mock_engine.with_payload(
                        *payload_id,
                        payload.clone(),
                        Some(bundle.clone()),
                    );
                }

                let mut total_success = 0usize;
                let mut dropped_count = 0usize;
                let mut promoted_count = 0usize;
                let mut winning_proposal = None;
                let mut winning_payload_bytes = None;

                for (idx, round) in rounds.iter().enumerate() {
                    let payload_id = payload_ids[idx];
                    let (payload, maybe_bundle) =
                        mock_engine.get_payload_with_blobs(payload_id).await?;
                    let bundle = maybe_bundle.expect("bundle should exist");
                    let (proposed, payload_bytes, maybe_sidecars) = propose_with_optional_blobs(
                        &mut proposer,
                        height,
                        *round,
                        &payload,
                        Some(&bundle),
                    )
                    .await?;
                    let sidecars = maybe_sidecars.expect("sidecars expected");

                    if !sidecars.is_empty() {
                        proposer
                            .blob_engine()
                            .verify_and_store(height, round.as_i64(), &sidecars)
                            .await?;
                    }
                    proposer
                        .store_undecided_block_data(height, *round, payload_bytes.clone())
                        .await?;

                    total_success += sidecars.len();
                    if round.as_u32() == Some(0) {
                        dropped_count = sidecars.len();
                    } else {
                        promoted_count = sidecars.len();
                    }

                    let stream_messages: Vec<StreamMessage<_>> = proposer
                        .stream_proposal(
                            proposed,
                            payload_bytes.clone(),
                            Some(sidecars.as_slice()),
                            None,
                        )
                        .collect();

                    let mut received = None;
                    for msg in stream_messages {
                        if let Some(value) = follower
                            .received_proposal_part(
                                test_peer_id(round.as_u32().expect("round") as u8 + 20),
                                msg,
                            )
                            .await?
                        {
                            received = Some(value);
                        }
                    }
                    let received = received.expect("follower should reconstruct proposal");
                    if round.as_u32() == Some(1) {
                        winning_proposal = Some(received);
                        winning_payload_bytes = Some(payload_bytes.clone());
                    }
                }

                let winning_round = rounds[1];
                let certificate = CommitCertificate {
                    height,
                    round: winning_round,
                    value_id: winning_proposal
                        .as_ref()
                        .expect("winning proposal stored")
                        .value
                        .id(),
                    commit_signatures: Vec::new(),
                };
                let payload_bytes = winning_payload_bytes.expect("winning payload bytes tracked");

                let follower_payload = follower
                    .get_block_data(height, winning_round)
                    .await
                    .unwrap_or_else(|| payload_bytes.clone());
                let mut follower_notifier = MockExecutionNotifier::default();
                let follower_outcome = follower
                    .process_decided_certificate(
                        &certificate,
                        follower_payload,
                        &mut follower_notifier,
                    )
                    .await?;

                let imported = follower.blob_engine().get_for_import(height).await?;

                let metrics = follower_metrics.snapshot();
                assert_eq!(metrics.verifications_success, total_success as u64);
                assert_eq!(metrics.lifecycle_promoted, promoted_count as u64);
                assert_eq!(metrics.lifecycle_dropped, dropped_count as u64);
                assert_eq!(metrics.storage_bytes_decided, (promoted_count * BYTES_PER_BLOB) as i64);
                assert_eq!(metrics.storage_bytes_undecided, 0);

                let undecided =
                    follower.blob_engine().get_undecided_blobs(height, rounds[0].as_i64()).await?;
                assert!(undecided.is_empty(), "losing round blobs should be dropped");
                assert_eq!(follower.current_height, Height::new(1));
                assert_eq!(follower_outcome.blob_count, promoted_count);
                assert!(
                    imported.len() >= follower_outcome.blob_count,
                    "expected at least {} decided blobs, found {}",
                    follower_outcome.blob_count,
                    imported.len()
                );
                assert_eq!(follower_notifier.new_block_calls.lock().unwrap().len(), 1);

                let proposer_payload = proposer
                    .get_block_data(height, winning_round)
                    .await
                    .unwrap_or_else(|| payload_bytes.clone());
                let mut proposer_notifier = MockExecutionNotifier::default();
                proposer
                    .process_decided_certificate(
                        &certificate,
                        proposer_payload,
                        &mut proposer_notifier,
                    )
                    .await?;
                assert_eq!(proposer.current_height, Height::new(1));
                assert_eq!(proposer_notifier.new_block_calls.lock().unwrap().len(), 1);

                let proposer_undecided =
                    proposer.blob_engine().get_undecided_blobs(height, rounds[0].as_i64()).await?;
                assert!(
                    proposer_undecided.is_empty(),
                    "proposer should drop losing round blobs during commit"
                );

                drop(proposer);
                drop(follower);
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_restream_multi_validator() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(2)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                network.stop_node(1).await?;

                let (mut proposer, _) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };
                let (mut follower, follower_metrics) = {
                    let node = network.node_ref(1)?;
                    open_state_ready_with_metrics(node).await?
                };

                let height = Height::new(0);
                let round = Round::new(0);
                let bundle = sample_blob_bundle(1);
                let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
                let payload_id = payload_id(3);
                let mock_engine = MockEngineApi::default().with_payload(
                    payload_id,
                    payload.clone(),
                    Some(bundle.clone()),
                );

                let (payload, maybe_bundle) =
                    mock_engine.get_payload_with_blobs(payload_id).await?;
                let bundle = maybe_bundle.expect("bundle");

                let (proposed, payload_bytes, maybe_sidecars) = propose_with_optional_blobs(
                    &mut proposer,
                    height,
                    round,
                    &payload,
                    Some(&bundle),
                )
                .await?;
                let sidecars = maybe_sidecars.expect("sidecars expected");
                if !sidecars.is_empty() {
                    proposer
                        .blob_engine()
                        .verify_and_store(height, round.as_i64(), &sidecars)
                        .await?;
                }
                proposer.store_undecided_block_data(height, round, payload_bytes.clone()).await?;

                let stream_messages: Vec<StreamMessage<_>> = proposer
                    .stream_proposal(proposed.clone(), payload_bytes.clone(), Some(&sidecars), None)
                    .collect();

                let mut received = None;
                for msg in stream_messages {
                    let peer_id = test_peer_id(1);
                    if let Some(value) = follower.received_proposal_part(peer_id, msg).await? {
                        received = Some(value);
                    }
                }
                let received = received.expect("follower should reconstruct proposal");

                let certificate = CommitCertificate {
                    height,
                    round,
                    value_id: received.value.id(),
                    commit_signatures: Vec::new(),
                };

                let follower_payload = follower
                    .get_block_data(height, round)
                    .await
                    .unwrap_or_else(|| payload_bytes.clone());
                let mut follower_notifier = MockExecutionNotifier::default();
                let follower_outcome = follower
                    .process_decided_certificate(
                        &certificate,
                        follower_payload,
                        &mut follower_notifier,
                    )
                    .await?;

                let imported = follower.blob_engine().get_for_import(height).await?;
                assert_eq!(imported.len(), sidecars.len());
                assert_eq!(follower.current_height, Height::new(1));
                assert_eq!(follower_outcome.blob_count, sidecars.len());
                assert_eq!(follower_notifier.new_block_calls.lock().unwrap().len(), 1);

                let metrics = follower_metrics.snapshot();
                assert_eq!(metrics.verifications_success, sidecars.len() as u64);
                assert_eq!(metrics.verifications_failure, 0);
                assert_eq!(metrics.lifecycle_promoted, sidecars.len() as u64);
                assert_eq!(metrics.storage_bytes_decided, (sidecars.len() * BYTES_PER_BLOB) as i64);

                let proposer_payload = proposer
                    .get_block_data(height, round)
                    .await
                    .unwrap_or_else(|| payload_bytes.clone());
                let mut proposer_notifier = MockExecutionNotifier::default();
                proposer
                    .process_decided_certificate(
                        &certificate,
                        proposer_payload,
                        &mut proposer_notifier,
                    )
                    .await?;
                assert_eq!(proposer.current_height, Height::new(1));
                assert_eq!(proposer_notifier.new_block_calls.lock().unwrap().len(), 1);

                drop(proposer);
                drop(follower);
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_value_sync_commitment_mismatch() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let validator_address = network.node_address(0)?;
                let (mut node_state, node_metrics) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };

                let height = Height::new(0);
                let round = Round::new(0);
                let bundle = sample_blob_bundle(1);
                let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));

                let (_proposed, payload_bytes, maybe_sidecars) = propose_with_optional_blobs(
                    &mut node_state,
                    height,
                    round,
                    &payload,
                    Some(&bundle),
                )
                .await?;
                let sidecars = maybe_sidecars.expect("sidecars expected");

                let mut fake_commitment_bytes = sidecars[0].kzg_commitment.0;
                fake_commitment_bytes[0] ^= 0xFF;
                let fake_commitment = KzgCommitment(fake_commitment_bytes);

                let header = ExecutionPayloadHeader::from_payload(&payload);
                let fake_metadata = ValueMetadata::new(header, vec![fake_commitment]);
                let fake_value = StateValue::new(fake_metadata);

                let package = SyncedValuePackage::Full {
                    value: fake_value,
                    execution_payload_ssz: payload_bytes.clone(),
                    blob_sidecars: sidecars.clone(),
                };

                let encoded = package.encode().map_err(|e| eyre::eyre!(e))?;
                let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| eyre::eyre!(e))?;

                let result = node_state
                    .process_synced_package(height, round, validator_address, decoded)
                    .await?;
                assert!(result.is_none(), "commitment mismatch should be rejected");

                let metrics = node_metrics.snapshot();
                assert_eq!(metrics.sync_failures, 1);
                assert_eq!(metrics.verifications_success, 1);

                let round_i64 = round.as_i64();
                let undecided =
                    node_state.blob_engine().get_undecided_blobs(height, round_i64).await?;
                assert!(
                    undecided.is_empty(),
                    "invalid blobs should be dropped after validation failure"
                );

                let metadata = node_state.load_blob_metadata_for_round(height, round).await?;
                assert!(
                    metadata.map_or(true, |m| m.blob_count() == 0),
                    "fake metadata should not be stored"
                );

                drop(node_state);
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_value_sync_inclusion_proof_failure() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let validator_address = network.node_address(0)?;
                let (mut node_state, node_metrics) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };

                let height = Height::new(0);
                let round = Round::new(0);
                let bundle = sample_blob_bundle(1);
                let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));

                let (_proposed, payload_bytes, maybe_sidecars) = propose_with_optional_blobs(
                    &mut node_state,
                    height,
                    round,
                    &payload,
                    Some(&bundle),
                )
                .await?;
                let mut sidecars = maybe_sidecars.expect("sidecars expected");
                sidecars[0].kzg_commitment_inclusion_proof.clear();

                let header = ExecutionPayloadHeader::from_payload(&payload);
                let metadata = ValueMetadata::new(header, bundle.commitments.clone());
                let value = StateValue::new(metadata);

                let package = SyncedValuePackage::Full {
                    value,
                    execution_payload_ssz: payload_bytes.clone(),
                    blob_sidecars: sidecars.clone(),
                };

                let encoded = package.encode().map_err(|e| eyre::eyre!(e))?;
                let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| eyre::eyre!(e))?;

                let result = node_state
                    .process_synced_package(height, round, validator_address, decoded)
                    .await?;
                assert!(result.is_none(), "inclusion proof failure should be rejected");

                let metrics = node_metrics.snapshot();
                assert_eq!(metrics.sync_failures, 1);
                assert_eq!(metrics.verifications_success, 1);

                let round_i64 = round.as_i64();
                let undecided =
                    node_state.blob_engine().get_undecided_blobs(height, round_i64).await?;
                assert!(
                    undecided.is_empty(),
                    "invalid blobs should be dropped after inclusion proof failure"
                );

                let decided = node_state.blob_engine().get_for_import(height).await?;
                assert!(
                    decided.is_empty(),
                    "no blobs should be promoted when inclusion proof verification fails"
                );

                drop(node_state);
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_blob_blobless_sequence_behaves() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let (mut state, metrics) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };

                commit_block_for_sequence(&mut state, Height::new(0), 1).await?;
                commit_block_for_sequence(&mut state, Height::new(1), 0).await?;
                commit_block_for_sequence(&mut state, Height::new(2), 2).await?;

                let metrics_snapshot = metrics.snapshot();
                assert_eq!(metrics_snapshot.lifecycle_promoted, 3);
                assert_eq!(metrics_snapshot.storage_bytes_decided, (3 * BYTES_PER_BLOB) as i64);

                let blobs_h0 = state.blob_engine().get_for_import(Height::new(0)).await?;
                assert_eq!(blobs_h0.len(), 1);
                let blobs_h1 = state.blob_engine().get_for_import(Height::new(1)).await?;
                assert!(blobs_h1.is_empty());
                let blobs_h2 = state.blob_engine().get_for_import(Height::new(2)).await?;
                assert_eq!(blobs_h2.len(), 2);
                assert_eq!(state.current_height, Height::new(3));
                drop(state);
                Ok(())
            })
        })
        .await
}

async fn commit_block_for_sequence(
    state: &mut State<BlobEngineImpl<RocksDbBlobStore>>,
    height: Height,
    blob_count: usize,
) -> Result<()> {
    let round = Round::new(0);
    let bundle = if blob_count == 0 { None } else { Some(sample_blob_bundle(blob_count)) };
    let payload = sample_execution_payload_v3_for_height(height, bundle.as_ref());
    let (proposed, bytes, maybe_sidecars) =
        propose_with_optional_blobs(state, height, round, &payload, bundle.as_ref()).await?;
    if let Some(sidecars) = maybe_sidecars.as_ref() {
        state.blob_engine().verify_and_store(height, round.as_i64(), sidecars).await?;
    }
    state.store_undecided_block_data(height, round, bytes.clone()).await?;
    let certificate = CommitCertificate {
        height,
        round,
        value_id: proposed.value.id(),
        commit_signatures: Vec::new(),
    };
    let mut notifier = MockExecutionNotifier::default();
    state.process_decided_certificate(&certificate, bytes, &mut notifier).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_blob_pruning_retains_recent_heights() -> Result<()> {
    init_test_logging();
    const TOTAL_HEIGHTS: usize = 8;
    const RETENTION: u64 = 5;

    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let (mut state, metrics) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };
                state.set_blob_retention_window_for_testing(RETENTION);

                for idx in 0..TOTAL_HEIGHTS {
                    let height = Height::new(idx as u64);
                    let round = Round::new(0);
                    let bundle = sample_blob_bundle(1);
                    let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
                    let (proposed, bytes, sidecars) = propose_with_optional_blobs(
                        &mut state,
                        height,
                        round,
                        &payload,
                        Some(&bundle),
                    )
                    .await?;
                    let sidecars = sidecars.expect("sidecars expected");
                    state.blob_engine().verify_and_store(height, round.as_i64(), &sidecars).await?;
                    state.store_undecided_block_data(height, round, bytes.clone()).await?;
                    let certificate = CommitCertificate {
                        height,
                        round,
                        value_id: proposed.value.id(),
                        commit_signatures: Vec::new(),
                    };
                    let mut notifier = MockExecutionNotifier::default();
                    state.process_decided_certificate(&certificate, bytes, &mut notifier).await?;
                }

                let metrics_snapshot = metrics.snapshot();
                let retention_window = RETENTION as usize;
                let expected_pruned = TOTAL_HEIGHTS.saturating_sub(retention_window);
                assert_eq!(metrics_snapshot.lifecycle_promoted, TOTAL_HEIGHTS as u64);
                assert_eq!(metrics_snapshot.lifecycle_pruned, expected_pruned as u64);
                assert_eq!(
                    metrics_snapshot.storage_bytes_decided,
                    ((TOTAL_HEIGHTS - expected_pruned) * BYTES_PER_BLOB) as i64
                );

                for height in 0..expected_pruned {
                    let decided =
                        state.blob_engine().get_for_import(Height::new(height as u64)).await?;
                    assert!(
                        decided.is_empty(),
                        "height {} should be pruned beyond retention window",
                        height
                    );
                }
                for height in expected_pruned..TOTAL_HEIGHTS {
                    let decided =
                        state.blob_engine().get_for_import(Height::new(height as u64)).await?;
                    assert_eq!(decided.len(), 1, "height {} should retain blob", height);
                }
                assert_eq!(state.current_height, Height::new(TOTAL_HEIGHTS as u64));
                drop(state);
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_sync_package_roundtrip() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let (mut state, metrics) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };

                let height = Height::new(0);
                let round = Round::new(0);
                let bundle = sample_blob_bundle(1);
                let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));
                let payload_bytes = Bytes::from(payload.as_ssz_bytes());
                let header = ExecutionPayloadHeader::from_payload(&payload);
                let value_metadata = ValueMetadata::new(header.clone(), bundle.commitments.clone());
                let value = StateValue::new(value_metadata.clone());

                let locally_proposed = LocallyProposedValue::new(height, round, value.clone());
                let (_signed_header, sidecars) =
                    state.prepare_blob_sidecar_parts(&locally_proposed, Some(&bundle))?;

                let package = SyncedValuePackage::Full {
                    value: value.clone(),
                    execution_payload_ssz: payload_bytes.clone(),
                    blob_sidecars: sidecars.clone(),
                };
                let encoded = package.encode().map_err(|e| eyre::eyre!(e))?;
                let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| eyre::eyre!(e))?;

                let proposer = state.validator_address().clone();
                let proposed_value = state
                    .process_synced_package(height, round, proposer, decoded)
                    .await?
                    .expect("sync package should yield proposal");

                let certificate = CommitCertificate {
                    height,
                    round,
                    value_id: proposed_value.value.id(),
                    commit_signatures: Vec::new(),
                };
                let payload_for_commit = state
                    .get_block_data(height, round)
                    .await
                    .unwrap_or_else(|| payload_bytes.clone());
                let mut notifier = MockExecutionNotifier::default();
                state
                    .process_decided_certificate(&certificate, payload_for_commit, &mut notifier)
                    .await?;

                let imported = state.blob_engine().get_for_import(height).await?;
                assert_eq!(imported.len(), 1);
                assert_eq!(imported[0].kzg_commitment, bundle.commitments[0]);
                assert!(state.get_blob_metadata(height).await?.is_some());

                let metrics_snapshot = metrics.snapshot();
                assert_eq!(metrics_snapshot.verifications_success, 1);
                assert_eq!(metrics_snapshot.lifecycle_promoted, 1);
                drop(state);
                Ok(())
            })
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires full-node harness; run via make itest-node"]
#[serial(full_node)]
async fn full_node_value_sync_proof_failure() -> Result<()> {
    init_test_logging();
    FullNodeTestBuilder::new()
        .node_count(1)
        .start_height(Some(Height::new(0)))
        .run(|network| {
            Box::pin(async move {
                network.stop_node(0).await?;
                let (mut node_state, node_metrics) = {
                    let node = network.node_ref(0)?;
                    open_state_ready_with_metrics(node).await?
                };

                let height = Height::new(0);
                let round = Round::new(0);
                let bundle = sample_blob_bundle(1);
                let payload = sample_execution_payload_v3_for_height(height, Some(&bundle));

                let (_proposed, payload_bytes, maybe_sidecars) = propose_with_optional_blobs(
                    &mut node_state,
                    height,
                    round,
                    &payload,
                    Some(&bundle),
                )
                .await?;
                let mut sidecars = maybe_sidecars.expect("sidecars expected");
                sidecars[0].kzg_proof.0[0] ^= 0xFF;

                let package = SyncedValuePackage::Full {
                    value: StateValue::new(ValueMetadata::new(
                        ExecutionPayloadHeader::from_payload(&payload),
                        bundle.commitments.clone(),
                    )),
                    execution_payload_ssz: payload_bytes.clone(),
                    blob_sidecars: sidecars.clone(),
                };
                let encoded = package.encode().map_err(|e| eyre::eyre!(e))?;
                let decoded = SyncedValuePackage::decode(&encoded).map_err(|e| eyre::eyre!(e))?;

                let result = node_state
                    .process_synced_package(height, round, network.node_address(0)?, decoded)
                    .await?;
                assert!(result.is_none(), "tampered proof should be rejected");

                let metrics = node_metrics.snapshot();
                assert_eq!(metrics.sync_failures, 1);
                assert_eq!(metrics.verifications_failure, sidecars.len() as u64);
                assert_eq!(metrics.verifications_success, 0);
                let round_i64 = round.as_i64();
                let undecided =
                    node_state.blob_engine().get_undecided_blobs(height, round_i64).await?;
                assert!(undecided.is_empty());
                drop(node_state);
                Ok(())
            })
        })
        .await
}

/// Representation of a node running inside the multi-node harness.
struct NodeProcess {
    handle: Option<Handle>,
    home: TempDir,
    engine_stub: EngineRpcStub,
    event_rx: TokioMutex<RxEvent<LoadContext>>,
    config: Config,
    genesis_path: PathBuf,
    key_path: PathBuf,
    jwt_path: PathBuf,
    base_start_height: Option<Height>,
    validator_address: Address,
    stub_state: Arc<TokioMutex<StubState>>,
    running: bool,
}

impl NodeProcess {
    async fn start(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        let resume = self.compute_resume_info().await?;
        if let Some(block) = resume.last_decided_block.clone() {
            self.align_stub_head(block).await;
        }
        let stub_latest = {
            let guard = self.stub_state.lock().await;
            guard.latest_block.block_number
        };
        let app_start_height =
            if resume.derived_from_store { None } else { self.base_start_height };
        let engine_url = self.engine_stub.url();
        let eth_url = engine_url.clone();
        debug_log!(
            "starting node: resume_height={} (from_store={}), start_override={:?}, stub latest block={}",
            resume.resume_height,
            resume.derived_from_store,
            app_start_height,
            stub_latest
        );
        let app = App {
            config: self.config.clone(),
            home_dir: self.home.path().to_path_buf(),
            genesis_file: self.genesis_path.clone(),
            private_key_file: self.key_path.clone(),
            start_height: app_start_height,
            engine_http_url: Some(engine_url),
            engine_ipc_path: None,
            eth1_rpc_url: Some(eth_url),
            jwt_path: Some(self.jwt_path.clone()),
        };
        let handle = app.start().await?;
        self.event_rx = TokioMutex::new(handle.tx_event.subscribe());
        self.handle = Some(handle);
        self.base_start_height = Some(resume.resume_height);
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
        self.running = false;
        Ok(())
    }

    async fn restart(&mut self) -> Result<()> {
        self.stop().await?;
        self.start().await
    }

    async fn align_stub_head(&self, latest_block: ExecutionBlock) {
        let mut guard = self.stub_state.lock().await;
        if latest_block.block_number <= guard.latest_block.block_number {
            return;
        }
        guard.latest_block = latest_block;
        guard.pending.clear();
        debug_log!(
            "aligned engine stub head to height {} (hash={})",
            guard.latest_block.block_number,
            format_b256(guard.latest_block.block_hash)
        );
    }

    async fn compute_resume_info(&self) -> Result<ResumeInfo> {
        let fallback = self.base_start_height.unwrap_or_else(|| Height::new(1));
        let store_path = self.home.path().join("store.db");
        if !store_path.exists() {
            tracing::info!(
                node = ?self.validator_address,
                %fallback,
                "resume height uses fallback (store missing)"
            );
            return Ok(ResumeInfo {
                resume_height: fallback,
                last_decided_block: None,
                derived_from_store: false,
            });
        }
        let store = Store::open(store_path, DbMetrics::new())
            .wrap_err("open store to compute resume height")?;
        let decided = store.max_decided_value_height().await;
        let resume = decided.map(|h| Height::new(h.as_u64() + 1)).unwrap_or(fallback);
        let last_block = if let Some(height) = decided {
            match store.get_blob_metadata(height).await {
                Ok(Some(metadata)) => Some(execution_block_from_metadata(&metadata)),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        node = ?self.validator_address,
                        height = %height,
                        error = %e,
                        "Failed to load BlobMetadata while computing resume info"
                    );
                    None
                }
            }
        } else {
            None
        };
        tracing::info!(
            node = ?self.validator_address,
            decided_height = ?decided,
            %resume,
            has_decided_block = %last_block.is_some(),
            "resume height derived from store"
        );
        Ok(ResumeInfo {
            resume_height: resume,
            last_decided_block: last_block,
            derived_from_store: decided.is_some(),
        })
    }
}

struct ResumeInfo {
    resume_height: Height,
    last_decided_block: Option<ExecutionBlock>,
    derived_from_store: bool,
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
            let payload_plan = config.payload_plan();
            let stub_state = Arc::new(TokioMutex::new(StubState::new(payload_plan.clone())));
            let engine_stub = EngineRpcStub::start(stub_state.clone()).await?;
            let home = TempDir::new().wrap_err("create node tempdir")?;
            let validator = &validator_keys[index];
            let validator_address = validator.address();
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
            let engine_http_url = engine_stub.url();
            let eth1_rpc_url = engine_http_url.clone();

            let app = App {
                config: node_config,
                home_dir: home.path().to_path_buf(),
                genesis_file: genesis_path,
                private_key_file: key_path,
                start_height: config.start_height,
                engine_http_url: Some(engine_http_url.clone()),
                engine_ipc_path: None,
                eth1_rpc_url: Some(eth1_rpc_url),
                jwt_path: Some(jwt_path),
            };

            let handle = app.start().await?;
            let event_rx = TokioMutex::new(handle.tx_event.subscribe());
            nodes.push(NodeProcess {
                handle: Some(handle),
                home,
                engine_stub,
                event_rx,
                config: stored_config,
                genesis_path: stored_genesis,
                key_path: stored_key,
                jwt_path: stored_jwt,
                base_start_height: config.start_height,
                validator_address,
                stub_state,
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
                    Ok(Event::Decided(cert)) => {
                        debug_log!("node {} decided height {}", node_index, cert.height);
                        if cert.height >= target_height {
                            break Ok(());
                        }
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

    fn node_ref(&self, node_index: usize) -> Result<&NodeProcess> {
        self.nodes.get(node_index).ok_or_else(|| eyre::eyre!("node {node_index} missing"))
    }

    fn node_address(&self, node_index: usize) -> Result<Address> {
        Ok(self.node_ref(node_index)?.validator_address.clone())
    }

    async fn ensure_shutdown(&mut self) -> Result<()> {
        if !self.is_shutdown {
            self.shutdown().await?;
            sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if self.is_shutdown {
            return Ok(());
        }
        for node in &mut self.nodes {
            node.stop().await?;
            node.engine_stub.shutdown().await;
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
        self.ensure_shutdown().await?;
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

    async fn assert_blob_metadata(
        &mut self,
        node_index: usize,
        height: Height,
        expected_blobs: usize,
    ) -> Result<()> {
        self.ensure_shutdown().await?;
        let node =
            self.nodes.get(node_index).ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        let metadata = load_blob_metadata(node, node_index, height).await?;
        let actual = usize::from(metadata.blob_count());
        assert_eq!(
            actual, expected_blobs,
            "node {node_index} expected {expected_blobs} blobs at height {height}, found {}",
            actual
        );
        Ok(())
    }

    async fn assert_parent_root_matches(
        &mut self,
        node_index: usize,
        height: Height,
    ) -> Result<()> {
        self.ensure_shutdown().await?;
        let node =
            self.nodes.get(node_index).ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        let mut state = open_state_from_disk(node)?;
        state.hydrate_blob_parent_root().await?;
        let metadata = state
            .get_blob_metadata(height)
            .await?
            .ok_or_else(|| eyre::eyre!("node {node_index} missing metadata at height {height}"))?;
        let expected_root = metadata.to_beacon_header().hash_tree_root();
        let cached_root = state.blob_parent_root();
        assert_eq!(
            cached_root,
            expected_root,
            "node {node_index} parent root mismatch at height {height}: expected {}, got {}",
            format_b256(expected_root),
            format_b256(cached_root)
        );
        Ok(())
    }

    async fn assert_rebuilds_sidecars(
        &mut self,
        node_index: usize,
        height: Height,
        expected_blobs: usize,
    ) -> Result<()> {
        self.ensure_shutdown().await?;
        let node =
            self.nodes.get(node_index).ok_or_else(|| eyre::eyre!("node {node_index} missing"))?;
        let state = open_state_ready(node).await?;
        let metadata = state
            .get_blob_metadata(height)
            .await?
            .ok_or_else(|| eyre::eyre!("node {node_index} missing metadata for height {height}"))?;
        let blobs = state.blob_engine().get_for_import(height).await?;
        let rebuilt =
            state.rebuild_blob_sidecars_for_restream(&metadata, &node.validator_address, &blobs)?;
        assert_eq!(
            rebuilt.len(),
            expected_blobs,
            "node {node_index} expected {expected_blobs} rebuilt sidecars at height {height}, found {}",
            rebuilt.len()
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

async fn load_blob_metadata(
    node: &NodeProcess,
    node_index: usize,
    height: Height,
) -> Result<BlobMetadata> {
    let store = Store::open(node.home.path().join("store.db"), DbMetrics::new())
        .wrap_err_with(|| format!("open store for node {node_index} to read metadata"))?;
    store
        .get_blob_metadata(height)
        .await?
        .ok_or_else(|| eyre::eyre!("node {node_index} missing metadata for height {height}"))
}

fn open_state_and_metrics(
    node: &NodeProcess,
) -> Result<(State<BlobEngineImpl<RocksDbBlobStore>>, BlobEngineMetrics)> {
    let genesis_file = File::open(&node.genesis_path).wrap_err("open genesis file")?;
    let genesis: Genesis = serde_json::from_reader(genesis_file).wrap_err("decode genesis file")?;
    let key_file = File::open(&node.key_path).wrap_err("open validator key file")?;
    let private_key: PrivateKey =
        serde_json::from_reader(key_file).wrap_err("decode validator key")?;
    let store = Store::open(node.home.path().join("store.db"), DbMetrics::new())?;
    let blob_store = RocksDbBlobStore::open(node.home.path().join("blob_store.db"))?;
    let blob_metrics = BlobEngineMetrics::new();
    let blob_engine = BlobEngineImpl::new(blob_store, blob_metrics.clone())?;
    let provider = Ed25519Provider::new(private_key);
    let state = State::new(
        genesis,
        LoadContext::new(),
        provider,
        node.validator_address.clone(),
        node.base_start_height.unwrap_or_else(|| Height::new(1)),
        store,
        blob_engine,
        blob_metrics.clone(),
    );
    Ok((state, blob_metrics))
}

fn open_state_from_disk(node: &NodeProcess) -> Result<State<BlobEngineImpl<RocksDbBlobStore>>> {
    let (state, _) = open_state_and_metrics(node)?;
    Ok(state)
}

async fn open_state_ready(node: &NodeProcess) -> Result<State<BlobEngineImpl<RocksDbBlobStore>>> {
    let (mut state, _) = open_state_and_metrics(node)?;
    state.seed_genesis_blob_metadata().await?;
    state.hydrate_blob_parent_root().await?;
    Ok(state)
}

async fn open_state_ready_with_metrics(
    node: &NodeProcess,
) -> Result<(State<BlobEngineImpl<RocksDbBlobStore>>, BlobEngineMetrics)> {
    let (mut state, metrics) = open_state_and_metrics(node)?;
    state.seed_genesis_blob_metadata().await?;
    state.hydrate_blob_parent_root().await?;
    Ok((state, metrics))
}

/// Minimal Engine + Eth RPC stub that satisfies the calls Ultramarine makes during tests.
struct EngineRpcStub {
    addr: SocketAddr,
    state: Arc<TokioMutex<StubState>>,
    handle: JoinHandle<()>,
}

impl EngineRpcStub {
    async fn start(state: Arc<TokioMutex<StubState>>) -> Result<Self> {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.wrap_err("bind engine stub")?;
        let addr = listener.local_addr().unwrap();
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
    payload_plan: Option<PayloadPlan>,
}

impl StubState {
    fn new(payload_plan: Option<PayloadPlan>) -> Self {
        Self {
            latest_block: default_execution_block(),
            next_payload_id: 0,
            pending: HashMap::new(),
            payload_plan,
        }
    }

    fn blob_count_for_height(&self, height: Height) -> usize {
        self.payload_plan.as_ref().map(|plan| plan(height)).unwrap_or(1)
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

const SAMPLE_PAYLOAD_TIMESTAMP_BASE: u64 = 1_700_000_000;

fn block_hash_for_height(height: u64) -> B256 {
    B256::from([height as u8; 32])
}

fn timestamp_for_height(height: u64) -> u64 {
    SAMPLE_PAYLOAD_TIMESTAMP_BASE + height
}

fn height_from_block_hash(hash: B256) -> Option<u64> {
    if hash == B256::ZERO { None } else { Some(u64::from(hash.0[0])) }
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

    if let Some(head_height) = height_from_block_hash(forkchoice.head_block_hash) {
        if head_height > guard.latest_block.block_number {
            let parent_hash =
                if head_height == 0 { B256::ZERO } else { block_hash_for_height(head_height - 1) };
            guard.latest_block.block_number = head_height;
            guard.latest_block.block_hash = forkchoice.head_block_hash;
            guard.latest_block.parent_hash = parent_hash;
            guard.latest_block.timestamp = timestamp_for_height(head_height);
            debug_log!(
                "engine_forkchoiceUpdatedV3: updated latest block to height {} from forkchoice head",
                head_height
            );
        }
    }

    if let Some(_attrs) = payload_attrs {
        // Generate payload for next height (latest + 1)
        let next_height = Height::new(guard.latest_block.block_number + 1);

        debug_log!("engine_forkchoiceUpdatedV3: generating payload for height {}", next_height);

        let blob_count = guard.blob_count_for_height(next_height);
        let bundle = if blob_count == 0 { None } else { Some(sample_blob_bundle(blob_count)) };
        let payload = sample_execution_payload_v3_for_height(next_height, bundle.as_ref());
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

fn execution_block_from_metadata(metadata: &BlobMetadata) -> ExecutionBlock {
    let header = metadata.execution_payload_header();
    ExecutionBlock {
        block_hash: header.block_hash,
        block_number: header.block_number,
        parent_hash: header.parent_hash,
        timestamp: header.timestamp,
        prev_randao: header.prev_randao,
    }
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
        let store_src = node.home.path().join("store.db");
        if store_src.exists() {
            let snapshot_dir = tempfile::tempdir().wrap_err("create store snapshot dir")?;
            let snapshot_path = snapshot_dir.path().join("store.db");
            copy_path(&store_src, &snapshot_path).wrap_err("snapshot store db")?;
            let store = Store::open(snapshot_path, DbMetrics::new())?;
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
        } else {
            tracing::warn!(
                node = node_index,
                path = %store_src.display(),
                "Timeout diagnostics: store path missing"
            );
        }
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
        match tempfile::tempdir() {
            Ok(snapshot_dir) => {
                let wal_snapshot = snapshot_dir.path().join("consensus.wal");
                if let Err(e) = copy_path(&wal_path, &wal_snapshot) {
                    tracing::warn!(
                        node = node_index,
                        path = %wal_path.display(),
                        error = %e,
                        "Timeout diagnostics: failed to snapshot WAL"
                    );
                    return;
                }

                match WalLog::open(&wal_snapshot) {
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
                        error = %e,
                        "Timeout diagnostics: failed to open WAL snapshot"
                    ),
                }
            }
            Err(e) => tracing::warn!(
                node = node_index,
                error = %e,
                "Timeout diagnostics: failed to create WAL snapshot dir"
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

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let target = dst.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_path(&entry.path(), &target)?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(entry.path(), &target)?;
            }
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn build_error(id: &Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message },
        "id": id,
    })
}
