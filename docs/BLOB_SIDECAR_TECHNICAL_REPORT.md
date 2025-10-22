# EIP-4844 Blob Sidecar Integration for Ultramarine Consensus Client
## Comprehensive Technical Review and Integration Plan

**Date:** October 8, 2025
**Version:** 1.0
**Status:** Technical Feasibility Analysis

---

## Executive Summary

This document provides a comprehensive technical analysis of integrating EIP-4844 blob sidecars into the Ultramarine consensus client, which is built on the Malachite BFT consensus library. The analysis covers:

1. **Current Architecture** - Deep dive into Ultramarine, Malachite, and reference implementations
2. **EIP-4844 Requirements** - Complete specification analysis for blob support
3. **Feasibility Assessment** - What's possible and what are the constraints
4. **Proposed Architecture** - Flexible, performant blob sidecar design
5. **Integration Plan** - Step-by-step implementation roadmap

**Key Finding:** Integration is **highly feasible**. Malachite's architecture was designed for exactly this use case - the streaming infrastructure, extensible Context trait system, and separate proposal parts channel provide excellent extension points for blob sidecars without requiring core consensus changes.

---

## Table of Contents

1. [Current Architecture Analysis](#1-current-architecture-analysis)
2. [EIP-4844 Requirements Deep Dive](#2-eip-4844-requirements-deep-dive)
3. [Feasibility Assessment](#3-feasibility-assessment)
4. [Proposed Blob Sidecar Architecture](#4-proposed-blob-sidecar-architecture)
5. [Integration Challenges and Solutions](#5-integration-challenges-and-solutions)
6. [Step-by-Step Integration Plan](#6-step-by-step-integration-plan)
7. [Performance Considerations](#7-performance-considerations)
8. [Testing Strategy](#8-testing-strategy)
9. [Appendix: Reference Implementations](#9-appendix-reference-implementations)

---

## 1. Current Architecture Analysis

### 1.1 Ultramarine Architecture Overview

**Core Components:**
```
┌─────────────────────────────────────────────────────────────┐
│                     Ultramarine Node                        │
├─────────────────────────────────────────────────────────────┤
│  ┌──────────────┐     ┌──────────────┐     ┌─────────────┐ │
│  │   CLI/Args   │────▶│   Runtime    │────▶│  Consensus  │ │
│  └──────────────┘     └──────────────┘     │    State    │ │
│                                             └─────────────┘ │
│                                                    │         │
│                                                    ▼         │
│  ┌──────────────────────────────────────────────────────┐  │
│  │           Malachite BFT Engine                       │  │
│  │  ┌──────────────┐  ┌───────────┐  ┌──────────────┐  │  │
│  │  │   Consensus  │  │  Network  │  │    Sync      │  │  │
│  │  │     Core     │  │   Layer   │  │   Protocol   │  │  │
│  │  └──────────────┘  └───────────┘  └──────────────┘  │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                    │         │
│                                                    ▼         │
│  ┌──────────────────────────────────────────────────────┐  │
│  │          Execution Layer Client                      │  │
│  │  ┌──────────────┐          ┌──────────────────────┐  │  │
│  │  │  Engine API  │          │    Eth RPC Client    │  │  │
│  │  │  (HTTP/IPC)  │          │      (HTTP only)     │  │  │
│  │  └──────────────┘          └──────────────────────┘  │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐  │
│  │          Persistent Storage (redb)                   │  │
│  │  • Certificates  • Values  • Proposals  • Blocks     │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

**Key Architectural Properties:**
- **Consensus Engine:** Malachite BFT (Tendermint-style) via `malachitebft-app-channel`
- **Execution Integration:** Dual endpoint strategy (Engine API + Eth RPC)
- **P2P Networking:** Fully delegated to Malachite (libp2p-based)
- **Storage:** redb embedded database with 5-table schema
- **Streaming:** Large payloads (>10MB) split into 128KB chunks for network transmission

**Current Engine API Support:**
- ✅ V3 (Cancun): `engine_newPayloadV3`, `engine_getPayloadV3`, `engine_forkchoiceUpdatedV3`
- ❌ V4 (Prague): Defined but commented out in capabilities
- ❌ Blob handling: No implementation (blueprint exists)

**File Locations:**
- Main application: `ultramarine/crates/node/src/app.rs`
- Execution client: `ultramarine/crates/execution/src/client.rs`
- Engine API: `ultramarine/crates/execution/src/engine_api/client.rs`
- Consensus state: `ultramarine/crates/consensus/src/state.rs`
- Streaming logic: `ultramarine/crates/consensus/src/streaming.rs`

### 1.2 Malachite BFT Deep Dive

**Context Trait System** (Extension Point #1):
```rust
pub trait Context {
    type Address: Address;
    type Height: Height;
    type ProposalPart: ProposalPart<Self>;    // ⭐ Key extension point
    type Proposal: Proposal<Self>;
    type Validator: Validator<Self>;
    type ValidatorSet: ValidatorSet<Self>;
    type Value: Value;                         // ⭐ Key extension point
    type Vote: Vote<Self>;
    type Extension: Extension;                 // ⭐ For vote extensions
    type SigningScheme: SigningScheme;
}
```

**Proposal Streaming Architecture:**

```
Proposer Side:
┌─────────────────────────────────────────────────────────┐
│  1. GetValue effect → Application builds value          │
│  2. Application splits value into ProposalParts         │
│  3. Each part wrapped in StreamMessage<ProposalPart>    │
│  4. Published to ProposalParts channel (separate!)      │
│  5. Last part is StreamContent::Fin                     │
└─────────────────────────────────────────────────────────┘
              │
              │ Network (libp2p gossip)
              ▼
Receiver Side:
┌─────────────────────────────────────────────────────────┐
│  1. ReceivedProposalPart host message                   │
│  2. Application stores part (keyed by StreamId)         │
│  3. When Fin received + all parts → Reconstruct         │
│  4. Validate complete value                             │
│  5. Return ProposedValue to consensus                   │
└─────────────────────────────────────────────────────────┘
```

**Network Channels** (Extension Point #2):
```rust
pub enum Channel {
    Consensus,      // Votes, full proposals (small data)
    ProposalParts,  // Streaming proposal parts (large data) ⭐
    Liveness,       // Polka/round certificates
    Sync,           // Status broadcasts, catch-up
}
```

**Key Architectural Benefits:**
1. **Separation of concerns:** Consensus doesn't know about blob internals
2. **Flexible part types:** `ProposalPart` trait is intentionally minimal
3. **Streaming infrastructure:** Already handles large data efficiently
4. **Independent channels:** Blobs flow separately from votes/consensus
5. **Extension points:** Vote extensions for blob attestations
6. **No core changes needed:** Extensions happen in application layer

**Malachite Files Reference:**
- Context trait: `malachite/code/crates/core-types/src/context.rs`
- Proposal parts: `malachite/code/crates/core-types/src/proposal_part.rs`
- Network layer: `malachite/code/crates/network/src/`
- Host interface: `malachite/code/crates/engine/src/host.rs`
- Streaming util: `malachite/code/crates/engine/src/util/streaming.rs`

### 1.3 Lighthouse Reference Implementation

Lighthouse provides a mature, production-grade blob sidecar implementation:

**Key Learnings:**
1. **Separate Storage Domain:** Blobs in dedicated `blobs_db` directory
2. **Multi-stage Verification Pipeline:** Cheap checks → Signature → KZG (batched)
3. **Availability Caching:** LRU cache for pending blocks awaiting blobs (~32 blocks capacity)
4. **Gossip Subnet Strategy:** One subnet per blob index (6 subnets total)
5. **Observation Pattern:** Track `(proposer, slot, index)` tuples for DoS prevention
6. **Coupling Strategy:** Never import blocks without required blobs (state machine: `MissingComponents` → `Available`)
7. **Pruning:** Based on data availability boundary (not just finalization)
8. **Performance:** Batch KZG verification is 5-10x faster than individual

**Lighthouse Files Reference:**
- Core types: `lighthouse/consensus/types/src/blob_sidecar.rs`
- Gossip validation: `lighthouse/beacon_node/beacon_chain/src/blob_verification.rs`
- Availability checker: `lighthouse/beacon_node/beacon_chain/src/data_availability_checker.rs`
- Storage: `lighthouse/beacon_node/store/src/hot_cold_store.rs`

---

## 2. EIP-4844 Requirements Deep Dive

### 2.1 Blob Structure and Size Limits

**Constants:**
```
FIELD_ELEMENTS_PER_BLOB = 4,096
BYTES_PER_FIELD_ELEMENT = 32
BYTES_PER_BLOB = 131,072 (128 KB)
BYTES_PER_COMMITMENT = 48 (KZG commitment)
BYTES_PER_PROOF = 48 (KZG proof)
MAX_BLOBS_PER_BLOCK = 6 (Deneb/Cancun) → 9 (Electra)
```

**BlobSidecar Structure:**
```rust
pub struct BlobSidecar {
    index: BlobIndex,              // 0-5 (or 0-8 in Electra)
    blob: Blob,                    // 131,072 bytes
    kzg_commitment: KzgCommitment, // 48 bytes
    kzg_proof: KzgProof,           // 48 bytes
    signed_block_header: SignedBeaconBlockHeader,
    kzg_commitment_inclusion_proof: Vector<Bytes32, 17>, // Merkle proof
}
```

**Total Size per Sidecar:** ~131.3 KB
**Maximum per Block:** ~768 KB (6 blobs) or ~1.15 MB (9 blobs in Electra)

### 2.2 Validation Requirements

**1. KZG Proof Verification:**
```python
# Individual verification
verify_blob_kzg_proof(blob: Blob, commitment: KZGCommitment, proof: KZGProof) -> bool

# Batch verification (recommended for performance)
verify_blob_kzg_proof_batch(blobs: List[Blob], commitments: List[KZGCommitment],
                            proofs: List[KZGProof]) -> bool
```

**2. Versioned Hash Derivation:**
```python
def kzg_commitment_to_versioned_hash(commitment: KZGCommitment) -> VersionedHash:
    return VERSIONED_HASH_VERSION_KZG + SHA256(commitment)[1:]
    # Version byte: 0x01 (KZG)
    # Followed by 31 bytes of SHA256 hash
```

**3. Inclusion Proof Verification:**
- Merkle proof depth: 17 levels
- Proves KZG commitment is in `BeaconBlockBody.blob_kzg_commitments`
- Validates against `signed_block_header.message.body_root`

**4. Block-Level Validation:**
- `len(blob_kzg_commitments) <= MAX_BLOBS_PER_BLOCK`
- Extract versioned hashes from blob transactions
- Match against CL-provided `expectedBlobVersionedHashes` in `engine_newPayloadV3`
- Order must match transaction inclusion order

### 2.3 Engine API V3/V4 Requirements

**ExecutionPayloadV3 (Cancun) - Current:**
```json
{
  // ... existing fields ...
  "blobGasUsed": "0x20000",      // NEW: Must be multiple of DATA_GAS_PER_BLOB
  "excessBlobGas": "0x0"         // NEW: For blob gas fee market
}
```

**engine_getPayloadV3 Response:**
```json
{
  "executionPayload": ExecutionPayloadV3,
  "blockValue": "0x...",
  "blobsBundle": {                // ⭐ NEW field
    "commitments": ["0x...", ...], // 48-byte KZG commitments
    "proofs": ["0x...", ...],      // 48-byte KZG proofs
    "blobs": ["0x...", ...]        // 131,072-byte blobs
  },
  "shouldOverrideBuilder": true
}
```

**engine_newPayloadV3 Request:**
```json
[
  executionPayload,                    // ExecutionPayloadV3
  expectedBlobVersionedHashes,         // ⭐ Array of 32-byte versioned hashes
  parentBeaconBlockRoot               // 32-byte root
]
```

**Validation Rules:**
1. Execution layer must extract versioned hashes from type-3 blob transactions
2. Compare against `expectedBlobVersionedHashes` (order matters!)
3. Return `INVALID` status if mismatch
4. All arrays in `blobsBundle` must be same length

**engine_getBlobsV1 (Optional but Recommended):**
```json
Request: ["0x01abcd...", "0x01ef12...", ...]  // Versioned hashes
Response: [
  { "blob": "0x...", "proof": "0x..." },
  null,  // Missing/pruned
  { "blob": "0x...", "proof": "0x..." }
]
```
- CL can fetch blobs from EL blob pool
- Maintains request order, returns `null` for missing
- Must support ≥128 blob requests

### 2.4 Consensus Layer Responsibilities

**1. Data Availability Check:**
```python
def is_data_available(block_root, commitments) -> bool:
    blobs, proofs = retrieve_blobs_and_proofs(block_root)
    return verify_blob_kzg_proof_batch(blobs, commitments, proofs)
```
- Block MUST NOT be considered valid until all blobs available
- Required in fork choice `on_block()` handler
- Previously validated blocks remain available even after pruning

**2. Blob Propagation:**
- Gossip via `blob_sidecar_{subnet_id}` topics (6 subnets)
- Each blob maps to subnet: `index % BLOB_SIDECAR_SUBNET_COUNT`
- Validate: signature, inclusion proof, KZG proof before gossip

**3. Storage and Retention:**
- Store blob sidecars for ≥4,096 epochs (~18 days)
- Serve blobs via sync protocol (`BlobSidecarsByRange`, `BlobSidecarsByRoot`)
- May prune after retention period

**4. Sync Protocol:**
- Request limits: Max 768 sidecars per request (128 blocks × 6 blobs)
- Serve range: `[max(current_epoch - 4096, DENEB_FORK_EPOCH), current_epoch]`
- Must backfill to serve full range

### 2.5 Execution Layer Responsibilities

**1. Blob Transaction Handling:**
- Validate type-3 blob transactions in mempool
- Extract versioned hashes: `tx.blob_versioned_hashes`
- Verify against CL-provided hashes in `newPayload`

**2. Blob Gas Accounting:**
- Track `blob_gas_used` per block (multiple of 131,072)
- Calculate `excess_blob_gas` for EIP-1559 style fee market
- Include in `ExecutionPayloadV3`

**3. Blob Pool Management:**
- Store blobs temporarily for pending transactions
- Serve via `engine_getBlobsV1`
- May prune after inclusion (implementation-dependent)

**4. Payload Building:**
- Include blob transactions based on gas price
- Return `blobsBundle` with commitments, proofs, blobs
- Ensure order matches transaction order

---

## 3. Feasibility Assessment

### 3.1 What's Already in Place ✅

1. **Streaming Infrastructure:**
   - Ultramarine already splits large payloads into 128KB chunks
   - Uses `ProposalPart` enum with `Init`, `Data`, `Fin` variants
   - Network handles streaming via separate `ProposalParts` channel
   - **Assessment:** Can directly extend for blob sidecars

2. **Malachite Extension Points:**
   - `Context` trait allows custom `ProposalPart` types
   - `Extension` trait for vote extensions (blob attestations)
   - Separate network channels prevent congestion
   - **Assessment:** Perfect fit for blob architecture

3. **Engine API Foundation:**
   - V3 methods implemented: `newPayloadV3`, `getPayloadV3`, `forkchoiceUpdatedV3`
   - Transport layer supports HTTP and IPC
   - JWT authentication working
   - **Assessment:** Need to extend for blob handling

4. **Storage Layer:**
   - redb provides performant key-value storage
   - Already has table structure for proposals and blocks
   - Pruning logic in place (keeps last 5 heights)
   - **Assessment:** Can add blob-specific tables

5. **Consensus State Management:**
   - Tracks height, round, proposer
   - Handles peer join/leave events
   - Manages proposal assembly and validation
   - **Assessment:** Clear extension points for blob logic

### 3.2 What Needs to Be Built 🔨

1. **ProposalPart Extension:**
   ```rust
   pub enum ProposalPart {
       Init(ProposalInit),           // Existing
       Data(ProposalData),            // Existing
       BlobSidecar(BlobSidecar),      // 🆕 NEW
       BlobCommitments(BlobBundle),   // 🆕 NEW (optional optimization)
       Fin(ProposalFin),              // Existing
   }
   ```

2. **Engine API V3 Blob Support:**
   - Modify `engine_getPayloadV3` to capture `blobsBundle`
   - Extract versioned hashes from payload
   - Pass versioned hashes to `engine_newPayloadV3`
   - Optionally implement `engine_getBlobsV1` for fast path

3. **Blob Storage:**
   ```rust
   // New redb tables
   BLOB_SIDECARS_TABLE: (Height, Round, BlobIndex) -> BlobSidecar
   BLOB_COMMITMENTS_TABLE: (Height, Round) -> Vec<KzgCommitment>
   ```

4. **KZG Verification:**
   - Integrate c-kzg library (Rust bindings: `c-kzg-rs`)
   - Implement batch verification for performance
   - Load trusted setup at startup

5. **Data Availability Logic:**
   - Track pending proposals awaiting blobs
   - Implement coupling logic (block + blobs → import)
   - Handle race conditions (blob arrives before block)

6. **Pruning Extension:**
   - Prune blob sidecars beyond retention period
   - Configurable retention (default: 4,096 epochs equivalent)
   - Separate from block pruning logic

### 3.3 Constraints and Trade-offs ⚖️

**1. P2P Networking Dependency:**
- **Constraint:** No direct control over Malachite's network layer
- **Impact:** Cannot implement custom gossip subnets like Lighthouse
- **Solution:** Use existing `ProposalParts` channel, rely on Malachite's load balancing
- **Trade-off:** Less fine-grained control, but simpler architecture

**2. Consensus Model Differences:**
- **Constraint:** Malachite uses (height, round) not slot-based timing
- **Impact:** Different finalization and availability semantics
- **Solution:** Map epochs to height ranges, define retention in terms of heights
- **Trade-off:** Cannot directly copy Ethereum's epoch-based retention

**3. No Native Subnet Support:**
- **Constraint:** Malachite doesn't have per-blob-index subnets
- **Impact:** All blobs flow through same channel (higher bandwidth per peer)
- **Solution:** Rely on streaming to handle bandwidth, consider future Malachite extension
- **Trade-off:** Higher per-peer bandwidth, but simpler for MVP

**4. Single-Threaded App Loop:**
- **Constraint:** Ultramarine's app runs in single tokio task
- **Impact:** KZG verification blocks consensus if done inline
- **Solution:** Spawn verification tasks, return results via channel
- **Trade-off:** Adds complexity but necessary for performance

**5. Storage Size:**
- **Constraint:** Each blob sidecar is ~131 KB, 6 per block
- **Impact:** ~768 KB per height, ~3.3 GB for 4,096 heights
- **Solution:** Implement aggressive pruning, configurable retention
- **Trade-off:** Less historical data, but manageable storage

### 3.4 Performance Characteristics 📊

**Blob Sizes:**
- Single blob: 131,072 bytes
- 6 blobs: 786,432 bytes (~768 KB)
- With commitments/proofs: ~789 KB per block

**Network Bandwidth:**
- Assuming 5-second block time: ~157 KB/s steady state (6 blobs)
- Peak (if all 9 blobs in Electra): ~230 KB/s
- **Assessment:** Well within modern network capabilities

**KZG Verification Times (from Lighthouse benchmarks):**
- Single blob verification: ~5-10 ms
- Batch verification (6 blobs): ~15-20 ms total (~2.5-3 ms per blob)
- **Assessment:** Batch verification is critical

**Storage Growth:**
- 6 blobs/block × 131 KB × 7,200 blocks/day ≈ 5.5 GB/day
- With 18-day retention: ~100 GB blob storage
- **Assessment:** Significant but manageable with pruning

**Streaming Performance:**
- 128 KB chunks (existing): 1 chunk per blob
- Network latency: ~1 RTT per part for small proposals
- For 6 blobs: ~6 parts minimum (if 1 blob per part)
- **Assessment:** Existing streaming handles this well

### 3.5 Feasibility Conclusion ✅

**Overall Assessment: HIGHLY FEASIBLE**

**Strengths:**
1. ✅ Malachite architecture is **ideally suited** for blob sidecars
2. ✅ Streaming infrastructure **already exists** and is proven
3. ✅ Extension points are **clean and well-defined**
4. ✅ No core consensus changes required
5. ✅ Reference implementations (Lighthouse) provide clear patterns

**Challenges:**
1. ⚠️ KZG verification integration (new dependency)
2. ⚠️ Performance optimization required (batch verification, async)
3. ⚠️ Storage management (pruning, separate blob DB)
4. ⚠️ Engine API extension for blob bundle handling

**Risk Level: LOW to MEDIUM**
- Technical risks are well-understood
- Architecture supports the changes cleanly
- Main risk is complexity of KZG crypto integration
- Lighthouse provides proven reference implementation

---

## 4. Proposed Blob Sidecar Architecture

### 4.1 High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Ultramarine Node                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌────────────────────────────────────────────────────────┐    │
│  │              Consensus Application Layer                │    │
│  │                                                          │    │
│  │  ┌──────────────────────────────────────────────────┐  │    │
│  │  │         Blob Availability Checker                 │  │    │
│  │  │  • Tracks pending proposals                       │  │    │
│  │  │  • Couples blocks + blobs                         │  │    │
│  │  │  • State: MissingBlobs → Available                │  │    │
│  │  └──────────────────────────────────────────────────┘  │    │
│  │                          │                              │    │
│  │  ┌────────────────┐     │      ┌────────────────────┐ │    │
│  │  │  Part Assembly │◀────┴─────▶│  KZG Verification  │ │    │
│  │  │   • BlobStore  │             │   • Batch verify   │ │    │
│  │  │   • Streaming  │             │   • Async workers  │ │    │
│  │  └────────────────┘             └────────────────────┘ │    │
│  └──────────────────────────────────────────────────────────┘  │
│                          │                   │                  │
│                          ▼                   ▼                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │              Malachite BFT Engine                        │  │
│  │  ┌──────────────┐  ┌─────────────┐  ┌────────────────┐  │  │
│  │  │   Consensus  │  │ProposalParts│  │     Sync       │  │  │
│  │  │     Core     │  │   Channel   │  │   Protocol     │  │  │
│  │  └──────────────┘  └─────────────┘  └────────────────┘  │  │
│  └──────────────────────────────────────────────────────────┘  │
│                          │                   │                  │
│                          ▼                   ▼                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │              Execution Layer Client                      │  │
│  │  ┌────────────────────────────────────────────────────┐  │  │
│  │  │  Engine API with Blob Support                      │  │  │
│  │  │  • getPayloadV3 → capture blobsBundle              │  │  │
│  │  │  • newPayloadV3 → pass versioned hashes            │  │  │
│  │  │  • getBlobsV1 → fast path from EL pool (optional)  │  │  │
│  │  └────────────────────────────────────────────────────┘  │  │
│  └──────────────────────────────────────────────────────────┘  │
│                          │                                      │
│                          ▼                                      │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │               Persistent Storage                         │  │
│  │  ┌────────────────┐  ┌────────────────┐                 │  │
│  │  │   Block Store  │  │   Blob Store   │                 │  │
│  │  │   (existing)   │  │     (new)      │                 │  │
│  │  │  • Certs       │  │  • Sidecars    │                 │  │
│  │  │  • Values      │  │  • Commitments │                 │  │
│  │  │  • Proposals   │  │  • LRU Cache   │                 │  │
│  │  └────────────────┘  └────────────────┘                 │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 4.2 Extended ProposalPart Enum

```rust
/// Extended ProposalPart enum with blob sidecar support
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalPart {
    /// Proposal metadata: height, round, proposer, value_id
    Init(ProposalInit),

    /// Execution payload header (block header without transactions)
    Header(ExecutionPayloadHeader),

    /// Transaction data chunk (existing streaming)
    Transactions(TransactionBatch),

    /// 🆕 NEW: Blob sidecar with full verification data
    BlobSidecar {
        index: u64,                      // 0-5 (or 0-8 in Electra)
        blob: Blob,                      // 131,072 bytes
        kzg_commitment: KzgCommitment,   // 48 bytes
        kzg_proof: KzgProof,             // 48 bytes
    },

    /// 🆕 NEW (Optional): Blob commitments bundle for early verification
    /// Sent before individual blobs to allow early validation
    BlobCommitmentsBundle {
        commitments: Vec<KzgCommitment>,
        count: u8,  // Number of blobs to expect
    },

    /// Final part with signature over all previous parts
    Fin(ProposalFin),
}

impl ProposalPart {
    fn is_first(&self) -> bool {
        matches!(self, ProposalPart::Init(_))
    }

    fn is_last(&self) -> bool {
        matches!(self, ProposalPart::Fin(_))
    }
}
```

### 4.3 Blob Availability Checker

```rust
/// State machine for tracking proposals awaiting blobs
pub struct BlobAvailabilityChecker {
    /// Pending proposals awaiting blob completion
    pending: LruCache<ProposalId, PendingProposal>,

    /// KZG verification worker pool
    kzg_workers: KzgVerificationPool,

    /// Configuration
    config: AvailabilityConfig,
}

/// State of a proposal in the availability checker
pub enum ProposalState {
    /// Missing blob sidecars
    MissingBlobs {
        header: ExecutionPayloadHeader,
        transactions: Vec<Transaction>,
        received_blobs: HashMap<u64, BlobSidecar>,  // index -> sidecar
        expected_count: u8,
    },

    /// All blobs received, KZG verification in progress
    VerifyingBlobs {
        complete_proposal: CompleteProposal,
        verification_task: JoinHandle<Result<(), KzgError>>,
    },

    /// Fully available and verified
    Available {
        proposal: VerifiedProposal,
        timestamp: Instant,
    },
}

pub struct PendingProposal {
    height: Height,
    round: Round,
    proposer: Address,
    state: ProposalState,
}

impl BlobAvailabilityChecker {
    /// Add a proposal part to the checker
    pub fn add_part(
        &mut self,
        proposal_id: ProposalId,
        part: ProposalPart,
    ) -> Result<Option<VerifiedProposal>, AvailabilityError> {
        // Update state machine
        // If all parts received → spawn KZG verification
        // If verification complete → return VerifiedProposal
    }

    /// Check if proposal is ready (all blobs available and verified)
    pub fn is_available(&self, proposal_id: &ProposalId) -> bool {
        matches!(
            self.pending.get(proposal_id).map(|p| &p.state),
            Some(ProposalState::Available { .. })
        )
    }

    /// Prune proposals older than retention period
    pub fn prune(&mut self, before_height: Height) {
        self.pending.retain(|_, proposal| proposal.height >= before_height);
    }
}
```

### 4.4 KZG Verification Pool

```rust
/// Asynchronous KZG verification worker pool
pub struct KzgVerificationPool {
    /// Trusted setup (loaded once at startup)
    kzg_settings: Arc<KzgSettings>,

    /// Worker thread pool (size = num_cpus)
    workers: Arc<ThreadPool>,

    /// Verification request channel
    tx: mpsc::Sender<VerificationRequest>,
    rx: mpsc::Receiver<VerificationResult>,
}

pub struct VerificationRequest {
    proposal_id: ProposalId,
    blobs: Vec<Blob>,
    commitments: Vec<KzgCommitment>,
    proofs: Vec<KzgProof>,
}

pub struct VerificationResult {
    proposal_id: ProposalId,
    result: Result<(), KzgError>,
    duration: Duration,
}

impl KzgVerificationPool {
    /// Spawn a batch verification task
    pub fn verify_batch(&self, req: VerificationRequest) -> JoinHandle<VerificationResult> {
        let settings = Arc::clone(&self.kzg_settings);

        tokio::task::spawn_blocking(move || {
            let start = Instant::now();

            // Use batch verification for better performance
            let result = c_kzg::KzgProof::verify_blob_kzg_proof_batch(
                &req.blobs,
                &req.commitments,
                &req.proofs,
                &settings,
            );

            VerificationResult {
                proposal_id: req.proposal_id,
                result: result.map_err(|e| KzgError::VerificationFailed(e)),
                duration: start.elapsed(),
            }
        })
    }
}
```

### 4.5 Blob Storage Schema

```rust
/// Blob storage with redb backend
pub struct BlobStore {
    db: Arc<Database>,
    cache: Arc<Mutex<LruCache<BlobId, Arc<BlobSidecar>>>>,
}

/// Blob identifier
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct BlobId {
    height: Height,
    round: Round,
    index: u64,
}

/// redb table definitions
const BLOB_SIDECARS_TABLE: TableDefinition<(u64, u32, u64), &[u8]> =
    TableDefinition::new("blob_sidecars");

const BLOB_COMMITMENTS_TABLE: TableDefinition<(u64, u32), &[u8]> =
    TableDefinition::new("blob_commitments");

impl BlobStore {
    /// Store a blob sidecar
    pub fn store_sidecar(
        &self,
        height: Height,
        round: Round,
        sidecar: BlobSidecar,
    ) -> Result<(), StorageError> {
        let blob_id = BlobId { height, round, index: sidecar.index };

        // Update cache
        self.cache.lock().unwrap().put(blob_id, Arc::new(sidecar.clone()));

        // Persist to disk
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(BLOB_SIDECARS_TABLE)?;
            let key = (height.as_u64(), round.as_u32(), sidecar.index);
            let value = bincode::serialize(&sidecar)?;
            table.insert(key, value.as_slice())?;
        }
        write_txn.commit()?;

        Ok(())
    }

    /// Retrieve a blob sidecar
    pub fn get_sidecar(
        &self,
        height: Height,
        round: Round,
        index: u64,
    ) -> Result<Option<Arc<BlobSidecar>>, StorageError> {
        let blob_id = BlobId { height, round, index };

        // Check cache first
        if let Some(sidecar) = self.cache.lock().unwrap().get(&blob_id) {
            return Ok(Some(Arc::clone(sidecar)));
        }

        // Fall back to disk
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(BLOB_SIDECARS_TABLE)?;
        let key = (height.as_u64(), round.as_u32(), index);

        if let Some(value) = table.get(key)? {
            let sidecar: BlobSidecar = bincode::deserialize(value.value())?;
            let sidecar = Arc::new(sidecar);

            // Update cache
            self.cache.lock().unwrap().put(blob_id, Arc::clone(&sidecar));

            Ok(Some(sidecar))
        } else {
            Ok(None)
        }
    }

    /// Prune blobs before a given height
    pub fn prune_before(&self, height: Height) -> Result<u64, StorageError> {
        let write_txn = self.db.begin_write()?;
        let mut count = 0;

        {
            let mut table = write_txn.open_table(BLOB_SIDECARS_TABLE)?;
            let to_delete: Vec<_> = table
                .iter()?
                .filter_map(|entry| {
                    let (key, _) = entry.ok()?;
                    let (h, r, i) = key.value();
                    if h < height.as_u64() {
                        Some((h, r, i))
                    } else {
                        None
                    }
                })
                .collect();

            for key in to_delete {
                table.remove(key)?;
                count += 1;
            }
        }

        write_txn.commit()?;

        // Clear cache entries
        self.cache.lock().unwrap().clear();

        Ok(count)
    }
}
```

### 4.6 Engine API Blob Integration

```rust
/// Extended Engine API trait with blob support
#[async_trait]
pub trait EngineApiWithBlobs: EngineApi {
    /// Get payload with blobs bundle (V3)
    async fn get_payload_with_blobs(
        &self,
        payload_id: PayloadId,
    ) -> Result<PayloadWithBlobs, EngineError>;

    /// New payload with versioned hashes (V3)
    async fn new_payload_with_blobs(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<VersionedHash>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, EngineError>;

    /// Optional: Get blobs from execution layer pool
    async fn get_blobs_from_pool(
        &self,
        versioned_hashes: Vec<VersionedHash>,
    ) -> Result<Vec<Option<BlobAndProof>>, EngineError>;
}

pub struct PayloadWithBlobs {
    pub execution_payload: ExecutionPayloadV3,
    pub block_value: U256,
    pub blobs_bundle: Option<BlobsBundle>,
    pub should_override_builder: bool,
}

pub struct BlobsBundle {
    pub commitments: Vec<KzgCommitment>,
    pub proofs: Vec<KzgProof>,
    pub blobs: Vec<Blob>,
}

impl BlobsBundle {
    /// Validate that all arrays have the same length
    pub fn validate(&self) -> Result<(), ValidationError> {
        let len = self.commitments.len();
        if self.proofs.len() != len || self.blobs.len() != len {
            return Err(ValidationError::MismatchedLengths {
                commitments: self.commitments.len(),
                proofs: self.proofs.len(),
                blobs: self.blobs.len(),
            });
        }

        if len > MAX_BLOBS_PER_BLOCK {
            return Err(ValidationError::TooManyBlobs(len));
        }

        Ok(())
    }

    /// Convert to individual blob sidecars
    pub fn into_sidecars(self) -> Vec<BlobSidecar> {
        self.blobs
            .into_iter()
            .zip(self.commitments)
            .zip(self.proofs)
            .enumerate()
            .map(|(index, ((blob, commitment), proof))| BlobSidecar {
                index: index as u64,
                blob,
                kzg_commitment: commitment,
                kzg_proof: proof,
            })
            .collect()
    }
}
```

### 4.7 Application Flow Integration

**Proposer Path (when building new block):**

```rust
// In app.rs, AppMsg::GetValue handler
async fn handle_get_value(
    state: &mut State,
    execution_layer: &ExecutionLayer,
    height: Height,
    round: Round,
) -> Result<ProposedValue> {
    // 1. Request payload with blobs from execution layer
    let payload_with_blobs = execution_layer
        .get_payload_with_blobs(payload_id)
        .await?;

    // 2. Extract execution payload and blobs bundle
    let execution_payload = payload_with_blobs.execution_payload;
    let blobs_bundle = payload_with_blobs.blobs_bundle;

    // 3. Validate blobs bundle
    if let Some(bundle) = &blobs_bundle {
        bundle.validate()?;

        // Verify KZG proofs locally before proposing
        state.kzg_pool.verify_batch(VerificationRequest {
            proposal_id: (height, round).into(),
            blobs: bundle.blobs.clone(),
            commitments: bundle.commitments.clone(),
            proofs: bundle.proofs.clone(),
        }).await?;
    }

    // 4. Create proposal parts
    let mut parts = vec![
        ProposalPart::Init(ProposalInit {
            height,
            round,
            proposer: state.address,
            value_id: compute_value_id(&execution_payload, &blobs_bundle),
        }),
        ProposalPart::Header(execution_payload.header()),
    ];

    // 5. Add transaction chunks (existing logic)
    for tx_chunk in chunk_transactions(&execution_payload.transactions, CHUNK_SIZE) {
        parts.push(ProposalPart::Transactions(tx_chunk));
    }

    // 6. 🆕 Add blob sidecars (if present)
    if let Some(bundle) = blobs_bundle {
        for (index, ((blob, commitment), proof)) in bundle.blobs
            .into_iter()
            .zip(bundle.commitments)
            .zip(bundle.proofs)
            .enumerate()
        {
            parts.push(ProposalPart::BlobSidecar {
                index: index as u64,
                blob,
                kzg_commitment: commitment,
                kzg_proof: proof,
            });
        }
    }

    // 7. Add final signature
    parts.push(ProposalPart::Fin(ProposalFin {
        signature: sign_all_parts(&parts, &state.signing_provider),
    }));

    // 8. Stream all parts via network
    for (seq, part) in parts.into_iter().enumerate() {
        let stream_msg = StreamMessage {
            stream_id: state.next_stream_id(),
            sequence: seq as u64,
            content: StreamContent::Data(part),
        };
        state.channels.network.send(NetworkMsg::PublishProposalPart(stream_msg))?;
    }

    Ok(ProposedValue { height, round, ... })
}
```

**Receiver Path (when receiving proposal parts):**

```rust
// In app.rs, AppMsg::ReceivedProposalPart handler
async fn handle_received_proposal_part(
    state: &mut State,
    from: PeerId,
    part: StreamMessage<ProposalPart>,
) -> Result<Option<ProposedValue>> {
    let stream_id = part.stream_id;

    match part.content {
        StreamContent::Data(ProposalPart::Init(init)) => {
            // Start new proposal assembly
            state.availability_checker.start_proposal(stream_id, init)?;
        }

        StreamContent::Data(ProposalPart::BlobSidecar { index, blob, kzg_commitment, kzg_proof }) => {
            // 🆕 Store blob sidecar
            let sidecar = BlobSidecar { index, blob, kzg_commitment, kzg_proof };

            // Add to availability checker
            state.availability_checker.add_blob_sidecar(stream_id, sidecar)?;

            // Check if all blobs received
            if state.availability_checker.all_blobs_received(stream_id) {
                // Spawn KZG verification task
                let proposal = state.availability_checker.get_pending_proposal(stream_id)?;

                state.kzg_pool.verify_batch(VerificationRequest {
                    proposal_id: proposal.id,
                    blobs: proposal.blobs.clone(),
                    commitments: proposal.commitments.clone(),
                    proofs: proposal.proofs.clone(),
                })?;
            }
        }

        StreamContent::Fin => {
            // Verify signature over all parts
            verify_proposal_signature(stream_id, &state.streams_map)?;

            // Wait for KZG verification to complete (if in progress)
            if let Some(verified_proposal) = state.availability_checker.await_verification(stream_id).await? {
                // Proposal is fully available and verified!
                return Ok(Some(verified_proposal));
            }
        }

        _ => {
            // Handle other parts (Header, Transactions, etc.)
            state.streams_map.add_part(stream_id, part)?;
        }
    }

    Ok(None)
}
```

**Commit Path (when deciding on a proposal):**

```rust
// In app.rs, AppMsg::Decided handler
async fn handle_decided(
    state: &mut State,
    execution_layer: &ExecutionLayer,
    certificate: CommitCertificate,
) -> Result<()> {
    let height = certificate.height;
    let round = certificate.round;

    // 1. Retrieve complete proposal (with blobs)
    let proposal = state.availability_checker
        .get_verified_proposal(height, round)?
        .ok_or(ConsensusError::MissingProposal)?;

    // 2. Extract execution payload
    let execution_payload = proposal.execution_payload;

    // 3. 🆕 Compute versioned hashes from blob commitments
    let versioned_hashes: Vec<VersionedHash> = proposal.blob_sidecars
        .iter()
        .map(|sidecar| kzg_commitment_to_versioned_hash(&sidecar.kzg_commitment))
        .collect();

    // 4. Send newPayloadV3 with versioned hashes
    let payload_status = execution_layer
        .new_payload_with_blobs(
            execution_payload,
            versioned_hashes,
            proposal.parent_beacon_block_root,
        )
        .await?;

    // 5. Validate payload status
    if !payload_status.is_valid() {
        return Err(ExecutionError::InvalidPayload(payload_status));
    }

    // 6. Update fork choice
    execution_layer.set_latest_forkchoice_state(proposal.block_hash).await?;

    // 7. 🆕 Persist blob sidecars to storage
    for sidecar in proposal.blob_sidecars {
        state.blob_store.store_sidecar(height, round, sidecar)?;
    }

    // 8. Persist block data (existing logic)
    state.store.store_decided_value(height, &proposal)?;
    state.store.store_certificate(height, &certificate)?;

    // 9. 🆕 Prune old blobs
    let retention_height = height.saturating_sub(state.config.blob_retention_heights);
    state.blob_store.prune_before(retention_height)?;

    // 10. Move to next height
    state.current_height = height.increment();

    Ok(())
}
```

### 4.8 Configuration

```rust
/// Blob-related configuration
#[derive(Clone, Debug)]
pub struct BlobConfig {
    /// Enable blob support
    pub enabled: bool,

    /// Blob retention period (in heights)
    /// Equivalent to Ethereum's 4,096 epochs (~18 days)
    pub retention_heights: u64,

    /// Blob cache size (number of recent blob sidecars to cache)
    pub cache_size: usize,

    /// Number of KZG verification worker threads
    pub kzg_workers: usize,

    /// Path to KZG trusted setup file
    pub trusted_setup_path: PathBuf,

    /// Maximum blobs per block (6 for Deneb, 9 for Electra)
    pub max_blobs_per_block: u8,
}

impl Default for BlobConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_heights: 28_800,  // ~4096 epochs equivalent (assuming 5s blocks)
            cache_size: 32,             // Cache last ~32 proposals worth of blobs
            kzg_workers: num_cpus::get(),
            trusted_setup_path: PathBuf::from("./assets/trusted_setup.txt"),
            max_blobs_per_block: 6,
        }
    }
}
```

---

## 5. Integration Challenges and Solutions

### 5.1 Challenge: KZG Cryptography Integration

**Problem:**
- KZG proofs require BLS12-381 elliptic curve operations
- Trusted setup (KZG parameters) must be loaded
- Verification is computationally expensive (5-10ms per blob)
- C library (`c-kzg`) must be integrated into Rust

**Solution:**

1. **Use `c-kzg` Rust Bindings:**
   ```toml
   [dependencies]
   c-kzg = "1.0"  # Official Rust bindings
   ```

2. **Load Trusted Setup at Startup:**
   ```rust
   pub fn load_trusted_setup(path: &Path) -> Result<KzgSettings> {
       let setup_bytes = std::fs::read(path)?;
       let settings = KzgSettings::load_trusted_setup(&setup_bytes)?;
       Ok(settings)
   }
   ```

3. **Implement Batch Verification:**
   ```rust
   pub fn verify_blob_batch(
       blobs: &[Blob],
       commitments: &[KzgCommitment],
       proofs: &[KzgProof],
       settings: &KzgSettings,
   ) -> Result<bool> {
       // Use batch API for 5-10x speedup
       c_kzg::KzgProof::verify_blob_kzg_proof_batch(
           blobs,
           commitments,
           proofs,
           settings,
       )
   }
   ```

4. **Async Verification with Worker Pool:**
   ```rust
   // Spawn verification in background thread pool
   let handle = tokio::task::spawn_blocking(move || {
       verify_blob_batch(&blobs, &commitments, &proofs, &settings)
   });
   ```

**Risk Mitigation:**
- ✅ `c-kzg` is battle-tested (used by Ethereum clients)
- ✅ Trusted setup is standardized (same file as Ethereum)
- ✅ Batch verification significantly reduces overhead
- ✅ Async verification prevents blocking consensus

### 5.2 Challenge: P2P Network Bandwidth

**Problem:**
- 6 blobs × 131 KB = 768 KB per block
- All blobs flow through same `ProposalParts` channel
- No per-blob-index subnets like Ethereum
- Higher per-peer bandwidth compared to Lighthouse

**Solution:**

1. **Leverage Existing Streaming:**
   - 128 KB chunks already proven to work
   - Each blob fits in ~1 chunk
   - Network layer handles backpressure

2. **Optimize Chunk Size for Blobs:**
   ```rust
   // Align chunk size with blob size for efficiency
   const BLOB_CHUNK_SIZE: usize = 131_072;  // Exact blob size
   ```

3. **Implement Blob Caching:**
   - Cache verified blobs in LRU cache
   - Avoid re-transmitting to peers who already have them
   - Use bloom filters to track "what each peer has"

4. **Consider Compression (Future):**
   - Blobs may compress well (especially if sparse)
   - Could reduce bandwidth by 20-50%
   - Trade-off: CPU cost for compression/decompression

**Bandwidth Analysis:**
- 768 KB per 5-second block = 153 KB/s steady state
- Modern networks: 100 Mbps = 12.5 MB/s
- Blob bandwidth: ~1.2% of available capacity
- **Conclusion:** Not a bottleneck for foreseeable future

### 5.3 Challenge: Storage Management

**Problem:**
- 768 KB per block × 5,760 blocks/day = 4.4 GB/day
- 18-day retention = ~79 GB blob storage
- Separate from block storage (different lifecycle)
- Need efficient indexing and pruning

**Solution:**

1. **Separate Blob Database:**
   ```rust
   // Use dedicated redb database for blobs
   let blob_db_path = data_dir.join("blobs_db");
   let blob_db = Database::create(blob_db_path)?;
   ```

2. **Implement Aggressive Pruning:**
   ```rust
   // Prune on every finalized height
   if height % PRUNE_INTERVAL == 0 {
       let cutoff = height.saturating_sub(RETENTION_HEIGHTS);
       blob_store.prune_before(cutoff)?;
   }
   ```

3. **LRU Cache for Hot Data:**
   ```rust
   // Keep recent blobs in memory
   let cache = LruCache::new(BLOB_CACHE_SIZE);  // e.g., 32 proposals
   ```

4. **Configurable Retention:**
   ```toml
   [blob]
   retention_heights = 28800  # ~18 days
   prune_interval = 100       # Prune every 100 heights
   ```

5. **Optional: Separate Disk Volume:**
   - Allow blob storage on separate disk (SSD vs HDD)
   - Configurable path: `--blob-db-path /mnt/blobs`

**Storage Growth Mitigation:**
- ✅ Pruning keeps storage bounded (~80-100 GB max)
- ✅ Can reduce retention for lower storage (e.g., 7 days = ~30 GB)
- ✅ Future: Blob compression could reduce by 20-50%

### 5.4 Challenge: Race Conditions (Block vs Blobs)

**Problem:**
- Block header may arrive before blobs (or vice versa)
- Cannot commit block without all blobs
- Need state machine to track "pending" proposals
- Timeout handling if blobs never arrive

**Solution:**

1. **Availability State Machine:**
   ```rust
   enum ProposalState {
       MissingBlobs { header, txs, received_blobs, expected_count },
       VerifyingBlobs { proposal, verification_task },
       Available { proposal },
       TimedOut,
   }
   ```

2. **Buffering with Timeouts:**
   ```rust
   // Buffer proposals awaiting blobs (30s timeout)
   const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(30);

   if proposal.elapsed() > AVAILABILITY_TIMEOUT {
       // Mark as timed out, request re-proposal
       return Err(AvailabilityError::Timeout);
   }
   ```

3. **Early Blob Arrival Handling:**
   ```rust
   // Blobs can arrive before Init part
   // Store in temporary buffer keyed by stream_id
   pending_blobs: HashMap<StreamId, Vec<BlobSidecar>>,
   ```

4. **Commit Gating:**
   ```rust
   // Never commit unless all blobs verified
   fn can_commit(&self, proposal_id: ProposalId) -> bool {
       matches!(
           self.get_state(proposal_id),
           Some(ProposalState::Available { .. })
       )
   }
   ```

**Risk Mitigation:**
- ✅ State machine prevents premature commits
- ✅ Timeout ensures we don't wait forever
- ✅ Buffering handles out-of-order arrival
- ✅ Clear invariant: Available → all blobs verified

### 5.5 Challenge: Engine API Compatibility

**Problem:**
- Current implementation uses V3 methods
- Need to handle `blobsBundle` in `getPayloadV3` response
- Need to pass versioned hashes to `newPayloadV3`
- Execution clients may not support blobs yet

**Solution:**

1. **Capability Negotiation:**
   ```rust
   // Check if EL supports blobs
   let capabilities = engine_api.exchange_capabilities().await?;
   if !capabilities.get_payload_v3 {
       return Err(EngineError::BlobsNotSupported);
   }
   ```

2. **Graceful Degradation:**
   ```rust
   // If blobsBundle is empty, treat as no-blob block
   let blobs_bundle = payload.blobs_bundle.unwrap_or_default();
   if blobs_bundle.is_empty() {
       // No blobs, proceed with regular flow
   }
   ```

3. **Extended Response Parsing:**
   ```rust
   #[derive(Deserialize)]
   #[serde(rename_all = "camelCase")]
   pub struct GetPayloadV3Response {
       pub execution_payload: ExecutionPayloadV3,
       pub block_value: U256,
       #[serde(default)]  // Optional if not present
       pub blobs_bundle: Option<BlobsBundleV1>,
       pub should_override_builder: bool,
   }
   ```

4. **Versioned Hash Extraction:**
   ```rust
   fn extract_versioned_hashes(payload: &ExecutionPayloadV3) -> Vec<VersionedHash> {
       payload.transactions
           .iter()
           .filter_map(|tx| {
               if is_blob_transaction(tx) {
                   Some(tx.blob_versioned_hashes().to_vec())
               } else {
                   None
               }
           })
           .flatten()
           .collect()
   }
   ```

5. **Optional: Implement `getBlobsV1`:**
   ```rust
   // Fast path: fetch blobs from EL pool
   async fn try_get_blobs_from_el(
       &self,
       versioned_hashes: Vec<VersionedHash>
   ) -> Result<Vec<BlobAndProof>> {
       if self.capabilities.get_blobs_v1 {
           return self.engine_api.get_blobs(versioned_hashes).await;
       }
       Err(EngineError::MethodNotSupported)
   }
   ```

**Compatibility Matrix:**
| EL Client | V3 Support | Blob Support | Notes |
|-----------|------------|--------------|-------|
| Reth      | ✅         | ✅           | Full support |
| Geth      | ✅         | ✅           | Full support |
| Besu      | ✅         | ✅           | Full support |
| Nethermind| ✅         | ✅           | Full support |
| Erigon    | ✅         | ✅           | Full support |

### 5.6 Challenge: Malachite Context Trait Integration

**Problem:**
- Need to extend `LoadContext` with blob-aware types
- `ProposalPart` trait requires `is_first()` and `is_last()`
- Must maintain compatibility with existing Malachite code

**Solution:**

1. **Extend ProposalPart Enum:**
   ```rust
   // In ultramarine/crates/types/src/proposal_part.rs
   #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
   pub enum ProposalPart {
       Init(ProposalInit),
       Header(ExecutionPayloadHeader),
       Transactions(TransactionBatch),
       BlobSidecar(BlobSidecarPart),  // 🆕 NEW
       Fin(ProposalFin),
   }

   impl malachitebft_core_types::ProposalPart<LoadContext> for ProposalPart {
       fn is_first(&self) -> bool {
           matches!(self, ProposalPart::Init(_))
       }

       fn is_last(&self) -> bool {
           matches!(self, ProposalPart::Fin(_))
       }
   }
   ```

2. **Blob-Aware Value Type:**
   ```rust
   #[derive(Clone, Debug, PartialEq)]
   pub struct LoadValue {
       pub execution_payload: ExecutionPayloadV3,
       pub blob_sidecars: Vec<BlobSidecar>,  // 🆕 NEW
   }

   impl malachitebft_core_types::Value for LoadValue {
       type Id = ValueId;

       fn id(&self) -> Self::Id {
           // Include blob commitments in value ID
           compute_value_id(&self.execution_payload, &self.blob_sidecars)
       }
   }
   ```

3. **No Core Malachite Changes Needed:**
   - Malachite's generic design handles this naturally
   - `Context` trait already parameterized over all types
   - Network layer doesn't care about part contents

**Risk Mitigation:**
- ✅ Existing Malachite code unchanged
- ✅ Type system enforces correctness
- ✅ Serialization/deserialization handled by codec
- ✅ Can evolve independently of Malachite upgrades

---

## 6. Step-by-Step Integration Plan

### Phase 1: Foundation (Weeks 1-2)

**Goal:** Set up blob infrastructure without consensus integration

**Tasks:**

1. **Add Dependencies** (Day 1)
   ```toml
   [dependencies]
   c-kzg = "1.0"
   ```
   - Add `c-kzg` Rust bindings
   - Download KZG trusted setup file
   - Add to `assets/` directory

2. **Implement KZG Verification Module** (Days 2-3)
   - `ultramarine/crates/consensus/src/kzg.rs`
   - Load trusted setup
   - Single blob verification
   - Batch blob verification
   - Unit tests with test vectors

3. **Create Blob Storage Layer** (Days 4-5)
   - `ultramarine/crates/consensus/src/blob_store.rs`
   - Define redb tables
   - Implement CRUD operations
   - Add LRU cache
   - Pruning logic
   - Unit tests

4. **Extend Type System** (Days 6-7)
   - Update `ProposalPart` enum with `BlobSidecar` variant
   - Add `BlobSidecar` struct
   - Implement serialization/deserialization
   - Update `Value` type to include blobs
   - Unit tests

5. **Implement Availability Checker** (Days 8-10)
   - `ultramarine/crates/consensus/src/availability.rs`
   - State machine: `MissingBlobs` → `Verifying` → `Available`
   - Async KZG verification integration
   - Timeout handling
   - Unit tests with mock proposals

**Deliverable:** Blob infrastructure ready for integration (no consensus changes yet)

---

### Phase 2: Engine API Integration (Weeks 3-4)

**Goal:** Connect to execution layer blob support

**Tasks:**

1. **Extend Engine API Client** (Days 11-13)
   - Update `engine_getPayloadV3` to parse `blobsBundle`
   - Update `engine_newPayloadV3` to send versioned hashes
   - Implement `engine_getBlobsV1` (optional)
   - Add blob-specific error handling
   - Integration tests with mock EL

2. **Implement Versioned Hash Logic** (Day 14)
   - Extract versioned hashes from payload
   - Compute versioned hash from commitment
   - Validation logic
   - Unit tests

3. **Update Execution Layer Client** (Days 15-16)
   - Modify `generate_block()` to capture blobs bundle
   - Modify `notify_new_block()` to pass versioned hashes
   - Handle empty blobs bundle gracefully
   - Update error types
   - Integration tests

4. **Capability Negotiation** (Day 17)
   - Update `ULTRAMARINE_CAPABILITIES` to include V3 with blobs
   - Add blob capability checks
   - Graceful degradation if EL doesn't support blobs

5. **End-to-End Testing** (Days 18-20)
   - Test with Reth execution client
   - Test with Geth execution client
   - Verify blob bundle capture
   - Verify versioned hash matching
   - Test error cases

**Deliverable:** Working Engine API blob integration (can fetch/submit blobs)

---

### Phase 3: Consensus Integration (Weeks 5-6)

**Goal:** Integrate blobs into proposal/validation flow

**Tasks:**

1. **Proposer Path Integration** (Days 21-23)
   - Update `AppMsg::GetValue` handler
   - Capture blobs bundle from `getPayloadV3`
   - Create `BlobSidecar` proposal parts
   - Stream blob parts via network
   - Local KZG verification before proposing
   - Unit tests

2. **Receiver Path Integration** (Days 24-26)
   - Update `AppMsg::ReceivedProposalPart` handler
   - Add blob sidecar to availability checker
   - Trigger KZG verification when complete
   - Wait for verification before voting
   - Unit tests

3. **Commit Path Integration** (Days 27-28)
   - Update `AppMsg::Decided` handler
   - Compute versioned hashes from blobs
   - Pass to `newPayloadV3`
   - Persist blob sidecars to storage
   - Trigger pruning
   - Unit tests

4. **Streaming Optimization** (Day 29)
   - Tune chunk size for blob efficiency
   - Add progress tracking
   - Handle partial reassembly
   - Performance benchmarks

5. **Integration Testing** (Days 30-32)
   - End-to-end consensus with blobs
   - Multi-node testnet
   - Test with 6 blobs per block
   - Test with varying blob counts (0, 1, 3, 6)
   - Stress test with maximum blobs

**Deliverable:** Full consensus integration with blob sidecars

---

### Phase 4: Storage and Sync (Weeks 7-8)

**Goal:** Complete storage management and sync protocol

**Tasks:**

1. **Blob Pruning** (Days 33-35)
   - Implement retention policy
   - Scheduled pruning task
   - Metrics for blob storage size
   - Configurable retention period
   - Test pruning logic

2. **Sync Protocol Extension** (Days 36-38)
   - Update `GetDecidedValue` to include blobs
   - Extend `ProcessSyncedValue` to handle blobs
   - Blob-specific validation during sync
   - Handle missing blobs during catch-up
   - Sync integration tests

3. **Storage Migration** (Day 39)
   - Database schema migration if needed
   - Backward compatibility for nodes without blobs
   - Migration tool for existing databases

4. **Performance Optimization** (Days 40-42)
   - Parallel KZG verification benchmark
   - LRU cache tuning
   - Database query optimization
   - Compression experiments (optional)
   - Load testing

**Deliverable:** Production-ready storage and sync

---

### Phase 5: Testing and Hardening (Weeks 9-10)

**Goal:** Comprehensive testing and production readiness

**Tasks:**

1. **Unit Test Coverage** (Days 43-45)
   - Achieve >80% coverage for blob code
   - Edge case testing
   - Error handling tests
   - Property-based tests (proptest)

2. **Integration Test Suite** (Days 46-48)
   - Single node with blobs
   - Multi-node consensus with blobs
   - Byzantine scenarios (invalid KZG proofs)
   - Network partition scenarios
   - Sync from scratch with blobs

3. **Performance Benchmarks** (Days 49-50)
   - KZG verification throughput
   - Blob storage write/read latency
   - Network bandwidth utilization
   - Memory usage profiling
   - Establish performance baselines

4. **Security Audit** (Days 51-52)
   - DoS attack vectors (malicious blobs)
   - KZG verification failures
   - Storage exhaustion attacks
   - Race condition analysis
   - Cryptographic security review

5. **Documentation** (Days 53-55)
   - Architecture documentation (this document!)
   - API documentation (rustdoc)
   - Configuration guide
   - Troubleshooting guide
   - Operator runbook

**Deliverable:** Production-ready blob sidecar implementation

---

### Phase 6: Deployment and Monitoring (Weeks 11-12)

**Goal:** Deploy to testnet and production

**Tasks:**

1. **Testnet Deployment** (Days 56-58)
   - Deploy to Load Network testnet
   - Monitor blob propagation
   - Measure performance metrics
   - Collect operator feedback
   - Bug fixes

2. **Monitoring and Observability** (Days 59-60)
   - Add Prometheus metrics
     - Blob verification times
     - Storage size
     - Cache hit rates
     - Proposal assembly times
   - Add logging
   - Create Grafana dashboards
   - Set up alerting

3. **Configuration Tuning** (Days 61-62)
   - Optimize cache sizes based on testnet data
   - Tune retention periods
   - Adjust KZG worker pool size
   - Network parameter tuning

4. **Production Readiness Review** (Days 63-64)
   - Checklist review
   - Rollback plan
   - Incident response plan
   - Documentation review
   - Final security review

5. **Mainnet Deployment** (Days 65+)
   - Coordinate with operators
   - Phased rollout
   - Monitor closely
   - On-call support

**Deliverable:** Blob sidecars live on mainnet

---

### Timeline Summary

```
Phase 1: Foundation              [███████░░░░░░] Weeks 1-2  (10 days)
Phase 2: Engine API              [███████░░░░░░] Weeks 3-4  (10 days)
Phase 3: Consensus Integration   [████████░░░░░] Weeks 5-6  (12 days)
Phase 4: Storage & Sync          [████████░░░░░] Weeks 7-8  (10 days)
Phase 5: Testing & Hardening     [█████████░░░░] Weeks 9-10 (13 days)
Phase 6: Deployment              [████████░░░░░] Weeks 11-12 (10+ days)

Total: ~12 weeks (60-65 engineering days)
```

**Team Recommendation:**
- 2-3 engineers working full-time
- 1 engineer for KZG/crypto (Phases 1, 5)
- 1 engineer for Engine API/execution (Phases 2, 4)
- 1 engineer for consensus integration (Phases 3, 6)
- All engineers for testing/deployment (Phases 5, 6)

---

## 7. Performance Considerations

### 7.1 KZG Verification Performance

**Benchmarks (from Lighthouse data):**
- Single blob verification: ~5-10 ms
- Batch verification (6 blobs): ~15-20 ms (~2.5-3 ms per blob)
- **Speedup:** 5-10x faster with batching

**Optimization Strategies:**

1. **Always Use Batch Verification:**
   ```rust
   // DON'T: Verify individually (60 ms for 6 blobs)
   for sidecar in sidecars {
       verify_blob_kzg_proof(sidecar.blob, sidecar.commitment, sidecar.proof)?;
   }

   // DO: Verify in batch (15 ms for 6 blobs)
   let (blobs, commitments, proofs): (Vec<_>, Vec<_>, Vec<_>) =
       sidecars.iter().map(|s| (s.blob, s.commitment, s.proof)).unzip();
   verify_blob_kzg_proof_batch(&blobs, &commitments, &proofs)?;
   ```

2. **Async Verification:**
   ```rust
   // Spawn in thread pool, don't block consensus
   tokio::task::spawn_blocking(move || {
       verify_blob_kzg_proof_batch(...)
   })
   ```

3. **Early Verification:**
   ```rust
   // Proposer: verify locally before streaming
   // Prevents proposing invalid blobs
   verify_before_propose(&blobs_bundle)?;
   ```

4. **Parallel Multi-Proposal Verification:**
   ```rust
   // If multiple proposals arrive, verify in parallel
   let handles: Vec<_> = proposals
       .into_iter()
       .map(|p| spawn_verification(p))
       .collect();

   for handle in handles {
       handle.await?;
   }
   ```

**Expected Performance:**
- Verification overhead: ~15-20 ms per proposal (negligible)
- Block time: 5 seconds
- Verification: <0.5% of block time
- **Conclusion:** Not a bottleneck

### 7.2 Network Bandwidth Analysis

**Blob Sizes:**
- 1 blob: 131 KB
- 6 blobs: 786 KB
- + Commitments/proofs: ~800 KB total per block

**Bandwidth Requirements:**
- 5-second blocks: 160 KB/s steady state
- 1-second blocks (worst case): 800 KB/s
- **Comparison:** 1080p video stream is ~5 MB/s (30x higher)

**Network Overhead:**
- Streaming metadata: ~1 KB per part
- 6 blob parts: ~6 KB overhead (<1%)
- **Conclusion:** Negligible overhead

**Bandwidth Comparison:**
| Scenario | Bandwidth | Notes |
|----------|-----------|-------|
| 6 blobs @ 5s blocks | 160 KB/s | Ultramarine steady state |
| 9 blobs @ 1s blocks | 1.2 MB/s | Worst case (Electra upgrade) |
| Lighthouse (Ethereum) | 160 KB/s | Same blob load |
| Typical home internet | 12.5 MB/s | 100 Mbps connection |

**Mitigation Strategies:**
1. **Compression (Future):** 20-50% bandwidth reduction
2. **Selective Relay:** Don't relay to peers who already have blobs
3. **Bloom Filters:** Track "what each peer has" to avoid redundant sends

### 7.3 Storage I/O Performance

**Write Performance:**
- 800 KB per block write
- 5-second blocks: 160 KB/s write rate
- Modern SSD: 500 MB/s write
- **Utilization:** 0.03% of SSD bandwidth

**Read Performance:**
- Sync scenario: Read 800 KB × 100 blocks = 80 MB
- SSD read speed: 3 GB/s
- **Time:** <30 ms for 100 blocks worth of blobs

**Database Sizing:**
- 18-day retention: ~80 GB blob storage
- 1-year retention: ~1.6 TB (if desired for archive nodes)
- redb overhead: ~10-20% on top of raw data

**Optimization Strategies:**

1. **LRU Cache:**
   ```rust
   // Cache last 32 proposals worth of blobs (~25 MB)
   let cache = LruCache::new(32);
   ```
   - Cache hit ratio: >90% for recent blobs
   - Reduces disk reads significantly

2. **Separate Storage:**
   ```rust
   // Put blobs on separate disk/volume
   --blob-db-path /mnt/fast-ssd/blobs
   ```
   - Allows using faster disk for hot data
   - Isolates I/O load

3. **Batch Writes:**
   ```rust
   // Write all blobs from a proposal in single transaction
   let txn = db.begin_write()?;
   for sidecar in sidecars {
       table.insert(key, value)?;
   }
   txn.commit()?;  // Single fsync
   ```

### 7.4 Memory Usage Analysis

**Per-Proposal Memory:**
- Execution payload: ~500 KB (average)
- 6 blob sidecars: 800 KB
- Metadata: ~10 KB
- **Total:** ~1.3 MB per proposal

**LRU Cache:**
- 32 proposals × 1.3 MB = 41.6 MB

**Pending Proposals:**
- Availability checker buffer: ~10 proposals
- 10 × 1.3 MB = 13 MB

**KZG Settings:**
- Trusted setup: ~80 MB (loaded once)

**Total Memory Overhead:**
- ~135 MB for blob infrastructure
- **Comparison:** Existing ultramarine uses ~200 MB base
- **Increase:** ~67% increase (acceptable)

### 7.5 CPU Utilization

**Breakdown by Operation:**

| Operation | CPU Time | Frequency | % of Block Time |
|-----------|----------|-----------|-----------------|
| KZG Verification | 15 ms | Once per proposal | 0.3% |
| Serialization | 5 ms | Once per proposal | 0.1% |
| Streaming Assembly | 2 ms | Once per proposal | 0.04% |
| Storage Write | 1 ms | Once per commit | 0.02% |
| **Total** | **23 ms** | **Per block** | **0.46%** |

**Multi-Core Utilization:**
- KZG verification: Can parallelize across proposals
- Worker pool: Uses up to `num_cpus` threads
- Expected CPU usage: <5% on 8-core machine

### 7.6 Performance Benchmarks (Target)

**Phase 5 Benchmarks to Achieve:**

1. **KZG Verification:**
   - Target: <20 ms for 6-blob batch
   - Stretch: <15 ms with optimizations

2. **Proposal Assembly (Proposer):**
   - Target: <50 ms end-to-end (capture blobs → stream all parts)
   - Includes: Engine API call, serialization, streaming

3. **Proposal Validation (Receiver):**
   - Target: <100 ms end-to-end (receive all parts → KZG verify → vote)
   - Includes: Reassembly, KZG verification, storage

4. **Storage Writes:**
   - Target: <5 ms to persist 6 blob sidecars
   - Batch write in single transaction

5. **Storage Reads (Cache Miss):**
   - Target: <10 ms to fetch 6 blob sidecars from disk
   - Single read transaction

6. **Pruning:**
   - Target: <1 second to prune 1000 heights worth of blobs
   - Background task, non-blocking

---

## 8. Testing Strategy

### 8.1 Unit Tests

**KZG Module:**
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_kzg_single_blob_verification() {
        // Test with valid blob + commitment + proof
        // Test with invalid proof
        // Test with mismatched commitment
    }

    #[test]
    fn test_kzg_batch_verification() {
        // Test with 6 valid blobs
        // Test with 1 invalid blob in batch (should fail all)
        // Test with empty batch
    }

    #[test]
    fn test_trusted_setup_loading() {
        // Test loading from file
        // Test invalid file
        // Test corrupted data
    }
}
```

**Blob Storage:**
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_store_and_retrieve_sidecar() {
        // Store blob sidecar
        // Retrieve by (height, round, index)
        // Verify contents match
    }

    #[test]
    fn test_lru_cache_eviction() {
        // Fill cache beyond capacity
        // Verify LRU eviction
        // Verify cache hit/miss
    }

    #[test]
    fn test_pruning() {
        // Store 1000 heights worth of blobs
        // Prune before height 500
        // Verify heights 0-499 deleted
        // Verify heights 500+ remain
    }
}
```

**Availability Checker:**
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_state_machine_transitions() {
        // MissingBlobs → VerifyingBlobs → Available
        // Test timeout → TimedOut
        // Test invalid KZG → Failed
    }

    #[test]
    fn test_out_of_order_parts() {
        // Blobs arrive before Init
        // Init arrives after some blobs
        // Verify correct assembly
    }

    #[test]
    fn test_concurrent_proposals() {
        // Multiple proposals at different heights
        // Verify no cross-contamination
        // Verify independent verification
    }
}
```

### 8.2 Integration Tests

**Engine API Integration:**
```rust
#[tokio::test]
async fn test_get_payload_with_blobs() {
    // Mock execution client returns blobsBundle
    // Verify parsing
    // Verify blob extraction
    // Verify versioned hash computation
}

#[tokio::test]
async fn test_new_payload_with_versioned_hashes() {
    // Send payload with versioned hashes
    // Mock EL verifies hashes
    // Test mismatch scenario (should return INVALID)
}

#[tokio::test]
async fn test_empty_blobs_bundle() {
    // Payload with no blob transactions
    // Verify empty bundle handling
    // Verify flow continues normally
}
```

**Consensus Integration:**
```rust
#[tokio::test]
async fn test_propose_and_vote_with_blobs() {
    // Node 1: Propose block with 6 blobs
    // Node 2: Receive all parts
    // Node 2: Verify KZG
    // Node 2: Vote
    // Verify consensus reached
}

#[tokio::test]
async fn test_byzantine_invalid_kzg_proof() {
    // Malicious proposer sends invalid KZG proof
    // Receivers reject proposal
    // Verify no vote on invalid proposal
    // Verify re-proposal with valid proof succeeds
}

#[tokio::test]
async fn test_partial_blob_reception() {
    // Network drops some blob parts
    // Verify timeout mechanism
    // Verify request for re-transmission
}
```

**Multi-Node Tests:**
```rust
#[tokio::test]
async fn test_four_node_consensus_with_blobs() {
    // 4 nodes, 3f+1 = 4 (f=1, 1 Byzantine fault tolerance)
    // Node 1 proposes with 6 blobs
    // All nodes receive and verify
    // Verify consensus reached
    // Verify all nodes commit same block + blobs
}

#[tokio::test]
async fn test_network_partition_with_blobs() {
    // Partition network mid-blob streaming
    // Verify timeout and recovery
    // Verify consensus eventually reached
}
```

### 8.3 Stress Tests

**Maximum Blob Load:**
```rust
#[tokio::test]
async fn test_max_blobs_per_block() {
    // Propose block with 6 blobs (max for Deneb)
    // Verify handling
    // Measure performance
}

#[tokio::test]
async fn test_continuous_max_blob_load() {
    // 100 consecutive blocks with 6 blobs each
    // Verify no memory leaks
    // Verify no performance degradation
    // Verify pruning keeps storage bounded
}
```

**Concurrent Verification:**
```rust
#[tokio::test]
async fn test_multiple_proposals_concurrent_verification() {
    // 10 proposals arrive simultaneously
    // All have 6 blobs
    // Verify worker pool handles load
    // Verify no deadlocks
}
```

**Storage Stress:**
```rust
#[tokio::test]
async fn test_storage_growth_and_pruning() {
    // Run for 30,000 heights (equivalent to 18 days at 5s blocks)
    // Verify pruning triggers
    // Verify storage stays below 100 GB
    // Measure pruning performance
}
```

### 8.4 Property-Based Tests

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_versioned_hash_roundtrip(commitment in any::<KzgCommitment>()) {
        let versioned_hash = kzg_commitment_to_versioned_hash(&commitment);
        assert_eq!(versioned_hash[0], 0x01);  // KZG version byte
        assert_eq!(versioned_hash.len(), 32);
    }

    #[test]
    fn test_blob_sidecar_serialization_roundtrip(sidecar in any::<BlobSidecar>()) {
        let bytes = bincode::serialize(&sidecar).unwrap();
        let deserialized: BlobSidecar = bincode::deserialize(&bytes).unwrap();
        assert_eq!(sidecar, deserialized);
    }

    #[test]
    fn test_proposal_part_ordering(parts in prop::collection::vec(any::<ProposalPart>(), 1..20)) {
        // Property: Init must be first, Fin must be last
        let init_indices: Vec<_> = parts.iter()
            .enumerate()
            .filter(|(_, p)| p.is_first())
            .map(|(i, _)| i)
            .collect();

        let fin_indices: Vec<_> = parts.iter()
            .enumerate()
            .filter(|(_, p)| p.is_last())
            .map(|(i, _)| i)
            .collect();

        // At most one Init and one Fin
        assert!(init_indices.len() <= 1);
        assert!(fin_indices.len() <= 1);

        // If both present, Init before Fin
        if !init_indices.is_empty() && !fin_indices.is_empty() {
            assert!(init_indices[0] < fin_indices[0]);
        }
    }
}
```

### 8.5 Chaos Engineering Tests

**Network Chaos:**
```rust
#[tokio::test]
async fn test_random_packet_loss() {
    // Inject 10% packet loss
    // Verify retransmission logic
    // Verify eventual consistency
}

#[tokio::test]
async fn test_network_delays() {
    // Inject 0-500ms random delays
    // Verify timeout handling
    // Verify no deadlocks
}
```

**Node Failures:**
```rust
#[tokio::test]
async fn test_proposer_crash_mid_streaming() {
    // Proposer crashes after sending 3/6 blob parts
    // Verify timeout on receivers
    // Verify re-proposal by new proposer
}

#[tokio::test]
async fn test_receiver_crash_during_verification() {
    // Receiver crashes during KZG verification
    // Restart receiver
    // Verify catch-up sync includes blobs
}
```

---

## 9. Appendix: Reference Implementations

### 9.1 Lighthouse Blob Implementation

**Key Files:**
- `consensus/types/src/blob_sidecar.rs` - BlobSidecar type definition
- `beacon_node/beacon_chain/src/blob_verification.rs` - Multi-stage verification
- `beacon_node/beacon_chain/src/data_availability_checker.rs` - Availability state machine
- `beacon_node/store/src/hot_cold_store.rs` - Storage architecture
- `beacon_node/lighthouse_network/src/types/topics.rs` - Gossip subnet definitions

**Architecture Highlights:**
1. Separate `blobs_db` directory for blob storage
2. Multi-stage verification: Gossip → Signature → KZG
3. LRU cache for hot blobs (configurable size)
4. Observation pattern for DoS resistance
5. Data availability boundary enforcement

**Performance:**
- Batch KZG verification: 5-10x speedup
- Cache hit rate: >90% for recent blobs
- Storage overhead: ~100 GB for 18-day retention

### 9.2 Malachite Architecture

**Key Files:**
- `core-types/src/context.rs` - Context trait (extension point)
- `core-types/src/proposal_part.rs` - ProposalPart trait
- `engine/src/host.rs` - Host interface (application callbacks)
- `engine/src/util/streaming.rs` - StreamMessage wrapper
- `network/src/channel.rs` - Network channel definitions

**Architecture Highlights:**
1. Generic Context trait parameterizes all types
2. ProposalPart intentionally minimal (just `is_first/last`)
3. Separate channels for different message types
4. Streaming infrastructure built-in
5. No P2P implementation details exposed to app

**Extension Points:**
1. `Context::ProposalPart` - Custom proposal part types
2. `Context::Extension` - Vote extensions (e.g., blob attestations)
3. `NetworkMsg::PublishProposalPart` - Stream custom parts
4. `HostMsg::ReceivedProposalPart` - Receive and assemble parts

### 9.3 EIP-4844 Specification

**Key Documents:**
- `consensus-specs/specs/deneb/beacon-chain.md` - Core blob types
- `consensus-specs/specs/deneb/polynomial-commitments.md` - KZG operations
- `consensus-specs/specs/deneb/p2p-interface.md` - Network propagation
- `execution-apis/src/engine/cancun.md` - Engine API V3 with blobs

**Key Constants:**
- `BYTES_PER_BLOB = 131,072` (128 KB)
- `MAX_BLOBS_PER_BLOCK = 6` (Deneb) → 9 (Electra)
- `MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS = 4,096` (~18 days)

**Key Algorithms:**
- `verify_blob_kzg_proof(blob, commitment, proof)` - Single verification
- `verify_blob_kzg_proof_batch(blobs, commitments, proofs)` - Batch verification
- `kzg_commitment_to_versioned_hash(commitment)` - Hash derivation

### 9.4 Code Examples

**Example: Complete Proposal Assembly**

```rust
async fn assemble_proposal_with_blobs(
    state: &mut State,
    execution_layer: &ExecutionLayer,
    height: Height,
    round: Round,
) -> Result<Vec<ProposalPart>, ProposalError> {
    // 1. Get payload with blobs from execution layer
    let payload_with_blobs = execution_layer
        .get_payload_with_blobs(payload_id)
        .await?;

    let execution_payload = payload_with_blobs.execution_payload;
    let blobs_bundle = payload_with_blobs.blobs_bundle;

    // 2. Validate blobs bundle
    if let Some(ref bundle) = blobs_bundle {
        bundle.validate()?;

        // Verify KZG proofs locally before proposing
        verify_blob_kzg_proof_batch(
            &bundle.blobs,
            &bundle.commitments,
            &bundle.proofs,
            &state.kzg_settings,
        )?;
    }

    // 3. Build proposal parts
    let mut parts = Vec::new();

    // Init part
    parts.push(ProposalPart::Init(ProposalInit {
        height,
        round,
        proposer: state.address,
        value_id: compute_value_id(&execution_payload, &blobs_bundle),
        timestamp: Instant::now(),
    }));

    // Header part
    parts.push(ProposalPart::Header(ExecutionPayloadHeader {
        parent_hash: execution_payload.parent_hash,
        block_number: execution_payload.block_number,
        block_hash: execution_payload.block_hash,
        // ... other header fields
    }));

    // Transaction parts (chunked)
    for tx_chunk in execution_payload.transactions.chunks(CHUNK_SIZE) {
        parts.push(ProposalPart::Transactions(
            TransactionBatch { transactions: tx_chunk.to_vec() }
        ));
    }

    // Blob sidecar parts (if present)
    if let Some(bundle) = blobs_bundle {
        for (index, ((blob, commitment), proof)) in bundle.blobs
            .into_iter()
            .zip(bundle.commitments)
            .zip(bundle.proofs)
            .enumerate()
        {
            parts.push(ProposalPart::BlobSidecar {
                index: index as u64,
                blob,
                kzg_commitment: commitment,
                kzg_proof: proof,
            });
        }
    }

    // Fin part (signature over all previous parts)
    let signature = sign_proposal_parts(&parts, &state.signing_provider)?;
    parts.push(ProposalPart::Fin(ProposalFin { signature }));

    Ok(parts)
}
```

**Example: Blob Availability State Machine**

```rust
impl BlobAvailabilityChecker {
    pub async fn process_part(
        &mut self,
        proposal_id: ProposalId,
        part: ProposalPart,
    ) -> Result<Option<VerifiedProposal>, AvailabilityError> {
        let entry = self.pending.entry(proposal_id)
            .or_insert_with(|| PendingProposal {
                height: proposal_id.height,
                round: proposal_id.round,
                state: ProposalState::MissingBlobs {
                    header: None,
                    transactions: Vec::new(),
                    received_blobs: HashMap::new(),
                    expected_count: 0,
                },
            });

        match (&mut entry.state, part) {
            // Receive Init part
            (ProposalState::MissingBlobs { expected_count, .. },
             ProposalPart::Init(init)) => {
                // Extract expected blob count from Init metadata
                *expected_count = init.blob_count;
            }

            // Receive blob sidecar
            (ProposalState::MissingBlobs { received_blobs, expected_count, .. },
             ProposalPart::BlobSidecar { index, blob, kzg_commitment, kzg_proof }) => {
                // Store blob sidecar
                received_blobs.insert(index, BlobSidecar {
                    index,
                    blob,
                    kzg_commitment,
                    kzg_proof,
                });

                // Check if all blobs received
                if received_blobs.len() == *expected_count as usize {
                    // Transition to verifying state
                    let complete_proposal = self.build_complete_proposal(entry)?;

                    // Spawn KZG verification task
                    let verification_task = self.kzg_workers.verify_batch(
                        VerificationRequest {
                            proposal_id,
                            blobs: complete_proposal.blobs(),
                            commitments: complete_proposal.commitments(),
                            proofs: complete_proposal.proofs(),
                        }
                    );

                    entry.state = ProposalState::VerifyingBlobs {
                        complete_proposal,
                        verification_task,
                    };
                }
            }

            // Check verification task completion
            (ProposalState::VerifyingBlobs { verification_task, complete_proposal },
             _) => {
                // Poll verification task
                if verification_task.is_finished() {
                    let result = verification_task.await??;

                    if result.is_ok() {
                        // Transition to available state
                        let verified_proposal = VerifiedProposal {
                            proposal: complete_proposal.clone(),
                            verified_at: Instant::now(),
                        };

                        entry.state = ProposalState::Available {
                            proposal: verified_proposal.clone(),
                            timestamp: Instant::now(),
                        };

                        return Ok(Some(verified_proposal));
                    } else {
                        // Verification failed - reject proposal
                        return Err(AvailabilityError::InvalidKzgProof);
                    }
                }
            }

            _ => {
                // Ignore other parts or invalid state transitions
            }
        }

        Ok(None)
    }
}
```

---

## Conclusion

This technical review demonstrates that **integrating EIP-4844 blob sidecars into Ultramarine is highly feasible** and can be accomplished within a 12-week timeline with 2-3 engineers.

### Key Takeaways

1. **Architecture Fit:** Malachite's design is ideal for blob sidecars
   - Context trait provides clean extension points
   - Streaming infrastructure already handles large data
   - Separate network channels prevent congestion
   - No core consensus changes required

2. **Technical Feasibility:** All major challenges have known solutions
   - KZG verification: Use `c-kzg` with batch verification
   - Storage: Separate blob DB with aggressive pruning
   - Network: Existing streaming handles blob bandwidth
   - Engine API: Straightforward extension of V3 methods

3. **Performance:** Not a bottleneck
   - KZG verification: <0.5% of block time
   - Network bandwidth: ~160 KB/s (trivial for modern networks)
   - Storage: ~100 GB with 18-day retention (manageable)
   - Memory: ~135 MB overhead (acceptable)

4. **Risk Level:** Low to medium
   - Proven patterns from Lighthouse
   - Well-defined specifications (EIP-4844)
   - Mature dependencies (c-kzg, redb)
   - Main risk: KZG crypto integration complexity

### Next Steps

1. **Approve this plan** and allocate engineering resources
2. **Set up development environment** with KZG dependencies
3. **Start Phase 1** (Foundation) - weeks 1-2
4. **Regular reviews** at phase boundaries
5. **Testnet deployment** at week 8 (after Phase 4)
6. **Production deployment** at week 12+

### Success Metrics

- [ ] KZG verification: <20 ms for 6-blob batch
- [ ] Proposal assembly: <50 ms end-to-end
- [ ] Proposal validation: <100 ms end-to-end
- [ ] Storage: <100 GB with 18-day retention
- [ ] Network bandwidth: <200 KB/s steady state
- [ ] Memory overhead: <150 MB
- [ ] Test coverage: >80% for blob code
- [ ] Zero consensus failures in testnet (4+ weeks)
- [ ] Production-ready documentation complete

**The architecture is sound. The plan is achievable. Let's build it! 🚀**

---

## Document History

| Version | Date | Author | Changes |
|---------|------|--------|---------|
| 1.0 | 2025-10-08 | Claude Code | Initial comprehensive technical review |

---

## References

1. [EIP-4844: Shard Blob Transactions](https://eips.ethereum.org/EIPS/eip-4844)
2. [Consensus Specs - Deneb](https://github.com/ethereum/consensus-specs/tree/dev/specs/deneb)
3. [Execution APIs - Cancun](https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md)
4. [Lighthouse Consensus Client](https://github.com/sigp/lighthouse)
5. [Malachite BFT](https://github.com/informalsystems/malachite)
6. [c-kzg Library](https://github.com/ethereum/c-kzg-4844)
7. [Ultramarine Consensus Client](https://github.com/loadnetwork/ultramarine)

---

*End of Technical Report*
