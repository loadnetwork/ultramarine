# Ultramarine Developer Workflow Guide

This guide shows how to develop, run, test, and observe an Ultramarine local network end‑to‑end, using the included utils, Docker stack, and dashboards. It captures the core flow between the consensus (Malachite/Tendermint) and the execution client (Engine API), and provides troubleshooting and performance tips.

## Prerequisites

- Rust stable toolchain and Cargo
- Docker + Docker Compose
- Optional tooling: Foundry `cast` (for quick EL checks)
- Open ports:
  - Reth JSON‑RPC: `8545`, `18545`, `28545`
  - Engine API: `8551`, `18551`, `28551`
  - Grafana: `3000`

## Workspace Overview

- `crates/node`: app loop wiring the Malachite Channels API with the EL client
- `crates/execution`: Engine API client (HTTP/IPC), Alloy Eth RPC, and transports
- `crates/consensus`: state + store for proposals, blocks, certificates
- `crates/types`: consensus types, Context, and Engine API JSON models
- `crates/utils`: CLI utilities (`genesis`, `spam`)
- `compose.yaml`: Reth x3 + Prometheus + Grafana stack
- `scripts/`: helper scripts (`add_peers.sh`, `spawn.bash`, tmux spawner)
- `docs/`: design docs and this workflow guide

## Quick Start: Local Testnet

This repository includes `make` targets to quickly stand up a complete local testnet using Docker Compose.

### HTTP Variant (Default)

This setup uses the standard HTTP transport for the Engine API.

**Start the testnet:**
```bash
make all
```

This command will:
- Build Ultramarine (debug).
- Generate a genesis file.
- Bring up a Docker stack with 3 Reth nodes, Prometheus, and Grafana.
- Wire the EL peers.
- Generate configurations for 3 Ultramarine nodes.
- Spawn the 3 Ultramarine nodes in `tmux` sessions.

### IPC Variant

This setup uses the more performant IPC (Unix sockets) transport for the Engine API.

**Start the testnet:**
```bash
make all-ipc
```
This command performs the same steps as `make all`, but uses `compose.ipc.yaml` to configure the Docker containers for IPC communication.

### Stopping and Cleaning

- **Stop the stack:** `make stop` (for HTTP) or `make stop-ipc` (for IPC).
- **Clean all data:** `make clean-net` (for HTTP) or `make clean-net-ipc` (for IPC).

## Core Flow (How It Works)

1) ConsensusReady → StartHeight
- App checks EL capabilities, reads EL head, tells Malachite to start at current height.

2) Proposer path
- On `GetValue`, app asks EL to build a block (`getPayloadV3`), stores block bytes, replies with a proposal, and streams proposal parts to peers.
- We respect Malachite’s `timeout`: if block build exceeds the timeout, we do not reply and let consensus take the propose timeout path to prevote‑nil.

3) Non‑proposer path
- Receives proposal parts, verifies signature, reassembles payload, stores undecided proposal + bytes, and replies to consensus.

4) Decide/Commit
- On `Decided`, app submits block to EL (`newPayloadV3`) and updates forkchoice; commits value + block bytes to the store; starts next height.

Notes:
- “nil” in Tendermint is a vote target, not a proposal. The app must not fabricate an “empty” proposal; silence on timeout is correct.
- For sync, values should be stored before replying; this is being hardened.

## Observability & Dashboards

- Grafana: http://localhost:3000
  - Engine metrics panels:
    - `reth_engine_rpc_get_payload_v3_count`: payload builds
    - `reth_engine_rpc_new_payload_*`: block import status
    - `reth_engine_rpc_forkchoice_updated_*`: head updates
  - Throughput and error state panels provide quick health checks.

- Logs:
  - Nodes are started via tmux. Inspect sessions:
    - `tmux ls` → list sessions
    - `tmux attach -t <session>` → attach
  - App logs show rounds, proposals, decisions, tx counts, and bytes/sec.

## Utils: Genesis & Spammer

### Generate Genesis

```
cargo run --bin ultramarine-utils -- genesis \
  --output ./assets/genesis.json \
  --chain-id 1337
```

- Writes the chain config and prefunds 3 test signers.
- Chain id flows into the spammer automatically (or via `--chain-id`).

### Spam Transactions

EIP‑1559, auto‑detect chain id:

```
cargo run --bin ultramarine-utils -- spam \
  --rpc-url http://127.0.0.1:8545 \
  --time 60 --rate 500
```

EIP‑1559, explicit chain id:

```
cargo run --bin ultramarine-utils -- spam \
  --rpc-url http://127.0.0.1:8545 \
  --chain-id 1337 \
  --time 60 --rate 500
```

EIP‑4844 (experimental):

```
cargo run --bin ultramarine-utils -- spam \
  --rpc-url http://127.0.0.1:8545 \
  --blobs --time 30 --rate 50
```

Warning: On Cancun (Engine V3) non‑proposer blob imports may fail without sidecar support; behavior can be inconsistent until Engine API V4 sidecar wiring is implemented (see `docs/4844-sidecar-blueprint.md`).

### Convenience Make Targets

- `make spam` → 60s @ 500 TPS to `http://127.0.0.1:8545`

## Quick EL Sanity Checks

Using Foundry `cast`:
- `cast block-number`
- `cast block 3`
- `cast rpc txpool_status`

Using curl:

```
curl -s http://127.0.0.1:8545 \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

## Development Workflow

- Build: `make build` (release) or `make build-debug`
- Run: `make run ARGS="--help"`
- Tests: `make test`
- Lint: `make lint` (fmt, clippy, sort, typos, TOML format)
- Docker image: `make docker-build`

Recommended cadence:
- Use `make all` to bootstrap once; iterate by rebuilding the node (`make build-debug`) and restarting specific tmux panes, or re‑run `scripts/spawn.bash`.

## Verifying Health

- Chain progresses:
  - App logs show repeated `Started round` and `Consensus has decided on value`.
  - Grafana `new_payload` valid count increases; `forkchoice_updated` valid increases.
  - `cast block-number` increments consistently.

- Transactions flow:
  - Spammer prints txs/sec; `txpool_status` shows pending/queued.
  - App logs report tx count per block and aggregate bytes/sec.

## Troubleshooting

- Engine capabilities check fails:
  - Ensure reth is running and JWT secret is readable at `./assets/jwtsecret`.

- Missing or empty block bytes on `Decided`:
  - Indicates undecided proposal bytes weren’t stored (e.g., sync path). The app now avoids panics and returns a clear error; restart node or re‑run the round.

- Blob transactions fail on non‑proposers:
  - Expected on Cancun without sidecars. Either avoid blobs or proceed to V4 sidecar integration (see blueprint doc).

- High TPS measurements flatten:
  - Increase EL txpool/gas settings in `compose.yaml`.
  - Tune spammer rate and RPC timeout (`reqwest` has 1s timeout by default in utils).

### Round‑0 Timeouts (Startup Race)

On manual starts, the proposer can reach round 0 and stream the proposal before the other
validators fully initialise networking and Engine connectivity. If peers miss the proposal
parts before their Prevote time limit, they will prevote `nil`, and the network can sit in
Prevote rebroadcast loops.

Workarounds and tuning:

- Start order: first ensure EL is up and peers are wired (`scripts/add_peers.sh`), then start all
  nodes together (use `make all`) or start the proposer last.
- Increase initial timeouts: bump `TimeoutConfig` for round‑0 (Propose/Prevote) in the generated
  testnet config for development runs to be more forgiving.
- Proposer grace (optional): add a small delay before building/streaming the first proposal until
  N−1 peers are connected. A future toggle like `ULTRAMARINE_PROPOSER_GRACE_MS` can help avoid the
  startup race without impacting steady‑state performance.

## Performance Tips

- Use `--rate` and `--time` to shape load; keep an eye on Grafana for `get_payload_v3` and `new_payload_v3` rates and failures.
- Reduce proposal timeouts in consensus config if payloads are reliably fast; increase them for larger payloads.
- Run `make spam` from a separate terminal to continuously load the EL.

## Advanced: 4844 Sidecar Plan

See `docs/4844-sidecar-blueprint.md` for a pragmatic approach to integrate blob sidecars in a Tendermint setting using Engine API V4. In short: stream and store sidecars as part of proposals, then call `engine_newPayloadV4` with the reassembled bundle on `Decided`.

---

If something is unclear or you want automated scripts for end‑to‑end smoke tests, consider adding a `make e2e` that runs spam + assertions against Grafana/EL stats.

## Configuring Engine/Eth Endpoints

Ultramarine supports both HTTP and IPC for the Engine API, and an HTTP Eth1 RPC endpoint for non‑engine calls.

- Flags (via `ultramarine start ...`):
  - `--engine-ipc-path /path/to/geth.ipc`
  - `--engine-http-url http://localhost:8551` (ignored if `--engine-ipc-path` is set)
  - `--eth1-rpc-url http://localhost:8545`
  - `--jwt-path ./assets/jwtsecret`

Priority and defaults:
- CLI flags (above) take precedence.
- If not provided, Ultramarine derives “development defaults” from the node moniker (`test-0/1/2`) to match the docker compose mapping:
  - test-0 → http://localhost:8545 (Eth) and http://localhost:8551 (Engine)
  - test-1 → http://localhost:18545 / 18551
  - test-2 → http://localhost:28545 / 28551
- JWT defaults to `./assets/jwtsecret` which `make all` creates if missing.

IPC notes:
- Ensure your EL exposes an IPC socket (`--ipcpath`) and that it’s available on the host (via volume mount). Then start Ultramarine with `--engine-ipc-path`.
