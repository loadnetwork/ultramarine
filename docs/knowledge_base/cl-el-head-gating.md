# CL↔EL Head Alignment (FCU Gate)

## Purpose

This note documents the **consensus‑critical** rules for aligning CL decided head with EL head,
how Ultramarine enforces the gate, and where to look in code/tests/ops.

## Glossary

- **CL**: Consensus Layer (Ultramarine / Malachite host).
- **EL**: Execution Layer (load‑reth).
- **FCU**: `engine_forkchoiceUpdated` call to the Engine API.
- **SYNCING**: Engine API status meaning the EL cannot validate the head yet (missing data).
- **Gate**: The rule set that blocks proposing/voting until EL is aligned to the CL decided head.

## Problem (Summary)

This gate exists to prevent CL/EL head divergence after restarts and to keep
consensus participation safe when the execution layer is lagging.

## Correctness Rules (Specs)

### Engine API

The **only** readiness oracle is the Engine API forkchoice status:

- `engine_forkchoiceUpdated` returns `VALID | INVALID | SYNCING`
- `SYNCING` means the head is unknown/unvalidated; CL must not build/vote.

Refs:

- `execution-apis/src/engine/common.md` (ordering + retry)
- `execution-apis/src/engine/paris.md` (FCU semantics)
- `execution-apis/src/engine/shanghai.md`
- `execution-apis/src/engine/cancun.md`

### Malachite (Tendermint)

Invalid proposal → **PREVOTE nil** is the expected path.
Async `getValue()` can fail; round proceeds safely.

Refs:

- `malachite/specs/consensus/overview.md`
- `malachite/specs/consensus/design.md`
- `malachite/specs/synchronization/valuesync/README.md`

## Required Gate Behavior

1. **Source of truth = CL decided head (store), not EL HTTP.**
2. **Before proposing or voting**, call FCU with `payloadAttributes=None`.
   - `VALID` → proceed
   - `SYNCING/INVALID` → refuse proposal/vote (nil)
3. **Do not use `eth_getBlockByNumber(latest)` for consensus gating.**

## Failure Modes & Expected Behavior

| Condition                                                   | Expected Behavior                                                           |
| ----------------------------------------------------------- | --------------------------------------------------------------------------- |
| FCU returns **SYNCING**                                     | Do not propose/vote; allow round timeout → nil vote; retry FCU later.       |
| FCU returns **INVALID**                                     | Do not propose/vote; mark EL degraded and retry later.                      |
| FCU returns **ACCEPTED**                                    | Treat as Engine API spec violation; fail the gate and refuse proposal/vote. |
| FCU returns **VALID** but `latestValidHash` mismatches head | Treat as spec violation; fail the gate and refuse proposal/vote.            |
| EL not reachable (FCU call fails)                           | Fail the gate; do not propose/vote; retry until EL is reachable.            |
| No decided head on disk (fresh genesis)                     | Seed `latest_block` from execution `genesis.json` (same as EL).             |
| Missing execution genesis path                              | Fail fast at startup; do not enter consensus.                               |

## Implementation (Ultramarine)

### App gating

- Startup alignment: `crates/node/src/app.rs` (ConsensusReady → FCU gate)
- Proposer gate: `crates/node/src/app.rs` (`GetValue`)
- Validator gate: `crates/node/src/app.rs` (`ReceivedProposalPart`)
- Execution genesis bootstrap: `crates/node/src/node.rs`

### Engine API client

- FCU status handling: `crates/execution/src/client.rs`

### Decided path

- FCU‑before‑newPayload during sync: `crates/consensus/src/state.rs`
- MetadataOnly sync advances `latest_block`: `crates/consensus/src/state.rs`

### Design decisions

- `CLAUDE.md` → **Engine API Design Decisions**

## Execution Genesis Bootstrap (No HTTP)

Ultramarine no longer uses `eth_getBlockByNumber(latest)` to seed `latest_block`.
Instead, it computes the execution genesis header locally from the same `genesis.json`
used by load-reth, then uses that as the initial `latest_block` when no decided head
exists in the CL store.

**Inputs**

- CLI: `--execution-genesis-path=/path/to/genesis.json`
- Env: `ULTRAMARINE_EL_GENESIS_JSON=/path/to/genesis.json`

**Derived fields**

- `state_root` computed from `alloc` (MPT root).
- Fork-gated fields set if active at genesis:
  - London: `base_fee_per_gas`
  - Shanghai: `withdrawals_root`
  - Cancun: `parent_beacon_block_root`, `blob_gas_used`, `excess_blob_gas`
  - Prague: `requests_hash`

**Why**

- Eliminates HTTP RPC dependency for consensus-critical initialization.
- Ensures CL/EL agree on genesis header without importing `reth-chainspec`.

## Tests (Tier‑1, make itest-node)

- `full_node_el_syncing_degrades_node`
- `full_node_el_syncing_still_sends_fcu`
- `full_node_el_syncing_blocks_payload_build`
- `full_node_el_transient_syncing_recovers`
- `full_node_fcu_gate_does_not_require_http_latest`
- `full_node_fcu_accepted_rejected`
- `full_node_split_head_recovery`

Location: `crates/test/tests/full_node/node_harness.rs`\
Runner: `Makefile` → `itest-node`

## Harness Invariant (Engine Stub Hashes)

The Tier‑1 harness uses synthetic block hashes (`[height as u8; 32]`) to map
heights inside the Engine API stub. The stub must **not** interpret arbitrary
hashes as heights; otherwise it can jump its internal head and produce
`BuiltPayloadMismatch` errors during `getPayload`.

See: `docs/knowledge_base/itest-node-harness.md`

## Operations (Ansible)

CL restart must wait for EL readiness (`eth_syncing == false`) **after IPC socket appears**:

- `infra/ansible/playbooks/roll.yml`
- `infra/ansible/playbooks/deploy.yml`
- `infra/ansible/playbooks/blockscout.yml`

## Runbook: Detecting & Resolving Split‑Head

### Symptoms

- Repeated `Parent hash mismatch` in CL logs at the same height.
- Two validator groups each reject the other’s proposals.
- RPC/Blockscout shows a head that disagrees with some validators.

### Quick Checks

1. **Compare CL decided head vs EL head**
   - CL: check latest decided height/hash in `store.db` (or logs).
   - EL: `engine_forkchoiceUpdated` (FCU) status for that head; do **not** trust `eth_getBlockByNumber(latest)` for gating.
2. **Confirm EL readiness**
   - `eth_syncing == false` (per Ansible readiness checks).

### Recovery Steps

1. **Realign EL to CL decided head**
   - Apply FCU to the CL decided head (no payload attributes).
   - If `SYNCING`, wait and retry; do not propose/vote in the meantime.
2. **Restart order (if needed)**
   - Restart EL first, wait for IPC socket + `eth_syncing == false`, then restart CL.
3. **Verify recovery**
   - Parent mismatch errors stop.
   - Validators converge on the same head and heights advance.

### Note on node‑rpc / public views

RPC nodes may align with a single validator group (as seen in BUG‑013).\
If RPC head diverges from the validator majority, public explorers can show a forked view.
Treat RPC alignment as **observability**, not consensus truth.

## Related Docs

- CL runtime settings: `docs/knowledge_base/cl-runtime.md`
- Execution genesis bootstrap: `docs/knowledge_base/execution-genesis.md`
- EL persistence for 1-slot finality: `docs/knowledge_base/el-persistence.md`

## Further Reading

- Incident report (example scenario): `docs/journal/BUG-013-chain-split-parent-mismatch.md`
