use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre;
use malachitebft_app::node::Node;
use malachitebft_config::MetricsConfig;
use tracing::info;
use url::Url;

use crate::metrics;

#[derive(Parser, Debug, Clone, Default, PartialEq)]
pub struct StartCmd {
    #[clap(long)]
    pub start_height: Option<u64>,

    /// Override Engine API HTTP endpoint, e.g. http://localhost:8551
    #[clap(long)]
    pub engine_http_url: Option<Url>,

    /// Override Engine API IPC path, e.g. /path/to/geth.ipc (takes precedence over
    /// --engine-http-url)
    #[clap(long)]
    pub engine_ipc_path: Option<PathBuf>,

    /// Override Eth1 JSON-RPC URL for non-engine RPCs, e.g. http://localhost:8545
    #[clap(long)]
    pub eth1_rpc_url: Option<Url>,

    /// Path to execution-layer genesis.json (same file used by load-reth --chain)
    #[clap(long)]
    pub execution_genesis_path: Option<PathBuf>,

    /// Override JWT secret path used for Engine API authentication
    #[clap(long)]
    pub jwt_path: Option<PathBuf>,
}

impl StartCmd {
    /// Run the node with optional metrics. Note: endpoint overrides (engine_http_url,
    /// engine_ipc_path, eth1_rpc_url, jwt_path) are applied when constructing the
    /// concrete Node (e.g., in the binary), not here.
    pub async fn run(&self, node: impl Node, metrics: Option<MetricsConfig>) -> eyre::Result<()> {
        if self.engine_http_url.is_some() && self.engine_ipc_path.is_some() {
            // Match runtime behavior: IPC takes precedence over HTTP if both are provided.
            info!(
                "Both --engine-ipc-path and --engine-http-url provided; IPC will take precedence"
            );
        }

        info!("Node is starting...");
        start(node, metrics).await?;
        info!("Node has stopped");
        Ok(())
    }
}

/// start command to run a node.
pub async fn start(node: impl Node, metrics: Option<MetricsConfig>) -> eyre::Result<()> {
    // Enable Prometheus
    if let Some(metrics) = metrics &&
        metrics.enabled
    {
        tokio::spawn(metrics::serve(metrics.listen_addr));
    }

    // Start the node
    node.run().await?;

    Ok(())
}
