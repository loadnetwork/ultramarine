# PERF-001: Fibernet Throughput Fix and 10k TPS Validation

## Status: VERIFIED (deployed)

## Summary
Initial throughput was ~100 TPS due to reth defaults (36M gas for custom chains). After fixing builder/txpool gas limits and tuning CL runtime, we validated near-10k TPS sustained load on fibernet with stable 1s block time and zero timestamp drift.

## Root Cause (Original Bottleneck)
`reth/crates/node/core/src/cli/config.rs:44-55` defaults custom chains to 36M gas:
```rust
fn gas_limit_for(&self, chain: Chain) -> u64 {
    match chain.kind() {
        ChainKind::Named(Mainnet | Sepolia | Holesky | Hoodi) => ETHEREUM_BLOCK_GAS_LIMIT_60M,
        _ => ETHEREUM_BLOCK_GAS_LIMIT_36M,  // <- 36M for custom chains!
    }
}
```

| Parameter | Chain Spec | Builder Default | Impact |
|-----------|------------|-----------------|--------|
| Gas Limit | 2B | 36M | 98% capacity lost |

## Fixes Applied

### Load-reth / Infra
1. [x] Code fix: `load-reth/src/engine/builder.rs:83` - default to `LOAD_EXECUTION_GAS_LIMIT`
2. [x] Systemd template: add builder/txpool flags (2B gas, larger pools)
3. [x] CL configs: `log_level=info`, `runtime.flavor=multi_threaded`
4. [x] Genesis: add test account

### Load-blaster
5. [x] Rate limiter fix: `load-blaster/src/rate.rs` switched to `Quota::per_second(tps)`
6. [x] Nonce health check: `load-blaster/src/engine.rs` resync drifted accounts every 5s

## Deployment
- [x] Wipe and redeploy fibernet
- [x] Verify throughput and timestamps

## Load Test Report (10k TPS)

### Test Configuration
| Parameter | Value |
|-----------|-------|
| Target TPS | 10,000 |
| Duration | 120s |
| Test Accounts | 1,000 |
| Max Pending/Account | 32 |
| Endpoint | http://67.213.117.143:8545 |

### Submission Metrics
| Metric | Value |
|--------|-------|
| Total Submitted | 1,206,215 |
| Submission Rate | 10,042 TPS |
| Errors | 3,735 (0.31%) |
| Backpressure Events | 0 |

### Inclusion Metrics
| Metric | Value |
|--------|-------|
| Blocks Analyzed | 201 (5250-5450) |
| Blocks with Transactions | 105 |
| Total Included | 1,191,018 |
| Effective TPS | 9,925 |
| Peak Block (#5305) | 21,596 txs |

### Block Timing
- Effective block time: 1.000s (average over the test window)
- Zero deviations observed at peak load (20k+ txs/block)

## Capacity Note (Not Gas-Limit Constrained)
Assuming simple transfers (~21,000 gas), the peak block of 21,596 txs is:
- ~453,516,000 gas used (~23% of 2B gas limit)
This indicates the test was not gas-limit constrained; remaining headroom is in tx submission, pool ingest, or builder fill rate.

## Verification
```bash
cast rpc eth_getBlockByNumber latest false --rpc-url http://67.213.117.143:8545 \
  | jq -r '.result.gasLimit, .result.gasUsed, (.result.transactions|length)'
```

## Code Changes (Key Excerpts)

### load-reth/src/engine/builder.rs
```rust
use crate::chainspec::LOAD_EXECUTION_GAS_LIMIT;
// ...
let gas_limit = conf.gas_limit().unwrap_or(LOAD_EXECUTION_GAS_LIMIT);
```

### ultramarine/infra/templates/systemd/load-reth@.service.j2
```bash
--builder.gaslimit=2000000000
--builder.interval=50ms
--builder.deadline=2
--builder.max-tasks=10
--txpool.gas-limit=2000000000
--txpool.max-account-slots=32
--txpool.pending-max-count=50000
--txpool.queued-max-count=50000
--txpool.pending-max-size=512
--txpool.queued-max-size=512
```

## References
- Knowledge base: [el-gas-limits.md](../knowledge_base/el-gas-limits.md)
- Knowledge base: [cl-runtime.md](../knowledge_base/cl-runtime.md)
