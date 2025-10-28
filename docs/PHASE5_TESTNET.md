# Phase 5 Testnet: Integration Testing & Blob Observability

**Status**: 🔄 In Progress
**Started**: 2025-10-28
**Goal**: Validate blob sidecar integration end-to-end, add comprehensive observability, and establish testnet workflow for production readiness.

---

## Table of Contents

1. [Overview](#overview)
2. [Design Philosophy](#design-philosophy)
3. [Current State Analysis](#current-state-analysis)
4. [Implementation Plan](#implementation-plan)
5. [Daily Progress Log](#daily-progress-log)
6. [Testing Strategy](#testing-strategy)
7. [Success Criteria](#success-criteria)

---

## Overview

### Motivation

Phases 1-4 delivered a complete blob sidecar implementation with 23/23 passing unit tests. Phase 5-5.2 fixed critical bugs for live consensus. However, **blob behavior is invisible** - we have:

- ✅ Working 3-node testnet infrastructure
- ⚠️ Spam tool with `--blobs` flag exists, but currently produces invalid blob transactions (Phase E fixes this)
- ❌ **Zero blob-specific metrics** (verification time, storage size, lifecycle transitions)
- ❌ **Zero blob dashboard panels** (Grafana has 18 panels, none for blobs)
- ❌ **Zero integration tests** (unit tests pass, but no E2E blob flow validation)

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

### Spam Tool (⚠️ Incomplete Blob Support)

**Location**: `crates/utils/src/commands/spam.rs`, `crates/utils/src/tx.rs`

**Status**: ❌ **NOT FUNCTIONAL** - Creates incomplete blob transactions

**Current Implementation** (tx.rs:33-49):
```rust
pub(crate) fn make_eip4844_tx(nonce: u64, to: Address, chain_id: u64) -> TxEip4844 {
    TxEip4844 {
        // ... standard fields ...
        blob_versioned_hashes: vec![b256!(
            "0000000000000000000000000000000000000000000000000000000000000001"
        )],  // ← HARDCODED FAKE HASH!
        max_fee_per_blob_gas: 20_000_000_000,
    }
}
```

**What It Does**:
- ✅ Creates EIP-4844 transaction (type 3)
- ✅ Sets `blob_versioned_hashes` field
- ✅ Sets `max_fee_per_blob_gas`
- ❌ **Uses hardcoded fake versioned hash** (`0x00...01`)
- ❌ **Does NOT include actual blob data** (131KB)
- ❌ **Does NOT generate KZG commitments or proofs**
- ❌ **Does NOT submit blobs via proper RPC method**

**What Happens**:
1. Transaction gets submitted to txpool ✅
2. Txpool accepts it (type check passes) ✅
3. **BUT**: When block builder tries to include it, execution layer has no blob data ❌
4. **Result**: Transaction sits in txpool but **cannot be included in blocks** ❌

**Warning is Accurate** (spam.rs:48-50):
```rust
eprintln!(
    "[warning] EIP-4844 blob transactions enabled. On Cancun (engine V3),
    non-proposer imports may fail without sidecar support; expect inconsistent
    behavior until Engine API V4 sidecar wiring is implemented."
);
```

**Critical Finding**: The spam tool needs significant work before it can test blob sidecars. See Phase E below.

---

## Implementation Plan

### Phase A – Metrics Instrumentation (4–6 hours)
- Implement `BlobEngineMetrics` with counters/gauges/histograms for verification, promotion, storage bytes, restream rebuilds, and failures.
- Feed metrics from `BlobEngineImpl::{verify_and_store, mark_decided, get_for_import, drop_round}` and consensus handlers.
- Register metrics in the node bootstrap (`crates/node/src/node.rs`) so Prometheus scrapes them by default.
- Produce a metric reference (name, help, units) for docs and dashboards (**TODO**: add table once metrics land).

### Phase B – In-Process Tests (6–8 hours)
- Implement inline helper structs inside `tests/` (and optionally `tests/common`) to:
  - Spin up Ultramarine nodes on Tokio runtimes with per-test `TempDir` storage.
  - Mock only the Execution client while reusing real blob engine/KZG verification.
- Provide helper functions (`wait_for_height`, `restart_node`, `scrape_metrics`).
- Add `#[tokio::test] #[ignore]` cases:
  1. `blob_roundtrip` – proposer stores blobs, receivers restream, commit promotes metadata, restream rebuild matches commitments and metrics.
  2. `restart_hydrate` – commit a blobbed block, drop/restart node, run `hydrate_blob_parent_root`, assert cache + metadata alignment.
  3. `sync_package_roundtrip` – ingest `SyncedValuePackage::Full`, verify immediate promotion/availability.
- Clarify restart behavior: spawn multiple `App` instances within one Tokio runtime, reusing the same on-disk store to simulate restarts.
- Use `serial_test` or per-test temp dirs to keep runs deterministic (2–5 s each).

### Phase C – Optional Full-Stack Smoke & Observability (4–6 hours)
- Wrap existing Docker workflow in `make itest-full`:
  - Boot stack (`make all` steps).
  - Use the repaired spammer to submit valid blob transactions.
  - Run a Rust helper that polls RPC/metrics and asserts blobs were imported/promoted.
  - Tear down stack (`make clean-net`).
- Expand Grafana dashboard with blob panels once metrics arrive; document panel meanings.
- Update `DEV_WORKFLOW.md` and new `TESTNET_WORKFLOW.md` with `make itest` vs `make itest-full` usage.

### Phase D – Tooling & Documentation (2–3 hours)
- Fix spam utility (Phase E details below) to generate real blobs and versioned hashes (likely via Alloy blob helpers or Reth transaction builder APIs).
- Publish `TESTNET_WORKFLOW.md` capturing scenarios, make targets, and troubleshooting.
- Refresh `README.md` / `FINAL_PLAN.md` to point at harness tests, metrics, dashboards.


## Testing Strategy

### Tier 1 – In-Process (default)
- Run via `cargo test --test '*blob*' -- --ignored` or `make itest`.
- Uses inline helpers with a mocked Execution client (real blob engine/KZG) to exercise consensus and restream flows without Docker.
- Covers:
  1. `blob_roundtrip`
  2. `restart_hydrate`
  3. `sync_package_roundtrip`
- Each test completes in ~2–5 s and relies on `tempfile::TempDir` Drop for cleanup.
- Assertions rely on store state, blob engine metrics, and deterministic logs.

### Tier 2 – Full Stack (opt-in)
- Run via `make itest-full` (gated by env var, e.g., `ULTRA_E2E=1`).
- Boots docker stack (`make all`), runs blob spam script, queries RPC/metrics for verification, tears down (`make clean-net`).
- Used for load/perf validation and manual dashboards; not required for every CI run.

### Manual Exploratory Checks
- Grafana dashboards, Prometheus queries, and `tmux` logs remain available for debugging beyond automated assertions.
- Documented in `TESTNET_WORKFLOW.md`.

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

**Next Steps**:
- **Phase E**: Fix spam tool (4-6 hours) - CRITICAL for integration testing
- Phase A.1: Create BlobEngine metrics module
- Phase A.2: Register metrics in node startup
- ~~Quick win: Test current blob spam behavior~~ - **Blocked: spam tool non-functional**

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
- ✅ In-process integration suite passes (`blob_roundtrip`, `restart_hydrate`, `sync_package_roundtrip`) via `cargo test --test '*blob*' -- --ignored`.
- ✅ Optional docker smoke (`make itest-full`) verifies real network path and blob imports.
- ✅ No verification failures during normal operation.
- ✅ No memory leaks or unbounded storage growth observed during harness + smoke runs.


### Documentation
- ✅ `TESTNET_WORKFLOW.md` documents all testing procedures
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
- `crates/utils/src/commands/spam.rs` - Transaction spam tool (blob path functional after Phase E)

---

## Open Questions

1. **Spam Tool Implementation** ✅ **ANSWERED**: The `--blobs` flag does NOT work
   - **Finding**: Creates incomplete EIP-4844 transactions with fake versioned hashes
   - **Impact**: Cannot test blob sidecars until spam tool is fixed (Phase E)
   - **Action**: Implement Phase E (4-6 hours) before integration testing

2. **Blob RPC Submission Method**: How does Reth accept blob transactions?
   - **Research Needed**: Check if Reth supports `eth_sendBlobTransaction` or requires Alloy's blob sidecar API
   - **Blocked By**: Phase E.5 implementation
   - **Priority**: HIGH (required for spam tool to work)

3. **Blob Transaction Defaults**: What's a realistic blob count per transaction?
   - **Current**: Spam tool hardcodes 1 blob with fake hash
   - **Recommendation**: Add `--blobs-per-tx` flag (default 3, range 1-6) in Phase E.6
   - **Rationale**: Ethereum mainnet averages 2-4 blobs per block

4. **Storage Growth**: What's acceptable storage size for 1000 blocks with blobs?
   - **Action**: Measure during load testing (after Phase E completion)
   - **Expected**: ~400 MB per 1000 blocks (3 blobs/tx avg, full blocks)

5. **Metrics Export**: Should blob metrics use separate prefix (`blob_engine_*`) or nest under app (`app_channel_blob_*`)?
   - **Recommendation**: Use `blob_engine_*` prefix for clarity, matches BlobEngine crate
   - **Decision**: Final in Phase A.1

---

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Spam tool non-functional** | 🔴 **CRITICAL** | **Implement Phase E before integration tests** (4-6h effort) |
| Blob RPC submission method unknown | HIGH | Research Reth blob APIs, check Alloy documentation (Phase E.5) |
| Blob spam causes consensus slowdown | HIGH | Load test at increasing rates, identify bottleneck |
| Storage grows unbounded without pruning | MEDIUM | Monitor `storage_size_bytes` metric, defer pruning to Phase 6 |
| Integration tests flaky (timing-dependent) | LOW | Use retries, generous timeouts, deterministic test data |
| Grafana dashboard too complex | LOW | Group panels logically, provide simple "Overview" section |

---

**Last Updated**: 2025-10-28
**Next Review**: After Phase A completion (metrics instrumentation)
