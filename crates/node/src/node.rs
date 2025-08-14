//! The Application (or Node) definition. The Node trait implements the Consensus context and the
//! cryptographic library used for signing.
#![allow(missing_docs)]
use std::path::PathBuf;

use async_trait::async_trait;
use color_eyre::eyre;
use malachitebft_app_channel::app::{
    EngineHandle, Node, NodeHandle,
    events::{RxEvent, TxEvent},
    types::{Keypair, config::Config, core::VotingPower},
};
use rand::{CryptoRng, RngCore};
use tokio::task::JoinHandle;
// use ultramarine_execution::{engine::Engine, engine_rpc::EngineRPC,
// ethereum_rpc::EthereumRPC};
use ultramarine_types::height::Height;
use ultramarine_types::{
    address::Address,
    context::LoadContext,
    genesis::Genesis,
    signing::{Ed25519Provider, PrivateKey, PublicKey},
};

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
        todo!()
    }

    fn get_signing_provider(&self, private_key: PrivateKey) -> Self::SigningProvider {
        todo!()
    }

    fn generate_private_key<R>(&self, rng: R) -> PrivateKey
    where
        R: RngCore + CryptoRng,
    {
        todo!()
    }

    fn get_address(&self, pk: &PublicKey) -> Address {
        todo!()
    }

    fn get_public_key(&self, pk: &PrivateKey) -> PublicKey {
        todo!()
    }

    fn get_keypair(&self, pk: PrivateKey) -> Keypair {
        todo!()
    }

    fn load_private_key(&self, file: Self::PrivateKeyFile) -> PrivateKey {
        todo!()
    }

    fn load_private_key_file(&self) -> std::io::Result<Self::PrivateKeyFile> {
        todo!()
    }

    fn make_private_key_file(&self, private_key: PrivateKey) -> Self::PrivateKeyFile {
        todo!()
    }

    fn load_genesis(&self) -> std::io::Result<Self::Genesis> {
        todo!()
    }

    fn make_genesis(&self, validators: Vec<(PublicKey, VotingPower)>) -> Self::Genesis {
        todo!()
    }

    async fn start(&self) -> eyre::Result<Handle> {
        todo!()
    }

    async fn run(self) -> eyre::Result<()> {
        todo!()
    }
}
