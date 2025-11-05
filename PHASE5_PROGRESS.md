# Phase 5: Testnet & Observability — Progress Log

**Status**: ✅ Phase 5 Complete - All Sub-Phases Validated + Critical Bug Fixed
**Focus**: Metrics instrumentation, integration tests, full-stack observability, and comprehensive code review
**Last Updated**: 2025-11-08 (evening)

---

## 2025-11-08 (Saturday, evening) — Comprehensive Review & Critical Bug Fix

### Code Review Completed

- ✅ Conducted comprehensive end-to-end review of blob integration implementation
- ✅ Traced execution paths through all 13 integration tests, focusing on `blob_restream_multi_round`
- ✅ Cross-referenced implementation against EIP-4844, Tendermint/BFT specs, and Ethereum consensus specs
- ✅ Verified architectural decisions (three-layer metadata, blob streaming, Engine API v3)
- ✅ Validated code idiomaticity, error handling, and Rust best practices

### Critical Bug Discovered & Fixed

**🔴 CRITICAL BUG: Proposer Blob Round Cleanup Failure**

**Discovery**: During execution path tracing of `blob_restream_multi_round`, discovered that proposers never tracked their own blob rounds in the `blob_rounds` HashMap, causing failed rounds' blobs to accumulate indefinitely.

**Root Cause**:
- Followers track blob rounds via `received_proposal_part()` (state.rs:633) ✅
- Proposers create proposals via `propose_value_with_blobs()` but never tracked rounds ❌
- Result: Proposer cleanup logic (commit:1241-1284) had empty `blob_rounds`, never dropped failed rounds

**Impact**:
- Storage bloat (unbounded growth of undecided blobs)
- Memory leaks in RocksDB undecided tables
- Performance degradation over time
- Violated BFT symmetry principle

**Fix Applied** (state.rs:1462-1467):
```rust
// Track blob rounds for cleanup (proposer path)
// This mirrors the tracking in received_proposal_part() for followers
if has_blobs {
    let round_i64 = round.as_i64();
    self.blob_rounds.entry(height).or_default().insert(round_i64);
}
```

**Test Coverage Added** (blob_restream_multi_round:182-188):
```rust
// Verify proposer also cleaned up losing round blobs (Bug #1 fix verification)
let proposer_undecided =
    proposer.state.blob_engine().get_undecided_blobs(height, rounds[0].as_i64()).await?;
assert!(
    proposer_undecided.is_empty(),
    "proposer should also drop losing round blobs during commit"
);
```

**Verification**: ✅ Test passes (4.01s), proposer cleanup now works correctly

### Review Findings Summary

**Architecture** ✅ Excellent:
- Three-layer metadata separation (Consensus → Ethereum → Blobs) is sound
- Blob streaming via existing ProposalParts channel avoids network complexity
- Engine API v3 integration follows EIP-4844 correctly

**Implementation** ✅ High Quality:
- Idiomatic Rust with proper async/await patterns
- Comprehensive error handling with eyre::Result
- Protobuf serialization ensures network consistency
- Metrics instrumentation complete (12 blob-specific metrics)

**Test Coverage** ✅ Comprehensive:
- 13/13 integration tests passing
- Covers proposer/follower paths, restart survival, sync, pruning, negative cases
- Real KZG verification using c-kzg library

**Remaining Issues**:
- 🟡 Medium: `AppMsg::Decided` handler not exercised by tests (non-blocking)
- 🟢 Low: Magic number for retention (5) should be constant (Phase 6 will make configurable)

**Documentation Updates**:
- ✅ Added comprehensive bug analysis to `docs/phase5_test_bugs.md`
- ✅ Updated priority matrix with fix status
- ✅ Updated action items with completion dates
- ✅ This progress log entry

---

## 2025-11-08 (Saturday, morning) — Decided Helper & Negative Coverage

- ✅ Added `State::process_decided_certificate` and the `ExecutionNotifier` trait so the app handler and integration tests share a single Decided flow.
- ✅ Updated the integration suite to drive proposer and follower commits via the shared helper, including the new `blob_decided_el_rejection` regression test.
- ✅ `make itest` now runs 13 scenarios (~49 s on dev hardware) covering commitment mismatches, execution-layer rejection, pruning, restart hydration, and proposer restream paths.

---

## 2025-11-03 (Monday) — Metrics Plan Review & Dashboard Reset

- ✅ Restored Grafana provisioning from malaketh baseline (`monitoring/config-grafana/provisioning/`), dashboards now load with the original panel set.
- 🔍 Verified current panels rely only on core `app_channel_*` and Reth metrics; blob-specific queries intentionally absent until instrumentation lands.
- 📄 Reviewed `docs/METRICS_PROGRESS.md` and aligned it with the codebase:
  - Metric table maps directly onto `BlobStore` and consensus (`State::commit`, `State::rebuild_blob_sidecars_for_restream`) touch points, with separate counters for success/failure to stay within the `malachitebft` metrics API.
  - Key design notes flag the need to capture serialized blob sizes when inserting/pruning so storage gauges stay accurate; store helpers will have to return byte counts before promotion/pruning.
  - Instrumentation checklist now calls out consensus hooks (commit, restream rebuild, proposal assembly) so dashboards can correlate Tendermint lifecycle events with blob activity once emitted.
- 🧭 Confirmed the metrics plan reuses the existing `SharedRegistry` wiring in `node.rs`, keeping it compatible with current Prometheus/Grafana setup; future Grafana panels just need the updated metric names (`*_success_total`, `*_failure_total`, etc.).

### Next Actions
1. Implement `BlobEngineMetrics` module under `crates/blob_engine`, following the `DbMetrics` pattern.
2. Thread metrics registration through `node/src/node.rs` and pass `BlobEngineMetrics` into the blob engine/state constructors.
3. Instrument consensus paths (`State::assemble_and_store_blobs`, `State::commit`, restream rebuilds) so blob metrics carry height/round context.

---

## 2025-11-01 (Wednesday) — Metrics Implementation & Test Refactor

- ✅ Implemented `BlobEngineMetrics` with 12 separate counters and gauges using Prometheus primitives, aligned to the Phase 5 doc design.
- ✅ Instrumented all `BlobEngineImpl` paths: `verify_and_store`, `mark_decided`, `get_for_import`, `drop_round`, `mark_archived`, `prune_archived_before`.
- ✅ Wired up `BlobEngineMetrics` in consensus state creation, pulling metrics into the node's shared registry.
- ✅ Added `snapshot()` helper to expose metrics as plain-data struct for integration test assertions.
- ✅ Refactored integration test helpers to construct blob engine + metrics outside each test, reducing boilerplate from ~40 lines down to a single `build_state(validator, height)` call.
- 🔧 Disabled leftover debug-prints after confirming metric increments are captured correctly in mock scenarios.

---

## 2025-10-30 (Tuesday) — Rebase to Upgraded Malachite

- ✅ Rebased on new Malachite commit (b205f4252f3064d9a74716056f63834ff33f2de9) after channel/streaming overhaul.
- ✅ Fixed StreamMessage type signature changes: old `StreamMessage { stream_id, sequence, content }` now requires `StreamMessage::new(stream_id, sequence, content)`.
- ✅ Updated network gossip to use new `NetworkMsg::PublishProposalPart` variant for streaming proposal parts.
- ✅ Tests compile + run (all green locally), though Malachite upgrade introduced minor lifetime adjustment in proposal codec.
- 📝 Added note to final plan about using explicit constructors for `StreamMessage`.

---

## 2025-10-28 (Monday) — Sync + Restart Scenarios

- ✅ Added `blob_new_node_sync.rs` to validate full state-sync flow (proposer → lagging follower → commit with blobs).
- ✅ Implemented restart + hydration test (`restart_hydrate.rs`) to verify blobs persist across node crashes.
- ✅ Extended `blob_restream.rs` to cover the scenario where the follower misses the initial proposal part but receives it on retry.
- ✅ Verified `blob_engine.get_for_import()` returns correct sidecars after state restart (hydration from RocksDB).
- 🔧 Discovered missing parent-root chaining in restart path; adding seed helper (`State::hydrate_blob_parent_root`) to restore from decided metadata.

---

## 2025-10-25 (Friday) — Pruning Integration

- ✅ Added `prune_archived_before(height)` method to `BlobEngineImpl`.
- ✅ Integrated pruning into `State::commit`, triggering cleanup for blobs older than `height - retention`.
- ✅ Created `blob_pruning.rs` test to verify blobs are deleted from both decided + archived tables.
- 🔧 Fixed off-by-one in retention window: old code kept `height - 1`, now keeps last 5 heights as designed.
- 🐛 Fixed RocksDB range iterator to exclude empty entries (previously crashed on missing blob indices).

---

## 2025-10-22 (Tuesday) — Multi-Round Cleanup

- ✅ Added `drop_round(height, round)` method to `BlobEngineImpl`.
- ✅ Integrated round cleanup into `State::commit`, triggering `drop_round` for all non-winning rounds.
- ✅ Created `blob_restream_multi_round.rs` test to verify losing-round blobs are dropped after consensus decision.
- 🔧 Added `blob_rounds: HashMap<Height, HashSet<i64>>` to track which rounds have blobs at each height, enabling targeted cleanup.
- 🐛 Fixed metrics not counting dropped blobs; now increments `blobs_dropped_total` counter.

---

## 2025-10-18 (Friday) — Restart + Multi-Height Sync

- ✅ Added `blob_restart_multi_height.rs` + `blob_restart_multi_height_sync.rs` to test node restarts across multiple blocks with blobs.
- ✅ Verified blobs survive restart and can be retrieved after hydration.
- ✅ Tested sync scenario where a lagging node catches up multiple heights worth of blobs.
- 🔧 Fixed issue where `hydrate_blob_parent_root` wasn't called on restart, causing parent-root mismatch.

---

## 2025-10-15 (Tuesday) — Sync Package Roundtrip

- ✅ Added `sync_package_roundtrip.rs` to test full sync flow using `GetDecidedValue` / `ProcessSyncedValue`.
- ✅ Verified `SyncedValuePackage::Full` correctly bundles execution payload + blob sidecars.
- ✅ Tested metadata-only fallback for pruned blobs (Phase 6 preview).
- 🔧 Fixed double `mark_decided` call in sync path (idempotency now works correctly).

---

## 2025-10-12 (Saturday) — Phase 5 Kickoff

- ✅ Created Phase 5 progress log to track testnet preparation.
- ✅ Defined metrics instrumentation plan (12 blob-specific metrics).
- ✅ Set up Grafana dashboard scaffolding for blob observability.
- 📋 Established integration test checklist (14 scenarios planned, 13 completed as of 2025-11-08).
