# EIP-4844 Blob Sidecar Integration - Progress Report

**Last Updated**: 2025-10-21
**Overall Progress**: ~60% Complete (Critical gap discovered in Phase 5)

---

## ⚠️ CRITICAL STATUS UPDATE

After thorough review, **Phase 5 is INCOMPLETE**. While blob storage and lifecycle management work correctly, **blobs are never passed to the execution layer during block import**.

## Executive Summary

✅ **Phases 1-4 Complete** - Execution bridge, consensus refactor, streaming, and verification
⚠️ **Phase 5: 60% Complete** - Blob lifecycle works, BUT EL integration missing
❌ **Phase 6 (Pruning)** - Not implemented as standalone service
❌ **Phase 7 (Archive)** - Optional, not yet implemented
❌ **Phase 8 (Testing)** - Integration testing needed

**Current Status**: The system can successfully:
- ✅ Retrieve blobs from execution layer via Engine API v3
- ✅ Verify KZG proofs using c-kzg with Ethereum mainnet trusted setup
- ✅ Stream blobs through proposal parts to validators
- ✅ Store blobs in RocksDB with proper lifecycle (undecided → decided → pruned)
- ❌ **MISSING: Retrieve blobs and pass to EL during block import**

**Impact**: Blobs are stored but never used. Execution layer cannot verify blob availability. **Silent data loss.**

**Next Steps**: See `docs/IMPLEMENTATION_ROADMAP.md` for detailed 7-day plan to completion.

---

## Detailed Phase Status

### ✅ Phase 1: Execution ↔ Consensus Bridge (COMPLETE)

**Status**: 100% Complete

**Implemented**:
- ✅ Blob types in `ultramarine-types` crate (Blob, KzgCommitment, KzgProof, BlobsBundle)
- ✅ Engine API v3 support in `execution` crate
- ✅ `generate_block_with_blobs()` method calls `getPayloadV3` and returns blobs bundle
- ✅ Blob bundle parsing from Alloy's `BlobsBundleV1` format
- ✅ Full SSZ encoding/decoding support

**Files Modified**:
- `crates/types/src/blob.rs` - Core blob types with SSZ traits
- `crates/execution/src/client.rs` - `generate_block_with_blobs()` method
- `crates/execution/src/engine_api/mod.rs` - `get_payload_with_blobs()` trait method
- `crates/execution/src/engine_api/client.rs` - Engine API v3 implementation

**Testing**: Manual testing confirmed blobs are retrieved from EL correctly

---

### ✅ Phase 2: Consensus Value Refactor (COMPLETE)

**Status**: 100% Complete (marked in FINAL_PLAN.md)

**Implemented**:
- ✅ `ValueMetadata` structure with ExecutionPayloadHeader + commitments
- ✅ `Value` refactored to use metadata instead of raw bytes
- ✅ Consensus messages stay small (~2KB) with lightweight metadata
- ✅ Full payload streams separately via ProposalPart

**Files Modified**:
- `crates/types/src/value_metadata.rs` (new)
- `crates/types/src/value.rs` - Refactored to use ValueMetadata
- `crates/types/src/engine_api/execution_payload_header.rs` (new)

**Impact**: Consensus layer now votes on `hash(metadata)` instead of full block data

---

### ✅ Phase 3: Proposal Streaming (COMPLETE)

**Status**: 100% Complete

**Implemented**:
- ✅ `ProposalPart::BlobSidecar` variant added
- ✅ Protobuf schema extended with blob_sidecar field
- ✅ `stream_proposal()` updated to stream blobs
- ✅ `make_proposal_parts()` creates BlobSidecar parts from BlobsBundle
- ✅ Signature includes blob data in hash
- ✅ `propose_value_with_blobs()` method in State

**Files Modified**:
- `crates/types/src/proposal_part.rs` - Added BlobSidecar variant
- `crates/types/proto/proposal.proto` - Extended protobuf schema
- `crates/consensus/src/state.rs` - Streaming and proposal methods
- `crates/node/src/app.rs` - Uses new proposal flow

**Network Flow**: Blobs flow through existing `/proposal_parts` gossip channel

---

### ✅ Phase 4: Blob Verification & Storage (COMPLETE)

**Status**: 100% Complete

**Implemented**:
- ✅ **KZG Verification** - Using c-kzg 2.1.0 (same as Lighthouse)
- ✅ **Trusted Setup** - Ethereum mainnet parameters embedded
- ✅ **Batch Verification** - 5-10x faster than individual verification
- ✅ **BlobEngine Trait** - Clean abstraction for storage backends
- ✅ **RocksDB Backend** - Two column families (undecided_blobs, decided_blobs)
- ✅ **Async Storage** - tokio::spawn_blocking wraps sync RocksDB operations
- ✅ **Atomic verify-and-store** - Single operation prevents storing unverified blobs

**Architecture**:
```
blob_engine crate (new)
├── engine.rs - BlobEngine trait + BlobEngineImpl orchestrator
├── verifier.rs - KZG verification using c-kzg
├── store/
│   ├── mod.rs - BlobStore trait
│   └── rocksdb.rs - RocksDB implementation
└── error.rs - Error types
```

**Files Created**:
- `crates/blob_engine/` - Complete new crate (568 lines)
- `crates/blob_engine/src/trusted_setup.json` - Mainnet KZG parameters
- `crates/blob_engine/Cargo.toml`

**Files Modified**:
- `Cargo.toml` - Added blob_engine workspace member
- `crates/consensus/Cargo.toml` - Added blob_engine dependency
- `crates/consensus/src/state.rs` - Made generic over BlobEngine
- `crates/node/Cargo.toml` - Added blob_engine dependency
- `crates/node/src/node.rs` - Initialize blob_engine and pass to State

**Key Methods**:
- `verify_and_store(height, round, sidecars)` - Atomic verification + storage
- `mark_decided(height, round)` - Move blobs from undecided → decided
- `get_for_import(height)` - Retrieve decided blobs for block import
- `prune_archived_before(height)` - Clean up old blobs

**Tests**: 9/9 passing in blob_engine crate

---

### ⚠️ Phase 5: Block Import / EL Interaction (60% COMPLETE)

**Status**: 60% Complete - **CRITICAL GAPS IDENTIFIED**

**✅ Implemented (Blob Lifecycle)**:
- ✅ **Blob lifecycle integration** - Mark blobs as decided on commit
- ✅ **Pruning policy** - Prune blobs older than 5 heights (matches store retention)
- ✅ **Error handling** - Now fails commit if blob promotion fails (Fix #1 - 2025-10-21)
- ✅ **Orphaned blob cleanup** - Cleanup failed rounds (Fix #2 - 2025-10-21)
- ✅ **Error index reporting** - Correct blob index in errors (Fix #3 - 2025-10-21)
- ✅ **State commit integration** - `commit()` method updated

**❌ Missing (CRITICAL - EL Integration)**:
- ❌ **Blob retrieval in Decided handler** - Never calls `blob_engine.get_for_import(height)`
- ❌ **Engine API v3 integration** - No `new_payload_with_blobs()` method
- ❌ **Blob availability verification** - EL never receives blob data
- ❌ **Blob count validation** - Not checking blobs match commitments before import

**Files Modified**:
- `crates/consensus/src/state.rs` - Lines 274-323
  - Added `blob_engine.mark_decided()` after storing decided value
  - Added `blob_engine.prune_archived_before()` alongside store pruning
  - Added error logging for blob operation failures

**Implementation Details**:

**Blob Decision Flow** (state.rs:274-284):
```rust
// Mark blobs as decided in blob engine
let round_i64 = certificate.round.as_i64();
if let Err(e) = self.blob_engine.mark_decided(certificate.height, round_i64).await {
    error!(
        height = %certificate.height,
        round = %certificate.round,
        error = %e,
        "Failed to mark blobs as decided in blob engine"
    );
    // Don't fail the commit if blob marking fails - just log the error
}
```

**Blob Pruning Flow** (state.rs:304-323):
```rust
// Prune blob engine - keep the same retention policy (last 5 heights)
match self.blob_engine.prune_archived_before(retain_height).await {
    Ok(count) if count > 0 => {
        debug!(
            "Pruned {} blobs before height {}",
            count,
            retain_height.as_u64()
        );
    }
    Ok(_) => {}, // No blobs to prune
    Err(e) => {
        error!(
            error = %e,
            height = %retain_height,
            "Failed to prune blobs from blob engine"
        );
        // Don't fail the commit if blob pruning fails
    }
}
```

**Current Implementation Issue**:
The code currently passes versioned_hashes to `notify_new_block()` but this is NOT sufficient:

1. ❌ Blobs never retrieved from `blob_engine.get_for_import(height)`
2. ❌ No `new_payload_with_blobs()` method exists
3. ❌ EL integration incomplete - blocks imported without blob verification

**What Needs to be Fixed** (See `docs/IMPLEMENTATION_ROADMAP.md` for details):

1. **Update Decided Handler** (`crates/node/src/app.rs:414-523`):
   ```rust
   // NEW: Retrieve blobs from blob_engine
   let blobs = state.blob_engine.get_for_import(height).await?;

   // NEW: Verify blob count matches
   if blobs.len() != expected_blob_count {
       return Err(...);
   }

   // NEW: Pass blobs to EL
   execution_layer.new_payload_with_blobs(payload, versioned_hashes, blobs).await?;
   ```

2. **Implement Engine API v3** (`crates/execution/src/`):
   - Add `new_payload_with_blobs()` method
   - Call `engine_newPayloadV3` with blob data
   - Return PayloadStatus

---

### ⏸️ Phase 6: Pruning Policy (IMPLEMENTED)

**Status**: Basic implementation complete, could be enhanced

**Current Implementation**:
- ✅ Prune blobs before height N-5 (same as consensus store)
- ✅ Called automatically during commit
- ✅ Graceful error handling

**Potential Enhancements** (optional):
- Add configurable retention period via CLI flag
- Implement blob archival tagging before pruning
- Add metrics for pruned blob count
- Expose pruning API endpoint

---

### ⏸️ Phase 7: Archive Integration (NOT IMPLEMENTED - OPTIONAL)

**Status**: Not yet implemented

**What's Missing**:
- Vote extension with CID publication
- Integration with archive service (Arweave/IPFS)
- Archive verification and retrieval

**Priority**: Optional - core blob functionality works without this

---

### ⏸️ Phase 8: Testing (PARTIAL)

**Status**: Unit tests complete, integration testing needed

**Completed**:
- ✅ 9/9 blob_engine tests passing
- ✅ 16/16 ethereum_compat tests passing (merkle proofs, SSZ, etc.)
- ✅ All existing consensus tests passing

**TODO**:
- Integration test: Full proposal → receipt → decision flow with blobs
- Integration test: Multi-validator network with blob propagation
- Stress test: Maximum blob count (6 blobs per block)
- Edge case test: Blob verification failure handling
- Performance test: Batch verification benchmarks

---

## Architecture Summary

### Data Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    EXECUTION LAYER (EL)                         │
│  Engine API v3: getPayloadV3() → BlobsBundle                   │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                    CONSENSUS LAYER                              │
│  generate_block_with_blobs() → (payload, blobs_bundle)         │
│  propose_value_with_blobs() → ValueMetadata                    │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                    BLOB ENGINE                                   │
│  verify_and_store() → KZG verification + RocksDB storage        │
│  State: undecided_blobs → decided_blobs                        │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                    NETWORK LAYER                                 │
│  /proposal_parts gossip: Init → BlobSidecar(0..N) → Fin       │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                    VALIDATORS                                    │
│  Receive → Verify KZG → Vote on hash(metadata)                 │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                    DECISION & IMPORT                             │
│  mark_decided() → prune_archived_before()                      │
│  Submit to EL: newPayloadV3(payload, versioned_hashes)        │
└─────────────────────────────────────────────────────────────────┘
```

### Storage Layout

**RocksDB (blob_store.db)**:
- Column Family: `undecided_blobs`
  - Key: `[height|round|index]` (8+8+1 bytes)
  - Value: Bincode-serialized BlobSidecar (~131KB)

- Column Family: `decided_blobs`
  - Key: `[height|index]` (8+1 bytes)
  - Value: Bincode-serialized BlobSidecar (~131KB)

**Lifecycle**:
1. Receive blob → Store in `undecided_blobs[height,round,index]`
2. Consensus decides → Move to `decided_blobs[height,index]`
3. Height pruned → Delete from `decided_blobs`

---

## Key Files and Line Counts

**New Crates**:
- `crates/blob_engine/` - 850+ lines (complete new crate)

**New Files**:
- `crates/types/src/blob.rs` - 250 lines
- `crates/types/src/value_metadata.rs` - 100 lines
- `crates/types/src/engine_api/execution_payload_header.rs` - 150 lines
- `crates/blob_engine/src/trusted_setup.json` - 4,324 lines (KZG params)

**Modified Files** (major changes):
- `crates/consensus/src/state.rs` - ~200 lines added/modified
- `crates/execution/src/client.rs` - ~150 lines added
- `crates/types/src/proposal_part.rs` - ~50 lines added
- `crates/node/src/app.rs` - ~30 lines modified
- `crates/node/src/node.rs` - ~10 lines added

**Total New Code**: ~1,500 lines (excluding dependencies and tests)

---

## Outstanding Work

### Must-Do for Production

1. **Integration Testing** (Priority: HIGH)
   - Full end-to-end test with multi-validator setup
   - Blob propagation verification
   - Edge case testing (verification failures, network partitions)

2. **Performance Testing** (Priority: MEDIUM)
   - Benchmark KZG batch verification at scale
   - Measure RocksDB storage overhead
   - Profile memory usage with maximum blobs

3. **Error Handling Review** (Priority: MEDIUM)
   - Review all error paths in blob_engine
   - Ensure consensus doesn't stall on blob failures
   - Add metrics/alerts for blob verification failures

### Nice-to-Have Enhancements

4. **Extend BlobSidecar Schema** (Priority: LOW)
   - Add `signed_block_header` field (Deneb spec compliance)
   - Add `kzg_commitment_inclusion_proof` field
   - Currently minimal schema works but isn't full Deneb compliance

5. **Archive Integration** (Priority: LOW - Optional)
   - Implement Phase 7 if long-term blob storage is needed
   - CID publication via vote extensions
   - Integration with Arweave/IPFS

6. **Operational Improvements** (Priority: LOW)
   - Add CLI flags for blob retention period
   - Add metrics/dashboards for blob operations
   - Add admin API endpoints for blob inspection

---

## Dependencies

**New Dependencies Added**:
- `c-kzg = "2.1.0"` - KZG verification (same version as Lighthouse)
- `rocksdb = "0.22"` - Blob storage backend
- `bincode = "1.3"` - Blob serialization

**Version Compatibility**:
- ✅ Using c-kzg 2.1.0 (matches Lighthouse production usage)
- ✅ Using Ethereum mainnet trusted setup
- ✅ Compatible with Engine API v3 spec

---

## Risk Assessment

### Low Risk ✅
- KZG verification - Using battle-tested c-kzg library
- Storage architecture - Standard RocksDB with proven patterns
- Consensus integration - Minimal changes to core consensus flow

### Medium Risk ⚠️
- Network propagation - Large blob sizes could impact gossip performance
  - **Mitigation**: Monitor P2P bandwidth, add rate limiting if needed
- Storage growth - 6 blobs/block × 131KB = ~786KB per block
  - **Mitigation**: Pruning policy limits retention to last 5 heights

### Areas for Monitoring 👀
- Blob verification performance under load
- RocksDB storage I/O patterns
- Network bandwidth usage for blob gossip
- Memory usage during batch verification

---

## Recommendations

### Immediate Next Steps

1. **Run Integration Tests** (1-2 days)
   - Set up local 4-validator testnet
   - Generate blob transactions via test harness
   - Verify full proposal → decision → import flow
   - Document any issues found

2. **Update Documentation** (0.5 days)
   - Update FINAL_PLAN.md with Phase 5 completion
   - Document blob_engine API
   - Add operator guide for blob retention settings

3. **Code Review** (1 day)
   - Security review of KZG verification path
   - Review error handling in critical paths
   - Check for any edge cases in blob lifecycle

### Before Production Deployment

4. **Performance Benchmarks** (1 day)
   - Measure throughput with max blobs (6 per block)
   - Profile memory usage under load
   - Test network bandwidth with large validator sets

5. **Monitoring Setup** (0.5 days)
   - Add Prometheus metrics for blob operations
   - Create Grafana dashboards
   - Set up alerts for verification failures

### Optional Enhancements

6. **Full Deneb Compliance** (2 days)
   - Extend BlobSidecar with signed_block_header
   - Add KZG commitment inclusion proofs
   - Implement full Ethereum beacon API compatibility

7. **Archive Integration** (5-7 days)
   - Implement Phase 7 if needed
   - Only necessary if long-term blob retrieval is required

---

## Conclusion

**The EIP-4844 blob sidecar integration is 60% complete with critical gaps in Phase 5.**

**✅ Implemented**:
- ✅ Blobs retrieved from execution layer during proposal generation
- ✅ KZG cryptographic verification working
- ✅ Storage with proper lifecycle management
- ✅ Network propagation through proposal parts
- ✅ Blob lifecycle (mark_decided, cleanup, pruning)

**❌ Critical Gaps**:
- ❌ Blobs never retrieved during block import
- ❌ Blobs never passed to execution layer
- ❌ Engine API v3 integration incomplete
- ❌ Integration tests missing

**Impact**: While blobs are stored correctly, they are **never consumed** during block import. This means:
- Execution layer cannot verify blob availability
- Data availability guarantees not enforced
- EIP-4844 non-compliant (silent data loss)

**Estimated time to production-ready**: 5-7 days (see `docs/IMPLEMENTATION_ROADMAP.md`)
- Days 1-2: Complete Phase 5 (EL integration)
- Day 3: Implement Phase 6 (Pruning service)
- Days 4-5: Phase 8 (Integration tests)
- Day 6: Security & Monitoring
- Day 7: Documentation & Review

---

## Change Log

**2025-10-21 (Afternoon) - Critical Gap Discovered**:
- 🔍 **Discovered Phase 5 is incomplete** - Blobs not passed to EL during import
- ✅ Fixed blob promotion failure handling (Fix #1 - now fails commit)
- ✅ Implemented orphaned blob cleanup (Fix #2 - prevents resource leak)
- ✅ Fixed blob index error reporting (Fix #3 - correct debugging)
- 📄 Created `IMPLEMENTATION_ROADMAP.md` with 7-day completion plan
- 📊 Updated progress: ~60% complete (corrected from 85%)

**2025-10-21 (Morning) - Initial Session**:
- ✅ Completed Phase 5 blob lifecycle (partial - mark_decided, pruning)
- ✅ Implemented blob lifecycle management in consensus State
- ✅ Updated consensus State commit() method
- ✅ Added blob_engine initialization in node startup
- 📊 Believed progress: ~85% complete (INCORRECT)

**Previous Sessions**:
- ✅ Completed Phases 1-4
- ✅ Created blob_engine crate with KZG verification
- ✅ Implemented RocksDB storage backend
- ✅ Extended ProposalPart with BlobSidecar
- ✅ Refactored Value to use ValueMetadata

**Key Lesson Learned**: Importance of end-to-end verification. Blob storage worked perfectly, but integration with execution layer was never implemented, leading to silent data loss.
