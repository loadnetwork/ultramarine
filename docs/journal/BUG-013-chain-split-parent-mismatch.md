# BUG-013: Chain Split on Parent Hash Mismatch (Height 5006)

## Status: FIXED (validated 2026-01-26)

## Summary

On the fibernet testnet, consensus stalled at height 5006 with repeated `Parent hash mismatch` errors.
The network split into two views of the latest parent:

- Group A (AMS nodes) treated **5005 = 0x2177…** as the latest head.
- Group B (LON2 nodes) treated **5004 = 0x7268…** as the latest head.

As a result, proposals were built on different parents and validators rejected each other’s proposals indefinitely.

## Impact

- Consensus stuck at height 5006 with repeated round timeouts.
- Validators entered a persistent split‑head condition.
- Public RPC visibility followed one side of the split (node‑rpc aligned with AMS), which could mislead external observers.

## Environment

- Network: fibernet
- Date: 2026‑01‑22
- Height: 5006
- Nodes affected: f4‑metal‑medium‑lon2‑fibernet‑1 (node‑0/1), f4‑metal‑medium‑ams‑fibernet‑2 (node‑2/3)
- Also observed: node‑rpc aligned with AMS head (5005 = 0x2177…)

## Timeline (Key Facts)

All timestamps are UTC from journald.

**1) EL/CL restarts around the split window**

- LON2 EL restarted:
  - `2026‑01‑22 14:28:08` Stopped `load-reth@node-0`
  - `2026‑01‑22 14:28:10` Started `load-reth@node-0`
  - `2026‑01‑22 14:28:12` Stopped `load-reth@node-1`
  - `2026‑01‑22 14:28:14` Started `load-reth@node-1`
- AMS EL restarted:
  - `2026‑01‑22 14:27:09` Stopped `load-reth@node-2`
  - `2026‑01‑22 14:27:11` Started `load-reth@node-2`
  - `2026‑01‑22 14:27:13` Stopped `load-reth@node-3`
  - `2026‑01‑22 14:27:15` Started `load-reth@node-3`
- AMS CL restarted:
  - `2026‑01‑22 14:27:21` Started `ultramarine@node-2`
  - `2026‑01‑22 14:27:25` Started `ultramarine@node-3`
- LON2 CL restarted:
  - `2026‑01‑22 14:28:26` `🚀 App message loop starting` (node‑1)

**2) AMS nodes decided height 5005 as 0x2177…**

- `2026‑01‑22 14:27:06` (AMS)
  - `✅ Decided certificate processed successfully height=5005 ... block_hash=0x2177b29a...`

**3) AMS built 5006 on parent 0x2177…**

- `2026‑01‑22 14:27:56` (AMS)
  - `🟠 generate_block_with_blobs on top of ExecutionBlock { block_hash: 0x2177..., block_number: 5005 }`
  - `Received execution payload ... block_number=5006 parent_hash=0x2177...`

**4) LON2 built on stale parent 0x7268… (block 5004)**

- `2026‑01‑22 14:28:40` (LON2)
  - `🟠 generate_block_with_blobs on top of ExecutionBlock { block_hash: 0x7268..., block_number: 5004 }`

**5) Parent mismatch errors begin (split visible)**
LON2 rejects proposals built on 0x2177…:

- `2026‑01‑22 14:30:55` (LON2)
  - `Parent hash mismatch at height 5006 ... proposed_parent=0x2177... latest_parent=0x7268...`

AMS rejects proposals built on 0x7268…:

- `2026‑01‑22 14:52:01` (AMS)
  - `Parent hash mismatch at height 5006 ... proposed_parent=0x7268... latest_parent=0x2177...`

**6) RPC node aligned with AMS head**

- `2026‑01‑22 14:48:xx` (node‑rpc)
  - `eth_getBlockByNumber(latest)` shows `number=0x138d` `hash=0x2177...` `parent=0x7268...`

## Critical Log Excerpts (Raw)

LON2 (node‑0/1):

```
2026-01-22T14:28:40.408141Z DEBUG 🟠 generate_block_with_blobs on top of ExecutionBlock { block_hash: 0x7268a47b..., block_number: 5004 }
2026-01-22T14:30:55.068442Z ERROR Parent hash mismatch at height 5006 ... proposed_parent=0x2177b29a... latest_parent=0x7268a47b...
```

AMS (node‑2/3):

```
2026-01-22T14:27:06.023793Z DEBUG Finalizing decided certificate height=5005 block_hash=0x2177b29a...
2026-01-22T14:27:56.669896Z DEBUG 🟠 generate_block_with_blobs on top of ExecutionBlock { block_hash: 0x2177b29a..., block_number: 5005 }
2026-01-22T14:52:01.612345Z ERROR Parent hash mismatch at height 5006 ... proposed_parent=0x7268a47b... latest_parent=0x2177b29a...
```

## Root Cause

**Missing safety gate between CL decided head and EL head.**
On restart, `latest_block` was initialized from EL (`eth_getBlockByNumber(Latest)`), which can be stale after EL restarts.
If CL’s decided head is ahead of EL’s canonical head, the validator still participates in consensus and validates parent hashes
against a stale `latest_block`, creating a persistent split when other validators build on the true decided parent.

## Contributing Factors

1. **EL lag not treated as a consensus blocker**
   Nodes continued proposing/prevoting even when EL was SYNCING or behind the CL decided head.
2. **No single source of truth for parent at startup**
   CL store (decided) and EL head were treated as separate truths; on restart, EL dominated.
3. **Large‑payload latency window** (general risk)
   Under high load, payload build latency can allow heads to shift mid‑build, increasing mismatch risk.
4. **EL persistence defaults (tip-2)** (not root cause, but amplifies restart desync)
   Reth defaults keep recent canonical blocks in memory; single-block finality makes this unsafe on restart.

## Fixes Applied (2026-01-26)

1. **Removed EL HTTP bootstrap for `latest_block`**
   - `latest_block` now seeds from execution `genesis.json` (same file as load-reth `--chain`).
   - CL store remains the primary source of truth on restart.
2. **Explicit execution genesis wiring**
   - New CLI/env: `--execution-genesis-path` / `ULTRAMARINE_EL_GENESIS_JSON`.
   - Startup fails fast if missing to avoid silent CL/EL divergence.
3. **Engine API edge-case enforcement**
   - Reject `ACCEPTED` and `latestValidHash` mismatch.
   - Unit coverage added in `crates/execution/src/client.rs`.
4. **EL persistence hardened for 1-slot finality**
   - `--engine.persistence-threshold=0` in infra and compose.
   - **Hardcoded in load-reth binary** via `DefaultEngineValues` API (reth v1.10.2+).
   - Prevents loss of finalized blocks across restarts.
5. **Tier‑1 harness Engine API stub hardened**
   - `height_from_block_hash` now only accepts synthetic hashes (`[height; 32]`).
   - Prevents false head jumps and `BuiltPayloadMismatch` during `make itest-node`.

## Specification Alignment (Engine API ↔ Malachite)

### Engine API (execution-apis)

Relevant sources:

- `execution-apis/src/engine/common.md`
- `execution-apis/src/engine/paris.md`
- `execution-apis/src/engine/shanghai.md`
- `execution-apis/src/engine/cancun.md`

Key requirements for the gate:

1. **Ordering**: CL **MUST** respect forkchoice update ordering; EL **MUST** process FCU in the same order.
2. **SYNCING semantics**: FCU returns `SYNCING` when the head is unknown/can’t be validated.
3. **Payload build**: Build starts only after a **VALID** head; `SYNCING` implies no build.
4. **Timeouts/retry**: CL should retry transient failures but must not advance consensus on stale heads.

**Implication**: EL readiness is defined by **Engine API status**, not by `eth_getBlockByNumber(latest)`.
HTTP RPC is not a consensus‑critical oracle for head alignment.

### Malachite (consensus + ValueSync)

Relevant sources:

- `malachite/specs/consensus/overview.md`
- `malachite/specs/consensus/design.md`
- `malachite/specs/consensus/message-handling.md`
- `malachite/specs/synchronization/valuesync/README.md`

Key requirements for the gate:

1. **Invalid proposal → PREVOTE nil** is valid and expected.
2. **Async getValue()** can fail; the round proceeds safely (timeouts/nil votes).
3. **Consensus is the single decision point**; ValueSync supplies data but does not decide.

**Implication**: A safety gate that refuses to propose/vote when EL is not aligned is consistent with Malachite.
It should cause nil votes or proposal timeouts, not safety violations.

## Spec‑Driven Flow (Correct Behavior)

1. **Define the canonical head from CL decided state**
   - Use the locally decided head as the canonical CL reference for height/parent checks.
   - If no decided head exists (fresh genesis), seed from execution `genesis.json`.
2. **Pre‑proposal gate via Engine API (no attrs)**
   - Call FCU with `head=CL decided head`, `payloadAttributes=null`.
   - `VALID` → proceed; `SYNCING/INVALID` → do not build and allow the round to time out (nil‑vote path).
3. **Proposer build only after valid FCU**
   - FCU with attrs starts build, then `engine_getPayload`.
4. **Validator proposal validation + nil vote on invalid**
   - If invalid (parent mismatch, timestamp, etc.) → PREVOTE nil.
5. **Consensus proceeds independently once a value is decided**
   - Decided value is final; EL lag delays execution import but must not roll back consensus.
6. **Notify EL after decision (post‑consensus path)**
   - After decision, call `newPayload` and/or FCU to drive EL to the decided head.
   - `SYNCING` → mark degraded and retry; do not invalidate the decided height.

## Persistence Notes (Not Root Cause, but Relevant)

Ultramarine persists consensus state and blobs across restarts:

- **Consensus store**: `store.db` (redb) holds decided values/certificates and metadata.
- **Blob store**: `blob_store.db` (RocksDB) holds blob sidecars and metadata CFs.

The split was **not** caused by DB corruption. The issue was the handoff between the decided head
in the store and the EL head after restarts.

## Optional Postmortem Questions (forensics only)

- Did EL actually contain block 5005 but mark canonical head as 5004 because FCU was not re‑applied after restart?
- What is the exact sequence of CL startup calls and EL responses on LON2 between `14:28:26` and first mismatch?
- Were there transient network partitions (dial failures) that delayed head propagation?
- Was there a window where CL had `max_decided=5005` while EL reported `latest=5004` on LON2/FRA, and did the node proceed without gating?

## Postmortem Validation (2026-01-26)

### Code & Spec Alignment

- Gate uses Engine API FCU (not HTTP RPC) before propose/vote.
- Startup alignment uses CL decided head from store and FCU to realign EL.
- Post‑Decided path sends FCU before newPayload when degraded/syncing.
- Engine API compliance: FCU `ACCEPTED` is rejected; `latestValidHash` mismatch on `VALID` is rejected.
- Execution genesis bootstrap uses `genesis.json` (no HTTP fallback).
- EL persistence threshold hardcoded to `0` in load-reth binary (reth v1.10.2+).

### Ops Validation (Ansible)

- CL restart waits for EL readiness (`eth_syncing == false`) after IPC socket appears.
- load-reth services pass `--engine.persistence-threshold=0`.
- Ultramarine services pass `--execution-genesis-path=/assets/genesis.json`.
- Playbooks updated: `infra/ansible/playbooks/roll.yml`, `deploy.yml`, `blockscout.yml`.

### Tests Executed (local)

Run from `ultramarine/`:

- `full_node_split_head_recovery`
- `full_node_fcu_gate_does_not_require_http_latest`
- `full_node_el_syncing_degrades_node`
- `full_node_el_syncing_still_sends_fcu`
- `full_node_el_syncing_blocks_payload_build`
- `full_node_fcu_accepted_rejected`
- `ultramarine-execution` unit tests for ACCEPTED/latestValidHash edge cases
- `make itest-node` (Tier‑1 full‑node harness; 20/20 scenarios)

## Actions & Status

### Completed (bug resolution)

- [x] Collect logs from LON2 (node‑0/1) and AMS (node‑2/3) for 2026‑01‑22 14:26–14:56.
- [x] Confirm divergent parents in error logs (`0x2177…` vs `0x7268…`).
- [x] Implement safety gate via Engine API status (no HTTP RPC gating).
- [x] Enforce startup alignment: initialize CL head from store; attempt FCU to align EL.
- [x] Add and validate EL‑syncing gate tests (`full_node_el_syncing_*`).
- [x] Update MetadataOnly sync path to advance `latest_block` from payload header.
- [x] Add Tier‑1 test: `full_node_fcu_gate_does_not_require_http_latest`.
- [x] Add Tier‑1 test: `full_node_split_head_recovery`.
- [x] Add Tier‑1 test: `full_node_fcu_accepted_rejected`.
- [x] Add EL readiness checks (`eth_syncing`) before CL start in Ansible playbooks.
- [x] Spec alignment review (Engine API + Malachite).
- [x] Remove misleading “observer-only mode” wording from startup log.
- [x] Document Engine API design decisions in `CLAUDE.md`.
- [x] Reject FCU `ACCEPTED` status as Engine API spec violation.
- [x] Reject FCU `VALID` with `latestValidHash` mismatch.
- [x] Bootstrap `latest_block` from execution `genesis.json` (no HTTP fallback).
- [x] Add `--execution-genesis-path` / `ULTRAMARINE_EL_GENESIS_JSON`.
- [x] Set `--engine.persistence-threshold=0` for load-reth in infra/compose.
- [x] Hardcode `persistence_threshold=0` in load-reth binary via `DefaultEngineValues` API (reth v1.10.2+).
- [x] Document execution genesis + EL persistence in knowledge base.
- [x] Add runbook section for detecting and resolving split‑head.
- [x] Add note on node‑rpc alignment and RPC visibility impact.

### Optional Postmortem Follow‑ups (not required for resolution)

- [ ] Correlate CL decided head vs EL canonical head immediately after restart on each node.
- [ ] Verify whether EL had block 5005 in DB but not canonical (reth debug/inspection).

## References

- Related incident: `ultramarine/docs/journal/BUG-012-fullnode-sync-race.md`
- Gate rules + runbook: `ultramarine/docs/knowledge_base/cl-el-head-gating.md`
- Execution genesis bootstrap: `ultramarine/docs/knowledge_base/execution-genesis.md`
- EL persistence for 1-slot finality: `ultramarine/docs/knowledge_base/el-persistence.md`
- Network inventory: `ultramarine/infra/networks/fibernet/inventory.yml`
