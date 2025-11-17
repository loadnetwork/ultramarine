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

## Blob Testing & Observability

Ultramarine includes comprehensive blob sidecar support (EIP-4844) with full observability.

### Testing Blob Transactions

1. **Start the testnet:**
   ```bash
   make all
   ```

2. **Run blob spam:**
   ```bash
   make spam-blobs
   ```
   This sends 60 seconds of blob transactions (50 TPS, 6 blobs per transaction) to test blob lifecycle.

3. **Check blob metrics:**
   ```bash
   curl http://localhost:29000/metrics | grep blob_engine
   ```

### Blob Metrics

All metrics are exposed at each node's metrics endpoint (`:29000`, `:29001`, `:29002`) with the `blob_engine_` prefix:

**Verification Metrics:**
- `blob_engine_verifications_success_total` - KZG proof verifications that succeeded
- `blob_engine_verifications_failure_total` - KZG proof verifications that failed
- `blob_engine_verification_time` - Histogram of KZG verification duration (seconds)

**Storage Metrics:**
- `blob_engine_storage_bytes_undecided` - Bytes used by undecided blobs
- `blob_engine_storage_bytes_decided` - Bytes used by decided blobs
- `blob_engine_undecided_blob_count` - Current count of undecided blobs

**Lifecycle Metrics:**
- `blob_engine_lifecycle_promoted_total` - Blobs promoted from undecided to decided
- `blob_engine_lifecycle_dropped_total` - Blobs dropped from undecided pool
- `blob_engine_lifecycle_pruned_total` - Decided blobs pruned/archived

**Consensus Integration:**
- `blob_engine_blobs_per_block` - Number of blobs in the last finalized block
- `blob_engine_restream_rebuilds_total` - Blob metadata rebuilds during restream
- `blob_engine_sync_failures_total` - Blob sync/fetch failures

### Grafana Blob Dashboard

The Grafana dashboard at http://localhost:3000 includes a **Blob Engine** section with 9 panels:

1. **Blob Verifications (Total)** - Success and failure counters over time
2. **Blob Verification Latency (P99/P50)** - KZG verification performance
3. **Undecided Blob Count** - Blobs awaiting finalization
4. **Blob Storage Size (Bytes)** - Undecided vs. decided storage
5. **Blob Lifecycle** - Promoted, dropped, and pruned counts
6. **Blobs per Block** - Blob density in finalized blocks
7. **Blob Restream Rebuilds** - Restream operation tracking
8. **Blob Sync Failures** - Sync error tracking

### Verifying Blob Health

After running `make spam-blobs`, verify blob operations are working:

```bash
# Check verification count (should match number of blobs sent)
curl -s http://localhost:29000/metrics | grep "blob_engine_verifications_success_total"

# Check blob lifecycle (promoted should increase as blocks finalize)
curl -s http://localhost:29000/metrics | grep "blob_engine_lifecycle"

# Check storage usage
curl -s http://localhost:29000/metrics | grep "blob_engine_storage_bytes"
```

Expected behavior:
- Verification success count increases with each blob transaction
- Undecided blobs appear briefly, then get promoted to decided
- Storage gauges reflect blob activity
- Zero verification failures under normal operation
- Grafana panels update in real-time

## Integration Test Harness (Phase 5B)

The blob integration suite lives in the dedicated `crates/test` package so Cargo can discover and execute the scenarios without bringing up Docker or the full network.

- **List available tests**
  ```bash
  make itest-list
  ```

- **Run the deterministic in-process suite**
  ```bash
  make itest        # verbose output (~15 s)
  ```
  This loops over each blob-state scenario and runs
  `cargo test -p ultramarine-test --test <name> -- --nocapture` so Tier 0
  stays isolated from the heavier full-node harness. Run the specific command
  directly if you only need a single scenario (e.g.,
  `cargo test -p ultramarine-test --test blob_roundtrip -- --nocapture`).
  Calling `cargo test -p ultramarine-test` without filters executes the **entire**
  suite (Tier 0 + Tier 1).

- **Run the full-node (Tier 1) harness**
  ```bash
  make itest-node   # spins Ultramarine + Engine stub (~1 min; runs each test in isolation)
  ```
  This boots real Ultramarine nodes (Malachite channel actors, WAL, libp2p) and
  drives blobbed proposals end-to-end using the Engine API stub. To avoid shared
  process state between scenarios, the Makefile now invokes each Tier 1 test via
  its own `cargo test -p ultramarine-test --test full_node <name> -- --ignored --nocapture`
  invocation. Running `cargo test -p ultramarine-test --test full_node -- --ignored` directly
  is still useful for local exploration, but the reproducible path is `make itest-node`.

  The suite currently contains:
  - `full_node_blob_quorum_roundtrip` – three validators finalize two blobbed blocks
  - `full_node_validator_restart_recovers` – 4-validator cluster where node 0 is stopped immediately at test start, forcing it to ValueSync heights 1 and 2 from peers when restarted, then verify it can participate in height 3
  - `full_node_restart_mid_height` – crash a follower mid-stream while height 2 is streaming, then ensure it catches up to height 3
  - `full_node_new_node_sync` – run a 4-validator cluster, take the fourth validator offline for the first two heights, then bring it back and ensure ValueSync fetches the missing blobs/metadata
  - `full_node_multi_height_valuesync_restart` – keep validator 3 offline through heights 1–3, let ValueSync import a 1/0/2 blob mix, then restart and inspect the on-disk metadata + parent-root cache.
  - `full_node_restart_multi_height_rebuilds` – drive a 1/0/2 blob mix to height 3, then restart a validator and assert it can rebuild blob sidecars + parent roots solely from disk.
  - `full_node_restream_multiple_rounds_cleanup` – reproduce the multi-round proposer/follower flow with real stores and ensure losing-round blobs are dropped after commit.
  - `full_node_value_sync_commitment_mismatch` – feed a tampered ValueSync package (mismatched commitments vs. blobs) through a full-node state and assert it is rejected with proper metrics/cleanup.
  - `full_node_restream_multi_validator` – end-to-end restream between two real validators to ensure sidecar transmission, metrics, and commit bookkeeping match the state-level harness.
  - `full_node_value_sync_inclusion_proof_failure` – corrupt a blob inclusion proof inside a ValueSync package and verify the full-node state rejects it, records the sync failure, and leaves no blobs behind.
  - `full_node_blob_blobless_sequence_behaves` – commit a blobbed → blobless → blobbed sequence in the real state and assert metrics/blobs match expectations.
  - `full_node_blob_pruning_retains_recent_heights` – override the retention window, commit eight blobbed heights, and ensure pruning + metrics reflect the configured window.
  - `full_node_sync_package_roundtrip` – ingest a synthetic `SyncedValuePackage::Full` and confirm the node promotes blobs/metadata immediately before commit.
  - `full_node_value_sync_proof_failure` – tamper with blob proofs (not commitments/inclusion proofs) to cover the remaining sync failure path.

  Each scenario dumps store/WAL diagnostics if a timeout occurs so failures are actionable, and they all share the new
  builder harness (`FullNodeTestBuilder`) located in `crates/test/tests/full_node/node_harness.rs`.

### What the harness does

- Spins real consensus + blob engine components inside the current Tokio runtime (no Docker).
- Loads the Ethereum mainnet trusted setup once and generates **real** KZG commitments/proofs with `c-kzg`.
- Exercises the ten Phase 5B scenarios end-to-end:
  - `blob_roundtrip`
  - `blob_restream_multi_validator`
  - `blob_restream_multiple_rounds`
  - `blob_new_node_sync`
  - `blob_blobless_sequence`
  - `blob_sync_failure_rejects_invalid_proof`
  - `blob_sync_commitment_mismatch_rejected`
  - `blob_sync_across_restart_multiple_heights`
  - `blob_restart_hydrates_multiple_heights`
  - `blob_pruning_retains_recent_heights`
  - `restart_hydrate`
  - `sync_package_roundtrip`
- Uses the shared helpers in `crates/test/tests/common` (deterministic payloads, payload IDs, mock Engine API).

### When to run it

- After touching consensus/blob-engine/state code.
- Before PRs that touch proposal streaming, restream, sync, or blob storage.
- As a fast regression check prior to launching the Docker testnet.

> **Note:** The harness prints workspace warnings (e.g., missing docs) inherited from other crates; the important signal is that all ten tests pass.

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

EIP‑4844 (blob transactions with real KZG commitments and proofs):

```
cargo run --bin ultramarine-utils -- spam \
  --rpc-url http://127.0.0.1:8545 \
  --blobs --blobs-per-tx 6 --time 60 --rate 50
```

Blob spam generates deterministic 131,072-byte blobs with valid KZG commitments and proofs. Blob lifecycle:
1. Transactions submitted to Reth txpool
2. Consensus proposes blocks with blob transactions
3. Blobs verified (KZG proof check) and stored as "undecided"
4. Upon block finalization, blobs promoted to "decided"
5. Blobs pruned/archived after serving their purpose

### Convenience Make Targets

- `make spam` → 60s @ 500 TPS (EIP-1559 transactions)
- `make spam-blobs` → 60s @ 50 TPS with 6 blobs per transaction

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
