# Syncing Implementation v0 - Technical Documentation

**Version**: 0.2 (Updated)
**Date**: 2025-10-22
**Status**: Design Document - Not Yet Implemented
**Author**: Claude (based on codebase analysis and architectural discussion)

---

## Executive Summary

This document describes the implementation plan for state synchronization in Ultramarine with **archival-aware blob handling**. The design recognizes that blobs are **temporary data** (pruned after retention period) and implements a two-mode sync protocol:

1. **Full Sync**: For recent blocks (within retention) - includes execution payload + blobs
2. **Metadata-Only Sync**: For archived blocks (beyond retention) - includes only consensus metadata

**Key Architectural Principles**:
- ✅ **Blobs are temporary by design** (EIP-4844 philosophy: cheap DA through time-limited availability)
- ✅ **CL stores consensus metadata forever** (lightweight: ~2KB per block)
- ✅ **CL stores execution payloads + blobs temporarily** (pruned after retention period)
- ✅ **EL syncs independently** (full blockchain history via devp2p)
- ✅ **New nodes can start from genesis** (get metadata for old blocks, full data for recent blocks)

**Critical Finding**: State sync is currently broken because only Value metadata is transferred, not the complete block data (execution payload + blobs).

---

## Background

### System Architecture

```
┌─────────────────────────────────────────┐
│  CL (Consensus Layer) - Ultramarine     │
│  ├─ Malachite consensus                 │
│  ├─ Consensus metadata (forever)        │
│  ├─ Execution payloads (temporary)      │
│  └─ Blobs (temporary, retention period) │
└─────────────────────────────────────────┘
                 ↓ Engine API
┌─────────────────────────────────────────┐
│  EL (Execution Layer) - Reth            │
│  ├─ Full blockchain (blocks, state)     │
│  ├─ Syncs independently via devp2p      │
│  └─ Source of truth for execution ✅    │
└─────────────────────────────────────────┘
```

### Blob Lifecycle

```
Block Height Timeline:
├─ Height 1,000,000 (old, archived)
│  ├─ CL: Has Value metadata ✅ (forever)
│  ├─ CL: NO execution payload ❌ (pruned)
│  ├─ CL: NO blobs ❌ (pruned)
│  └─ EL: Has full block ✅ (synced via devp2p)
│
├─ Height 3,640,000 (retention boundary)
│  └─ Blobs older than this are ARCHIVED
│
└─ Height 3,650,000 (recent, available)
   ├─ CL: Has Value metadata ✅
   ├─ CL: Has execution payload ✅ (temporary)
   ├─ CL: Has blobs ✅ (temporary)
   └─ EL: Will receive when finalized ✅
```

**Retention Period** (configurable, e.g., 18 days = ~155,000 blocks):
- **Within retention**: Blobs available, full sync possible
- **Beyond retention**: Blobs archived (pruned), metadata-only sync

### Two Separate Data Paths

#### Path 1: Live Consensus (Working ✅)

When a validator proposes a new value:

```
Proposer
  ↓ GetValue
  ↓ execution_client.generate_block_with_blobs()
  ↓ ExecutionPayload + BlobsBundle
  ↓
  ↓ stream_proposal()
  ├─ ProposalPart::Init (metadata)
  ├─ ProposalPart::Data (execution payload chunks)
  ├─ ProposalPart::BlobSidecar(0) ← 131KB blob
  ├─ ProposalPart::BlobSidecar(1) ← 131KB blob
  ├─ ...
  └─ ProposalPart::Fin (signature)
       ↓
Validators
  ↓ ReceivedProposalPart
  ↓ assemble_and_store_blobs()
  ↓ blob_engine.verify_and_store() ← KZG verification
  ↓ Store as UNDECIDED
  ↓
  ↓ Consensus decides
  ↓ blob_engine.mark_decided()
  ↓ UNDECIDED → DECIDED
  ↓
  ↓ Import to execution layer ✅
```

**Status**: ✅ Fully functional (Phase 1-5 complete)

#### Path 2: State Sync (Broken ❌)

When a node falls behind and needs to catch up:

```
Lagging Peer                    Helping Peer
     |                               |
     | ValueRequest(height 101)      |
     |------------------------------>|
     |                               |
     |                          GetDecidedValue(101)
     |                               ├─ Get Value from store
     |                               ├─ ❌ Missing: execution payload
     |                               └─ ❌ Missing: blobs
     |                               |
     |   ValueResponse               |
     |<------------------------------|
     |   RawDecidedValue {           |
     |     certificate,              |
     |     value_bytes: [metadata]   | ← Only ~2KB metadata!
     |   }                           |
     |                               |
     | ProcessSyncedValue            |
     |  ├─ Decode Value              |
     |  ├─ ❌ No execution payload   |
     |  ├─ ❌ No blobs               |
     |  └─ Store incomplete data     |
     |                               |
     | Consensus decides             |
     |  ↓                            |
     | Decided handler               |
     |  ├─ get_block_data()          |
     |  ├─ ❌ NOT FOUND              |
     |  ├─ get_for_import()          |
     |  └─ ❌ NO BLOBS               |
     |     IMPORT FAILS!             |
```

**Status**: ❌ Broken - Cannot sync blocks

---

## Problem Analysis

### Current Implementation Gaps

#### Gap 1: GetDecidedValue (app.rs:669-683)

**Current Code**:
```rust
AppMsg::GetDecidedValue { height, reply } => {
    let decided_value = state.get_decided_value(height).await;

    let raw_decided_value = decided_value.map(|decided_value| RawDecidedValue {
        certificate: decided_value.certificate,
        value_bytes: ProtobufCodec.encode(&decided_value.value).expect("..."),
        // ❌ MISSING: Execution payload bytes
        // ❌ MISSING: Blob sidecars
        // ❌ MISSING: Archival status awareness
    });

    reply.send(raw_decided_value);
}
```

**Problems**:
1. Only sends Value metadata (~2KB), not full block data
2. Doesn't check if blobs are archived or available
3. Doesn't adapt response based on retention period

#### Gap 2: ProcessSyncedValue (app.rs:591-609)

**Current Code**:
```rust
AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
    let value = decode_value(value_bytes);

    // ❌ Doesn't store execution payload bytes
    // ❌ Doesn't store blob sidecars
    // ❌ Doesn't handle metadata-only mode
    // ❌ Just sends Value to consensus

    reply.send(ProposedValue { ... });
}
```

**Problems**:
1. Even if we sent complete data, it's not being stored
2. No handling of archived vs available blocks
3. Commented-out code (lines 610-660) shows correct approach but incomplete

---

## Design Solution

### Key Insights

**1. Blobs Are Temporary (By Design)**

EIP-4844 philosophy: Cheap DA through time-limited availability
- Blobs available for retention period (e.g., 18 days)
- After retention: Pruned from ALL nodes (not a bug, a feature!)
- Rollups/users must download within retention window
- L1 consensus tracks: "blob existed and was available for N days"

**2. New Nodes Don't Need All Historical Blobs**

```
New node joining year-old network:
├─ EL: Syncs full blockchain (1 → 3,650,000) via devp2p ✅
│  └─ Already has executed state (blobs not needed!)
│
└─ CL: Syncs consensus metadata
   ├─ Old blocks (1 → 3,495,000): MetadataOnly ✅
   │  └─ Just Value (header + commitments)
   │
   └─ Recent blocks (3,495,001 → 3,650,000): Full ✅
      └─ Execution payload + blobs
```

**3. Two-Mode Sync Protocol**

Sync mode depends on archival status:

```rust
pub enum SyncedBlockData {
    /// Full sync: Recent blocks with blobs available
    Full {
        execution_payload: Bytes,
        blobs: Vec<BlobSidecar>,
    },

    /// Metadata-only: Archived blocks (blobs pruned)
    MetadataOnly {
        value: Value,  // Header + commitments only
    },
}
```

### Archival Service & Status Tracking

In v0 the validator that originally **built the block** is also responsible for archiving its blobs. The proposer runs an archival task immediately after finalization, exporting the blob sidecars (and any auxiliary metadata) to long-term storage and then recording the archival event in the blob engine. Once that happens the node emits a protocol notification (gossip message or lightweight consensus transaction) so peers learn the blobs are no longer expected to be served.

The blob engine should therefore expose a clearly defined status API instead of inferring archival status from `current_height - height`. The service transitions records like this:

```rust
pub enum BlobStatus {
    Available,   // Archival service has NOT completed export; blobs must be served
    Archiving,   // Export in progress; continue serving until finalized
    Archived,    // Export finalized; blobs may be pruned locally
}

impl BlobEngine {
    pub async fn status(&self, height: Height) -> BlobStatus { /* ... */ }
    pub async fn mark_archived(&self, height: Height, proof: ArchiveProof) { /* ... */ }
}
```

> **Why:** relying solely on `current_height - height > retention` can race with the actual export workflow. The explicit status lets us guarantee that no node prunes before the archival proof is recorded, and it gives us a clean hook to broadcast the “archived” notification to the network.

---

## Implementation Plan

### Phase 1: Define Data Structures (2 hours)

**File**: `ultramarine/crates/types/src/sync.rs` (NEW)

```rust
use bytes::Bytes;
use ethereum_ssz::{Decode, Encode};
use crate::{proposal_part::BlobSidecar, value::Value};

/// Synced block data with archival awareness
///
/// Supports two modes:
/// - Full: Recent blocks with execution payload + blobs
/// - MetadataOnly: Archived blocks with just consensus metadata
#[derive(Clone, Debug, Encode, Decode)]
pub enum SyncedBlockData {
    /// Full block data (within retention period)
    ///
    /// Contains everything needed to import the block:
    /// - ExecutionPayloadV3 bytes (SSZ-encoded)
    /// - Blob sidecars with KZG proofs
    ///
    /// Used when: current_height - height <= retention_period
    Full {
        /// SSZ-encoded ExecutionPayloadV3
        execution_payload: Bytes,

        /// Blob sidecars (each ~131KB + proofs)
        blobs: Vec<BlobSidecar>,
    },

    /// Metadata-only (beyond retention period)
    ///
    /// Contains just the consensus metadata:
    /// - ExecutionPayloadHeader (~516 bytes)
    /// - KZG commitments (48 bytes × N blobs)
    ///
    /// Used when: current_height - height > retention_period
    ///
    /// The execution payload and blobs are archived (pruned).
    /// The syncing peer's EL will have synced the block
    /// independently via devp2p.
    MetadataOnly {
        /// Lightweight Value (header + commitments)
        value: Value,
    },
}

impl SyncedBlockData {
    /// Create full sync data
    pub fn full(execution_payload: Bytes, blobs: Vec<BlobSidecar>) -> Self {
        Self::Full { execution_payload, blobs }
    }

    /// Create metadata-only sync data
    pub fn metadata_only(value: Value) -> Self {
        Self::MetadataOnly { value }
    }

    /// Check if this is full or metadata-only
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    /// Get Value from either mode
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Full { .. } => None,  // Value not stored in Full mode
            Self::MetadataOnly { value } => Some(value),
        }
    }

    /// Serialize to bytes for transmission
    pub fn encode_for_sync(&self) -> Result<Bytes, Box<dyn std::error::Error>> {
        let encoded = self.as_ssz_bytes();
        Ok(Bytes::from(encoded))
    }

    /// Deserialize from bytes
    pub fn decode_from_sync(bytes: Bytes) -> Result<Self, Box<dyn std::error::Error>> {
        let decoded = Self::from_ssz_bytes(&bytes)?;
        Ok(decoded)
    }

    /// Calculate approximate size
    pub fn size_estimate(&self) -> usize {
        match self {
            Self::Full { execution_payload, blobs } => {
                execution_payload.len() + blobs.len() * 131_168  // ~131KB + proofs
            }
            Self::MetadataOnly { value } => {
                std::mem::size_of::<Value>()  // ~2KB
            }
        }
    }
}
```

### Phase 2: Implement GetDecidedValue (4 hours)

**File**: `ultramarine/crates/node/src/app.rs:669-683`

```rust
AppMsg::GetDecidedValue { height, reply } => {
    info!(%height, "🟢🟢 GetDecidedValue");

    let decided_value = state.get_decided_value(height).await;

    let raw_decided_value = if let Some(decided_value) = decided_value {
        let round = decided_value.certificate.round;

        // Check archival status
        let current_height = state.current_height;
        let retention_period = state.config.blob_retention_blocks;  // e.g., 155,000
        let blob_status = BlobStatus::check(height, current_height, retention_period);

        let synced_data = match blob_status {
            BlobStatus::Available => {
                // FULL SYNC: Recent block, blobs available
                info!(
                    height = %height,
                    "Serving full sync (blobs available)"
                );

                // Get execution payload
                let execution_payload = match state.get_block_data(height, round).await {
                    Some(data) => data,
                    None => {
                        error!(
                            height = %height,
                            "Missing execution payload for available block"
                        );
                        if reply.send(None).is_err() {
                            error!("Failed to send GetDecidedValue error reply");
                        }
                        return;
                    }
                };

                // Get blobs
                let blobs = if !decided_value.value.metadata.blob_kzg_commitments.is_empty() {
                    match state.blob_engine().get_for_import(height).await {
                        Ok(blobs) => {
                            info!(
                                height = %height,
                                blob_count = blobs.len(),
                                "Retrieved blobs for full sync"
                            );
                            blobs
                        }
                        Err(e) => {
                            error!(
                                height = %height,
                                error = %e,
                                "Failed to retrieve blobs for available block"
                            );
                            if reply.send(None).is_err() {
                                error!("Failed to send GetDecidedValue error reply");
                            }
                            return;
                        }
                    }
                } else {
                    vec![]
                };

                SyncedBlockData::full(execution_payload, blobs)
            }

            BlobStatus::Archived => {
                // METADATA-ONLY SYNC: Old block, blobs archived
                info!(
                    height = %height,
                    "Serving metadata-only sync (blobs archived)"
                );

                SyncedBlockData::metadata_only(decided_value.value)
            }
        };

        // Serialize
        let value_bytes = match synced_data.encode_for_sync() {
            Ok(bytes) => {
                info!(
                    height = %height,
                    size_kb = bytes.len() / 1024,
                    mode = if synced_data.is_full() { "full" } else { "metadata" },
                    "Encoded synced block data"
                );
                bytes
            }
            Err(e) => {
                error!(
                    height = %height,
                    error = %e,
                    "Failed to encode synced block data"
                );
                if reply.send(None).is_err() {
                    error!("Failed to send GetDecidedValue error reply");
                }
                return;
            }
        };

        Some(RawDecidedValue {
            certificate: decided_value.certificate,
            value_bytes,
        })
    } else {
        info!(%height, "No decided value found at height");
        None
    };

    if reply.send(raw_decided_value).is_err() {
        error!("Failed to send GetDecidedValue reply");
    }
}
```

### Phase 3: Implement ProcessSyncedValue (5 hours)

**File**: `ultramarine/crates/node/src/app.rs:591-661`

```rust
AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
    info!(%height, %round, "🟢🟢 Processing synced value");

    // Decode synced block data
    let synced_data = match SyncedBlockData::decode_from_sync(value_bytes) {
        Ok(data) => {
            info!(
                height = %height,
                mode = if data.is_full() { "full" } else { "metadata" },
                "Decoded synced block data"
            );
            data
        }
        Err(e) => {
            error!(
                height = %height,
                error = %e,
                "Failed to decode synced block data"
            );
            if reply.send(None).is_err() {
                error!("Failed to send ProcessSyncedValue error reply");
            }
            return;
        }
    };

    match synced_data {
        SyncedBlockData::Full { execution_payload, blobs } => {
            // FULL SYNC MODE: Store everything and import
            info!(
                height = %height,
                payload_kb = execution_payload.len() / 1024,
                blob_count = blobs.len(),
                "Processing full sync"
            );

            // 1. Store execution payload
            if let Err(e) = state.store_undecided_block_data(execution_payload.clone()).await {
                error!(
                    height = %height,
                    error = %e,
                    "Failed to store execution payload"
                );
                if reply.send(None).is_err() {
                    error!("Failed to send ProcessSyncedValue error reply");
                }
                return;
            }

            // 2. Verify and store blobs as DECIDED
            if !blobs.is_empty() {
                let round_i64 = round.as_i64();

                // Verify KZG proofs
                if let Err(e) = state.blob_engine()
                    .verify_and_store(height, round_i64, &blobs)
                    .await
                {
                    error!(
                        height = %height,
                        error = %e,
                        "Blob verification failed"
                    );
                    if reply.send(None).is_err() {
                        error!("Failed to send ProcessSyncedValue error reply");
                    }
                    return;
                }

                // Mark as decided immediately
                if let Err(e) = state.blob_engine().mark_decided(height, round_i64).await {
                    error!(
                        height = %height,
                        error = %e,
                        "Failed to mark blobs as decided"
                    );
                    if reply.send(None).is_err() {
                        error!("Failed to send ProcessSyncedValue error reply");
                    }
                    return;
                }

                info!(
                    height = %height,
                    blob_count = blobs.len(),
                    "✅ Blobs verified and stored as decided"
                );
            }

            // 3. Decode Value from execution payload
            let value = match decode_value_from_payload(&execution_payload) {
                Some(v) => v,
                None => {
                    error!(%height, "Failed to decode value from payload");
                    if reply.send(None).is_err() {
                        error!("Failed to send ProcessSyncedValue error reply");
                    }
                    return;
                }
            };

            // 4. Create ProposedValue
            let proposed_value = ProposedValue {
                height,
                round,
                valid_round: Round::Nil,
                proposer,
                value,
                validity: Validity::Valid,
            };

            // 5. Store proposal
            if let Err(e) = state.store.store_undecided_proposal(proposed_value.clone()).await {
                error!(
                    height = %height,
                    error = %e,
                    "Failed to store proposal"
                );
                if reply.send(None).is_err() {
                    error!("Failed to send ProcessSyncedValue error reply");
                }
                return;
            }

            // 6. Send to consensus
            if reply.send(Some(proposed_value)).is_err() {
                error!("Failed to send ProcessSyncedValue reply");
                return;
            }

            info!(
                height = %height,
                "✅ Successfully processed full sync"
            );
        }

        SyncedBlockData::MetadataOnly { value } => {
            // METADATA-ONLY MODE: Just store consensus metadata
            info!(
                height = %height,
                "Processing metadata-only sync (archived block)"
            );

            // Don't try to import - EL already has this block via devp2p
            // Just store the consensus metadata

            let proposed_value = ProposedValue {
                height,
                round,
                valid_round: Round::Nil,
                proposer,
                value,
                validity: Validity::Valid,
            };

            // Store proposal (just metadata)
            if let Err(e) = state.store.store_undecided_proposal(proposed_value.clone()).await {
                error!(
                    height = %height,
                    error = %e,
                    "Failed to store metadata"
                );
                if reply.send(None).is_err() {
                    error!("Failed to send ProcessSyncedValue error reply");
                }
                return;
            }

            // Send to consensus
            if reply.send(Some(proposed_value)).is_err() {
                error!("Failed to send ProcessSyncedValue reply");
                return;
            }

            info!(
                height = %height,
                "✅ Successfully processed metadata-only sync"
            );
        }
    }
}
```

### Phase 4: Configuration (1 hour)

**File**: `ultramarine/crates/types/src/config.rs` (or wherever config lives)

```rust
pub struct ConsensusConfig {
    // Existing fields...

    /// Blob retention period in blocks
    ///
    /// Blobs older than this are archived (pruned).
    /// Ethereum mainnet: ~155,000 blocks (18 days at 10s blocks)
    ///
    /// Default: 155,000 blocks (~18 days)
    pub blob_retention_blocks: u64,

    /// Execution payload retention period in blocks
    ///
    /// Similar to blobs, execution payloads can be pruned after
    /// they've been imported to EL.
    ///
    /// Default: Same as blob retention
    pub payload_retention_blocks: u64,
}

impl Default for ConsensusConfig {
    fn default() -> Self {
        Self {
            // ... existing defaults
            blob_retention_blocks: 155_000,  // 18 days
            payload_retention_blocks: 155_000,
        }
    }
}
```

### Phase 5: Update Pruning Logic (2 hours)

**File**: `ultramarine/crates/consensus/src/state.rs`

Update the pruning logic to use configured retention period instead of hardcoded 5 blocks. **Important:** pruning should only run after the archival service has marked a height as `Archived`, otherwise we risk deleting data that is still being exported.

```rust
pub async fn commit(&mut self, certificate: CommitCertificate) -> eyre::Result<()> {
    // ... existing commit logic ...

    // Prune old data (use configured retention)
    let retention = self.config.payload_retention_blocks;
    let retain_height = Height::new(
        certificate.height.as_u64().saturating_sub(retention)
    );
    // Only prune once the archival service recorded the height as Archived.
    if self.blob_engine.status(certificate.height).await == BlobStatus::Archived {
        self.store.prune(retain_height).await?;
    }

    // Prune old blobs (use configured retention)
    let blob_retention = self.config.blob_retention_blocks;
    let blob_prune_height = Height::new(
        certificate.height.as_u64().saturating_sub(blob_retention)
    );
    self.blob_engine.prune_archived_before(blob_prune_height).await?;

    // ... rest of commit logic ...
}
```

---

## Testing Plan

### Unit Tests

#### Test 1: Archival Status Check

```rust
#[test]
fn test_blob_status_check() {
    let current = Height::new(3_650_000);
    let retention = 155_000;

    // Recent block: Available
    let recent = Height::new(3_640_000);
    assert_eq!(
        BlobStatus::check(recent, current, retention),
        BlobStatus::Available
    );

    // Old block: Archived
    let old = Height::new(3_400_000);
    assert_eq!(
        BlobStatus::check(old, current, retention),
        BlobStatus::Archived
    );
}
```

#### Test 2: SyncedBlockData Serialization

```rust
#[test]
fn test_synced_data_full_roundtrip() {
    let payload = Bytes::from(vec![1, 2, 3, 4]);
    let blobs = vec![/* test blobs */];

    let data = SyncedBlockData::full(payload.clone(), blobs.clone());
    let encoded = data.encode_for_sync().unwrap();
    let decoded = SyncedBlockData::decode_from_sync(encoded).unwrap();

    match decoded {
        SyncedBlockData::Full { execution_payload, blobs: dec_blobs } => {
            assert_eq!(execution_payload, payload);
            assert_eq!(dec_blobs.len(), blobs.len());
        }
        _ => panic!("Expected Full mode"),
    }
}

#[test]
fn test_synced_data_metadata_roundtrip() {
    let value = Value::new(/* ... */);

    let data = SyncedBlockData::metadata_only(value.clone());
    let encoded = data.encode_for_sync().unwrap();
    let decoded = SyncedBlockData::decode_from_sync(encoded).unwrap();

    match decoded {
        SyncedBlockData::MetadataOnly { value: dec_value } => {
            assert_eq!(dec_value.id(), value.id());
        }
        _ => panic!("Expected MetadataOnly mode"),
    }
}
```

### Integration Tests

#### Test 3: Full Sync (Recent Block)

```rust
#[tokio::test]
async fn test_sync_recent_block_with_blobs() {
    let node1 = setup_node(1).await;
    let node2 = setup_node(2).await;

    // Node 1 proposes block with blobs at height 100
    let blobs = generate_test_blobs(3);
    node1.propose_block_with_blobs(Height::new(100), blobs).await;
    wait_for_height(&node1, 101).await;

    // Node 2 syncs (within retention)
    let decided = node1.get_decided_value(Height::new(100)).await.unwrap();
    let synced = SyncedBlockData::decode_from_sync(decided.value_bytes).unwrap();

    // Should be Full mode
    assert!(synced.is_full());

    // Process sync
    node2.process_synced_value(Height::new(100), decided).await.unwrap();

    // Verify blobs stored
    let imported_blobs = node2.blob_engine.get_for_import(Height::new(100)).await.unwrap();
    assert_eq!(imported_blobs.len(), 3);
}
```

#### Test 4: Metadata-Only Sync (Archived Block)

```rust
#[tokio::test]
async fn test_sync_archived_block() {
    let node1 = setup_node_with_retention(1, 100).await;  // 100 blocks retention
    let node2 = setup_node(2).await;

    // Node 1 produces 200 blocks (first 100 will be archived)
    for h in 1..=200 {
        node1.propose_block_with_blobs(Height::new(h), vec![]).await;
    }
    wait_for_height(&node1, 201).await;

    // Try to sync block 50 (archived)
    let decided = node1.get_decided_value(Height::new(50)).await.unwrap();
    let synced = SyncedBlockData::decode_from_sync(decided.value_bytes).unwrap();

    // Should be MetadataOnly mode
    assert!(!synced.is_full());

    // Process sync
    node2.process_synced_value(Height::new(50), decided).await.unwrap();

    // Blobs should NOT be present
    let blobs_result = node2.blob_engine.get_for_import(Height::new(50)).await;
    assert!(blobs_result.is_err() || blobs_result.unwrap().is_empty());

    // But consensus metadata should be stored
    let value = node2.get_decided_value(Height::new(50)).await;
    assert!(value.is_some());
}
```

#### Test 5: New Node From Genesis

```rust
#[tokio::test]
async fn test_new_node_sync_from_genesis() {
    // Simulate year-old network
    let retention = 1000;  // Small retention for test
    let network_height = 5000;

    // Setup existing node with history
    let old_node = setup_node_with_retention(1, retention).await;
    for h in 1..=network_height {
        old_node.propose_block(Height::new(h)).await;
    }
    wait_for_height(&old_node, network_height + 1).await;

    // New node joins
    let new_node = setup_node(2).await;

    // Sync from genesis
    for h in 1..=network_height {
        let decided = old_node.get_decided_value(Height::new(h)).await.unwrap();
        let synced = SyncedBlockData::decode_from_sync(decided.value_bytes).unwrap();

        if h < network_height - retention {
            // Old blocks: MetadataOnly
            assert!(!synced.is_full());
        } else {
            // Recent blocks: Full
            assert!(synced.is_full());
        }

        new_node.process_synced_value(Height::new(h), decided).await.unwrap();
    }

    // Verify new node caught up
    assert_eq!(new_node.current_height(), Height::new(network_height));
}
```

---

## Performance Considerations

### Message Size by Mode

**Full Mode** (recent blocks):
- Value metadata: ~2 KB
- Execution payload: ~100 KB (varies)
- 3 blobs: 3 × 131 KB = ~393 KB
- **Total**: ~495 KB per sync response

**MetadataOnly Mode** (archived blocks):
- Value metadata: ~2 KB
- **Total**: ~2 KB per sync response

**Bandwidth savings**: 250x smaller for archived blocks!

### Sync Performance Estimates

**Syncing 365,000 blocks (1 year at 10s blocks)**:
- Archived (first 210,000): 210,000 × 2 KB = ~420 MB
- Recent (last 155,000): 155,000 × 495 KB = ~77 GB

**Total**: ~77.4 GB (vs ~180 GB if all blocks had full data)

**Time estimate** (100 Mbps network):
- Archived: ~34 seconds
- Recent: ~103 minutes
- **Total**: ~104 minutes

> **TODO:** Re-measure once we have real payload sizes and adjust `max_response_size` / bandwidth expectations accordingly.

---

## Security Considerations

### Verification Requirements

**Full Mode**: MUST verify KZG proofs
```rust
// Always verify, even for synced blobs
state.blob_engine()
    .verify_and_store(height, round, &blobs)
    .await?;
```

**MetadataOnly Mode**: Certificate validation (done by Malachite)
- No blobs to verify
- Malachite validates certificate signatures

### Trust Model

**Full Sync**:
- Trust certificate (2/3+ validators)
- Verify blobs cryptographically (KZG proofs)
- No trust in serving peer needed

**Metadata-Only Sync**:
- Trust certificate (2/3+ validators)
- Trust EL sync (separate devp2p protocol)
- Minimal data, minimal risk

---

## Migration Plan

### Phase 1: Implement Two-Mode Sync (Week 1)
1. Add `SyncedBlockData` enum
2. Implement archival status checking
3. Update `GetDecidedValue` with mode selection
4. Update `ProcessSyncedValue` with mode handling
5. Add configuration for retention period

### Phase 2: Testing (Week 2)
1. Unit tests for serialization
2. Integration tests for both modes
3. End-to-end sync scenarios
4. Performance benchmarks

### Phase 3: Deployment (Week 3)
1. Deploy to testnet
2. Test with various retention periods
3. Monitor sync performance
4. Production rollout

---

## Open Questions / Follow-ups

1. **Archival notification format** – We need to choose how the archival service advertises completed exports (consensus transaction, gossip topic, etc.) so every peer can safely transition to `BlobStatus::Archived`.
2. **Execution payload recovery** – Define a fallback if `state.get_block_data` is missing during full sync (e.g., re-query EL vs. abort sync). Document the policy and error handling.
3. **Response sizing** – Full-mode replies can be hundreds of MB. Confirm Malachite’s `max_response_size` and capture recommended settings in operator docs.
4. **Peer scoring on sync errors** – When decoding/verifying synced blobs fails we should return `None` to Malachite *and* penalize/blacklist the serving peer. Finalize the scoring policy.
5. **Testing harness** – Integration tests in this document rely on helper APIs that don’t exist yet. Track the work to build the fixtures or adapt existing harnesses before implementation.
6. **Coordination with archival service** – Ensure pruning tasks listen to the same status updates as the sync handlers so we never prune data that another peer still expects us to serve.

---

## Appendix: Comparison with Ethereum

### Lighthouse/Ethereum Architecture

```
CL (Lighthouse):
├─ Beacon chain sync (CL-to-CL)
│  └─ Just beacon blocks (no execution payloads)
│
└─ Blob sync (separate protocol)
   ├─ BlobSidecarsByRange RPC
   ├─ BlobSidecarsByRoot RPC
   └─ Coupled with block sync

EL (Reth/Geth):
└─ Blockchain sync (EL-to-EL via devp2p)
   └─ Completely separate from CL
```

### Ultramarine Architecture (This Design)

```
CL (Ultramarine):
├─ Consensus sync (CL-to-CL via Malachite)
│  ├─ Full: payload + blobs (recent)
│  └─ MetadataOnly: just Value (archived)
│
└─ Blob lifecycle managed in CL
   └─ Archived after retention period

EL (Reth):
└─ Blockchain sync (EL-to-EL via devp2p)
   └─ Independent from CL sync
```

**Key Difference**: Ultramarine bundles execution payloads + blobs in CL sync, while Ethereum separates them completely.

**Advantage**: Simpler architecture, fewer moving parts
**Trade-off**: CL stores more data temporarily (but pruned after retention)

---

## References

### Internal Documentation
- `docs/BLOB_SYNC_GAP_ANALYSIS.md` - Original gap analysis
- `docs/PHASE_5_COMPLETION.md` - Blob integration status
- `docs/FINAL_PLAN.md` - Overall project plan

### External References
- EIP-4844: https://eips.ethereum.org/EIPS/eip-4844
- Ethereum Consensus Specs: `/consensus-specs/specs/deneb/`
- Lighthouse Sync: `/lighthouse/beacon_node/network/src/sync/`

---

**End of Document**
