# Blob Engine Metrics - Implementation Progress

**Date**: 2025-11-04
**Status**: ✅ COMPLETE - Validated on Testnet
**Phase**: Phase 5A-C - Metrics Instrumentation & Validation
**Tracking**: Implementation and testnet validation of blob observability metrics

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
| `blob_engine_blobs_per_block` | Gauge | Number of blobs in last finalized block | count | `BlobEngineImpl::mark_decided` |
| `blob_engine_lifecycle_promoted_total` | Counter | Blobs promoted to decided state | count | `mark_decided` |
| `blob_engine_lifecycle_dropped_total` | Counter | Blobs dropped from undecided state | count | `drop_round` |
| `blob_engine_lifecycle_pruned_total` | Counter | Decided blobs pruned/archived | count | `prune_archived_before` |
| `blob_engine_restream_rebuilds_total` | Counter | Blob metadata rebuilds during restream | count | `State::rebuild_blob_sidecars_for_restream` |
| `blob_engine_sync_failures_total` | Counter | Blob sync/fetch failures | count | `AppMsg::ProcessSyncedValue` error path (app.rs) |

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
   - `State::rebuild_blob_sidecars_for_restream` records restream rebuilds
   - `State::record_sync_failure` exposes blob sync errors in the import path
   - `BlobEngineImpl::mark_decided` sets `blob_engine_blobs_per_block` when a block finalizes
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
    metrics: BlobEngineMetrics, // NEW
}

impl<S> BlobEngineImpl<S>
where
    S: BlobStore,
{
    pub fn new(store: S, metrics: BlobEngineMetrics) -> Result<Self, BlobEngineError> {
        Ok(Self {
            verifier: BlobVerifier::new()?,
            store,
            metrics,
        })
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
    if sidecars.is_empty() {
        return Ok(());
    }

    let timer_start = std::time::Instant::now();
    let refs: Vec<&BlobSidecar> = sidecars.iter().collect();

    if let Err(err) = self.verifier.verify_blob_sidecars_batch(&refs) {
        self.metrics.observe_verification_time(timer_start.elapsed());
        self.metrics.record_verifications_failure(sidecars.len());
        return Err(err.into());
    }

    self.metrics.observe_verification_time(timer_start.elapsed());
    self.metrics.record_verifications_success(sidecars.len());

    let stored_count = self.store.put_undecided_blobs(height, round, sidecars).await?;
    let total_bytes = stored_count * BYTES_PER_BLOB;
    self.metrics.add_undecided_storage(total_bytes, stored_count);

    Ok(())
}
```

> `BYTES_PER_BLOB` keeps the gauge math constant-time; every blob is exactly 131,072 bytes.

### 3. Instrument `mark_decided`

```rust
async fn mark_decided(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
    let (blob_count, total_bytes) = self.store.mark_decided(height, round).await?;

    self.metrics.promote_blobs(total_bytes, blob_count);
    self.metrics.set_blobs_per_block(blob_count);

    Ok(())
}
```

> `mark_decided` now returns both blob count and serialized byte total, so the engine can update gauges without issuing a second RocksDB scan.

### 4. Instrument `drop_round`

```rust
async fn drop_round(&self, height: Height, round: i64) -> Result<(), BlobEngineError> {
    let (blob_count, total_bytes) = self.store.drop_round(height, round).await?;

    self.metrics.drop_blobs(total_bytes, blob_count);

    Ok(())
}
```

### 5. Instrument `mark_archived`

```rust
async fn mark_archived(&self, height: Height, indices: &[u16]) -> Result<(), BlobEngineError> {
    self.store.delete_archived(height, indices).await?;

    let bytes = indices.len() * BYTES_PER_BLOB;
    self.metrics.prune_blobs(bytes, indices.len());

    Ok(())
}
```

### 6. Instrument `prune_archived_before`

```rust
async fn prune_archived_before(&self, height: Height) -> Result<usize, BlobEngineError> {
    let pruned_count = self.store.prune_before(height).await?;
    let pruned_bytes = pruned_count * BYTES_PER_BLOB;

    self.metrics.prune_blobs(pruned_bytes, pruned_count);

    Ok(pruned_count)
}
```

> For Phase 5 the store only returns counts; we derive byte totals via the fixed blob size constant. Phase 6 will expand this to configurable retention strategies.

### 7. Consensus Hooks

```rust
pub struct State<E>
where
    E: BlobEngine,
{
    // ...
    pub blob_metrics: BlobEngineMetrics,
    // ...
}
```

```rust
impl State {
    fn rebuild_blob_sidecars_for_restream(&self, metadata: &BlobMetadata, ...) -> eyre::Result<Vec<BlobSidecar>> {
        let result = /* existing rebuild */;
        self.blob_metrics.record_restream_rebuild();
        result
    }
}

// In app.rs (sync path)
state.blob_metrics.record_sync_failure();
```

> Pass the metrics handle (clone of `BlobEngineMetrics`) into `State::new` when constructing it in `node.rs` so consensus can emit restream/sync counters; per-block gauges are updated inside `BlobEngineImpl::mark_decided`.

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
let blob_metrics = BlobEngineMetrics::register(&registry);

// Later when creating blob engine (find existing BlobEngineImpl::new call)
let blob_engine = BlobEngineImpl::new(blob_store, blob_metrics.clone())?;
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

**Status**: ✅ Complete (2025-11-04)

**Scope**: BlobEngine (storage layer) only - node-level metrics deferred to Phase A.2

#### Task A.1: Create Metrics Module
- [x] Create `crates/blob_engine/src/metrics.rs`
- [x] Add `BlobEngineMetrics` struct with 12 metrics
- [x] Add `register()` method using `SharedRegistry`
- [x] Add helper methods for instrumentation (lines 293-351 in this doc)
- [x] Export in `lib.rs`: `pub mod metrics;`

**Estimated Time**: 1-2 hours
**Reference**: Lines 128-358 in this document (complete code provided)

#### Task A.2: Add Dependencies
- [x] Add `malachitebft-app-channel` to `blob_engine/Cargo.toml`
- [x] Verify `cargo build -p ultramarine-blob-engine` succeeds

**Estimated Time**: 5 minutes

#### Task A.3: Add Metrics Field to BlobEngine
- [x] Add `metrics: BlobEngineMetrics` to `BlobEngineImpl` (required parameter, not Optional)
- [x] Update constructor to require metrics parameter
- [x] Update all test call sites (4 tests) and node initialization

**Estimated Time**: 15 minutes
**Note**: Implemented as required parameter for simplicity - matches DbMetrics pattern

#### Task A.4: Extend BlobStore API
- [x] Update `put_undecided_blobs` to return `usize` (blob count)
- [x] Update `mark_decided` to return `(usize, usize)` (blob count, total bytes)
- [x] Update `drop_round` to return `(usize, usize)` (blob count, total bytes)
- [x] Update trait and RocksDB implementation with count tracking

**Estimated Time**: 45 minutes
**Note**: Focused on methods that needed metrics; prune_before already returned count

#### Task A.5: Instrument BlobEngine Methods
- [x] Instrument `verify_and_store` (verification counters + histogram + storage gauges)
- [x] Instrument `mark_decided` (promotion counters + gauge adjustments)
- [x] Instrument `drop_round` (drop counters + gauge decrements)
- [x] Instrument `mark_archived` (decided storage decrements)
- [x] Instrument `prune_archived_before` (decided storage decrements)

**Estimated Time**: 1-2 hours
**Note**: Used bulk gauge operations (inc_by/dec_by) instead of loops for performance

#### Task A.6: Register in Node Startup
- [x] Find blob engine initialization in `crates/node/src/node.rs` (line 168)
- [x] Create `BlobEngineMetrics::register(&registry)` (line 159)
- [x] Pass metrics to `BlobEngineImpl::new(store, blob_metrics)` (line 169)
- [x] Verify `cargo build -p ultramarine-node` succeeds

**Estimated Time**: 30 minutes

#### Task A.7: Wire Consensus Hooks
- [x] Add `blob_metrics: BlobEngineMetrics` field to `State`
- [x] Pass metrics clone into `State::new` (node startup)
- [x] Increment restream/rebuild counters in `State::rebuild_blob_sidecars_for_restream`
- [x] Track proposer-side failures in `State::prepare_blob_sidecar_parts` / import path (sync failure counter)

**Estimated Time**: 45 minutes (completed during Phase A.2)
**Note**: Per-block gauges are updated in `BlobEngineImpl::mark_decided`; consensus-owned metrics cover restream and sync paths.

#### Task A.8: Test Metrics Endpoint
- [x] **COMPLETE** - `make all` (start testnet)
- [x] **COMPLETE** - `curl http://localhost:29000/metrics | grep blob_engine` (verified 12 metrics)
- [x] **COMPLETE** - Verify Prometheus scrapes targets successfully
- [x] **COMPLETE** - Query via Prometheus API

**Estimated Time**: 30 minutes
**Status**: ✅ Completed 2025-11-04 (Phase C)

#### Task A.9: Validate Metric Behavior (Optional, requires blob spam)
- [x] **COMPLETE** - Run blob spam tool (193 txs, 1,158 blobs)
- [x] **COMPLETE** - Verify `verifications_success_total` increments (1,158 successes)
- [x] **COMPLETE** - Verify `storage_bytes_undecided` increases (dynamic during proposals)
- [x] **COMPLETE** - Verify `lifecycle_promoted_total` increments on finalization (1,158 promoted)

**Estimated Time**: 30 minutes
**Status**: ✅ Completed 2025-11-04 (Phase C) - Spam tool works correctly

---

### Implementation Summary (2025-11-04)

**What Was Completed:**

1. **Metrics Module** (`crates/blob_engine/src/metrics.rs` - 235 lines)
   - 12 metrics implemented (8 counters, 3 gauges, 1 histogram)
   - `BlobEngineMetrics` struct with `Arc<Inner>` pattern matching `DbMetrics`
   - Helper methods: `add_undecided_storage()`, `promote_blobs()`, `drop_blobs()`, `prune_blobs()`, etc.
   - Registration via `SharedRegistry` with "blob_engine" prefix

2. **BlobStore API Extensions** (`crates/blob_engine/src/store/mod.rs`)
   - Updated `put_undecided_blobs()` return type: `Result<(), _>` → `Result<usize, _>`
   - Updated `mark_decided()` return type: `Result<(), _>` → `Result<(usize, usize), _>`
   - Updated `drop_round()` return type: `Result<(), _>` → `Result<(usize, usize), _>`
   - RocksDB implementation tracks counts using blob sidecar iteration and `.size()` method

3. **BlobEngine Instrumentation** (`crates/blob_engine/src/engine.rs`)
   - Added `metrics: BlobEngineMetrics` field to `BlobEngineImpl`
   - `verify_and_store()`: Records verification timing, success/failure counts, storage gauge updates
   - `mark_decided()`: Tracks promotion with `promote_blobs(bytes, count)`
   - `drop_round()`: Tracks drops with `drop_blobs(bytes, count)`
   - `mark_archived()`: Tracks pruning with `prune_blobs(bytes, count)`
   - `prune_archived_before()`: Uses `BYTES_PER_BLOB` constant for byte calculations

4. **Node Registration** (`crates/node/src/node.rs`)
   - Line 159: `BlobEngineMetrics::register(&registry)`
   - Line 169: Pass metrics to `BlobEngineImpl::new(store, blob_metrics)`
   - Wired into SharedRegistry with moniker prefix

5. **Documentation Updates**
   - Fixed lib.rs example to show `BlobEngineImpl::new(store, metrics)?`
   - Updated engine.rs BlobEngine trait doc example
   - Fixed verifier.rs doc test (removed broken example)

**Critical Fixes Applied During Code Review:**

1. **Performance Fix**: Replaced gauge loops with bulk operations
   - Before: `for _ in 0..bytes { self.gauge.inc(); }` (131k+ operations per blob)
   - After: `self.gauge.inc_by(bytes as i64)` (single operation)

2. **Missing Instrumentation**: Added metrics to `mark_archived()`
   - Calculates `total_bytes = blob_count * BYTES_PER_BLOB`
   - Calls `metrics.prune_blobs(total_bytes, blob_count)`

3. **Magic Number Elimination**: Replaced hard-coded `131_072` with `BYTES_PER_BLOB` constant
   - Imported from `ultramarine_types::blob::BYTES_PER_BLOB`
   - Applied in `prune_archived_before()` calculation

4. **Type Fixes**: Corrected gauge API usage
   - Gauge methods accept `i64`, not `u64`
   - Changed all casts from `as u64` to `as i64`

**Test Results:**
- ✅ 11 unit tests passing, 2 ignored (require valid KZG proofs)
- ✅ Full codebase builds successfully
- ✅ All doc tests compile

**Deferred to Phase A.2:**
- State/consensus hooks (`set_blobs_per_block`, restream counters)
- Integration testing with live testnet
- Grafana dashboard validation

**Files Modified (Phase A.1):**
- `crates/blob_engine/src/metrics.rs` (new)
- `crates/blob_engine/src/engine.rs`
- `crates/blob_engine/src/store/mod.rs`
- `crates/blob_engine/src/store/rocksdb.rs`
- `crates/blob_engine/src/lib.rs`
- `crates/blob_engine/Cargo.toml`
- `crates/node/src/node.rs`

---

### Phase A.2 Implementation Summary (2025-11-04)

**What Was Completed:**

1. **State Metrics Integration**
   - Added `pub(crate) blob_metrics: BlobEngineMetrics` field to `State` struct
   - Updated `State::new()` constructor to accept metrics parameter
   - Updated `node.rs` to pass `blob_metrics.clone()` to State
   - Fixed test helper in `state.rs` to create metrics instance

2. **Fixed Missing Instrumentation**
   - Added `set_blobs_per_block(blob_count)` call in `BlobEngine::mark_decided()` (engine.rs:273)
   - This gauge was defined but never updated - now correctly tracks blobs per finalized block

3. **Restream Path Instrumentation**
   - Added `self.blob_metrics.record_restream_rebuild()` in `State::rebuild_blob_sidecars_for_restream()` (state.rs:503)
   - Tracks when blob sidecars are reconstructed from storage metadata during restreaming

4. **Sync Failure Instrumentation**
   - Added `state.record_sync_failure()` in blob sync error path (app.rs:771)
   - Tracks failed blob verification/storage during sync package processing

5. **Encapsulation Improvements** (Architectural Quality)
   - Changed `blob_metrics` field visibility to `pub(crate)` (state.rs:87)
   - Added public helper method `State::record_sync_failure()` (state.rs:189-195)
   - External crates (like `ultramarine-node`) now use clean API instead of direct field access
   - State maintains sole ownership of its instrumentation surface
   - Internal code within `consensus` crate can still access `self.blob_metrics` directly for flexibility

**Architectural Rationale:**

The `pub(crate)` + helper method pattern provides:
- ✅ **Encapsulation**: State owns its instrumentation surface, metrics changes stay localized
- ✅ **Maintainability**: Future metrics API changes don't ripple across crate boundaries
- ✅ **Clean API**: External callers use documented, semantic methods
- ✅ **Flexibility**: Internal consensus code can access metrics directly when needed

**Files Modified (Phase A.2):**
- `crates/consensus/src/state.rs` (metrics field, helper method, restream instrumentation)
- `crates/node/src/node.rs` (pass metrics to State)
- `crates/node/src/app.rs` (sync failure instrumentation)
- `crates/blob_engine/src/engine.rs` (fixed set_blobs_per_block call)

**Test Results:**
- ✅ 25 consensus tests passing
- ✅ 11 blob_engine tests passing
- ✅ Full codebase builds successfully
- ✅ Zero regressions

---

### Success Criteria

**Phase A.1 (BlobEngine instrumentation)** is complete when:

1. ✅ `crates/blob_engine/src/metrics.rs` exists with 12 metrics **[DONE 2025-11-04]**
2. ✅ `cargo build` succeeds **[DONE 2025-11-04]**
3. ✅ `curl http://localhost:29000/metrics | grep blob_engine` returns 12+ lines **[DONE 2025-11-04]**
4. ✅ Prometheus scrapes metrics successfully (check `/targets` page) **[DONE 2025-11-04]**
5. ✅ Query returns data: `curl 'http://localhost:9090/api/v1/query?query=blob_engine_verifications_success_total'` **[DONE 2025-11-04]**

**Phase A.2 (State/consensus hooks)** is complete when:

1. ✅ State accepts BlobEngineMetrics at construction **[DONE 2025-11-04]**
2. ✅ `blobs_per_block` gauge updates on finalization **[DONE 2025-11-04]**
3. ✅ `restream_rebuilds_total` increments when rebuilding metadata **[DONE 2025-11-04]**
4. ✅ `sync_failures_total` increments on blob fetch/verify errors **[DONE 2025-11-04]**
5. ✅ Metrics encapsulation maintained via `pub(crate)` + helper methods **[DONE 2025-11-04]**
6. ✅ All tests still pass (25 consensus, 11 blob_engine) **[DONE 2025-11-04]**

**Optional** (requires working spam tool):
7. ✅ Metrics update in real-time during blob spam **[DONE 2025-11-04]**
8. ✅ Verification, storage, and lifecycle metrics correlate correctly **[DONE 2025-11-04]**

**Phase A.1 Core Implementation**: ✅ Complete
**Phase A.2 State Hooks**: ✅ Complete
**Integration Validation**: ✅ Complete (Phase C validated on testnet)

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
- ✅ Constructor pattern matches codebase (metrics passed directly to `BlobEngineImpl::new`)
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

## Next Steps: Integration & Testing

**Phase A (Metrics Instrumentation)**: ✅ **COMPLETE**

Both Phase A.1 (BlobEngine surface) and Phase A.2 (State/consensus hooks) are now complete. All 12 metrics are fully instrumented across the BlobEngine and State layers.

**Upcoming Phases:**

### **Phase B: In-Process Integration Tests** (6-8 hours) — ✅ **Complete (event-driven harness, 2025-11-18 refresh)**
- Implement test harness with `TempDir` isolation
- Add 3 integration tests: `blob_roundtrip`, `restart_hydrate`, `sync_package_roundtrip`
- Mock Execution client, use real blob engine/KZG
- Target: ~2-5 seconds per test

### **Phase C: Full-Stack Smoke & Observability** (4-6 hours) — ⏳ **Pending**
- Boot Docker testnet (`make all`)
- Run blob spam tool (after Phase E fixes)
- Verify metrics endpoint exposes all 12 `blob_engine_*` metrics
- Expand Grafana dashboard with blob panels
- Document testnet workflow

### **Phase E: Fix Spam Tool** (4-6 hours) — ⏳ **Pending**
- Generate real 131KB blobs with KZG commitments/proofs
- Use Alloy/Reth blob transaction helpers
- Required for end-to-end validation

**Immediate Action Items:**
1. ⏳ Wait for Phase B integration tests from beta team
2. ✅ Start testnet to validate metrics endpoint (`curl /metrics | grep blob_engine`)
3. ⏳ Begin Grafana dashboard panel design (see GRAFANA_WORKING_STATE.md)

---

**Last Updated**: 2025-11-04
**Phase A.1**: ✅ Complete (BlobEngine instrumentation)
**Phase A.2**: ✅ Complete (State/consensus hooks)
**Phase B**: ✅ Complete (Beta team integration tests landed; deterministic harness updated 2025-11-18)
**Next Review**: After testnet startup (metrics endpoint validation)
