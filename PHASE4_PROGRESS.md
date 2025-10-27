# Phase 4: Blob Header Persistence — Implementation Progress

**Status**: 🟡 In Progress  
**Started**: 2025-01-XX  
**Target**: Production-ready blob header persistence (no blob-engine dependency)

---

## 🎯 Goals

- Persist every consensus-visible blob metadata record in the consensus store (remove blob-engine dependency).
- Keep the in-memory parent-root cache consistent even when rounds fail or nodes restart.
- Support multi-round proposals with clean header isolation.
- Maintain a continuous parent-root chain, including blobless blocks.
- Provide O(1) `get_latest_blob_metadata()` performance.

---

## 📐 Design Overview

We are adopting the **three-component metadata architecture** from the counterproposal.  
Instead of storing `SignedBeaconBlockHeader` directly in the consensus store, we split responsibilities into three horizontal abstractions:

1. **Consensus metadata (conceptual Layer 1)** – pure Malachite/Tendermint naming (`height`, `round`, `proposer`) with no Ethereum leakage.
2. **Blob metadata (conceptual Layer 2)** – Ethereum Deneb/EIP‑4844 bridge that can be swapped out for other DA formats.
3. **Blob store (conceptual Layer 3)** – existing RocksDB engine that keeps raw blobs and execution payload bytes on a prunable window.

These “layers” are conceptual only; they live side-by-side inside Ultramarine but with cleaner ownership boundaries.

### Storage Model (redb)

| Table                       | Purpose                                   | Key Format                     | Value                             |
|----------------------------|-------------------------------------------|--------------------------------|-----------------------------------|
| `consensus_block_metadata` | Canonical consensus info (kept forever)   | `height:u64` (BE)              | `ConsensusBlockMetadata` protobuf |
| `blob_metadata_undecided`  | Round-scoped blob metadata pre-finalize   | `(height:u64, round:i64)` (BE) | `BlobMetadata` protobuf           |
| `blob_metadata_decided`    | Finalized blob metadata (kept forever)    | `height:u64` (BE)              | `BlobMetadata` protobuf           |
| `blob_metadata_meta`       | Latest pointers / migration flags         | `b"latest_height"` etc.        | Small byte payloads               |

### Blob Store (RocksDB)

| Column Family         | Purpose                                      |
|-----------------------|----------------------------------------------|
| `undecided_blobs`     | Raw blobs keyed by `(height, round)`         |
| `decided_blobs`       | Raw blobs keyed by `height`                  |
| `execution_payloads`* | Optional column for prunable payload bytes   |

> *We can reuse existing decided/undecided block-data tables or add a dedicated column family; the pruning policy matches blobs.

### Metadata Types

```rust
/// Pure consensus-layer block metadata (Layer 1 abstraction)
pub struct ConsensusBlockMetadata {
    pub height: Height,
    pub round: Round,
    pub proposer: Address,
    pub timestamp: u64,
    pub validator_set_hash: B256,
    pub execution_block_hash: B256,
    pub gas_limit: u64,
    pub gas_used: u64,
}
```

```rust
/// Ethereum-facing blob metadata (Layer 2 abstraction)
pub struct BlobMetadata {
    pub height: Height,
    pub parent_blob_root: B256,
    pub kzg_commitments: Vec<KzgCommitment>,
    pub blob_count: u16,
    pub execution_state_root: B256,
    pub execution_block_hash: B256,
    pub proposer_index_hint: Option<u64>, // populated from Layer 1 when available
}
```

`BlobMetadata::to_beacon_header()` resolves the proposer index by combining the stored hint with the validator set. This keeps sidecar verification intact even though consensus no longer stores `SignedBeaconBlockHeader`.

### Header Lifecycle

```
┌─────────────────────────────────┐
│ UNDECIDED (height, round)       │  put_undecided_blob_metadata
│ • Written on propose/receive    │  • Idempotent write (compare bytes)
│ • Multiple rounds per height    │
└──────────────▲──────────────────┘
               │  mark_blob_metadata_decided (single WriteBatch)
               │   1. Read undecided (h,r)
               │   2. Write decided (h)
               │   3. Update latest pointer
               │   4. Delete undecided (h,r)
               │
┌──────────────┴──────────────────┐
│      DECIDED (height)           │  get_blob_metadata
│ • Exactly one canonical record  │  • Feeds parent-root & restarts
└─────────────────────────────────┘
```

### Cache Management (CRITICAL RULE)

`last_blob_parent_root` is updated **only** when metadata is canonical:

1. **Startup**: `hydrate_blob_parent_root()` loads the latest decided metadata (if any).  
2. **Finalization**: `commit()` promotes `(height, round)` metadata and refreshes the cache.

➡️ We do **not** mutate the cache during proposal or receive flows; failed rounds cannot corrupt the parent root.

### Restream & Recovery

- Restream pulls metadata via `store.get_undecided_blob_metadata(height, round)` with a decided fallback — no blob-engine dependency.
- `cleanup_stale_blob_metadata()` runs on startup to drop orphaned entries left behind by crashes/timeouts.
- Height 0 parent root is `B256::ZERO`; heights > 0 resolve the parent from the decided table (migration window may log warnings).

### Optional Migration Support

- Iterate decided heights.  
- For blobbed heights, derive `BlobMetadata` from existing headers + commitments and write into `blob_metadata_decided`.  
- Populate `consensus_block_metadata` from stored certificates / execution payload samples.  
- Update latest pointer flags in `blob_metadata_meta`.  
- Blobless heights use `BlobMetadata::blobless()` and will repopulate automatically after upgrade.  
- During migration, missing parents can log warnings instead of hard failures.

---

## 🚀 Implementation Roadmap

### Phase 1 – Core Types & Storage (est. 6h)

1. **ConsensusBlockMetadata type**  
   - [ ] Add `crates/types/src/consensus_block_metadata.rs`.  
   - [ ] Define protobuf schema (`crates/types/proto/consensus.proto`).  
   - [ ] Implement helpers (`hash()`, `proposer_index()`, etc.) and unit tests.  
   - [ ] Export from `crates/types/src/lib.rs`.

2. **BlobMetadata type**  
   - [ ] Add `crates/types/src/blob_metadata.rs`.  
   - [ ] Schema updates (`crates/types/proto/blob.proto`).  
   - [ ] `blob_count: u16`, `to_beacon_header()` using proposer-index hint + validator set.  
   - [ ] Helpers for `blobless()`, `compute_blob_root()`, and round-trip tests.

3. **Table definitions / initialization**  
   - [ ] Add `consensus_block_metadata`, `blob_metadata_undecided`, `blob_metadata_decided`, `blob_metadata_meta` tables to redb store.  
   - [ ] Ensure big-endian encoding for deterministic iteration.  
   - [ ] Introduce metadata-pointer helpers (latest height, migration flags).  
   - [ ] Confirm atomic write batches cover undecided→decided promotion.

4. **Store methods (idempotent + atomic)**  
   - [ ] `put_consensus_block_metadata`.  
   - [ ] `put/get/drop_undecided_blob_metadata`.  
   - [ ] `mark_blob_metadata_decided` (single write batch, updates metadata pointer).  
   - [ ] `get_blob_metadata`, `get_latest_blob_metadata`.  
   - [ ] `get_all_undecided_blob_metadata_before` for cleanup.  
   - [ ] Async wrappers + metrics updates.

### Phase 2 – State Integration (est. 5–6h)

1. **Startup hydration & cleanup**  
   - [ ] `hydrate_blob_parent_root()` seeds cache from decided metadata.  
   - [ ] `cleanup_stale_blob_metadata()` removes orphaned `(height, round)` entries (decided or beyond retention).

2. **Proposer flow**  
   - [ ] Build `ConsensusBlockMetadata` + `BlobMetadata` before streaming.  
   - [ ] Store consensus metadata and undecided blob metadata prior to emitting parts.  
   - [ ] Cache remains untouched.  
   - [ ] Continue with blob verification/storage and streaming using Layer 2 metadata.

3. **Receiver flow**  
   - [ ] After `verify_blob_sidecars`, persist metadata via `put_undecided_blob_metadata`.  
   - [ ] Blobless blocks call `BlobMetadata::blobless()`; no placeholder signatures needed.  
   - [ ] Cache unaffected.

4. **Restream path**  
   - [ ] Fetch metadata via `get_undecided_blob_metadata(height, proposal_round)` (fallback to decided).  
   - [ ] Rebuild sidecars with stored metadata and proposer-index hint.  
   - [ ] Abort with log if metadata missing.

5. **Commit flow**  
   - [ ] Promote metadata with `mark_blob_metadata_decided(height, round)` before blob-engine promotion.  
   - [ ] Refresh `last_blob_parent_root` from decided metadata and log new root.  
   - [ ] Persist consensus metadata alongside certificates as part of commit pipeline.

6. **Verification adjustments**  
   - [ ] Guard `height == 0` (parent = zero).  
   - [ ] Fetch parent from decided metadata; warn during migration if missing.  
   - [ ] Continue inclusion-proof, signature, commitment checks using new helpers.

7. **Round cleanup**  
   - [ ] Ensure timeout/round-drop paths call `drop_undecided_blob_metadata`.  
   - [ ] Integrate with pruning routines and blob-engine cleanup.

### Phase 3 – Tests (est. 6h)

1. **Store unit tests**  
   - [ ] Undecided roundtrip.  
   - [ ] Multi-round isolation.  
   - [ ] `mark_blob_metadata_decided` lifecycle (atomic promotion).  
   - [ ] `get_latest_blob_metadata()` performance (<10ms with 1k entries).  
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
- `get_latest_blob_metadata()` verified O(1).  
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
- [ ] Next: implement `ConsensusBlockMetadata` + `BlobMetadata` types (Phase 1).

---

## 🚀 Next Actions

1. Implement metadata types + protobufs (Phase 1).
2. Add new column families & table initialization (Phase 1.2).
3. Implement storage methods with idempotency + atomic promotion (Phase 1.3).

---
---

# ✅ Adopted Three-Component Metadata Architecture

**Status**: 🟢 Approved  
**Decision Date**: 2025-01-27 (architecture review)  
**Focus**: Implement separation of consensus metadata, blob metadata, and prunable blob storage.

---

## 📋 Executive Summary

This alternative design proposes a **three-layer architecture** that cleanly separates:
1. **Pure BFT consensus state** (Malachite/Tendermint naming, no Ethereum)
2. **Blob metadata** (Ethereum EIP-4844 compatibility bridge)
3. **Blob data storage** (prunable raw data)

**Key Difference from Legacy Plan**: Instead of storing `ConsensusBlobHeader` (which wraps `SignedBeaconBlockHeader`) in consensus, we store two separate metadata structures that clearly separate consensus concerns from Ethereum compatibility.

---

## 🎯 Design Philosophy

### Problem with Legacy Approach

The previous header-wrapper plan stored `ConsensusBlobHeader(SignedBeaconBlockHeader)` in the consensus store, which:
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
│ blob_metadata_decided:  height → BlobMetadata              │
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

## ✅ Advantages Over Legacy Plan

### 1. Clean Separation of Concerns

| Layer | Concern | Naming |
|-------|---------|--------|
| Layer 1 | BFT consensus | Tendermint/Malachite (`height`, `round`, `proposer`) |
| Layer 2 | Ethereum compat | EIP-4844 (`kzg_commitments`, `parent_blob_root`) |
| Layer 3 | Data storage | Technology-neutral |

**Legacy plan**: Mixed Ethereum types (`SignedBeaconBlockHeader`) into consensus layer.

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

**Legacy plan**: Stored full `SignedBeaconBlockHeader` (~300+ bytes) in consensus.

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

## 📊 Comparison Matrix (Legacy vs Adopted)

| Aspect | Legacy Header Wrapper | Adopted Three-Component Architecture |
|--------|-----------------------|--------------------------------------|
| **Consensus Purity** | ❌ Stores Ethereum types | ✅ Pure BFT types only |
| **Naming** | ⚠️ Mixed (height + Ethereum header) | ✅ BFT-aligned (height, round, proposer) |
| **Technology Neutral** | ❌ Tied to Ethereum blobs | ✅ Can swap DA layers |
| **Storage Size** | ~300 bytes/block | ~500 bytes/block (two metadata layers) |
| **Complexity** | ⚠️ Simpler (single type) | ⚠️ Extra protobuf + tables |
| **Edge Cases** | ✅ Handled | ✅ Handled |
| **Ethereum Compat** | ✅ Direct wrapper | ✅ Via conversion shim |
| **Future Extensibility** | ❌ Difficult | ✅ Straightforward |

---

## ✅ Resolved Considerations

- **Complexity vs. purity**: Extra protobuf/types accepted to keep consensus technology-neutral.  
- **Storage overhead**: +200 bytes/block is acceptable for the data chain roadmap.  
- **Migration**: Development phase allows either wipe or scripted import; documented in “Optional Migration Support”.  
- **Timeline**: Phase 4 scope increases slightly (≈20 h total) but unblocks downstream work once delivered.  
- **Sync behaviour**: Prunable payload/blob data stay out of sync snapshots; only Layers 1–2 replicate.

---

_**Prepared**: 2025-01-24_  
_**Updated**: 2025-01-27 (decision recorded)_  
_**Status**: Implementation pending (architecture locked)_
