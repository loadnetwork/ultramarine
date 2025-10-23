//! The Application (or Node) definition. The Node trait implements the Consensus context and the
//! cryptographic library used for signing.
#![allow(missing_docs)]
use std::{path::PathBuf, str::FromStr};

use async_trait::async_trait;
use color_eyre::eyre;
use malachitebft_app_channel::app::{
    events::{RxEvent, TxEvent},
    metrics::SharedRegistry,
    node::{EngineHandle, Node, NodeHandle},
    types::{Keypair, core::VotingPower},
};
use rand::{CryptoRng, RngCore};
use tokio::task::JoinHandle;
use ultramarine_blob_engine::{BlobEngineImpl, store::rocksdb::RocksDbBlobStore};
use ultramarine_cli::{config::Config, metrics};
use ultramarine_consensus::{metrics::DbMetrics, state::State, store::Store};
use ultramarine_execution::{
    client::ExecutionClient,
    config::{self, ExecutionConfig},
};
use ultramarine_types::{
    address::Address,
    codec::proto::ProtobufCodec,
    context::LoadContext,
    genesis::Genesis,
    height::Height,
    signing::{Ed25519Provider, PrivateKey, PublicKey},
    validator_set::{Validator, ValidatorSet},
};
use url::Url;

/// Main application struct implementing the consensus node functionality
#[derive(Clone, Debug)]
pub struct App {
    pub config: Config,
    pub home_dir: PathBuf,
    pub genesis_file: PathBuf,
    pub private_key_file: PathBuf,
    pub start_height: Option<Height>,

    // Optional execution-layer configuration overrides
    pub engine_http_url: Option<Url>,
    pub engine_ipc_path: Option<PathBuf>,
    pub eth1_rpc_url: Option<Url>,
    pub jwt_path: Option<PathBuf>,
}

pub struct Handle {
    pub app: JoinHandle<()>,
    pub engine: EngineHandle,
    pub tx_event: TxEvent<LoadContext>,
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
        Ok(self.config.clone())
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

        let codec = ProtobufCodec;

        // Start malachite engine with separate WAL and network codecs
        let (mut channels, engine_handle) = malachitebft_app_channel::start_engine(
            ctx,
            self.clone(),
            self.config.clone(),
            codec, // wal_codec
            codec, // net_codec (same codec for both)
            self.start_height,
            initial_validator_set,
        )
        .await?;

        let tx_event = channels.events.clone();

        let registry = SharedRegistry::global().with_moniker(&self.config.moniker);
        let metrics = DbMetrics::register(&registry);

        if self.config.metrics.enabled {
            tokio::spawn(metrics::serve(self.config.metrics.listen_addr));
        }

        let store = Store::open(self.get_home_dir().join("store.db"), metrics)?;

        // Initialize blob engine for EIP-4844 blob storage and KZG verification
        let blob_store = RocksDbBlobStore::open(self.get_home_dir().join("blob_store.db"))?;
        let blob_engine = BlobEngineImpl::new(blob_store)?;

        let start_height = self.start_height.unwrap_or_default();
        let mut state =
            State::new(genesis, ctx, signing_provider, address, start_height, store, blob_engine);

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

        // 2) Eth RPC reachability and basic sanity (latest block must exist)
        match execution_client
            .eth()
            .get_block_by_number(alloy_rpc_types_eth::BlockNumberOrTag::Latest, false)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(eyre::eyre!(
                    "Eth RPC at {} returned no 'latest' block. Check genesis/chain is initialized.",
                    eth_url_str
                ))
            }
            Err(e) => return Err(eyre::eyre!("Failed to reach Eth RPC at {}: {}", eth_url_str, e)),
        }

        let app_handle = tokio::spawn(async move {
            if let Err(e) = crate::app::run(&mut state, &mut channels, execution_client).await {
                tracing::error!(%e, "Application error");
            }
        });

        Ok(Handle { app: app_handle, engine: engine_handle, tx_event })
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
