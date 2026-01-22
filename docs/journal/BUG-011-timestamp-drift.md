# BUG-011: Block Timestamp Drift Into Future

## Status: VERIFIED

## Summary
Block timestamps drift ~2 months into future due to `parent.timestamp + 1` at >20 blocks/sec.

## Root Cause
`ultramarine/crates/execution/src/client.rs:203` used `timestamp = parent.timestamp + 1` which is deterministic but detached from wall-clock time.

## Fix
1. [x] Add protocol constants (LOAD_MIN_BLOCK_TIME_SECS=1, LOAD_MAX_FUTURE_DRIFT_SECS=15)
2. [x] Proposer throttling with timeout respect
3. [x] Timestamp calculation: max(now, parent + min_block_time)
4. [x] Validator-side enforcement (critical!)
5. [x] Parent hash match before timestamp validation
6. [x] Wipe and redeploy fibernet
7. [x] Verify timestamps - PASSED (0 second drift, 60 blocks in 60 seconds)

## Protocol Constants
- `LOAD_MIN_BLOCK_TIME_SECS = 1` - minimum slot matching EVM timestamp granularity
- `LOAD_MAX_FUTURE_DRIFT_SECS = 15` - geth/Ethereum standard for future timestamp tolerance

## Validator Enforcement (Protocol Rules)
Validators MUST enforce on every proposal:
1. `proposed_parent_hash == latest_block.block_hash` (parent link)
2. `proposed_ts > parent_ts` (strictly increasing)
3. `proposed_ts >= parent_ts + LOAD_MIN_BLOCK_TIME_SECS` (minimum slot)
4. `proposed_ts <= local_now + LOAD_MAX_FUTURE_DRIFT_SECS` (drift guard)

Violation results in vote nil.

## References
- Protocol constants: ultramarine/crates/types/src/constants.rs
- Throttling: ultramarine/crates/node/src/app.rs:handle_get_value
- Timestamp: ultramarine/crates/execution/src/client.rs
- Validator checks: ultramarine/crates/consensus/src/state.rs:received_proposal_part

## Deployment Notes
After code changes:
1. Build: `docker buildx build --platform linux/amd64 -t loadnetwork/ultramarine:fibernet --push .`
2. Wipe fibernet (required - chain-time already ahead)
3. Verify timestamps match wall-clock (within 15 seconds)
