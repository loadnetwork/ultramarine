//! The Application (or Node) definition. The Node trait implements the Consensus context and the
//! cryptographic library used for signing.
#![allow(missing_docs)]
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use alloy_consensus::{Header, constants::EMPTY_WITHDRAWALS};
use alloy_eips::{eip1559::INITIAL_BASE_FEE, eip7685::EMPTY_REQUESTS_HASH};
use alloy_genesis::Genesis as ExecutionGenesis;
use alloy_primitives::{B64, B256};
use alloy_trie::root::state_root_ref_unhashed;
use async_trait::async_trait;
use color_eyre::eyre;
use malachitebft_app_channel::app::{
    events::{RxEvent, TxEvent},
    metrics::SharedRegistry,
    node::{EngineHandle, Node, NodeHandle},
    types::{Keypair, core::VotingPower},
};
use rand::{CryptoRng, RngCore};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use ultramarine_blob_engine::{BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
use ultramarine_cli::{config::Config, metrics};
use ultramarine_consensus::{metrics::DbMetrics, state::State, store::Store};
use ultramarine_execution::{
    client::ExecutionClient,
    config::{self, ExecutionConfig},
};
use ultramarine_types::{
    address::Address,
    archive::ArchiveNotice,
    codec::proto::ProtobufCodec,
    context::LoadContext,
    engine_api::{ExecutionBlock, load_prev_randao},
    genesis::Genesis,
    height::Height,
    signing::{Ed25519Provider, PrivateKey, PublicKey},
    validator_set::{Validator, ValidatorSet},
};
use url::Url;

use crate::archiver::{self, ArchiveJobSubmitter, ArchiverHandle};

fn validate_validator_archiver_setting(
    is_validator: bool,
    archiver_enabled: bool,
    address: &Address,
) -> eyre::Result<()> {
    if is_validator && !archiver_enabled && !cfg!(feature = "test-harness") {
        return Err(eyre::eyre!(
            "archiver.enabled=false but this node ({}) is in the validator set; \
             validators must run with archiver enabled",
            address
        ));
    }
    Ok(())
}

fn validate_archiver_config_strict(effective_config: &Config) -> eyre::Result<()> {
    // Phase 6 strictness: no silent mock / missing config.
    // In production binaries, mock:// backends are disallowed.
    // If archiver is enabled, require provider_url/provider_id/bearer_token.
    if effective_config.archiver.enabled && !cfg!(any(test, feature = "test-harness")) {
        if effective_config.archiver.provider_url.starts_with("mock://") {
            return Err(eyre::eyre!(
                "Archiver is enabled but provider_url={} is mock mode. \
                 Mock backends are only allowed in tests.",
                effective_config.archiver.provider_url
            ));
        }
        if effective_config.archiver.provider_url.trim().is_empty() {
            return Err(eyre::eyre!("Archiver is enabled but archiver.provider_url is empty"));
        }
        if effective_config.archiver.provider_id.trim().is_empty() {
            return Err(eyre::eyre!("Archiver is enabled but archiver.provider_id is empty"));
        }
        if effective_config.archiver.bearer_token.as_deref().unwrap_or("").trim().is_empty() {
            return Err(eyre::eyre!(
                "Archiver is enabled but archiver.bearer_token is not set. \
                 Set ULTRAMARINE_ARCHIVER_BEARER_TOKEN or archiver.bearer_token in config."
            ));
        }
        tracing::info!(
            provider_url = %effective_config.archiver.provider_url,
            provider_id = %effective_config.archiver.provider_id,
            "Phase 6: Archiver configured for real provider"
        );
    }
    Ok(())
}

/// Main application struct implementing the consensus node functionality
#[derive(Clone, Debug)]
pub struct App {
    pub config: Config,
    pub home_dir: PathBuf,
    pub genesis_file: PathBuf,
    pub private_key_file: PathBuf,
    pub start_height: Option<Height>,
    /// Execution-layer genesis file (same JSON used by load-reth --chain).
    pub execution_genesis_file: Option<PathBuf>,

    // Optional execution-layer configuration overrides
    pub engine_http_url: Option<Url>,
    pub engine_ipc_path: Option<PathBuf>,
    pub eth1_rpc_url: Option<Url>,
    pub jwt_path: Option<PathBuf>,
}

fn fork_block_active_at_genesis(block: Option<u64>) -> bool {
    matches!(block, Some(0))
}

fn fork_time_active_at_genesis(time: Option<u64>, genesis_ts: u64) -> bool {
    time.map(|t| t <= genesis_ts).unwrap_or(false)
}

fn build_execution_genesis_header(genesis: &ExecutionGenesis) -> eyre::Result<Header> {
    let london_active = fork_block_active_at_genesis(genesis.config.london_block);
    let base_fee_per_gas = if london_active {
        let base_fee = genesis.base_fee_per_gas.unwrap_or(u128::from(INITIAL_BASE_FEE));
        let base_fee_u64 = u64::try_from(base_fee).map_err(|_| {
            eyre::eyre!("genesis base_fee_per_gas {} does not fit in u64", base_fee)
        })?;
        Some(base_fee_u64)
    } else {
        None
    };

    let shanghai_active =
        fork_time_active_at_genesis(genesis.config.shanghai_time, genesis.timestamp);
    let withdrawals_root = if shanghai_active { Some(EMPTY_WITHDRAWALS) } else { None };

    let cancun_active = fork_time_active_at_genesis(genesis.config.cancun_time, genesis.timestamp);
    let (parent_beacon_block_root, blob_gas_used, excess_blob_gas) = if cancun_active {
        (
            Some(B256::ZERO),
            Some(genesis.blob_gas_used.unwrap_or(0)),
            Some(genesis.excess_blob_gas.unwrap_or(0)),
        )
    } else {
        (None, None, None)
    };

    let prague_active = fork_time_active_at_genesis(genesis.config.prague_time, genesis.timestamp);
    let requests_hash = if prague_active { Some(EMPTY_REQUESTS_HASH) } else { None };

    Ok(Header {
        parent_hash: genesis.parent_hash.unwrap_or_default(),
        number: genesis.number.unwrap_or_default(),
        gas_limit: genesis.gas_limit,
        difficulty: genesis.difficulty,
        nonce: B64::from(genesis.nonce),
        extra_data: genesis.extra_data.clone(),
        timestamp: genesis.timestamp,
        mix_hash: genesis.mix_hash,
        beneficiary: genesis.coinbase,
        state_root: state_root_ref_unhashed(&genesis.alloc),
        base_fee_per_gas,
        withdrawals_root,
        parent_beacon_block_root,
        blob_gas_used,
        excess_blob_gas,
        requests_hash,
        ..Default::default()
    })
}

fn execution_genesis_block_from_file(path: &Path) -> eyre::Result<ExecutionBlock> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        eyre::eyre!("Failed to read execution genesis at {}: {}", path.display(), e)
    })?;
    let genesis: ExecutionGenesis = serde_json::from_str(&raw).map_err(|e| {
        eyre::eyre!("Failed to parse execution genesis at {}: {}", path.display(), e)
    })?;
    let header = build_execution_genesis_header(&genesis)?;
    let block_hash = header.hash_slow();
    Ok(ExecutionBlock {
        block_hash,
        block_number: header.number,
        parent_hash: header.parent_hash,
        timestamp: header.timestamp,
        prev_randao: load_prev_randao(),
    })
}

pub struct Handle {
    pub app: JoinHandle<()>,
    pub engine: EngineHandle,
    pub tx_event: TxEvent<LoadContext>,
    pub archiver: Option<ArchiverHandle>,
    pub shutdown: CancellationToken,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").field("app", &self.app).finish_non_exhaustive()
    }
}

#[async_trait]
impl NodeHandle<LoadContext> for Handle {
    fn subscribe(&self) -> RxEvent<LoadContext> {
        self.tx_event.subscribe()
    }

    async fn kill(&self, _reason: Option<String>) -> eyre::Result<()> {
        if let Some(archiver) = &self.archiver {
            archiver.abort();
        }
        self.engine.actor.kill_and_wait(None).await?;
        self.app.abort();
        self.engine.handle.abort();
        Ok(())
    }
}

#[async_trait]
impl Node for App {
    type Context = LoadContext;
    type Config = Config;
    type Genesis = Genesis;
    type PrivateKeyFile = PrivateKey;
    type SigningProvider = Ed25519Provider;
    type NodeHandle = Handle;

    fn get_home_dir(&self) -> PathBuf {
        self.home_dir.to_owned()
    }

    fn load_config(&self) -> eyre::Result<Self::Config> {
        let mut cfg = self.config.clone();
        if !cfg!(feature = "test-harness") {
            cfg.apply_archiver_env_overrides();
        }
        Ok(cfg)
    }

    fn get_signing_provider(&self, private_key: PrivateKey) -> Self::SigningProvider {
        Ed25519Provider::new(private_key)
    }

    fn get_address(&self, pk: &PublicKey) -> Address {
        Address::from_public_key(pk)
    }

    fn get_public_key(&self, pk: &PrivateKey) -> PublicKey {
        pk.public_key()
    }

    fn get_keypair(&self, pk: PrivateKey) -> Keypair {
        Keypair::ed25519_from_bytes(pk.inner().to_bytes())
            .expect("a valid private key should always produce a valid keypair")
    }

    fn load_private_key(&self, file: Self::PrivateKeyFile) -> PrivateKey {
        file
    }

    fn load_private_key_file(&self) -> eyre::Result<Self::PrivateKeyFile> {
        let private_key = std::fs::read_to_string(&self.private_key_file)?;
        Ok(serde_json::from_str(&private_key)?)
    }

    fn load_genesis(&self) -> eyre::Result<Self::Genesis> {
        let genesis = std::fs::read_to_string(&self.genesis_file)?;
        Ok(serde_json::from_str(&genesis)?)
    }

    async fn start(&self) -> eyre::Result<Handle> {
        let span = tracing::error_span!("node", moniker = %self.config.moniker);
        let _enter = span.enter();

        // Load a single effective config (base + env overrides) and use it everywhere.
        let effective_config = self.load_config()?;
        validate_archiver_config_strict(&effective_config)?;

        let private_key_file = self.load_private_key_file()?;
        let private_key = self.load_private_key(private_key_file);
        let public_key = self.get_public_key(&private_key);
        let address = self.get_address(&public_key);
        let signing_provider = self.get_signing_provider(private_key);
        let ctx = LoadContext::new();

        let genesis = self.load_genesis()?;
        // TODO: how should it be handled in dynamic set? or it's like we init with genesis and
        // then connect to the peers and receive the actual validator set?
        // hmm
        let initial_validator_set = genesis.validator_set.clone();

        // PoA strictness: if this node is in the validator set, archiver must be enabled.
        // Otherwise, heights proposed by this validator can never become "fully archived",
        // and the network will retain blob bytes indefinitely for those heights.
        //
        // The full-node integration harness builds `ultramarine-node` with `test-harness`
        // and disables archiver by default to keep blob bytes on disk for assertions.
        let is_validator = genesis.validator_set.get_by_address(&address).is_some();
        validate_validator_archiver_setting(
            is_validator,
            effective_config.archiver.enabled,
            &address,
        )?;

        let codec = ProtobufCodec;

        // Start malachite engine with separate WAL and network codecs
        let (mut channels, engine_handle) = malachitebft_app_channel::start_engine(
            ctx,
            self.clone(),
            effective_config.clone(),
            codec, // wal_codec
            codec, // net_codec (same codec for both)
            self.start_height,
            initial_validator_set,
        )
        .await?;

        let tx_event = channels.events.clone();

        let registry = SharedRegistry::global().with_moniker(&effective_config.moniker);
        let metrics = DbMetrics::register(&registry);
        let blob_metrics = ultramarine_blob_engine::BlobEngineMetrics::register(&registry);
        let archive_metrics =
            ultramarine_consensus::archive_metrics::ArchiveMetrics::register(&registry);

        if effective_config.metrics.enabled {
            tokio::spawn(metrics::serve(effective_config.metrics.listen_addr));
        }

        let store = Store::open(self.get_home_dir().join("store.db"), metrics)?;

        // Initialize blob engine for EIP-4844 blob storage and KZG verification
        // Wrapped in Arc to allow sharing with the archiver worker
        let blob_store = RocksDbBlobStore::open(self.get_home_dir().join("blob_store.db"))?;
        let blob_engine = Arc::new(BlobEngineImpl::new(blob_store, blob_metrics.clone())?);

        let start_height = self.start_height.unwrap_or_default();

        // Create a separate signing provider for the archiver by loading the private key again
        // (Ed25519Provider doesn't implement Clone, so we need to create a new instance)
        let archiver_signer = {
            let archiver_pk_file = self.load_private_key_file()?;
            let archiver_pk = self.load_private_key(archiver_pk_file);
            self.get_signing_provider(archiver_pk)
        };
        let archiver_address = address;
        let archive_metrics_for_archiver = archive_metrics.clone();

        let mut state = State::new(
            genesis,
            ctx,
            signing_provider,
            address,
            start_height,
            store,
            blob_engine,
            blob_metrics, // Clone already done above
            archive_metrics,
        );

        let execution_genesis_path = self
            .execution_genesis_file
            .clone()
            .or_else(|| std::env::var("ULTRAMARINE_EL_GENESIS_JSON").ok().map(PathBuf::from))
            .ok_or_else(|| {
                eyre::eyre!(
                    "execution genesis path missing; set --execution-genesis-path or ULTRAMARINE_EL_GENESIS_JSON"
                )
            })?;
        let execution_genesis = execution_genesis_block_from_file(&execution_genesis_path)?;
        state.latest_block = Some(execution_genesis);

        // Phase 4: Hydrate blob parent root from BlobMetadata (Layer 2)
        state.hydrate_blob_parent_root().await?;

        // Phase 4: Cleanup stale undecided blob metadata from crashes/timeouts
        state.cleanup_stale_blob_metadata().await?;

        // Phase 6: Spawn archiver worker if enabled
        let archiver_channels: Option<(
            ArchiverHandle,
            mpsc::Receiver<ArchiveNotice>,
            ArchiveJobSubmitter,
        )> = if effective_config.archiver.enabled {
            // Tell State that archiver worker is enabled so commit() skips synchronous notices
            state.set_archiver_enabled(true);

            let (notice_tx, notice_rx) = mpsc::channel(100);
            let handle = archiver::spawn_archiver(
                effective_config.archiver.clone(),
                notice_tx,
                archiver_signer,
                archiver_address,
                archive_metrics_for_archiver,
                state.blob_engine_shared(),
            )?;
            tracing::info!(
                provider_id = %effective_config.archiver.provider_id,
                "Phase 6: Archiver worker spawned"
            );

            let (job_submit_tx, mut job_submit_rx) = mpsc::unbounded_channel();
            let worker_job_tx = handle.job_tx.clone();
            tokio::spawn(async move {
                while let Some(job) = job_submit_rx.recv().await {
                    if let Err(e) = worker_job_tx.send(job).await {
                        tracing::error!(
                            error = %e,
                            "Archiver worker channel closed; stopping dispatcher"
                        );
                        break;
                    }
                }
            });

            // Recover any pending archive jobs from previous run (crash recovery)
            // Use blocking send to ensure all recovered jobs are enqueued - startup can wait.
            match state.recover_pending_archive_jobs().await {
                Ok(recovered_jobs) => {
                    let count = recovered_jobs.len();
                    let mut enqueued = 0;
                    let mut failed = 0;
                    for job in recovered_jobs {
                        // Use async send (not try_send) to wait for queue space if needed
                        // This ensures recovered jobs aren't dropped on startup
                        match handle.job_tx.send(job).await {
                            Ok(_) => enqueued += 1,
                            Err(e) => {
                                // Channel closed - worker died during startup
                                tracing::error!(
                                    error = %e,
                                    "Archiver channel closed during job recovery"
                                );
                                failed += 1;
                                break; // No point trying more if channel is closed
                            }
                        }
                    }
                    if count > 0 {
                        tracing::info!(
                            recovered_count = count,
                            enqueued = enqueued,
                            failed = failed,
                            "Phase 6: Recovered pending archive jobs from previous run"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to recover pending archive jobs"
                    );
                }
            }

            Some((handle, notice_rx, job_submit_tx))
        } else {
            tracing::info!("Phase 6: Archiver disabled in config");
            None
        };

        // --- Initialize Execution Client ---
        // Development defaults: if Engine/Eth endpoints are not provided, derive them from
        // the moniker (test-0/1/2) to match compose.yaml port mapping.
        // Priority: CLI overrides (if set) > dev defaults (moniker mapping).
        let default_engine_http: Url = {
            let engine_port = match self.config.moniker.as_str() {
                "test-0" => 8551,
                "test-1" => 18551,
                "test-2" => 28551,
                _ => 8551,
            };
            Url::parse(&format!("http://localhost:{engine_port}"))?
        };

        let default_eth_http: Url = {
            let eth_port = match self.config.moniker.as_str() {
                "test-0" => 8545,
                "test-1" => 18545,
                "test-2" => 28545,
                _ => 8545,
            };
            Url::parse(&format!("http://localhost:{eth_port}"))?
        };

        // Select Engine endpoint: IPC takes precedence over HTTP if provided.
        let engine_endpoint = if let Some(ipc_path) = &self.engine_ipc_path {
            let absolute_ipc_path = if ipc_path.is_absolute() {
                ipc_path.clone()
            } else {
                std::env::current_dir()?.join(ipc_path)
            };
            config::EngineApiEndpoint::Ipc(absolute_ipc_path)
        } else {
            let url = self.engine_http_url.clone().unwrap_or(default_engine_http);
            config::EngineApiEndpoint::Http(url)
        };
        // Keep a human-friendly description for error messages after move
        let engine_endpoint_desc = match &engine_endpoint {
            config::EngineApiEndpoint::Http(url) => format!("Http({url})"),
            config::EngineApiEndpoint::Ipc(path) => format!("Ipc({})", path.display()),
        };

        // Select Eth1 RPC endpoint
        let eth_url: Url = self.eth1_rpc_url.clone().unwrap_or(default_eth_http);
        let eth_url_str = eth_url.to_string();

        // JWT handling: required only for HTTP Engine API. Skip reading a JWT for IPC.
        let (jwt_secret, jwt_path_display): ([u8; 32], Option<String>) = match &engine_endpoint {
            config::EngineApiEndpoint::Ipc(_) => {
                // Placeholder; not used by IPC transport.
                ([0u8; 32], None)
            }
            config::EngineApiEndpoint::Http(_) => {
                let jwt_path = if let Some(ref p) = self.jwt_path {
                    p.clone()
                } else {
                    PathBuf::from_str("./assets/jwtsecret")?
                };

                // Read JWT secret file. Accept raw 32 bytes or 64 hex chars (+optional newline).
                let jwt_raw = std::fs::read(&jwt_path).map_err(|e| {
                    eyre::eyre!("Failed to read JWT secret from {}: {}", jwt_path.display(), e)
                })?;
                let jwt_secret: [u8; 32] = if jwt_raw.len() == 32 {
                    jwt_raw.as_slice().try_into().expect("slice length checked to be 32")
                } else {
                    let as_str = String::from_utf8_lossy(&jwt_raw);
                    let hex = as_str.trim();
                    let decoded = hex::decode(hex).map_err(|e| {
                        eyre::eyre!(
                            "JWT secret at {} must be 32 raw bytes or 64 hex chars: {}",
                            jwt_path.display(),
                            e
                        )
                    })?;
                    decoded.as_slice().try_into().map_err(|_| {
                        eyre::eyre!(
                            "JWT secret at {} must decode to exactly 32 bytes (got {} bytes)",
                            jwt_path.display(),
                            decoded.len()
                        )
                    })?
                };
                (jwt_secret, Some(jwt_path.display().to_string()))
            }
        };

        // Log selected endpoints (and JWT source if applicable) for easier diagnostics
        match &jwt_path_display {
            Some(jwt_path) => tracing::info!(
                moniker = %self.config.moniker,
                engine_endpoint = %engine_endpoint_desc,
                eth_rpc = %eth_url_str,
                jwt_path = %jwt_path,
                "Execution client configured"
            ),
            None => tracing::info!(
                moniker = %self.config.moniker,
                engine_endpoint = %engine_endpoint_desc,
                eth_rpc = %eth_url_str,
                "Execution client configured (IPC, no JWT)"
            ),
        }

        let execution_config = ExecutionConfig {
            engine_api_endpoint: engine_endpoint,
            eth1_rpc_url: eth_url,
            jwt_secret,
        };

        let execution_client = ExecutionClient::new(execution_config).await?;

        // --- Fail‑fast preflight checks ---
        // Validate Execution (Engine) API and Eth RPC connectivity before returning the handle.
        // If anything is misconfigured (JWT, endpoint, ports), exit early with a clear error
        // instead of letting the background task fail later and consensus limp along.

        // 1) Engine API capabilities (auth + method availability)
        if let Err(e) = execution_client.check_capabilities().await {
            return Err(eyre::eyre!(
                "Execution client capability check failed for Engine endpoint: {}. Error: {}",
                engine_endpoint_desc,
                e
            ));
        }

        // 2) No Eth RPC preflight here: Engine API is the consensus oracle.

        // Extract job_tx and notice_rx from archiver_channels
        let (archiver_handle, archiver_job_tx, archive_notice_rx) = match archiver_channels {
            Some((handle, notice_rx, submit_tx)) => {
                (Some(handle), Some(submit_tx), Some(notice_rx))
            }
            None => (None, None, None),
        };

        // Create shutdown token for graceful shutdown coordination
        let shutdown = CancellationToken::new();
        let shutdown_for_signal = shutdown.clone();
        let shutdown_for_app = shutdown.clone();

        // Spawn signal handler for graceful shutdown
        // Unix: handle both SIGTERM and SIGINT (Ctrl+C)
        // Windows: handle only Ctrl+C
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::SignalKind;
                let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
                    .expect("Failed to register SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("Received SIGINT (Ctrl+C), initiating graceful shutdown...");
                    }
                    _ = sigterm.recv() => {
                        tracing::info!("Received SIGTERM, initiating graceful shutdown...");
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("Received Ctrl+C, initiating graceful shutdown...");
            }
            shutdown_for_signal.cancel();
        });

        let app_handle = tokio::spawn(async move {
            if let Err(e) = crate::app::run(
                &mut state,
                &mut channels,
                execution_client,
                archiver_job_tx,
                archive_notice_rx,
                shutdown_for_app,
            )
            .await
            {
                tracing::error!(%e, "Application error");
            }
        });

        Ok(Handle {
            app: app_handle,
            engine: engine_handle,
            tx_event,
            archiver: archiver_handle,
            shutdown,
        })
    }

    async fn run(self) -> eyre::Result<()> {
        let handles = self.start().await?;
        handles.app.await.map_err(Into::into)
    }
}

// Implementations of separate capability traits
use malachitebft_app_channel::app::node::{
    CanGeneratePrivateKey, CanMakeGenesis, CanMakePrivateKeyFile,
};

impl CanGeneratePrivateKey for App {
    fn generate_private_key<R>(&self, rng: R) -> PrivateKey
    where
        R: RngCore + CryptoRng,
    {
        PrivateKey::generate(rng)
    }
}

impl CanMakePrivateKeyFile for App {
    fn make_private_key_file(&self, private_key: PrivateKey) -> Self::PrivateKeyFile {
        private_key
    }
}

impl CanMakeGenesis for App {
    fn make_genesis(&self, validators: Vec<(PublicKey, VotingPower)>) -> Self::Genesis {
        let validators = validators.into_iter().map(|(pk, vp)| Validator::new(pk, vp));
        let validator_set = ValidatorSet::new(validators);
        Genesis { validator_set }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "test-harness"))]
    fn validator_requires_archiver_enabled() {
        let address = Address::repeat_byte(0x11);
        let err = validate_validator_archiver_setting(true, false, &address)
            .expect_err("validator with archiver disabled must error");
        let msg = format!("{err:?}");
        assert!(msg.contains("archiver.enabled=false"), "unexpected error: {msg}");
    }

    #[test]
    #[cfg(feature = "test-harness")]
    fn validator_can_disable_archiver_in_test_harness() {
        let address = Address::repeat_byte(0x11);
        validate_validator_archiver_setting(true, false, &address)
            .expect("test harness should allow validators to disable archiver");
    }

    #[test]
    fn non_validator_can_disable_archiver() {
        let address = Address::repeat_byte(0x22);
        validate_validator_archiver_setting(false, false, &address)
            .expect("non-validator should be allowed to disable archiver");
    }
}
