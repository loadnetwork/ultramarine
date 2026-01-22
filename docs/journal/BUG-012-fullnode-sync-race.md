# BUG-012: Fullnode Sync Race Condition After Rolling Restart

## Status: FIXED (pending wider validation)

## Summary
Fullnodes lose sync after rolling restart because EL lag was not treated as sync-mode during normal operation. When EL returned SYNCING, the CL stopped sending FCU (forkchoiceUpdated), so the EL never received a sync target and remained behind. Fixed by always sending FCU when EL is degraded and aligning CL head tracking even when execution is pending.

## Symptoms
- Fullnode EL stuck at block N
- CL trying to push block N+50+
- EL keeps returning SYNCING status
- Gap grows indefinitely
- Blockscout can't index new blocks

## Root Cause

### Rolling Restart Sequence
1. EL restarted with block N as latest
2. IPC socket appears - Ansible considers EL "ready" (premature)
3. Ultramarine restarted - fetches latest block (N) from EL
4. ValueSync starts - tries to push block N+X
5. EL returns SYNCING - doesn't have parent blocks N+1 to N+X-1
6. CL marks execution pending and skips FCU in non-sync mode
7. EL has no sync target → stays behind until P2P catches up (often slow)

### Code Locations
- Rolling restart order: `infra/ansible/playbooks/roll.yml` lines 36-62
- EL SYNCING handling: `crates/consensus/src/state.rs` (newPayload retry + execution_pending)
- FCU-before-newPayload: now triggered when EL is degraded (not only sync mode)
- Sync block processing: `crates/node/src/app.rs` lines 823-854
- Consensus head tracking: `latest_block` now advances even when execution is pending

### Why Fullnodes Are Affected (Not Validators)
- Validators produce blocks locally, don't depend on syncing
- Fullnodes must receive and import blocks from peers
- Without FCU, EL has no sync target and relies on P2P only

## Fixes Implemented

1. **Treat EL lag as sync mode**  
   When EL returns SYNCING or `el_degraded` is set, send FCU **before** newPayload.  
   This gives EL a target head and unblocks pipeline sync.

2. **Advance consensus head even when execution is pending**  
   This keeps parent-hash validation consistent on validators while EL lags.

## Follow-ups (Optional Hardening)

1. Add backfill queue for `execution_pending` heights to replay once EL catches up
2. Improve EL readiness gate (wait for `eth_syncing == false` or FCU success)

## Workaround
Wipe fullnode state and let it resync from genesis:
```bash
make net-wipe NET=<network> LIMIT=<fullnode-host> WIPE_CONFIRM=YES WIPE_STATE=true WIPE_NODES=<node-name>
make net-blockscout NET=<network>
```
## References
- Industry pattern: send FCU before newPayload when EL is syncing (Lighthouse/Teku/Prysm)
- Related: `ultramarine/docs/fibernet_deploy_progress.md` (FIX-008 FCU-before-newPayload)

## Testing
1. Integration test: `full_node_el_syncing_still_sends_fcu` (make itest-node)
2. Full restart a synced fullnode; verify EL catches up and CL/EL heads converge
3. Verify blockscout continues indexing after restart
