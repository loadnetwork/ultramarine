# Ultramarine State Sync - End-to-End Trace (Pre-V0)

**Version**: Pre-V0 (Minimal Working Implementation)
**Date**: 2025-10-22
**Status**: Implementation Analysis - Current System
**Author**: Technical Analysis of Existing Code

---

## Executive Summary

This document provides a comprehensive end-to-end trace of Ultramarine's current state synchronization implementation (Pre-V0). The implementation is a **minimal working version** that:

- ✅ **Works correctly** for the happy path
- ✅ **Properly stores all data** (payload + blobs) before consensus decides
- ✅ **Validates blob integrity** via KZG proofs
- ⚠️ **Lacks some production hardening** (peer scoring, bandwidth limits)

**Key Finding**: The current implementation is **sound and production-ready** for the pre-v0 phase. All critical invariants hold, and the architecture supports future v0 upgrades.

---

## Table of Contents

1. [Overview](#overview)
2. [Data Flow Architecture](#data-flow-architecture)
3. [Server Behavior (GetDecidedValue)](#1-server-behavior-helping-node---getdecidedvalue)
4. [Client Behavior (ProcessSyncedValue)](#2-client-behavior-lagging-node---processsyncedvalue)
5. [Consensus Decided Handler](#3-consensus-decided-handler-synced-values)
6. [Invariants](#4-invariants)
7. [Potential Issues](#5-potential-issues)
8. [Correctness Verification](#6-correctness-verification)
9. [Comparison with Design Doc](#7-comparison-with-design-doc)
10. [Summary](#8-summary)

---

## Overview

### Current Implementation: Pre-V0

- **No pruning** yet - all data stored indefinitely
- **Always sends Full packages** - execution payload + blobs
- **MetadataOnly exists as fallback** but should never be used in pre-v0

### Key Components

- **`SyncedValuePackage`**: Enum wrapper for sync data (Full vs MetadataOnly)
- **`RawDecidedValue`**: Malachite's sync response format
- **Blob Engine**: SQLite-based blob storage with status tracking
- **Consensus Store**: RocksDB-based storage for proposals and block data

---

## Data Flow Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    SYNC REQUEST/RESPONSE FLOW                    │
└─────────────────────────────────────────────────────────────────┘

CLIENT (Lagging Node)                  SERVER (Helping Node)
     │                                        │
     │  1. Malachite detects lag             │
     │     (peer height > my height)         │
     │                                        │
     │  2. ValueRequest(height)               │
     ├───────────────────────────────────────>│
     │                                        │
     │                          3. AppMsg::GetDecidedValue
     │                             ├─ get_decided_value(height)
     │                             ├─ get_block_data(height, round)
     │                             ├─ blob_engine.get_for_import(height)
     │                             │
     │                          4. Build SyncedValuePackage::Full {
     │                                value,
     │                                execution_payload_ssz,
     │                                blob_sidecars
     │                             }
     │                             │
     │                          5. Encode to bytes
     │                             │
     │                          6. Return RawDecidedValue {
     │  7. ValueResponse                certificate,
     │<───────────────────────────────────┤  value_bytes
     │                                        │
     │  8. AppMsg::ProcessSyncedValue         │
     │     ├─ Decode SyncedValuePackage       │
     │     ├─ store_synced_block_data()       │
     │     ├─ blob_engine.verify_and_store()  │
     │     ├─ blob_engine.mark_decided()      │
     │     └─ store_synced_proposal()         │
     │                                        │
     │  9. Consensus decides on synced value  │
     │                                        │
     │  10. AppMsg::Decided                   │
     │      ├─ get_block_data() ✅ Found!     │
     │      ├─ blob_engine.get_for_import() ✅│
     │      ├─ Import to EL                   │
     │      └─ commit()                       │
```

---

## 1. SERVER BEHAVIOR (Helping Node - GetDecidedValue)

### Entry Point: `AppMsg::GetDecidedValue` (app.rs:764-845)

### Step 1: Retrieve Decided Value

**Location**: app.rs:767

```rust
let decided_value = state.get_decided_value(height).await;
```

**Retrieves from store**:
- `DecidedValue { value: Value, certificate: CommitCertificate }`
- Returns `None` if height not decided yet

**Database operation**:
- `store.rs:430-437` → `db.get_decided_value(height)`
- Reads from RocksDB column family: `DECIDED_VALUES`

---

### Step 2: Get Certificate Round

**Location**: app.rs:772

```rust
let round = decided_value.certificate.round;
```

**Critical**: This round tells us which proposal data to retrieve (multiple proposals can exist per height if consensus retried).

---

### Step 3: Retrieve Execution Payload Bytes

**Location**: app.rs:775

```rust
let payload_bytes = state.get_block_data(height, round).await;
```

**Returns**: `Option<Bytes>` containing the SSZ-encoded `ExecutionPayloadV3`

**Database operation**:
- `store.rs:471-478` → `db.get_block_data(height, round)`
- Reads from RocksDB column family: `UNDECIDED_BLOCK_DATA`
- Key format: `{height}_{round}`

**Storage source**: This data was stored during:
- **Proposer path**: `AppMsg::GetValue` → `store_undecided_proposal_data()` (app.rs:137)
- **Receiver path**: `AppMsg::ReceivedProposalPart` → internal streaming assembly
- **Sync path**: `AppMsg::ProcessSyncedValue` → `store_synced_block_data()` (app.rs:619)

---

### Step 4: Retrieve Blob Sidecars

**Location**: app.rs:778

```rust
let blob_sidecars_result = state.blob_engine().get_for_import(height).await;
```

**Returns**: `Result<Vec<BlobSidecar>, BlobEngineError>`

**Each BlobSidecar contains**:
- `index: u8` (0-5, max 6 blobs per block)
- `blob: Blob` (131,072 bytes)
- `kzg_commitment: KzgCommitment` (48 bytes)
- `kzg_proof: KzgProof` (48 bytes)

**Blob Engine operation**:
- Reads from SQLite database
- **Critical query**: `WHERE height = ? AND status = 'DECIDED'`
- Returns blobs only if status is `DECIDED` (not `UNDECIDED`)

**Storage source**: Blobs were stored and marked decided during:
- **Proposer path**: `AppMsg::GetValue` → `received_proposal_part()` → streaming
- **Receiver path**: `AppMsg::ReceivedProposalPart` → streaming assembly
- **Sync path**: `AppMsg::ProcessSyncedValue` → `verify_and_store()` + `mark_decided()`

---

### Step 5: Build SyncedValuePackage

**Location**: app.rs:781-822

```rust
let package = match (payload_bytes, blob_sidecars_result) {
    (Some(payload), Ok(blobs)) if !blobs.is_empty() => {
        // Full package with blobs
        SyncedValuePackage::Full {
            value: decided_value.value.clone(),
            execution_payload_ssz: payload,
            blob_sidecars: blobs,
        }
    }
    (Some(payload), Ok(_blobs)) => {
        // Full package without blobs (block has no blobs)
        SyncedValuePackage::Full {
            value: decided_value.value.clone(),
            execution_payload_ssz: payload,
            blob_sidecars: vec![],
        }
    }
    _ => {
        // FALLBACK: MetadataOnly
        // This should NEVER happen in pre-v0!
        error!("Payload or blobs missing...");
        SyncedValuePackage::MetadataOnly {
            value: decided_value.value.clone(),
        }
    }
};
```

**Package sizes**:
- **Full (no blobs)**: ~2KB (value) + ~5-100KB (payload) = ~7-102KB
- **Full (3 blobs)**: ~2KB (value) + ~50KB (payload) + 3×131KB (blobs) = ~445KB
- **MetadataOnly**: ~2KB (value only)

---

### Step 6: Encode Package

**Location**: app.rs:825

```rust
let value_bytes = package.encode()?;
```

**Serialization**: Uses `bincode` (efficient binary format)
- `sync.rs:209-213` → `bincode::serialize(self)`
- Entire `SyncedValuePackage` enum serialized to `Bytes`

---

### Step 7: Return RawDecidedValue

**Location**: app.rs:826-829

```rust
Some(RawDecidedValue {
    certificate: decided_value.certificate,
    value_bytes,
})
```

**Sent to Malachite**: Via `reply.send()`
- Malachite includes this in `ValueResponse`
- Sent to requesting peer over network

---

## 2. CLIENT BEHAVIOR (Lagging Node - ProcessSyncedValue)

### Entry Point: `AppMsg::ProcessSyncedValue` (app.rs:593-755)

### Step 1: Decode SyncedValuePackage

**Location**: app.rs:597

```rust
let package = SyncedValuePackage::decode(&value_bytes)?;
```

**Deserialization**: `bincode::deserialize()`
- `sync.rs:231-234`
- Returns enum: `Full { ... }` or `MetadataOnly { ... }`

**Error handling**: If decode fails:

```rust
Err(e) => {
    error!("Failed to decode SyncedValuePackage: {}", e);
    continue; // Skip this message entirely
}
```

---

### Step 2: Match Package Variant

**Location**: app.rs:607

```rust
match package {
    SyncedValuePackage::Full { value, execution_payload_ssz, blob_sidecars } => { ... }
    SyncedValuePackage::MetadataOnly { value } => { ... }
}
```

---

### 2A. Processing Full Package (app.rs:608-673)

#### Step 2A.1: Store Execution Payload

**Location**: app.rs:618-624

```rust
state.store_synced_block_data(height, round, execution_payload_ssz).await?
```

**Storage path**:
- `state.rs:252-262` → `store.store_undecided_block_data()`
- `store.rs:480-489` → `db.insert_undecided_block_data(height, round, data)`
- **Database**: RocksDB column `UNDECIDED_BLOCK_DATA`
- **Key**: `{height}_{round}`
- **Value**: Raw bytes (SSZ-encoded `ExecutionPayloadV3`)

**Invariant**: This data MUST be stored before `Decided` handler runs, otherwise:

```rust
// app.rs:425 - Decided handler
let Some(block_bytes) = state.get_block_data(height, round).await else {
    return Err(eyre!("Missing block bytes...")); // ❌ CRASH
};
```

---

#### Step 2A.2: Verify and Store Blobs

**Location**: app.rs:627-646

```rust
if !blob_sidecars.is_empty() {
    let round_i64 = round.as_i64();

    // Verify KZG proofs
    state.blob_engine()
        .verify_and_store(height, round_i64, &blob_sidecars)
        .await?;

    // Mark as DECIDED immediately
    state.blob_engine().mark_decided(height, round_i64).await?;
}
```

**Blob Engine operations**:

1. **`verify_and_store()`**:
   - Validates KZG proofs cryptographically
   - Inserts into SQLite with status = `UNDECIDED`
   - If verification fails → returns error, sync aborted

2. **`mark_decided()`**:
   - Updates status: `UNDECIDED` → `DECIDED`
   - **Critical**: Synced values are already decided by consensus at another node
   - Skips the `UNDECIDED` state that normal proposals go through

**Why mark_decided immediately?**
- During normal flow: blobs go `UNDECIDED` → wait for consensus → `DECIDED`
- During sync: consensus already decided at peer, we're just catching up
- `Decided` handler will call `get_for_import()` which requires `DECIDED` status

---

#### Step 2A.3: Build ProposedValue

**Location**: app.rs:649-656

```rust
let proposed_value = ProposedValue {
    height,
    round,
    valid_round: Round::Nil,
    proposer,
    value, // From the Full package
    validity: Validity::Valid,
};
```

**Field meanings**:
- `height`, `round`: From sync request
- `valid_round: Round::Nil`: Synced values have no POL round
- `proposer`: Original proposer address (from certificate)
- `value`: The `Value` metadata (execution header + blob commitments)
- `validity: Valid`: We trust the peer's decided value

---

#### Step 2A.4: Store Synced Proposal 🚨 CRITICAL

**Location**: app.rs:661-664

```rust
state.store_synced_proposal(proposed_value.clone()).await?;
```

**Why this is CRITICAL**:

The `Decided` handler will later call:

```rust
// state.rs:292-331 - commit()
let proposal = self.store
    .get_undecided_proposal(height, round, value_id)
    .await?
    .ok_or_else(|| eyre!("Trying to commit a value that is not decided"))?;
    // ^^^^^^^ If proposal not in store → CRASH
```

**Storage path**:
- `state.rs:270-278` → `store.store_undecided_proposal()`
- `store.rs:450-463` → `db.insert_undecided_proposal()`
- **Database**: RocksDB column `UNDECIDED_PROPOSALS`
- **Key**: `{height}_{round}_{value_id}`

**Invariant violated if skipped**:

```
ProcessSyncedValue → reply.send(proposal)
     ↓
Consensus decides
     ↓
AppMsg::Decided → state.commit()
     ↓
get_undecided_proposal() → None ❌
     ↓
ERROR: "Trying to commit a value that is not decided"
```

---

#### Step 2A.5: Reply to Consensus

**Location**: app.rs:668-672

```rust
if reply.send(proposed_value).is_err() {
    error!("Failed to send ProcessSyncedValue success reply");
} else {
    info!("✅ Successfully processed Full sync package");
}
```

**Sent to Malachite**: `ProposedValue`
- Consensus will validate signatures on certificate
- Will eventually send `AppMsg::Decided` for this height

---

### 2B. Processing MetadataOnly Package (app.rs:675-701)

```rust
SyncedValuePackage::MetadataOnly { value: _value } => {
    // 🔴 CRITICAL ERROR in pre-v0!
    error!("Received MetadataOnly sync package in pre-v0! \
            This should NEVER happen (no pruning yet).");

    // Do NOT store proposal
    // Do NOT reply to consensus
    continue; // Skip this synced value
}
```

**Why this is an error**:
- Pre-v0 has **no pruning** → all data always available
- If MetadataOnly received, it means:
  1. Peer is buggy/malicious, OR
  2. `GetDecidedValue` implementation is broken

**Why we can't proceed**:
- No execution payload bytes stored
- `Decided` handler will call `get_block_data()` → returns `None`
- Node crashes with "Missing block bytes for decided value"

**Better to fail fast** at sync than crash later at commit.

---

## 3. CONSENSUS DECIDED HANDLER (Synced Values)

After sync completes, consensus eventually decides:

### Entry Point: `AppMsg::Decided` (app.rs:417-582)

### Step 3.1: Retrieve Block Data

**Location**: app.rs:425-433

```rust
let Some(block_bytes) = state.get_block_data(height, round).await else {
    return Err(eyre!("Missing block bytes for decided value..."));
};
```

**For synced values**: This data was stored in `ProcessSyncedValue` step 2A.1

---

### Step 3.2: Retrieve Blobs

**Location**: app.rs:486-506

```rust
if !versioned_hashes.is_empty() {
    let blobs = state.blob_engine().get_for_import(height).await?;

    // Verify blob count
    if blobs.len() != versioned_hashes.len() {
        return Err(eyre!("Blob count mismatch..."));
    }
}
```

**For synced values**: Blobs were stored and marked `DECIDED` in step 2A.2

**Critical check**: Blob count must match versioned hashes in payload

---

### Step 3.3: Verify Versioned Hashes

**Location**: app.rs:511-531

```rust
let computed_hashes: Vec<BlockHash> = blobs.iter()
    .map(|sidecar| {
        let mut hash = Sha256::digest(sidecar.kzg_commitment.as_bytes());
        hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
        BlockHash::from_slice(&hash)
    })
    .collect();

if computed_hashes != versioned_hashes {
    return Err(eyre!("Versioned hash mismatch..."));
}
```

**Defense-in-depth**: Recompute hashes from stored commitments, verify they match payload

---

### Step 3.4: Import to Execution Layer

**Location**: app.rs:540-541

```rust
let payload_status = execution_layer
    .notify_new_block(execution_payload, versioned_hashes)
    .await?;
```

**EL import**: `engine_newPayloadV3(payload, versioned_hashes, parent_beacon_block_root)`
- EL validates and executes the block
- Returns status: `VALID`, `INVALID`, or `SYNCING`

---

### Step 3.5: Update Forkchoice

**Location**: app.rs:550-555

```rust
let latest_valid_hash = execution_layer
    .set_latest_forkchoice_state(new_block_hash)
    .await?;
```

**Finalizes block**: `engine_forkchoiceUpdatedV3()`
- Marks this block as canonical head
- EL updates its view of the chain

---

### Step 3.6: Commit to Consensus Store

**Location**: app.rs:558

```rust
state.commit(certificate).await?;
```

**Moves data from undecided → decided**:
- `store_decided_value(certificate, value)`
- `store_decided_block_data(height, data)`
- Blob engine updates status (already `DECIDED` for synced values)
- **Prunes old undecided data** (currently disabled, retention = ∞)

---

## 4. INVARIANTS

### I1: Payload Storage Completeness

**Invariant**: If `get_decided_value(h)` returns Some, then `get_block_data(h, r)` MUST return Some

**Where enforced**:
- `GetDecidedValue` (server): Only sends Full if payload exists (app.rs:781-809)
- `ProcessSyncedValue` (client): Stores payload before replying (app.rs:618-624)

**Violation consequence**: Crash in `Decided` handler (app.rs:425-432)

**Current status**: ✅ **HOLDS** - all code paths store payload

---

### I2: Blob Availability

**Invariant**: If `Value` has blob commitments, then `blob_engine.get_for_import(h)` MUST return matching blobs

**Where enforced**:
- `GetDecidedValue` (server): Only sends Full if blobs retrieved successfully (app.rs:782-794)
- `ProcessSyncedValue` (client): Verifies and stores blobs before replying (app.rs:627-646)

**Violation consequence**: Crash in `Decided` handler (app.rs:497-506)

**Current status**: ✅ **HOLDS** - blobs verified and stored

---

### I3: Blob Status Consistency

**Invariant**: When `Decided` handler runs, blobs MUST be in `DECIDED` status

**Where enforced**:
- Normal flow: `ReceivedProposalPart` → stores as `UNDECIDED` → `Decided` handler calls `mark_decided()`
- Sync flow: `ProcessSyncedValue` calls `mark_decided()` immediately (app.rs:642)

**Violation consequence**: `get_for_import()` returns empty (queries `WHERE status = 'DECIDED'`)

**Current status**: ✅ **HOLDS** - sync path marks decided early

---

### I4: Proposal Store Completeness

**Invariant**: Before consensus decides, `get_undecided_proposal(h, r, vid)` MUST return Some

**Where enforced**:
- Normal flow: `ReceivedProposalPart` stores proposal when assembly completes
- Sync flow: `ProcessSyncedValue` stores via `store_synced_proposal()` (app.rs:661)

**Violation consequence**: Crash in `commit()` (state.rs:313)

**Current status**: ✅ **HOLDS** - added in pre-v0 implementation

---

### I5: Value Encoding Consistency

**Invariant**: `SyncedValuePackage.value` MUST match `DecidedValue.value`

**Where enforced**:
- `GetDecidedValue`: Clones from `decided_value.value` (app.rs:791, 805, 819)
- `ProcessSyncedValue`: Uses `value` from package as-is (app.rs:654)

**Violation consequence**: Certificate signature verification fails in consensus

**Current status**: ✅ **HOLDS** - direct cloning, no transformation

---

### I6: Round Consistency

**Invariant**: Execution payload bytes stored under `(height, round)` must match the decided certificate round

**Where enforced**:
- `GetDecidedValue`: Uses `certificate.round` to fetch data (app.rs:772, 775)
- `ProcessSyncedValue`: Stores under received `round` parameter (app.rs:619)

**Violation consequence**: Wrong payload retrieved, EL import fails (parent hash mismatch)

**Current status**: ✅ **HOLDS** - round extracted from certificate

---

### I7: Pre-V0 No MetadataOnly

**Invariant**: In pre-v0, `MetadataOnly` packages MUST NOT be sent (no pruning yet)

**Where enforced**:
- `GetDecidedValue`: Falls back to MetadataOnly only if payload missing (app.rs:810-821)
- `ProcessSyncedValue`: Rejects MetadataOnly with error (app.rs:689-701)

**Violation consequence**: Client skips synced value, remains lagging

**Current status**: ⚠️ **SOFT ENFORCEMENT**
- Server logs error but still sends MetadataOnly (app.rs:813-816)
- Client properly rejects it (app.rs:689)
- Should be impossible if storage is working correctly

---

## 5. POTENTIAL ISSUES

### Issue 1: MetadataOnly Fallback Misleading

**Location**: app.rs:810-821 (GetDecidedValue)

```rust
_ => {
    error!("Payload or blobs missing, sending MetadataOnly...");
    SyncedValuePackage::MetadataOnly { value }
}
```

**Problem**:
- In pre-v0, this should be **IMPOSSIBLE** (no pruning)
- If it happens, indicates storage corruption or implementation bug
- Server still sends it instead of returning `None`

**Better behavior**:
```rust
_ => {
    error!("CRITICAL: Payload missing in pre-v0 (no pruning). Storage corrupted?");
    reply.send(None).unwrap();
    return; // Don't send anything
}
```

**Impact**: Low - client properly rejects it anyway (app.rs:689)

---

### Issue 2: No Peer Scoring on Sync Errors

**Location**: app.rs:597-605 (ProcessSyncedValue decode error)

```rust
Err(e) => {
    error!("Failed to decode SyncedValuePackage: {}", e);
    continue; // Just skip
}
```

**Problem**:
- Malicious/buggy peer sent invalid data
- No penalty, no blacklisting
- Peer can keep sending garbage

**Better behavior**:
```rust
Err(e) => {
    error!("Failed to decode from peer {}: {}", from_peer, e);
    // TODO: Penalize peer in sync scoring
    continue;
}
```

**Impact**: Medium - enables DoS attacks (spam with invalid sync responses)

---

### Issue 3: No Timeout on Blob Verification

**Location**: app.rs:632-638 (ProcessSyncedValue blob verify)

```rust
state.blob_engine()
    .verify_and_store(height, round_i64, &blob_sidecars)
    .await?;
```

**Problem**:
- KZG verification is computationally expensive (~100ms per blob)
- No timeout - malicious peer could send invalid blobs
- Verification will fail, but wastes CPU

**Better behavior**:
```rust
tokio::time::timeout(
    Duration::from_secs(5),
    state.blob_engine().verify_and_store(...)
).await??;
```

**Impact**: Low - verification is fast enough in practice

---

### Issue 4: Duplicate Sync Handling

**Scenario**: Two peers both send synced value for same height

```
Peer A sends Full{height=100}
    → ProcessSyncedValue stores payload, blobs, proposal
Peer B sends Full{height=100}
    → ProcessSyncedValue stores again (overwrites)
```

**Current behavior**:
- RocksDB `insert_undecided_block_data()` overwrites silently
- BlobEngine `verify_and_store()` checks if blobs exist, skips insert
- Proposal store overwrites

**Impact**: None - data is identical (same decided value)

**Potential improvement**: Detect duplicate and skip processing:
```rust
if state.has_synced_value(height) {
    debug!("Already have synced value for height {}", height);
    continue;
}
```

---

### Issue 5: No Bandwidth Tracking

**Problem**: No limits on sync response sizes

**Current situation**:
- Full package with 6 blobs = ~800KB
- No rate limiting
- No max_response_size enforcement at app level (Malachite may have limits)

**Potential attack**:
- Requester spams sync requests for recent heights (all have blobs)
- Server sends ~800KB per response
- Bandwidth exhaustion

**Mitigation needed**: Track bytes sent per peer per time window

---

## 6. CORRECTNESS VERIFICATION

### ✅ Happy Path: Full Sync (with blobs)

```
Height 100 decided at Server, Client is at height 99

1. Server: GetDecidedValue(100)
   ├─ get_decided_value(100) → Some(decided_value)
   ├─ certificate.round = 0
   ├─ get_block_data(100, 0) → Some(payload_bytes)
   ├─ blob_engine.get_for_import(100) → Ok([blob0, blob1, blob2])
   ├─ Build Full package
   ├─ Encode to bytes
   └─ Return RawDecidedValue { certificate, value_bytes }

2. Network: ValueResponse sent to Client

3. Client: ProcessSyncedValue(100, 0, proposer, value_bytes)
   ├─ Decode → Full { value, payload, blobs }
   ├─ store_synced_block_data(100, 0, payload) ✅
   ├─ blob_engine.verify_and_store(100, 0, blobs) ✅
   ├─ blob_engine.mark_decided(100, 0) ✅
   ├─ store_synced_proposal(proposal) ✅
   └─ reply.send(proposal) → Consensus

4. Consensus: Validates certificate, decides

5. Client: Decided(certificate{height=100, round=0})
   ├─ get_block_data(100, 0) → Some(payload) ✅
   ├─ Decode ExecutionPayloadV3
   ├─ blob_engine.get_for_import(100) → Ok(blobs) ✅
   ├─ Verify versioned hashes ✅
   ├─ EL.notify_new_block(payload, hashes) ✅
   ├─ EL.set_latest_forkchoice_state() ✅
   └─ commit(certificate) ✅

Result: ✅ Block imported, height 100 finalized
```

---

### ✅ Happy Path: Full Sync (no blobs)

```
Height 50 decided at Server (block has no blobs)

1. Server: GetDecidedValue(50)
   ├─ get_decided_value(50) → Some(decided_value)
   ├─ get_block_data(50, 0) → Some(payload_bytes)
   ├─ blob_engine.get_for_import(50) → Ok([]) (empty)
   ├─ Build Full { value, payload, blob_sidecars: vec![] }
   └─ Return encoded package

2. Client: ProcessSyncedValue(50, 0, ...)
   ├─ Decode → Full { payload, blob_sidecars: [] }
   ├─ store_synced_block_data(50, 0, payload) ✅
   ├─ Skip blob storage (empty vec)
   ├─ store_synced_proposal(proposal) ✅
   └─ reply.send(proposal)

3. Decided(50)
   ├─ get_block_data(50, 0) → Some(payload) ✅
   ├─ versioned_hashes.is_empty() → true
   ├─ Skip blob checks
   ├─ EL.notify_new_block(payload, []) ✅
   └─ commit() ✅

Result: ✅ Block imported successfully
```

---

### 🔴 Error Path: Invalid Sync Data

```
Client receives corrupted value_bytes

1. Client: ProcessSyncedValue(100, 0, ..., corrupted_bytes)
   ├─ SyncedValuePackage::decode(corrupted_bytes)
   └─ Err("Failed to decode") ❌

2. Error handler:
   ├─ error!("Failed to decode SyncedValuePackage")
   └─ continue (skip this message)

Result: Client remains lagging, will retry with another peer
```

---

### 🔴 Error Path: Missing Payload at Server

```
Server's storage corrupted, payload missing

1. Server: GetDecidedValue(100)
   ├─ get_decided_value(100) → Some(decided_value)
   ├─ get_block_data(100, 0) → None ❌
   ├─ blob_engine.get_for_import(100) → Ok(blobs)
   ├─ Match: (None, Ok(blobs))
   ├─ Fall into _ branch
   ├─ error!("Payload missing, sending MetadataOnly")
   └─ Return MetadataOnly package

2. Client: ProcessSyncedValue
   ├─ Decode → MetadataOnly { value }
   ├─ Match MetadataOnly branch
   ├─ error!("CRITICAL: Received MetadataOnly in pre-v0")
   └─ continue (reject this sync)

Result: Sync fails, client tries another peer
```

---

## 7. COMPARISON WITH DESIGN DOC

| Aspect | Design Doc (V0) | Current Implementation (Pre-V0) |
|--------|-----------------|----------------------------------|
| **Archival awareness** | Yes - two modes based on retention | No - always Full |
| **BlobStatus API** | Explicit status tracking | Implicit (get_for_import query) |
| **Pruning** | After retention period | Disabled (retention = ∞) |
| **MetadataOnly mode** | For archived blocks | Error/fallback only |
| **Payload storage** | Temporary (pruned) | Permanent |
| **Blob storage** | Temporary (pruned) | Permanent |
| **Sync message size** | 2KB-500KB (adaptive) | Always ~500KB if blobs exist |
| **Peer scoring** | Planned | Missing |
| **Bandwidth limits** | Planned | Missing |

**Key simplification**: Pre-v0 removes all complexity around archival/pruning, making it straightforward to debug and verify correctness.

---

## 8. SUMMARY

### ✅ What Works

1. **Full sync path is correct**: Payload + blobs properly transferred
2. **Storage invariants hold**: All data stored before `Decided` runs
3. **Blob verification intact**: KZG proofs checked before storage
4. **Versioned hash validation**: Defense-in-depth at import
5. **Critical fix applied**: `store_synced_proposal()` prevents commit crash

---

### ⚠️ What Could Improve

1. **Peer scoring**: Penalize peers sending invalid data
2. **Bandwidth limits**: Prevent sync-based DoS attacks
3. **Deduplication**: Skip redundant sync processing
4. **MetadataOnly handling**: Return `None` instead of sending fallback
5. **Timeout on blob verification**: Bound CPU usage per sync

---

### 🎯 Readiness for V0

The current implementation provides a **solid foundation** for v0:
- All core sync paths working correctly
- Invariants properly enforced
- Storage architecture supports future pruning (data is already separated: decided vs undecided)
- Clean separation between Value metadata and execution payload

**Next steps for V0**:
1. Add `BlobStatus` API to blob engine
2. Implement archival service (mark blobs as archived)
3. Enable pruning in `commit()` based on retention period
4. Update `GetDecidedValue` to check archival status before choosing mode
5. Implement peer scoring and bandwidth tracking

---

## References

### Related Documentation
- `docs/SYNCING_IMPLEMENTATION_V0.md` - Proposed v0 design with archival awareness
- `docs/BLOB_SYNC_GAP_ANALYSIS.md` - Original gap analysis (if exists)
- `docs/PHASE_5_COMPLETION.md` - Blob integration status (if exists)

### Code Locations
- **Sync protocol**: `ultramarine/crates/node/src/app.rs` (lines 593-845)
- **Sync types**: `ultramarine/crates/types/src/sync.rs`
- **State management**: `ultramarine/crates/consensus/src/state.rs`
- **Store operations**: `ultramarine/crates/consensus/src/store.rs`
- **Blob engine**: `ultramarine/crates/blob_engine/`

---

**End of Document**
