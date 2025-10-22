# Blob Sync Gap Analysis - Critical Findings

**Date**: 2025-10-21
**Status**: 🚨 **CRITICAL GAPS IDENTIFIED**
**Impact**: **DATA AVAILABILITY BROKEN FOR SYNCING PEERS**

---

## Executive Summary

While the core blob integration (Phases 1-5) is complete and working for **live consensus**, there are **critical gaps in state synchronization** that break data availability guarantees for peers that fall behind.

**Core Issue**: Syncing peers receive Value metadata but NOT blob data, making it impossible to import blocks with blob transactions.

---

## What IS Working ✅

### 1. Live Consensus Flow (Proposer → Validators) ✅

**Complete implementation** for the happy path:

```
Proposer (height 100)
  ↓ GetValue
  ↓ generate_block_with_blobs() → ExecutionPayload + BlobsBundle
  ↓ stream_proposal(value, bytes, blobs_bundle)
  ├─> Init(height, round, metadata)
  ├─> Data(execution_payload chunks)
  ├─> BlobSidecar(0) ← 131KB
  ├─> BlobSidecar(1) ← 131KB
  ├─> ...
  └─> Fin(signature)

Validators
  ↓ ReceivedProposalPart
  ↓ assemble_and_store_blobs()
  ├─> verify_and_store() ← KZG verification
  └─> blob_engine stores blobs

Consensus decides
  ↓ Decided handler
  ↓ get_for_import(height)
  ↓ Versioned hash verification ← Lighthouse parity
  ↓ notify_new_block(payload, versioned_hashes)
  └─> Block imported with DA guarantee ✅
```

**Files**:
- `crates/node/src/app.rs:98-156` - GetValue with blob streaming
- `crates/node/src/app.rs:362-392` - ReceivedProposalPart
- `crates/consensus/src/state.rs:589-694` - Blob verification & storage
- `crates/node/src/app.rs:415-580` - Decided handler with blob import

**Status**: ✅ **FULLY WORKING** - All tests passing

---

## What IS NOT Working ❌

### 1. State Sync (Critical Gap) 🚨

**Scenario**: Peer falls behind and needs to catch up

```
Lagging Peer (height 100)     Advanced Peer (height 1000)
     |                              |
     |  "I'm at height 100"         |
     |----------------------------->|
     |                              |
     |   ProcessSyncedValue(101)    |
     |<-----------------------------|
     |   value_bytes: Value         | ← Only metadata!
     |   (NO BLOBS!)                |
     |                              |
     | ❌ Tries to import block 101 |
     | ❌ get_for_import(101)       |
     | ❌ ERROR: No blobs found     |
     | ❌ CRASH                     |
```

#### Current Implementation (BROKEN)

**File**: `crates/node/src/app.rs:591-609`

```rust
AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
    info!(%height, %round, "🟢🟢 Processing synced value");

    let value = decode_value(value_bytes);  // ← Only Value (metadata)

    // ❌ PROBLEM: No blob retrieval!
    // ❌ PROBLEM: No blob storage!
    // ❌ PROBLEM: Decided handler will fail when it tries to import!

    if reply.send(ProposedValue { ... }).is_err() {
        error!("Failed to send ProcessSyncedValue reply");
    }
}
```

#### What's Missing

1. **No blob data transfer**
   - `ProcessSyncedValue` only receives `value_bytes` (Value metadata)
   - No mechanism to retrieve blobs from peer
   - No RPC method like `GetBlobsForHeight(height, round)`

2. **No blob storage**
   - Even if we had blob data, it's not stored
   - `blob_engine.verify_and_store()` is never called
   - Blobs need to be in "decided" state for import

3. **No execution payload storage**
   - The commented-out code (lines 610-660) shows the CORRECT way
   - Need to call `state.store.store_undecided_proposal()`
   - Need to store execution payload bytes separately

#### Impact

**SEVERITY**: 🚨 **CRITICAL - BREAKS DATA AVAILABILITY**

- New peers **cannot sync** any blocks with blob transactions
- Peers that fall behind **cannot catch up**
- Network becomes **non-functional** after a single missed block with blobs
- **DA guarantee is completely broken** for syncing peers

---

### 2. GetDecidedValue (Critical Gap) 🚨

**Scenario**: Advanced peer helps lagging peer catch up

```
Advanced Peer (height 1000)    Lagging Peer (height 100)
     |                              |
     |   GetDecidedValue(101)       |
     |<-----------------------------|
     |                              |
     | RawDecidedValue {            |
     |   certificate,               |
     |   value_bytes,               | ← Only Value metadata!
     |   ❌ blobs: NOT INCLUDED     |
     | }                            |
     |----------------------------->|
     |                              |
     |                              | ❌ Peer gets Value but NO blobs
     |                              | ❌ Cannot import block
     |                              | ❌ Stuck forever
```

#### Current Implementation (BROKEN)

**File**: `crates/node/src/app.rs:669-683`

```rust
AppMsg::GetDecidedValue { height, reply } => {
    info!(%height, "🟢🟢 GetDecidedValue");
    let decided_value = state.get_decided_value(height).await;

    let raw_decided_value = decided_value.map(|decided_value| RawDecidedValue {
        certificate: decided_value.certificate,
        value_bytes: ProtobufCodec.encode(&decided_value.value).expect("..."),
        // ❌ MISSING: Blob data!
    });

    if reply.send(raw_decided_value).is_err() {
        error!("Failed to send GetDecidedValue reply");
    }
}
```

#### What's Missing

1. **No blob retrieval**
   - Need to call `blob_engine.get_decided(height)` or similar
   - Blobs exist in blob_engine but aren't sent to peer

2. **No blob serialization**
   - `RawDecidedValue` doesn't have a field for blobs
   - May need to extend type or use separate channel

3. **No execution payload**
   - Need to send full execution payload bytes for SSZ reconstruction
   - Currently only sending Value metadata

#### Impact

**SEVERITY**: 🚨 **CRITICAL - BREAKS PEER SYNC**

- Peers requesting historical blocks get incomplete data
- Syncing is one-sided (receiving side is broken, sending side is broken)
- **Complete failure of state sync mechanism**

---

### 3. RestreamProposal (High Priority) ⚠️

**Scenario**: Validator misses some blob parts during gossip

```
Validator (missed BlobSidecar 2)
     |
     | ❌ Proposal incomplete (missing blob 2/6)
     | ❌ Cannot verify KZG proofs
     | ❌ Cannot vote
     |
     | Needs: RestreamProposal(height, round, value_id, address)
     | ❌ NOT IMPLEMENTED
     |
     | Result: Prevote(nil) → Consensus timeout → New round
```

#### Current Implementation (NOT IMPLEMENTED)

**File**: `crates/node/src/app.rs:292-355`

```rust
AppMsg::RestreamProposal { height: _, round: _, valid_round: _, address: _, value_id: _ } => {
    error!("🔴 RestreamProposal not implemented");
    // Entire handler is commented out (63 lines of code)!
}
```

#### What's Missing

The commented-out code shows the CORRECT approach:

1. Retrieve proposal from store
2. Retrieve execution payload bytes
3. **Retrieve blobs from blob_engine** (MISSING in commented code!)
4. Re-stream all parts using `stream_proposal()`

#### Current Code Issues

Even the commented-out implementation is incomplete:

```rust
// TODO: The `state.stream_proposal` function uses the address of the *current* node
// when creating the `Init` part of the stream. For restreaming, it should use the
// address of the *original* proposer, which is available here as `address`.
```

**Missing from commented code**:
- ❌ No blob retrieval
- ❌ No BlobsBundle reconstruction
- ❌ Can't call `stream_proposal()` without blobs

#### Impact

**SEVERITY**: ⚠️ **HIGH - CAUSES CONSENSUS DELAYS**

- Validators who miss parts during network glitches can't recover
- Causes unnecessary timeouts and new rounds
- Mentioned in startup TODO (lines 99-102): round-0 timing issues
- Not a complete blocker but significantly degrades performance

---

## Detailed Fix Requirements

### Fix 1: Implement Blob Sync in ProcessSyncedValue 🚨

**Priority**: **CRITICAL** (P0)
**Estimated Time**: 1 day
**Blocker for**: Network sync, new peer onboarding

#### Required Changes

**File**: `crates/node/src/app.rs:591-661`

```rust
AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
    info!(%height, %round, "🟢🟢 Processing synced value");

    // 1. Decode the Value to check if it has blobs
    let value = decode_value(value_bytes.clone());

    // 2. If value has blob commitments, request blobs from peer
    if !value.metadata.blob_kzg_commitments.is_empty() {
        // TODO: Need to add peer tracking to know who sent this
        // TODO: Need RPC method: request_blobs_for_height(peer, height, round)

        let blob_count = value.metadata.blob_kzg_commitments.len();
        info!("Synced value has {} blobs, requesting from peer...", blob_count);

        // Request blobs (NEW RPC NEEDED)
        let blobs = match channels.network.request_blobs(peer, height, round).await {
            Ok(blobs) => blobs,
            Err(e) => {
                error!("Failed to retrieve blobs for synced value: {}", e);
                if reply.send(None).is_err() {
                    error!("Failed to send ProcessSyncedValue reply");
                }
                return;
            }
        };

        // 3. Verify and store blobs
        let round_i64 = round.as_i64();
        if let Err(e) = state.blob_engine().verify_and_store(height, round_i64, &blobs).await {
            error!("Blob verification failed for synced value: {}", e);
            if reply.send(None).is_err() {
                error!("Failed to send ProcessSyncedValue reply");
            }
            return;
        }

        // 4. Mark blobs as decided immediately (synced values are already decided)
        if let Err(e) = state.blob_engine().mark_decided(height, round_i64).await {
            error!("Failed to mark synced blobs as decided: {}", e);
            if reply.send(None).is_err() {
                error!("Failed to send ProcessSyncedValue reply");
            }
            return;
        }
    }

    // 5. Store the proposal (uncomment existing code!)
    let proposed_value = ProposedValue { ... };
    if let Err(e) = state.store.store_undecided_proposal(proposed_value.clone()).await {
        error!("Failed to store synced value: {}", e);
        if reply.send(None).is_err() {
            error!("Failed to send ProcessSyncedValue reply");
        }
        return;
    }

    // 6. Send to consensus
    if reply.send(Some(proposed_value)).is_err() {
        error!("Failed to send ProcessSyncedValue reply");
    }
}
```

#### New Dependencies

**Need to add**:
1. New RPC method in networking layer: `request_blobs_for_height(peer, height, round)`
2. Peer tracking in `ProcessSyncedValue` (currently we don't know which peer sent it!)
3. Execution payload bytes storage (need full payload, not just Value)

---

### Fix 2: Extend GetDecidedValue to Include Blobs 🚨

**Priority**: **CRITICAL** (P0)
**Estimated Time**: 4 hours
**Blocker for**: Helping lagging peers catch up

#### Required Changes

**File**: `crates/node/src/app.rs:669-683`

```rust
AppMsg::GetDecidedValue { height, reply } => {
    info!(%height, "🟢🟢 GetDecidedValue");

    // 1. Get decided value
    let decided_value = state.get_decided_value(height).await;

    let raw_decided_value = if let Some(decided_value) = decided_value {
        // 2. Get execution payload bytes
        let block_data = match state.get_block_data(height, decided_value.certificate.round).await {
            Some(data) => data,
            None => {
                error!("Missing block data for decided value at height {}", height);
                None
            }
        };

        // 3. Get blobs if value has commitments
        let blobs = if !decided_value.value.metadata.blob_kzg_commitments.is_empty() {
            match state.blob_engine().get_decided(height).await {
                Ok(blobs) => Some(blobs),
                Err(e) => {
                    error!("Failed to retrieve blobs for decided value at height {}: {}", height, e);
                    None // Continue without blobs (better than failing completely)
                }
            }
        } else {
            None
        };

        // 4. Encode value
        let value_bytes = ProtobufCodec.encode(&decided_value.value)
            .expect("Failed to encode value");

        // 5. Create extended RawDecidedValue
        Some(RawDecidedValue {
            certificate: decided_value.certificate,
            value_bytes,
            // TODO: Need to extend RawDecidedValue type to include:
            // - block_data: Option<Bytes>
            // - blobs: Option<Vec<BlobSidecar>>
        })
    } else {
        None
    };

    if reply.send(raw_decided_value).is_err() {
        error!("Failed to send GetDecidedValue reply");
    }
}
```

#### New Dependencies

**Need to add**:
1. Extend `RawDecidedValue` type (Malachite type or app-layer wrapper)
2. Add `get_decided(height)` method to BlobEngine trait
3. Ensure block_data is stored for decided values

**Alternative Approach**:
- Keep `GetDecidedValue` as-is for Value only
- Add separate RPC: `GetBlobsForHeight(height)` → `Vec<BlobSidecar>`
- Receiver calls both RPCs when syncing

---

### Fix 3: Implement RestreamProposal ⚠️

**Priority**: **HIGH** (P1)
**Estimated Time**: 4 hours
**Blocker for**: Reliable gossip under network conditions

#### Required Changes

**File**: `crates/node/src/app.rs:292-355`

```rust
AppMsg::RestreamProposal { height, round, valid_round, address, value_id } => {
    info!(%height, %round, %value_id, %address, "Received request to restream proposal");

    // Determine which round the proposal was from
    let proposal_round = if valid_round == Round::Nil {
        round
    } else {
        valid_round
    };

    // 1. Retrieve proposal from store
    match state.store.get_undecided_proposal(height, proposal_round, value_id).await {
        Ok(Some(proposal)) => {
            // Verify proposer matches
            if proposal.proposer != address {
                error!(
                    "Proposer mismatch: expected {}, got {}",
                    address, proposal.proposer
                );
                return;
            }

            // 2. Retrieve execution payload bytes
            let block_data = match state.get_block_data(height, proposal_round).await {
                Some(data) => data,
                None => {
                    error!("Missing block data for restream at height {} round {}", height, proposal_round);
                    return;
                }
            };

            // 3. Retrieve blobs if proposal has commitments
            let blobs_bundle = if !proposal.value.metadata.blob_kzg_commitments.is_empty() {
                let round_i64 = proposal_round.as_i64();
                match state.blob_engine().get_undecided(height, round_i64).await {
                    Ok(blobs) => {
                        // Reconstruct BlobsBundle from sidecars
                        Some(BlobsBundle::from_sidecars(&blobs))
                    }
                    Err(e) => {
                        error!("Failed to retrieve blobs for restream: {}", e);
                        return;
                    }
                }
            } else {
                None
            };

            // 4. Create LocallyProposedValue for streaming
            // NOTE: We use current round for the stream, not the original proposal round
            let locally_proposed_value = LocallyProposedValue {
                height,
                round, // Current round
                value: proposal.value,
            };

            // 5. Stream the proposal
            // TODO: Need to modify stream_proposal() to accept optional proposer address
            // For now it will use self.address which is incorrect for restreaming
            for stream_message in state.stream_proposal(locally_proposed_value, block_data, blobs_bundle) {
                info!(%height, %round, "Restreaming proposal part: {stream_message:?}");
                if let Err(e) = channels.network.send(NetworkMsg::PublishProposalPart(stream_message)).await {
                    error!("Failed to restream proposal part: {}", e);
                    break;
                }
            }
        }
        Ok(None) => {
            warn!(
                %height,
                %proposal_round,
                %value_id,
                "Could not find proposal to restream (may have been pruned)"
            );
        }
        Err(e) => {
            error!("Failed to access store for restream: {}", e);
        }
    }
}
```

#### New Dependencies

**Need to add**:
1. `get_undecided(height, round)` method to BlobEngine trait
2. `BlobsBundle::from_sidecars()` helper method
3. Modify `stream_proposal()` to accept optional `original_proposer: Address`

---

## Additional Missing Features

### 4. Blob Request/Response RPC (Nice to Have)

**Priority**: Medium (P2)
**Use Case**: Partial blob recovery, optimized sync

Currently, there's no way to request specific blobs:
- No `request_blobs(height, round, indices: Vec<u8>)` RPC
- No `BlobsByRange` request (Ethereum has this)
- No `BlobsByRoot` request (Ethereum has this)

**Lighthouse Reference**:
- `blobs_by_range.rs` - Request blobs for a range of heights
- `blobs_by_root.rs` - Request specific blobs by versioned hash

---

## Impact Assessment

### Current State (Without Fixes)

| Scenario | Works? | Impact |
|----------|--------|--------|
| Live consensus with blobs | ✅ Yes | None - fully working |
| Validator joins at genesis | ✅ Yes | No sync needed |
| Validator falls behind 1 block with blobs | ❌ No | **CRITICAL - Cannot sync** |
| New validator joins network | ❌ No | **CRITICAL - Cannot sync** |
| Validator misses blob gossip | ❌ No | **HIGH - Timeouts** |
| Network with 100% uptime | ✅ Yes | Works if no one ever falls behind |

### With Fixes

| Scenario | Works? | Impact |
|----------|--------|--------|
| Live consensus with blobs | ✅ Yes | None - already working |
| Validator falls behind | ✅ Yes | Can sync via ProcessSyncedValue |
| New validator joins | ✅ Yes | Can sync full history |
| Validator misses gossip | ✅ Yes | Can request restream |
| Network partition | ⚠️ Partial | May need blob RPC for robustness |

---

## Recommended Implementation Order

### Phase 1: Critical Fixes (1-2 days)

**Blocker for**: Any real-world usage

1. **Fix ProcessSyncedValue** (1 day)
   - Add blob request mechanism
   - Store blobs before consensus
   - Test with lagging peer scenario

2. **Fix GetDecidedValue** (4 hours)
   - Include blobs in response
   - Test peer-to-peer sync

### Phase 2: High Priority (4 hours)

**Blocker for**: Reliable network operation

3. **Implement RestreamProposal** (4 hours)
   - Retrieve blobs from blob_engine
   - Re-stream complete proposal
   - Test with missed gossip scenario

### Phase 3: Optimization (Optional, 1 day)

**Nice to have**: Better performance

4. **Add Blob RPC Methods** (1 day)
   - `GetBlobsByHeight(height, round)`
   - `GetBlobsByRange(start_height, end_height)`
   - Optimize bandwidth for large syncs

---

## Testing Requirements

### Unit Tests Needed

1. **Blob Sync Test**
   ```rust
   #[tokio::test]
   async fn test_process_synced_value_with_blobs() {
       // Setup peer at height 100, network at height 110
       // Simulate ProcessSyncedValue(101) with blobs
       // Verify blobs are stored and marked decided
       // Verify block can be imported
   }
   ```

2. **GetDecidedValue Test**
   ```rust
   #[tokio::test]
   async fn test_get_decided_value_includes_blobs() {
       // Commit block with blobs at height 50
       // Request GetDecidedValue(50)
       // Verify blobs are included in response
   }
   ```

3. **RestreamProposal Test**
   ```rust
   #[tokio::test]
   async fn test_restream_proposal_with_blobs() {
       // Store proposal with blobs
       // Simulate RestreamProposal request
       // Verify all parts (including blobs) are re-streamed
   }
   ```

### Integration Tests Needed

1. **Lagging Peer Sync**
   - Start 4-node network
   - Stop node 3 at height 10
   - Let network advance to height 20 with blobs
   - Restart node 3
   - Verify node 3 syncs successfully

2. **Network Partition**
   - Split network into 2 partitions
   - Each partition produces blocks
   - Rejoin network
   - Verify sync resolves correctly

---

## References

### Ethereum Implementation

Lighthouse has these features we're missing:

1. **Sync Blobs**
   - `lighthouse/beacon_node/network/src/sync/network_context/requests/blobs_by_range.rs`
   - `lighthouse/beacon_node/network/src/sync/network_context/requests/blobs_by_root.rs`

2. **Blob Availability**
   - `lighthouse/beacon_node/beacon_chain/src/data_availability_checker.rs`
   - Ensures blobs are available before import

3. **RPC Methods**
   - `BlobsByRangeRequest` - Request blobs for range of slots
   - `BlobsByRootRequest` - Request specific blobs by hash
   - Used during sync and recovery

---

## Conclusion

**Summary**: Phase 5 is 100% complete for **live consensus**, but has **critical gaps for state synchronization**.

**Must Fix**:
1. ❌ ProcessSyncedValue - No blob retrieval/storage
2. ❌ GetDecidedValue - No blob data in response
3. ❌ RestreamProposal - Completely unimplemented

**Risk Level**: 🚨 **CRITICAL**

**Recommendation**: **Do NOT deploy to production** until sync is fixed. Network will become non-functional as soon as any peer falls behind.

**Estimated Fix Time**: 1-2 days for critical fixes (ProcessSyncedValue + GetDecidedValue)

---

**Next Steps**:
1. Implement blob sync in ProcessSyncedValue
2. Extend GetDecidedValue to include blobs
3. Implement RestreamProposal
4. Write integration tests for sync scenarios
5. Test with multi-node network with simulated failures

---

**Report By**: Claude (Anthropic AI Assistant)
**Date**: 2025-10-21
**Session**: Blob Sync Gap Analysis
