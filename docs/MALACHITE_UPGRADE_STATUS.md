# Malachite Upgrade Status - v0.1 to Latest (b205f425)

**Date**: 2025-10-22
**Target Version**: b205f4252f3064d9a74716056f63834ff33f2de9
**Status**: Partial - Breaking Changes Require Extensive Refactoring

---

## Summary

Upgrading ultramarine to the latest malachite involves several breaking API changes. Some have been fixed, but others require significant refactoring of the CLI and configuration management.

---

## ✅ COMPLETED FIXES

### 1. ProcessSyncedValue Reply Type (DONE)
**Status**: ✅ All 6 changes complete
**Files**: `crates/node/src/app.rs`

```rust
// OLD API (commit 0968a34)
reply: Reply<ProposedValue<Ctx>>

// NEW API (commit b205f425)
reply: Reply<Option<ProposedValue<Ctx>>>
```

**Changes Made**:
- Line 602: Decode failure → `reply.send(None)`
- Line 624: Store payload failure → `reply.send(None)`
- Line 640: Blob verification failure → `reply.send(None)`
- Line 668: Store proposal failure → `reply.send(None)`
- Line 675: Success path → `reply.send(Some(proposed_value))`
- Line 707: MetadataOnly rejection → `reply.send(None)`

---

### 2. Context Trait Method Signatures (DONE)
**Status**: ✅ Complete
**File**: `crates/types/src/context.rs`

```rust
// OLD
fn new_proposal(height: Height, ...) -> Proposal
fn new_prevote(height: Height, ...) -> Vote
fn new_precommit(height: Height, ...) -> Vote

// NEW - requires &self
fn new_proposal(&self, height: Height, ...) -> Proposal
fn new_prevote(&self, height: Height, ...) -> Vote
fn new_precommit(&self, height: Height, ...) -> Vote
```

**Changes Made**: Added `&self` to all three methods (lines 51, 62, 72)

---

## ❌ REMAINING BREAKING CHANGES

### 3. Node Module Restructure
**Status**: ❌ Requires refactoring
**Impact**: CLI crate (6 files)

**Problem**: `malachitebft_app::Node` moved to `malachitebft_app::node::Node`

**Affected Files**:
```
crates/cli/src/file.rs:5
crates/cli/src/new.rs:5
crates/cli/src/cmd/init.rs:6
crates/cli/src/cmd/testnet.rs:7
crates/cli/src/cmd/distributed_testnet.rs:8
crates/cli/src/cmd/start.rs:5
```

**Quick Fix**:
```rust
// OLD
use malachitebft_app::Node;

// NEW
use malachitebft_app::node::Node;
```

---

### 4. Config Type Removed
**Status**: ❌ Requires major refactoring
**Impact**: CLI crate configuration management

**Problem**: `malachitebft_config::Config` no longer exists. It was split into:
- `ConsensusConfig`
- `ValueSyncConfig`
- `MempoolConfig`
- `P2pConfig`
- `RuntimeConfig`
- `MetricsConfig`
- `LoggingConfig`
- `TestConfig`

**Affected Files**:
```
crates/cli/src/file.rs:6,11
crates/cli/src/new.rs:67,72
crates/cli/src/cmd/init.rs:8
crates/cli/src/cmd/distributed_testnet.rs:208,213,280
```

**Example from file.rs**:
```rust
// OLD
pub fn save_config(config_file: &Path, config: &Config) -> Result<(), Error>

// NEW - Need to decide which config type(s) to use
// Option A: Use ConsensusConfig
pub fn save_config(config_file: &Path, config: &ConsensusConfig) -> Result<(), Error>

// Option B: Create a unified config struct
pub struct UltramarineConfig {
    pub consensus: ConsensusConfig,
    pub value_sync: ValueSyncConfig,
    pub metrics: MetricsConfig,
    // ...
}
```

**Decision Needed**:
- Does ultramarine need a unified config?
- Should it use malachite's individual config types directly?
- How should config serialization/deserialization work?

---

### 5. SigningProvider Import
**Status**: ❌ Needs investigation
**Impact**: `crates/types/src/signing.rs:4`

**Problem**: `malachitebft_core_types::SigningProvider` not found

**Possible Solutions**:
- Check if moved to different module
- Check if renamed
- May now be in `malachitebft_signing` crate instead

---

### 6. VoteSet and AggregatedSignature
**Status**: ❌ Needs investigation
**Impact**: `crates/types/src/codec/proto/mod.rs:6`

**Problem**: These types removed or moved:
- `malachitebft_core_types::AggregatedSignature`
- `malachitebft_core_types::VoteSet`
- `sync::VoteSetRequest`
- `sync::VoteSetResponse`

**Needs**: Check malachite codebase for where these moved or if they were removed entirely.

---

## Compilation Errors Summary

```
Total Errors: 19
- Context trait methods: 3 (FIXED ✅)
- Node imports: 6 (Easy fix)
- Config type: 5 (Requires refactoring)
- SigningProvider: 1 (Needs investigation)
- VoteSet/AggregatedSignature: 2 (Needs investigation)
- VoteSetRequest/Response: 2 (Needs investigation)
```

---

## Recommended Next Steps

### Phase 1: Quick Fixes (1-2 hours)
1. ✅ ~~ProcessSyncedValue reply type~~ (DONE)
2. ✅ ~~Context trait &self~~ (DONE)
3. Update Node imports (6 files - simple search/replace)
   ```bash
   sed -i '' 's/use malachitebft_app::Node/use malachitebft_app::node::Node/g' crates/cli/src/**/*.rs
   ```

### Phase 2: Investigation (2-4 hours)
4. Research where SigningProvider moved
5. Research VoteSet/AggregatedSignature changes
6. Check if VoteSetRequest/Response still needed

### Phase 3: Major Refactoring (8-16 hours)
7. Design ultramarine config architecture
   - Create UltramarineConfig wrapper?
   - Use individual configs directly?
   - How to handle CLI config file format changes?
8. Update all config usage in CLI
9. Update config serialization/deserialization
10. Update tests

### Phase 4: Testing
11. Build verification
12. Integration tests
13. Sync protocol tests

---

## Risk Assessment

**Low Risk (Already Fixed)**:
- ✅ ProcessSyncedValue protocol
- ✅ Context trait signatures

**Medium Risk (Easy to fix)**:
- Node import paths (mechanical change)

**High Risk (Requires design decisions)**:
- Config architecture
  - Breaking change for existing config files
  - Need migration strategy
  - CLI UX changes

**Unknown Risk (Needs investigation)**:
- SigningProvider location
- VoteSet/AggregatedSignature removal/move
- May uncover additional breaking changes

---

## Questions for Team

1. **Config Strategy**: Should ultramarine create its own unified config type, or use malachite's individual configs?

2. **Breaking Changes**: Is it acceptable to break existing config files for this upgrade?

3. **CLI Priority**: How critical is CLI functionality? Can it be temporarily disabled while we fix core functionality?

4. **VoteSet Usage**: Is `VoteSet` still needed? What functionality did it provide?

5. **Migration Timeline**: What's the timeline for completing this upgrade?

---

## Alternative Approach

If full upgrade is too risky right now, consider:

1. **Hybrid Approach**:
   - Keep using malachite `0968a34` via git dependency
   - Only apply the ProcessSyncedValue fixes if merging latest malachite locally
   - Schedule full upgrade for next major release

2. **Incremental Upgrade**:
   - Fix core functionality first (node crate)
   - Temporarily disable CLI features
   - Upgrade CLI separately in follow-up PR

3. **Fork Strategy**:
   - Fork malachite at `0968a34`
   - Cherry-pick specific fixes needed
   - Full upgrade when ready

---

## References

**Malachite Commits**:
- Old: `0968a34ba747130467569b1d10b2b1ef18f4b69b`
- New: `b205f4252f3064d9a74716056f63834ff33f2de9`

**Key Breaking Commit**:
- `bbb5fbe` - "feat(code)!: Decouple `Host` messages from `Consensus` actor"

**Documentation**:
- Malachite Changelog: (if available)
- Migration Guide: (if available)

---

**Status**: Awaiting decision on config architecture and priority/timeline
