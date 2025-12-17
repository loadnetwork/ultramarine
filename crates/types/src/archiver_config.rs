//! Configuration for the archiver worker (Phase 6 archive/prune).
//!
//! This config is in the types crate to avoid circular dependencies
//! between cli, consensus, and node crates.

use serde::{Deserialize, Serialize};

/// Configuration for the archiver worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchiverConfig {
    /// Whether archiving is enabled.
    pub enabled: bool,

    /// Provider URL for uploads (e.g., <https://s3-node-1.load.network>).
    pub provider_url: String,

    /// Upload path appended to `provider_url` for blob archival.
    ///
    /// Defaults to `/upload`.
    /// Set this only when your deployment mounts the route under a prefix.
    #[serde(default)]
    pub upload_path: Option<String>,

    /// Provider identifier used in archive notices.
    pub provider_id: String,

    /// Optional bearer token for authenticated uploads.
    #[serde(default)]
    pub bearer_token: Option<String>,

    /// Number of retry attempts for failed uploads.
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,

    /// Base backoff duration in milliseconds for retries.
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,

    /// Maximum jobs in the queue before dropping oldest.
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
}

impl ArchiverConfig {
    /// The effective upload path used for requests.
    pub fn effective_upload_path(&self) -> &str {
        match self.upload_path.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => v,
            None => "/upload",
        }
    }
}

fn default_retry_attempts() -> u32 {
    3
}

fn default_retry_backoff_ms() -> u64 {
    1000
}

fn default_max_queue_size() -> usize {
    1000
}

impl Default for ArchiverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Production default: real Load S3 Agent endpoint.
            provider_url: "https://load-s3-agent.load.network".to_string(),
            upload_path: None,
            provider_id: "load-s3-agent".to_string(),
            bearer_token: None,
            retry_attempts: default_retry_attempts(),
            retry_backoff_ms: default_retry_backoff_ms(),
            max_queue_size: default_max_queue_size(),
        }
    }
}
