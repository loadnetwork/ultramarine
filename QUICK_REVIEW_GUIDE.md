# Quick Review Guide - Blob Sidecar Integration

**Last Updated**: 2025-10-21
**Reviewer**: Start here!

---

## 📋 TL;DR - What Changed

- ✅ Created new `blob_engine` crate with KZG verification
- ✅ Integrated blob storage into consensus layer
- ✅ ~1,500 lines of new code, all tests passing
- ❌ Integration tests needed before production

---

## 🎯 Review Priority Order

### 1. Start Here (Critical Paths - 2 hours)

**Security Critical** - Review in this order:

```bash
# 1. KZG Verification (30 min)
vim crates/blob_engine/src/verifier.rs
# Focus: Lines 155-310 (verification methods)
# Question: Is c-kzg API usage correct?

# 2. Consensus Integration (45 min)
vim crates/consensus/src/state.rs
# Focus: Lines 252-356 (assemble_and_store_blobs)
# Question: Can verification be bypassed?

# 3. Storage Backend (30 min)
vim crates/blob_engine/src/store/rocksdb.rs
# Focus: Lines 100-300 (async methods)
# Question: Are keys collision-free?

# 4. Commit Lifecycle (15 min)
vim crates/consensus/src/state.rs
# Focus: Lines 274-323 (mark_decided + pruning)
# Question: Is graceful degradation acceptable?
```

### 2. Supporting Code (Medium Priority - 1 hour)

```bash
# 5. Orchestration layer
vim crates/blob_engine/src/engine.rs

# 6. Error types
vim crates/blob_engine/src/error.rs

# 7. Node initialization
vim crates/node/src/node.rs
```

### 3. Tests & Documentation (Low Priority - 1 hour)

```bash
# 8. Tests
cargo test -p ultramarine-blob-engine --verbose

# 9. Documentation
cat docs/REVIEW_REPORT.md
cat docs/BLOB_INTEGRATION_STATUS.md
```

---

## 🔍 Critical Review Questions

### Must Answer Before Approval

1. **Security**: Is KZG verification implemented correctly?
   - [ ] Check c-kzg API usage in `verifier.rs:220-310`
   - [ ] Verify trusted setup matches Ethereum mainnet
   - [ ] Confirm batch verification logic is sound

2. **Consensus Safety**: Can blob failures stall consensus?
   - [ ] Review error handling in `state.rs:commit()`
   - [ ] Check if blob marking failure should block commit
   - [ ] Verify pruning doesn't delete active blobs

3. **Storage Integrity**: Is RocksDB usage correct?
   - [ ] Verify key encoding prevents collisions
   - [ ] Check bincode deserialization is safe
   - [ ] Confirm async wrapping is correct

4. **Architecture**: Is the design sound?
   - [ ] Review BlobEngine trait abstraction
   - [ ] Check if generic parameter is justified
   - [ ] Verify error types are comprehensive

---

## 🚨 Known Issues & Trade-offs

### Design Decisions (Review Needed)

**1. Graceful Degradation in Commit**
```rust
// consensus/src/state.rs:276-284
if let Err(e) = self.blob_engine.mark_decided(height, round).await {
    error!("Failed to mark blobs as decided");
    // DOES NOT FAIL COMMIT - just logs error
}
```
**Question**: Should blob operation failures block consensus?
- Current: No - prioritizes liveness
- Alternative: Yes - prioritizes blob availability

**2. Trusted Setup Validation**
```rust
// blob_engine/src/verifier.rs:163-175
let trusted_setup = include_str!("trusted_setup.json");
// NO HASH VERIFICATION
```
**Question**: Should we verify hash on startup?
- Current: Embedded, no validation
- Alternative: Verify hash or support external file

**3. Storage Path Hardcoded**
```rust
// node/src/node.rs:180
let blob_store = RocksDbBlobStore::open(
    self.get_home_dir().join("blob_store.db")  // HARDCODED
)?;
```
**Question**: Should this be configurable?

---

## ✅ Quick Test Checklist

Run these to verify basic functionality:

```bash
# 1. Build check
cargo check --all
# Expected: Success with warnings (no errors)

# 2. Unit tests
cargo test -p ultramarine-blob-engine
# Expected: 9/9 passing

# 3. Existing tests still pass
cargo test -p ultramarine-consensus
# Expected: All passing

# 4. Integration test (TODO - doesn't exist yet!)
# cargo test -p ultramarine-integration --test blob_e2e
# Expected: MISSING - needs to be written
```

---

## 📊 Files Changed Summary

### New Files (8)

```
crates/blob_engine/
├── Cargo.toml                     ← Dependencies
├── src/lib.rs                     ← 70 lines
├── src/error.rs                   ← 90 lines
├── src/engine.rs                  ← 200 lines ⚠️ CRITICAL
├── src/verifier.rs                ← 150 lines 🔒 SECURITY
├── src/store/mod.rs               ← 100 lines
├── src/store/rocksdb.rs           ← 568 lines ⚠️ CRITICAL
└── src/trusted_setup.json         ← 4,324 lines
```

### Modified Files (6)

```
crates/consensus/src/state.rs      ← ~200 lines changed ⚠️ CRITICAL
crates/node/src/node.rs            ← ~10 lines changed
crates/consensus/Cargo.toml        ← +1 dependency
crates/node/Cargo.toml             ← +1 dependency
Cargo.toml                         ← +2 workspace entries
docs/FINAL_PLAN.md                 ← Status updates
```

### Documentation (3 new)

```
docs/BLOB_INTEGRATION_STATUS.md    ← 500+ lines (comprehensive)
docs/REVIEW_REPORT.md              ← 600+ lines (review guide)
docs/SESSION_SUMMARY.md            ← 200+ lines (quick summary)
```

---

## 🔐 Security Review Checklist

Copy this and check off as you review:

```markdown
## KZG Verification
- [ ] c-kzg API usage is correct (verifier.rs:220-310)
- [ ] Trusted setup matches Ethereum mainnet hash
- [ ] Batch verification logic is sound
- [ ] Error handling prevents verification bypass
- [ ] Test with invalid proofs (manual testing)

## Storage Security
- [ ] Key encoding prevents collisions (rocksdb.rs:70-98)
- [ ] Bincode deserialization is safe
- [ ] Column family isolation works correctly
- [ ] No SQL injection equivalent possible
- [ ] Concurrent access is safe

## Consensus Integration
- [ ] Proposal rejection path is correct (state.rs:196-207)
- [ ] Commitments match metadata (state.rs:312-315)
- [ ] No race conditions in mark_decided
- [ ] Pruning doesn't delete active blobs
- [ ] Error propagation is correct

## Attack Vectors
- [ ] Invalid KZG proof → Rejected
- [ ] Blob storage exhaustion → Mitigated by pruning
- [ ] Commitment mismatch → Detected
- [ ] RocksDB corruption → Error handling?
- [ ] Race conditions → Review needed
```

---

## 🐛 Potential Bugs to Check

1. **Race Condition**: `mark_decided()` + `get_for_import()` concurrent
   - Location: `blob_engine/src/engine.rs`
   - Test: Call both simultaneously for same height

2. **Off-by-One**: Pruning boundary
   - Location: `consensus/src/state.rs:301`
   - Question: Should we keep height N-5 or prune it?

3. **Key Collision**: BlobKey encoding
   - Location: `blob_engine/src/store/rocksdb.rs:70-98`
   - Verify: No overlap between undecided and decided keys

4. **Error String Loss**: Type information lost
   - Location: `consensus/src/state.rs:255`
   - Impact: Harder to debug failures

---

## ⚡ Performance Questions

1. **KZG Latency**: Does 10-50ms block proposal handling?
   - Measure: Add timing metrics
   - Accept: If < 100ms for 6 blobs

2. **Storage Growth**: 786KB per block acceptable?
   - Calculate: Retention × block rate
   - Monitor: Disk usage over time

3. **Pruning I/O**: Blocking during delete operations?
   - Measure: Time to prune 100 blobs
   - Consider: Rate limiting if > 1 second

---

## 📝 Review Sign-off Template

Copy this into your review comments:

```markdown
## Code Review - Blob Sidecar Integration

**Reviewer**: [Your Name]
**Date**: [Date]
**Commit**: [Git SHA]

### Critical Paths Reviewed
- [ ] KZG Verification (verifier.rs)
- [ ] Consensus Integration (state.rs)
- [ ] Storage Backend (rocksdb.rs)
- [ ] Commit Lifecycle (state.rs)

### Security Assessment
- [ ] No verification bypass possible
- [ ] Storage is secure
- [ ] Error handling is comprehensive
- [ ] Attack vectors mitigated

### Concerns/Questions
[List any concerns or questions]

### Recommendations
[List recommendations before merge]

### Approval Status
- [ ] ✅ Approved - ready to merge
- [ ] ⚠️ Approved with changes - minor fixes needed
- [ ] ❌ Changes requested - major issues found
- [ ] 🔄 Needs discussion - architectural questions

**Overall**: [Approved/Conditional/Rejected]
```

---

## 🚀 Next Steps After Review

### If Approved

1. **Write Integration Tests** (1-2 days)
   ```bash
   # Create test file
   touch crates/consensus/tests/blob_integration_test.rs

   # Test full flow:
   # - Generate block with blobs
   # - Propose and stream
   # - Verify and store
   # - Decide and commit
   # - Prune old blobs
   ```

2. **Performance Benchmarks** (1 day)
   ```bash
   # Add criterion benchmark
   cargo bench -p ultramarine-blob-engine

   # Measure:
   # - KZG verification throughput
   # - Storage I/O patterns
   # - Memory usage under load
   ```

3. **Add Monitoring** (0.5 days)
   ```rust
   // Add metrics
   - blob_verification_failures_total
   - blob_verification_duration_seconds
   - blob_storage_bytes
   - blob_prune_count_total
   ```

4. **Deploy to Testnet** (1 day)

### If Changes Requested

1. Address feedback
2. Re-run tests
3. Request re-review
4. Repeat until approved

---

## 📞 Get Help

**Questions About**:
- Architecture → Read `BLOB_INTEGRATION_STATUS.md`
- Specific code → Read `REVIEW_REPORT.md`
- Quick overview → Read `SESSION_SUMMARY.md`

**Stuck on Review**?
- Focus on critical paths first (security > consensus > storage)
- Use checklist above to stay organized
- Flag major concerns immediately
- Minor issues can be post-merge fixes

---

## 🎯 Review Time Estimate

- **Quick review** (critical paths only): 2 hours
- **Thorough review** (all code + tests): 4-6 hours
- **Deep security review** (with testing): 1-2 days

**Recommended**: Start with 2-hour quick review, then decide if deep dive needed.

---

**Ready to start?** Begin with `crates/blob_engine/src/verifier.rs` 🚀

**Questions?** Check `docs/REVIEW_REPORT.md` for detailed guidance.

**Approved?** Move on to integration testing!

---

*Quick Guide Version 1.0 - 2025-10-21*
