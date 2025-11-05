use std::{ops::Deref, sync::Arc, time::Duration};

use malachitebft_app_channel::app::metrics::{
    SharedRegistry,
    prometheus::metrics::{
        counter::Counter,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
};

#[derive(Clone, Debug)]
pub struct BlobEngineMetrics(Arc<Inner>);

impl Deref for BlobEngineMetrics {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct Inner {
    // Verification metrics
    verifications_success: Counter,
    verifications_failure: Counter,
    verification_time: Histogram,

    // Storage metrics (gauges)
    storage_bytes_undecided: Gauge,
    storage_bytes_decided: Gauge,
    undecided_blob_count: Gauge,
    blobs_per_block: Gauge,

    // Lifecycle metrics (counters)
    lifecycle_promoted: Counter,
    lifecycle_dropped: Counter,
    lifecycle_pruned: Counter,

    // Restream/Sync metrics (counters)
    restream_rebuilds: Counter,
    sync_failures: Counter,
}

impl Inner {
    pub fn new() -> Self {
        Self {
            verifications_success: Counter::default(),
            verifications_failure: Counter::default(),
            verification_time: Histogram::new(exponential_buckets(0.001, 2.0, 10)),

            storage_bytes_undecided: Gauge::default(),
            storage_bytes_decided: Gauge::default(),
            undecided_blob_count: Gauge::default(),
            blobs_per_block: Gauge::default(),

            lifecycle_promoted: Counter::default(),
            lifecycle_dropped: Counter::default(),
            lifecycle_pruned: Counter::default(),

            restream_rebuilds: Counter::default(),
            sync_failures: Counter::default(),
        }
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self::new()
    }
}

impl BlobEngineMetrics {
    pub fn new() -> Self {
        Self(Arc::new(Inner::new()))
    }

    /// Capture a snapshot of the current metric values.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            verifications_success: self.verifications_success.get(),
            verifications_failure: self.verifications_failure.get(),
            storage_bytes_undecided: self.storage_bytes_undecided.get(),
            storage_bytes_decided: self.storage_bytes_decided.get(),
            undecided_blob_count: self.undecided_blob_count.get(),
            blobs_per_block: self.blobs_per_block.get(),
            lifecycle_promoted: self.lifecycle_promoted.get(),
            lifecycle_dropped: self.lifecycle_dropped.get(),
            lifecycle_pruned: self.lifecycle_pruned.get(),
            restream_rebuilds: self.restream_rebuilds.get(),
            sync_failures: self.sync_failures.get(),
        }
    }

    pub fn register(registry: &SharedRegistry) -> Self {
        let metrics = Self::new();

        registry.with_prefix("blob_engine", |registry| {
            // Verification metrics
            registry.register(
                "verifications_success_total",
                "Successful blob KZG proof verifications",
                metrics.verifications_success.clone(),
            );

            registry.register(
                "verifications_failure_total",
                "Failed blob KZG proof verifications",
                metrics.verifications_failure.clone(),
            );

            registry.register(
                "verification_time",
                "Time taken to verify blob KZG proofs (seconds)",
                metrics.verification_time.clone(),
            );

            // Storage metrics
            registry.register(
                "storage_bytes_undecided",
                "Storage size of undecided blobs (bytes)",
                metrics.storage_bytes_undecided.clone(),
            );

            registry.register(
                "storage_bytes_decided",
                "Storage size of decided blobs (bytes)",
                metrics.storage_bytes_decided.clone(),
            );

            registry.register(
                "undecided_blob_count",
                "Current number of undecided blobs",
                metrics.undecided_blob_count.clone(),
            );

            registry.register(
                "blobs_per_block",
                "Number of blobs in last finalized block",
                metrics.blobs_per_block.clone(),
            );

            // Lifecycle metrics
            registry.register(
                "lifecycle_promoted_total",
                "Blobs promoted to decided state",
                metrics.lifecycle_promoted.clone(),
            );

            registry.register(
                "lifecycle_dropped_total",
                "Blobs dropped from undecided state",
                metrics.lifecycle_dropped.clone(),
            );

            registry.register(
                "lifecycle_pruned_total",
                "Decided blobs pruned/archived",
                metrics.lifecycle_pruned.clone(),
            );

            // Restream/Sync metrics
            registry.register(
                "restream_rebuilds_total",
                "Blob metadata rebuilds during restream",
                metrics.restream_rebuilds.clone(),
            );

            registry.register(
                "sync_failures_total",
                "Blob sync/fetch failures",
                metrics.sync_failures.clone(),
            );
        });

        metrics
    }

    // ===== Helper Methods for Instrumentation =====

    /// Record successful verification batch
    pub fn record_verifications_success(&self, count: usize) {
        self.verifications_success.inc_by(count as u64);
    }

    /// Record failed verification batch
    pub fn record_verifications_failure(&self, count: usize) {
        self.verifications_failure.inc_by(count as u64);
    }

    /// Record verification duration
    pub fn observe_verification_time(&self, duration: Duration) {
        self.verification_time.observe(duration.as_secs_f64());
    }

    /// Add undecided blob storage (when storing new blobs)
    pub fn add_undecided_storage(&self, bytes: usize, blob_count: usize) {
        // Bulk increment by bytes
        self.storage_bytes_undecided.inc_by(bytes as i64);
        // Bulk increment blob count
        self.undecided_blob_count.inc_by(blob_count as i64);
    }

    /// Move blobs from undecided to decided
    pub fn promote_blobs(&self, bytes: usize, blob_count: usize) {
        // Move bytes from undecided to decided
        self.storage_bytes_undecided.dec_by(bytes as i64);
        self.storage_bytes_decided.inc_by(bytes as i64);
        // Update blob counts
        self.undecided_blob_count.dec_by(blob_count as i64);
        self.lifecycle_promoted.inc_by(blob_count as u64);
    }

    /// Drop undecided blobs
    pub fn drop_blobs(&self, bytes: usize, blob_count: usize) {
        // Bulk decrement undecided storage
        self.storage_bytes_undecided.dec_by(bytes as i64);
        // Bulk decrement undecided count
        self.undecided_blob_count.dec_by(blob_count as i64);
        self.lifecycle_dropped.inc_by(blob_count as u64);
    }

    /// Prune decided blobs
    pub fn prune_blobs(&self, bytes: usize, blob_count: usize) {
        // Bulk decrement decided storage
        self.storage_bytes_decided.dec_by(bytes as i64);
        self.lifecycle_pruned.inc_by(blob_count as u64);
    }

    /// Set blobs per finalized block
    pub fn set_blobs_per_block(&self, count: usize) {
        self.blobs_per_block.set(count as i64);
    }

    /// Record restream rebuild
    pub fn record_restream_rebuild(&self) {
        self.restream_rebuilds.inc();
    }

    /// Record sync failure
    pub fn record_sync_failure(&self) {
        self.sync_failures.inc();
    }
}

impl Default for BlobEngineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Immutable snapshot of blob engine metric counters and gauges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Number of blobs successfully verified.
    pub verifications_success: u64,
    /// Number of blob verification failures.
    pub verifications_failure: u64,
    /// Bytes currently held in undecided storage.
    pub storage_bytes_undecided: i64,
    /// Bytes currently held in decided storage.
    pub storage_bytes_decided: i64,
    /// Count of blobs pending decision.
    pub undecided_blob_count: i64,
    /// Blob count recorded for the most recent finalized block.
    pub blobs_per_block: i64,
    /// Total blobs promoted to decided state.
    pub lifecycle_promoted: u64,
    /// Total blobs dropped from undecided state.
    pub lifecycle_dropped: u64,
    /// Total blobs pruned or archived.
    pub lifecycle_pruned: u64,
    /// Total restream rebuild operations performed.
    pub restream_rebuilds: u64,
    /// Total sync failures recorded during blob processing.
    pub sync_failures: u64,
}
