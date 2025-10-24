# Phase 4: Blob Header Persistence — Implementation Progress

**Status**: 🟡 In Progress  
**Started**: 2025-01-XX  
**Target**: Production-ready blob header persistence (no blob-engine dependency)

---

## 🎯 Goals

- Persist every consensus-visible blob header in the consensus store (remove blob-engine dependency).
- Keep the in-memory parent-root cache consistent even when rounds fail or nodes restart.
- Support multi-round proposals with clean header isolation.
- Maintain a continuous parent-root chain, including blobless blocks.
- Provide O(1) `get_latest_blob_header()` performance.

---

## 📐 Design Overview

### Storage Model

| Column Family / Table      | Purpose                          | Key Format                     | Value                                |
|---------------------------|----------------------------------|--------------------------------|--------------------------------------|
| `block_headers_undecided` | Headers written pre-finalization | `(height:u64, round:i64)` (BE) | `ConsensusBlobHeader` protobuf       |
| `block_headers_decided`   | Canonical finalized headers      | `height:u64` (BE)              | `ConsensusBlobHeader` protobuf       |
| `block_headers_meta`      | Metadata (latest pointer, flags) | `b"latest_header_height"`      | `height:u64` (BE bytes)              |

### ConsensusBlobHeader Newtype

```rust
pub struct ConsensusBlobHeader(pub SignedBeaconBlockHeader);
```

- Consensus-friendly naming; Deneb compatibility remains internal.  
- Provides helpers (`height()`, `hash_tree_root()`, `parent_root()`).  
- Implements `malachitebft_proto::Protobuf` by delegating to the inner type.

### Header Lifecycle

```
┌──────────────────────────────┐
│ UNDECIDED (height, round)    │  put_undecided_blob_header
│ • Written on propose/receive │  • Idempotent write (compare bytes)
│ • Multiple rounds per height │
└──────────────▲───────────────┘
               │  mark_blob_header_decided (single WriteBatch)
               │   1. Read undecided (h,r)
               │   2. Write decided (h)
               │   3. Update latest pointer
               │   4. Delete undecided (h,r)
               │
┌──────────────┴───────────────┐
│      DECIDED (height)        │  get_decided_blob_header
│ • Exactly one canonical hdr  │  • Feeds parent-root & restarts
└──────────────────────────────┘
```

### Cache Management (CRITICAL RULE)

`last_blob_header_root` is updated **only** when the header is known-canonical:

1. **Startup**: `hydrate_blob_header_root()` loads the latest decided header (if any).  
2. **Finalization**: `commit()` updates the cache after `mark_blob_header_decided()`.

➡️ We do **not** mutate the cache during proposal or receive flows; failed rounds cannot corrupt the parent root.

### Restream & Recovery

- Restream pulls headers via `store.get_undecided_blob_header(height, round)` (or decided fallback) — no blob-engine dependency.
- `cleanup_stale_undecided_headers()` runs on startup to drop orphaned entries left behind by crashes/timeouts.
- Height 0 parent root is `B256::ZERO`; heights > 0 must resolve the parent from the decided table (migration window may log warnings).

### Optional Migration Support

- Iterate decided heights.  
- For blobbed heights, read header from first sidecar and write into `block_headers_decided`.  
- Update latest pointer and set `headers_migrated_v1` flag in metadata.  
- Blobless heights remain empty and will repopulate after upgrade.  
- During migration window, missing parent headers can be logged/warned instead of failing validation.

---

## 🚀 Implementation Roadmap

### Phase 1 – Core Storage (est. 6h)

1. **ConsensusBlobHeader newtype**  
   - [ ] Create `crates/types/src/consensus_blob_header.rs`.  
   - [ ] Add helper accessors + `Protobuf` passthrough.  
   - [ ] Export from `crates/types/src/lib.rs`.  
   - [ ] Unit test construction/hash helpers.

2. **Table definitions / initialization**  
   - [ ] Add `block_headers_undecided`, `block_headers_decided`, `block_headers_meta`.  
   - [ ] Ensure big-endian key encoding for deterministic iteration.  
   - [ ] Confirm DB transactions cover multi-table writes; use RocksDB if redb batching proves insufficient.

3. **Store methods (idempotent + atomic)**  
   - [ ] `put_undecided_blob_header` — compare existing bytes before writing.  
   - [ ] `get_undecided_blob_header`.  
   - [ ] `drop_undecided_blob_header`.  
   - [ ] `mark_blob_header_decided` — single `WriteBatch`.  
   - [ ] `get_decided_blob_header`.  
   - [ ] `get_latest_blob_header` — O(1) via metadata pointer.  
   - [ ] `get_all_undecided_headers_before` — supports startup cleanup.  
   - [ ] Update async wrappers (spawn_blocking).  
   - [ ] Update metrics (bytes/time) for reads/writes.

### Phase 2 – State Integration (est. 5h)

1. **Startup hydration & cleanup**  
   - [ ] `hydrate_blob_header_root()` seeds cache from decided table (logs restored root).  
   - [ ] `cleanup_stale_undecided_headers()` removes orphaned `(height, round)` entries (decided or beyond retention window).

2. **Proposer flow**  
   - [ ] `prepare_blob_sidecar_parts()` returns `(ConsensusBlobHeader, Vec<BlobSidecar>)`.  
   - [ ] `build_blob_header_message()` uses cached parent (height 0 guard).  
   - [ ] Sign header, wrap in `ConsensusBlobHeader`.  
   - [ ] Call `put_undecided_blob_header(height, round, &header)` **before** streaming.  
   - [ ] Cache remains untouched.  
   - [ ] Continue with blob verification/storage and streaming.

3. **Receiver flow**  
   - [ ] After `verify_blob_sidecars`, store header via `put_undecided_blob_header`.  
   - [ ] Blobless blocks produce placeholder-signed header (all-zero signature).  
   - [ ] Cache unaffected.

4. **Restream path**  
   - [ ] Fetch header via `get_undecided_blob_header(height, proposal_round)` (fallback to decided if necessary).  
   - [ ] Stream original sidecars with stored header.  
   - [ ] Log/abort if header missing.

5. **Commit flow**  
   - [ ] Call `mark_blob_header_decided(height, round)` (fatal on failure).  
   - [ ] Read decided header and set `last_blob_header_root = header.hash_tree_root()`.  
   - [ ] Log new root for observability.

6. **Verification adjustments**  
   - [ ] Guard `height == 0` (parent = zero).  
   - [ ] Fetch parent from decided table; error if missing (migration window may warn).  
   - [ ] Continue inclusion-proof, signature, commitment checks.

7. **Round cleanup**  
   - [ ] Ensure every timeout/round-drop path calls `drop_undecided_blob_header`.  
   - [ ] Integrate with pruning routines.

### Phase 3 – Tests (est. 6h)

1. **Store unit tests**  
   - [ ] Undecided roundtrip.  
   - [ ] Multi-round isolation.  
   - [ ] `mark_blob_header_decided` lifecycle (atomic promotion).  
   - [ ] `get_latest_blob_header()` performance (<10ms with 1k entries).  
   - [ ] Drop undecided entry.  
   - [ ] Idempotent writes.  
   - [ ] Height 0 guard.  
   - [ ] Optional: simulate partial failure to confirm atomicity.

2. **State tests**  
   - [ ] Cache only moves on commit.  
   - [ ] Parent-root chaining across commits (blobbed + blobless).  
   - [ ] Startup cleanup removes stale undecided entries.

3. **Integration tests**  
   - [ ] Restart survival (height 100 decided → restart → height 101 parent matches).  
   - [ ] Blobless block continuity (blob → no blob → blob).  
   - [ ] Multi-round isolation (round 1 undecided persists until cleanup, round 2 decided).

### Phase 4 – Cleanup & Docs (est. 1h)

- [ ] Remove `put_beacon_header` / `get_beacon_header` and header CF from blob engine.  
- [ ] Drop header helpers from blob-engine RocksDB implementation.  
- [ ] Remove unused imports; run `cargo fmt` / `cargo clippy`.  
- [ ] Document header lifecycle and cache strategy in `store.rs` / `state.rs`.  
- [ ] Update `CHANGELOG.md` (breaking: wipe data dir or run migration script).  
- [ ] Optional: add metrics for undecided/decided counts and O(1) pointer hits.

---

## 🧪 Testing Checklist

### Manual

- Fresh start (clean data dir) → propose blocks → verify headers stored.  
- Restart after several heights → ensure cache restoration + correct parent roots.  
- Simulate multi-round timeout → confirm undecided entries removed.  
- Blobless block sandwich (blob/no-blob/blob) → verify continuous chain.

### Automated

- `cargo test -p ultramarine-consensus --lib` (store/state tests).  
- `cargo test -p ultramarine-node --test header_lifecycle` (integration).  
- `cargo test --workspace`.  
- `cargo build --workspace --release`.

---

## 🐛 Known Issues / Blockers

- Verify DB backend supports atomic multi-table writes (use RocksDB if necessary).  
- Ensure composite-key ordering uses big-endian encoding.  
- Define behaviour for missing parent headers during optional migration window.

---

## 📝 Decision Log

| Date       | Decision                                                      | Rationale                                       |
|------------|----------------------------------------------------------------|-------------------------------------------------|
| 2025-01-XX | Cache only follows finalized headers                           | Prevents failed rounds from leaking forward     |
| 2025-01-XX | Undecided/decided split with atomic promotion                  | Matches blob lifecycle & supports multi-round   |
| 2025-01-XX | Restream pulls headers from consensus store                    | Removes blob-engine dependency                  |
| 2025-01-XX | Startup cleanup of stale undecided entries                     | Avoids unbounded growth after crashes           |
| 2025-01-XX | Optional migration reconstructs signatures from sidecars       | Blobbed heights recoverable; blobless handled live |

---

## 🎯 Success Criteria

- All phases complete with tests passing.  
- `get_latest_blob_header()` verified O(1).  
- Cache consistent across restarts and failed rounds.  
- Parent-root chain unbroken for blobless blocks.  
- Blob engine no longer persists headers.  
- Documentation + CHANGELOG updated.

---

## 📊 Progress Snapshot

| Phase                     | Status | Hours | Progress |
|---------------------------|--------|-------|----------|
| Phase 1 – Core Storage    | 🔴 Not Started | 0 / 6 | 0% |
| Phase 2 – State Integration | 🔴 Not Started | 0 / 5 | 0% |
| Phase 3 – Tests           | 🔴 Not Started | 0 / 6 | 0% |
| Phase 4 – Cleanup & Docs  | 🔴 Not Started | 0 / 1 | 0% |

*(Legend: 🔴 Not Started · 🟡 In Progress · 🟢 Complete)*

---

## 🔄 Daily Log

### 2025-01-XX
- [ ] Drafted updated design (this document).  
- [ ] Next: implement ConsensusBlobHeader newtype (Phase 1.1).

---

## 🚀 Next Actions

1. Implement `ConsensusBlobHeader` newtype (Phase 1.1).
2. Add new column families & table initialization (Phase 1.2).
3. Implement storage methods with idempotency + atomic promotion (Phase 1.3).

---
---

# 🔄 ALTERNATIVE DESIGN PROPOSAL (PENDING APPROVAL)

**Status**: ⚠️ **NEEDS REVIEW**
**Review Date**: Monday, January 27th, 2025
**Reviewers**: Engineering Leads

---

## 📋 Executive Summary

This alternative design proposes a **three-layer architecture** that cleanly separates:
1. **Pure BFT consensus state** (Malachite/Tendermint naming, no Ethereum)
2. **Blob metadata** (Ethereum EIP-4844 compatibility bridge)
3. **Blob data storage** (prunable raw data)

**Key Difference from Current Plan**: Instead of storing `ConsensusBlobHeader` (which wraps `SignedBeaconBlockHeader`) in consensus, we store two separate metadata structures that clearly separate consensus concerns from Ethereum compatibility.

---

## 🎯 Design Philosophy

### Problem with Current Approach

The current plan stores `ConsensusBlobHeader(SignedBeaconBlockHeader)` in the consensus store, which:
- ❌ Leaks Ethereum terminology into consensus layer (`slot`, `proposer_index`, etc.)
- ❌ Tightly couples consensus to Ethereum blob format
- ❌ Makes it hard to swap DA layers (e.g., migrate to Celestia later)
- ❌ Mixes BFT concerns with Ethereum compatibility

### Three-Layer Architecture (Proposed)

```
┌─────────────────────────────────────────────────────────────┐
│        LAYER 1: CONSENSUS STATE (Pure BFT)                   │
│                    Keep Forever ♾️                          │
├─────────────────────────────────────────────────────────────┤
│ decided_values:           height → Value                    │
│ certificates:             height → CommitCertificate        │
│ consensus_block_metadata: height → ConsensusBlockMetadata  │
│                                                              │
│ Naming: Tendermint/Malachite aligned (height, round)       │
│ Purpose: Pure BFT consensus decisions                       │
│ Size: ~200 bytes per block                                  │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│     LAYER 2: BLOB METADATA (Ethereum Compatibility)          │
│                    Keep Forever ♾️                          │
├─────────────────────────────────────────────────────────────┤
│ blob_metadata:           height → BlobMetadata              │
│ blob_metadata_undecided: (h, r) → BlobMetadata              │
│                                                              │
│ Contains: parent_blob_root, kzg_commitments, state_root    │
│ Purpose: EIP-4844 compatibility bridge                      │
│ Size: ~300 bytes per block                                  │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│       LAYER 3: BLOB ENGINE (Data Storage)                    │
│                Prune after 30 days 🗑️                       │
├─────────────────────────────────────────────────────────────┤
│ decided_blobs:       height → Vec<BlobSidecar>              │
│ undecided_blobs:     (h, r) → Vec<BlobSidecar>              │
│ execution_payloads:  height → Bytes                         │
│                                                              │
│ Purpose: Raw data storage (prunable)                        │
│ Size: ~780 GB for 30 days                                   │
└─────────────────────────────────────────────────────────────┘
```

---

## 📐 Type Definitions

### 1. ConsensusBlockMetadata (Layer 1 - Pure BFT)

```rust
/// Pure consensus-layer block metadata
///
/// Contains ONLY what's relevant to Ultramarine's BFT consensus.
/// Uses Tendermint/Malachite terminology (height, round, proposer).
/// NO Ethereum types, NO blob-specific data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsensusBlockMetadata {
    /// Block height (NOT "slot")
    pub height: Height,

    /// Consensus round that decided this block
    pub round: Round,

    /// Validator who proposed this block
    pub proposer: Address,

    /// Timestamp when block was proposed (Unix timestamp)
    pub timestamp: u64,

    /// Hash of active validator set at this height
    pub validator_set_hash: B256,

    /// Execution layer block hash (from Engine API)
    pub execution_block_hash: B256,

    /// Gas limit for this block
    pub gas_limit: u64,

    /// Gas used in this block
    pub gas_used: u64,
}
```

**Protobuf Schema**:
```protobuf
message ConsensusBlockMetadata {
  uint64 height = 1;
  int32 round = 2;
  bytes proposer = 3;  // Address (20 bytes)
  uint64 timestamp = 4;
  bytes validator_set_hash = 5;  // B256 (32 bytes)
  bytes execution_block_hash = 6;  // B256 (32 bytes)
  uint64 gas_limit = 7;
  uint64 gas_used = 8;
}
```

**Key Points**:
- ✅ Zero Ethereum terminology (`height` not `slot`, `proposer` not `proposer_index`)
- ✅ Pure BFT concerns only
- ✅ ~200 bytes per block

---

### 2. BlobMetadata (Layer 2 - Ethereum Compatibility)

```rust
/// Ethereum EIP-4844 compatibility metadata
///
/// This is the bridge between Ultramarine consensus and Ethereum blob format.
/// Contains everything needed to build SignedBeaconBlockHeader.
/// Isolated from consensus layer for technology neutrality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Block height (maps to Ethereum slot)
    pub height: Height,

    /// Parent blob header root (chains blob headers together)
    pub parent_blob_root: B256,

    /// KZG commitments for all blobs at this height
    pub kzg_commitments: Vec<KzgCommitment>,

    /// Number of blobs (0 for blobless blocks)
    pub blob_count: u8,

    /// Execution layer state root
    pub execution_state_root: B256,

    /// Execution layer block hash
    pub execution_block_hash: B256,
}

impl BlobMetadata {
    /// Build Ethereum-compatible BeaconBlockHeader
    ///
    /// This is ONLY called when constructing BlobSidecars for network streaming.
    /// Consensus layer never calls this - it's an Ethereum compatibility shim.
    pub fn to_beacon_header(&self) -> BeaconBlockHeader {
        BeaconBlockHeader {
            slot: self.height.as_u64(),
            proposer_index: 0,  // Not used in Ultramarine
            parent_root: self.parent_blob_root,
            state_root: self.execution_state_root,
            body_root: self.compute_body_root(),
        }
    }

    /// Compute body_root for BeaconBlockBody
    fn compute_body_root(&self) -> B256 {
        let body = BeaconBlockBodyMinimal {
            blob_kzg_commitments: self.kzg_commitments.clone(),
        };
        body.hash_tree_root()
    }

    /// Compute blob root for parent chaining
    pub fn compute_blob_root(&self) -> B256 {
        self.to_beacon_header().hash_tree_root()
    }

    /// Create metadata for blobless block
    pub fn blobless(
        height: Height,
        parent_blob_root: B256,
        execution: &ExecutionPayloadHeader,
    ) -> Self {
        Self {
            height,
            parent_blob_root,
            kzg_commitments: vec![],
            blob_count: 0,
            execution_state_root: execution.state_root,
            execution_block_hash: execution.block_hash,
        }
    }
}
```

**Protobuf Schema**:
```protobuf
message BlobMetadata {
  uint64 height = 1;
  bytes parent_blob_root = 2;  // B256 (32 bytes)
  repeated bytes kzg_commitments = 3;  // 48 bytes each
  uint32 blob_count = 4;
  bytes execution_state_root = 5;  // B256 (32 bytes)
  bytes execution_block_hash = 6;  // B256 (32 bytes)
}
```

**Key Points**:
- ✅ All Ethereum baggage isolated here
- ✅ Conversion to `BeaconBlockHeader` only when building sidecars
- ✅ Consensus never sees this
- ✅ ~300 bytes per block (6 blobs avg)

---

## 🗄️ Storage Model

### Consensus Store (redb)

```rust
// === LAYER 1: CONSENSUS STATE (BFT) ===
const DECIDED_VALUES: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_values");

const CERTIFICATES: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("certificates");

const CONSENSUS_BLOCK_METADATA: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("consensus_block_metadata");

// === LAYER 2: BLOB METADATA (Ethereum compat) ===
const BLOB_METADATA: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("blob_metadata");

const BLOB_METADATA_UNDECIDED: redb::TableDefinition<UndecidedKey, Vec<u8>> =
    redb::TableDefinition::new("blob_metadata_undecided");

const BLOB_METADATA_META: redb::TableDefinition<&str, Vec<u8>> =
    redb::TableDefinition::new("blob_metadata_meta");

// === LAYER 3: EXECUTION PAYLOADS (prunable) ===
const EXECUTION_PAYLOADS: redb::TableDefinition<HeightKey, Vec<u8>> =
    redb::TableDefinition::new("execution_payloads");
```

### Blob Engine (RocksDB)

```rust
// Layer 3: Blob data (prunable)
const CF_DECIDED_BLOBS: &str = "decided_blobs";
const CF_UNDECIDED_BLOBS: &str = "undecided_blobs";
```

---

## 🔄 Data Flow Examples

### Proposer Flow

```rust
async fn handle_get_value(&mut self, height: Height, round: Round) -> Result<()> {
    // 1. Get execution payload + blobs from EL
    let (payload, blobs_bundle) = self.execution_client.get_payload_v3().await?;

    // 2. Build LAYER 1 metadata (Pure BFT)
    let consensus_metadata = ConsensusBlockMetadata {
        height,
        round,
        proposer: self.address,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        validator_set_hash: self.genesis.validator_set.hash(),
        execution_block_hash: payload.block_hash,
        gas_limit: payload.gas_limit,
        gas_used: payload.gas_used,
    };

    // 3. Build LAYER 2 metadata (Ethereum compat)
    let blob_metadata = if let Some(ref bundle) = blobs_bundle {
        BlobMetadata {
            height,
            parent_blob_root: self.last_blob_parent_root,
            kzg_commitments: bundle.commitments.clone(),
            blob_count: bundle.blobs.len() as u8,
            execution_state_root: payload.state_root,
            execution_block_hash: payload.block_hash,
        }
    } else {
        BlobMetadata::blobless(height, self.last_blob_parent_root, &payload)
    };

    // 4. Store metadata (both layers)
    self.store.put_consensus_block_metadata(&consensus_metadata).await?;
    self.store.put_undecided_blob_metadata(height, round.as_i64(), &blob_metadata).await?;

    // 5. Store LAYER 3 blobs (if any)
    if let Some(bundle) = blobs_bundle {
        let sidecars = self.build_sidecars(&blob_metadata, &bundle)?;
        self.blob_engine.verify_and_store(height, round.as_i64(), &sidecars).await?;
    }

    // 6. Build Value for consensus (lightweight metadata only)
    let value = Value::new(/* ... */);

    // 7. Stream proposal (consensus only sees Value, no Ethereum types)
    self.stream_proposal(value, payload_bytes, blobs_bundle);

    Ok(())
}
```

### Commit Flow

```rust
async fn handle_decided(&mut self, certificate: CommitCertificate) -> Result<()> {
    let height = certificate.height;
    let round = certificate.round;

    // 1. Mark blob metadata as decided (atomic promotion)
    self.store.mark_blob_metadata_decided(height, round.as_i64()).await?;

    // 2. Get decided metadata and update cache
    if let Some(blob_metadata) = self.store.get_blob_metadata(height).await? {
        self.last_blob_parent_root = blob_metadata.compute_blob_root();
        info!(
            %height, %round,
            blob_count = blob_metadata.blob_count,
            new_parent_root = %self.last_blob_parent_root,
            "Updated blob parent root"
        );
    }

    // 3. Mark blobs as decided in blob engine
    self.blob_engine.mark_decided(height, round.as_i64()).await?;

    // 4. Import to execution layer
    let blobs = self.blob_engine.get_for_import(height).await?;
    self.execution_client.import_block(payload, blobs).await?;

    Ok(())
}
```

### Building BlobSidecars (Ethereum Conversion)

```rust
fn build_sidecars(
    &self,
    blob_metadata: &BlobMetadata,
    bundle: &BlobsBundle,
) -> Result<Vec<BlobSidecar>> {
    // Convert to Ethereum format ONLY here
    let beacon_header = blob_metadata.to_beacon_header();
    let signed_header = SignedBeaconBlockHeader {
        message: beacon_header,
        signature: self.signing_provider.sign(&beacon_header.hash_tree_root()),
    };

    // Attach to each sidecar
    bundle.blobs.iter().enumerate().map(|(idx, blob)| {
        Ok(BlobSidecar {
            index: idx as u8,
            blob: blob.clone(),
            kzg_commitment: blob_metadata.kzg_commitments[idx],
            kzg_proof: bundle.proofs[idx],
            signed_block_header: signed_header.clone(),
            kzg_commitment_inclusion_proof: compute_proof(idx),
        })
    }).collect()
}
```

---

## ✅ Advantages Over Current Plan

### 1. Clean Separation of Concerns

| Layer | Concern | Naming |
|-------|---------|--------|
| Layer 1 | BFT consensus | Tendermint/Malachite (`height`, `round`, `proposer`) |
| Layer 2 | Ethereum compat | EIP-4844 (`kzg_commitments`, `parent_blob_root`) |
| Layer 3 | Data storage | Technology-neutral |

**Current plan**: Mixes Ethereum types (`SignedBeaconBlockHeader`) into consensus layer.

---

### 2. Technology Neutrality

**Today: Ethereum blobs**
```rust
blob_metadata: height → BlobMetadata {
    kzg_commitments,
    parent_blob_root,
}
```

**Tomorrow: Celestia DA** (just swap Layer 2)
```rust
celestia_metadata: height → CelestiaMetadata {
    namespace_id,
    share_commitments,
    data_root,
}
```

**Consensus layer (Layer 1) unchanged!** ✅

---

### 3. Proper BFT Naming Alignment

**Layer 1 uses pure BFT terminology**:
- ✅ `height` (not `slot`)
- ✅ `round` (not `epoch`)
- ✅ `proposer` (not `proposer_index`)
- ✅ `validator_set_hash` (not beacon state)
- ✅ `timestamp` (not slot time)

**No Ethereum terminology leaks into consensus.**

---

### 4. Storage Efficiency

| Layer | Size per Block | Retention | Storage at 1M Blocks |
|-------|---------------|-----------|----------------------|
| ConsensusBlockMetadata | ~200 bytes | Forever | 200 MB |
| BlobMetadata | ~300 bytes | Forever | 300 MB |
| Blob Data | ~786 KB | 30 days | ~23 GB (active window) |

**Total metadata kept forever**: ~500 MB per 1M blocks ✅

**Current plan**: Stores full `SignedBeaconBlockHeader` (~300+ bytes) in consensus.

---

### 5. Handles All Edge Cases

| Scenario | Current Plan | Three-Layer Plan |
|----------|-------------|------------------|
| Blobless blocks | Placeholder signature | `blob_count = 0`, empty commitments |
| Multi-round | Undecided storage | `blob_metadata_undecided` per round |
| Post-pruning | Headers survive | Both metadata layers survive |
| RestreamProposal | Fetch from undecided | Fetch from `blob_metadata_undecided` |
| Parent chain | Via headers | Via `parent_blob_root` in BlobMetadata |

Both handle edge cases, but three-layer is cleaner conceptually.

---

## 🚀 Revised Implementation Plan

### Phase 1: Core Types & Storage (6-7h)

**1.1 ConsensusBlockMetadata Type (2h)**
- File: `crates/types/src/consensus_block_metadata.rs` (NEW)
- Protobuf schema: `crates/types/proto/consensus.proto`
- Tests: Protobuf roundtrip, size verification

**1.2 BlobMetadata Type (2h)**
- File: `crates/types/src/blob_metadata.rs` (NEW)
- Protobuf schema: `crates/types/proto/blob.proto`
- Methods: `to_beacon_header()`, `compute_blob_root()`, `blobless()`
- Tests: Blobless creation, beacon header conversion, parent chain

**1.3 Storage Tables & Methods (2-3h)**
- Add tables to `crates/consensus/src/store.rs`
- Implement store methods with atomic promotion
- Idempotent writes (compare bytes)
- Big-endian key encoding

---

### Phase 2: State Integration (5-6h)

**2.1 Startup Hydration (1h)**
- `hydrate_blob_parent_root()` from `blob_metadata`
- `cleanup_stale_blob_metadata()` on startup

**2.2 Proposer Flow (2h)**
- Build both metadata layers
- Store before streaming
- Cache remains untouched

**2.3 Commit Flow (1h)**
- Atomic promotion of `blob_metadata`
- Update `last_blob_parent_root` cache

**2.4 RestreamProposal (1h)**
- Fetch from `blob_metadata_undecided`
- Rebuild sidecars with stored metadata

**2.5 Blobless Blocks (1h)**
- Use `BlobMetadata::blobless()`
- Verify parent chain continuity

---

### Phase 3: Tests (6h)

**Store Tests**:
- ConsensusBlockMetadata roundtrip
- BlobMetadata lifecycle (undecided → decided)
- Atomic promotion verification
- Multi-round isolation
- Blobless blocks

**State Tests**:
- Cache discipline (only updates on finalization)
- Parent-root chaining (blobbed + blobless)
- Startup cleanup

**Integration Tests**:
- Full proposal → decision → restart → next block
- Multi-validator network with blob sync
- Blobless block sandwich

---

### Phase 4: Cleanup & Docs (1h)

- Remove old Ethereum types from consensus
- Update CHANGELOG.md
- Document three-layer architecture
- Add metrics for metadata sizes

---

## 📊 Comparison Matrix

| Aspect | Current Plan (ConsensusBlobHeader) | Three-Layer Plan |
|--------|-------------------------------------|------------------|
| **Consensus Purity** | ❌ Stores Ethereum types | ✅ Pure BFT types only |
| **Naming** | ⚠️ Mixed (height + Ethereum header) | ✅ BFT-aligned (height, round, proposer) |
| **Technology Neutral** | ❌ Tied to Ethereum blobs | ✅ Can swap DA layers |
| **Storage Size** | ~300 bytes/block | ~500 bytes/block (two layers) |
| **Complexity** | ⚠️ One type (simpler) | ⚠️ Two types (more complex) |
| **Edge Cases** | ✅ All handled | ✅ All handled |
| **Ethereum Compat** | ✅ Direct wrapper | ✅ Via conversion layer |
| **Future Extensibility** | ❌ Hard to change | ✅ Easy to swap Layer 2 |

---

## ❓ Open Questions for Review

### 1. Is the added complexity worth it?

**Trade-off**: Two types vs. one type
- **Benefit**: Cleaner separation, technology neutrality
- **Cost**: More code, more protobuf schemas

### 2. Storage overhead acceptable?

**Difference**: ~500 bytes vs ~300 bytes per block
- **Extra cost**: 200 bytes × 1M blocks = 200 MB per million blocks
- **Benefit**: Clean layer separation

### 3. Migration path?

**Question**: Do we migrate existing data or start fresh?
- **Option A**: Wipe data (development phase, acceptable)
- **Option B**: Migrate from single-table to two-layer

### 4. Timeline impact?

**Current plan**: 18 hours
**Three-layer plan**: 18-20 hours (similar)

---

## 🎯 Recommendation

**Architecture Team to decide**:

1. **If prioritizing purity and extensibility** → Three-layer design
2. **If prioritizing simplicity and faster delivery** → Current plan (ConsensusBlobHeader)

Both designs are technically sound and handle all edge cases. The key difference is philosophical: do we want consensus to be pure BFT, or is wrapping Ethereum types acceptable?

---

## 📅 Review Checklist for Monday 27th

- [ ] Review three-layer architecture diagram
- [ ] Evaluate naming philosophy (BFT vs Ethereum terms)
- [ ] Assess storage overhead (~200 bytes extra per block)
- [ ] Consider future DA layer changes (Celestia, EigenDA)
- [ ] Decide on migration strategy
- [ ] Approve implementation plan OR stick with current plan
- [ ] Set target completion date

---

_**Prepared**: 2025-01-24_
_**Review Date**: 2025-01-27 (Monday)_
_**Status**: Awaiting Engineering Lead Approval_

