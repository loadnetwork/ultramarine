//! Init command

use std::path::Path;

use clap::Parser;
use malachitebft_app::Node;
use malachitebft_config::{
    BootstrapProtocol, Config, LoggingConfig, RuntimeConfig, Selector, TransportProtocol,
};
use tracing::{info, warn};

use crate::{
    error::Error,
    file::{save_config, save_genesis, save_priv_validator_key},
    new::{generate_config, generate_genesis, generate_private_keys},
};

#[derive(Parser, Debug, Clone, Default, PartialEq)]
pub struct InitCmd {
    /// Overwrite existing configuration files
    #[clap(long)]
    pub overwrite: bool,

    /// Enable peer discovery.
    /// If enabled, the node will attempt to discover other nodes in the network
    #[clap(long, default_value = "true")]
    pub enable_discovery: bool,

    /// Bootstrap protocol
    /// The protocol used to bootstrap the discovery mechanism
    /// Possible values:
    /// - "kademlia": Kademlia
    /// - "full": Full mesh (default)
    #[clap(long, default_value = "full", verbatim_doc_comment)]
    pub bootstrap_protocol: BootstrapProtocol,

    /// Selector
    /// The selection strategy used to select persistent peers
    /// Possible values:
    /// - "kademlia": Kademlia-based selection, only available with the Kademlia bootstrap protocol
    /// - "random": Random selection (default)
    #[clap(long, default_value = "random", verbatim_doc_comment)]
    pub selector: Selector,

    /// Number of outbound peers
    #[clap(long, default_value = "20", verbatim_doc_comment)]
    pub num_outbound_peers: usize,

    /// Number of inbound peers
    /// Must be greater than or equal to the number of outbound peers
    #[clap(long, default_value = "20", verbatim_doc_comment)]
    pub num_inbound_peers: usize,

    /// Ephemeral connection timeout
    /// The duration in milliseconds an ephemeral connection is kept alive
    #[clap(long, default_value = "5000", verbatim_doc_comment)]
    pub ephemeral_connection_timeout_ms: u64,
}

impl InitCmd {
    /// Execute the init command
    pub fn run<N>(
        &self,
        node: &N,
        config_file: &Path,
        genesis_file: &Path,
        priv_validator_key_file: &Path,
        logging: LoggingConfig,
    ) -> Result<(), Error>
    where
        N: Node,
    {
        let config = &generate_config(
            0,
            1,
            RuntimeConfig::SingleThreaded,
            self.enable_discovery,
            self.bootstrap_protocol,
            self.selector,
            self.num_outbound_peers,
            self.num_inbound_peers,
            self.ephemeral_connection_timeout_ms,
            TransportProtocol::Tcp,
            logging,
        );

        init(node, config, config_file, genesis_file, priv_validator_key_file, self.overwrite)?;

        Ok(())
    }
}

/// init command to generate defaults.
pub fn init<N>(
    node: &N,
    config: &Config,
    config_file: &Path,
    genesis_file: &Path,
    priv_validator_key_file: &Path,
    overwrite: bool,
) -> Result<(), Error>
where
    N: Node,
{
    // Save configuration
    if config_file.exists() && !overwrite {
        warn!(file = ?config_file.display(), "Configuration file already exists, skipping")
    } else {
        info!(file = ?config_file, "Saving configuration");
        save_config(config_file, config)?;
    }

    // Save default priv_validator_key
    if priv_validator_key_file.exists() && !overwrite {
        warn!(
            file = ?priv_validator_key_file.display(),
            "Private key file already exists, skipping",
        );
    } else {
        info!(file = ?priv_validator_key_file, "Saving private key");
        let private_keys = generate_private_keys(node, 1, false);
        let priv_validator_key = node.make_private_key_file(private_keys[0].clone());
        save_priv_validator_key(node, priv_validator_key_file, &priv_validator_key)?;
    }

    // Save default genesis
    if genesis_file.exists() && !overwrite {
        warn!("Genesis file already exists at {:?}, skipping", genesis_file.display())
    } else {
        let private_keys = generate_private_keys(node, 1, false);
        let public_keys = private_keys.iter().map(|pk| node.get_public_key(pk)).collect();

        let genesis = generate_genesis(node, public_keys, false);
        info!(file = ?genesis_file, "Saving test genesis");
        save_genesis(node, genesis_file, &genesis)?;
    }

    Ok(())
}
