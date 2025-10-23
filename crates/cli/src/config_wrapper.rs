// Custom Config wrapper for ultramarine CLI
// Malachite no longer provides a single Config type, so we create our own

use malachitebft_app_channel::app::node::NodeConfig;
use malachitebft_config::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub moniker: String,
    pub consensus: ConsensusConfig,
    pub mempool: MempoolConfig,
    pub sync: ValueSyncConfig,
    pub metrics: MetricsConfig,
    pub logging: LoggingConfig,
    pub runtime: RuntimeConfig,
    pub test: TestConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            moniker: "node".to_string(),
            consensus: ConsensusConfig::default(),
            mempool: MempoolConfig::default(),
            sync: ValueSyncConfig::default(),
            metrics: MetricsConfig::default(),
            logging: LoggingConfig::default(),
            runtime: RuntimeConfig::default(),
            test: TestConfig::default(),
        }
    }
}

impl NodeConfig for Config {
    fn moniker(&self) -> &str {
        &self.moniker
    }

    fn consensus(&self) -> &ConsensusConfig {
        &self.consensus
    }

    fn consensus_mut(&mut self) -> &mut ConsensusConfig {
        &mut self.consensus
    }

    fn value_sync(&self) -> &ValueSyncConfig {
        &self.sync
    }

    fn value_sync_mut(&mut self) -> &mut ValueSyncConfig {
        &mut self.sync
    }
}
