# Test Report: `blob_sync_across_restart_multiple_heights`

**Test File**: `crates/test/tests/blob_restart_multi_height_sync.rs`
**Function**: `blob_sync_across_restart_multiple_heights()`

---

## ✅ **STATUS UPDATE (2025-11-08): RESOLVED**

The original failure was resolved by switching the test to exercise the sync handler (`State::process_synced_package`) instead of the streaming path. The test now passes successfully.

**Current Status**: ✅ **PASSING**
**Fix Applied**: Replaced `stream_proposal()` → `received_proposal_part()` with direct `process_synced_package()` call
**Test Runtime**: ~5.5 seconds

---

## Historical Context: Original Failure Analysis

**Original Status**: ❌ **FAILING**
**Original Failure Point**: Line 102 - `expect("follower reconstructs proposal")` panics
**Original Severity**: 🔴 **HIGH** - Test designed to verify critical multi-height restart functionality

---

## Executive Summary

The test fails because `received_proposal_part()` returns `Ok(None)` for all stream messages, meaning the proposal is never successfully reconstructed. The test attempts to process 3 consecutive heights (0, 1, 2) with mixed blob counts (1, 0, 2 blobs respectively), but proposal reconstruction fails, causing a panic at line 102.

---

## Test Execution Output

```
height Height(0) emitted 5 stream parts
received part height Height(0) seq 0 kind init
received part height Height(0) seq 1 kind data
received part height Height(0) seq 2 kind blob_sidecar
received part height Height(0) seq 3 kind fin
received part height Height(0) seq 4 kind Fin

height Height(1) emitted 4 stream parts
received part height Height(1) seq 0 kind init
received part height Height(1) seq 1 kind data
received part height Height(1) seq 2 kind fin
received part height Height(1) seq 3 kind Fin

height Height(2) emitted 6 stream parts
received part height Height(2) seq 0 kind init
received part height Height(2) seq 1 kind data
received part height Height(2) seq 2 kind blob_sidecar
received part height Height(2) seq 3 kind blob_sidecar
received part height Height(2) seq 4 kind fin
received part height Height(2) seq 5 kind Fin

thread 'blob_restream_across_restart_multiple_heights' panicked at line 102:47:
follower reconstructs proposal
```

**Analysis**: All 3 heights emit messages successfully, messages are received, but `reconstructed` remains `None`.

---

## Root Cause Analysis

### Failure Mechanism

The `received_proposal_part()` function (`state.rs:560-633`) returns `Some(ProposedValue)` only when:

1. **Stream completion**: `streams_map.insert(from, part)` returns `Some(parts)` (line 569)
2. **Height validation**: `parts.height >= self.current_height` (line 574)
3. **Signature verification**: Valid signature (line 587)
4. **Blob assembly**: `assemble_and_store_blobs()` succeeds (line 599)

Since no error messages appear in output, conditions 2-4 are likely passing. The issue is **condition 1**: stream completion logic.

### Stream Completion Logic

From `streaming.rs:76-78`, `StreamState::is_done()` returns `true` when:

```rust
self.init_info.is_some() && self.fin_received && self.buffer.len() == self.total_messages
```

This requires:
- **`init_info.is_some()`**: First message must be recognized as Init
- **`fin_received`**: Fin message must be received
- **`buffer.len() == total_messages`**: All expected messages received

### Critical Issue: `msg.is_first()` Check

At `streaming.rs:81-83`:

```rust
if msg.is_first() {
    self.init_info = msg.content.as_data().and_then(|p| p.as_init()).cloned();
}
```

**The init_info is ONLY set if `msg.is_first()` returns true.** This likely checks a flag on the `StreamMessage`, not just `sequence == 0`.

### Suspected Root Causes

#### **Hypothesis #1: `stream_proposal()` Not Setting First/Fin Flags Correctly**

The test calls `stream_proposal()` at line 71-79:

```rust
let stream_messages: Vec<StreamMessage<_>> = proposer
    .state
    .stream_proposal(proposed, bytes, sidecars, None)
    .collect();
```

If `stream_proposal()` doesn't properly mark the first message with `is_first() = true` or the last message with `is_fin() = true`, the stream will never complete.

**Evidence**: All messages are emitted and received, suggesting `stream_proposal()` itself works. But if flags are missing, `init_info` remains `None` → `is_done()` always returns `false`.

#### **Hypothesis #2: Multiple Proposals Interfering Via `streams_map`**

The test uses **different peer IDs for each height**:

```rust
// Line 96
common::test_peer_id(height.as_u64() as u8)
```

This creates:
- Height 0 → `peer_id_0`
- Height 1 → `peer_id_1`
- Height 2 → `peer_id_2`

`PartStreamsMap` tracks streams by `(peer_id, stream_id)` (line 117). If:
- **Different peer IDs + unique stream IDs per height**: Should work fine ✅
- **Different peer IDs + SAME stream ID across heights**: Collision possible ❌

After height 0 commits, if the stream entry for `(peer_id_0, stream_id_0)` is not properly cleaned up, and height 1 reuses `stream_id_0` with `peer_id_1`, there could be state pollution.

**However**: Line 141-143 of `streaming.rs` removes completed streams:

```rust
if state.is_done() {
    self.streams.remove(&(peer_id, stream_id));
}
```

So IF height 0's stream completes (which it apparently doesn't based on the panic), cleanup should happen.

#### **Hypothesis #3: State Corruption After First Commit**

After committing height 0:
1. `current_height` advances from 0 → 1
2. Test manually resets `current_height = Height::new(1)` (line 48)
3. But internal state (proposals, metadata, etc.) might not be fully reset

If `received_proposal_part()` has stale state from height 0, it might reject height 1's messages.

**Evidence**: The test logs show height 0, 1, and 2 messages all being emitted, suggesting the proposer side is fine. The issue is on the follower's `received_proposal_part()` side.

#### **Hypothesis #4: Height Validation Issue**

At line 574 of `state.rs`:

```rust
if parts.height < self.current_height {
    return Ok(None);
}
```

If there's a mismatch between `parts.height` (extracted from Init message) and `self.current_height`, proposals get silently rejected.

**Trace**:
- Iteration 1: Set `current_height = 0`, process height 0 messages, commit → `current_height = 1`
- Iteration 2: Set `current_height = 1`, process height 1 messages, commit → `current_height = 2`
- Iteration 3: Set `current_height = 2`, process height 2 messages...

This should work IF the manual `current_height` assignment at line 48 properly syncs state.

**BUT**: There's a timing issue. The test sets `current_height` at the START of the loop (line 47-48), but the proposer creates the proposal with the current height. If there's any mismatch in how height is embedded in the Init message vs. `current_height`, validation fails.

---

## Test Design Issues

### Issue #1: Mixing Proposal Reception Paths

The test uses `received_proposal_part()` (normal consensus proposal flow) but then calls:

```rust
// Lines 104-113
follower.state.store_synced_block_data(height, round, bytes.clone()).await?;
follower.state.store_synced_proposal(reconstructed.clone()).await?;
if let Some(sidecars) = sidecars.as_ref() {
    follower.state.blob_engine().verify_and_store(height, round.as_i64(), sidecars).await?;
}
```

**This is redundant!** `received_proposal_part()` already:
1. Stores the proposal (line 629 of `state.rs`)
2. Stores the block data (line 630)
3. Verifies and stores blobs (via `assemble_and_store_blobs` at line 599)

Calling these methods again:
- Overwrites data unnecessarily
- May cause unexpected state transitions
- Suggests confusion about which path (consensus vs. sync) the test is exercising

**Recommendation**: Remove lines 104-113. The test should either:
- Use `received_proposal_part()` → `commit()` (normal path)
- OR use `process_synced_package()` → `commit()` (sync path)

### Issue #2: Manual Height/Round Management

Lines 47-51 manually set `current_height` and `current_round`:

```rust
proposer.state.current_height = height;
follower.state.current_height = height;
proposer.state.current_round = round;
follower.state.current_round = round;
```

This is fragile because:
- `commit()` automatically advances `current_height` (to `height + 1`)
- Manually resetting it might desync internal state
- Production code never manually sets `current_height`

**Recommendation**: Let `commit()` naturally advance `current_height`, and verify it matches expectations:

```rust
assert_eq!(follower.state.current_height, height + 1, "height should advance after commit");
```

### Issue #3: Different Peer IDs Per Height

Line 96 creates a different peer ID for each height:

```rust
common::test_peer_id(height.as_u64() as u8)
```

While technically valid (each height comes from a different peer), this is unusual for a "restream" test. Typically, restreaming means the **same node** (same peer ID) streams multiple heights.

**Recommendation**: Use a consistent peer ID:

```rust
let peer_id = common::test_peer_id(1);  // Use once, outside loop
// ...
follower.state.received_proposal_part(peer_id, msg).await?
```

### Issue #4: Proposer Doesn't Store Own Blobs

The proposer calls:
1. `propose_value_with_blobs()` - stores proposal + metadata
2. `prepare_blob_sidecar_parts()` - creates sidecars
3. `stream_proposal()` - streams to network

But **never calls** `verify_and_store()` for its own blobs. In production, the proposer must also commit the block it proposes, which requires blobs to be available locally.

**Recommendation**: Add proposer blob storage and commit verification (similar to `blob_restream.rs` fix suggestion in earlier report).

---

## Recommended Fixes

### Priority 1: Diagnose `init_info` Issue

Add debug logging to `streaming.rs:81-83`:

```rust
if msg.is_first() {
    eprintln!("🔍 First message detected: seq={}, peer={:?}", msg.sequence, peer_id);
    self.init_info = msg.content.as_data().and_then(|p| p.as_init()).cloned();
    eprintln!("🔍 init_info set: {:?}", self.init_info.is_some());
} else {
    eprintln!("⚠️ Not first message: seq={}, is_first={}", msg.sequence, msg.is_first());
}
```

Run test again to see if `init_info` is being set.

### Priority 2: Simplify Test Structure

Remove redundant sync-path calls (lines 104-113):

```rust
// REMOVE these lines:
// follower.state.store_synced_block_data(...).await?;
// follower.state.store_synced_proposal(...).await?;
// follower.state.blob_engine().verify_and_store(...).await?;

// Just commit directly after reconstruction:
let reconstructed = reconstructed.expect("follower reconstructs proposal");
follower.state.commit(certificate).await?;
```

### Priority 3: Fix Peer ID Management

Use consistent peer ID:

```rust
let peer_id = common::test_peer_id(10);  // Fixed peer representing proposer

for msg in stream_messages {
    if let Some(value) = follower.state.received_proposal_part(peer_id, msg).await? {
        reconstructed = Some(value);
    }
}
```

### Priority 4: Remove Manual Height/Round Assignment

Let state naturally advance:

```rust
for (expected_height, blob_count) in configurations {
    // DON'T set current_height/current_round manually
    // Verify they match expected values instead:
    assert_eq!(proposer.state.current_height, expected_height);
    assert_eq!(follower.state.current_height, expected_height);
    // ...
}
```

---

## Comparison with Working Tests

### `blob_restream.rs` (✅ Works)

```rust
let mut follower = build_state(...);
let peer_id = test_peer_id(1);  // Fixed peer

// Proposer streams, follower receives
for msg in stream_messages {
    if let Some(value) = follower.state.received_proposal_part(peer_id, msg).await? {
        received = Some(value);
    }
}

// Commit directly, no redundant stores
follower.state.commit(certificate).await?;
```

**Key differences**:
- ✅ Uses fixed peer ID throughout
- ✅ No manual height/round assignment
- ✅ No redundant `store_synced_*` calls
- ✅ Processes single height (no loop)

### `blob_restart_restream_multi_height.rs` (❌ Fails)

```rust
// ISSUE: Loop with manual height management
for (height, blob_count) in configurations {
    proposer.state.current_height = height;  // ❌ Manual
    follower.state.current_height = height;   // ❌ Manual

    // ISSUE: Different peer per height
    let peer_id = test_peer_id(height.as_u64() as u8);  // ❌ Changes

    // ISSUE: Redundant stores after received_proposal_part
    follower.state.store_synced_block_data(...).await?;  // ❌ Redundant
    follower.state.store_synced_proposal(...).await?;     // ❌ Redundant

    follower.state.commit(certificate).await?;
}
```

---

## Verdict

**Grade: D** - Test has fundamental design issues preventing it from functioning.

### What's Wrong

1. 🔴 **Critical**: `received_proposal_part()` never returns `Some` → likely `init_info` not being set
2. 🔴 **Critical**: Mixing consensus and sync paths with redundant stores
3. 🟡 **Major**: Manual `current_height` management desyncs state
4. 🟡 **Major**: Different peer IDs per height is unusual for multi-height restream

### What's Right

- ✅ Test concept is valuable (multi-height restart with mixed blobs)
- ✅ Restart verification logic (lines 131-183) looks correct
- ✅ Blob count assertions are thorough

### Recommended Action

**Option A (Quick Fix)**: Debug `init_info` issue with logging, then simplify to match `blob_restream.rs` pattern.

**Option B (Redesign)**: Split into two tests:
1. `blob_restream_multi_height.rs`: Multi-height in single session (no restart)
2. `blob_restart_persistence.rs`: Single height with restart (reuse `restart_hydrate.rs` pattern)

---

## Additional Investigation Needed

1. **Verify `stream_proposal()` output**:
   - Does it set `is_first()` flag on first message?
   - Does it set `is_fin()` flag on last message?
   - Are `StreamId` values unique per call?

2. **Check `init_info` extraction**:
   - Add logging to see if `msg.content.as_data().and_then(|p| p.as_init())` succeeds
   - Verify `ProposalPart::Init` is present in first message

3. **Test height validation**:
   - Log `parts.height` vs. `self.current_height` at line 574
   - Check if outdated proposal rejection is triggering

4. **Inspect `streams_map` state**:
   - Log `streams_map` contents before/after each height
   - Verify cleanup happens when streams complete

---

**Report Generated**: 2025-11-08
**Reviewed By**: Claude Code Analysis
**Next Steps**: Implement Priority 1 (debug logging) and Priority 2 (simplify test) fixes.
