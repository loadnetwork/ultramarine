//! Metrics for the archive/prune pipeline (Phase 6).
//!
//! These metrics track the archiver worker lifecycle, notice propagation,
//! and pruning operations as specified in PHASE6_ARCHIVE_PRUNE_FINAL.md.

use std::{ops::Deref, sync::Arc, time::Duration};

use malachitebft_app_channel::app::metrics::{
    SharedRegistry,
    prometheus::metrics::{
        counter::Counter,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
};

/// Metrics for archive/prune operations.
///
/// These are registered with the prefix `archiver_` to distinguish from
/// blob_engine lifecycle metrics.
#[derive(Clone, Debug)]
pub struct ArchiveMetrics(Arc<Inner>);

impl Deref for ArchiveMetrics {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct Inner {
    // === Counters ===
    /// Total successful archive jobs
    pub jobs_success_total: Counter,
    /// Total failed archive jobs
    pub jobs_failure_total: Counter,
    /// Total upload failures to archive provider
    pub upload_failures_total: Counter,
    /// Archive notices with mismatched receipts (commitment/hash mismatch)
    pub receipt_mismatch_total: Counter,
    /// Total blobs pruned after archival
    pub pruned_total: Counter,
    /// Total blobs served directly from local storage
    pub served_total: Counter,
    /// Total blob requests that hit pruned/archived status
    pub served_archived_total: Counter,

    // === Gauges ===
    /// Current length of the archiver job queue
    pub queue_len: Gauge,
    /// Oldest height with pending archive jobs (0 if none)
    pub backlog_height: Gauge,
    /// Cumulative bytes archived (archived but not necessarily pruned yet)
    pub archived_bytes: Gauge,
    /// Cumulative bytes pruned from local storage
    pub pruned_bytes: Gauge,

    // === Histograms ===
    /// Duration of uploads to archive provider (seconds)
    pub upload_duration: Histogram,
    /// Time from notice emission to peer acknowledgment (seconds)
    pub notice_propagation: Histogram,
}

impl Inner {
    pub fn new() -> Self {
        Self {
            jobs_success_total: Counter::default(),
            jobs_failure_total: Counter::default(),
            upload_failures_total: Counter::default(),
            receipt_mismatch_total: Counter::default(),
            pruned_total: Counter::default(),
            served_total: Counter::default(),
            served_archived_total: Counter::default(),

            queue_len: Gauge::default(),
            backlog_height: Gauge::default(),
            archived_bytes: Gauge::default(),
            pruned_bytes: Gauge::default(),

            // Upload duration: 10ms to 60s with exponential buckets
            upload_duration: Histogram::new(exponential_buckets(0.01, 2.0, 12)),
            // Notice propagation: 1ms to 30s with exponential buckets
            notice_propagation: Histogram::new(exponential_buckets(0.001, 2.0, 15)),
        }
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchiveMetrics {
    /// Create a new unregistered metrics instance.
    pub fn new() -> Self {
        Self(Arc::new(Inner::new()))
    }

    /// Register metrics with the given registry under prefix `archiver_`.
    pub fn register(registry: &SharedRegistry) -> Self {
        let metrics = Self::new();

        registry.with_prefix("archiver", |registry| {
            // Counters
            registry.register(
                "jobs_success",
                "Total successful archive jobs",
                metrics.jobs_success_total.clone(),
            );

            registry.register(
                "jobs_failure",
                "Total failed archive jobs",
                metrics.jobs_failure_total.clone(),
            );

            registry.register(
                "upload_failures",
                "Total upload failures to archive provider",
                metrics.upload_failures_total.clone(),
            );

            registry.register(
                "receipt_mismatch",
                "Archive notices with mismatched commitment/hash",
                metrics.receipt_mismatch_total.clone(),
            );

            registry.register(
                "pruned",
                "Total blobs pruned after archival",
                metrics.pruned_total.clone(),
            );
            registry.register(
                "served",
                "Total blobs served directly from local storage",
                metrics.served_total.clone(),
            );
            registry.register(
                "served_archived",
                "Total blob requests hitting archived/unavailable",
                metrics.served_archived_total.clone(),
            );

            // Gauges
            registry.register(
                "queue_len",
                "Current archiver job queue length",
                metrics.queue_len.clone(),
            );

            registry.register(
                "backlog_height",
                "Oldest height with pending archive jobs",
                metrics.backlog_height.clone(),
            );

            registry.register(
                "archived_bytes",
                "Cumulative bytes archived",
                metrics.archived_bytes.clone(),
            );

            registry.register(
                "pruned_bytes",
                "Cumulative bytes pruned from local storage",
                metrics.pruned_bytes.clone(),
            );

            // Histograms
            registry.register(
                "upload_duration_seconds",
                "Duration of uploads to archive provider",
                metrics.upload_duration.clone(),
            );

            registry.register(
                "notice_propagation_seconds",
                "Time from notice emission to peer acknowledgment",
                metrics.notice_propagation.clone(),
            );

            // Force initial exposure so Grafana panels don’t show “no data”
            metrics.served_total.inc_by(0);
            metrics.served_archived_total.inc_by(0);
        });

        metrics
    }

    // ===== Helper Methods for Instrumentation =====

    /// Record a successful archive job.
    pub fn record_job_success(&self) {
        self.jobs_success_total.inc();
    }

    /// Record a failed archive job.
    pub fn record_job_failure(&self) {
        self.jobs_failure_total.inc();
    }

    /// Record an upload failure.
    pub fn record_upload_failure(&self) {
        self.upload_failures_total.inc();
    }

    /// Record a receipt/commitment mismatch.
    pub fn record_receipt_mismatch(&self) {
        self.receipt_mismatch_total.inc();
    }

    /// Record blobs pruned.
    pub fn record_pruned(&self, blob_count: usize, bytes: usize) {
        self.pruned_total.inc_by(blob_count as u64);
        self.pruned_bytes.inc_by(bytes as i64);
    }

    /// Update the archive job queue length.
    pub fn set_queue_len(&self, len: usize) {
        self.queue_len.set(len as i64);
    }

    /// Update the backlog height (oldest pending height).
    pub fn set_backlog_height(&self, height: u64) {
        self.backlog_height.set(height as i64);
    }

    /// Record bytes archived (not yet pruned).
    pub fn add_archived_bytes(&self, bytes: usize) {
        self.archived_bytes.inc_by(bytes as i64);
    }

    /// Record upload duration.
    pub fn observe_upload_duration(&self, duration: Duration) {
        self.upload_duration.observe(duration.as_secs_f64());
    }

    /// Record notice propagation time.
    pub fn observe_notice_propagation(&self, duration: Duration) {
        self.notice_propagation.observe(duration.as_secs_f64());
    }

    /// Record blobs served directly from local storage.
    pub fn record_served(&self, blob_count: usize) {
        if blob_count > 0 {
            self.served_total.inc_by(blob_count as u64);
        }
    }

    /// Record requests that hit archived/pruned status.
    pub fn record_served_archived(&self, blob_count: usize) {
        self.served_archived_total.inc_by(blob_count.max(1) as u64);
    }
}

impl Default for ArchiveMetrics {
    fn default() -> Self {
        Self::new()
    }
}
