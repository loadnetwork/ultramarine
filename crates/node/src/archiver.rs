//! Background archiver worker for Phase 6 archive/prune.
//!
//! The archiver runs as a background task, receiving archive jobs from the consensus
//! state and uploading blobs to a storage provider. It generates signed archive notices
//! that are broadcast to peers for verification.
//!
//! ## Upload Modes
//!
//! - **Real mode (default)**: Uses HTTP POST to the configured provider URL with retry/backoff.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, hex};
use malachitebft_app_channel::app::types::core::Round;
use reqwest::{Client, header::AUTHORIZATION, multipart};
use serde::Deserialize;
use tokio::sync::mpsc;
use ultramarine_blob_engine::BlobEngine;
use ultramarine_consensus::archive_metrics::ArchiveMetrics;
// Re-export types for convenience
pub use ultramarine_types::archive::ArchiveJob;
pub use ultramarine_types::archiver_config::ArchiverConfig;
use ultramarine_types::{
    address::Address,
    archive::{ArchiveNotice, ArchiveNoticeBody},
    blob::{BYTES_PER_BLOB, KzgCommitment},
    height::Height,
    signing::Ed25519Provider,
};

/// Channel sender for submitting archive jobs to the worker queue.
pub type ArchiveJobSender = mpsc::Sender<ArchiveJob>;

/// Channel receiver for the archiver worker.
pub type ArchiveJobReceiver = mpsc::Receiver<ArchiveJob>;

/// Non-blocking submission channel used by the app loop.
pub type ArchiveJobSubmitter = mpsc::UnboundedSender<ArchiveJob>;

/// Handle to the archiver worker, including the job channel.
#[derive(Debug)]
pub struct ArchiverHandle {
    /// Sender for submitting archive jobs.
    pub job_tx: ArchiveJobSender,
    /// Join handle for the worker task.
    pub handle: tokio::task::JoinHandle<()>,
}

impl ArchiverHandle {
    /// Submit an archive job to the worker.
    pub async fn submit(&self, job: ArchiveJob) -> Result<(), mpsc::error::SendError<ArchiveJob>> {
        self.job_tx.send(job).await
    }

    /// Abort the worker task.
    pub fn abort(&self) {
        self.handle.abort();
    }
}

/// Channel for sending generated archive notices back to the state.
pub type ArchiveNoticeSender = mpsc::Sender<ArchiveNotice>;

/// Entry in the retry queue for failed jobs.
struct RetryEntry {
    /// The job to retry.
    job: ArchiveJob,
    /// Number of attempts so far (1 = first retry).
    attempts: u32,
    /// When to retry this job.
    retry_at: Instant,
}

/// The archiver worker that processes archive jobs in the background.
///
/// Generic over `E: BlobEngine` to allow fetching blob data for uploads.
pub struct ArchiverWorker<E: BlobEngine> {
    config: ArchiverConfig,
    job_rx: ArchiveJobReceiver,
    notice_tx: ArchiveNoticeSender,
    signer: Ed25519Provider,
    validator_address: Address,
    metrics: ArchiveMetrics,
    /// HTTP client for real uploads (reused for connection pooling)
    http_client: Client,
    /// Blob engine for fetching blob data to upload
    blob_engine: Arc<E>,
}

impl<E: BlobEngine + 'static> ArchiverWorker<E> {
    /// Create a new archiver worker.
    pub fn new(
        config: ArchiverConfig,
        job_rx: ArchiveJobReceiver,
        notice_tx: ArchiveNoticeSender,
        signer: Ed25519Provider,
        validator_address: Address,
        metrics: ArchiveMetrics,
        blob_engine: Arc<E>,
    ) -> Self {
        // Build HTTP client with reasonable timeouts
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            config,
            job_rx,
            notice_tx,
            signer,
            validator_address,
            metrics,
            http_client,
            blob_engine,
        }
    }

    /// Maximum retry backoff delay for failed jobs.
    const MAX_RETRY_BACKOFF_MS: u64 = 60_000;

    /// Run the worker, processing jobs until the channel is closed.
    ///
    /// Jobs that fail are requeued with exponential backoff. This ensures that
    /// transient failures (network issues, provider unavailable) don't cause
    /// permanent data loss. If a job fails after `archiver.retry_attempts`
    /// attempts, it is logged as a permanent failure but kept in the retry
    /// queue for manual intervention (pruning will wait indefinitely for the
    /// archive notice).
    pub async fn run(mut self) {
        tracing::info!(
            provider_id = %self.config.provider_id,
            provider_url = %self.config.provider_url,
            "Archiver worker started"
        );

        // Retry queue: jobs that failed and are waiting to be retried
        let mut retry_queue: VecDeque<RetryEntry> = VecDeque::new();

        loop {
            // Check for due retries first
            let now = Instant::now();
            while let Some(entry) = retry_queue.front() {
                if entry.retry_at <= now {
                    let entry = retry_queue.pop_front().unwrap();
                    self.process_with_retry(entry.job, entry.attempts, &mut retry_queue).await;
                } else {
                    break;
                }
            }

            // Calculate timeout for next retry (if any)
            let retry_timeout =
                retry_queue.front().map(|e| e.retry_at.saturating_duration_since(now));

            // Wait for new job or retry timeout
            let job = match retry_timeout {
                Some(timeout) if !timeout.is_zero() => {
                    tokio::select! {
                        job = self.job_rx.recv() => job,
                        _ = tokio::time::sleep(timeout) => continue,
                    }
                }
                _ => self.job_rx.recv().await,
            };

            match job {
                Some(job) => {
                    self.metrics.set_queue_len(self.job_rx.len() + retry_queue.len());
                    self.metrics.set_backlog_height(job.height.as_u64());
                    self.process_with_retry(job, 0, &mut retry_queue).await;
                }
                None => {
                    // Channel closed - but don't exit if we have pending retries
                    if retry_queue.is_empty() {
                        break;
                    }
                    // Process remaining retries before exiting
                    tracing::info!(
                        pending_retries = retry_queue.len(),
                        "Channel closed, processing remaining retries"
                    );
                }
            }

            if self.job_rx.is_empty() && retry_queue.is_empty() {
                self.metrics.set_backlog_height(0);
            }
        }

        self.metrics.set_queue_len(0);
        self.metrics.set_backlog_height(0);
        tracing::info!("Archiver worker stopped");
    }

    /// Process a job and handle retry on failure.
    async fn process_with_retry(
        &mut self,
        job: ArchiveJob,
        attempts: u32,
        retry_queue: &mut VecDeque<RetryEntry>,
    ) {
        let height = job.height;
        match self.process_job(job.clone()).await {
            Ok(()) => {
                self.metrics.record_job_success();
                if attempts > 0 {
                    tracing::info!(
                        height = %height,
                        attempts = attempts + 1,
                        "Archive job succeeded after retries"
                    );
                }
            }
            Err(e) => {
                let next_attempts = attempts + 1;
                self.metrics.record_job_failure();

                let warn_threshold = self.config.retry_attempts.max(1);
                if next_attempts >= warn_threshold {
                    tracing::error!(
                        height = %height,
                        attempts = next_attempts,
                        warn_threshold = warn_threshold,
                        error = %e,
                        "Archive job failed permanently after max retries. \
                         Blobs at this height will NOT be pruned until manually resolved. \
                         Job will continue retrying at max backoff interval."
                    );
                } else {
                    tracing::warn!(
                        height = %height,
                        attempt = next_attempts,
                        warn_threshold = warn_threshold,
                        error = %e,
                        "Archive job failed, scheduling retry"
                    );
                }

                // Calculate backoff with exponential growth, capped at MAX_RETRY_BACKOFF_MS.
                // The base backoff is configured via `archiver.retry_backoff_ms`.
                let base_backoff_ms = self.config.retry_backoff_ms.max(1);
                let backoff_ms = base_backoff_ms
                    .saturating_mul(2u64.saturating_pow(attempts.min(16)))
                    .min(Self::MAX_RETRY_BACKOFF_MS);

                retry_queue.push_back(RetryEntry {
                    job,
                    attempts: next_attempts,
                    retry_at: Instant::now() + Duration::from_millis(backoff_ms),
                });
            }
        }
    }

    /// Process a single archive job.
    ///
    /// Fetches blob data from the blob engine, uploads each blob, and generates
    /// signed archive notices. Fails fast if any required data is missing.
    async fn process_job(&mut self, job: ArchiveJob) -> color_eyre::Result<()> {
        use color_eyre::eyre::eyre;

        let start = std::time::Instant::now();

        tracing::debug!(
            height = %job.height,
            blob_count = job.blob_indices.len(),
            "Processing archive job"
        );

        // Validate job has required data - fail fast, don't zero-fill
        if job.commitments.len() != job.blob_indices.len() {
            return Err(eyre!(
                "Archive job for height {} has mismatched commitments: expected {}, got {}",
                job.height,
                job.blob_indices.len(),
                job.commitments.len()
            ));
        }
        if job.blob_keccaks.len() != job.blob_indices.len() {
            return Err(eyre!(
                "Archive job for height {} has mismatched blob_keccaks: expected {}, got {}",
                job.height,
                job.blob_indices.len(),
                job.blob_keccaks.len()
            ));
        }
        if job.versioned_hashes.len() != job.blob_indices.len() {
            return Err(eyre!(
                "Archive job for height {} has mismatched versioned hashes: expected {}, got {}",
                job.height,
                job.blob_indices.len(),
                job.versioned_hashes.len()
            ));
        }

        // Fetch blob sidecars from blob engine
        let sidecars = self
            .blob_engine
            .get_for_import(job.height)
            .await
            .map_err(|e| eyre!("Failed to fetch blobs for height {}: {}", job.height, e))?;

        if sidecars.is_empty() {
            return Err(eyre!(
                "No blobs found in blob engine for height {} - cannot archive",
                job.height
            ));
        }

        let archived_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // For each blob in the job, upload and create a notice
        for (i, &blob_idx) in job.blob_indices.iter().enumerate() {
            // Get commitment and keccak from job (already validated above)
            let commitment = job.commitments[i].clone();
            let blob_keccak = job.blob_keccaks[i];
            let versioned_hash = job.versioned_hashes[i];

            // Find the sidecar for this blob index
            let sidecar = sidecars.iter().find(|s| s.index == blob_idx).ok_or_else(|| {
                eyre!("Blob index {} not found in sidecars for height {}", blob_idx, job.height)
            })?;

            // Extract blob data for upload
            let blob_data = sidecar.blob.data().as_ref();

            // Upload blob with retry/backoff
            let upload_start = std::time::Instant::now();
            let upload_result = self
                .upload_blob(
                    job.height,
                    job.round,
                    blob_idx,
                    blob_data,
                    &commitment,
                    versioned_hash,
                    blob_keccak,
                )
                .await;
            let upload_elapsed = upload_start.elapsed();
            self.metrics.observe_upload_blob_duration(upload_elapsed);

            let locator = match upload_result {
                Ok(locator) => {
                    self.metrics.record_upload_success(blob_data.len());
                    locator
                }
                Err(err) => {
                    self.metrics.record_upload_failure();
                    return Err(err);
                }
            };

            tracing::info!(
                height = %job.height,
                round = %job.round,
                index = %blob_idx,
                provider_id = %self.config.provider_id,
                locator = %locator,
                "📦 Blob archived to provider"
            );

            // Create archive notice body
            let body = ArchiveNoticeBody {
                height: job.height,
                round: job.round,
                blob_index: blob_idx,
                kzg_commitment: commitment,
                blob_keccak,
                provider_id: self.config.provider_id.clone(),
                locator,
                archived_by: self.validator_address.clone(),
                archived_at,
            };

            // Sign the notice
            let notice = ArchiveNotice::sign(body, &self.signer);

            // Send notice back to state for handling and broadcast.
            // Treat delivery failures as fatal so the job is retried instead of silently
            // succeeding.
            self.notice_tx.send(notice).await.map_err(|e| {
                tracing::error!(
                    height = %job.height,
                    index = %blob_idx,
                    error = %e,
                    "Failed to deliver archive notice to state"
                );
                color_eyre::eyre::eyre!(
                    "archiver worker could not deliver notice for height {} index {}",
                    job.height,
                    blob_idx
                )
            })?;

            // Track archived bytes
            self.metrics.add_archived_bytes(BYTES_PER_BLOB);
        }

        let duration = start.elapsed();
        self.metrics.observe_upload_duration(duration);

        tracing::debug!(
            height = %job.height,
            duration_ms = duration.as_millis(),
            "Archive job completed"
        );

        Ok(())
    }

    /// Upload a blob with retry and exponential backoff.
    ///
    /// `mock://` provider URLs are supported by tests only.
    async fn upload_blob(
        &self,
        height: Height,
        round: Round,
        blob_idx: u16,
        blob_data: &[u8],
        commitment: &KzgCommitment,
        versioned_hash: B256,
        blob_keccak: B256,
    ) -> color_eyre::Result<String> {
        if self.config.provider_url.starts_with("mock://") {
            #[cfg(test)]
            {
                return self.mock_upload(height, blob_idx).await;
            }
            #[cfg(not(test))]
            {
                return Err(color_eyre::eyre::eyre!(
                    "mock:// provider_url is only allowed in tests"
                ));
            }
        }

        self.do_upload(height, round, blob_idx, blob_data, commitment, versioned_hash, blob_keccak)
            .await
    }

    /// Perform the actual HTTP POST to the storage provider.
    ///
    /// Returns the locator string reported by the provider. The request
    /// includes blob metadata (height/index/commitment/hash) via headers.
    async fn do_upload(
        &self,
        height: Height,
        round: Round,
        blob_idx: u16,
        blob_data: &[u8],
        commitment: &KzgCommitment,
        versioned_hash: B256,
        blob_keccak: B256,
    ) -> color_eyre::Result<String> {
        use color_eyre::eyre::bail;

        let base = self.config.provider_url.trim_end_matches('/');
        let path = self.config.effective_upload_path().trim_start_matches('/');
        let url = format!("{base}/{path}");

        let proposer_hex = format!("0x{}", hex::encode(self.validator_address.into_inner()));
        let commitment_hex = format!("0x{}", hex::encode(commitment.as_bytes()));
        let versioned_hash_hex = format!("0x{}", hex::encode(versioned_hash.as_slice()));
        let blob_keccak_hex = format!("0x{}", hex::encode(blob_keccak.as_slice()));

        #[derive(serde::Serialize)]
        struct Tag<'a> {
            key: &'a str,
            value: String,
        }

        let tags = vec![
            Tag { key: "load", value: "true".to_string() },
            Tag { key: "load.network", value: "fibernet".to_string() },
            Tag { key: "load.kind", value: "blob".to_string() },
            Tag { key: "load.height", value: height.as_u64().to_string() },
            Tag { key: "load.round", value: round.as_i64().to_string() },
            Tag { key: "load.blob_index", value: blob_idx.to_string() },
            Tag { key: "load.kzg_commitment", value: commitment_hex.clone() },
            Tag { key: "load.versioned_hash", value: versioned_hash_hex.clone() },
            Tag { key: "load.blob_keccak", value: blob_keccak_hex.clone() },
            Tag { key: "load.proposer", value: proposer_hex.clone() },
            Tag { key: "load.provider", value: self.config.provider_id.clone() },
        ];

        let tags_json = serde_json::to_string(&tags)
            .map_err(|e| color_eyre::eyre::eyre!("Failed to serialize tags JSON: {e}"))?;

        let file_part = multipart::Part::bytes(blob_data.to_vec())
            .file_name(format!("blob-{}-{}.bin", height.as_u64(), blob_idx))
            .mime_str("application/octet-stream")
            .map_err(|e| color_eyre::eyre::eyre!("Invalid mime for upload multipart: {e}"))?;

        let form = multipart::Form::new()
            .part("file", file_part)
            .text("content_type", "application/octet-stream")
            .text("tags", tags_json);

        let mut request = self.http_client.post(url).multipart(form);

        if let Some(token) = &self.config.bearer_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request.send().await.map_err(|e| {
            color_eyre::eyre::eyre!(
                "Failed to upload blob {} at height {}: {}",
                blob_idx,
                height,
                e
            )
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status.as_u16() == 404 || status.as_u16() == 405 {
                bail!(
                    "Provider {} rejected upload at path '{}' (status {}): {}. \
                     Check archiver.provider_url and archiver.upload_path / ULTRAMARINE_ARCHIVER_UPLOAD_PATH.",
                    self.config.provider_url,
                    self.config.effective_upload_path(),
                    status,
                    body
                );
            }
            bail!(
                "Provider {} rejected upload (status {}): {}",
                self.config.provider_url,
                status,
                body
            );
        }

        let bytes = response.bytes().await.map_err(|e| {
            color_eyre::eyre::eyre!(
                "Failed to read provider response for blob {} at height {}: {}",
                blob_idx,
                height,
                e
            )
        })?;

        if bytes.is_empty() {
            bail!("Provider {} returned an empty response", self.config.provider_url);
        }

        #[derive(Debug, Deserialize)]
        struct UploadResponse {
            /// Whether the provider accepted and stored the blob.
            success: Option<bool>,
            /// Explicit locator string (preferred).
            locator: Option<String>,
            /// ANS-104 id (Load S3 Agent).
            #[serde(alias = "dataitem_id")]
            dataitem_id: Option<String>,
            /// Human message.
            message: Option<String>,
        }

        let parsed = serde_json::from_slice::<UploadResponse>(&bytes).map_err(|e| {
            color_eyre::eyre::eyre!(
                "Provider {} response was not valid JSON ({e}). Raw body: {}",
                self.config.provider_url,
                String::from_utf8_lossy(&bytes)
            )
        })?;

        if let Some(false) = parsed.success {
            return Err(color_eyre::eyre::eyre!(
                "Provider {} reported success=false: {:?}",
                self.config.provider_url,
                parsed.message
            ));
        }

        let locator = if let Some(loc) = parsed.locator {
            loc
        } else if let Some(id) = parsed.dataitem_id {
            if id.starts_with("load-s3://") { id } else { format!("load-s3://{id}") }
        } else {
            return Err(color_eyre::eyre::eyre!(
                "Provider {} response did not include a locator or dataitem_id",
                self.config.provider_url
            ));
        };

        if locator.trim().is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "Provider {} returned an empty locator",
                self.config.provider_url
            ));
        }

        Ok(locator)
    }

    /// Mock upload that returns a fake locator (tests only).
    #[cfg(test)]
    async fn mock_upload(&self, height: Height, blob_idx: u16) -> color_eyre::Result<String> {
        // Simulate upload (mock always succeeds after simulated delay)
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Mock locator format: {provider_id}://{height}/{blob_idx}
        let locator = format!("{}://{}/{}", self.config.provider_id, height.as_u64(), blob_idx);

        tracing::trace!(
            height = %height,
            blob_idx,
            locator = %locator,
            "Mock upload completed"
        );

        Ok(locator)
    }
}

/// Spawn the archiver worker as a background task.
///
/// Returns the handle with the job sender channel and the task join handle.
///
/// # Type Parameters
///
/// * `E` - The blob engine type, must implement `BlobEngine + Send + 'static`
pub fn spawn_archiver<E>(
    config: ArchiverConfig,
    notice_tx: ArchiveNoticeSender,
    signer: Ed25519Provider,
    validator_address: Address,
    metrics: ArchiveMetrics,
    blob_engine: Arc<E>,
) -> ArchiverHandle
where
    E: BlobEngine + Send + 'static,
{
    let (job_tx, job_rx) = mpsc::channel(config.max_queue_size);

    let worker = ArchiverWorker::new(
        config,
        job_rx,
        notice_tx,
        signer,
        validator_address,
        metrics,
        blob_engine,
    );

    let handle = tokio::spawn(worker.run());

    ArchiverHandle { job_tx, handle }
}
