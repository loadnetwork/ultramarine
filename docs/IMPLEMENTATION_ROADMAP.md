# EIP-4844 Blob Integration - Implementation Roadmap

**Last Updated**: 2025-10-21
**Current Status**: Phase 5 (Block Import) - 60% Complete
**Estimated Time to Production**: 5-7 days

---

## 🎯 Executive Summary

This roadmap outlines the remaining work to complete EIP-4844 blob sidecar integration into Ultramarine. The critical discovery is that **Phase 5 is incomplete** - while blobs are verified and stored, they are **never retrieved and passed to the execution layer** during block import.

### Current State:
- ✅ Phases 1-4: Complete (Execution bridge, value refactor, streaming, verification)
- ⚠️ **Phase 5: 60% Complete** - Blob lifecycle works, but EL integration missing
- ❌ Phase 6: Not started (Pruning)
- ❌ Phase 8: Not started (Integration tests)

### Critical Path to Production:
1. **Complete Phase 5** (2 days) - Get blobs to execution layer
2. **Implement Phase 6** (1 day) - Prevent disk exhaustion
3. **Write Phase 8 tests** (2-3 days) - Validate end-to-end
4. **Security review** (1 day) - Verify trusted setup
5. **Deploy monitoring** (0.5 days) - Production observability

---

## 📋 Completed Work

### ✅ Phase 1: Execution ↔ Consensus Bridge
- Engine API types defined
- Blob bundle retrieval from EL (`generate_block_with_blobs`)
- Status: **Complete**

### ✅ Phase 2: Consensus Value Refactor
- `ValueMetadata` with KZG commitments
- Lightweight consensus messages (~2KB)
- Status: **Complete**

### ✅ Phase 3: Proposal Streaming
- `BlobSidecar` type in `ProposalPart` enum
- Streaming infrastructure for blobs
- Status: **Complete**

### ✅ Phase 4: Blob Verification & Storage
- `blob_engine` crate with KZG verification (c-kzg 2.1.0)
- RocksDB storage with lifecycle management
- Blob lifecycle: undecided → decided → archived
- Status: **Complete**

### ✅ Phase 5: Blob Lifecycle (Partial)
**Completed**:
- ✅ Blob tracking in consensus State
- ✅ `verify_and_store()` during proposal receipt
- ✅ `mark_decided()` during commit (with Fix #1 - fails commit on error)
- ✅ `drop_round()` cleanup for failed proposals (Fix #2)
- ✅ Proper error index reporting (Fix #3)
- ✅ Pruning of old blobs after height advance

**Missing** (CRITICAL):
- ❌ **Blob retrieval in `Decided` handler** - Never calls `get_for_import()`
- ❌ **Engine API v3 integration** - Never passes blobs to execution layer
- ❌ **Blob availability verification** - EL never receives blob data

---

## 🚨 Critical Gap: Phase 5 Completion

### The Problem

**Current Flow** (BROKEN):
```
Proposer:
  1. GetValue → EL returns (payload + blobs)
  2. Store blobs via verify_and_store() ✅
  3. Stream blobs to validators ✅
  4. Validators verify and store ✅

Consensus Decision:
  5. mark_decided() promotes blobs ✅
  6. Decided handler retrieves block data ✅
  7. ❌ Blobs NEVER retrieved from blob_engine
  8. ❌ EL called with versioned_hashes but NO blob data
  9. Block imported WITHOUT blobs ❌
```

**Impact**:
- Blobs stored but never used
- Execution layer cannot verify blob availability
- EIP-4844 non-compliant (silent data loss)

### The Solution

**Fixed Flow**:
```
Consensus Decision:
  5. mark_decided() promotes blobs ✅
  6. Decided handler retrieves block data ✅
  7. ✅ NEW: Call blob_engine.get_for_import(height)
  8. ✅ NEW: Verify blob count matches commitments
  9. ✅ NEW: Call EL with newPayloadV3(payload, versioned_hashes, blobs)
  10. Block imported WITH blobs ✅
```

---

## 📅 Implementation Plan (Days 1-7)

### **Day 1-2: Complete Phase 5 - Block Import** ⚠️ CRITICAL

**Estimated Time**: 1.5-2 days
**Priority**: HIGHEST - Blocks production deployment

#### Task 5.1: Update Decided Handler (4 hours)
**File**: `crates/node/src/app.rs`
**Lines**: ~414-523 (Decided handler)

**Changes Required**:
1. Add blob retrieval after decoding execution payload:
   ```rust
   // Retrieve stored blobs from blob engine
   let blobs = state.blob_engine.get_for_import(height).await
       .map_err(|e| eyre!("Failed to retrieve blobs for import: {}", e))?;

   debug!("Retrieved {} blobs for import at height {}", blobs.len(), height);
   ```

2. Verify blob count matches commitments:
   ```rust
   let expected_blob_count = block.body.blob_versioned_hashes_iter().count();
   if blobs.len() != expected_blob_count {
       return Err(eyre!("Blob count mismatch: stored {}, expected {}",
                        blobs.len(), expected_blob_count));
   }
   ```

3. Replace `notify_new_block()` with blob-aware call:
   ```rust
   let payload_status = execution_layer
       .new_payload_with_blobs(execution_payload, versioned_hashes, blobs)
       .await?;
   ```

**Testing**:
- Unit test: Mock blob_engine returns blobs
- Error test: Blob count mismatch rejected
- Error test: get_for_import failure aborts commit

---

#### Task 5.2: Implement Engine API v3 (8 hours)
**Files**:
- `crates/execution/src/engine_api/client.rs` (primary)
- `crates/execution/src/client.rs` (wrapper)

**Changes Required**:

**Step 1**: Add blob conversion utilities (1 hour)
```rust
// crates/execution/src/engine_api/client.rs

use ultramarine_types::proposal_part::BlobSidecar;

/// Convert BlobSidecar to Engine API v3 format
fn to_engine_blobs(sidecars: &[BlobSidecar]) -> Vec<String> {
    sidecars.iter()
        .map(|s| format!("0x{}", hex::encode(&s.blob.as_bytes())))
        .collect()
}

/// Convert versioned hashes to hex strings
fn to_hex_hashes(hashes: &[[u8; 32]]) -> Vec<String> {
    hashes.iter()
        .map(|h| format!("0x{}", hex::encode(h)))
        .collect()
}
```

**Step 2**: Implement `new_payload_v3()` (4 hours)
```rust
impl EngineApiClient {
    /// Submit block with blobs to execution layer (Engine API v3)
    ///
    /// # Arguments
    /// * `payload` - Execution payload (block)
    /// * `versioned_hashes` - KZG commitment hashes (versioned)
    /// * `blobs` - Blob sidecars with proofs
    ///
    /// # Returns
    /// PayloadStatus indicating validity
    pub async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<[u8; 32]>,
        blobs: Vec<BlobSidecar>,
    ) -> Result<PayloadStatus, ExecutionError> {
        // Verify capabilities
        let capabilities = self.check_capabilities().await?;
        if !capabilities.supports_engine_api_v3() {
            warn!("Execution client does not support Engine API v3, falling back to v2");
            return self.new_payload_v2(payload.into(), &[0u8; 32]).await;
        }

        // Convert to hex format for JSON-RPC
        let payload_json = payload.to_json();
        let versioned_hashes_hex = to_hex_hashes(&versioned_hashes);
        let blobs_hex = to_engine_blobs(&blobs);
        let commitments_hex = blobs.iter()
            .map(|s| format!("0x{}", hex::encode(&s.kzg_commitment.as_bytes())))
            .collect::<Vec<_>>();
        let proofs_hex = blobs.iter()
            .map(|s| format!("0x{}", hex::encode(&s.kzg_proof.as_bytes())))
            .collect::<Vec<_>>();

        // Call engine_newPayloadV3
        let params = json!([
            payload_json,
            versioned_hashes_hex,
            "0x0000000000000000000000000000000000000000000000000000000000000000", // parent_beacon_block_root (placeholder)
        ]);

        debug!("Calling engine_newPayloadV3 with {} blobs", blobs.len());

        let response: serde_json::Value = self
            .rpc_client
            .call("engine_newPayloadV3", params)
            .await
            .map_err(|e| ExecutionError::RpcError(e.to_string()))?;

        self.parse_payload_status(response)
    }

    fn parse_payload_status(
        &self,
        json: serde_json::Value,
    ) -> Result<PayloadStatus, ExecutionError> {
        let status = json["status"]
            .as_str()
            .ok_or(ExecutionError::InvalidResponse("Missing status field".into()))?;

        match status {
            "VALID" => Ok(PayloadStatus::Valid),
            "INVALID" => Ok(PayloadStatus::Invalid),
            "SYNCING" => Ok(PayloadStatus::Syncing),
            "ACCEPTED" => Ok(PayloadStatus::Accepted),
            _ => Err(ExecutionError::InvalidResponse(format!("Unknown status: {}", status))),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadStatus {
    Valid,
    Invalid,
    Syncing,
    Accepted,
}
```

**Step 3**: Update ExecutionClient wrapper (2 hours)
```rust
// crates/execution/src/client.rs

impl ExecutionClient {
    /// Import block with blobs (Engine API v3)
    pub async fn new_payload_with_blobs(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<[u8; 32]>,
        blobs: Vec<BlobSidecar>,
    ) -> Result<PayloadStatus, ExecutionError> {
        info!("Submitting block with {} blobs to execution layer", blobs.len());

        let result = self.engine_api
            .new_payload_v3(payload, versioned_hashes, blobs)
            .await?;

        match result {
            PayloadStatus::Valid => {
                info!("✅ Execution layer accepted block with blobs");
            }
            PayloadStatus::Invalid => {
                error!("❌ Execution layer rejected block (invalid)");
            }
            PayloadStatus::Syncing => {
                warn!("⚠️ Execution layer still syncing");
            }
            PayloadStatus::Accepted => {
                info!("✅ Execution layer accepted block (optimistic)");
            }
        }

        Ok(result)
    }
}
```

**Step 4**: Update capability detection (1 hour)
```rust
// crates/execution/src/engine_api/capabilities.rs

impl Capabilities {
    pub fn supports_engine_api_v3(&self) -> bool {
        self.methods.contains(&"engine_newPayloadV3".to_string())
    }
}
```

**Testing**:
- Unit test: Blob conversion to hex format
- Integration test: Call mock EL with blobs
- Error test: Fallback to v2 if v3 unavailable
- Error test: Invalid response handling

---

#### Task 5.3: Integration Testing (4 hours)
**File**: `crates/node/tests/blob_import_test.rs` (NEW)

```rust
#[tokio::test]
async fn test_blob_import_flow() {
    // 1. Setup mock EL that supports engine_newPayloadV3
    // 2. Create proposal with 6 blobs
    // 3. Simulate consensus decision
    // 4. Verify blobs retrieved from blob_engine
    // 5. Verify EL called with blobs
    // 6. Verify block imported successfully
}

#[tokio::test]
async fn test_blob_count_mismatch_rejected() {
    // 1. Store 6 blobs in blob_engine
    // 2. Modify block to expect 4 blobs
    // 3. Trigger Decided handler
    // 4. Verify import fails with error
}

#[tokio::test]
async fn test_missing_blobs_aborts_import() {
    // 1. Consensus decides on block
    // 2. get_for_import() returns error (blobs missing)
    // 3. Verify import aborted
    // 4. Verify error logged
}
```

**Acceptance Criteria**:
- ✅ Blobs retrieved from blob_engine during import
- ✅ Blobs passed to execution layer via newPayloadV3
- ✅ Blob count mismatch detected and rejected
- ✅ Missing blobs abort import
- ✅ All existing tests still pass

---

### **Day 3: Phase 6 - Blob Pruning**

**Estimated Time**: 6-8 hours
**Priority**: HIGH - Prevents disk exhaustion

#### Task 6.1: Pruning Service (4 hours)
**File**: `crates/node/src/pruning.rs` (NEW)

```rust
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info};
use ultramarine_blob_engine::BlobEngine;
use ultramarine_types::height::Height;

/// Configuration for blob pruning
#[derive(Clone, Debug)]
pub struct PruningConfig {
    /// Retain blobs for this many heights (default: 5 for development, 4096 for production)
    pub retention_heights: u64,

    /// How often to run pruning (in heights)
    pub pruning_interval_heights: u64,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            retention_heights: 5, // Keep last 5 heights (conservative for dev)
            pruning_interval_heights: 1, // Prune every height
        }
    }
}

impl PruningConfig {
    /// Production configuration (Ethereum mainnet settings)
    pub fn production() -> Self {
        Self {
            retention_heights: 4096 * 32, // 4096 epochs * 32 slots = ~18 days
            pruning_interval_heights: 100, // Prune every 100 heights
        }
    }
}

/// Background pruning service for blob sidecars
pub struct PruningService<E: BlobEngine> {
    blob_engine: E,
    config: PruningConfig,
    last_pruned_height: Option<Height>,
}

impl<E: BlobEngine> PruningService<E> {
    pub fn new(blob_engine: E, config: PruningConfig) -> Self {
        Self {
            blob_engine,
            config,
            last_pruned_height: None,
        }
    }

    /// Run pruning check for a given height
    ///
    /// Called from consensus commit() after successful block import
    pub async fn maybe_prune(&mut self, current_height: Height) -> Result<(), String> {
        // Check if we should prune based on interval
        let should_prune = match self.last_pruned_height {
            None => true, // First run
            Some(last) => {
                current_height.as_u64() >= last.as_u64() + self.config.pruning_interval_heights
            }
        };

        if !should_prune {
            return Ok(());
        }

        // Calculate pruning threshold
        let prune_before_height = if current_height.as_u64() > self.config.retention_heights {
            Height::new(current_height.as_u64() - self.config.retention_heights)
        } else {
            debug!("Not enough history to prune (current: {})", current_height);
            return Ok(());
        };

        // Prune old blobs
        info!("Pruning blobs before height {}", prune_before_height);

        match self.blob_engine.prune_archived_before(prune_before_height).await {
            Ok(count) => {
                info!("✅ Pruned {} blob sidecars before height {}", count, prune_before_height);
                self.last_pruned_height = Some(current_height);
                Ok(())
            }
            Err(e) => {
                error!("Failed to prune blobs: {}", e);
                Err(format!("Pruning failed: {}", e))
            }
        }
    }

    /// Get pruning statistics
    pub fn stats(&self) -> PruningStats {
        PruningStats {
            last_pruned_height: self.last_pruned_height,
            retention_heights: self.config.retention_heights,
            pruning_interval: self.config.pruning_interval_heights,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PruningStats {
    pub last_pruned_height: Option<Height>,
    pub retention_heights: u64,
    pub pruning_interval: u64,
}
```

#### Task 6.2: Integrate into Consensus Commit (2 hours)
**File**: `crates/consensus/src/state.rs`

**Add to State struct**:
```rust
pub struct State<E = BlobEngineImpl<RocksDbBlobStore>>
where
    E: BlobEngine,
{
    // ... existing fields ...

    /// Pruning service for blob cleanup
    pruning_service: PruningService<E>,
}
```

**Add to commit() method** (after successful mark_decided):
```rust
// Prune old blobs (existing code at ~347-365)
let retain_height = if certificate.height.as_u64() >= 5 {
    Height::new(certificate.height.as_u64() - 5)
} else {
    Height::new(0)
};

// NEW: Use pruning service instead of direct call
if let Err(e) = self.pruning_service.maybe_prune(certificate.height).await {
    error!(
        height = %certificate.height,
        error = %e,
        "Blob pruning failed (non-fatal)"
    );
    // Don't fail commit - pruning is best-effort
}
```

#### Task 6.3: Configuration (1 hour)
**File**: `crates/node/src/config.rs`

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct NodeConfig {
    // ... existing fields ...

    /// Blob pruning configuration
    #[serde(default)]
    pub blob_pruning: PruningConfig,
}
```

#### Task 6.4: Testing (2 hours)
**File**: `crates/node/tests/pruning_test.rs` (NEW)

```rust
#[tokio::test]
async fn test_pruning_service() {
    // 1. Create blob_engine with test data
    // 2. Create pruning service with retention=5
    // 3. Advance to height 10
    // 4. Trigger pruning
    // 5. Verify blobs before height 5 deleted
    // 6. Verify blobs 5-10 still exist
}

#[tokio::test]
async fn test_pruning_respects_interval() {
    // 1. Set pruning_interval_heights=5
    // 2. Call maybe_prune at heights 1,2,3,4,5,6
    // 3. Verify pruning only happens at heights 5, 10, etc.
}
```

**Acceptance Criteria**:
- ✅ Pruning service integrated into State
- ✅ Old blobs deleted after retention period
- ✅ Pruning interval respected
- ✅ Pruning failures don't block commits
- ✅ Configuration via config file

---

### **Day 4-5: Phase 8 - Integration Testing**

**Estimated Time**: 1.5-2 days
**Priority**: HIGH - Required for production

#### Task 8.1: End-to-End Blob Flow Test (6 hours)
**File**: `crates/node/tests/integration/blob_e2e_test.rs` (NEW)

```rust
//! End-to-end test for blob sidecar integration
//!
//! Tests the complete flow:
//! 1. Proposer requests block from EL with blobs
//! 2. Proposer streams proposal with blobs
//! 3. Validators receive and verify blobs
//! 4. Consensus decides on block
//! 5. Blobs promoted to decided state
//! 6. Blobs passed to EL during import
//! 7. Old blobs pruned

use ultramarine_node::*;
use ultramarine_types::*;

#[tokio::test]
async fn test_full_blob_lifecycle() {
    // Setup
    let (mut node, mock_el) = setup_test_node_with_mock_el().await;

    // Configure mock EL to return 6 blobs
    mock_el.configure_blob_count(6);

    // Step 1: GetValue - proposer requests block with blobs
    let height = Height::new(1);
    let round = Round::new(0);

    let proposal = node.get_value(height, round).await.unwrap();
    assert_eq!(proposal.blob_count(), 6);

    // Step 2: Stream proposal
    let parts = node.stream_proposal(proposal).await;
    assert_eq!(parts.len(), 8); // Init + 6 BlobSidecar + Fin

    // Step 3: Validators receive and verify
    for part in parts {
        node.receive_proposal_part(part).await.unwrap();
    }

    // Verify blobs stored in undecided state
    let undecided = node.blob_engine.get_undecided(height, round).await.unwrap();
    assert_eq!(undecided.len(), 6);

    // Step 4: Consensus decision
    let certificate = simulate_consensus_decision(height, round);

    // Step 5: Decided handler promotes blobs
    node.handle_decided(certificate).await.unwrap();

    // Verify blobs moved to decided state
    let decided = node.blob_engine.get_for_import(height).await.unwrap();
    assert_eq!(decided.len(), 6);

    // Step 6: Verify EL received blobs
    assert!(mock_el.received_new_payload_v3_with_blobs());
    let el_blobs = mock_el.get_last_blobs();
    assert_eq!(el_blobs.len(), 6);

    // Step 7: Advance to height 10 and verify pruning
    for h in 2..=10 {
        node.advance_height(Height::new(h)).await.unwrap();
    }

    // Blobs from height 1 should be pruned (retention=5)
    let result = node.blob_engine.get_for_import(Height::new(1)).await;
    assert!(result.is_err() || result.unwrap().is_empty());
}

#[tokio::test]
async fn test_blob_verification_failure_rejects_proposal() {
    let (mut node, mock_el) = setup_test_node_with_mock_el().await;

    // Create proposal with INVALID KZG proofs
    let mut proposal = create_proposal_with_blobs(6);
    proposal.blobs[0].kzg_proof = KzgProof([0xff; 48]); // Invalid proof

    // Attempt to verify and store
    let result = node.blob_engine.verify_and_store(
        proposal.height,
        proposal.round,
        &proposal.blobs
    ).await;

    // Verification should fail
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), BlobEngineError::VerificationFailed { .. }));

    // Proposal should be rejected
    let proposal_value = node.receive_proposal_part(proposal).await.unwrap();
    assert!(proposal_value.is_none()); // Rejected due to verification failure
}

#[tokio::test]
async fn test_blob_count_mismatch_aborts_import() {
    let (mut node, _) = setup_test_node_with_mock_el().await;

    // Create proposal with 6 blobs
    let proposal = create_proposal_with_blobs(6);
    node.receive_proposal(proposal).await.unwrap();

    // Simulate consensus decision
    let certificate = simulate_consensus_decision(Height::new(1), Round::new(0));

    // Manually delete 2 blobs from storage (simulate corruption)
    node.blob_engine.delete_blob(Height::new(1), 4).await.unwrap();
    node.blob_engine.delete_blob(Height::new(1), 5).await.unwrap();

    // Decided handler should fail due to blob count mismatch
    let result = node.handle_decided(certificate).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Blob count mismatch"));
}

#[tokio::test]
async fn test_orphaned_blob_cleanup() {
    let (mut node, _) = setup_test_node_with_mock_el().await;

    let height = Height::new(1);

    // Receive proposal for round 0 with 6 blobs
    let proposal_r0 = create_proposal_with_blobs_at_round(height, Round::new(0), 6);
    node.receive_proposal(proposal_r0).await.unwrap();

    // Receive proposal for round 1 with 4 blobs (timeout happened)
    let proposal_r1 = create_proposal_with_blobs_at_round(height, Round::new(1), 4);
    node.receive_proposal(proposal_r1).await.unwrap();

    // Verify both stored in undecided state
    let r0_blobs = node.blob_engine.get_undecided(height, Round::new(0)).await.unwrap();
    assert_eq!(r0_blobs.len(), 6);
    let r1_blobs = node.blob_engine.get_undecided(height, Round::new(1)).await.unwrap();
    assert_eq!(r1_blobs.len(), 4);

    // Consensus decides on round 1
    let certificate = simulate_consensus_decision(height, Round::new(1));
    node.handle_decided(certificate).await.unwrap();

    // Verify round 0 blobs cleaned up (orphaned)
    let r0_after = node.blob_engine.get_undecided(height, Round::new(0)).await;
    assert!(r0_after.is_err() || r0_after.unwrap().is_empty());

    // Verify round 1 blobs promoted to decided
    let decided = node.blob_engine.get_for_import(height).await.unwrap();
    assert_eq!(decided.len(), 4);
}

// Helper functions
async fn setup_test_node_with_mock_el() -> (TestNode, MockExecutionLayer) {
    // Setup test node with blob_engine and mock EL
}

fn create_proposal_with_blobs(count: usize) -> Proposal {
    // Create test proposal with N blobs
}

fn simulate_consensus_decision(height: Height, round: Round) -> Certificate {
    // Create mock certificate
}
```

#### Task 8.2: Performance Benchmarks (4 hours)
**File**: `crates/blob_engine/benches/kzg_verification.rs` (NEW)

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use ultramarine_blob_engine::*;

fn bench_kzg_verification(c: &mut Criterion) {
    let mut group = c.benchmark_group("kzg_verification");

    for blob_count in [1, 3, 6] {
        group.bench_with_input(
            BenchmarkId::new("batch_verify", blob_count),
            &blob_count,
            |b, &count| {
                let blobs = create_test_blobs(count);
                let verifier = BlobVerifier::new().unwrap();

                b.iter(|| {
                    verifier.verify_blob_sidecars_batch(black_box(&blobs))
                });
            },
        );
    }

    group.finish();
}

fn bench_blob_storage(c: &mut Criterion) {
    let mut group = c.benchmark_group("blob_storage");

    for blob_count in [1, 3, 6] {
        group.bench_with_input(
            BenchmarkId::new("rocksdb_write", blob_count),
            &blob_count,
            |b, &count| {
                let blobs = create_test_blobs(count);
                let store = create_test_store();

                b.iter(|| async {
                    store.put_undecided_blobs(
                        black_box(Height::new(1)),
                        black_box(0),
                        black_box(&blobs)
                    ).await
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_kzg_verification, bench_blob_storage);
criterion_main!(benches);
```

**Acceptance Criteria**:
- ✅ Full lifecycle test passes
- ✅ Verification failure test passes
- ✅ Blob count mismatch test passes
- ✅ Orphaned blob cleanup test passes
- ✅ Benchmarks show acceptable performance:
  - KZG verification: < 100ms for 6 blobs
  - Storage write: < 50ms for 6 blobs
  - Storage read: < 20ms for 6 blobs

---

### **Day 6: Security Hardening & Monitoring**

**Estimated Time**: 6-8 hours
**Priority**: MEDIUM-HIGH

#### Task: Trusted Setup Verification (2 hours)
**File**: `crates/blob_engine/src/verifier.rs`

```rust
// Add at top of file
const MAINNET_TRUSTED_SETUP_HASH: &str =
    "0x1234567890abcdef..."; // TODO: Get official hash from Ethereum Foundation

impl BlobVerifier {
    pub fn new() -> Result<Self, BlobVerificationError> {
        let trusted_setup = include_str!("trusted_setup.json");

        // Verify hash matches Ethereum mainnet
        let hash = sha256(trusted_setup.as_bytes());
        let hash_hex = hex::encode(hash);

        if hash_hex != MAINNET_TRUSTED_SETUP_HASH {
            return Err(BlobVerificationError::TrustedSetupLoad(
                format!(
                    "Trusted setup hash mismatch: expected {}, got {}",
                    MAINNET_TRUSTED_SETUP_HASH,
                    hash_hex
                )
            ));
        }

        info!("✅ Trusted setup verified (hash: {})", &hash_hex[..16]);

        // ... rest of existing code ...
    }
}
```

#### Task: Add Monitoring Metrics (4 hours)
**File**: `crates/blob_engine/src/metrics.rs` (NEW)

```rust
use prometheus::{IntCounter, IntGauge, Histogram, register_int_counter, register_int_gauge, register_histogram};

lazy_static! {
    pub static ref BLOB_VERIFICATION_FAILURES: IntCounter = register_int_counter!(
        "blob_verification_failures_total",
        "Total number of blob verification failures"
    ).unwrap();

    pub static ref BLOB_VERIFICATION_DURATION: Histogram = register_histogram!(
        "blob_verification_duration_seconds",
        "Duration of blob verification operations"
    ).unwrap();

    pub static ref BLOB_STORAGE_BYTES: IntGauge = register_int_gauge!(
        "blob_storage_bytes",
        "Total bytes stored in blob storage"
    ).unwrap();

    pub static ref BLOB_UNDECIDED_COUNT: IntGauge = register_int_gauge!(
        "blob_undecided_count",
        "Number of blobs in undecided state"
    ).unwrap();

    pub static ref BLOB_DECIDED_COUNT: IntGauge = register_int_gauge!(
        "blob_decided_count",
        "Number of blobs in decided state"
    ).unwrap();

    pub static ref BLOB_PRUNE_COUNT: IntCounter = register_int_counter!(
        "blob_prune_count_total",
        "Total number of blobs pruned"
    ).unwrap();
}
```

**Add to engine.rs**:
```rust
use crate::metrics::*;

impl BlobEngine for BlobEngineImpl {
    async fn verify_and_store(...) -> Result<...> {
        let timer = BLOB_VERIFICATION_DURATION.start_timer();

        let result = self.verifier.verify_blob_sidecars_batch(&sidecar_refs);

        if result.is_err() {
            BLOB_VERIFICATION_FAILURES.inc();
        }

        timer.observe_duration();

        // ... rest of implementation ...
    }
}
```

---

### **Day 7: Documentation & Final Review**

**Estimated Time**: 4-6 hours
**Priority**: MEDIUM

#### Task: Update Documentation
1. Update `BLOB_INTEGRATION_STATUS.md` with Phase 5 completion
2. Update `SESSION_SUMMARY.md` with all changes
3. Update `REVIEW_REPORT.md` with new review areas
4. Create `OPERATOR_GUIDE.md` with configuration options
5. Create `TROUBLESHOOTING.md` with common issues

#### Task: Final Testing
1. Run all unit tests: `cargo test --all`
2. Run integration tests: `cargo test --test blob_e2e`
3. Run benchmarks: `cargo bench -p ultramarine-blob-engine`
4. Manual testing with devnet

---

## 📊 Success Criteria

### Phase 5 Complete When:
- ✅ Blobs retrieved from blob_engine in Decided handler
- ✅ Blobs passed to execution layer via newPayloadV3
- ✅ Blob count verification before import
- ✅ Integration tests pass
- ✅ All existing tests still pass

### Phase 6 Complete When:
- ✅ Pruning service integrated
- ✅ Old blobs automatically deleted
- ✅ Configurable retention period
- ✅ Pruning metrics exported

### Phase 8 Complete When:
- ✅ End-to-end lifecycle test passes
- ✅ Failure mode tests pass
- ✅ Performance benchmarks meet targets
- ✅ Network simulation tests pass

### Production Ready When:
- ✅ All phases complete
- ✅ Security review passed
- ✅ Monitoring deployed
- ✅ Documentation complete
- ✅ Devnet testing successful

---

## 🚀 Deployment Checklist

**Before deploying to production**:

1. **Code Review**:
   - [ ] Phase 5 changes reviewed
   - [ ] Phase 6 pruning logic reviewed
   - [ ] Security review completed
   - [ ] All tests passing

2. **Configuration**:
   - [ ] Pruning retention period configured
   - [ ] Blob storage path configured
   - [ ] Monitoring endpoints configured

3. **Testing**:
   - [ ] All unit tests pass
   - [ ] All integration tests pass
   - [ ] Performance benchmarks acceptable
   - [ ] Devnet testing successful (1 week minimum)

4. **Monitoring**:
   - [ ] Prometheus metrics exposed
   - [ ] Grafana dashboards created
   - [ ] Alerts configured:
     - Blob verification failures > 10/hour
     - Blob storage > 80% capacity
     - Pruning failures > 5/day

5. **Documentation**:
   - [ ] Operator guide published
   - [ ] Troubleshooting guide published
   - [ ] API documentation updated
   - [ ] Release notes written

6. **Rollout Plan**:
   - [ ] Deploy to devnet (Day 1)
   - [ ] Monitor for 3 days
   - [ ] Deploy to testnet (Day 4)
   - [ ] Monitor for 1 week
   - [ ] Deploy to mainnet (Day 11+)

---

## 📞 Support & Questions

**For implementation questions**: See individual task descriptions above

**For architectural questions**: Review `docs/BLOB_INTEGRATION_STATUS.md`

**For code review**: See `docs/REVIEW_REPORT.md`

**For troubleshooting**: See `docs/TROUBLESHOOTING.md` (to be created)

---

**Timeline Summary**:
- Day 1-2: Complete Phase 5 (EL integration)
- Day 3: Implement Phase 6 (Pruning)
- Day 4-5: Phase 8 (Integration tests)
- Day 6: Security & Monitoring
- Day 7: Documentation & Review

**Total Estimated Time**: 5-7 days of focused work

**Current Progress**: 60% complete (Phases 1-4 + partial Phase 5)

**Critical Path**: Phase 5 → Phase 6 → Phase 8 → Production

---

*Last Updated: 2025-10-21*
*Next Review: After Phase 5 completion*
