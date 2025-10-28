# EIP-4844 Blob Sidecar Integration - Final Implementation Plan

**Project**: Integrate blob sidecars into Ultramarine consensus client
**Timeline**: 10-15 days (2-3 weeks with focused effort)
**Architecture**: Channel-based approach using existing Malachite patterns
**Status**: 🟢 **Phase 4 Cleanup Complete – Ready for Integration Tests (Phase 5+)**
**Progress**: **Phases 1-5.2 complete** (core blob integration + three-layer metadata architecture)
**Implementation**: Live consensus + state sync fully operational with blob transfer + metadata-only storage
**Current Focus**: Integration validation & operator docs (see [PHASE4_PROGRESS.md](../PHASE4_PROGRESS.md) for Phase 4 completion)
**Last Updated**: 2025-10-28
**Malachite Version**: b205f4252f3064d9a74716056f63834ff33f2de9 (upgraded ✅)

---

## Executive Summary

This plan integrates EIP-4844 blob sidecars into Ultramarine while maintaining clean separation between consensus and data availability layers. The critical architectural decision is to:

1. **Consensus votes on hash(metadata)** - keeps consensus messages ~2KB
2. **Blobs flow separately** via existing ProposalParts channel - 131KB+ per blob
3. **Application layer enforces availability** before marking blocks valid

**Key Insight**: We do NOT modify Malachite library or add new network topics. Blobs flow through the existing `/proposal_parts` gossip channel by extending the application-layer `ProposalPart` enum.

### Current Status (2025-10-28)

**Completed** ✅:
- **Phase 1 – Execution Bridge** *(2025-10-27)*: Engine API v3 wired end-to-end with blob bundle retrieval; execution payload header extraction added to `ValueMetadata`.
- **Phase 2 – Three-Layer Metadata Architecture** *(2025-10-27)*: Introduced `ConsensusBlockMetadata`, `BlobMetadata`, and blob-engine persistence (Layer 1/2/3) with redb tables and protobuf encoding.
- **Phase 3 – Proposal Streaming** *(2025-10-28)*: Extended `ProposalPart` with `BlobSidecar`, enabled streaming of blobs via `/proposal_parts` with commitment/proof payloads.
- **Phase 4 – Cleanup & Storage Finalisation** *(2025-10-28)*: Removed legacy header tables/APIs, made `BlobMetadata` the single source of truth, updated parent-root validation and restream rebuilds. See [PHASE4_PROGRESS.md](../PHASE4_PROGRESS.md#2025-10-28-tuesday--phase-4--cleanup-complete) for full log.
- **Phase 5 (5.1/5.2) – Live Consensus + Sync Fixes** *(2025-10-23)*: Delivered proposer blob storage, restream sealing, synced-value protobuf path corrections (tracked separately in `STATUS_UPDATE_2025-10-21.md`).
- **Quality Gates**: 23/23 unit tests (`cargo test -p ultramarine-consensus --lib`) covering store (9) and state (14) scenarios; `cargo fmt --all` and `cargo clippy -p ultramarine-consensus` run clean for the consensus crate.

**Up Next** 🟡:
- Phase 5 integration validation: restart survival, multi-validator blob sync, and restream replay tests.
- Documentation polish: consolidate operator/developer notes on metadata-only storage.
- Optional metrics for consensus store (undecided/decided counts, pointer hits).

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

## Phase 1: Execution ↔ Consensus Bridge ✅ COMPLETED (2025-10-27)

### Goal
Extend execution client interface to support Engine API v3 with blob bundle retrieval.

### Files to Modify
- `ultramarine/crates/execution/src/client.rs`
- `ultramarine/crates/execution/src/types.rs`

### Implementation References
- `crates/types/src/blob.rs` – blob/commitment/proof types (MAX_BLOBS_PER_BLOCK = 1024)
- `crates/types/src/blob.rs::BlobsBundle` – validation, versioned hashes
- `crates/execution/src/client.rs` – Engine API v3 integration (`generate_block_with_blobs`, Alloy conversions)
- `crates/execution/src/engine_api/client.rs` – getPayloadV3 envelope parsing
- `crates/execution/src/engine_api/mod.rs` – EngineApi trait extension
- Tests: `cargo nextest run -p ultramarine-execution engine_api_v3`

## Phase 2: Three-Layer Metadata Architecture ✅ COMPLETED (2025-10-27)

### Outcome (2025-10-27)
- Established the **three-layer metadata architecture**:
  1. `ConsensusBlockMetadata` (Layer 1) – pure Malachite/Tendermint terms (height/round/proposer) with SSZ tree-hash validator set.
  2. `BlobMetadata` (Layer 2) – Ethereum compatibility shim storing execution payload headers, KZG commitments, proposer hints.
  3. Blob engine (Layer 3) – prunable RocksDB storage keyed by height/round.
- Added redb tables: `consensus_block_metadata`, `blob_metadata_undecided`, `blob_metadata_decided`, `blob_metadata_meta` (O(1) latest pointer).
- Implemented protobuf serialization for both metadata layers; consensus now serialises via `ProtobufCodec`.
- Unit tests cover metadata creation, protobuf round-trips, validator-set hashing (see [PHASE4_PROGRESS.md](../PHASE4_PROGRESS.md#phase-1--core-storage)).

## Phase 3: Proposal Streaming ✅ COMPLETED (2025-10-28)

### Outcome (2025-10-28)
- Added `ProposalPart::BlobSidecar` plus protobuf codecs so blobs stream on `/proposal_parts` without new topics.
- Updated consensus state to assemble/disseminate proposals with optional blob bundles; proposer/receiver flows store undecided metadata only.
- Added restream helpers that rebuild sidecars/signatures from stored metadata (see Phase 4 for final cleanup).
- Size budgeting validated (~131 KB per blob sidecar) and covered by unit tests in `ultramarine-types` and `ultramarine-consensus`.

### Files Modified
- `ultramarine/crates/types/src/proposal_part.rs` ✅
- `ultramarine/crates/types/src/blob.rs` ✅
- `ultramarine/crates/proto/src/ultramarine.proto` ✅

### Implementation References
- `crates/types/src/proposal_part.rs` – `ProposalPart::BlobSidecar`, serde/protobuf conversions
- `crates/types/src/blob.rs` – blob bundle utilities used during streaming
- `crates/types/proto/consensus.proto` – BlobSidecar schema (fields 5 & 6)
- `crates/consensus/src/state.rs` – `stream_proposal` / `make_proposal_parts` emitting blobs
- `crates/node/src/app.rs` – proposal assembly, blob verification, storage integration

## Phase 4: Cleanup & Storage Finalisation ✅ COMPLETED (2025-10-28)

### Outcome (2025-10-28)
- Removed all legacy header persistence: `BLOCK_HEADERS_TABLE`, low-level insert/get helpers, and state/store/node methods that wrote beacon headers.
- Consensus relies solely on `BlobMetadata` (Layer 2) for header reconstruction; parent-root verification loads previous decided metadata and restream rebuilds signed headers at broadcast time.
- Strengthened round hygiene (`commit_cleans_failed_round_blob_metadata`) and metadata fallback logic (`load_blob_metadata_for_round_falls_back_to_decided`).
- Added restream reconstruction test (`rebuild_blob_sidecars_for_restream_reconstructs_headers`); unit-suite now 23/23 passing.
- `cargo fmt --all`, `cargo clippy -p ultramarine-consensus`, and `cargo test -p ultramarine-consensus --lib` run clean for the consensus crate.
- See [PHASE4_PROGRESS.md](../PHASE4_PROGRESS.md#2025-10-28-tuesday--phase-4--cleanup-complete) for detailed timeline and code references.

## Phase 5: Block Import / EL Interaction (Days 8-9) ✅ COMPLETED

### Goal
Modify `Decided` handler to validate blob availability before block import.

**Status**: ✅ **Complete - All Critical Paths Working**
**Date Started**: 2025-10-21
**Date Completed**: 2025-10-23 (live consensus + state sync)

**✅ Completed**:
- Blob lifecycle (mark_decided, drop_round, pruning)
- Error handling fixes (#1, #2, #3)
- Blob availability validation in Decided handler
- Blob count verification before import
- **Versioned hash verification (Lighthouse parity)**
- Engine API v3 integration (was already implemented)
- **ProcessSyncedValue with blob sync** (all 6 reply paths correct)
- **GetDecidedValue with blob bundling**

**Recent Enhancement**:
- RestreamProposal - ✅ Implemented (network partition recovery edge case resolved)

**Key Discovery**: Engine API v3 was already fully implemented. We only needed to add blob availability validation and versioned hash verification. Initial code review identified gaps in state synchronization, which have been **fully resolved** with SyncedValuePackage implementation.

**See**:
- `docs/PHASE_5_COMPLETION.md` - Live consensus completion details
- `docs/LIGHTHOUSE_PARITY_COMPLETE.md` - Versioned hash verification
- `docs/BLOB_SYNC_GAP_ANALYSIS.md` - **Critical sync gaps and fixes**

### Files to Modify
- `ultramarine/crates/node/src/app.rs` (lines 404-624)
- `ultramarine/crates/execution/src/client.rs`

### Implementation References
- `crates/node/src/app.rs` – Decided handler availability checks, EL import flow
- `crates/consensus/src/state.rs` – blob tracking (`mark_decided`, orphan cleanup, prune)
- `crates/execution/src/client.rs` – Engine API notifications (`notify_new_block`, `set_latest_forkchoice_state`)
- `crates/blob_engine/src/engine.rs` – promotion + pruning used during commit
- `crates/blob_engine/src/store/rocksdb.rs` – undecided/decided storage semantics
- Tests/validation: `cargo nextest run -p ultramarine-blob-engine`, `cargo nextest run -p ultramarine-node decided_handler`

### Phase 5.1: State Sync Implementation ✅ COMPLETED (2025-10-23)

**Status**: ✅ **Complete - All Sync Paths Working**

**Completed Fixes** (~1.5 days):
1. ✅ ProcessSyncedValue blob sync with all 6 reply paths (app.rs:574-709)
2. ✅ GetDecidedValue blob bundling with SyncedValuePackage (app.rs:711-829)
3. ✅ RestreamProposal implemented (original proposer restreams blobs + payload)

**Implementation**: SyncedValuePackage (crates/types/src/sync.rs:420 lines)

**See**: `docs/MESSAGE_HANDLER_AUDIT.md` for complete handler verification

---

### Phase 5.2: Critical Post-Implementation Fixes ✅ COMPLETED (2025-10-23)

**Status**: ✅ **Complete - All Critical Bugs Fixed**

**Timeline**: ~4 hours (comprehensive code audit and fixes)

#### Overview

Following the initial implementation completion, a comprehensive code audit revealed three critical findings and one high-severity security bug. All issues have been resolved with fixes that are **more correct than the reference implementations** (malachite examples and snapchain).

---

#### Finding #1: Proposer Never Stored Own Blobs ✅ FIXED

**Severity**: 🔴 **Critical** - Block import failure

**Issue**:
- Proposer generated blobs and streamed them to validators
- Validators received blobs via ProposalPart stream and stored them
- **Proposer never called `blob_engine.verify_and_store()` for own blobs**
- When block was decided, proposer had no blobs → `get_for_import()` returned empty
- EL import failed: "blob count mismatch" (header says N blobs, got 0)

**Root Cause**: GetValue handler (app.rs) only stored undecided proposal data, never stored blobs

**Fix Applied** (app.rs:145-189):
```rust
// CRITICAL FIX: Store blobs locally for our own proposal
if let Some(ref bundle) = blobs_bundle {
    if !bundle.blobs.is_empty() {
        let blob_sidecars: Vec<_> = bundle.blobs.iter().enumerate()
            .map(|(index, blob)| {
                BlobSidecar::from_bundle_item(
                    index as u8,
                    blob.clone(),
                    bundle.commitments[index],
                    bundle.proofs[index],
                )
            })
            .collect();

        state.blob_engine()
            .verify_and_store(height, round.as_i64(), &blob_sidecars)
            .await?;
    }
}
```

**Helper Added** (proposal_part.rs:163-202):
- Created `BlobSidecar::from_bundle_item()` for initial bundle conversions; later Phase 4 rebuild logic re-signs headers from stored metadata during restream.

**Result**: ✅ Proposer now stores own blobs with UNDECIDED state, promoted to DECIDED on finalization

---

#### Finding #2: RestreamProposal Used Wrong Proposer Address ✅ FIXED

**Severity**: 🟡 **Medium** - Sync recovery failure

**Issue**:
- `make_proposal_parts()` hardcoded `self.address` at line 551
- During RestreamProposal, this put wrong proposer in ProposalInit
- Validators receiving restreamed proposal saw incorrect proposer attribution
- **Reference implementations (malachite & snapchain) have the same bug!**

**Fix Applied** (state.rs:518-558):

1. **Modified `stream_proposal()` signature**:
```rust
pub fn stream_proposal(
    &mut self,
    value: LocallyProposedValue<LoadContext>,
    data: Bytes,
    blobs_bundle: Option<BlobsBundle>,
    proposer: Option<Address>,  // NEW: For RestreamProposal support
) -> impl Iterator<Item = StreamMessage<ProposalPart>> {
    let proposer_address = proposer.unwrap_or(self.address);  // Default to self
    let parts = self.make_proposal_parts(value, data, blobs_bundle, proposer_address);
    // ...
}
```

2. **Modified `make_proposal_parts()` signature**:
```rust
fn make_proposal_parts(
    &self,
    value: LocallyProposedValue<LoadContext>,
    data: Bytes,
    blobs_bundle: Option<BlobsBundle>,
    proposer: Address,  // NEW: Explicit proposer parameter
) -> Vec<ProposalPart> {
    parts.push(ProposalPart::Init(ProposalInit::new(
        value.height,
        value.round,
        proposer,  // Use provided proposer, not self.address
    )));
    // ...
}
```

3. **Implemented RestreamProposal handler** (app.rs:335-444):
```rust
AppMsg::RestreamProposal { height, round, valid_round, address, value_id } => {
    // Guard: Only original proposer handles (see Finding #2.1)
    if state.address != address {
        debug!("Ignoring RestreamProposal: we are not the original proposer");
        continue;
    }

    // Retrieve proposal data
    let proposal = state.store.get_undecided_proposal(height, proposal_round).await?;
    let proposal_bytes = state.store.get_block_data(height, proposal_round).await?;

    // Retrieve and reconstruct blobs
    let blobs_bundle = state.blob_engine()
        .get_undecided_blobs(height, proposal_round.as_i64())
        .await?
        .map(|sidecars| reconstruct_blobs_bundle(sidecars));

    // Restream with proper proposer
    for stream_message in state.stream_proposal(
        locally_proposed_value,
        proposal_bytes,
        blobs_bundle,
        None  // Use self.address (we're the proposer)
    ) {
        channels.network.send(NetworkMsg::PublishProposalPart(stream_message)).await?;
    }
}
```

4. **Added `get_undecided_blobs()` to BlobEngine** (engine.rs:135-139, 273-288):
```rust
async fn get_undecided_blobs(
    &self,
    height: Height,
    round: i64,
) -> Result<Vec<BlobSidecar>, BlobEngineError> {
    self.store.get_undecided_blobs(height, round).await
}
```

**Result**: ✅ RestreamProposal correctly uses original proposer address and retrieves/restreams blobs

---

#### Finding #2.1: RestreamProposal Signature Verification Bug 🔴 CRITICAL SECURITY FIX

**Severity**: 🔴 **CRITICAL** - Signature verification failure, all restreamed proposals rejected

**Issue Discovered During Cross-Reference**:
- Consensus engine broadcasts `RestreamProposal` to **ALL validators** (not just proposer)
- Every validator tried to handle the message
- **Signature mismatch**:
  - Init part: `proposer = address` (original proposer, e.g., Alice)
  - Fin part: Signed with `self.signing_provider` (wrong validator, e.g., Bob)
  - Verification: Looks up Alice's public key, checks against Bob's signature → **FAILS**

**Signature Verification Flow** (state.rs:720-765):
```rust
fn verify_proposal_signature(&self, parts: &ProposalParts) -> Result<(), SignatureVerificationError> {
    // Line 755: Get public key for proposer from Init
    let public_key = self.get_validator_set()
        .get_by_address(&parts.proposer)  // ← Uses Init proposer (Alice)
        .map(|v| v.public_key);

    // Line 760: Verify signature matches that public key
    if !self.signing_provider.verify(&hash, signature, &public_key) {
        return Err(SignatureVerificationError::InvalidSignature);  // ← FAILS with Bob's signature!
    }
}
```

**Critical Fix Applied** (app.rs:341-348):
```rust
AppMsg::RestreamProposal { height, round, valid_round, address, value_id } => {
    // CRITICAL: Only the original proposer should handle RestreamProposal.
    // The Fin part is signed with self.signing_provider, which must match the proposer
    // address stamped in the Init part. If we're not the original proposer, our signature
    // will fail verification on all peers (they'll look up the Init proposer's public key
    // and find our signature doesn't match).
    if state.address != address {
        debug!(
            %height, %round, %address,
            our_address = %state.address,
            "Ignoring RestreamProposal: we are not the original proposer"
        );
        continue;  // ← EXIT EARLY if not original proposer
    }

    info!(%height, %round, "Restreaming our own proposal");
    // ... rest of handler (only original proposer reaches here)
}
```

**Why This Works**:
- ✅ Only original proposer (Alice) handles RestreamProposal
- ✅ Init has `proposer = self.address` (Alice's address)
- ✅ Fin signed with `self.signing_provider` (Alice's key)
- ✅ **Addresses match**, signature verification succeeds on all peers

**Comparison with Reference Implementations**:

| Implementation | Has Proposer Guard? | Signature Correct? |
|---------------|---------------------|-------------------|
| Malachite example | ❌ No | ❌ No (uses self.address always) |
| Snapchain | ❌ No | ⚠️ Single-blob approach |
| **Ultramarine** | ✅ **Yes (341-348)** | ✅ **Yes (None → self.address)** |

**Result**: 🛡️ **Security vulnerability eliminated** - Only original proposer restreams, signatures verify correctly

---

#### Finding #3: SyncedValuePackage Used Bincode ✅ FIXED (Protobuf Migration)

**Severity**: 🟡 **Medium** - Forward compatibility risk

**Issue**:
- Initial implementation used bincode + version byte for `SyncedValuePackage` serialization
- Inconsistent with rest of codebase (all consensus types use protobuf)
- Manual versioning error-prone
- No forward/backward compatibility guarantees

**Protobuf is Idiomatic for Malachite Clients**:
- All consensus messages use protobuf
- Built-in versioning via field numbers
- Forward/backward compatible schema evolution
- Standard serialization for network types

**Fix Applied**:

1. **Added protobuf schema** (sync.proto:69-88):
```protobuf
message SyncedValuePackage {
  oneof package {
    FullPackage full = 1;
    MetadataOnlyPackage metadata_only = 2;
  }
}

message FullPackage {
  Value value = 1;
  bytes execution_payload_ssz = 2;
  repeated BlobSidecar blob_sidecars = 3;
}

message MetadataOnlyPackage {
  Value value = 1;
}
```

2. **Implemented Protobuf trait** (sync.rs:259-332):
```rust
impl Protobuf for SyncedValuePackage {
    type Proto = proto::SyncedValuePackage;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        match proto.package {
            Some(proto::synced_value_package::Package::Full(full)) => {
                let value = Value::from_proto(full.value)?;
                let blob_sidecars = full.blob_sidecars
                    .into_iter()
                    .map(BlobSidecar::from_proto)
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(SyncedValuePackage::Full {
                    value,
                    execution_payload_ssz: full.execution_payload_ssz,
                    blob_sidecars,
                })
            }
            Some(proto::synced_value_package::Package::MetadataOnly(metadata)) => {
                Ok(SyncedValuePackage::MetadataOnly {
                    value: Value::from_proto(metadata.value)?,
                })
            }
            None => Err(ProtoError::missing_field::<proto::SyncedValuePackage>("package")),
        }
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        // ... (symmetrical encoding)
    }
}
```

3. **Added Protobuf trait for BlobSidecar** (proposal_part.rs:219-312):
```rust
impl Protobuf for BlobSidecar {
    type Proto = crate::proto::BlobSidecar;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let blob = Blob::new(proto.blob)?;
        let kzg_commitment = KzgCommitment::from_slice(&proto.kzg_commitment)?;
        let kzg_proof = KzgProof::from_slice(&proto.kzg_proof)?;
        let signed_block_header = SignedBeaconBlockHeader::from_proto(proto.signed_block_header)?;

        Ok(Self {
            index: proto.index as u8,
            blob,
            kzg_commitment,
            kzg_proof,
            signed_block_header,
            kzg_commitment_inclusion_proof: proto.kzg_commitment_inclusion_proof
                .into_iter()
                .map(|bytes| B256::from_slice(&bytes))
                .collect(),
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        // ... (symmetrical encoding)
    }
}
```

4. **Updated RocksDB blob storage** (blob_engine/src/store/rocksdb.rs:73-102):
```rust
fn serialize_blob(blob: &BlobSidecar) -> Result<Vec<u8>, BlobStoreError> {
    let proto = blob.to_proto()?;
    let mut buf = Vec::new();
    proto.encode(&mut buf)?;  // Protobuf encoding
    Ok(buf)
}

fn deserialize_blob(bytes: &[u8]) -> Result<BlobSidecar, BlobStoreError> {
    let proto = ultramarine_types::proto::BlobSidecar::decode(bytes)?;
    BlobSidecar::from_proto(proto)
}
```

**Result**: ✅ 100% protobuf serialization across entire codebase (consensus + storage)

---

#### Bincode Cleanup ✅ COMPLETED

**Removed bincode dependencies**:
- ❌ Removed from workspace `Cargo.toml` (line 80)
- ❌ Removed from `crates/types/Cargo.toml` (line 27)
- ❌ Removed from `crates/blob_engine/Cargo.toml`

**Replaced with protobuf**:
- ✅ Added `malachitebft-proto` to blob_engine
- ✅ Added `prost` to blob_engine
- ✅ All serialization now uses protobuf

**Verification**: All tests pass with protobuf-only serialization

---

#### Summary of Files Modified

**Core Implementation**:
- `crates/node/src/app.rs` (145-189, 335-444)
  - Finding #1 fix: Proposer blob storage
  - Finding #2 fix: RestreamProposal implementation
  - Finding #2.1 fix: Proposer guard (security)

- `crates/consensus/src/state.rs` (518-558)
  - Finding #2 fix: stream_proposal proposer parameter
  - Finding #2 fix: make_proposal_parts proposer parameter

- `crates/types/src/proposal_part.rs` (163-202, 219-312)
  - Finding #1 helper: from_bundle_item()
  - Finding #3 fix: BlobSidecar Protobuf trait

- `crates/types/src/sync.rs` (62-332)
  - Finding #3 fix: SyncedValuePackage Protobuf trait
  - Removed bincode, added protobuf encode/decode

- `crates/types/proto/sync.proto` (69-88)
  - Finding #3 fix: SyncedValuePackage schema

- `crates/blob_engine/src/engine.rs` (135-139, 273-288)
  - Finding #2 support: get_undecided_blobs() method

- `crates/blob_engine/src/store/rocksdb.rs` (73-102)
  - Finding #3 fix: Protobuf serialization for blobs

**Dependency Updates**:
- `Cargo.toml` (removed bincode)
- `crates/types/Cargo.toml` (removed bincode)
- `crates/blob_engine/Cargo.toml` (removed bincode, added protobuf)

---

#### Cross-Reference with Reference Implementations

Our fixes were validated against:
1. **Malachite examples/channel** (b205f4252f3064d9a74716056f63834ff33f2de9)
2. **Snapchain client** (LoadNetwork internal)

**Findings**:
- ✅ Our RestreamProposal is **more correct** (has proposer guard + blob support)
- ✅ Our serialization is **more consistent** (100% protobuf)
- ✅ Our proposer blob storage is **complete** (reference impls don't handle blobs)

**Key Improvement**: We fixed bugs that exist in BOTH reference implementations!

---

#### Testing Status

- ✅ `cargo fmt --all`
- ✅ `cargo clippy -p ultramarine-consensus`
- ✅ `cargo test -p ultramarine-consensus --lib` (23/23 passing)
- ⚠️ Remaining warnings reside in other crates (`ultramarine-types`, `ultramarine-blob-engine`) from earlier TODOs.

---

#### Next Steps

1. Phase 5 integration validation (restart survival, restream replay, multi-validator blob sync).
2. Document metadata-only storage expectations for developers/operators.
3. Optional: add metrics around metadata tables and blob promotion performance.

---

## Phase 5 Testnet: Integration Testing & Observability 🔄 IN PROGRESS

### Goal
Validate blob sidecar integration end-to-end with comprehensive metrics, integration tests, and testnet workflows.

**Status**: 🔄 **In Progress**
**Date Started**: 2025-10-28
**Estimated Time**: 2-3 days

### Scope

**Metrics Instrumentation**:
- Add 12+ Prometheus metrics to BlobEngine (verification latency, storage size, lifecycle transitions)
- Instrument verification (KZG proof validation), storage (undecided/decided), and lifecycle (promotions, imports, cleanup)

**Grafana Dashboard**:
- Create 10+ blob-specific panels (verification rate, P99 latency, storage by state, failures, lifecycle transitions)
- Add cross-layer correlation panels (blob imports vs consensus height, blobs per block)

**Integration Tests**:
- Blob lifecycle E2E test (spam → verify → decide → import)
- Restart survival test (persistence and hydration validation)
- Multi-validator sync test (P2P blob propagation)

**Testnet Workflow**:
- Document blob testing procedures (`TESTNET_WORKFLOW.md`)
- Validate spam tool blob support (`--blobs` flag)
- Establish performance baselines (P99 verification latency, storage growth)

### Current State (2025-10-28)

**Infrastructure** ✅:
- 3-node testnet with Docker Compose (reth + ultramarine)
- Prometheus scraping 6 targets (3 EL + 3 CL) every 1s
- Grafana dashboard with 18 panels (5 Malachite, 13 Reth)
- Spam tool with `--blobs` flag for EIP-4844 transactions
- Automated setup (`make all`, `make spam`, `make clean-net`)

**Observability Gaps** ❌:
- Zero blob-specific metrics (no verification time, storage size, lifecycle tracking)
- Zero blob dashboard panels (Grafana has 18 panels, none for blobs)
- No integration tests (23/23 unit tests pass, but no E2E validation)

### Implementation Plan

**Phase A**: Metrics Instrumentation (4-6 hours)
- Create `BlobEngineMetrics` module with 12 metrics (verification, storage, lifecycle, latency)
- Instrument key operations (verify_and_store, mark_decided, get_for_import, drop_round)
- Register metrics in node startup

**Phase B**: Grafana Dashboard (2-3 hours)
- Add "Blob Sidecars (EIP-4844)" row to dashboard
- Create 10 panels (verification rate, latency, storage, failures, lifecycle, correlation)
- Export updated dashboard JSON

**Phase C**: Integration Tests (6-8 hours)
- Blob lifecycle E2E test (`tests/integration/blob_lifecycle.rs`)
- Restart survival test (`tests/integration/blob_restart_survival.rs`)
- Multi-validator sync test (`tests/integration/blob_multi_validator_sync.rs`)

**Phase D**: Documentation (2 hours)
- Create `TESTNET_WORKFLOW.md` with testing procedures
- Update `README.md` with blob testing quick start
- Document metrics reference

### Success Criteria

- ✅ BlobEngine exposes 12+ Prometheus metrics
- ✅ Grafana dashboard has 10+ blob-specific panels
- ✅ 3 integration tests passing (lifecycle, restart, multi-validator)
- ✅ Load testing validates 100 blob tx/sec performance
- ✅ Documentation covers testnet workflows
- ✅ Performance baselines established (P99 < 50ms, no failures)

### See Also

- [PHASE5_TESTNET.md](./PHASE5_TESTNET.md) - Complete implementation plan with daily progress log
- [PHASE4_PROGRESS.md](./PHASE4_PROGRESS.md) - Phase 1-4 completion log
- Makefile:427-550 - Testnet automation targets

---

### Phase 6: Pruning Policy ⏳ PENDING

**Status**: Ready to start

**Dependencies**: Phase 5 Testnet completed (observability and integration tests in place)

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
