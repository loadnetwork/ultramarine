# Integration Test Parity Plan

_Last updated: 2025-11-XX_

Ultramarine now ships two tiers of integration coverage:

- **Tier 0 (component smokes)** — 3 fast tests in `crates/consensus/tests` (`blob_roundtrip`,
  `blob_sync_commitment_mismatch`, `blob_pruning`) that exercise `State<TestBlobEngine>` with real
  RocksDB stores and KZG verification. These run in the main `make test` sweep and are always in CI.
- **Tier 1 (full-node harness)** — 14 end-to-end scenarios in `crates/test/tests/full_node` that boot
  Malachite channel actors, libp2p, WAL, and the application loop. These run via `make itest-node`
  and are wired into CI as a dedicated job (`itest-tier1`) after unit tests pass; `CARGO_NET_OFFLINE`
  is overridable for cold runners.

Both upstream projects we claim parity with already run multi-validator, networked harnesses
exclusively:

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
| Consensus `State` + BlobEngine      | ✅ (Tier 0)      | 3 smokes with real RocksDB + KZG in `crates/consensus/tests`. |
| Execution payload + blob verifier   | ✅ (Tier 0/1)    | Deterministic payloads, real KZG commitments/proofs via `c-kzg`. |
| Engine API bridge (generate block)  | ⚠️ Stubbed       | Tier 0 mocks; Tier 1 uses Engine RPC stub (HTTP ExecutionClient wiring still pending). |
| Execution notifier (FCU / payload)  | ⚠️ Stubbed       | Tier 0 uses `MockExecutionNotifier`; Tier 1 uses stubbed Execution client. |
| Malachite channel actors            | ✅ (Tier 1)      | Full-node harness boots channel actors/WAL/libp2p. |
| libp2p gossip transport             | ✅ (Tier 1)      | `/proposal_parts` streaming exercised end-to-end. |
| WAL / timers / crash recovery paths | ✅ (Tier 1)      | Restart/ValueSync paths deterministic via `StartedHeight` gating + `wait_for_nodes_at`. |
| CI signal                           | ✅               | Tier 0 in `make test`; Tier 1 runs in `itest-tier1` job after `test`, with failure artifacts. |

---

## 2. Target Parity Goals

1. **Channel-Service Harness** ✅  
   Tier 1 boots Ultramarine nodes (consensus channel actors + WAL + libp2p) and drives `/proposal_parts` end-to-end.

2. **Multi-Validator Scenario** ✅  
   Harness uses 3–4 validators (2f+1 quorum; 4 for ValueSync-only) with real networking/timers. Single-node paths removed.

3. **Execution Bridge Coverage** ⏳  
   Still using Engine RPC stub; HTTP ExecutionClient wiring remains a follow-up.

4. **Negative Paths Under Full Node** ✅  
   Commitment mismatch, inclusion proof failure, and EL rejection covered in Tier 1 scenarios.

5. **CI Integration** ✅  
   Tier 0 in `make test`; Tier 1 runs in `itest-tier1` job (20m timeout, artifacts on failure).

6. **(Optional) Ethereum Spec Compliance**  
   Future: blobless payload fallback, `engine_getBlobsV1`, sidecar gossip APIs.

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
| P1 | Multi-validator harness baseline | `crates/test/tests/full_node/` boots **four** validators where needed (ValueSync-only paths) and three for quorum flows, mirroring Malachite’s TestBuilder with 2f+1 voting. `make itest-node` exercises proposer/follower votes, `/proposal_parts` gossip, and WAL checkpoints end-to-end using the Engine RPC stub. | @team | 3 d | P0 | ✅ |
| P2 | Crash/sync flows on harness | Extend the multi-validator harness with follower nodes, restarts, and ValueSync enabled. At least one validator/full-node crash-and-recover path plus a late joiner must pass deterministically. *(Engine stub persistence + resume-height logging landed; restart scenario now stable with 100% pass rate using event-based waiting and immediate node-0 shutdown to force ValueSync.)* | @team | 2 d | P1 | ✅ |
| P3 | Sync & restart cases | Port `blob_sync_across_restart_multiple_heights` and `blob_restart_hydrates_multiple_heights` into Tier 1. **Done when**: restart path exercises WAL/timers (not just store reopen) and passes deterministically. | TBA | 2 d | P2 | ✅ (restart/ValueSync paths deterministic with `StartedHeight` gating) |
| P4 | Negative-path parity | Tier 1 versions of commitment mismatch, invalid proof, EL rejection. **Done when**: node logs/metrics show rejection, WAL cleanup verified. (Can run parallel w/ P3 once P2 is stable.) | TBA | 1–2 d | P2 | ✅ (commitment mismatch, inclusion-proof failure, EL rejection present) |
| P5 (optional) | Execution bridge wiring | Replace Engine stub with HTTP ExecutionClient so `generate_block_with_blobs`, `notify_new_block`, `forkchoice_updated` go over HTTP. | TBA | 2–3 d | P2 | ⏳ |
| P6 (optional) | CI integration & docs | Update `DEV_WORKFLOW.md`, add `make itest-node`, decide CI cadence (manual/nightly/per-PR). **Done when**: documented instructions + optional CI job exist. | TBA | 1 d | P2–P5 | 🟠 (docs updated; CI cadence pending) |

---

## 4. Near-Term Action Items

| Item | Description | Owner | Priority | Status |
|------|-------------|-------|----------|--------|
| Tier 0 reorg | Move existing state tests into `crates/test/tests/blob_state/`, update Makefile/docs references. | @team | High | ✅ |
| Harness Skeleton (P1) | Build the **multi-validator** harness using real libp2p transport + WAL, modeled after Malachite’s TestBuilder (≥3 validators + optional full nodes). Current single-node helper must be replaced. | @team | High | ✅ |
| Scenario Porting | Tier 1 must cover proposer/follower, crash/restart, and sync scenarios on the multi-node harness (no single-node shortcuts). | @team | High | ✅ (Tier 1 covers quorum, restart/ValueSync, negative paths) |
| Execution Bridge Stub | HTTP Engine stub that exercises the real `ExecutionClient` (replaces [`MockEngineApi`](../crates/test/tests/common/mocks.rs)). Landed via `EngineRpcStub` in the new harness. | @team | Medium | ⏳ (pending P5 HTTP ExecutionClient wiring) |
| Docs & CI | Keep docs in sync (`DEV_WORKFLOW.md`, this plan) and decide when `make itest-node` runs in CI. Docs landed; CI cadence pending runtime metrics. | @team | Medium | 🟠 (docs updated; CI cadence pending) |

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

- **Tier strategy**: Tier 0 = state-level tests (`blob_state/`), Tier 1 = full-node tests (`full_node/`) with multi-validator topologies (three validators for quorum flow; four validators for ValueSync-only recovery such as `full_node_validator_restart_recovers`). Tier 0 stays default for `make itest`, Tier 1 becomes `make itest-node`.
- **Harness builder**: Tier 1 now exposes `FullNodeTestBuilder` in `crates/test/tests/full_node/node_harness.rs`, so scenarios describe only the consensus actions while setup/teardown stays centralized.
- **Payload planning**: The builder can inject a per-height blob schedule into the Engine stub, enabling blobless heights and multi-blob rounds (needed for the multi-height ValueSync restart scenario and future negative-path work).
- **State inspection helpers**: The harness can now reopen a validator’s stores after a clean shutdown, rebuild blob sidecars, and run `State::process_synced_package` directly. This powers the restart hydration, restream cleanup, and commitment-mismatch tests without hand-editing RocksDB.
- **Execution cadence**: `make itest-node` runs each Tier 1 test in its own `cargo test` process (`blob_quorum`, `validator_restart` (4-node ValueSync recovery), `restart_mid_height`, `new_node_sync`, `multi_height_valuesync_restart`, `restart_multi_height_rebuilds`, `restream_multiple_rounds_cleanup`, `restream_multi_validator`, `value_sync_commitment_mismatch`, `value_sync_inclusion_proof_failure`, `blob_blobless_sequence_behaves`, `blob_pruning_retains_recent_heights`, `sync_package_roundtrip`, `value_sync_proof_failure`) to avoid cross-test resource leaks. Running `cargo test -p ultramarine-test --test full_node -- --ignored` is fine for ad-hoc runs but may time out when chaining all scenarios inside one process.
- **Tier‑0 → Tier‑1 migration**: We now have a broader set of promotions:  
  1. `blob_new_node_sync` → `full_node_new_node_sync` (4-validator cluster, validator 3 rejoins after heights 1–2 and ValueSync fetches blobs/metadata).  
  2. `blob_restart_multi_height_sync` → `full_node_multi_height_valuesync_restart` (validator 3 misses heights 1–3 with a 1/0/2 blob mix, ValueSync imports them, and we restart to assert store + parent-root hydration).  
  3. `blob_restart_multi_height` → `full_node_restart_multi_height_rebuilds` (multi-height blob mix without ValueSync; restarts rely purely on on-disk metadata + blob store).  
  4. `blob_restream_multiple_rounds` → `full_node_restream_multiple_rounds_cleanup` (uses real stores/keys to drive two rounds and ensure losing-round blobs are dropped at commit).  
  5. `blob_sync_commitment_mismatch_rejected` → `full_node_value_sync_commitment_mismatch` (feeds a tampered ValueSync package through a full-node state to verify rejection + cleanup).  
  6. `blob_restream_multi_validator` → `full_node_restream_multi_validator` (proposer/follower restream over real channels to confirm metrics/import paths).  
  7. `blob_sync_inclusion_proof_failure_rejected` → `full_node_value_sync_inclusion_proof_failure` (corrupt inclusion proofs inside ValueSync packages and assert full-node rejection/cleanup).  
  8. `blob_blobless_sequence_behaves` → `full_node_blob_blobless_sequence_behaves` (exercise mixed blob/bless heights with real metrics + storage).  
  9. `blob_pruning_retains_recent_heights` → `full_node_blob_pruning_retains_recent_heights` (override retention window and ensure pruning/metrics behave with real stores).  
  10. `sync_package_roundtrip` → `full_node_sync_package_roundtrip` (ingest a full ValueSync package and commit it end-to-end).  
  11. `blob_sync_failure_rejects_invalid_proof` → `full_node_value_sync_proof_failure` (tamper blob proofs to cover the last ValueSync rejection path).  
  These scenarios prove the builder can handle restart, restream, and sync-negative paths without hand-editing RocksDB.
- **Architecture references**: Malachite’s `TestBuilder` (networked validators + followers; see `malachite/code/crates/test/tests/it/full_nodes.rs`) and Snapchain’s consensus harness (`snapchain/tests/consensus_test.rs`) are the baselines we mirror.
- **Next steps**: With the Tier‑0 backlog promoted, the remaining work is wiring the builder to the execution bridge (real EngineClient / payload status plumbing) and deciding how to run those heavier cases in CI (Phase P5).

## 8. Current Status (Nov 2025)

- Tier 1 now ships fourteen scenarios: `full_node_blob_quorum_roundtrip`, `full_node_validator_restart_recovers`, `full_node_restart_mid_height`, `full_node_new_node_sync`, `full_node_multi_height_valuesync_restart`, `full_node_restart_multi_height_rebuilds`, `full_node_restream_multiple_rounds_cleanup`, `full_node_restream_multi_validator`, `full_node_value_sync_commitment_mismatch`, `full_node_value_sync_inclusion_proof_failure`, `full_node_blob_blobless_sequence_behaves`, `full_node_blob_pruning_retains_recent_heights`, `full_node_sync_package_roundtrip`, and `full_node_value_sync_proof_failure`. Together they cover quorum flow, restart hydration (with and without ValueSync), restream cleanup (multi-round + multi-validator), blobless/pruning behavior, and every ValueSync rejection path using the production state/engine wiring. Determinism updates (2025-11-18): `StartedHeight` gating for mid-height crash; shared `wait_for_nodes_at` helper; full suite passes via `cargo test -p ultramarine-test --test full_node -- --ignored --nocapture`.
- **Known limitation for P2:** the remaining negative-path coverage (e.g., inclusion-proof failures) still lives only in Tier 0. With the builder now resuming height from disk and keeping the Engine stub head persistent across restarts, porting those cases plus additional restream permutations is unblocked—The restart regression (`full_node_validator_restart_recovers`) is now stable and deterministic (4-validator cluster with immediate node-0 shutdown forcing ValueSync), completing P2 requirements.

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
