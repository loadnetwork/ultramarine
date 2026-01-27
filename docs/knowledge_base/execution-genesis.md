# Execution Genesis Bootstrap

## Goal

Make CL and EL agree on the same execution genesis header **without** HTTP RPC or
`reth-chainspec` dependencies.

## Inputs

The CL reads the same `genesis.json` used by load-reth (`--chain`).

- CLI: `--execution-genesis-path=/path/to/genesis.json`
- Env: `ULTRAMARINE_EL_GENESIS_JSON=/path/to/genesis.json`

## Behavior

At startup, Ultramarine builds an `ExecutionBlock` from the execution genesis file
and uses it as `latest_block` when there is no decided head in the CL store.

Derived fields:

- `state_root` computed from `alloc` (MPT root)
- Fork-gated header fields (if active at genesis):
  - London: `base_fee_per_gas`
  - Shanghai: `withdrawals_root`
  - Cancun: `parent_beacon_block_root`, `blob_gas_used`, `excess_blob_gas`
  - Prague: `requests_hash`

## Implementation

- `crates/node/src/node.rs` builds the header and hashes it to `block_hash`.
- `crates/cli/src/cmd/start.rs` exposes `--execution-genesis-path`.
- Infra wiring:
  - Systemd: `infra/templates/systemd/ultramarine@.service.j2` mounts `/assets` and passes `--execution-genesis-path=/assets/genesis.json`.
  - Docker compose: `compose.ipc.yaml` passes `--execution-genesis-path=/assets/genesis.json`.

## Failure Mode

If the execution genesis path is missing or invalid, startup fails fast to avoid
silent CL/EL divergence.

## Related

- CL/EL head gate: `docs/knowledge_base/cl-el-head-gating.md`
- EL persistence: `docs/knowledge_base/el-persistence.md`
