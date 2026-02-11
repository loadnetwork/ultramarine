# PERF-SUMMARY: Fibernet Throughput Journey

Date: 2026-02-11 (UTC)\
Network: `fibernet`\
Audience: engineering retrospective + interview/reference packet

Sanitization:

- hostnames, IPs, and direct RPC URLs are intentionally omitted
- shard aliases used: `shard-a`, `shard-b`, `shard-c`

## Scope

This document is the standalone record for the throughput campaign executed on:

- 2026-02-03 (baseline recovery, scale-up, and high-target stress)
- 2026-02-10 (post-redeploy validation and ceiling probe)

Covered outcomes:

- environment and test setup by phase
- key fixes by phase
- submit-side metrics by phase
- on-chain window metrics from the final probe
- post-run network health checks

## Environment And Test Setup

- topology: 3 validator shards (`shard-a`, `shard-b`, `shard-c`)
- chain profile: `chain_id=1984`, execution gas limit regime `2,000,000,000`
- load-generator class: AMD EPYC 4564P (32 cores), 187 GB RAM

Phase setup highlights:

- 2026-02-03 baseline recovery:
  - 10k run: `120s`, `1,000` accounts, `max_pending=32`, single endpoint
  - 20k run: `60s`, `10,000` accounts, `max_pending=16`, 3 endpoints, `clients_per_endpoint=4`, `batch_size=25`
- 2026-02-03 high-target stress:
  - 50k and 80k target runs from a co-located generator profile
  - `20,000` accounts (`10k` regular + `10k` blob), `clients_per_endpoint=8`, `batch_size=50`
  - elevated txpool and RPC connection ceilings for high fan-out submission
- 2026-02-10 validation/probe:
  - full `wipe + redeploy + rebuild` cycle before testing
  - `load-blaster` binary hash on validator hosts: `8c7b53e8b68990c18aa12ef889182faf34e5e4ed8d1cd4138f38c0ec4b1a5595`
  - `45,000` accounts funded (`0` failures), `0.1 ETH` each (`4,500 ETH` total)
  - post-funding balance floor check (`>=0.09 ETH`): `45,000 / 45,000` sufficient
  - regular transactions only (`blob=0`)

## Repro Commands (Sanitized)

Phase command patterns used in the campaign:

```bash
# 10k validation / 60s (per shard)
./target/release/load-blaster -c configs/fibernet/<shard>.toml run --target-tps 10000 --duration 60s

# 20k probe / 60s (per shard)
./target/release/load-blaster -c configs/fibernet/<shard>.toml run --target-tps 20000 --duration 60s
```

High-target stress profile (co-located generator) used 50k/80k targets with updated load-blaster
parallelism and elevated EL txpool/RPC limits described in this summary.

## Key Fixes By Phase

### Phase 1 (2026-02-03, baseline recovery)

- corrected configuration bottleneck in builder/txpool gas-limit behavior for custom-chain operation (`2B` regime alignment)
- tuned CL runtime to multithreaded production profile
- fixed load-generator rate limiter to true per-second quota semantics
- added periodic nonce drift reconciliation for sender health

### Phase 2 (2026-02-03, high-target stress)

- raised txpool count/size limits for large in-flight transaction sets
- raised RPC max-connections for high client fan-out
- switched test placement to co-located generator profile to reduce network-induced submit constraints

### Phase 3 (2026-02-10, stability and reproducibility)

- validated fresh deployment path end-to-end (`wipe -> deploy -> fund -> run`)
- hardened submission path to sustain `0` submit-side errors in 10k and 20k runs
- standardized post-run drain checks across all shard RPC endpoints

## Key Metrics By Phase

| Phase   | Run           | Target / Duration | Total Submitted | Effective Submit TPS | Errors             | Notes                    |
| ------- | ------------- | ----------------- | --------------- | -------------------- | ------------------ | ------------------------ |
| Phase 1 | Validation    | 10k / 120s        | `1,206,215`     | `10,042`             | `3,735` (`0.31%`)  | backpressure `0`         |
| Phase 1 | Scale-up      | 20k / 60s         | `1,193,860`     | `~19,898` (derived)  | `25,742`           | success `97.8%`          |
| Phase 2 | Stress        | 50k / 120s        | `3,632,369`     | `30,267`             | `19,681` (`0.54%`) | backpressure `0`         |
| Phase 2 | Stress        | 80k / 60s         | `1,807,299`     | `30,116`             | `9,312` (`0.52%`)  | backpressure `0`         |
| Phase 3 | Validation    | 10k / 60s         | `1,472,250`     | `24,537`             | `0`                | cluster total (3 shards) |
| Phase 3 | Ceiling probe | 20k / 60s         | `1,823,713`     | `30,395`             | `0`                | cluster total (3 shards) |

Phase 3 per-shard split for final 20k/60s probe:

- `shard-a`: `924,008` sent (`15,384.19 TPS`), errors `0`
- `shard-b`: `321,788` sent (`5,352.32 TPS`), errors `0`
- `shard-c`: `577,917` sent (`9,631.21 TPS`), errors `0`

## On-Chain Window Metrics (Final 20k Probe)

Selected window:

- block range: `10539..10603` (`65` blocks)
- elapsed time: `109s`

Aggregates:

- total on-chain transactions: `1,824,625`
- on-chain TPS: `16,739.68`
- average tx/block: `28,071.15`
- min tx/block: `0`
- max tx/block: `95,238`
- average gas utilization: `29.47%`
- average block time: `1.70s` (min `1s`, max `5s`)

Interpretation:

- window includes both pre-load idle blocks and a hot segment with repeated near-full gas utilization
- confirms high burst inclusion capacity with visible shard-level submission asymmetry

## Post-Run Health Checks

Checks executed across all shard RPC endpoints after final validation and probe:

- `txpool_status`: `pending=0`, `queued=0`
- `eth_syncing=false`
- `net_peerCount=0x6`

Operational conclusion:

- no residual mempool backlog after load
- nodes remained connected and not in sync-recovery state

## Current Baseline (As Of 2026-02-11)

- stable no-error submission demonstrated at:
  - `24,537 TPS` cluster submit throughput under 10k/60s targets
  - `30,395 TPS` cluster submit throughput under 20k/60s targets
- on-chain reference window for the final probe:
  - `16,739.68 TPS` (`1,824,625 tx / 109s`)
- practical submit ceiling observed so far remains in the `~30k TPS` band for tested configurations

## Deletion Caveat (What Is No Longer Preserved Separately)

With phase-specific reports removed, the repository no longer retains separate per-phase files containing:

- full per-block raw CSV listings for the final 65-block window
- phase-local command transcripts and long-form diagnostic excerpts
- phase-local code snippet context grouped by test day

This summary preserves the canonical fixes, outcomes, and metrics needed for ongoing engineering reference.
