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
// use ultramarine_execution::{engine::Engine, engine_rpc::EngineRPC,
// ethereum_rpc::EthereumRPC};
use ultramarine_types::height::Height;
use ultramarine_types::{
    address::Address,
    context::LoadContext,
    genesis::Genesis,
    proposal::Proposal,
    proposal_part::ProposalPart,
    signing::Ed25519Provider,
    validator_set::{Validator, ValidatorSet},
};

// use ultramarine_types::{
// Address, Ed25519Provider, Genesis, Height, PrivateKey, PublicKey, TestContext, Validator,
// ValidatorSet,
// };

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
impl NodeHandle<TestContext> for Handle {
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
