# Block Timing & Timestamps in Load Network

## Protocol Constants (same everywhere)
- `LOAD_MIN_BLOCK_TIME_SECS = 1`
- `LOAD_MAX_FUTURE_DRIFT_SECS = 15`

Defined in: `ultramarine/crates/types/src/constants.rs`

## Invariants (validator-enforced)
1. `proposed_parent_hash == latest_block.block_hash` (parent link)
2. `proposed_ts > parent_ts` (strictly increasing)
3. `proposed_ts >= parent_ts + 1` (minimum block time)
4. `proposed_ts <= now + 15` (drift guard)

## Why 1 second minimum
EVM timestamp = integer seconds. Cannot have >1 block/sec with real wall-clock time.

## Why 15 seconds drift
geth/Ethereum canonical value for future timestamp tolerance. Industry standard.
See: `geth/params/protocol_params.go` (allowedFutureBlockTimeSeconds = 15)

## Operational Requirement: NTP
Validators MUST run NTP (ntpd/chronyd). Proposers with clock >15s ahead will have
proposals rejected network-wide, resulting in nil-rounds until their clock syncs.

## Height 1 Behavior
At startup, `latest_block` is initialized from EL (genesis block if no committed blocks).
This ensures all timestamp checks (parent hash match, ts > parent, ts >= parent + 1, drift guard)
work from height 1 without exceptions.

## Timestamp Calculation (Proposer Side)
```rust
fn next_block_timestamp(parent_timestamp: u64) -> u64 {
    std::cmp::max(current_unix_time(), parent_timestamp + LOAD_MIN_BLOCK_TIME_SECS)
}
```

## Throttling (Proposer Side)
Before building a block, proposer waits until `now >= parent_ts + 1`:
- Uses subsecond precision to avoid ~1s jitter
- Respects GetValue timeout to preserve liveness
- If throttle exceeds timeout, refuses to propose (nil-vote via timeout)

## TPS Considerations
Block frequency is limited to 1 block/sec due to EVM timestamp granularity.
To maintain throughput, gas limit must compensate:
- TPS = (gas_limit x blocks/sec) / gas_per_tx
- With 2B gas limit: TPS = 2B / gas_per_tx

## References
- Malachite proto: consensus.proto:49-53
- Arbitrum: docs.arbitrum.io/block-numbers-and-time
- BeaconKit: NextBlockDelay
- geth: params/protocol_params.go (allowedFutureBlockTimeSeconds = 15)
