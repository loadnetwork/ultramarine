# EIP-4844 Blob Sidecar Integration - Final Implementation Plan

**Project**: Integrate blob sidecars into Ultramarine consensus client
**Timeline**: 10-15 days (2-3 weeks with focused effort)
**Architecture**: Channel-based approach using existing Malachite patterns
**Status**: ⚠️ **Live Consensus Complete - State Sync Broken** - 🚨 **NOT PRODUCTION READY**
**Progress**: **4.7/8 phases complete** (59%) - Live consensus working, sync broken
**Blocker**: State synchronization missing blob transfer - new/lagging peers cannot sync
**Last Updated**: 2025-10-21

---

## Executive Summary

This plan integrates EIP-4844 blob sidecars into Ultramarine while maintaining clean separation between consensus and data availability layers. The critical architectural decision is to:

1. **Consensus votes on hash(metadata)** - keeps consensus messages ~2KB
2. **Blobs flow separately** via existing ProposalParts channel - 131KB+ per blob
3. **Application layer enforces availability** before marking blocks valid

**Key Insight**: We do NOT modify Malachite library or add new network topics. Blobs flow through the existing `/proposal_parts` gossip channel by extending the application-layer `ProposalPart` enum.

---

## Architecture Overview

```

---

## Appendix: Blob Chunking & File Reconstruction Notes (to think)

- Blob size is fixed at 131,072 bytes (128 KB) per EIP-4844; changing it would require a new trusted setup and breaks compatibility. To increase throughput, raise the blob count per block and associated data-gas limits instead.
- Arbitrary files can be supported at the application layer by chunking into standard blobs, storing/referencing an ordered manifest (hash, length, blob commitments), and mirroring blob contents to long-term storage (e.g., Arweave/IPFS) after gossip.
- A smart contract can anchor file metadata/manifest while off-chain services handle blob submission, archival, and later reconstruction by fetching sidecars, verifying commitments, and concatenating in manifest order.
- Future tooling should include a chunker, manifest schema, archiver, and reassembler so operators can treat blobs as a “data vehicle” for NFTs or large assets while keeping consensus messages lightweight.
- Manifest storage must stay concise: for very large uploads store only total size, root commitment, and archive pointer on-chain; keep the full blob listing off-chain with Merkle/KZG proofs or bundle-level commitments.
- An SDK can give users a single-call UX (`upload(file)`) that chunks data, submits blobs, posts the manifest, and waits for an archiver to mirror the payload. Downloads then use `download(manifest_id)` to fetch and verify from the archive.
- Archiver services (initially trusted validators) watch manifests, retrieve blobs from sidecars, mirror to Arweave/IPFS/custom storage, and log the resulting CID on-chain for payouts/challenges.
┌─────────────────────────────────────────────────────────────────┐
│                      CONSENSUS LAYER (Malachite)                │
│  Votes on: hash(ValueMetadata) ← only 8 bytes, ~2KB messages   │
└─────────────────────────────────────────────────────────────────┘
                              ▲
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                   DATA AVAILABILITY LAYER                        │
│  ProposalParts Channel: Init → BlobSidecar(0..N) → Fin         │
│  Each blob: 131KB + proofs, transmitted separately              │
└─────────────────────────────────────────────────────────────────┘
                              ▲
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    EXECUTION LAYER (EL Client)                   │
│  Engine API v3: getPayloadV3() returns blobsBundle              │
│  Execution layer generates blobs, we verify & propagate         │
└─────────────────────────────────────────────────────────────────┘
```

**Network Flow**:
```
Proposer                           Validators
   │                                   │
   │ 1. GetValue trigger               │
   │ 2. Call EL getPayloadV3()        │
   │ 3. Receive blobs + commitments   │
   │                                   │
   │ 4. Stream on /proposal_parts:    │
   ├──> Init(height, round, metadata) ├──> Receive & buffer
   ├──> BlobSidecar(index=0)          ├──> Store temporarily
   ├──> BlobSidecar(index=1)          ├──> Verify KZG proofs
   ├──> ...                            ├──> Check against metadata
   ├──> BlobSidecar(index=N)          ├──> Mark available
   └──> Fin(signature)                 └──> Vote if DA complete
   │                                   │
   │ 5. Validators vote on hash(meta) │
   │ 6. Consensus reaches 2/3+        │
   │ 7. Block decided → import         │
   │ 8. Store blobs in hot DB         │
   │ 9. Publish CID in vote extension │
   └───────────────────────────────────┘
```

---

## Phase 1: Execution ↔ Consensus Bridge (Days 1-2)

### Goal
Extend execution client interface to support Engine API v3 with blob bundle retrieval.

### Files to Modify
- `ultramarine/crates/execution/src/client.rs`
- `ultramarine/crates/execution/src/types.rs`

### Implementation

#### 1.1 Add Blob Types

**File**: `ultramarine/crates/execution/src/types.rs`

```rust
use bytes::Bytes;

/// A single blob with its KZG commitment and proof (EIP-4844)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    /// The blob data (131,072 bytes)
    pub data: Bytes,
}

impl Blob {
    pub const BYTES_PER_BLOB: usize = 131_072;

    pub fn new(data: Bytes) -> Result<Self, String> {
        if data.len() != Self::BYTES_PER_BLOB {
            return Err(format!(
                "Invalid blob size: expected {}, got {}",
                Self::BYTES_PER_BLOB,
                data.len()
            ));
        }
        Ok(Self { data })
    }
}

/// KZG commitment (48 bytes)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KzgCommitment(pub [u8; 48]);

/// KZG proof (48 bytes)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KzgProof(pub [u8; 48]);

/// Bundle of blobs with their cryptographic commitments
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobsBundle {
    pub commitments: Vec<KzgCommitment>,
    pub proofs: Vec<KzgProof>,
    pub blobs: Vec<Blob>,
}

impl BlobsBundle {
    /// Validate bundle structure (equal lengths, within limits)
    pub fn validate(&self) -> Result<(), String> {
        if self.blobs.len() != self.commitments.len()
            || self.blobs.len() != self.proofs.len()
        {
            return Err("BlobsBundle length mismatch".to_string());
        }

        // Deneb: max 6, Electra: max 9
        const MAX_BLOBS_PER_BLOCK: usize = 6; // TODO: Make fork-aware
        if self.blobs.len() > MAX_BLOBS_PER_BLOCK {
            return Err(format!(
                "Too many blobs: {} > {}",
                self.blobs.len(),
                MAX_BLOBS_PER_BLOCK
            ));
        }

        Ok(())
    }

    /// Calculate versioned hashes for blob transactions
    pub fn versioned_hashes(&self) -> Vec<[u8; 32]> {
        use sha2::{Digest, Sha256};

        self.commitments
            .iter()
            .map(|commitment| {
                let mut hash = Sha256::digest(&commitment.0);
                hash[0] = 0x01; // BLOB_COMMITMENT_VERSION_KZG
                hash.into()
            })
            .collect()
    }
}
```

#### 1.2 Extend ExecutionClient

**File**: `ultramarine/crates/execution/src/client.rs`

```rust
use crate::types::{BlobsBundle, KzgCommitment, KzgProof, Blob};

impl ExecutionClient {
    /// Generate a new block with blob bundle (Engine API v3)
    pub async fn generate_block_with_blobs(
        &self,
        slot: u64,
        parent_hash: Hash256,
        timestamp: u64,
    ) -> Result<(ExecutionPayload, Option<BlobsBundle>), ExecutionError> {
        // 1. Capability negotiation
        let capabilities = self.check_capabilities().await?;
        if !capabilities.supports_engine_api_v3() {
            // Fallback to v2 without blobs
            let payload = self.generate_block_v2(slot, parent_hash, timestamp).await?;
            return Ok((payload, None));
        }

        // 2. Call getPayloadV3 (includes blobsBundle field)
        let payload_id = self
            .fork_choice_updated_v3(parent_hash, timestamp)
            .await?;

        let response: GetPayloadV3Response = self
            .call_engine_api("engine_getPayloadV3", json!([payload_id]))
            .await?;

        // 3. Parse blobs bundle from response
        let blobs_bundle = if let Some(bundle_json) = response.blobs_bundle {
            Some(self.parse_blobs_bundle(bundle_json)?)
        } else {
            None
        };

        // 4. Validate bundle if present
        if let Some(ref bundle) = blobs_bundle {
            bundle.validate()?;
        }

        Ok((response.execution_payload, blobs_bundle))
    }

    /// Parse JSON blob bundle from EL response
    fn parse_blobs_bundle(
        &self,
        json: serde_json::Value,
    ) -> Result<BlobsBundle, ExecutionError> {
        let commitments: Vec<String> = serde_json::from_value(
            json["commitments"].clone()
        )?;
        let proofs: Vec<String> = serde_json::from_value(
            json["proofs"].clone()
        )?;
        let blobs: Vec<String> = serde_json::from_value(
            json["blobs"].clone()
        )?;

        // Convert hex strings to typed structures
        let commitments: Vec<KzgCommitment> = commitments
            .into_iter()
            .map(|hex| self.hex_to_commitment(&hex))
            .collect::<Result<_, _>>()?;

        let proofs: Vec<KzgProof> = proofs
            .into_iter()
            .map(|hex| self.hex_to_proof(&hex))
            .collect::<Result<_, _>>()?;

        let blobs: Vec<Blob> = blobs
            .into_iter()
            .map(|hex| self.hex_to_blob(&hex))
            .collect::<Result<_, _>>()?;

        Ok(BlobsBundle {
            commitments,
            proofs,
            blobs,
        })
    }

    fn hex_to_commitment(&self, hex: &str) -> Result<KzgCommitment, ExecutionError> {
        let bytes = hex::decode(hex.trim_start_matches("0x"))?;
        if bytes.len() != 48 {
            return Err(ExecutionError::InvalidCommitment);
        }
        let mut array = [0u8; 48];
        array.copy_from_slice(&bytes);
        Ok(KzgCommitment(array))
    }

    fn hex_to_proof(&self, hex: &str) -> Result<KzgProof, ExecutionError> {
        let bytes = hex::decode(hex.trim_start_matches("0x"))?;
        if bytes.len() != 48 {
            return Err(ExecutionError::InvalidProof);
        }
        let mut array = [0u8; 48];
        array.copy_from_slice(&bytes);
        Ok(KzgProof(array))
    }

    fn hex_to_blob(&self, hex: &str) -> Result<Blob, ExecutionError> {
        let bytes = hex::decode(hex.trim_start_matches("0x"))?;
        Blob::new(Bytes::from(bytes))
            .map_err(|e| ExecutionError::InvalidBlob(e))
    }
}

#[derive(Debug)]
pub enum ExecutionError {
    InvalidCommitment,
    InvalidProof,
    InvalidBlob(String),
    // ... existing variants
}
```

### Testing
```bash
# Unit tests for blob bundle parsing
cargo nextest run -p ultramarine-execution test_blob_bundle_parsing

# Integration test with mock EL
cargo nextest run -p ultramarine-execution test_engine_api_v3_integration
```

---

## Phase 2: Consensus Value Refactor (Days 3-4) ✅ COMPLETED

### Goal
Refactor `Value` to only contain lightweight metadata (~2KB), NOT full blob data. This keeps consensus messages small and efficient.

**Status**
- `ValueMetadata` added with execution header + KZG commitments (~1 KB) and protobuf support.
- `Value::new` hashes metadata, debug-validates inputs, and keeps `extensions` only for backward compatibility.
- Consensus/storage now serialize via `ProtobufCodec`; legacy `from_bytes` remains temporarily until Phase 3 migration.
- Added tests for metadata validation, multiple blobs, protobuf roundtrips (`cargo test -p ultramarine-types`).

### Files to Modify
- `ultramarine/crates/types/src/value.rs`
- `ultramarine/crates/types/src/value_metadata.rs` (new file)

### Implementation

#### 2.1 Create ValueMetadata Structure

**File**: `ultramarine/crates/types/src/value_metadata.rs` (new)

```rust
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::execution::types::{ExecutionPayloadHeader, KzgCommitment};

/// Lightweight metadata about a proposed value
/// This is what gets voted on in consensus (keeps messages ~2KB)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueMetadata {
    /// Execution payload header (block hash, state root, etc.)
    pub execution_payload_header: ExecutionPayloadHeader,

    /// KZG commitments for blobs (48 bytes each, max 6-9 blobs)
    pub blob_kzg_commitments: Vec<KzgCommitment>,

    /// Number of blobs included
    pub blob_count: u8,

    /// Total size of blob data (for bandwidth estimation)
    pub total_blob_bytes: u32,
}

impl ValueMetadata {
    pub fn new(
        execution_payload_header: ExecutionPayloadHeader,
        blob_kzg_commitments: Vec<KzgCommitment>,
    ) -> Self {
        let blob_count = blob_kzg_commitments.len() as u8;
        let total_blob_bytes = (blob_count as u32) * 131_072;

        Self {
            execution_payload_header,
            blob_kzg_commitments,
            blob_count,
            total_blob_bytes,
        }
    }

    /// Calculate hash for consensus voting
    pub fn consensus_hash(&self) -> u64 {
        use std::{
            collections::hash_map::DefaultHasher,
            hash::{Hash, Hasher},
        };

        let mut hasher = DefaultHasher::new();

        // Hash execution payload header
        self.execution_payload_header.hash(&mut hasher);

        // Hash all commitments
        for commitment in &self.blob_kzg_commitments {
            commitment.0.hash(&mut hasher);
        }

        hasher.finish()
    }

    /// Estimate size in bytes
    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<ExecutionPayloadHeader>()
            + (self.blob_count as usize * 48) // commitments
            + 8 // other fields
    }
}
```

#### 2.2 Refactor Value Structure

**File**: `ultramarine/crates/types/src/value.rs`

```rust
use bytes::Bytes;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::{proto, value_metadata::ValueMetadata};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Serialize, Deserialize)]
pub struct ValueId(u64);

impl ValueId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

impl From<u64> for ValueId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// The value to decide on
/// CRITICAL: This is what consensus votes on - must be lightweight!
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Value {
    /// Consensus hash of the metadata
    pub value: u64,

    /// Lightweight metadata (~2KB) - NOT full blob data
    pub metadata: ValueMetadata,
}

impl Value {
    /// Create a new value from metadata
    pub fn new(metadata: ValueMetadata) -> Self {
        let value = metadata.consensus_hash();
        Self { value, metadata }
    }

    pub fn id(&self) -> ValueId {
        ValueId(self.value)
    }

    pub fn size_bytes(&self) -> usize {
        std::mem::size_of_val(&self.value) + self.metadata.size_bytes()
    }

    /// Get blob commitments for verification
    pub fn blob_commitments(&self) -> &[KzgCommitment] {
        &self.metadata.blob_kzg_commitments
    }
}

impl malachitebft_core_types::Value for Value {
    type Id = ValueId;

    fn id(&self) -> ValueId {
        self.id()
    }
}

impl Protobuf for Value {
    type Proto = proto::Value;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let metadata = ValueMetadata::from_proto(
            proto.metadata.ok_or_else(|| {
                ProtoError::missing_field::<Self::Proto>("metadata")
            })?
        )?;

        Ok(Value::new(metadata))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(proto::Value {
            value: self.value,
            metadata: Some(self.metadata.to_proto()?),
        })
    }
}
```

### Migration Impact
- All existing code that accesses `value.extensions` must be updated
- Consensus messages shrink from potentially MBs to ~2KB
- Enables efficient voting without full blob data

### Testing
```bash
# Verify consensus hash stability
cargo nextest run -p ultramarine-types test_value_metadata_hash_stability

# Check backward compatibility
cargo nextest run -p ultramarine-types test_value_serialization
```

---

## Phase 3: Proposal Streaming (Day 5)

### Goal
Extend `ProposalPart` enum to include blob sidecars, flowing through existing `/proposal_parts` channel.

### Files to Modify
- `ultramarine/crates/types/src/proposal_part.rs`
- `ultramarine/crates/consensus/src/streaming.rs`

### Implementation

#### 3.1 Extend ProposalPart Enum

**File**: `ultramarine/crates/types/src/proposal_part.rs`

```rust
use bytes::Bytes;
use malachitebft_core_types::Round;
use malachitebft_proto::{self as proto, Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::{
    address::Address,
    execution::types::{Blob, KzgCommitment, KzgProof},
    height::Height,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalPart {
    Init(ProposalInit),
    Data(ProposalData),

    /// NEW: Blob sidecar with KZG proof
    BlobSidecar(BlobSidecar),

    Fin(ProposalFin),
}

impl ProposalPart {
    pub fn get_type(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::Data(_) => "data",
            Self::BlobSidecar(_) => "blob_sidecar",
            Self::Fin(_) => "fin",
        }
    }

    pub fn as_blob_sidecar(&self) -> Option<&BlobSidecar> {
        match self {
            Self::BlobSidecar(sidecar) => Some(sidecar),
            _ => None,
        }
    }

    pub fn size_bytes(&self) -> usize {
        let proto_msg = self.to_proto().unwrap();
        proto_msg.encoded_len()
    }
}

/// A blob sidecar containing blob data and cryptographic proofs
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSidecar {
    /// Index of this blob (0-5 for Deneb, 0-8 for Electra)
    pub index: u8,

    /// The blob data (131,072 bytes)
    pub blob: Blob,

    /// KZG commitment for this blob
    pub kzg_commitment: KzgCommitment,

    /// KZG proof for verification
    pub kzg_proof: KzgProof,
}

impl BlobSidecar {
    pub fn new(
        index: u8,
        blob: Blob,
        kzg_commitment: KzgCommitment,
        kzg_proof: KzgProof,
    ) -> Self {
        Self {
            index,
            blob,
            kzg_commitment,
            kzg_proof,
        }
    }

    /// Calculate size in bytes
    pub fn size_bytes(&self) -> usize {
        1 + Blob::BYTES_PER_BLOB + 48 + 48 // index + blob + commitment + proof
    }
}

impl malachitebft_core_types::ProposalPart<LoadContext> for ProposalPart {
    fn is_first(&self) -> bool {
        matches!(self, Self::Init(_))
    }

    fn is_last(&self) -> bool {
        matches!(self, Self::Fin(_))
    }
}

impl Protobuf for ProposalPart {
    type Proto = crate::proto::ProposalPart;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        use crate::proto::proposal_part::Part;

        let part = proto.part.ok_or_else(|| {
            ProtoError::missing_field::<Self::Proto>("part")
        })?;

        match part {
            Part::Init(init) => Ok(Self::Init(ProposalInit {
                height: Height::new(init.height),
                round: Round::new(init.round),
                proposer: Address::from_proto(
                    init.proposer.ok_or_else(|| {
                        ProtoError::missing_field::<Self::Proto>("proposer")
                    })?
                )?,
            })),
            Part::Data(data) => Ok(Self::Data(ProposalData::new(data.bytes))),

            // NEW: Deserialize blob sidecar
            Part::BlobSidecar(sidecar) => {
                let blob = Blob::new(sidecar.blob)
                    .map_err(|e| ProtoError::Other(e))?;

                Ok(Self::BlobSidecar(BlobSidecar {
                    index: sidecar.index as u8,
                    blob,
                    kzg_commitment: KzgCommitment(
                        sidecar.kzg_commitment.try_into()
                            .map_err(|_| ProtoError::Other("Invalid commitment".into()))?
                    ),
                    kzg_proof: KzgProof(
                        sidecar.kzg_proof.try_into()
                            .map_err(|_| ProtoError::Other("Invalid proof".into()))?
                    ),
                }))
            }

            Part::Fin(fin) => Ok(Self::Fin(ProposalFin {
                signature: decode_signature(
                    fin.signature.ok_or_else(|| {
                        ProtoError::missing_field::<Self::Proto>("signature")
                    })?
                )?,
            })),
        }
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        use crate::proto::{self, proposal_part::Part};

        match self {
            Self::Init(init) => Ok(Self::Proto {
                part: Some(Part::Init(proto::ProposalInit {
                    height: init.height.as_u64(),
                    round: init.round.as_u32().unwrap(),
                    proposer: Some(init.proposer.to_proto()?),
                })),
            }),
            Self::Data(data) => Ok(Self::Proto {
                part: Some(Part::Data(proto::ProposalData {
                    bytes: data.bytes.clone(),
                })),
            }),

            // NEW: Serialize blob sidecar
            Self::BlobSidecar(sidecar) => Ok(Self::Proto {
                part: Some(Part::BlobSidecar(proto::BlobSidecar {
                    index: sidecar.index as u32,
                    blob: sidecar.blob.data.clone(),
                    kzg_commitment: sidecar.kzg_commitment.0.to_vec(),
                    kzg_proof: sidecar.kzg_proof.0.to_vec(),
                })),
            }),

            Self::Fin(fin) => Ok(Self::Proto {
                part: Some(Part::Fin(proto::ProposalFin {
                    signature: Some(encode_signature(&fin.signature)),
                })),
            }),
        }
    }
}
```

#### 3.2 Update Protobuf Schema

**File**: `ultramarine/crates/proto/src/ultramarine.proto`

```protobuf
message ProposalPart {
  oneof part {
    ProposalInit init = 1;
    ProposalData data = 2;
    BlobSidecar blob_sidecar = 3;  // NEW
    ProposalFin fin = 4;
  }
}

message BlobSidecar {
  uint32 index = 1;
  bytes blob = 2;              // 131,072 bytes
  bytes kzg_commitment = 3;     // 48 bytes
  bytes kzg_proof = 4;          // 48 bytes
}
```

#### 3.3 Update Streaming Handler

**File**: `ultramarine/crates/node/src/app.rs` (lines 351-381)

```rust
// In ReceivedProposalPart handler
Effect::ReceivedProposalPart { from, part, msg_epoch } => {
    let height = self.ctx.height();

    match &part {
        ProposalPart::Init(init) => {
            tracing::info!(
                "Received proposal init from {}: height={}, round={}",
                from,
                init.height,
                init.round
            );

            // Initialize streaming state
            self.streaming_state.start_proposal(
                init.height,
                init.round,
                &init.proposer
            );
        }

        ProposalPart::Data(data) => {
            // Buffer execution payload data
            self.streaming_state.add_data_part(height, data.bytes.clone());
        }

        // NEW: Handle blob sidecar parts
        ProposalPart::BlobSidecar(sidecar) => {
            tracing::debug!(
                "Received blob sidecar from {}: index={}, size={}",
                from,
                sidecar.index,
                sidecar.size_bytes()
            );

            // Store blob temporarily (Phase 4 will add verification)
            self.streaming_state.add_blob_sidecar(height, sidecar.clone());
        }

        ProposalPart::Fin(fin) => {
            tracing::info!("Received proposal fin from {}", from);

            // Assemble complete proposal with blobs
            if let Some(proposal) = self.streaming_state.finalize_proposal(height) {
                // Phase 4: Add KZG verification here before marking available
                self.on_proposal_received(proposal).await;
            }
        }
    }

    vec![]
}
```

### Testing
```bash
# Test proposal part serialization
cargo nextest run -p ultramarine-types test_proposal_part_blob_sidecar

# Test streaming with blobs
cargo nextest run -p ultramarine-consensus test_blob_streaming
```

---

## Phase 4: Blob Verification & Storage (Days 6-7) ✅ COMPLETED

### Goal
Implement KZG cryptographic verification and store blobs in hot database.

**Status**: ✅ Complete - Full blob_engine crate with KZG verification and RocksDB storage
**Date Completed**: 2025-10-21

**Progress**:
- ✅ **Ethereum Compatibility Layer Complete** - BeaconBlockHeader types, SSZ merkleization, Merkle proofs
- ✅ **All Crypto Primitives Working** - SSZ hash_tree_root, Ed25519 signatures, 17-branch Merkle proofs
- ✅ **16/16 Tests Passing** - Comprehensive test coverage including known test vectors
- ✅ **Architecture Validated** - 4096 blob capacity (spec preset), 1024 blob target (practical)
- ✅ **KZG Verification** - Using c-kzg 2.1.0 (same as Lighthouse production)
- ✅ **BlobEngine Trait** - Clean abstraction with RocksDB backend
- ✅ **Integrated** - Wired into consensus State, used in proposal flow

### Files to Modify
- `ultramarine/crates/node/src/blob_verifier.rs` (new file)
- `ultramarine/crates/storage/src/blob_store.rs` (new file)
- `ultramarine/Cargo.toml` (add c-kzg dependency)

### Implementation

#### 4.1 Add KZG Verification

**File**: `ultramarine/crates/node/src/blob_verifier.rs` (new)

```rust
use c_kzg::{Blob as CKzgBlob, Bytes48, KzgCommitment as CKzgCommitment, KzgProof as CKzgProof, KzgSettings};
use std::sync::Arc;

use crate::types::{Blob, BlobSidecar, KzgCommitment, KzgProof, ValueMetadata};

/// Handles KZG proof verification for blob sidecars
pub struct BlobVerifier {
    kzg_settings: Arc<KzgSettings>,
}

impl BlobVerifier {
    /// Load trusted setup from file
    pub fn new(trusted_setup_path: &str) -> Result<Self, String> {
        let kzg_settings = KzgSettings::load_trusted_setup_file(trusted_setup_path)
            .map_err(|e| format!("Failed to load trusted setup: {:?}", e))?;

        Ok(Self {
            kzg_settings: Arc::new(kzg_settings),
        })
    }

    /// Verify a single blob sidecar
    pub fn verify_blob_sidecar(
        &self,
        sidecar: &BlobSidecar,
    ) -> Result<(), String> {
        let blob = CKzgBlob::from_bytes(&sidecar.blob.data)
            .map_err(|e| format!("Invalid blob: {:?}", e))?;

        let commitment = CKzgCommitment::from_bytes(&sidecar.kzg_commitment.0)
            .map_err(|e| format!("Invalid commitment: {:?}", e))?;

        let proof = CKzgProof::from_bytes(&sidecar.kzg_proof.0)
            .map_err(|e| format!("Invalid proof: {:?}", e))?;

        // Verify KZG proof
        let valid = KzgProof::verify_blob_kzg_proof(
            &blob,
            &commitment,
            &proof,
            &self.kzg_settings,
        )
        .map_err(|e| format!("KZG verification failed: {:?}", e))?;

        if !valid {
            return Err("KZG proof verification failed".to_string());
        }

        Ok(())
    }

    /// Batch verify multiple blob sidecars (5-10x faster than individual)
    pub fn verify_blob_sidecars_batch(
        &self,
        sidecars: &[BlobSidecar],
    ) -> Result<(), String> {
        if sidecars.is_empty() {
            return Ok(())
        }

        let blobs: Vec<CKzgBlob> = sidecars
            .iter()
            .map(|s| CKzgBlob::from_bytes(&s.blob.data))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("Invalid blob in batch: {:?}", e))?;

        let commitments: Vec<CKzgCommitment> = sidecars
            .iter()
            .map(|s| CKzgCommitment::from_bytes(&s.kzg_commitment.0))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("Invalid commitment in batch: {:?}", e))?;

        let proofs: Vec<CKzgProof> = sidecars
            .iter()
            .map(|s| CKzgProof::from_bytes(&s.kzg_proof.0))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("Invalid proof in batch: {:?}", e))?;

        // Batch verification (much faster than individual)
        let valid = KzgProof::verify_blob_kzg_proof_batch(
            &blobs,
            &commitments,
            &proofs,
            &self.kzg_settings,
        )
        .map_err(|e| format!("Batch KZG verification failed: {:?}", e))?;

        if !valid {
            return Err("Batch KZG proof verification failed".to_string());
        }

        Ok(())
    }

    /// Verify blobs match the commitments in value metadata
    pub fn verify_blobs_against_metadata(
        &self,
        sidecars: &[BlobSidecar],
        metadata: &ValueMetadata,
    ) -> Result<(), String> {
        // 1. Check count matches
        if sidecars.len() != metadata.blob_kzg_commitments.len() {
            return Err(format!(
                "Blob count mismatch: got {}, expected {}",
                sidecars.len(),
                metadata.blob_kzg_commitments.len()
            ));
        }

        // 2. Check indices are sequential
        for (i, sidecar) in sidecars.iter().enumerate() {
            if sidecar.index as usize != i {
                return Err(format!(
                    "Non-sequential blob index: expected {}, got {}",
                    i, sidecar.index
                ));
            }
        }

        // 3. Check commitments match metadata
        for (sidecar, expected_commitment) in sidecars.iter().zip(&metadata.blob_kzg_commitments) {
            if sidecar.kzg_commitment != *expected_commitment {
                return Err(format!(
                    "Commitment mismatch for blob index {}",
                    sidecar.index
                ));
            }
        }

        // 4. Batch verify KZG proofs
        self.verify_blob_sidecars_batch(sidecars)?;

        Ok(())
    }
}
```

#### 4.2 Add Blob Storage

**File**: `ultramarine/crates/storage/src/blob_store.rs` (new)

```rust
use bytes::Bytes;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;

use crate::types::{BlobSidecar, Height};

/// Storage table for blob sidecars
const BLOB_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blobs");

/// Key format: height (8 bytes) || index (1 byte)
fn blob_key(height: Height, index: u8) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0..8].copy_from_slice(&height.as_u64().to_be_bytes());
    key[8] = index;
    key
}

/// Store and retrieve blob sidecars
pub struct BlobStore {
    db: Database,
}

impl BlobStore {
    /// Open or create blob store at given path
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let db = Database::create(path)
            .map_err(|e| format!("Failed to open blob store: {:?}", e))?;

        // Create table if it doesn't exist
        let write_txn = db.begin_write() 
            .map_err(|e| format!("Failed to begin write: {:?}", e))?;
        {
            let _ = write_txn.open_table(BLOB_TABLE)
                .map_err(|e| format!("Failed to open table: {:?}", e))?;
        }
        write_txn.commit() 
            .map_err(|e| format!("Failed to commit: {:?}", e))?;

        Ok(Self { db })
    }

    /// Store blob sidecar
    pub fn store_blob(&self, height: Height, sidecar: &BlobSidecar) -> Result<(), String> {
        let write_txn = self.db.begin_write() 
            .map_err(|e| format!("Failed to begin write: {:?}", e))?;

        {
            let mut table = write_txn.open_table(BLOB_TABLE)
                .map_err(|e| format!("Failed to open table: {:?}", e))?;

            let key = blob_key(height, sidecar.index);
            let value = sidecar.to_bytes();

            table.insert(&key[..], value.as_ref())
                .map_err(|e| format!("Failed to insert blob: {:?}", e))?;
        }

        write_txn.commit() 
            .map_err(|e| format!("Failed to commit: {:?}", e))?;

        tracing::debug!(
            "Stored blob sidecar: height={}, index={}",
            height,
            sidecar.index
        );

        Ok(())
    }

    /// Store multiple blob sidecars in a single transaction
    pub fn store_blobs(&self, height: Height, sidecars: &[BlobSidecar]) -> Result<(), String> {
        let write_txn = self.db.begin_write() 
            .map_err(|e| format!("Failed to begin write: {:?}", e))?;

        {
            let mut table = write_txn.open_table(BLOB_TABLE)
                .map_err(|e| format!("Failed to open table: {:?}", e))?;

            for sidecar in sidecars {
                let key = blob_key(height, sidecar.index);
                let value = sidecar.to_bytes();

                table.insert(&key[..], value.as_ref())
                    .map_err(|e| format!("Failed to insert blob: {:?}", e))?;
            }
        }

        write_txn.commit() 
            .map_err(|e| format!("Failed to commit: {:?}", e))?;

        tracing::info!(
            "Stored {} blob sidecars at height {}",
            sidecars.len(),
            height
        );

        Ok(())
    }

    /// Retrieve blob sidecar
    pub fn get_blob(&self, height: Height, index: u8) -> Result<Option<BlobSidecar>, String> {
        let read_txn = self.db.begin_read() 
            .map_err(|e| format!("Failed to begin read: {:?}", e))?;

        let table = read_txn.open_table(BLOB_TABLE)
            .map_err(|e| format!("Failed to open table: {:?}", e))?;

        let key = blob_key(height, index);
        let value = table.get(&key[..])
            .map_err(|e| format!("Failed to get blob: {:?}", e))?;

        match value {
            Some(bytes) => {
                let sidecar = BlobSidecar::from_bytes(bytes.value())
                    .map_err(|e| format!("Failed to deserialize blob: {:?}", e))?;
                Ok(Some(sidecar))
            }
            None => Ok(None),
        }
    }

    /// Retrieve all blobs for a height
    pub fn get_blobs(&self, height: Height) -> Result<Vec<BlobSidecar>, String> {
        let read_txn = self.db.begin_read() 
            .map_err(|e| format!("Failed to begin read: {:?}", e))?;

        let table = read_txn.open_table(BLOB_TABLE)
            .map_err(|e| format!("Failed to open table: {:?}", e))?;

        let start_key = blob_key(height, 0);
        let end_key = blob_key(Height::new(height.as_u64() + 1), 0);

        let mut blobs = Vec::new();
        for entry in table.range(&start_key[..]..&end_key[..])
            .map_err(|e| format!("Failed to iterate: {:?}", e))? 
        {
            let (_, value) = entry.map_err(|e| format!("Failed to read entry: {:?}", e))?;
            let sidecar = BlobSidecar::from_bytes(value.value())
                .map_err(|e| format!("Failed to deserialize: {:?}", e))?;
            blobs.push(sidecar);
        }

        Ok(blobs)
    }

    /// Delete blobs at or before given height (for pruning)
    pub fn prune_before(&self, height: Height) -> Result<usize, String> {
        let write_txn = self.db.begin_write() 
            .map_err(|e| format!("Failed to begin write: {:?}", e))?;

        let mut count = 0;
        {
            let mut table = write_txn.open_table(BLOB_TABLE)
                .map_err(|e| format!("Failed to open table: {:?}", e))?;

            let end_key = blob_key(height, 0);

            // Collect keys to delete
            let keys_to_delete: Vec<Vec<u8>> = table
                .range(&[0u8][..]..&end_key[..])
                .map_err(|e| format!("Failed to iterate: {:?}", e))?
                .map(|entry| {
                    entry.map(|(k, _)| k.value().to_vec())
                        .map_err(|e| format!("Failed to read key: {:?}", e))
                })
                .collect::<Result<_, String>>()?;

            // Delete all keys
            for key in keys_to_delete {
                table.remove(&key[..])
                    .map_err(|e| format!("Failed to delete: {:?}", e))?;
                count += 1;
            }
        }

        write_txn.commit() 
            .map_err(|e| format!("Failed to commit: {:?}", e))?;

        tracing::info!("Pruned {} blob sidecars before height {}", count, height);

        Ok(count)
    }
}
```

#### 4.3 Integrate Verification into App

**File**: `ultramarine/crates/node/src/app.rs`

```rust
use crate::blob_verifier::BlobVerifier;
use crate::storage::BlobStore;

pub struct ConsensusApp {
    // ... existing fields
    blob_verifier: BlobVerifier,
    blob_store: BlobStore,
}

impl ConsensusApp {
    pub fn new(config: Config) -> Result<Self, String> {
        let blob_verifier = BlobVerifier::new(&config.trusted_setup_path)?;
        let blob_store = BlobStore::open(&config.blob_store_path)?;

        Ok(Self {
            // ... existing initialization
            blob_verifier,
            blob_store,
        })
    }

    // Updated handler from Phase 3
    async fn on_proposal_received(&mut self, proposal: Proposal) {
        let height = proposal.height;
        let sidecars = proposal.blob_sidecars;

        // Verify blobs against metadata
        if let Err(e) = self.blob_verifier.verify_blobs_against_metadata(
            &sidecars,
            &proposal.value.metadata,
        ) {
            tracing::warn!(
                "Blob verification failed for height {}: {}",
                height,
                e
            );
            return; // Reject proposal
        }

        // Store verified blobs
        if let Err(e) = self.blob_store.store_blobs(height, &sidecars) {
            tracing::error!("Failed to store blobs: {}", e);
            return;
        }

        tracing::info!(
            "Verified and stored {} blobs at height {}",
            sidecars.len(),
            height
        );

        // Mark data as available and proceed with consensus vote
        self.mark_proposal_available(proposal);
    }
}
```

#### 4.4 Add Dependencies

**File**: `ultramarine/Cargo.toml`

```toml
[dependencies]
c-kzg = "1.0"
```

### Testing
```bash
# Test KZG verification
cargo nextest run -p ultramarine-node test_blob_verification

# Test blob storage
cargo nextest run -p ultramarine-storage test_blob_store

# Integration test with streaming
cargo nextest run -p ultramarine-node test_full_blob_flow
```

---

## Phase 5: Block Import / EL Interaction (Days 8-9) ⚠️ PARTIAL

### Goal
Modify `Decided` handler to validate blob availability before block import.

**Status**: ⚠️ **Live Consensus Complete - Sync Broken**
**Date Started**: 2025-10-21
**Date Completed**: 2025-10-21 (live consensus only)

**✅ Completed (Live Consensus)**:
- Blob lifecycle (mark_decided, drop_round, pruning)
- Error handling fixes (#1, #2, #3)
- Blob availability validation in Decided handler
- Blob count verification before import
- **Versioned hash verification (Lighthouse parity)**
- Engine API v3 integration (was already implemented)

**❌ Missing (State Sync)** - 🚨 **CRITICAL GAPS**:
- ProcessSyncedValue - No blob retrieval/storage
- GetDecidedValue - No blobs in response
- RestreamProposal - Not implemented

**Key Discovery**: Engine API v3 was already fully implemented. We only needed to add blob availability validation and versioned hash verification. However, critical code review revealed that **state synchronization is completely broken** for blocks with blobs.

**See**:
- `docs/PHASE_5_COMPLETION.md` - Live consensus completion details
- `docs/LIGHTHOUSE_PARITY_COMPLETE.md` - Versioned hash verification
- `docs/BLOB_SYNC_GAP_ANALYSIS.md` - **Critical sync gaps and fixes**

### Files to Modify
- `ultramarine/crates/node/src/app.rs` (lines 404-624)
- `ultramarine/crates/execution/src/client.rs`

### Implementation

#### 5.1 Extend newPayload to Include Blobs

**File**: `ultramarine/crates/execution/src/client.rs`

```rust
impl ExecutionClient {
    /// Import block with blob sidecars (Engine API v3)
    pub async fn new_payload_with_blobs(
        &self,
        payload: ExecutionPayload,
        versioned_hashes: Vec<[u8; 32]>,
        parent_beacon_block_root: [u8; 32],
    ) -> Result<PayloadStatus, ExecutionError> {
        // Check capability
        let capabilities = self.check_capabilities().await?;
        if !capabilities.supports_engine_api_v3() {
            // Fallback to v2 if blobs not supported
            return self.new_payload_v2(payload, parent_beacon_block_root).await;
        }

        // Call engine_newPayloadV3
        let response: serde_json::Value = self
            .call_engine_api(
                "engine_newPayloadV3",
                json!([
                    payload.to_json(),
                    versioned_hashes
                        .iter()
                        .map(|h| format!("0x{}", hex::encode(h)))
                        .collect::<Vec<_>>(),
                    format!("0x{}", hex::encode(parent_beacon_block_root)),
                ]),
            )
            .await?;

        self.parse_payload_status(response)
    }

    fn parse_payload_status(
        &self,
        json: serde_json::Value,
    ) -> Result<PayloadStatus, ExecutionError> {
        let status = json["status"]
            .as_str()
            .ok_or(ExecutionError::InvalidResponse)?;

        match status {
            "VALID" => Ok(PayloadStatus::Valid),
            "INVALID" => Ok(PayloadStatus::Invalid),
            "SYNCING" => Ok(PayloadStatus::Syncing),
            _ => Err(ExecutionError::InvalidResponse),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadStatus {
    Valid,
    Invalid,
    Syncing,
}
```

#### 5.2 Update Decided Handler

**File**: `ultramarine/crates/node/src/app.rs` (lines 404-624)

```rust
Effect::Decided { certificate } => {
    let height = certificate.height;
    let value = &certificate.value;

    tracing::info!(
        "Block decided at height {}: {} blobs",
        height,
        value.metadata.blob_count
    );

    // 1. Retrieve stored blobs
    let blobs = match self.blob_store.get_blobs(height) {
        Ok(blobs) => blobs,
        Err(e) => {
            tracing::error!("Failed to retrieve blobs for import: {}", e);
            return vec![];
        }
    };

    // 2. Verify blob count matches
    if blobs.len() != value.metadata.blob_count as usize {
        tracing::error!(
            "Blob count mismatch: stored {}, expected {}",
            blobs.len(),
            value.metadata.blob_count
        );
        return vec![];
    }

    // 3. Calculate versioned hashes for EL
    let versioned_hashes = value
        .metadata
        .blob_kzg_commitments
        .iter()
        .map(|commitment| {
            use sha2::{Digest, Sha256};
            let mut hash = Sha256::digest(&commitment.0);
            hash[0] = 0x01; // BLOB_COMMITMENT_VERSION_KZG
            hash.into()
        })
        .collect::<Vec<[u8; 32]>>();

    // 4. Import to execution layer with blobs
    let parent_beacon_block_root = self.compute_beacon_block_root(height);

    match self
        .execution_client
        .new_payload_with_blobs(
            value.metadata.execution_payload_header.to_full_payload(),
            versioned_hashes,
            parent_beacon_block_root,
        )
        .await
    {
        Ok(PayloadStatus::Valid) => {
            tracing::info!("Block imported successfully with {} blobs", blobs.len());

            // Mark block as finalized in our state
            self.finalize_block(height);
        }
        Ok(PayloadStatus::Invalid) => {
            tracing::error!("Execution layer rejected block at height {}", height);
            // Handle invalid block (potential slashing evidence)
        }
        Ok(PayloadStatus::Syncing) => {
            tracing::warn!("Execution layer still syncing, will retry");
            // Queue for retry
        }
        Err(e) => {
            tracing::error!("Failed to import block: {:?}", e);
        }
    }

    vec![]
}
```

### Testing
```bash
# Test EL integration with blobs
cargo nextest run -p ultramarine-execution test_new_payload_with_blobs

# Integration test: consensus → storage → EL
cargo nextest run -p ultramarine-node test_full_block_import
```

---

### ⚠️ **CRITICAL GAPS DISCOVERED - State Synchronization**

**Date Discovered**: 2025-10-21
**Status**: 🚨 **BLOCKS PRODUCTION DEPLOYMENT**
**Severity**: **CRITICAL** - Breaks data availability for syncing peers

#### Overview

While Phase 5 is **100% complete for live consensus**, a thorough code review revealed **critical gaps in state synchronization** that break blob data availability when peers fall behind or join the network.

**Core Issue**: Syncing peers receive Value metadata but NOT blob data, making it impossible to import blocks with blob transactions.

#### What Works ✅

**Live Consensus** (when all peers are online and up-to-date):
- ✅ Blob streaming via ProposalParts gossip
- ✅ KZG verification and storage
- ✅ Versioned hash verification (Lighthouse parity)
- ✅ Block import with blob availability checks
- ✅ Blob lifecycle (mark_decided, drop_round, pruning)

**Files**: `crates/node/src/app.rs:98-580`, `crates/consensus/src/state.rs:589-694`

#### Critical Gaps ❌

##### 1. **ProcessSyncedValue** - NO BLOB SYNC 🚨

**File**: `crates/node/src/app.rs:591-609`

**Problem**: When a peer falls behind and syncs, it receives **only Value metadata** - NO blobs!

```rust
AppMsg::ProcessSyncedValue { height, round, proposer, value_bytes, reply } => {
    let value = decode_value(value_bytes);  // ← Only metadata!

    // ❌ NO blob retrieval from peer
    // ❌ NO blob storage
    // ❌ Decided handler will CRASH when trying to import (no blobs!)

    if reply.send(ProposedValue { ... }).is_err() { ... }
}
```

**Impact**:
- New peers **cannot join** the network
- Peers that fall behind **cannot catch up**
- Network becomes **non-functional** after first missed block with blobs

**Required Fix**:
1. Add RPC method: `request_blobs_for_height(peer, height, round)`
2. Request blobs when syncing value with commitments
3. Verify and store blobs via `blob_engine.verify_and_store()`
4. Mark blobs as decided immediately via `blob_engine.mark_decided(height, round)` (synced values are already decided)

**Estimated Time**: 1 day

---

##### 2. **GetDecidedValue** - NO BLOBS IN RESPONSE 🚨

**File**: `crates/node/src/app.rs:669-683`

**Problem**: When helping peers catch up, we send **only Value metadata** - NO blobs!

```rust
AppMsg::GetDecidedValue { height, reply } => {
    let decided_value = state.get_decided_value(height).await;

    let raw_decided_value = decided_value.map(|dv| RawDecidedValue {
        certificate: dv.certificate,
        value_bytes: ProtobufCodec.encode(&dv.value).expect("..."),
        // ❌ NO blob data included!
    });

    if reply.send(raw_decided_value).is_err() { ... }
}
```

**Impact**:
- Peers requesting historical blocks get **incomplete data**
- Peer-to-peer sync is **broken**
- Cannot reconstruct full block for import

**Required Fix**:
1. Retrieve blobs via `blob_engine.get_for_import(height)` (add a helper if needed)
2. Extend `RawDecidedValue` to include blobs (or use separate channel)
3. Send full execution payload bytes + blob sidecars

**Alternative**: Add separate RPC `GetBlobsForHeight(height) → Vec<BlobSidecar>`

**Estimated Time**: 4 hours

---

##### 3. **RestreamProposal** - NOT IMPLEMENTED ❌

**File**: `crates/node/src/app.rs:292-355`

**Problem**: Entire handler is commented out!

```rust
AppMsg::RestreamProposal { ... } => {
    error!("🔴 RestreamProposal not implemented");
    // 63 lines of commented-out code
}
```

**Impact**:
- Validators who miss blob parts during gossip cannot recover
- Causes unnecessary consensus timeouts and new rounds
- Degrades network performance under poor network conditions

**Required Fix**:
1. Retrieve proposal and blobs from stores
2. Reconstruct BlobsBundle from sidecars
3. Re-stream all parts via `stream_proposal()`
4. Modify `stream_proposal()` to accept original proposer address

**Estimated Time**: 4 hours

---

#### Impact Assessment

| Scenario | Works? | Severity |
|----------|--------|----------|
| Live consensus (all peers online) | ✅ Yes | None |
| Peer falls behind 1 block with blobs | ❌ No | **CRITICAL** |
| New peer joins network | ❌ No | **CRITICAL** |
| Peer misses blob gossip | ❌ No | **HIGH** |
| 100% uptime, no failures | ✅ Yes | Works only in ideal conditions |

#### Detailed Analysis

**See**: `docs/BLOB_SYNC_GAP_ANALYSIS.md` for:
- Complete code review of all sync handlers
- Step-by-step fix implementations (ready to copy/paste)
- Test scenarios and integration tests needed
- Comparison with Lighthouse sync mechanisms
- Network partition recovery requirements

#### Recommended Action Plan

**Phase 5.1: Critical Sync Fixes** (1-2 days)

**Priority 1** (CRITICAL - Blocks deployment):
1. Fix `ProcessSyncedValue` - Add blob sync mechanism (1 day)
2. Fix `GetDecidedValue` - Include blobs in response (4 hours)

**Priority 2** (HIGH - Performance):
3. Implement `RestreamProposal` - Enable gossip recovery (4 hours)

**Priority 3** (MEDIUM - Optimization):
4. Add blob RPC methods - `GetBlobsByRange`, `GetBlobsByRoot` (1 day, optional)

**Total Time**: 1-2 days for critical fixes

---

**⚠️ DEPLOYMENT WARNING**: **DO NOT deploy to production** until sync is fixed. Network will fail as soon as any peer falls behind or new peers join.

**Status Change**: Phase 5 changed from "✅ COMPLETE" to "⚠️ COMPLETE (Live Consensus Only - Sync Broken)"

---

## Phase 6: Pruning Policy (Day 12)

### Goal
Implement pruning to delete old blobs after retention period (e.g., 4096 epochs / 18 days).

### Files to Modify
- `ultramarine/crates/node/src/pruning.rs` (new file)
- `ultramarine/crates/node/src/app.rs`

### Implementation

#### 6.1 Pruning Service

**File**: `ultramarine/crates/node/src/pruning.rs` (new)

```rust
use std::time::Duration;
use tokio::time::interval;

use crate::storage::BlobStore;
use crate::types::Height;

/// Configuration for blob pruning
#[derive(Clone, Debug)]
pub struct PruningConfig {
    /// Retain blobs for this many slots
    pub retention_slots: u64,

    /// How often to run pruning (e.g., every 100 slots)
    pub pruning_interval_slots: u64,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            retention_slots: 4096 * 32, // 4096 epochs * 32 slots/epoch = 18 days
            pruning_interval_slots: 100,
        }
    }
}

/// Background task that prunes old blob sidecars
pub struct PruningService {
    blob_store: BlobStore,
    config: PruningConfig,
}

impl PruningService {
    pub fn new(blob_store: BlobStore, config: PruningConfig) -> Self {
        Self { blob_store, config }
    }

    /// Start pruning service (runs in background)
    pub async fn run(self, mut current_height_rx: tokio::sync::watch::Receiver<Height>) {
        let slot_duration = Duration::from_secs(12); // Ethereum slot time
        let interval_duration =
            slot_duration * self.config.pruning_interval_slots as u32;

        let mut ticker = interval(interval_duration);

        loop {
            ticker.tick().await;

            // Get current height
            let current_height = *current_height_rx.borrow();

            // Calculate pruning threshold
            let prune_before_height = if current_height.as_u64() > self.config.retention_slots {
                Height::new(current_height.as_u64() - self.config.retention_slots)
            } else {
                continue; // Not enough history yet
            };

            // Prune old blobs
            match self.blob_store.prune_before(prune_before_height) {
                Ok(count) => {
                    tracing::info!(
                        "Pruned {} blob sidecars before height {}",
                        count,
                        prune_before_height
                    );
                }
                Err(e) => {
                    tracing::error!("Pruning failed: {}", e);
                }
            }
        }
    }
}
```

#### 6.2 Integrate into App

**File**: `ultramarine/crates/node/src/app.rs`

```rust
use crate::pruning::{PruningService, PruningConfig};

pub struct ConsensusApp {
    // ... existing fields
    current_height_tx: tokio::sync::watch::Sender<Height>,
}

impl ConsensusApp {
    pub fn new(config: Config) -> Result<Self, String> {
        // ... existing initialization

        // Create height broadcaster for pruning service
        let (current_height_tx, current_height_rx) =
            tokio::sync::watch::channel(Height::new(0));

        // Spawn pruning service
        let pruning_service = PruningService::new(
            blob_store.clone(),
            PruningConfig::default(),
        );

        tokio::spawn(async move {
            pruning_service.run(current_height_rx).await;
        });

        Ok(Self {
            // ... existing fields
            current_height_tx,
        })
    }

    // Update height when blocks are decided
    fn finalize_block(&mut self, height: Height) {
        // ... existing finalization logic

        // Notify pruning service
        let _ = self.current_height_tx.send(height);
    }
}
```

### Testing
```bash
# Test pruning logic
cargo nextest run -p ultramarine-node test_blob_pruning

# Test retention period calculation
cargo nextest run -p ultramarine-node test_pruning_config
```

---

## Phase 7: Archive Integration (Days 13-14) - OPTIONAL

### Goal
Publish blob CIDs to archive and include in vote extensions for network propagation.

### Files to Modify
- `ultramarine/crates/archive/src/publisher.rs` (new file)
- `ultramarine/crates/types/src/vote.rs`

### Implementation

#### 7.1 Archive Publisher

**File**: `ultramarine/crates/archive/src/publisher.rs` (new)

```rust
use cid::Cid;
use std::collections::HashMap;

use crate::types::{BlobSidecar, Height};

/// Publishes blob sidecars to archive and tracks CIDs
pub struct ArchivePublisher {
    archive_client: ArchiveClient,
    cid_cache: HashMap<Height, Vec<Cid>>,
}

impl ArchivePublisher {
    pub fn new(archive_endpoint: String) -> Self {
        Self {
            archive_client: ArchiveClient::new(archive_endpoint),
            cid_cache: HashMap::new(),
        }
    }

    /// Publish blobs to archive and return CIDs
    pub async fn publish_blobs(
        &mut self,
        height: Height,
        sidecars: &[BlobSidecar],
    ) -> Result<Vec<Cid>, String> {
        let mut cids = Vec::new();

        for sidecar in sidecars {
            let cid = self
                .archive_client
                .upload_blob(&sidecar.blob.data)
                .await
                .map_err(|e| format!("Archive upload failed: {:?}", e))?;

            cids.push(cid);
        }

        // Cache CIDs for vote extension
        self.cid_cache.insert(height, cids.clone());

        tracing::info!(
            "Published {} blobs to archive at height {}: {:?}",
            sidecars.len(),
            height,
            cids
        );

        Ok(cids)
    }

    /// Get cached CIDs for vote extension
    pub fn get_cids(&self, height: Height) -> Option<&[Cid]> {
        self.cid_cache.get(&height).map(|v| v.as_slice())
    }
}
```

#### 7.2 Extend Vote with CIDs

**File**: `ultramarine/crates/types/src/vote.rs`

```rust
use cid::Cid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    // ... existing fields

    /// Optional vote extension with blob CIDs
    pub extension: Option<VoteExtension>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteExtension {
    /// CIDs of blobs published to archive
    pub blob_cids: Vec<Cid>,
}

impl VoteExtension {
    pub fn new(blob_cids: Vec<Cid>) -> Self {
        Self { blob_cids }
    }

    pub fn to_bytes(&self) -> Bytes {
        // Serialize CIDs for gossip
        bincode::serialize(&self.blob_cids).unwrap().into()
    }
}
```

### Testing
```bash
# Test archive upload
cargo nextest run -p ultramarine-archive test_blob_upload

# Test vote extension serialization
cargo nextest run -p ultramarine-types test_vote_extension
```

---

## Phase 8: Testing (Days 15-17)

### Goal
Comprehensive testing across all layers: unit, integration, and end-to-end.

### Test Suite

#### 8.1 Unit Tests

```bash
# Execution bridge
cargo nextest run -p ultramarine-execution

# Value metadata
cargo nextest run -p ultramarine-types test_value_metadata

# Proposal part serialization
cargo nextest run -p ultramarine-types test_proposal_part_blob

# KZG verification
cargo nextest run -p ultramarine-node test_blob_verification

# Blob storage
cargo nextest run -p ultramarine-storage test_blob_store

# Pruning
cargo nextest run -p ultramarine-node test_pruning
```

#### 8.2 Integration Tests

**File**: `ultramarine/crates/node/tests/integration_blob_flow.rs` (new)

```rust
use ultramarine_node::*;
use ultramarine_types::*;

#[tokio::test] async fn test_full_blob_flow() {
    // 1. Setup mock execution layer with blob bundle
    let mock_el = MockExecutionLayer::new()
        .with_blobs(6) // 6 blobs for Deneb
        .with_commitments()
        .build();

    // 2. Create consensus app
    let app = ConsensusApp::new(test_config()).await.unwrap();

    // 3. Trigger GetValue (proposer generates block)
    let (payload, blobs_bundle) = app
        .execution_client
        .generate_block_with_blobs(1, GENESIS_HASH, timestamp())
        .await
        .unwrap();

    assert_eq!(blobs_bundle.unwrap().blobs.len(), 6);

    // 4. Stream proposal parts
    let parts = app.create_proposal_parts(payload, blobs_bundle).await;
    assert_eq!(parts.len(), 8); // Init + 6 BlobSidecar + Fin

    // 5. Simulate receiving parts
    for part in parts {
        app.handle_proposal_part(part).await;
    }

    // 6. Verify blobs stored
    let stored_blobs = app.blob_store.get_blobs(Height::new(1)).unwrap();
    assert_eq!(stored_blobs.len(), 6);

    // 7. Simulate consensus decision
    app.handle_decided(certificate).await;

    // 8. Verify EL received blobs
    assert!(mock_el.received_payload_with_blobs());
}
```

#### 8.3 Network Simulation

**File**: `ultramarine/crates/node/tests/network_blob_gossip.rs` (new)

```rust
#[tokio::test] async fn test_blob_gossip_propagation() {
    // Setup 4-node network
    let network = TestNetwork::new(4).await;

    // Node 0 is proposer
    let proposer = &network.nodes[0];
    let validator1 = &network.nodes[1];
    let validator2 = &network.nodes[2];

    // Proposer streams blobs
    proposer.propose_with_blobs(6).await;

    // Wait for gossip propagation
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify all validators received blobs
    assert!(validator1.has_complete_proposal());
    assert!(validator2.has_complete_proposal());

    // Verify KZG verification passed
    assert!(validator1.blob_verification_passed());
    assert!(validator2.blob_verification_passed());
}
```

#### 8.4 Benchmark Tests

**File**: `ultramarine/benches/blob_verification.rs` (new)

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_kzg_verification(c: &mut Criterion) {
    let verifier = setup_blob_verifier();
    let sidecar = create_test_blob_sidecar();

    c.bench_function("kzg_verify_single", |b| {
        b.iter(|| {
            verifier.verify_blob_sidecar(black_box(&sidecar)).unwrap()
        })
    });

    let sidecars = vec![create_test_blob_sidecar(); 6];

    c.bench_function("kzg_verify_batch_6", |b| {
        b.iter(|| {
            verifier.verify_blob_sidecars_batch(black_box(&sidecars)).unwrap()
        })
    });
}

criterion_group!(benches, bench_kzg_verification);
criterion_main!(benches);
```

Run benchmarks:
```bash
cargo bench --bench blob_verification
```

#### 8.5 Local Testnet

**File**: `scripts/local_testnet_with_blobs.sh` (new)

```bash
#!/bin/bash
# Launch local testnet with blob support

# 1. Start Kurtosis testnet with 4 nodes
kurtosis run github.com/kurtosis-tech/ethereum-package \
  --args-file ./configs/local-testnet-blobs.yaml

# 2. Configure blob transactions
cast send --private-key $PRIVATE_KEY \
  --blob-file ./test_data/test_blob.bin \
  --to $CONTRACT_ADDRESS

# 3. Monitor blob propagation
./scripts/monitor_blob_gossip.sh

# 4. Verify all nodes received blobs
./scripts/verify_blob_availability.sh
```

### Testing Timeline

| Day | Focus | Commands |
|-----|-------|----------|
| 13 | Unit tests | `make test` |
| 14 | Integration tests | `cargo nextest run --test integration_*` |
| 15 | Local testnet + benchmarks | `./scripts/local_testnet_with_blobs.sh` |

---

## Critical Success Factors

### 1. Value Refactor (Phase 2) ✅ CRITICAL
**Why**: If consensus votes on full blob data, messages balloon to MBs. This breaks Malachite's performance assumptions.

**Validation**: After Phase 2, measure Value message size:
```rust
assert!(value.size_bytes() < 3000); // Must be < 3KB
```

### 2. Batch KZG Verification (Phase 4) ✅ PERFORMANCE
**Why**: Verifying 6 blobs individually takes ~60ms. Batch verification reduces to ~12ms (5x speedup).

**Implementation**: Always use `verify_blob_sidecars_batch()` in production.

### 3. Single Topic Architecture ✅ SIMPLE
**Why**: Using existing `/proposal_parts` channel avoids modifying Malachite library.

**Trade-off**: For 64MB blobs, may need separate topic. Current plan supports up to ~1MB (9 blobs).

---

## Timeline Summary

```
Week 1: Core Infrastructure
  Day 1-2:  Phase 1 (Execution bridge)
  Day 3-4:  Phase 2 (Value refactor) ← CRITICAL
  Day 5:    Phase 3 (Proposal streaming)

Week 2: Verification & Integration
  Day 6-7:  Phase 4 (KZG verification + storage)
  Day 8-9:  Phase 5 (Block import)
  Day 10:   Phase 6 (Pruning)

Week 3: Polish & Testing (Optional Archive)
  Day 11-12: Phase 7 (Archive - optional)
  Day 13-15: Phase 8 (Testing)
```

**MVP Completion**: End of Day 10 (2 weeks)
**Full Feature Set**: End of Day 15 (3 weeks)

---

## Bandwidth Analysis

**Per-block overhead**:
- 6 blobs × 128KB = 768KB
- KZG proofs: 6 × 48 bytes = 288 bytes
- Total: ~768KB per block

**Network impact**:
- Block time: 12 seconds
- Bandwidth: 768KB / 12s = 64KB/s per validator
- For 100 validators: 6.4MB/s total network
- Modern networks (1Gbps): Only 5% utilization

**Conclusion**: Bandwidth is NOT a bottleneck for 6-9 blobs.

---

## Next Steps

1. **Review this plan** with your team
2. **Set up development environment**:
   ```bash
   # Install Rust toolchain
   rustup update stable

   # Clone Ultramarine
   cd /path/to/ultramarine

   # Install c-kzg
   cargo add c-kzg

   # Download trusted setup
   curl -O https://trusted-setup-eu.s3.amazonaws.com/
   ```

3. **Begin Phase 1**: Execution bridge implementation
4. **Daily sync**: Review progress, adjust timeline as needed

---

## Questions or Blockers?

If you encounter issues:
1. **Check BLOB_SIDECAR_TECHNICAL_REPORT.md** (sections 1-9 for deep background)
2. **Reference Lighthouse**: `lighthouse/consensus/types/src/blob_sidecar.rs`
3. **Malachite docs**: Understand Context trait and channel architecture
4. **Ask team**: Networking experts for gossip optimization

**Ready to start implementation!** 🚀

---
### Appendix: EIP-4844 Blob Sidecar Implementation Map

This document provides an index of the core logic for EIP-4844 blob sidecar implementation across four major consensus clients. It is intended to serve as a quick reference for developers to locate relevant data structures, verification logic, and networking components. Each section includes references to the Deneb consensus specs.

---


### 1. Lighthouse (Rust)

Lighthouse's implementation is clean, modular, and a strong reference for idiomatic Rust.

*   **Core Data Structure (`BlobSidecar`)**
    *   **File:** `lighthouse/consensus/types/src/blob_sidecar.rs`
    *   **Description:** Defines the primary `BlobSidecar<E: EthSpec>` struct.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `BlobSidecar`)

*   **Verification Logic**
    *   **File:** `lighthouse/beacon_node/beacon_chain/src/blob_verification.rs`
    *   **Description:** The central hub for all blob sidecar validation. The `validate_blob_sidecar_for_gossip` function orchestrates the entire verification pipeline.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `Validations`)

*   **Networking**
    *   **Gossip Handler:** Logic is integrated within the `beacon_chain` and `network` modules, with `blob_verification.rs` being the entry point for gossiped data.
    *   **RPC Handlers:**
        *   `lighthouse/beacon_node/network/src/sync/network_context/requests/blobs_by_range.rs`
        *   `lighthouse/beacon_node/network/src/sync/network_context/requests/blobs_by_root.rs`
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (sections: `BlobSidecarsByRange v1`, `BlobSidecarsByRoot v1`)

*   **Cryptographic Helpers**
    *   **KZG Proof Verification:** `lighthouse/beacon_node/beacon_chain/src/kzg_utils.rs`
    *   **Spec Reference:** `consensus-specs/specs/deneb/polynomial-commitments.md` (section: `verify_blob_kzg_proof`)
    *   **Inclusion Proof Verification:** `lighthouse/consensus/types/src/blob_sidecar.rs` (in `verify_blob_sidecar_inclusion_proof`)
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `verify_blob_sidecar_inclusion_proof`)

---


### 2. Prysm (Go)

Prysm's implementation stands out for its highly structured and explicit approach to verification, making it an excellent architectural reference.

*   **Core Data Structure (`BlobSidecar`)**
    *   **File:** `prysm/proto/prysm/v1alpha1/blobs.pb.go`
    *   **Description:** Defines the `BlobSidecar` struct, generated from a Protobuf definition.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `BlobSidecar`)

*   **Verification Logic**
    *   **File:** `prysm/beacon-chain/verification/blob.go`
    *   **Description:** Defines the verification "requirements" as an enum (`Requirement`). The `ROBlobVerifier` struct applies these requirements.
    *   **File:** `prysm/beacon-chain/sync/validate_blob.go`
    *   **Description:** This file uses the `ROBlobVerifier` to validate blobs in the context of network sync.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `Validations`)

*   **Networking**
    *   **Gossip Handler:** `prysm/beacon-chain/sync/subscriber_blob_sidecar.go`
    *   **RPC Handlers:**
        *   `prysm/beacon-chain/sync/rpc_blob_sidecars_by_range.go`
        *   `prysm/beacon-chain/sync/rpc_blob_sidecars_by_root.go`
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (sections: `BlobSidecarsByRange v1`, `BlobSidecarsByRoot v1`)

*   **Cryptographic Helpers**
    *   **KZG Proof Verification:** `prysm/beacon-chain/sync/verify/blob.go`
    *   **Spec Reference:** `consensus-specs/specs/deneb/polynomial-commitments.md` (section: `verify_blob_kzg_proof`)
    *   **Inclusion Proof Verification:** `prysm/consensus-types/blocks/roblob.go` (in `VerifyKZGInclusionProof`)
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `verify_blob_sidecar_inclusion_proof`)

---


### 3. Lodestar (TypeScript)

Lodestar's implementation is a clear and readable example of the specification in a TypeScript/Node.js environment.

*   **Core Data Structure (`BlobSidecar`)**
    *   **File:** `lodestar/packages/types/src/deneb/ssz.ts`
    *   **Description:** Defines the SSZ schema and TypeScript type for the `BlobSidecar`.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `BlobSidecar`)

*   **Verification Logic**
    *   **File:** `lodestar/packages/beacon-node/src/chain/validation/blobSidecar.ts`
    *   **Description:** Contains the primary validation function, `validateGossipBlobSidecar`.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `Validations`)

*   **Networking**
    *   **Gossip Handler:** Logic is integrated into the chain validation process, starting with `validateGossipBlobSidecar`.
    *   **RPC Handlers:**
        *   `lodestar/packages/beacon-node/src/network/reqresp/handlers/blobSidecarsByRange.ts`
        *   `lodestar/packages/beacon-node/src/network/reqresp/handlers/blobSidecarsByRoot.ts`
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (sections: `BlobSidecarsByRange v1`, `BlobSidecarsByRoot v1`)

*   **Cryptographic Helpers**
    *   **KZG Proof Verification:** `lodestar/packages/beacon-node/src/chain/validation/blobSidecar.ts` (in `validateBlobsAndBlobProofs`)
    *   **Spec Reference:** `consensus-specs/specs/deneb/polynomial-commitments.md` (section: `verify_blob_kzg_proof`)
    *   **Inclusion Proof Verification:** `lodestar/packages/beacon-node/src/chain/validation/blobSidecar.ts` (in `validateBlobSidecarInclusionProof`)
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `verify_blob_sidecar_inclusion_proof`)

---


### 4. Grandine (Rust)

Grandine's implementation is the most complex, using a highly asynchronous, task-based architecture. It's a good reference for advanced performance optimization.

*   **Core Data Structure (`BlobSidecar`)**
    *   **File:** `grandine/types/src/deneb/containers.rs`
    *   **Description:** Defines the `BlobSidecar<P: Preset>` struct.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `BlobSidecar`)

*   **Verification Logic**
    *   **File:** `grandine/fork_choice_store/src/store.rs`
    *   **Description:** The core validation logic is within the `Store` struct, specifically in the `validate_blob_sidecar_with_state` function.
    *   **Task Runner:** `grandine/fork_choice_control/src/tasks.rs` defines `BlobSidecarTask`, which wraps the call to the validation logic.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `Validations`)

*   **Networking**
    *   **Gossip Dispatcher:** `grandine/p2p/src/block_sync_service.rs`
    *   **RPC Handlers:** Logic is managed within `grandine/p2p/src/network.rs`.
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (sections: `BlobSidecarsByRange v1`, `BlobSidecarsByRoot v1`)

*   **Cryptographic Helpers**
    *   **KZG Proof Verification:** `grandine/fork_choice_store/src/store.rs` (calls `kzg_utils::eip_4844::verify_blob_kzg_proof`)
    *   **Spec Reference:** `consensus-specs/specs/deneb/polynomial-commitments.md` (section: `verify_blob_kzg_proof`)
    *   **Inclusion Proof Verification:** `grandine/fork_choice_store/src/store.rs` (calls `predicates::is_valid_blob_sidecar_inclusion_proof`)
    *   **Spec Reference:** `consensus-specs/specs/deneb/p2p-interface.md` (section: `verify_blob_sidecar_inclusion_proof`)

---

## Implementation Progress

**Overall Status**: ⚠️ **Live Consensus Ready - Sync Broken** - 🚨 **NOT PRODUCTION READY**

**Completion Summary**:
- ✅ Phase 1: Execution ↔ Consensus Bridge (100%)
- ✅ Phase 2: Consensus Value Refactor (100%)
- ✅ Phase 3: Proposal Streaming (100%)
- ✅ Phase 4: Blob Verification & Storage (100%)
- ⚠️ Phase 5: Block Import / EL Interaction (70% - **Sync broken**)
  - ✅ Live consensus complete with Lighthouse parity
  - ❌ **Critical**: ProcessSyncedValue missing blob sync
  - ❌ **Critical**: GetDecidedValue missing blobs
  - ❌ **High**: RestreamProposal not implemented
- ⏳ Phase 5.1: State Sync Fixes (0% - **URGENT**)
- ⏳ Phase 6: Pruning Policy (0% - Pending)
- ⏳ Phase 7: Archive Integration (0% - Optional)
- ⏳ Phase 8: Testing (0% - Pending)

**Progress**: **4.7/8 phases complete** (59% of total plan)

**Key Milestone**: Live consensus complete with Lighthouse security parity

**🚨 CRITICAL BLOCKER**: State synchronization broken - new peers cannot join, lagging peers cannot catch up. **See Phase 5 sync gaps section for details.**

---

### Phase 1: Execution ↔ Consensus Bridge ✅ COMPLETED (2025-10-15)

**Status**: All tasks completed and tested

#### 1.1 Blob Types ✅
- **File**: `ultramarine/crates/types/src/blob.rs` (NEW - 960 lines)
- **Implemented**:
  - `Blob` structure with 131,072-byte size validation
  - `KzgCommitment` and `KzgProof` types (48 bytes each)
  - `BlobsBundle` container with validation methods
  - Custom Serialize/Deserialize for 48-byte arrays
  - `versioned_hashes()` using Alloy's `kzg_to_versioned_hash`
  - `TryFrom<BlobsBundleV1>` for Engine API response parsing
- **Tests**: 7 tests passing (blob validation, bundle conversion, versioned hashes)

#### 1.2 ExecutionPayloadHeader ✅
- **File**: `ultramarine/crates/types/src/engine_api.rs`
- **Implemented**:
  - Lightweight header type (~516 bytes) for consensus voting
  - `from_payload()` method to extract from full `ExecutionPayloadV3`
  - `size_bytes()` helper for metrics
- **Purpose**: Keeps consensus messages under 2KB target

#### 1.3 Engine API Integration ✅
- **Files Modified**:
  - `ultramarine/crates/execution/src/engine_api/mod.rs` - Added `get_payload_with_blobs()` trait method
  - `ultramarine/crates/execution/src/engine_api/client.rs` - Implemented blob bundle parsing
  - `ultramarine/crates/execution/src/client.rs` - Added `generate_block_with_blobs()` method
- **Functionality**:
  - Parses full `ExecutionPayloadEnvelopeV3` from getPayloadV3 response
  - Extracts and validates `blobs_bundle` field
  - Converts Alloy's `BlobsBundleV1` to internal `BlobsBundle` type
  - Returns `(ExecutionPayloadV3, Option<BlobsBundle>)`

#### Dependencies Added ✅
- `alloy-eips.workspace = true` to `ultramarine-types/Cargo.toml`

#### Files Modified Summary
1. `/ultramarine/crates/types/src/blob.rs` - NEW (960 lines)
2. `/ultramarine/crates/types/src/lib.rs` - Added `pub mod blob;`
3. `/ultramarine/crates/types/src/engine_api.rs` - Added ExecutionPayloadHeader (280 lines)
4. `/ultramarine/crates/types/Cargo.toml` - Added alloy-eips dependency
5. `/ultramarine/crates/execution/src/engine_api/mod.rs` - Added get_payload_with_blobs() trait
6. `/ultramarine/crates/execution/src/engine_api/client.rs` - Implemented blob parsing
7. `/ultramarine/crates/execution/src/client.rs` - Added generate_block_with_blobs()

#### Test Results ✅
```
running 7 tests
test blob::tests::test_blob_size_validation ... ok
test blob::tests::test_bundle_length_validation ... ok
test blob::tests::test_bundle_max_blobs ... ok
test blob::tests::test_versioned_hashes ... ok
test blob::tests::test_blobs_bundle_conversion_from_alloy ... ok
test blob::tests::test_empty_blobs_bundle_conversion ... ok
test blob::tests::test_multiple_blobs_conversion ... ok

test result: ok. 7 passed; 0 failed
```

#### Next Steps
- **Phase 2**: ✅ Completed — ValueMetadata + Value refactor in place (see notes above)
- **Estimated Time**: 2 days (actual)

---

### Phase 2: Consensus Value Refactor ✅ COMPLETED

**Status**: Complete (tests passing, consensus message size < 3 KB)

**Highlights**:
- Created `ValueMetadata` structure with ExecutionPayloadHeader + `Vec<KzgCommitment>`.
- Refactored `Value` to carry only metadata (full payload now sent via proposal parts).
- Updated protobuf schemas and storage/codec usage.
- Verified consensus message size target and added regression tests.

---

### Phase 3: Proposal Streaming ✅ COMPLETED (2025-10-16)

**Status**: Infrastructure complete - blobs can be streamed via ProposalParts

**Highlights**:
- Added `BlobSidecar` struct with blob data + KZG commitment + proof
- Extended `ProposalPart` enum with `BlobSidecar` variant
- Updated protobuf schema (`consensus.proto`) with BlobSidecar message
- Implemented protobuf conversion (with Bytes type handling)
- Updated `make_proposal_parts()` to stream blobs as separate parts
- Updated `stream_proposal()` to accept `Option<BlobsBundle>` parameter
- Updated signature verification to include blobs in hash
- All code builds successfully

**Files Modified**:
1. `ultramarine/crates/types/src/proposal_part.rs` - Added BlobSidecar struct & enum variant
2. `ultramarine/crates/types/proto/consensus.proto` - Added BlobSidecar protobuf message
3. `ultramarine/crates/consensus/src/state.rs` - Updated streaming & signature logic
4. `ultramarine/crates/node/src/app.rs` - Updated stream_proposal() call

**Implementation Notes**:
- BlobSidecar is ~131,169 bytes (1 byte index + 131KB blob + 48B commitment + 48B proof)
- Signature hash includes: index, blob data, commitment, and proof
- Backward compatible: passing `None` for blobs maintains existing behavior
- Ready for integration: execution layer can now be connected via `generate_block_with_blobs()`

**Next Integration Steps** (for full E2E flow):
1. Connect `app.rs` GetValue handler to call `generate_block_with_blobs()` from execution layer
2. Update `propose_value()` to create `Value` from `ValueMetadata` (instead of deprecated `from_bytes`)
3. Update `assemble_value_from_parts()` to extract blobs from `BlobSidecar` parts
4. Add blob storage to persist received blobs

**Build Verification**: ✅ All packages compile successfully

---

### Phase 4: Full Ethereum-Compatible BlobSidecar (Days 6-9) 🔄 IN PROGRESS

**Status**: Blocker #2 RESOLVED (2025-10-17) - Proceeding with SSZ implementation

**⚠️ SIGNATURE SCHEMA DECISION**: Ed25519 signatures (NOT BLS12-381) for proposer authentication
- **Rationale**: Independent L1 with Malachite consensus, performance priority, ecosystem consistency
- **Impact**: Full Ultramarine compatibility + blob data verification works with Ethereum tooling
- **Trade-off**: Ethereum light clients cannot verify proposer identity (acceptable for our use case)
- **KZG layer**: Still uses BLS12-381 for polynomial commitments (mandatory, non-negotiable)

**Progress**:
- ✅ Created `ethereum_compat/` module (encapsulated compatibility layer)
- ✅ Implemented `BeaconBlockHeader` and `SignedBeaconBlockHeader` type structures
- ✅ Implemented Ed25519 signature verification with comprehensive tests (**Blocker #2 FIXED**)
- ✅ Implemented SSZ hash_tree_root with test vector verification (**Blocker #1 FIXED**)
- ✅ Added Merkle inclusion proof utility scaffolding
- ✅ Integrated Lighthouse `merkle_proof` crate (updated to v7.1.0 to fix getrandom issue)
- ✅ Integrated `tree_hash` crate v0.10.0 for SSZ merkleization
- 🔄 **IN PROGRESS**: Complete Merkle proof generation with body field proofs (Blocker #3)
- ⏳ Pending: Extend BlobSidecar structure with Phase 4 fields (blocked until Blocker #3 fixed)

**Goal**: Extend BlobSidecar to match Ethereum Deneb specification exactly, enabling:
- Independent blob validation without full proposal context
- Sync compatibility with Ethereum tooling (block explorers, RPC clients)
- Defense-in-depth verification (ProposalFin + inclusion proof + KZG proof)
- Future P2P blob sync support (BlobSidecarsByRoot/ByRange)

**Architecture Decision** (2025-10-17):
After team review, we're implementing **full spec compliance** with **isolated compatibility layer**:

1. **Compatibility Layer Pattern** (like Cosmos IBC, Polkadot bridges, WBTC):
   - Created `ethereum_compat/` module with `pub(crate)` visibility
   - Ethereum-specific types (BeaconBlockHeader, Merkle proofs) are encapsulated
   - Core Ultramarine consensus (Value, ValueMetadata, Malachite) remains unchanged
   - **No abstraction leakage** - compatibility types used ONLY within BlobSidecar

2. **Benefits of Encapsulation**:
   - ✅ Clear separation: Core consensus vs. Bridge layer
   - ✅ Can swap/modify Ethereum compat without affecting consensus
   - ✅ Follows industry best practices (bridge modules)
   - ✅ Makes intent obvious: "These types exist to speak Deneb"

3. **Spec Compliance Benefits**:
   - ✅ New nodes can sync historical blobs independently
   - ✅ Block explorers and RPC clients work out of the box
   - ✅ Future-proof for P2P blob sync protocols
   - ✅ Multi-layer validation catches implementation bugs
   - ✅ Can reference battle-tested Lighthouse/Prysm code directly

**Files Created/Modified** (2025-10-17):
- ✅ `ultramarine/crates/types/src/ethereum_compat/` (NEW) - Compatibility layer module
- ✅ `ultramarine/crates/types/src/ethereum_compat/beacon_header.rs` (NEW) - BeaconBlockHeader types
- ✅ `ultramarine/crates/types/src/ethereum_compat/merkle.rs` (NEW) - Merkle proof utilities
- ✅ `ultramarine/crates/types/Cargo.toml` - Added merkle_proof, fixed_bytes dependencies
- ⏳ `ultramarine/crates/types/src/proposal_part.rs` - Extend BlobSidecar structure (pending)
- ⏳ `ultramarine/crates/consensus/src/state.rs` - Update proposal creation/validation (pending)
- ⏳ `ultramarine/crates/node/src/blob_verifier.rs` (NEW) - KZG verification (pending)
- ⏳ `ultramarine/crates/storage/src/blob_store.rs` (NEW) - Blob storage (pending)

**🚫 PRODUCTION BLOCKERS** (Must fix before ANY integration testing):

These are not optional TODOs - they are **critical security and correctness issues** that prevent the implementation from working at all:

1. **✅ BLOCKER #1: SSZ Hash Tree Root** (`beacon_header.rs:85`) - **FIXED**:
   - **Was**: Used Keccak256 placeholder
   - **Now**: Proper SSZ merkleization using `tree_hash::TreeHash` trait (derived)
   - **Implementation**: Added `#[derive(TreeHash)]` and proper type conversion
   - **Tests**: Added test vector verification with known SSZ root for all-zero header
   - **Verification**: `0xc78009fdf07fc56a11f122370658a353aaa542ed63e44c4bc15ff4cd105ab33c`
   - **Completed**: 2025-10-17
   - **Dependencies**: tree_hash v0.10.0, tree_hash_derive v0.10.0

2. **✅ BLOCKER #2: Ed25519 Signature Verification** (`beacon_header.rs:175`) - **FIXED**:
   - **Was**: Returned `true` unconditionally (SECURITY HOLE)
   - **Now**: Real Ed25519 verification using Malachite's `PublicKey::verify()`
   - **Implementation**: Uses `crate::signing::{PublicKey, Signature}` for consistency
   - **Tests**: Comprehensive test with real keypair (success/failure paths)
   - **Completed**: 2025-10-17
   - **Note**: Uses Ed25519 (not BLS) - see Signature Schema Decision above

3. **🔄 BLOCKER #3: Complete Merkle Proof Generation** (`merkle.rs:118`) - **IN PROGRESS**:
   - **Current**: Hashes commitments with Keccak256 and pads with zeros instead of body field proof
   - **Required**: Proper SSZ merkleization + BeaconBlockBody generalized_index calculation
   - **Impact**: **PROOFS DON'T VERIFY** - Generated proofs rejected by Lighthouse/Prysm/spec
   - **Severity**: CRITICAL - Blobs cannot be validated independently
   - **Estimate**: 2-3 hours (need to understand Deneb BeaconBlockBody SSZ structure)
   - **Status**: Starting implementation now (SSZ foundation is complete)

**PROGRESS SUMMARY**:
- ✅ **2 of 3 blockers resolved** (Ed25519 signature verification + SSZ hash_tree_root)
- 🔄 **1 of 3 in progress** (Merkle proof generation)
- ⏳ **0 of 3 pending** (All blockers are being actively worked on)

**RECOMMENDATION**:
- ✅ Blockers #1 and #2 complete - SSZ foundation is solid, signatures work correctly
- 🔄 Now implementing Blocker #3 (Merkle proofs) - last critical piece
- ⏳ Once Blocker #3 is complete, can extend BlobSidecar structure
- **DO NOT** attempt end-to-end integration testing until Blocker #3 is resolved

---

#### 4.1 Extended BlobSidecar Structure

**File**: `ultramarine/crates/types/src/proposal_part.rs` (MODIFY)

**Current Structure** (Phase 3):
```rust
pub struct BlobSidecar {
    pub index: u8,
    pub blob: Blob,
    pub kzg_commitment: KzgCommitment,
    pub kzg_proof: KzgProof,
}
// Size: ~131,169 bytes
```

**New Structure** (Phase 4 - Full Spec):
```rust
pub struct BlobSidecar {
    // Core fields (unchanged)
    pub index: u8,
    pub blob: Blob,
    pub kzg_commitment: KzgCommitment,
    pub kzg_proof: KzgProof,

    // NEW: Ethereum compatibility fields
    pub signed_block_header: SignedBeaconBlockHeader,
    pub kzg_commitment_inclusion_proof: Vec<B256>,  // Length: 17
}
// Size: ~132,921 bytes (+1,752 bytes overhead for full compatibility)
```

**Supporting Types** (NEW in `beacon_types.rs`):
```rust
pub struct BeaconBlockHeader {
    pub slot: u64,                // ≈ Ultramarine's Height
    pub proposer_index: u64,      // Validator index
    pub parent_root: B256,        // Previous block hash
    pub state_root: B256,         // Beacon state root
    pub body_root: B256,          // BeaconBlockBody root (for inclusion proof)
}

pub struct SignedBeaconBlockHeader {
    pub message: BeaconBlockHeader,
    pub signature: Signature,     // BLS or Ed25519 signature
}
```

**Purpose of New Fields**:
1. **signed_block_header**: Allows validators to verify proposer signature without Init/Fin context
2. **kzg_commitment_inclusion_proof**: 17-level Merkle proof that commitment is in `blob_kzg_commitments` list
3. **Combined**: Enables independent blob validation for sync, RPC, and future P2P protocols

**Reference Implementation**: Lighthouse `blob_sidecar.rs` lines 57-66

---

#### 4.2 Merkle Inclusion Proof Generation

**File**: `ultramarine/crates/types/src/merkle.rs` (NEW - ~300 lines)

**Key Functions**:
```rust
/// Generate 17-level Merkle inclusion proof for a KZG commitment
pub fn generate_kzg_commitment_inclusion_proof(
    blob_kzg_commitments: &[KzgCommitment],
    index: usize,
) -> Result<Vec<B256>, Error> {
    // Part 1: Merkle tree for the commitments list
    let commitment_leaves: Vec<B256> = blob_kzg_commitments
        .iter()
        .map(|c| tree_hash::Hash256::from_slice(c.as_bytes()))
        .collect();

    let commitments_tree_depth = commitment_leaves.len().next_power_of_two().trailing_zeros() as usize;
    let subtree_proof = generate_merkle_proof(&commitment_leaves, index, commitments_tree_depth)?;

    // Part 2: Proof from commitments list root to body root
    let body_tree_proof = generate_body_field_proof(BLOB_KZG_COMMITMENTS_GINDEX)?;

    // Concatenate: subtree proof + body proof = 17 total branches
    let mut full_proof = Vec::with_capacity(17);
    full_proof.extend(subtree_proof);
    full_proof.extend(body_tree_proof);

    Ok(full_proof)
}

/// Verify KZG commitment inclusion proof
pub fn verify_kzg_commitment_inclusion_proof(
    kzg_commitment: &KzgCommitment,
    proof: &[B256],
    index: usize,
    body_root: B256,
) -> bool {
    if proof.len() != 17 {
        return false;
    }

    let leaf = tree_hash::Hash256::from_slice(kzg_commitment.as_bytes());
    verify_merkle_branch(leaf, proof, 17, index, body_root)
}
```

**Algorithm** (from Deneb spec + Lighthouse):
1. Build Merkle tree for `blob_kzg_commitments` list
2. Generate proof from commitment at `index` to list root (depth varies by blob count)
3. Generate proof from list root to BeaconBlockBody root (fixed depth)
4. Concatenate both proofs → total 17 branches

**Spec Reference**: `consensus-specs/specs/deneb/p2p-interface.md` lines 124-135

**Lighthouse Reference**: `lighthouse/consensus/types/src/beacon_block_body.rs` lines 1450-1520

---

#### 4.3 Updated Proposal Creation

**File**: `ultramarine/crates/consensus/src/state.rs` (MODIFY)

**Current** (Phase 3):
```rust
fn make_proposal_parts(..., blobs_bundle: Option<BlobsBundle>) -> Vec<ProposalPart> {
    // ...
    for (index, (blob, commitment, proof)) in blobs.iter().enumerate() {
        let sidecar = BlobSidecar::new(index, blob, commitment, proof);
        parts.push(ProposalPart::BlobSidecar(sidecar));
    }
}
```

**Updated** (Phase 4 - Full Spec):
```rust
fn make_proposal_parts(
    &self,
    value: LocallyProposedValue<LoadContext>,
    data: Bytes,
    execution_payload: &ExecutionPayloadV3,  // NEW: need full payload for body_root
    blobs_bundle: Option<BlobsBundle>,
) -> Vec<ProposalPart> {
    // ... existing Init + Data parts ...

    if let Some(bundle) = blobs_bundle {
        // Step 1: Compute body_root from execution payload
        let body_root = compute_body_root(execution_payload)?;

        // Step 2: Create signed block header
        let block_header = BeaconBlockHeader {
            slot: value.height.as_u64(),
            proposer_index: self.get_proposer_index(),
            parent_root: self.latest_block_hash(),
            state_root: execution_payload.state_root,
            body_root,
        };

        let signed_header = SignedBeaconBlockHeader {
            message: block_header.clone(),
            signature: self.sign_block_header(&block_header),
        };

        // Step 3: Generate inclusion proofs for each commitment
        for (index, (blob, commitment, proof)) in bundle.iter().enumerate() {
            let inclusion_proof = generate_kzg_commitment_inclusion_proof(
                &bundle.commitments,
                index,
            )?;

            // Step 4: Create full Ethereum-compatible sidecar
            let sidecar = BlobSidecar {
                index: index as u8,
                blob: blob.clone(),
                kzg_commitment: *commitment,
                kzg_proof: *proof,
                signed_block_header: signed_header.clone(),  // NEW
                kzg_commitment_inclusion_proof: inclusion_proof,  // NEW
            };

            parts.push(ProposalPart::BlobSidecar(sidecar));
        }
    }

    // ... Fin part ...
}
```

**Overhead**: ~1-2ms per blob to compute Merkle proofs (acceptable for 6-9 blobs)

---

#### 4.4 Updated Proposal Validation

**File**: `ultramarine/crates/consensus/src/state.rs` (MODIFY)

**Phase 4 Validation Pipeline**:
```rust
async fn verify_blob_sidecars(
    &self,
    blob_sidecars: &[&BlobSidecar],
    kzg: &c_kzg::Kzg,
) -> Result<(), Error> {
    for sidecar in blob_sidecars {
        // Layer 1: Verify proposer signature on block header
        let proposer_pubkey = self.get_validator_set()
            .get_by_index(sidecar.signed_block_header.message.proposer_index)?
            .public_key;

        if !self.verify_block_header_signature(
            &sidecar.signed_block_header,
            &proposer_pubkey,
        ) {
            return Err(Error::InvalidProposerSignature);
        }

        // Layer 2: Verify Merkle inclusion proof
        if !verify_kzg_commitment_inclusion_proof(
            &sidecar.kzg_commitment,
            &sidecar.kzg_commitment_inclusion_proof,
            sidecar.index as usize,
            sidecar.signed_block_header.message.body_root,
        ) {
            return Err(Error::InvalidInclusionProof);
        }

        // Layer 3: Verify KZG proof (blob data matches commitment)
        if !kzg.verify_blob_kzg_proof(
            sidecar.blob.as_bytes(),
            &sidecar.kzg_commitment,
            &sidecar.kzg_proof,
        )? {
            return Err(Error::InvalidKzgProof);
        }
    }

    Ok(())
}
```

**Defense in Depth**:
1. ✅ ProposalFin signature (Malachite trust model)
2. ✅ Proposer signature per blob (Ethereum trust model)
3. ✅ Merkle inclusion proof (blob belongs to block)
4. ✅ KZG proof (blob data matches commitment)

**Reference**: Lighthouse `blob_verification.rs` lines 180-350

---

#### 4.5 KZG Verification Implementation

**File**: `ultramarine/crates/node/src/blob_verifier.rs` (NEW - ~250 lines)

```rust
use c_kzg::{Blob as CKzgBlob, KzgCommitment, KzgProof, KzgSettings};

pub struct BlobVerifier {
    kzg_settings: Arc<KzgSettings>,
}

impl BlobVerifier {
    pub fn new(trusted_setup_path: &str) -> Result<Self, String> {
        let kzg_settings = KzgSettings::load_trusted_setup_file(trusted_setup_path)?;
        Ok(Self { kzg_settings: Arc::new(kzg_settings) })
    }

    /// Batch verify multiple blobs (5-10x faster than individual)
    pub fn verify_blob_sidecars_batch(
        &self,
        sidecars: &[BlobSidecar],
    ) -> Result<(), String> {
        let blobs: Vec<_> = sidecars.iter()
            .map(|s| CKzgBlob::from_bytes(s.blob.data()))
            .collect::<Result<_, _>>()?;

        let commitments: Vec<_> = sidecars.iter()
            .map(|s| KzgCommitment::from_bytes(&s.kzg_commitment.0))
            .collect::<Result<_, _>>()?;

        let proofs: Vec<_> = sidecars.iter()
            .map(|s| KzgProof::from_bytes(&s.kzg_proof.0))
            .collect::<Result<_, _>>()?;

        let valid = KzgProof::verify_blob_kzg_proof_batch(
            &blobs,
            &commitments,
            &proofs,
            &self.kzg_settings,
        )?;

        if !valid {
            return Err("Batch KZG verification failed".to_string());
        }

        Ok(())
    }
}
```

**Performance**: Batch verification reduces 6-blob verification from ~60ms to ~12ms

**Reference**: Lighthouse `kzg_utils.rs`

---

#### 4.6 Blob Storage (Ethereum-Compatible Format)

**File**: `ultramarine/crates/storage/src/blob_store.rs` (NEW - ~300 lines)

```rust
use redb::{Database, TableDefinition};

const BLOB_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blobs");

pub struct BlobStore {
    db: Database,
}

impl BlobStore {
    /// Store full Ethereum-compatible blob sidecar
    pub fn store_blob(&self, height: Height, sidecar: &BlobSidecar) -> Result<(), String> {
        let key = blob_key(height, sidecar.index);

        // Serialize to SSZ (Ethereum format)
        let ssz_bytes = sidecar.as_ssz_bytes();

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(BLOB_TABLE)?;
            table.insert(&key[..], &ssz_bytes)?;
        }
        write_txn.commit()?;

        Ok(())
    }

    /// Retrieve Ethereum-compatible blob sidecar (for RPC/sync)
    pub fn get_blob_sidecar(
        &self,
        height: Height,
        index: u8,
    ) -> Result<Option<BlobSidecar>, String> {
        let key = blob_key(height, index);

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(BLOB_TABLE)?;

        if let Some(bytes) = table.get(&key[..])? {
            let sidecar = BlobSidecar::from_ssz_bytes(bytes.value())?;
            Ok(Some(sidecar))
        } else {
            Ok(None)
        }
    }
}
```

**Storage Format**: Full SSZ-encoded BlobSidecar (compatible with Ethereum tooling)

---

#### 4.7 RPC Endpoints for Ethereum Compatibility

**File**: `ultramarine/crates/node/src/rpc.rs` (NEW - ~150 lines)

```rust
/// Ethereum-compatible RPC: Get blob sidecar by beacon block root and index
/// Compatible with: eth_getBlobSidecar, beacon API blob_sidecars endpoint
pub async fn get_blob_sidecar(
    &self,
    block_root: B256,
    blob_index: u8,
) -> Result<Option<BlobSidecar>, Error> {
    let (height, round) = self.block_index.get_height_round(block_root).await?;
    self.blob_store.get_blob_sidecar(height, blob_index).await
}

/// Get all blob sidecars for a block (returns Vec<BlobSidecar>)
pub async fn get_blob_sidecars(
    &self,
    block_root: B256,
) -> Result<Vec<BlobSidecar>, Error> {
    let (height, round) = self.block_index.get_height_round(block_root).await?;

    let mut sidecars = Vec::new();
    for index in 0..MAX_BLOBS_PER_BLOCK {
        if let Some(sidecar) = self.blob_store.get_blob_sidecar(height, index).await? {
            sidecars.push(sidecar);
        } else {
            break;
        }
    }

    Ok(sidecars)
}
```

**Compatible With**:
- Ethereum Beacon API: `/eth/v1/beacon/blob_sidecars/{block_id}`
- Block explorers (Beaconcha.in, Etherscan)
- Data availability samplers
- Cross-chain bridges

---

#### 4.8 Dependencies to Add

**File**: `ultramarine/Cargo.toml` (MODIFY)

```toml
[dependencies]
c-kzg = "1.0"           # KZG proof verification
merkle-proof = "0.3"    # Merkle tree utilities (or custom implementation)
tree-hash = "0.5"       # SSZ tree hashing
```

---

#### 4.9 Cost-Benefit Analysis

**Costs**:
- **Bandwidth**: +752 bytes per blob (6 blobs = +4.5 KB per proposal)
- **CPU**: +1-2ms per blob for Merkle proof generation
- **CPU**: +0.5ms per blob for Merkle proof verification
- **Storage**: +752 bytes per blob in BlobStore
- **Code Complexity**: ~800 lines (Merkle utilities + verification + storage)

**Benefits**:
- ✅ **Sync Compatibility**: New nodes can verify historical blobs independently
- ✅ **Tooling Compatibility**: Block explorers, RPC clients work immediately
- ✅ **Future-Proof**: Supports P2P blob sync (BlobSidecarsByRoot/ByRange)
- ✅ **Defense in Depth**: Multi-layer validation catches bugs
- ✅ **Spec Compliant**: Exact match with Ethereum Deneb specification
- ✅ **Reference Code**: Can use Lighthouse's battle-tested implementation

**Trade-off Conclusion**: Small overhead for significant ecosystem compatibility

---

#### 4.10 Implementation Timeline (Updated)

| Days | Task | Reference |
|------|------|-----------|
| 6 | Extend BlobSidecar structure + beacon types | Lighthouse blob_sidecar.rs |
| 7 | Implement Merkle proof generation/verification | Lighthouse beacon_block_body.rs |
| 8 | Update proposal creation with signed headers | Lighthouse BeaconChain::build_block |
| 9 | Add KZG verification + BlobStore + RPC | Lighthouse blob_verification.rs |

**Total**: 4 days (vs 2 days for simplified approach)
**Extra Cost**: 2 days for full spec compliance
**Gain**: Complete Ethereum compatibility + future-proof architecture

---

### Dependencies**: Phase 3 must be completed first ✅

**References**:
- Ethereum Deneb Spec: `consensus-specs/specs/deneb/p2p-interface.md`
- Lighthouse BlobSidecar: `lighthouse/consensus/types/src/blob_sidecar.rs`
- Lighthouse Verification: `lighthouse/beacon_node/beacon_chain/src/blob_verification.rs`
- Prysm Verification: `prysm/beacon-chain/verification/blob.go`

---

### Phase 5: Block Import / EL Interaction ⚠️ PARTIAL (2025-10-21)

**Status**: ⚠️ Live Consensus Complete - **State Sync Broken**

**Key Accomplishments (Live Consensus)**:
- ✅ Blob availability validation in Decided handler
- ✅ Blob count verification before import
- ✅ Versioned hash verification (Lighthouse parity - defense-in-depth)
- ✅ Engine API v3 integration (was already implemented)
- ✅ All tests passing (blob_engine: 10/10)

**🚨 Critical Gaps Discovered (State Sync)**:
- ❌ ProcessSyncedValue - No blob retrieval/storage (app.rs:591-609)
- ❌ GetDecidedValue - No blobs in response (app.rs:669-683)
- ❌ RestreamProposal - Not implemented (app.rs:292-355)

**Documentation**:
- `docs/PHASE_5_COMPLETION.md` - Live consensus completion
- `docs/LIGHTHOUSE_PARITY_COMPLETE.md` - Versioned hash verification
- `docs/BLOB_SYNC_GAP_ANALYSIS.md` - **Critical sync gaps and required fixes**

**Time Taken**: ~5 hours (live consensus only)

**Remaining Work**: 1-2 days to fix state sync (Phase 5.1)

---

### Phase 5.1: State Sync Fixes ⏳ URGENT

**Status**: Not started - **BLOCKS PRODUCTION DEPLOYMENT**

**Required Fixes** (1-2 days):
1. ProcessSyncedValue blob sync (1 day, CRITICAL)
2. GetDecidedValue include blobs (4 hours, CRITICAL)
3. RestreamProposal implementation (4 hours, HIGH)

**Dependencies**: None - can start immediately

**See**: `docs/BLOB_SYNC_GAP_ANALYSIS.md` for detailed implementation steps

---

### Phase 6: Pruning Policy ⏳ PENDING

**Status**: Ready to start

**Dependencies**: Phase 5 + Phase 5.1 completed (sync fixes in place)

**Estimated Time**: 1 day

---

### Phase 7: Archive Integration (Optional) ⏳ PENDING

**Status**: Not started (optional feature)

**Dependencies**: Phase 6 must be completed first

---

### Phase 8: Testing ⏳ PENDING

**Status**: Not started

**Dependencies**: Phases 5-6 must be completed first (Phase 7 is optional)

```
