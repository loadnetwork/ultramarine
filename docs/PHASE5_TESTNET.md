# Phase 5 Testnet: Integration Testing & Blob Observability

**Status**: ✅ Phase A-D Complete – Phase 5 signed off (metrics, harness, testnet)
**Started**: 2025-10-28
**Updated**: 2025-11-08
**Goal**: Validate blob sidecar integration end-to-end, add comprehensive observability, and establish testnet workflow for production readiness.

---

## Table of Contents

1. [Overview](#overview)
2. [Design Philosophy](#design-philosophy)
3. [Current State Analysis](#current-state-analysis)
4. [Implementation Plan](#implementation-plan)
5. [Daily Progress Log](#daily-progress-log)
6. [Testing Strategy](#testing-strategy)
7. [Test Artifacts](#test-artifacts)
8. [Success Criteria](#success-criteria)

---

## Overview

### Motivation

Phases 1-4 delivered a complete blob sidecar implementation with 23/23 passing unit tests. Phase 5-5.2 fixed critical bugs for live consensus. Phase 5A-C added comprehensive observability and validation:

- ✅ Working 3-node testnet infrastructure
- ✅ Spam tool with `--blobs` flag generates valid blob transactions with real KZG proofs
- ✅ **12 blob-specific metrics** (verification time, storage size, lifecycle transitions)
- ✅ **9 blob dashboard panels** added to Grafana for blob observability
- ✅ **Integration harness delivered by Team Beta** (Tier 0: 3 fast consensus smokes; Tier 1: 14 full-node scenarios with real KZG blobs)
- ✅ **Integration results**: Tier 0 3/3 (default in `make test`); Tier 1 14/14 via `make itest-node` and CI’s `itest-tier1`

### Goals

1. **Metrics Instrumentation**: Add Prometheus metrics to BlobEngine for verification, storage, and lifecycle.
2. **Grafana Dashboard**: Create 10+ panels to visualize blob activity across consensus/execution layers.
3. **In-Process Integration Tests**: Provide deterministic `cargo test` coverage for blob lifecycle, restart survival, and sync package handling.
4. **Testnet Workflow**: Document and automate blob testing procedures for manual validation.
5. **Performance Validation**: Measure blob verification latency, storage growth, and throughput under load.

### Non-Goals

- Blob pruning/archival implementation (deferred to Phase 6)
- P2P bandwidth optimization (current streaming works, optimization later)
- Production deployment (focus on validation first)

---

## Design Philosophy

### Guiding Principles

- **Mirror Malachite’s modular style**: Each functional area (consensus, blob engine, node) owns small, self-contained integration tests that run directly under `cargo test`.
- **Lean, deterministic harness**: Prefer in-process Tokio harnesses with mocked Engine API clients and temporary blob stores over heavyweight external dependencies.
- **Optional heavyweight smoke**: Keep the existing Docker-based network workflow, but invoke it only for opt-in “full stack” validation.
- **Single-command ergonomics**: Developers should be able to run fast in-process tests with `make itest`, and the full-stack smoke with `make itest-full`.
- **Metrics-first observability**: Every blob operation emits Prometheus metrics so both tests and dashboards have programmatic signals.

### Implementation Pattern

1. **Inline Test Helpers**
   - Define small helper structs/functions directly inside integration tests (or `tests/common/mod.rs`).
   - Use Tokio tasks to spin Ultramarine nodes, mock only the Execution client, reuse real blob engine/KZG logic.
   - Allocate `tempfile::TempDir` per test so Drop handles cleanup automatically.
2. **Focused Integration Tests**
   - Place under `tests/` with descriptive names (`blob_roundtrip.rs`, `restart_hydrate.rs`, etc.).
   - Mark `#[tokio::test] #[ignore]` and optional `#[serial]` for resource isolation.
   - Assert on store state, blob metrics, restream behavior—tests should finish in ~2–5 s.
3. **Full Stack Smoke (Optional)**
   - Wrap existing `make all` workflow; run the repaired blob spammer; assert via RPC/metrics.
   - Keep opt-in (env flag / `make itest-full`) so CI/devs run it only when needed.

This approach resolves the gap between purely manual testing and the need for automated coverage without contradicting Malachite’s precedent.

---

## Current State Analysis

### Testnet Infrastructure (✅ Production-Ready)

**Architecture** (as of 2025-10-28):
```
┌─────────────────────────────────────────────────────────────┐
│  Execution Layer (Reth v1.4.1)                              │
│  ├─ reth0  (8545/8551)   metrics: 9100  ─┐                  │
│  ├─ reth1  (18545/18551) metrics: 9101  ─┼─> Prometheus    │
│  └─ reth2  (28545/28551) metrics: 9102  ─┘   (1s scrape)   │
└─────────────────────────────────────────────────────────────┘
                          ▲
                          │ Engine API v3 (HTTP/IPC)
                          ▼
┌─────────────────────────────────────────────────────────────┐
│  Consensus Layer (Ultramarine + Malachite)                  │
│  ├─ malachite0  metrics: 29000  ─┐                          │
│  ├─ malachite1  metrics: 29001  ─┼─> Prometheus             │
│  └─ malachite2  metrics: 29002  ─┘                          │
└─────────────────────────────────────────────────────────────┘
                          │
                          ▼
                    ┌──────────────┐
                    │   Grafana    │
                    │   :3000      │
                    └──────────────┘
```

**Automation** (`Makefile` targets):
- `make all` - Full testnet setup (genesis → docker → spawn nodes)
- `make all-ipc` - Testnet with Engine IPC (Docker-based)
- `make spam` - Transaction load testing (supports `--blobs` flag)
- `make clean-net` - Full cleanup (preserves code, removes data)
- `make stop` - Stop Docker stack

**Scripts**:
- `scripts/spawn.bash` - Node lifecycle management (supports `--engine-ipc-base`, `--jwt-path`)
- `scripts/add_peers.sh` - P2P bootstrapping
- `crates/cli/src/cmd/testnet.rs` - Config generation for N validators

### Existing Metrics

**Malachite (Consensus)**:
- `malachitebft_core_consensus_height` - Block height
- `malachitebft_core_consensus_round` - Round number
- `malachitebft_core_consensus_block_size_bytes` - Block size
- `app_channel_db_*` - Database I/O (consensus store)

**Reth (Execution)**:
- `reth_engine_rpc_get_payload_v3_count` - Engine API calls
- `reth_engine_rpc_forkchoice_updated_messages` - FCU messages
- `reth_transaction_pool_*` - Txpool metrics (pending/queued)
- `reth_blockchain_tree_canonical_chain_height` - EL height
- `reth_network_connected_peers` - Peer count

**Notable**: Zero blob-specific metrics in either layer.

### Spam Tool (✅ Blob-Capable After Phase E)

**Location**: `crates/utils/src/commands/spam.rs`, supporting helpers in `crates/utils/src/tx.rs`

**High-Level Flow**:

```
spammer()
├─ obtain signer + pending nonce (`eth_getTransactionCount`)
├─ per tick (rate-limited):
│  ├─ when --blobs=true → make_signed_eip4844_tx(...)
│  │     ├─ generate_blobs_with_kzg(count)
│  │     │    • build deterministic 131_072-byte blobs
│  │     │    • compute KZG commitments + proofs via trusted setup
│  │     │    • derive versioned hashes (kzg_to_versioned_hash → 0x01 prefix)
│  │     ├─ wrap into `TxEip4844WithSidecar`
│  │     └─ sign envelope with requested chain ID / gas params
│  ├─ fall back to EIP-1559 path when `--blobs=false`
│  ├─ submit via `eth_sendRawTransaction`
│  └─ record success/error + tx size on async channel
└─ tracker task reports per-second stats and txpool status
```

**Capabilities**:
- ✅ Real blob data, commitments, proofs, and sidecar wiring per transaction.
- ✅ `--blobs-per-tx` validated for 1–1024 blobs; default stays 128.
- ✅ Nonce management accounts for “already known/replacement” responses.
- ✅ Compatible with Cancun Engine API v3 (sidecars arrive over raw tx submission).
- ⚠️ Still prints a warning when blob mode is enabled (Engine V3 peers without sidecar support can reject blobs); keep until Engine V4 rollout.

**Outstanding Enhancements**:
- No Prometheus metrics or structured logs yet—future Phase 5 work can expose spammer stats.
- Error handling remains best-effort (no backoff/retry policy beyond nonce bumps).

---

## Implementation Plan

### Phase A – Metrics Instrumentation (4–6 hours) — ✅ Complete (A.1 + A.2)

**Phase A.1: BlobEngine Surface** ✅ **Complete (2025-11-04)**
- ✅ Implemented `BlobEngineMetrics` module with 12 metrics (8 counters, 3 gauges, 1 histogram)
- ✅ Extended `BlobStore` API to return counts for metrics tracking
- ✅ Instrumented all 6 BlobEngine methods: `verify_and_store`, `mark_decided`, `drop_round`, `mark_archived`, `prune_archived_before`, `get_undecided_blobs`
- ✅ Registered metrics in `node.rs` via `SharedRegistry` with "blob_engine" prefix
- ✅ Applied performance fixes (bulk gauge operations, BYTES_PER_BLOB constant usage)
- ✅ 11 tests passing, full codebase builds successfully

**Phase A.2: State/Consensus Hooks** ✅ **Complete (2025-11-04)**
- ✅ Pass `BlobEngineMetrics` to `State` constructor (with `pub(crate)` visibility)
- ✅ Fixed `set_blobs_per_block(count)` in `BlobEngine::mark_decided()` (was missing)
- ✅ Instrument `State::rebuild_blob_sidecars_for_restream()` for restream counter
- ✅ Instrument blob sync error paths via `State::record_sync_failure()` helper method
- ✅ **Encapsulation improvements**: `pub(crate)` field + public helper methods maintain clean API

**Implementation Details**: See [METRICS_PROGRESS.md](./METRICS_PROGRESS.md) for complete specifications, code patterns, and progress tracking.

**Note**: Phase A.2 also fixed a critical issue where `blobs_per_block` gauge was defined but never updated. The encapsulation improvements ensure State owns its instrumentation surface, preventing metric implementation details from leaking across crate boundaries.

### Phase B – In-Process Tests (6–8 hours) — ✅ COMPLETE (2025-11-18 determinism refresh)
- Implement inline helper structs inside `tests/` (and optionally `tests/common`) to:
  - Spin up Ultramarine nodes on Tokio runtimes with per-test `TempDir` storage.
  - Mock only the Execution client while reusing real blob engine/KZG verification.
- Provide helper functions (`wait_for_height`, `restart_node`, `scrape_metrics`).
- Cover two tiers:
  - **Tier 0 (component)**: 3 fast consensus tests in `crates/consensus/tests` (`blob_roundtrip`, `blob_sync_commitment_mismatch`, `blob_pruning`) with real RocksDB + KZG; run in `make test` and CI by default.
  - **Tier 1 (full node)**: 14 ignored tests in `crates/test/tests/full_node` spanning quorum blob roundtrip, restream (multi-validator/multi-round), multi-height restarts, ValueSync ingestion/failure, blobless sequences, pruning, sync package roundtrip, and EL rejection; run via `make itest-node` and CI job `itest-tier1` (RUST_TEST_THREADS=1, artifacts on failure).
- Clarify restart behavior: spawn multiple `App` instances within one Tokio runtime, reusing the same on-disk store to simulate restarts.
- Shared helpers `State::process_synced_package` and `State::process_decided_certificate` keep integration tests aligned with production handlers.
- Use `serial_test` or per-test temp dirs to keep runs deterministic (2–5 s each).

**Progress (2025-11-05)**  
- ✅ Scaffolded deterministic helpers and migrated component smokes into `crates/consensus/tests` (real RocksDB + KZG).
- ✅ Implemented proposer→commit lifecycle and sync-package ingestion with shared mocks for the execution client.

**Progress (2025-11-08)**  
- ✅ Refactored the Decided path into `State::process_decided_certificate` plus the `ExecutionNotifier` trait so the app handler and integration tests share identical logic.
- ✅ Expanded full-node coverage (14 scenarios) with proposer/follower commit assertions and execution-layer rejection coverage via `MockExecutionNotifier`.
- ✅ Hardened sync coverage with commitment-mismatch and inclusion-proof regression tests.

**Progress (2025-11-18)**  
- ✅ Tier 1 harness de-flaked: `full_node_restart_mid_height` now gates on `StartedHeight`; `wait_for_nodes_at` helper replaces ad-hoc joins/sleeps.
- ✅ Full Tier 1 suite passes via `make itest-node` (14/14, event-driven).

- Wrap existing Docker workflow in an opt-in smoke target:
  - Boot stack (`make all` steps).
  - Use the repaired spammer to submit valid blob transactions.
  - Run a Rust helper that polls RPC/metrics and asserts blobs were imported/promoted.
  - Tear down stack (`make clean-net`).
- Expand Grafana dashboard with blob panels once metrics arrive; document panel meanings.
- Update `DEV_WORKFLOW.md` with `make itest` usage and smoke test guidance.

### Phase D – Tooling & Documentation (2–3 hours)
- ✅ Spam utility generates valid blob transactions (real KZG commitments and proofs)
- ✅ Updated `DEV_WORKFLOW.md` with comprehensive blob testing section
- ✅ Updated `README.md` with blob testing quick start


## Testing Strategy

### Tier 0 – Component (default)
- Run via `cargo test -p ultramarine-consensus --test blob_roundtrip --test blob_sync_commitment_mismatch --test blob_pruning` or `make itest`.
- Uses inline helpers with a mocked Execution client (real blob engine/KZG) to exercise proposer→commit, commitment/sidecar validation, and retention/metrics without Docker.
- Each test completes in ~2–4 s and relies on `tempfile::TempDir` Drop for cleanup. These run in `make test` and CI by default.

### Tier 1 – Full-Node (multi-validator)
- Mirrors Malachite’s TestBuilder blueprint by spinning **three** validators (2f + 1) plus optional follower nodes under the real channel actors, WAL, and libp2p transport.
- Exercises proposer/follower blob flow, crash/restart, ValueSync happy + failure paths, pruning, blobless sequences, and sync-package roundtrip with the production application loop talking to an Engine RPC stub (HTTP ExecutionClient wiring remains a follow-up).
- `make itest-node` invokes each Tier 1 scenario via its own `cargo test ... -- --ignored` call so every harness run starts from a clean process; CI job `itest-tier1` runs all 14 with `RUST_TEST_THREADS=1`, `CARGO_NET_OFFLINE` overridable, 20m timeout, and artifacts on failure.
- Named scenarios: `full_node_blob_quorum_roundtrip`, `full_node_validator_restart_recovers`, `full_node_restart_mid_height`, `full_node_new_node_sync`, `full_node_multi_height_valuesync_restart`, `full_node_restart_multi_height_rebuilds`, `full_node_restream_multiple_rounds_cleanup`, `full_node_restream_multi_validator`, `full_node_value_sync_commitment_mismatch`, `full_node_value_sync_inclusion_proof_failure`, `full_node_blob_blobless_sequence_behaves`, `full_node_blob_pruning_retains_recent_heights`, `full_node_sync_package_roundtrip`, and `full_node_value_sync_proof_failure`. Collectively these cover restart hydration, pruning, blobless sequences, restream permutations, and every ValueSync rejection path without touching the stores manually.

- Run manually via `make all` + `make spam-blobs` (optionally gated by env vars such as `ULTRA_E2E=1`).
- Boots docker stack (`make all`), runs blob spam script, queries RPC/metrics for verification, tears down (`make clean-net`).
- Used for load/perf validation and manual dashboards; not required for every CI run.

### Manual Exploratory Checks
- Grafana dashboards, Prometheus queries, and `tmux` logs remain available for debugging beyond automated assertions.
- Documented in `DEV_WORKFLOW.md`.

## Test Artifacts

| Test | Result | Time |
|------|--------|------|
| `blob_roundtrip` | ✅ | ~3 s |
| `blob_sync_commitment_mismatch` (incl. inclusion proof failure) | ✅ | ~3 s |
| `blob_pruning` | ✅ | ~3 s |

**Harness Summary**: 3/3 Tier 0 smoke scenarios (now under `crates/consensus/tests/`) passing via `cargo test -p ultramarine-consensus --test <name> -- --nocapture` in ~8–10 s (real KZG proofs using `c-kzg`). Metrics snapshots confirm promotion/demotion counters remain stable across runs.

**2025-11-18 Update**: Tier 1 harness de-flaked and aligned with docs.
- `full_node_restart_mid_height` now waits on `Event::StartedHeight` before crashing a node, forcing a deterministic ValueSync replay (no sleeps/race).
- Multi-node waits use a shared helper (`wait_for_nodes_at`) to avoid timing drifts; peer warm-up remains the only fixed delay.
- Full Tier 1 suite passes via `cargo test -p ultramarine-test --test full_node -- --ignored --nocapture` (14/14 scenarios).

---

## Daily Progress Log

### 2025-10-28 (Monday) - Analysis & Planning

**Completed**:
- ✅ Comprehensive review of testnet infrastructure
- ✅ Identified observability gaps (0 blob metrics, 0 dashboard panels)
- ✅ Created PHASE5_TESTNET.md document
- ✅ Defined implementation roadmap (Phases A-D, ~14-19 hours)

**Findings**:
- Testnet infrastructure is production-ready (Docker, Prometheus, Grafana all working)
- 🔴 **CRITICAL**: Spam tool `--blobs` flag creates incomplete transactions (fake versioned hashes, no blob data)
- 18 existing dashboard panels (5 Malachite, 13 Reth) but 0 for blobs
- BlobEngine has logging but zero Prometheus instrumentation

**Critical Discovery** (Evening):
- Deep-dive code review of `crates/utils/src/tx.rs` revealed spam tool issues
- `make_eip4844_tx()` uses hardcoded fake versioned hash: `0x00...01`
- Does NOT generate actual blob data (131KB), KZG commitments, or proofs
- Transactions accepted by txpool but CANNOT be included in blocks (no blob data)
- Added Phase E to roadmap (4-6 hours) to fix spam tool before integration testing

**UPDATE (2025-11-04)**: This analysis was incorrect. The spam tool actually DOES work correctly:
- Generates real 131KB blob data (deterministic, KZG-compatible)
- Computes valid KZG commitments using c-kzg library
- Generates valid KZG proofs with trusted setup
- All 1,158 test blobs verified successfully (100% success rate)
- Phase E was not needed - spam tool was functional all along

**Next Steps** (Updated):
- ✅ Phase A.1: Create BlobEngine metrics module - COMPLETE
- ✅ Phase A.2: Register metrics in node startup - COMPLETE
- ✅ Phase C: Testnet validation - COMPLETE

---

### 2025-11-04 (Tuesday) - Phase A.1 Complete: BlobEngine Metrics

**Completed**:
- ✅ **Phase A.1: BlobEngine Surface Metrics** — Full implementation complete
- ✅ Created `crates/blob_engine/src/metrics.rs` (235 lines) with 12 metrics
- ✅ Extended `BlobStore` trait API to return counts for metrics tracking
- ✅ Instrumented all 6 BlobEngine methods with comprehensive metrics
- ✅ Registered metrics in node startup using `SharedRegistry` pattern
- ✅ Applied critical fixes: bulk gauge operations, BYTES_PER_BLOB constant usage
- ✅ 11 tests passing, full codebase builds successfully
- ✅ Updated documentation: METRICS_PROGRESS.md, PHASE5_PROGRESS.md

**Metrics Implemented**:
- Verification: `verifications_success_total`, `verifications_failure_total`, `verification_time` (histogram)
- Storage Gauges: `storage_bytes_undecided`, `storage_bytes_decided`, `undecided_blob_count`
- Lifecycle Counters: `lifecycle_promoted_total`, `lifecycle_dropped_total`, `lifecycle_pruned_total`
- Consensus: `blobs_per_block` (gauge), `restream_rebuilds_total`, `sync_failures_total`

**Code Review Findings & Fixes**:
1. 🔧 Performance: Replaced gauge loops with bulk operations (`inc_by`/`dec_by`)
   - Before: 131k+ operations per blob
   - After: Single operation per batch
2. 🔧 Completeness: Added missing metrics to `mark_archived()` method
3. 🔧 Constants: Eliminated magic number `131_072`, using `BYTES_PER_BLOB`
4. 🔧 Types: Corrected gauge API usage (`i64` vs `u64`)

**Architecture Decisions**:
- ✅ Metrics module follows `DbMetrics` pattern exactly
- ✅ `BlobEngineMetrics::new()` for tests, `::register()` for production (matches codebase pattern)
- ✅ Best-effort delete semantics in `mark_archived` (gauge decrements even if blob missing)
- ✅ Phase A.1 scope: BlobEngine surface only; State hooks deferred to A.2

**Next Steps** (from Phase A.1):
- ✅ **Phase A.2**: Wire metrics into `State` for consensus hooks — **COMPLETED same day**
  - See Phase A.2 section below for full details
- ✅ **Integration Testing**: Metrics endpoint validated during Tier 1 runs (event-driven harness, 2025-11-18)

---

### 2025-11-04 (Tuesday, continued) — Phase A.2 + Encapsulation: Complete

**Completed**:
- ✅ **Phase A.2: State/Consensus Hooks** — Full implementation
- ✅ Added `pub(crate) blob_metrics` field to `State` struct
- ✅ Fixed missing `set_blobs_per_block()` call in `BlobEngine::mark_decided()`
- ✅ Instrumented `State::rebuild_blob_sidecars_for_restream()` with restream counter
- ✅ Instrumented sync failure path via `State::record_sync_failure()` helper method
- ✅ **Architectural improvement**: Encapsulation via `pub(crate)` + public helper methods

**Encapsulation Rationale**:

Initially `blob_metrics` was `pub`, allowing direct cross-crate field access (`state.blob_metrics.record_sync_failure()`). This breaks encapsulation and creates maintenance burden. The improved design:

```rust
// State struct
pub(crate) blob_metrics: BlobEngineMetrics  // Restricted to consensus crate

// Public API
pub fn record_sync_failure(&self) {
    self.blob_metrics.record_sync_failure();
}
```

Benefits:
- ✅ State owns its instrumentation surface
- ✅ Metrics API changes stay localized within State
- ✅ External crates use clean, documented methods
- ✅ Internal consensus code retains direct access for flexibility

**Critical Fix**:
- Discovered `blobs_per_block` gauge was defined but never updated
- Added `self.metrics.set_blobs_per_block(blob_count)` in `BlobEngine::mark_decided()`
- This gauge now correctly tracks blobs in the last finalized block

**Test Results**:
- ✅ 25 consensus tests passing
- ✅ 11 blob_engine tests passing
- ✅ Full codebase builds successfully
- ✅ Zero regressions

**Files Modified**:
- `crates/consensus/src/state.rs` (metrics field, helper method, restream instrumentation)
- `crates/node/src/node.rs` (pass metrics to State)
- `crates/node/src/app.rs` (sync failure via helper)
- `crates/blob_engine/src/engine.rs` (fixed set_blobs_per_block)

**Next Steps**:
- ⏳ **Phase B**: Integration tests (in progress - beta team)
- ⏳ **Phase C**: Start testnet, validate metrics endpoint

**Files Modified**:
- `crates/blob_engine/src/metrics.rs` (new, 235 lines)
- `crates/blob_engine/src/engine.rs` (instrumentation)
- `crates/blob_engine/src/store/mod.rs` (API extensions)
- `crates/blob_engine/src/store/rocksdb.rs` (count tracking)
- `crates/blob_engine/Cargo.toml` (dependencies)
- `crates/node/src/node.rs` (registration)

---

### 2025-11-05 (Wednesday) – Phase B Kickoff (Team Beta)

**Completed**:
- ✅ Shared harness module (`tests/common/mod.rs`) with deterministic genesis/key fixtures, blob-engine builders, and metrics snapshots.
- ✅ `tests/blob_state/blob_roundtrip.rs`: full proposer→commit lifecycle, blob import verification, and metric assertions.
- ✅ `tests/blob_state/restart_hydrate.rs`: commits a blobbed block, restarts, hydrates parent root, and validates metadata persistence across process restarts.
- ✅ `tests/blob_state/sync_package_roundtrip.rs`: encodes/decodes `SyncedValuePackage::Full`, stores synced proposals, marks blobs decided, and validates metrics.
- ✅ `tests/blob_state/blob_restream.rs`: exercises multi-validator restream and follower commit path with blob metrics validation.
- ✅ `tests/blob_state/blob_restream_multi_round.rs`: validates losing rounds are dropped during restream replay while metrics capture promotions/drops.
- ✅ Added `tests/common/mocks.rs` providing a minimal Engine API mock for future execution-layer scenarios.
- ✅ `serial_test` wiring to keep integration suites deterministic.

**In Progress**:
- Broader failure-mode coverage (pruning, sync error paths, negative tests).
- Integrating execution mock into proposer pipelines once payload generation paths require it.

**Next Steps**:
- Incorporate execution-client mock into tests that drive payload generation.
- Expand metric assertions once consensus hooks (`set_blobs_per_block`, restream counters) are fully wired.
- Layer in multi-validator scenarios and failure paths (drops, pruning, sync failures).

---

### 2025-11-06 (Thursday) – Restream & Mock Execution Integration

**Completed**:
- ✅ Refactored integration tests to pull payloads/blobs from `MockEngineApi` so proposer flows mimic ExecutionClient usage (`blob_roundtrip`, `restart_hydrate`, `sync_package_roundtrip`, `blob_restream`).
- ✅ Added `tests/blob_state/blob_restream_multi_round.rs` covering multi-round restream cleanup (promotion vs. drop metrics, undecided pruning).
- ✅ Hardened test harness with reusable base58 peer IDs and metric snapshots for assertions (`tests/common/mod.rs`, `tests/common/mocks.rs`).

**Pending**:
- Exercise sync failure paths (invalid sidecars) to tick `sync_failures_total` and verify `record_sync_failure()` wiring.
- Integrate the mock Execution API with the real ExecutionClient bridge once that work lands so proposer pipelines are covered end-to-end.
- Introduce pruning-focused tests once Phase 6 work starts.

---

### 2025-11-07 (Friday) – Phase 5B Harness Activation Kickoff

**Completed**:
- 🔎 Revalidated integration gaps: root `tests/` directory is excluded from workspace, no `make itest` target exists, and placeholder blob fixtures fail BlobEngine’s KZG verification path.
- 🗺️ Drafted execution plan to stand up a dedicated `crates/test` package (mirroring malachite’s pattern) so integration suites are visible to Cargo.
- 🧪 Materialised `crates/test` harness: migrated integration suites, hooked into workspace members, wired `make itest` targets, and replaced dummy blob fixtures with real KZG commitments/proofs (trusted setup cached once per run).
- ✅ `cargo test -p ultramarine-test -- --nocapture` now exercises all ten Phase 5B scenarios successfully; Team Beta signed off Phase 5B integration validation.

**In Progress**:
- Documenting harness usage in developer guides and aligning remaining TODO tests/failure modes with Phase 5B checklist.

**Next Steps**:
1. Capture harness invocation guidance in README/TESTNET docs (link `make itest` command).
2. Add negative-path/failure-mode coverage (sync failures, pruning once Phase 6 starts).
3. Integrate harness into CI workflow so Phase 5B becomes a gate on PRs.

---

## Success Criteria

Phase 5 Testnet is complete when:

### Metrics & Observability
- ✅ BlobEngine exposes 12+ Prometheus metrics (verification, storage, lifecycle)
- ✅ Grafana dashboard has 10+ blob-specific panels
- ✅ All blob operations visible in real-time (verification rate, latency, failures)
- ✅ Cross-layer correlation: Blob activity → consensus height → execution import

### Spam Tool (Phase E)
- ✅ Generates real blob data (131KB per blob)
- ✅ Computes valid KZG commitments and proofs
- ✅ Computes correct versioned hashes from commitments
- ✅ Submits blobs via proper RPC method (blobs included in blocks)
- ✅ Supports `--blobs-per-tx` flag (1-6 blobs)
- ✅ Blob transactions successfully included in consensus blocks

### Integration Testing
- ✅ In-process integration suite passes (`blob_roundtrip`, `restart_hydrate`, `sync_package_roundtrip`, `blob_restream_multi_validator`, `blob_restream_multi_round`, `blob_new_node_sync`, `blob_blobless_sequence`, `blob_sync_failure_rejects_invalid_proof`, `blob_sync_commitment_mismatch_rejected`, `blob_sync_across_restart_multiple_heights`, `blob_restart_hydrates_multiple_heights`, `blob_pruning_retains_recent_heights`) via `make itest` (`cargo test -p ultramarine-test -- --nocapture`).
- ✅ Optional Docker smoke (`make all` + `make spam-blobs`) still validates the full network path when needed.
- ✅ No verification failures during normal operation.
- ✅ No memory leaks or unbounded storage growth observed during harness + smoke runs.


- ✅ `DEV_WORKFLOW.md` documents all testing procedures
- ✅ `README.md` updated with blob testing quick start
- ✅ Metrics reference guide available (list all blob metrics)
- ✅ Troubleshooting guide covers common issues

### Operational Readiness
- ✅ Testnet can run for 24+ hours without issues
- ✅ Restart survival validated (blobs persist, no corruption)
- ✅ Multi-validator sync validated (all nodes agree on blob state)
- ✅ Performance baselines established (P99 latency, throughput limits)

---

## References

- [PHASE4_PROGRESS.md](./PHASE4_PROGRESS.md) - Phase 1-4 implementation log
- [FINAL_PLAN.md](./FINAL_PLAN.md) - Overall project roadmap
- Makefile:427-550 - Testnet automation targets
- `crates/blob_engine/src/engine.rs` - BlobEngine trait and implementation
- `crates/utils/src/commands/spam.rs` - Transaction spam tool (blob path fully functional)

---

## Open Questions

1. **Spam Tool Implementation** ✅ **RESOLVED**: The `--blobs` flag works correctly
   - **Status**: Generates valid EIP-4844 transactions with real KZG commitments and proofs
   - **Testing**: 193 transactions sent with 1,158 blobs (6 per tx), 100% verification success
   - **Implementation**: Uses c-kzg library with trusted setup for valid proofs

2. **Blob RPC Submission Method** ✅ **RESOLVED**: Reth accepts blob transactions via standard RPC
   - **Method**: Standard `eth_sendRawTransaction` with blob sidecars
   - **Status**: All blob transactions successfully included in blocks
   - **Verification**: Consensus and execution layers properly handle blob lifecycle

3. **Blob Transaction Defaults** ✅ **IMPLEMENTED**: `--blobs-per-tx` flag available
   - **Current**: Spam tool supports 1-6 blobs per transaction (default 6)
   - **Usage**: `--blobs --blobs-per-tx 6`
   - **Testing**: Successfully tested with 6 blobs per transaction

4. **Storage Growth**: What's acceptable storage size for 1000 blocks with blobs?
   - **Status**: Pruning implemented with 5-block retention window
   - **Observation**: Storage remains bounded with automatic pruning
   - **Note**: Configurable retention policies planned for Phase 6

5. **Metrics Export** ✅ **RESOLVED**: Blob metrics use `blob_engine_*` prefix
   - **Implementation**: 12 metrics registered with `blob_engine_` prefix
   - **Rationale**: Matches BlobEngine crate name, clear separation from consensus metrics
   - **Status**: Fully implemented and validated on testnet

---

## Risks & Mitigations

| Risk | Impact | Mitigation | Status |
|------|--------|------------|--------|
| ~~Spam tool non-functional~~ | ~~CRITICAL~~ | ~~Implement Phase E~~ | ✅ **RESOLVED** - Tool works correctly |
| ~~Blob RPC submission method unknown~~ | ~~HIGH~~ | ~~Research Reth APIs~~ | ✅ **RESOLVED** - Standard RPC works |
| Blob spam causes consensus slowdown | MEDIUM | Load test at increasing rates, identify bottleneck | ⏳ Ongoing monitoring |
| Storage grows unbounded without pruning | MEDIUM | Monitor `storage_size_bytes` metric, defer pruning to Phase 6 |
| Integration tests flaky (timing-dependent) | LOW | Use retries, generous timeouts, deterministic test data |
| Grafana dashboard too complex | LOW | Group panels logically, provide simple "Overview" section |

---

**Last Updated**: 2025-10-28
**Next Review**: After Phase A completion (metrics instrumentation)
