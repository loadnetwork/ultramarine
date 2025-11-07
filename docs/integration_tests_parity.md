# Integration Test Parity Plan

_Last updated: 2025-11-10_

Ultramarine currently ships fast, deterministic blob-focused integration tests that exercise
`State<TestBlobEngine>` with real RocksDB stores and KZG verification. These scenarios caught
multiple production bugs, but they terminate at the consensus state boundary: libp2p gossip,
Malachite channel actors, WAL, timers, and the execution bridge are replaced with lightweight
helpers or mocks. Malachite’s TestBuilder and Snapchain’s node harness spin full nodes (actors,
networking, WAL, storage) without mocks, so their integration suites provide end-to-end coverage
of the production stack.

This document tracks the work required to reach similar parity. The goal is to **keep** the
existing state-level tests as Tier 0 regression coverage and **add** a second tier that boots
real Ultramarine nodes and drives blob scenarios through `/proposal_parts` gossip, WAL, and the
execution bridge.

---

## 1. Current Coverage Snapshot

| Layer / Component                    | Exercised Today? | Notes |
|-------------------------------------|------------------|-------|
| Consensus `State` + BlobEngine      | ✅               | All Tier 0 scenarios (now under `crates/test/tests/blob_state`) run against the real state/engine with RocksDB. |
| Execution payload + blob verifier   | ✅               | Deterministic payloads, real KZG commitments/proofs via `c-kzg`. |
| Engine API bridge (generate block)  | ⚠️ Mocked        | [`MockEngineApi`](../crates/test/tests/common/mocks.rs) returns canned payloads/bundles; no Engine API client or EL node. |
| Execution notifier (FCU / payload)  | ⚠️ Mocked        | `MockExecutionNotifier` captures calls but does not touch the real ExecutionClient. |
| Malachite channel actors            | ❌               | Tests bypass `AppMsg`/`NetworkMsg` and call state methods directly. |
| libp2p gossip transport             | ❌               | No `/proposal_parts` streaming over the network stack. |
| WAL / timers / crash recovery paths | ❌               | Not covered; restarts simulated via store reopen only. |

---

## 2. Target Parity Goals

1. **Channel-Service Harness**  
   Run at least one Ultramarine node (consensus channel actors + WAL + libp2p) in-process,
   publish a blobbed proposal via `/proposal_parts`, and verify followers commit.

2. **Two-Node Scenario**  
   Extend the harness to two validators connected over loopback libp2p; cover proposer/follower,
   sync recovery, and restart hydration under real networking/timers.

3. **Execution Bridge Coverage**  
   Replace the current mock with the production ExecutionClient (pointed at an in-process or
   recorded Engine API stub) so `generate_block_with_blobs` and `notify_new_block` paths exercise
   the same code as a live node.

4. **Negative Paths Under Full Node**  
   Port at least the commitment-mismatch and invalid-proof scenarios to the full-node harness to
   ensure WAL cleanup + gossip error handling behave as expected.

5. **CI Integration**  
   Decide on cadence (e.g., nightly or gated job) so the heavier harness does not slow down the
   default developer loop but still catches regressions before release.

6. **(Optional) Ethereum Spec Compliance**  
   Once full-node parity lands, consider adding Deneb/Cancun-specific coverage (blobless payload
   fallback, `engine_getBlobsV1`, blob sidecar gossip APIs). Track as a future Phase P7 if needed.

---

## 3. Phased Work Plan

| Phase | Description | Deliverables & Acceptance Criteria | Owner | Est. Effort | Depends On | Status |
|-------|-------------|------------------------------------|-------|------------|------------|--------|
| P0 | Finalize parity scope & infra decisions | This doc, shared understanding of Tier 0 (`blob_state/`) vs Tier 1 (`full_node/`), and action list. | @team | 0.5 d | — | ✅ |
| P1 | Minimal node harness | `crates/test/tests/full_node/node_harness.rs` (or similar) that boots a single Ultramarine node (app loop + channel actors + WAL + real libp2p loopback) and an HTTP Engine stub. **Done when**: blobbed proposal travels through `/proposal_parts`, node commits, blob stored, test <30 s. | TBA | 2–3 d | P0 | ⏳ |
| P2 | Two-node flow | Extend harness to two validators exchanging over loopback. Reuse `blob_roundtrip` scenario end-to-end. **Done when**: proposer streams blobs, follower restreams/commits, metrics recorded. | TBA | 1–2 d | P1 | ⏳ |
| P3 | Sync & restart cases | Port `blob_sync_across_restart_multiple_heights` and `blob_restart_hydrates_multiple_heights` into Tier 1. **Done when**: restart path exercises WAL/timers (not just store reopen) and passes deterministically. | TBA | 2 d | P2 | ⏳ |
| P4 | Negative-path parity | Tier 1 versions of commitment mismatch, invalid proof, EL rejection. **Done when**: node logs/metrics show rejection, WAL cleanup verified. (Can run parallel w/ P3 once P2 is stable.) | TBA | 1–2 d | P2 | ⏳ |
| P5 (optional) | Execution bridge wiring | Replace `MockEngineApi` with HTTP Engine stub so the real `ExecutionClient` path runs unmodified; later optional upgrade to real reth devnet. **Done when**: `generate_block_with_blobs`, `notify_new_block`, `forkchoice_updated` go over HTTP stub. | TBA | 2–3 d | P2 | ⏳ |
| P6 (optional) | CI integration & docs | Update `DEV_WORKFLOW.md`, add `make itest-node`, decide CI cadence (manual/nightly/per-PR). **Done when**: documented instructions + optional CI job exist. | TBA | 1 d | P2–P5 | ⏳ |

---

## 4. Near-Term Action Items

| Item | Description | Owner | Priority | Status |
|------|-------------|-------|----------|--------|
| Tier 0 reorg | Move existing state tests into `crates/test/tests/blob_state/`, update Makefile/docs references. | @team | High | ✅ |
| Harness Skeleton (P1) | Build the single-node harness using real libp2p transport + WAL, modeled after Malachite’s TestBuilder. | TBA | High | ⏳ |
| Scenario Porting | Port `blob_roundtrip`/`blob_restream` to Tier 1 once harness exists. | TBA | High | ⏳ |
| Execution Bridge Stub | Implement HTTP Engine stub replacing [`MockEngineApi`](../crates/test/tests/common/mocks.rs) so `ExecutionClient` runs unmodified. | TBA | Medium | ⏳ |
| Docs & CI | Update `DEV_WORKFLOW.md`, add `make itest-node`, define CI cadence. | TBA | Medium | ⏳ |

---

## 5. Open Questions & Decision Process

| Question | Current Position | Decision Owner | Target Timing |
|----------|------------------|----------------|---------------|
| Transport choice | Use real libp2p TCP (same as Malachite/Snapchain). Revisit only if flakiness becomes unmanageable. | @you | Locked for P1 |
| Execution-layer strategy | Start with HTTP Engine stub that uses real `ExecutionClient`; optional later work to run against reth devnet. | @you | Before P5 |
| Runtime/CI budget | Measure after P2; default to manual/nightly runs unless suite <1 min. | @you | After P2 |

Decisions are recorded here; once you approve a direction it becomes part of scope.

---

## 6. Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| libp2p flakiness / port conflicts | Medium | High | Deterministic port allocator, retries, serialize Tier 1 tests initially. |
| Tier 1 suite too slow for per-PR | High | Medium | Keep Tier 0 for fast checks; run Tier 1 nightly until optimized. |
| Divergence between tiers | Low | Medium | Document scenario mapping; periodically run Tier 1 locally before releases. |
| Execution bridge complexity | Medium | High | Use HTTP stub first; defer real reth integration until harness stable. |
| Maintenance overhead | Medium | Medium | Share helpers between tiers, document setup, automate teardown. |

---

## 7. Notes

- **Tier strategy**: Tier 0 = state-level tests (`blob_state/`), Tier 1 = full-node tests (`full_node/`). Both remain in git; Tier 0 stays default for `make itest`, Tier 1 becomes `make itest-node`.
- **Architecture references**: Malachite’s `TestBuilder` (channel actors + libp2p) is a good blueprint; Snapchain’s consensus tests bind real TCP + gRPC services.
- **Next steps**: Confirm task ownership for P1, then begin renaming Tier 0 tests while scaffolding the new harness.
