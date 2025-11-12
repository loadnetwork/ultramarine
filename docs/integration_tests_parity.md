# Integration Test Parity Plan

_Last updated: 2025-11-07_

Ultramarine currently ships fast, deterministic blob-focused integration tests that exercise
`State<TestBlobEngine>` with real RocksDB stores and KZG verification. These scenarios caught
multiple production bugs, but they terminate at the consensus state boundary: libp2p gossip,
Malachite channel actors, WAL, timers, and the execution bridge are replaced with lightweight
helpers or mocks.

Both upstream projects we claim parity with already run multi-validator,
networked harnesses exclusively:

- **Malachite** always brings up at least three validators plus follower nodes inside the
  TestBuilder harness; even the “basic” scenario drives 3 validators and 2 followers to height 5
  and later tests mix crash/restart + sync behaviour (`malachite/code/crates/test/tests/it/full_nodes.rs:11-175`).
- **Snapchain** follows the same pattern: the consensus test harness wires libp2p gossip,
  gRPC services, RocksDB stores, and multiple nodes with deterministic port allocation before any
  assertions run (`snapchain/tests/consensus_test.rs:1-370`), and the suite is serialized via
  `serial_test` to avoid cross-talk.

Their approach means “Tier 1” coverage is inherently multi-node: leader election, quorum votes,
ValueSync, crash/restart, and networking are all exercised together. This document tracks the work
required to reach the same architecture instead of relying on single-node smoke tests.

This document tracks the work required to reach similar parity. The goal is to **keep** the
existing state-level tests as Tier 0 regression coverage and **add** a second tier that boots
real Ultramarine nodes and drives blob scenarios through `/proposal_parts` gossip, WAL, and the
execution bridge.

**Terminology**: The execution-layer (EL) bridge refers to the production `ExecutionClient`
(`crates/execution`) plus the Engine API calls (`forkchoiceUpdatedV3`, `getPayloadV3`,
`newPayloadV3`, blob bundles) that tie Malachite's Tendermint-style consensus actors to the EL.
Whenever this plan says “wire the EL bridge,” it means exercising that exact HTTP/IPC surface
instead of calling `MockEngineApi` directly.

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

2. **Multi-Validator Scenario**  
   Extend the harness to **three** validators (2f + 1) connected over loopback libp2p, plus optional
   follower full nodes. Cover proposer/follower flow, sync recovery, and restart hydration under
   real networking/timers. Single-node tests are explicitly out of scope and will be removed.

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

## 2a. Migration Plan (from single-node → multi-validator)

1. **Retire legacy single-node tests**  
   Delete `full_node_blob_roundtrip` and any single-validator helpers. Until the multi-validator
   harness lands, gate `make itest-node` (or mark the binary `#[ignore]`) so developers are not
   misled by invalid coverage.

2. **Promote the existing dual-node scaffolding to 3 validators**  
   Reuse `NetworkHarness` but always instantiate with 3 validators (and optional followers). Rename
   the surviving test to reflect blob quorum coverage and assert every validator persisted blobs.

3. **Port Tier 0 scenarios into the new harness**  
   Once the baseline is stable, iteratively add restart, sync, and negative-path cases on top of
   the multi-validator harness (mirroring Malachite’s `full_nodes.rs` suite) before progressing to
   P3/P4 in the work plan.

4. **Wire Execution Bridge + ValueSync**  
   After the harness runs deterministically, switch it to the Engine RPC stub + real ExecutionClient
   and enable ValueSync just like the upstream tests so blob sidecars, WAL, and sync packages are
   exercised together.

---

## 3. Phased Work Plan

| Phase | Description | Deliverables & Acceptance Criteria | Owner | Est. Effort | Depends On | Status |
|-------|-------------|------------------------------------|-------|------------|------------|--------|
| P0 | Finalize parity scope & infra decisions | This doc, shared understanding of Tier 0 (`blob_state/`) vs Tier 1 (`full_node/`), and action list. | @team | 0.5 d | — | ✅ |
| P1 | Multi-validator harness baseline | `crates/test/tests/full_node/` boots **three** validators (2f+1) plus optional full nodes, mirroring Malachite’s TestBuilder. `make itest-node` must exercise proposer/follower votes, `/proposal_parts` gossip, and WAL checkpoints end-to-end using the Engine RPC stub. | @team | 3 d | P0 | 🟡 |
| P2 | Crash/sync flows on harness | Extend the multi-validator harness with follower nodes, restarts, and ValueSync enabled. At least one validator/full-node crash-and-recover path plus a late joiner must pass deterministically. | @team | 2 d | P1 | ⏳ |
| P3 | Sync & restart cases | Port `blob_sync_across_restart_multiple_heights` and `blob_restart_hydrates_multiple_heights` into Tier 1. **Done when**: restart path exercises WAL/timers (not just store reopen) and passes deterministically. | TBA | 2 d | P2 | ⏳ |
| P4 | Negative-path parity | Tier 1 versions of commitment mismatch, invalid proof, EL rejection. **Done when**: node logs/metrics show rejection, WAL cleanup verified. (Can run parallel w/ P3 once P2 is stable.) | TBA | 1–2 d | P2 | ⏳ |
| P5 (optional) | Execution bridge wiring | Replace `MockEngineApi` with HTTP Engine stub so the real `ExecutionClient` path runs unmodified; later optional upgrade to real reth devnet. **Done when**: `generate_block_with_blobs`, `notify_new_block`, `forkchoice_updated` go over HTTP stub. | TBA | 2–3 d | P2 | ⏳ |
| P6 (optional) | CI integration & docs | Update `DEV_WORKFLOW.md`, add `make itest-node`, decide CI cadence (manual/nightly/per-PR). **Done when**: documented instructions + optional CI job exist. | TBA | 1 d | P2–P5 | ⏳ |

---

## 4. Near-Term Action Items

| Item | Description | Owner | Priority | Status |
|------|-------------|-------|----------|--------|
| Tier 0 reorg | Move existing state tests into `crates/test/tests/blob_state/`, update Makefile/docs references. | @team | High | ✅ |
| Harness Skeleton (P1) | Build the **multi-validator** harness using real libp2p transport + WAL, modeled after Malachite’s TestBuilder (≥3 validators + optional full nodes). Current single-node helper must be replaced. | @team | High | 🟡 |
| Scenario Porting | Tier 1 must cover proposer/follower, crash/restart, and sync scenarios on the multi-node harness (no single-node shortcuts). | @team | High | ⏳ |
| Execution Bridge Stub | HTTP Engine stub that exercises the real `ExecutionClient` (replaces [`MockEngineApi`](../crates/test/tests/common/mocks.rs)). Landed via `EngineRpcStub` in the new harness. | @team | Medium | ✅ |
| Docs & CI | Keep docs in sync (`DEV_WORKFLOW.md`, this plan) and decide when `make itest-node` runs in CI. Docs landed; CI cadence pending runtime metrics from P2. | @team | Medium | 🟡 |

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
| libp2p flakiness / port conflicts | Medium | High | Deterministic port allocator, retries, serialize Tier 1 tests initially. (*Serialization still pending.) |
| Tier 1 suite too slow for per-PR | High | Medium | Keep Tier 0 for fast checks; run Tier 1 nightly until optimized. (Currently gated manually; `#[ignore]`/`serial` still TODO.) |
| Divergence between tiers | Low | Medium | Document scenario mapping; periodically run Tier 1 locally before releases. |
| Execution bridge complexity | Medium | High | Use HTTP stub first; defer real reth integration until harness stable. |
| Maintenance overhead | Medium | Medium | Share helpers between tiers, document setup, automate teardown. |

---

## 7. Notes

- **Tier strategy**: Tier 0 = state-level tests (`blob_state/`), Tier 1 = full-node tests (`full_node/`) with **three** validators + followers to match Tendermint majority rules. Tier 0 stays default for `make itest`, Tier 1 becomes `make itest-node`.
- **Architecture references**: Malachite’s `TestBuilder` (networked validators + followers; see `malachite/code/crates/test/tests/it/full_nodes.rs`) and Snapchain’s consensus harness (`snapchain/tests/consensus_test.rs`) are the baselines we mirror.
- **Next steps**: Replace the single-node helper with the multi-validator harness, then port restart/sync/negative paths (P2→P4) before wiring the execution bridge (P5).

## 8. Current Status (Nov 2025)

- Tier 1 currently ships two scenarios: `full_node_blob_quorum_roundtrip` (quorum end-to-end) and `full_node_validator_restart_recovers` (post-height-1 restart with blob hydration checks). After the Nov 2025 harness fixes (start height alignment, tracing, deterministic parent hashes) both pass deterministically (~18 s).
- **Known limitation for P2:** restart coverage still stops at the “restart after height 1 decided” case. Extending to height 2+ restarts will require richer ValueSync/WAL diagnostics; keep this open for Phase P2 once instrumentation (store/WAL dumps, serial gating) lands.

---

## 9. 2025‑11 Review Findings

**Reference baselines**
- Snapchain’s consensus suite (`snapchain/tests/consensus_test.rs`) uses a deterministic builder with `serial_test`, explicit port allocation, and asserts on block store / shard store state rather than raw log output.
- Malachite’s `TestBuilder` scenarios (`malachite/code/crates/starknet/test/src/tests/full_nodes.rs`) always run ≥ 3 validators plus followers, drive crash/restart/value-sync cases, and rely on the engine itself to report progress (`wait_until(height)` and `run_with_params(...)` manage deadlines/diagnostics).

**Tier 0 (state-level) gaps**
- No serialization: every `blob_state` test can run concurrently even though they share the same global trusted setup cache and each spins RocksDB in `/tmp`. This differs from Snapchain/Malachite where heavy cases are `#[serial]` or moved to a `make itest` target. Consequence: nondeterministic timing, high CPU, and noisy failures when multiple suites run together.
- Coverage stops at `State::process_decided_certificate`: none of the Tier 0 scenarios force Malachite to emit `Decided`, so bugs between votes and commit (e.g., missing POLC, WAL replay divergence) slip through. Reference suites enforce consensus-level assertions before calling into application state.
- Helpers trigger permanent compiler warnings (`tests/common/mod.rs` exports unused mocks, harness structs). Keeping unused scaffolding diverges from the cleaner shared modules in Snapchain/Malachite and signals duplication across tests.

**Tier 1 (full-node) gaps**
- Tests are neither ignored nor serialized (`node_harness.rs:87-120`). Running `cargo test -p ultramarine-test` attempts to boot libp2p + Engine stubs alongside the fast Tier 0 suite, causing port conflicts that reference harnesses avoid by gating (snapchain) or using `serial_test` (malachite).
- The harness mutates node state out-of-band (`initialize_genesis_metadata()` opens RocksDB directly before the app starts). Production nodes already seed via `State::hydrate_blob_parent_root()`; touching the DB externally bypasses WAL replay and can leave consensus believing it still owes height 0, which explains the missing `Decided` events we observed.
- Instrumentation is minimal: when `wait_for_height` times out we only see the last 10 broadcast events. Reference harnesses inspect block stores, WAL entries, and consensus metrics to emit actionable reasons (“POLC not reached”, “value sync pending”). We should expose similar checks (e.g., query `Store::get_decided_value` per node, dump WAL).
- Engine stub closes the TCP connection after each request (`node_harness.rs:462-520`), whereas real Engine API connections stay open. Hyper retries hide the error, but repeated connection resets slow tests and obscure root causes; Snapchain/Malachite stubs keep connections alive and handle multiple RPCs per session.
- Every node is forced to `start_height: Some(Height::new(1))` with empty stores. Malachite’s builder either populates genesis in advance or lets the node derive start height from disk. Our shortcut means consensus believes it skipped height 0 even though the WAL is empty, leading to the stalled round‑1 behaviour currently seen.

**Recommended remediation**
1. Gate Tier 1 behind `#[ignore]` + `make itest-node`, add `serial_test::serial` so only one harness manipulates ports/state at a time.
2. Remove the manual RocksDB seeding from the harness and let `App::start` seed genesis metadata. If we must pre-populate, spawn the app once to height 0 rather than editing the DB externally.
3. Upgrade instrumentation: ✅ Store snapshot + WAL tail now dump automatically when a node times out. Future work: expose WAL decoding in a helper so we can assert on specific entry types.
4. Extend Tier 0 to cover proposer restreams and WAL replay by driving multiple rounds within the same test (serialised). This plugs the “Decided never called” gap without waiting for Tier 1 to finish.
5. Adopt a builder DSL (either port Malachite’s `TestBuilder` or create a thin wrapper) so future scenarios describe validator/full-node lifecycles declaratively, matching our reference implementations and reducing bespoke harness code.
