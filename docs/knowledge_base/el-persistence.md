# EL Persistence for 1-Slot Finality

## Problem

Reth defaults to keeping up to **2 canonical blocks in memory** before persisting to disk.
For chains with **single-block finality**, that creates a restart hazard:

- Block N is finalized by the CL
- EL has not persisted N yet (still in memory)
- EL restarts -> block N is missing on disk
- CL sends FCU to head=N -> EL returns **SYNCING**
- CL/EL diverge and consensus can stall

Ethereum mainnet tolerates this because finality is ~64 blocks. Load cannot.

## Solution: Hardcoded in load-reth (v1.10.2+)

As of reth v1.10.2, the `DefaultEngineValues` API allows setting engine defaults programmatically.
load-reth now **hardcodes `persistence_threshold=0`** in `main.rs` before CLI parsing:

```rust
use reth_node_core::args::DefaultEngineValues;

DefaultEngineValues::default()
    .with_persistence_threshold(0)
    .try_init()
    .ok();
```

This ensures:

- The setting is always applied regardless of command-line flags
- No ops/infra configuration can accidentally override it
- Defense-in-depth: infra configs still pass the flag, but it's not required

**Note:** `try_init()` is best-effort; if defaults were already set earlier in the
process (tests or other binaries), it will no-op. That’s why infra still passes
`--engine.persistence-threshold=0` as a secondary guard.

## Why persistence_threshold=0 Is Correct for Load

- Every canonical block is final immediately (1-slot finality)
- There is no reorg window to benefit from buffering
- Disk I/O overhead is minimal at 1 block/sec

## Configuration Layers (Defense-in-Depth)

### Primary: Hardcoded in load-reth binary

`load-reth/src/main.rs` sets the default before CLI parsing. This is the authoritative layer.

### Secondary: Infra configuration (redundant but safe)

- Systemd: `infra/templates/systemd/load-reth@.service.j2`
  - `--engine.persistence-threshold={{ loadnet_el_engine_persistence_threshold | default(0) }}`
- Docker compose:
  - `compose.yaml`
  - `compose.ipc.yaml`

## Verification

After restart:

1. CL applies FCU to decided head.
2. EL returns **VALID** (not SYNCING).
3. Proposals/votes resume without split-head.

If EL still returns SYNCING, check:

- EL datadir is intact
- persistence flag is present
- no manual pruning wiped recent blocks

## Notes

- Chains with multi-block finality can use higher thresholds.
- For Load, **threshold=0** is the safe default.

## Related

- CL/EL head gate: `docs/knowledge_base/cl-el-head-gating.md`
- Incident report: `docs/journal/BUG-013-chain-split-parent-mismatch.md`
