# Integration Test Parity Plan

_Last updated: 2025-11-19_

Ultramarine ships two tiers of integration coverage:

- **Tier 0 (component smokes)** — 3 fast tests in `crates/consensus/tests` (`blob_roundtrip`, `blob_sync_commitment_mismatch`, `blob_pruning`) that exercise `State<TestBlobEngine>` with real RocksDB stores and KZG verification. They run in `make test` and are always in CI.
- **Tier 1 (full-node harness)** — 14 end-to-end scenarios in `crates/test/tests/full_node` that boot Malachite channel actors, libp2p, WAL, and the production application loop. They run via `make itest-node` and the CI job `itest-tier1` (after unit tests), with `CARGO_NET_OFFLINE` overridable and artifacts on failure.

The parity target remains aligned with Malachite and Snapchain: multi-validator, networked harnesses that exercise leader election, ValueSync, crash/restart, WAL, and gossip end-to-end.

---

## 1. Current Coverage Snapshot

| Layer / Component                   | Exercised Today? | Notes                                                                                   |
| ----------------------------------- | ---------------- | --------------------------------------------------------------------------------------- |
| Consensus `State` + BlobEngine      | ✅ (Tier 0)      | 3 smokes with real RocksDB + KZG in `crates/consensus/tests`.                           |
| Execution payload + blob verifier   | ✅ (Tier 0/1)    | Deterministic payloads, real KZG commitments/proofs via `c-kzg`.                        |
| Engine API bridge (generate block)  | ⚠️ Stubbed        | Tier 0 mocks; Tier 1 uses Engine RPC stub (HTTP ExecutionClient wiring still pending).  |
| Execution notifier (FCU / payload)  | ⚠️ Stubbed        | Tier 0 uses `MockExecutionNotifier`; Tier 1 uses stubbed Execution client.              |
| Malachite channel actors            | ✅ (Tier 1)      | Full-node harness boots channel actors/WAL/libp2p.                                      |
| libp2p gossip transport             | ✅ (Tier 1)      | `/proposal_parts` streaming exercised end-to-end.                                       |
| WAL / timers / crash recovery paths | ✅ (Tier 1)      | Restart/ValueSync paths deterministic via `StartedHeight` gating + `wait_for_nodes_at`. |
| CI signal                           | ✅               | Tier 0 in `make test`; Tier 1 in `itest-tier1` (20m timeout, artifacts on failure).     |

---

## 2. Remaining Gaps

- **Execution bridge**: Tier 1 still uses the Engine RPC stub; wiring the HTTP `ExecutionClient` for payload generation/FCU/newPayload is the primary open item.
- **Spec extras (optional)**: Blobless payload fallback, `engine_getBlobsV1`, and sidecar gossip APIs remain future work.

---

## 3. Risk & Mitigation

| Risk                              | Likelihood | Impact | Mitigation                                                                                            |
| --------------------------------- | ---------- | ------ | ----------------------------------------------------------------------------------------------------- |
| libp2p flakiness / port conflicts | Medium     | High   | Deterministic ports + per-scenario process isolation; artifacts uploaded on CI failure.               |
| Tier 1 runtime on PRs             | Medium     | Medium | Keep Tier 0 in `make test`; Tier 1 runs as a separate CI job (`itest-tier1`, 20m timeout, artifacts). |
| Execution bridge parity           | Medium     | Medium | Follow-up to replace the stub with HTTP `ExecutionClient`.                                            |

---

## 4. Notes

- **Tier strategy**: Tier 0 = consensus crate smokes (3 tests, default in `make test`/CI); Tier 1 = full-node harness (14 ignored tests) run via `make itest-node` and CI job `itest-tier1`.
- **Harness builder**: `FullNodeTestBuilder` in `crates/test/tests/full_node/node_harness.rs` centralizes setup/teardown, payload plans, and deterministic ports.
- **Execution cadence**: Tier 1 scenarios run as separate `cargo test ... -- --ignored` invocations to avoid resource leaks; `cargo test -p ultramarine-test --test full_node -- --ignored` remains available for local ad-hoc runs.
- **CI**: Tier 1 executes after the main test job and uploads `target/debug/deps/full_node*` and `crates/test/test_output.log` on failure; `CARGO_NET_OFFLINE` is overridable for cold runners.
