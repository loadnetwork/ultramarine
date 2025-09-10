//! The Application (or Node) definition. The Node trait implements the Consensus context and the
//! cryptographic library used for signing.
#![allow(missing_docs)]
use std::{path::PathBuf, str::FromStr};

use async_trait::async_trait;
use color_eyre::eyre;
use malachitebft_app_channel::app::{
    EngineHandle, Node, NodeHandle,
    events::{RxEvent, TxEvent},
    metrics::SharedRegistry,
    types::{Keypair, config::Config, core::VotingPower},
};
use rand::{CryptoRng, RngCore};
use tokio::task::JoinHandle;
use ultramarine_cli::metrics;
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
#[derive(Clone)]
pub struct App {
    pub config: Config,
    pub home_dir: PathBuf,
    pub genesis_file: PathBuf,
    pub private_key_file: PathBuf,
    pub start_height: Option<Height>,
}

pub struct Handle {
    pub app: JoinHandle<()>,
    pub engine: EngineHandle,
    pub tx_event: TxEvent<LoadContext>,
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
    type Genesis = Genesis;
    type PrivateKeyFile = PrivateKey;
    type SigningProvider = Ed25519Provider;
    type NodeHandle = Handle;

    fn get_home_dir(&self) -> PathBuf {
        self.home_dir.to_owned()
    }

    fn get_signing_provider(&self, private_key: PrivateKey) -> Self::SigningProvider {
        Ed25519Provider::new(private_key)
    }

    fn generate_private_key<R>(&self, rng: R) -> PrivateKey
    where
        R: RngCore + CryptoRng,
    {
        PrivateKey::generate(rng)
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

    fn load_private_key_file(&self) -> std::io::Result<Self::PrivateKeyFile> {
        let private_key = std::fs::read_to_string(&self.private_key_file)?;
        serde_json::from_str(&private_key).map_err(|e| e.into())
    }

    fn make_private_key_file(&self, private_key: PrivateKey) -> Self::PrivateKeyFile {
        private_key
    }

    fn load_genesis(&self) -> std::io::Result<Self::Genesis> {
        let genesis = std::fs::read_to_string(&self.genesis_file)?;
        serde_json::from_str(&genesis).map_err(|e| e.into())
    }

    fn make_genesis(&self, validators: Vec<(PublicKey, VotingPower)>) -> Self::Genesis {
        let validators = validators.into_iter().map(|(pk, vp)| Validator::new(pk, vp));

        let validator_set = ValidatorSet::new(validators);

        Genesis { validator_set }
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

        // Start malachite engine
        let (mut channels, engine_handle) = malachitebft_app_channel::start_engine(
            ctx,
            codec,
            self.clone(),
            self.config.clone(),
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
        let start_height = self.start_height.unwrap_or_default();
        let mut state = State::new(genesis, ctx, signing_provider, address, start_height, store);

        // --- Initialize Execution Client ---
        // TODO: The following logic for determining ports and paths is for local
        // testing convenience. A production-ready implementation should remove this
        // and rely solely on the loaded configuration. The logic for generating
        // testnet configurations with unique ports should be moved to the
        // `TestnetCmd` in the `cli` crate.
        let engine_url: Url = {
            let engine_port = match self.config.moniker.as_str() {
                "test-0" => 8551,
                "test-1" => 18551,
                "test-2" => 28551,
                _ => 8551,
            };
            Url::parse(&format!("http://localhost:{engine_port}"))?
        };
        let jwt_path = PathBuf::from_str("./assets/jwtsecret")?;
        let eth_url: Url = {
            let eth_port = match self.config.moniker.as_str() {
                "test-0" => 8545,
                "test-1" => 18545,
                "test-2" => 28545,
                _ => 8545,
            };
            Url::parse(&format!("http://localhost:{eth_port}"))?
        };

        let jwt_secret_bytes = std::fs::read(&jwt_path).map_err(|e| {
            eyre::eyre!("Failed to read JWT secret from {}: {}", jwt_path.display(), e)
        })?;
        let jwt_secret: [u8; 32] = jwt_secret_bytes.try_into().map_err(|_| {
            eyre::eyre!("JWT secret at {} must be exactly 32 bytes", jwt_path.display())
        })?;

        let execution_config = ExecutionConfig {
            engine_api_endpoint: config::EngineApiEndpoint::Http(engine_url),
            eth1_rpc_url: eth_url,
            jwt_secret,
        };

        let execution_client = ExecutionClient::new(execution_config).await?;

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
