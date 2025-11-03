# Blob Engine Metrics - Implementation Progress

**Date**: 2025-11-03
**Status**: 🟢 Ready to Implement
**Phase**: Phase 5A - Metrics Instrumentation
**Tracking**: Implementation of blob observability metrics

**📋 OFFICIAL IMPLEMENTATION PLAN** - Use this document as the single source of truth

---

## Table of Contents

1. [Metric Specifications](#metric-specifications)
2. [Implementation Pattern](#implementation-pattern)
3. [Code Structure](#code-structure)
4. [Instrumentation Points](#instrumentation-points)
5. [Registration in Node](#registration-in-node)
6. [Testing Metrics](#testing-metrics)
7. [Dashboard Queries](#dashboard-queries)
8. [Future Work - Node-Level Metrics](#future-work-node-level-metrics)
9. [Progress Tracking](#progress-tracking)

---

## Metric Specifications

### Overview

**Total Metrics**: 12 (8 counters, 3 gauges, 1 histogram)
**Prefix**: `blob_engine_*`
**API**: `malachitebft-metrics` (SharedRegistry)
**Pattern**: Follow `crates/consensus/src/metrics.rs` (DbMetrics)

### Metric Table

| Name | Type | Help Text | Units | Instrumentation Point |
|------|------|-----------|-------|----------------------|
| `blob_engine_verifications_success_total` | Counter | Successful blob KZG proof verifications | count | `verify_and_store` (success path) |
| `blob_engine_verifications_failure_total` | Counter | Failed blob KZG proof verifications | count | `verify_and_store` (error path) |
| `blob_engine_verification_time` | Histogram | Time taken to verify blob KZG proofs | seconds | `verify_and_store` (timed) |
| `blob_engine_storage_bytes_undecided` | Gauge | Storage size of undecided blobs | bytes | `BlobStore::put_undecided_blobs` (+), `mark_decided`/`drop_round` (-) |
| `blob_engine_storage_bytes_decided` | Gauge | Storage size of decided blobs | bytes | `BlobStore::mark_decided` (+), `prune_archived_before` (-) |
| `blob_engine_undecided_blob_count` | Gauge | Current number of undecided blobs | count | `BlobStore::put_undecided_blobs` (+), `mark_decided`/`drop_round` (-) |
| `blob_engine_blobs_per_block` | Gauge | Number of blobs in last finalized block | count | `State::commit` (after promotion) |
| `blob_engine_lifecycle_promoted_total` | Counter | Blobs promoted to decided state | count | `mark_decided` |
| `blob_engine_lifecycle_dropped_total` | Counter | Blobs dropped from undecided state | count | `drop_round` |
| `blob_engine_lifecycle_pruned_total` | Counter | Decided blobs pruned/archived | count | `prune_archived_before` |
| `blob_engine_restream_rebuilds_total` | Counter | Blob metadata rebuilds during restream | count | `State::rebuild_blob_sidecars_for_restream` |
| `blob_engine_sync_failures_total` | Counter | Blob sync/fetch failures | count | Future sync implementation |

### Key Design Decisions

1. **No Label Variants**: Use separate metrics (e.g., `verifications_success_total` vs `verifications_failure_total`) instead of labels like `{result="success"}`.
   - **Reason**: Malachite metrics don't use `CounterVec`/`GaugeVec` - see `DbMetrics` pattern.
   - ✅ **Confirmed correct**: Follows codebase pattern exactly

2. **Moniker Context**: The `SharedRegistry::with_moniker()` adds moniker automatically.
   - **No need** to add `moniker` label in metric definitions.

3. **Histogram Buckets**: Use exponential buckets for `verification_time`.
   - **Range**: 0.001s (1ms) to ~10s with factor 2.0
   - **Reason**: Matches `db_read_time` pattern (line 67 in consensus/metrics.rs)
   - **Implementation**: Wrap verification path with a timer guard so both success and error exits enter the histogram

4. **Dependency**: `malachitebft-app-channel` ✅ **Correct Choice**
   - **Already in workspace**: Used by consensus crate
   - **Don't use**: `prometheus` crate directly (incompatible pattern)
   - **Reference**: `crates/consensus/src/metrics.rs:3-10`

5. **Gauge Management**: Helper methods handle inc/dec automatically
   - **Pattern**: `add_undecided_storage()`, `promote_blobs()`, `drop_blobs()`
   - **Benefit**: Atomic updates, no race conditions
   - **Implementation detail**: Capture serialized blob sizes in `BlobStore` to calculate deltas
   - ✅ **Correct approach**: Better than manual inc/dec in BlobEngine methods

6. **Consensus Visibility**: Surface Tendermint lifecycle alongside blob engine
   - `State::assemble_and_store_blobs` increments proposal counters (blob vs blobless)
   - `State::rebuild_blob_sidecars_for_restream` records restream rebuilds
   - `State::commit` sets `blob_engine_blobs_per_block` immediately after promotion
   - Ensures dashboards correlate blob activity with consensus height/round

---

## Implementation Pattern

### Reference: DbMetrics Structure

**File**: `crates/consensus/src/metrics.rs`

```rust
use malachitebft_app_channel::app::metrics::{
    SharedRegistry,
    prometheus::metrics::{
        counter::Counter,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
};

#[derive(Clone, Debug)]
pub struct DbMetrics(Arc<Inner>);

impl Deref for DbMetrics {
    type Target = Inner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct Inner {
    db_read_count: Counter,
    db_read_time: Histogram,
    // ...
}

impl DbMetrics {
    pub fn register(registry: &SharedRegistry) -> Self {
        let metrics = Self::new();

        registry.with_prefix("app_channel", |registry| {
            registry.register(
                "db_read_count_total",
                "Total number of reads from the database",
                metrics.db_read_count.clone(),
            );
            // ... register others
        });

        metrics
    }

    pub fn add_read_bytes(&self, bytes: u64) {
        self.db_read_count.inc();
    }
}
```

**Key Patterns**:
- ✅ `Arc<Inner>` wrapper with `Deref`
- ✅ `register()` method takes `&SharedRegistry`
- ✅ `with_prefix()` for namespace
- ✅ Helper methods for clean instrumentation

---

## Code Structure

### File: `crates/blob_engine/src/metrics.rs`

```rust
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
        self.storage_bytes_undecided.add(bytes as i64);
        self.undecided_blob_count.add(blob_count as i64);
    }

    /// Move blobs from undecided to decided
    pub fn promote_blobs(&self, bytes: usize, blob_count: usize) {
        self.storage_bytes_undecided.sub(bytes as i64);
        self.storage_bytes_decided.add(bytes as i64);
        self.undecided_blob_count.sub(blob_count as i64);
        self.lifecycle_promoted.inc_by(blob_count as u64);
    }

    /// Drop undecided blobs
    pub fn drop_blobs(&self, bytes: usize, blob_count: usize) {
        self.storage_bytes_undecided.sub(bytes as i64);
        self.undecided_blob_count.sub(blob_count as i64);
        self.lifecycle_dropped.inc_by(blob_count as u64);
    }

    /// Prune decided blobs
    pub fn prune_blobs(&self, bytes: usize, blob_count: usize) {
        self.storage_bytes_decided.sub(bytes as i64);
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
```

### File: `crates/blob_engine/src/lib.rs`

Add export:

```rust
pub mod metrics;
```

### File: `crates/blob_engine/Cargo.toml`

Add dependency:

```toml
[dependencies]
# ... existing dependencies ...

# Metrics
malachitebft-app-channel = { workspace = true }
```

---

## Instrumentation Points

### 1. Add Metrics to BlobEngineImpl

**File**: `crates/blob_engine/src/engine.rs`

```rust
use crate::metrics::BlobEngineMetrics;
use std::sync::Arc;

pub struct BlobEngineImpl<S>
where
    S: BlobStore,
{
    verifier: BlobVerifier,
    store: S,
    metrics: Option<Arc<BlobEngineMetrics>>, // NEW
}

impl<S> BlobEngineImpl<S>
where
    S: BlobStore,
{
    pub fn new(store: S) -> Result<Self, BlobEngineError> {
        Ok(Self {
            verifier: BlobVerifier::new()?,
            store,
            metrics: None,
        })
    }

    pub fn with_metrics(mut self, metrics: Arc<BlobEngineMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }
}
```

### 2. Instrument `verify_and_store`

```rust
use ultramarine_types::blob::BYTES_PER_BLOB;

async fn verify_and_store(
    &self,
    height: Height,
    round: i64,
    sidecars: &[BlobSidecar],
) -> Result<(), BlobEngineError> {
    let timer_start = std::time::Instant::now();
    let refs: Vec<&BlobSidecar> = sidecars.iter().collect();

    match self.verifier.verify_blob_sidecars_batch(&refs) {
        Ok(_) => {
            if let Some(ref m) = self.metrics {
                m.observe_verification_time(timer_start.elapsed());
                m.record_verifications_success(sidecars.len());
            }

            // Store blobs
            self.store.put_undecided_blobs(height, round, sidecars).await?;

            // Update storage metrics
            if let Some(ref m) = self.metrics {
                let total_bytes = sidecars.len() * BYTES_PER_BLOB;
                m.add_undecided_storage(total_bytes, sidecars.len());
            }

            Ok(())
        }
        Err(e) => {
            if let Some(ref m) = self.metrics {
                m.observe_verification_time(timer_start.elapsed());
                m.record_verifications_failure(sidecars.len());
            }
            Err(e)
        }
    }
}
```

> This assumes `BYTES_PER_BLOB` is imported from `ultramarine_types::blob`. No dynamic length lookup is required because blobs are fixed-size.

### 3. Instrument `mark_decided`

```rust
async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
    let blob_count = self.store.count_undecided_blobs(height, round).await?;
    let total_bytes = blob_count * BYTES_PER_BLOB;

    self.store.mark_decided(height, round).await?;

    if let Some(ref m) = self.metrics {
        m.promote_blobs(total_bytes, blob_count);
        m.set_blobs_per_block(blob_count);
    }

    Ok(())
}
```

> Add a lightweight `count_undecided_blobs` helper on the store that performs a prefix scan and counts keys without deserialising payloads.

### 4. Instrument `drop_round`

```rust
async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
    let blob_count = self.store.count_undecided_blobs(height, round).await?;
    let total_bytes = blob_count * BYTES_PER_BLOB;

    self.store.drop_round(height, round).await?;

    // Update metrics
    if let Some(ref m) = self.metrics {
        m.drop_blobs(total_bytes, blob_count);
    }

    Ok(())
}
```

### 5. Instrument `mark_archived`

```rust
async fn mark_archived(&self, height: Height, indices: &[u16]) -> Result<(), BlobEngineError> {
    self.store.delete_archived(height, indices).await?;

    if let Some(ref m) = self.metrics {
        let bytes = indices.len() * BYTES_PER_BLOB;
        m.prune_blobs(bytes, indices.len());
    }

    Ok(())
}
```

### 6. Instrument `prune_archived_before`

```rust
async fn prune_archived_before(&self, height: Height) -> Result<usize, BlobEngineError> {
    let (pruned_count, pruned_bytes) = self.store.prune_archived_before(height).await?;

    if let Some(ref m) = self.metrics {
        m.prune_blobs(pruned_bytes, pruned_count);
    }

    Ok(pruned_count)
}
```

> Update the `BlobStore` trait so `prune_archived_before` returns both the number of blobs and the total bytes pruned. For RocksDB the byte total can be derived as `count * BYTES_PER_BLOB`.

### 7. Consensus Hooks

```rust
impl State {
    pub fn with_blob_metrics(mut self, metrics: Arc<BlobEngineMetrics>) -> Self {
        self.blob_metrics = Some(metrics);
        self
    }

    fn commit(&mut self, ...) -> eyre::Result<()> {
        // existing logic ...
        if let Some(ref m) = self.blob_metrics {
            m.set_blobs_per_block(metadata.blob_count as usize);
        }
        Ok(())
    }

    fn rebuild_blob_sidecars_for_restream(&self, metadata: &BlobMetadata, ...) -> eyre::Result<Vec<BlobSidecar>> {
        let result = /* existing rebuild */;
        if let Some(ref m) = self.blob_metrics {
            m.record_restream_rebuild();
        }
        result
    }
}
```

> Pass the metrics handle into `State` when constructing it in `node.rs` so consensus can emit per-height signals (blobs per block, restream rebuilds, sync failures).

---

## Registration in Node

### File: `crates/node/src/node.rs`

**Location**: Around line 158 (after `DbMetrics::register`)

```rust
use ultramarine_blob_engine::metrics::BlobEngineMetrics;
use std::sync::Arc;

// ... inside node startup ...

// Existing code
let registry = SharedRegistry::global().with_moniker(&self.config.moniker);
let db_metrics = DbMetrics::register(&registry);

// NEW: Register blob metrics
let blob_metrics = Arc::new(BlobEngineMetrics::register(&registry));

// Later when creating blob engine (find existing BlobEngineImpl::new call)
let blob_engine = BlobEngineImpl::new(blob_store)?
    .with_metrics(blob_metrics);
```

---

## Testing Metrics

### Step 1: Build and Start Testnet

```bash
# Build with metrics
cargo build --release

# Start testnet
make all
```

### Step 2: Check Metrics Endpoint

```bash
# Check blob metrics are exposed
curl -s http://localhost:29000/metrics | grep blob_engine

# Expected output (initial state):
# blob_engine_verifications_success_total{job="malachite0",moniker="test-0"} 0
# blob_engine_verifications_failure_total{job="malachite0",moniker="test-0"} 0
# blob_engine_storage_bytes_undecided{job="malachite0",moniker="test-0"} 0
# blob_engine_storage_bytes_decided{job="malachite0",moniker="test-0"} 0
# blob_engine_undecided_blob_count{job="malachite0",moniker="test-0"} 0
# ... (12 total)
```

### Step 3: Verify Prometheus Scrapes

```bash
# Check Prometheus targets
curl -s http://localhost:9090/api/v1/targets | python3 -m json.tool | grep -A 10 "malachite0"

# Query specific metric
curl -s 'http://localhost:9090/api/v1/query?query=blob_engine_verifications_success_total' | python3 -m json.tool
```

### Step 4: Generate Load (After Spam Tool Fixed)

```bash
# Run blob spam
make spam-blobs

# Watch metrics update
watch -n 1 'curl -s http://localhost:29000/metrics | grep blob_engine_verifications'
```

### Step 5: Verify Metric Behavior

**Test Cases**:
1. ✅ `verifications_success_total` increments on valid blobs
2. ✅ `storage_bytes_undecided` increases when storing blobs
3. ✅ `undecided_blob_count` matches number of stored blobs
4. ✅ `lifecycle_promoted_total` increments on block finalization
5. ✅ `storage_bytes_decided` increases after promotion
6. ✅ `blobs_per_block` reflects blobs in last block
7. ✅ `verification_time` histogram shows P50/P99 values

---

## Dashboard Queries

### Pattern: Match GRAFANA_WORKING_STATE.md

**Simple queries (no aggregations, no template variables)**

### Panel 1: Verification Success Rate

```json
{
  "title": "Blob Verification Rate",
  "type": "timeseries",
  "targets": [{
    "expr": "rate(blob_engine_verifications_success_total[1m])",
    "legendFormat": "{{job}} - success",
    "range": true,
    "instant": false
  }]
}
```

### Panel 2: Verification Failures

```json
{
  "title": "Blob Verification Failures",
  "type": "timeseries",
  "targets": [{
    "expr": "rate(blob_engine_verifications_failure_total[1m])",
    "legendFormat": "{{job}} - failure",
    "range": true,
    "instant": false
  }]
}
```

### Panel 3: Storage Size by State

```json
{
  "title": "Blob Storage Size - Undecided",
  "type": "timeseries",
  "targets": [{
    "expr": "blob_engine_storage_bytes_undecided",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

```json
{
  "title": "Blob Storage Size - Decided",
  "type": "timeseries",
  "targets": [{
    "expr": "blob_engine_storage_bytes_decided",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

### Panel 4: Undecided Blob Count

```json
{
  "title": "Undecided Blobs Count",
  "type": "timeseries",
  "targets": [{
    "expr": "blob_engine_undecided_blob_count",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

### Panel 5: Lifecycle Transitions

```json
{
  "title": "Blob Lifecycle - Promoted",
  "type": "timeseries",
  "targets": [{
    "expr": "rate(blob_engine_lifecycle_promoted_total[1m])",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

```json
{
  "title": "Blob Lifecycle - Dropped",
  "type": "timeseries",
  "targets": [{
    "expr": "rate(blob_engine_lifecycle_dropped_total[1m])",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

### Panel 6: Blobs Per Block

```json
{
  "title": "Blobs Per Block",
  "type": "timeseries",
  "targets": [{
    "expr": "blob_engine_blobs_per_block",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

### Panel 7: Verification Latency (P99)

```json
{
  "title": "Blob Verification Latency (P99)",
  "type": "timeseries",
  "targets": [{
    "expr": "histogram_quantile(0.99, rate(blob_engine_verification_time_bucket[5m]))",
    "legendFormat": "{{job}} - P99",
    "range": true,
    "instant": false
  }]
}
```

### Panel 8: Restream Rebuilds

```json
{
  "title": "Restream Rebuilds Rate",
  "type": "timeseries",
  "targets": [{
    "expr": "rate(blob_engine_restream_rebuilds_total[1m])",
    "legendFormat": "{{job}}",
    "range": true,
    "instant": false
  }]
}
```

**Note**: Add panels to dashboard ONLY after metrics are confirmed working (Step 2 above passes).

---

## Future Work - Node-Level Metrics

**Status**: 🔵 **Phase A.2** - After BlobEngine metrics complete

### What's Not Covered (Yet)

This plan focuses on **BlobEngine** (storage layer) metrics. The following **consensus-layer** operations in `crates/node/src/app.rs` are not yet instrumented:

#### **Missing Metrics** (Recommend adding in Phase A.2):

1. **Blob Count Mismatch Detection** ⚠️ **CRITICAL SAFETY METRIC**
   - **Location**: `app.rs:630` (Decided handler)
   - **Code**: `if blobs.len() != expected_blob_count { ... }`
   - **Metric**: `app_blob_count_mismatch_total` (counter)
   - **Why Critical**: Detects blob availability failures during finalization

2. **Proposer Blob Storage Timing**
   - **Location**: `app.rs:163` (GetValue handler)
   - **Code**: `state.blob_engine().verify_and_store(...)`
   - **Metric**: `app_blob_proposal_storage_duration_seconds` (histogram)
   - **Purpose**: Track proposer-side blob storage latency

3. **Restream Rebuild Operations**
   - **Location**: `app.rs:410` (RestreamProposal handler)
   - **Code**: `state.blob_engine().get_undecided_blobs(...)`
   - **Metric**: `app_blob_restream_rebuilds_total` (counter)
   - **Purpose**: Track how often proposals are rebuilt for restreaming

4. **Sync Path Blob Operations**
   - **Location**: `app.rs:765` (ProcessSyncedValue handler)
   - **Code**: `state.blob_engine().verify_and_store(...)`
   - **Metric**: `app_blob_sync_received_total` (counter)
   - **Purpose**: Distinguish sync path from proposal path blobs

### Why Deferred

**Rationale**:
- BlobEngine metrics provide 80% of observability value
- Node-level metrics require understanding consensus flow patterns
- Can be added incrementally after BlobEngine metrics proven working
- Estimated effort: +2 hours (simple counters/histograms in app.rs)

### Implementation Approach (Phase A.2)

**File**: Create `crates/node/src/blob_metrics.rs` (separate from BlobEngine)

```rust
use malachitebft_app_channel::app::metrics::{
    SharedRegistry,
    prometheus::metrics::{counter::Counter, histogram::Histogram},
};

#[derive(Clone, Debug)]
pub struct AppBlobMetrics(Arc<Inner>);

#[derive(Debug)]
pub struct Inner {
    blob_count_mismatch: Counter,
    proposal_storage_duration: Histogram,
    restream_rebuilds: Counter,
    sync_received: Counter,
}

impl AppBlobMetrics {
    pub fn register(registry: &SharedRegistry) -> Self {
        // Register with prefix "app_blob_"
    }
}
```

**Instrumentation**: Add metric calls directly in `app.rs` handlers (no trait changes needed).

**Validation**: Same process as BlobEngine metrics (curl /metrics, check Prometheus).

---

## Progress Tracking

### Phase A: BlobEngine Metrics Instrumentation

**Status**: 🟢 Ready to Start

**Scope**: BlobEngine (storage layer) only - node-level metrics deferred to Phase A.2

#### Task A.1: Create Metrics Module
- [ ] Create `crates/blob_engine/src/metrics.rs`
- [ ] Add `BlobEngineMetrics` struct with 12 metrics
- [ ] Add `register()` method using `SharedRegistry`
- [ ] Add helper methods for instrumentation (lines 293-351 in this doc)
- [ ] Export in `lib.rs`: `pub mod metrics;`

**Estimated Time**: 1-2 hours
**Reference**: Lines 128-358 in this document (complete code provided)

#### Task A.2: Add Dependencies
- [ ] Add `malachitebft-app-channel` to `blob_engine/Cargo.toml`
- [ ] Verify `cargo build -p ultramarine-blob-engine` succeeds

**Estimated Time**: 5 minutes

#### Task A.3: Add Metrics Field to BlobEngine
- [ ] Add `metrics: Option<Arc<BlobEngineMetrics>>` to `BlobEngineImpl`
- [ ] Add `with_metrics()` builder method
- [ ] Update constructor

**Estimated Time**: 15 minutes

#### Task A.4: Extend BlobStore API
- [ ] Add `count_undecided_blobs(height, round)` that returns the number of blobs without materialising payloads
- [ ] Update `prune_archived_before` to return `(count, bytes)` (derive bytes as `count * BYTES_PER_BLOB` for RocksDB)
- [ ] Expose helpers in the trait and RocksDB implementation

**Estimated Time**: 45 minutes

#### Task A.5: Instrument BlobEngine Methods
- [ ] Instrument `verify_and_store` (verification counters + histogram)
- [ ] Instrument `mark_decided` (promotion counters + blobs_per_block gauge)
- [ ] Instrument `drop_round` (drop counters)
- [ ] Instrument `mark_archived` / `prune_archived_before` (decided storage decrements)

**Estimated Time**: 1-2 hours

#### Task A.6: Register in Node Startup
- [ ] Find blob engine initialization in `crates/node/src/node.rs`
- [ ] Create `BlobEngineMetrics::register(&registry)`
- [ ] Call `.with_metrics()` on blob engine
- [ ] Verify `cargo build -p ultramarine-node` succeeds

**Estimated Time**: 30 minutes

#### Task A.7: Wire Consensus Hooks
- [ ] Add `blob_metrics: Option<Arc<BlobEngineMetrics>>` field to `State`
- [ ] Pass `Arc<BlobEngineMetrics>` (or thin wrapper) into `State`
- [ ] Record `set_blobs_per_block` inside `State::commit`
- [ ] Increment restream/rebuild counters in `State::rebuild_blob_sidecars_for_restream`
- [ ] Track proposer-side failures in `State::prepare_blob_sidecar_parts` / import path

**Estimated Time**: 45 minutes

#### Task A.8: Test Metrics Endpoint
- [ ] `make all` (start testnet)
- [ ] `curl http://localhost:29000/metrics | grep blob_engine` (verify 12 metrics)
- [ ] Verify Prometheus scrapes targets successfully
- [ ] Query via Prometheus API

**Estimated Time**: 30 minutes

#### Task A.9: Validate Metric Behavior (Optional, requires blob spam)
- [ ] Run blob spam tool (after Phase E fixes)
- [ ] Verify `verifications_success_total` increments
- [ ] Verify `storage_bytes_undecided` increases
- [ ] Verify `lifecycle_promoted_total` increments on finalization

**Estimated Time**: 30 minutes (blocked by spam tool)

---

### Success Criteria

Phase A is complete when:

1. ✅ `crates/blob_engine/src/metrics.rs` exists with 12 metrics
2. ✅ `cargo build` succeeds
3. ✅ `curl http://localhost:29000/metrics | grep blob_engine` returns 12+ lines
4. ✅ Prometheus scrapes metrics successfully (check `/targets` page)
5. ✅ Query returns data: `curl 'http://localhost:9090/api/v1/query?query=blob_engine_verifications_success_total'`

**Optional** (requires working spam tool):
6. ✅ Metrics update in real-time during blob spam
7. ✅ Verification, storage, and lifecycle metrics correlate correctly

---

## Notes

### What to Avoid
- ❌ Using `prometheus` crate directly (use `malachitebft-app-channel` instead)
- ❌ `CounterVec`/`GaugeVec` with labels (use separate metrics like DbMetrics)
- ❌ Complex dashboard queries (no `max by`, no filters initially)
- ❌ Adding metrics to dashboard before Step 2 passes
- ❌ Following other plan documents (BLOB_METRICS_MINIMAL_PLAN.md, PHASE5_ACTION_PLAN.md are archived)

### What to Do
- ✅ Follow `DbMetrics` pattern exactly (`crates/consensus/src/metrics.rs`)
- ✅ Use `SharedRegistry::with_prefix("blob_engine", ...)`
- ✅ Test metrics endpoint BEFORE adding dashboard panels
- ✅ Keep helper methods simple (e.g., `record_verifications_success`)
- ✅ Use this document (METRICS_PROGRESS.md) as single source of truth

### Dependencies
- `malachitebft-app-channel` workspace dependency ✅ **Already in workspace**
- Existing `SharedRegistry` in node startup ✅ **Available**
- Working Prometheus/Grafana setup ✅ **Functional** (verified 2025-11-03)

### Architecture Validation ✅

**This plan has been reviewed against the codebase and confirmed:**
- ✅ Follows exact pattern from `consensus/src/metrics.rs` (DbMetrics)
- ✅ Uses correct dependency (`malachitebft-app-channel`)
- ✅ Constructor pattern matches codebase (`with_metrics()` builder)
- ✅ Helper methods handle gauge inc/dec correctly
- ✅ Compatible with existing monitoring infrastructure (Prometheus + Grafana)
- ⚠️ Defers node-level metrics (app.rs) to Phase A.2 (see section above)

---

## References

- **Pattern**: `crates/consensus/src/metrics.rs` (DbMetrics)
- **Registration**: `crates/node/src/node.rs:158` (DbMetrics::register)
- **Dashboard Style**: `docs/GRAFANA_WORKING_STATE.md` (simple queries)
- **Phase Plan**: `docs/PHASE5_TESTNET.md` (Phase A)

---

**Last Updated**: 2025-11-03
**Next Review**: After Task A.6 completion (metrics endpoint tested)
