// Custom Config wrapper for ultramarine CLI
// Malachite no longer provides a single Config type, so we create our own

use malachitebft_app_channel::app::node::NodeConfig;
use malachitebft_config::*;
use serde::{Deserialize, Serialize};
use tracing::warn;
use ultramarine_types::archiver_config::ArchiverConfig;

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
    #[serde(default)]
    pub archiver: ArchiverConfig,
}

impl Config {
    /// Apply environment variable overrides for the archiver worker.
    ///
    /// Supported variables (per docs/ARCHIVER_OPS.md):
    /// - ULTRAMARINE_ARCHIVER_ENABLED
    /// - ULTRAMARINE_ARCHIVER_PROVIDER_URL
    /// - ULTRAMARINE_ARCHIVER_UPLOAD_PATH
    /// - ULTRAMARINE_ARCHIVER_PROVIDER_ID
    /// - ULTRAMARINE_ARCHIVER_BEARER_TOKEN
    /// - ULTRAMARINE_ARCHIVER_RETRY_ATTEMPTS
    /// - ULTRAMARINE_ARCHIVER_RETRY_BACKOFF_MS
    /// - ULTRAMARINE_ARCHIVER_MAX_QUEUE_SIZE
    pub fn apply_archiver_env_overrides(&mut self) {
        fn get(key: &str) -> Option<String> {
            std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        }

        if let Some(v) = get("ULTRAMARINE_ARCHIVER_ENABLED") {
            match v.parse::<bool>() {
                Ok(b) => self.archiver.enabled = b,
                Err(_) => warn!(value = %v, "Invalid ULTRAMARINE_ARCHIVER_ENABLED, ignoring"),
            }
        }

        if let Some(v) = get("ULTRAMARINE_ARCHIVER_PROVIDER_URL") {
            self.archiver.provider_url = v;
        }
        if let Some(v) = get("ULTRAMARINE_ARCHIVER_UPLOAD_PATH") {
            self.archiver.upload_path = Some(v);
        }
        if let Some(v) = get("ULTRAMARINE_ARCHIVER_PROVIDER_ID") {
            self.archiver.provider_id = v;
        }
        if let Some(v) = get("ULTRAMARINE_ARCHIVER_BEARER_TOKEN") {
            // Do not log token value.
            self.archiver.bearer_token = Some(v);
        }
        if let Some(v) = get("ULTRAMARINE_ARCHIVER_RETRY_ATTEMPTS") {
            match v.parse::<u32>() {
                Ok(n) => self.archiver.retry_attempts = n.max(1),
                Err(_) => {
                    warn!(value = %v, "Invalid ULTRAMARINE_ARCHIVER_RETRY_ATTEMPTS, ignoring")
                }
            }
        }
        if let Some(v) = get("ULTRAMARINE_ARCHIVER_RETRY_BACKOFF_MS") {
            match v.parse::<u64>() {
                Ok(n) => self.archiver.retry_backoff_ms = n.max(1),
                Err(_) => {
                    warn!(value = %v, "Invalid ULTRAMARINE_ARCHIVER_RETRY_BACKOFF_MS, ignoring")
                }
            }
        }
        if let Some(v) = get("ULTRAMARINE_ARCHIVER_MAX_QUEUE_SIZE") {
            match v.parse::<usize>() {
                Ok(n) => self.archiver.max_queue_size = n.max(1),
                Err(_) => {
                    warn!(value = %v, "Invalid ULTRAMARINE_ARCHIVER_MAX_QUEUE_SIZE, ignoring")
                }
            }
        }
    }
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
            archiver: ArchiverConfig::default(),
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
