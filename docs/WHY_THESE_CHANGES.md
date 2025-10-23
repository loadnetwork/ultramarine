# Why These Changes Were Made

## Overview

This document explains the rationale behind the three major categories of changes made during the malachite b205f4252f3064d9a74716056f63834ff33f2de9 upgrade.

---

## 1. Why LivenessMsg Was Added

### The Problem

When upgrading to malachite b205f425, the build failed with errors like:
```
error: the trait bound `ProtobufCodec: Codec<LivenessMsg<LoadContext>>` is not satisfied
```

### Root Cause

Malachite introduced a new **ConsensusCodec** trait that requires implementations for **4 message types**:

**From** `malachite/code/crates/engine/src/consensus.rs:51-57`:
```rust
pub trait ConsensusCodec<Ctx>
where
    Ctx: Context,
    Self: codec::Codec<Ctx::ProposalPart>,              // ✅ We had this
    Self: codec::Codec<SignedConsensusMsg<Ctx>>,        // ✅ We had this
    Self: codec::Codec<LivenessMsg<Ctx>>,               // ❌ MISSING!
    Self: codec::Codec<StreamMessage<Ctx::ProposalPart>>, // ✅ We had this
{
}
```

**Before the upgrade**: Ultramarine only implemented 3 of the 4 required codecs.

**After the upgrade**: Malachite's `start_engine` function requires a codec that implements `ConsensusCodec`, which **must** include `LivenessMsg`.

### What is LivenessMsg?

**From** `malachite/code/crates/core-consensus/src/types.rs:131-135`:
```rust
pub enum LivenessMsg<Ctx: Context> {
    Vote(SignedVote<Ctx>),
    PolkaCertificate(PolkaCertificate<Ctx>),
    SkipRoundCertificate(RoundCertificate<Ctx>),
}
```

**Purpose**: LivenessMsg is used for **consensus liveness mechanisms**:

1. **Vote**: Regular prevote/precommit messages
2. **PolkaCertificate**: Proof that 2f+1 validators prevoted for a value
   - Allows nodes to know a value is "locked" by the network
   - Helps nodes that are behind catch up safely
3. **SkipRoundCertificate**: Proof that 2f+1 validators want to skip a round
   - Used for **fast round-skipping** when proposer is unresponsive
   - Improves consensus performance in fault scenarios

**Why this is important**: Without LivenessMsg codec, consensus cannot gossip these certificates efficiently, which would slow down or break the consensus algorithm's ability to:
- Recover from network partitions
- Skip stalled rounds quickly
- Help lagging nodes catch up

### What We Had to Do

1. **Added proto schema**: `crates/types/proto/liveness.proto`
   - Copied from malachite's test implementation
   - Defines wire format for PolkaCertificate and RoundCertificate

2. **Implemented codec**: `crates/types/src/codec/proto/mod.rs:457-651`
   - `impl Codec<LivenessMsg<LoadContext>> for ProtobufCodec`
   - Helper functions for encoding/decoding polka and round certificates

3. **Updated build**: `crates/types/build.rs`
   - Added liveness.proto to protobuf compilation

**Result**: ProtobufCodec now satisfies all ConsensusCodec requirements.

---

## 2. Why Config Was Updated

### The Problem

Malachite **split the monolithic config** into separate typed modules. The old approach of having a single `Config` struct no longer works.

### What Changed in Malachite

**Before** (pre-b205f425): Malachite had a single config type
```rust
struct Config {
    // All fields in one place
    consensus_field1: ...,
    consensus_field2: ...,
    sync_field1: ...,
    mempool_field1: ...,
    // etc.
}
```

**After** (b205f425): Malachite split config into **typed modules**
```rust
// From malachitebft_config crate:
struct ConsensusConfig { ... }  // Consensus-specific settings
struct MempoolConfig { ... }    // Mempool-specific settings
struct ValueSyncConfig { ... }  // Sync-specific settings
struct MetricsConfig { ... }    // Metrics-specific settings
struct LoggingConfig { ... }    // Logging-specific settings
struct RuntimeConfig { ... }    // Runtime-specific settings
```

### Why Malachite Did This

**Benefits of split config**:
1. **Type safety**: Can't accidentally pass sync config to consensus component
2. **Modularity**: Each component only sees its own config
3. **Clarity**: Clear boundaries between subsystem configurations
4. **Evolution**: Easier to add/remove fields without affecting other components

### New NodeConfig Trait Requirement

Malachite introduced the **NodeConfig trait** that applications must implement:

**From** `malachite/code/crates/app/src/node.rs:36-44`:
```rust
pub trait NodeConfig {
    fn moniker(&self) -> &str;

    fn consensus(&self) -> &ConsensusConfig;
    fn consensus_mut(&mut self) -> &mut ConsensusConfig;

    fn value_sync(&self) -> &ValueSyncConfig;
    fn value_sync_mut(&mut self) -> &mut ValueSyncConfig;
}
```

**Purpose**: The engine needs to access specific config sections without knowing your app's full config structure.

### What We Had to Do

**Created**: `crates/cli/src/config_wrapper.rs`

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub moniker: String,
    pub consensus: ConsensusConfig,    // From malachitebft_config
    pub mempool: MempoolConfig,        // From malachitebft_config
    pub sync: ValueSyncConfig,         // From malachitebft_config (renamed!)
    pub metrics: MetricsConfig,        // From malachitebft_config
    pub logging: LoggingConfig,        // From malachitebft_config
    pub runtime: RuntimeConfig,        // From malachitebft_config
    pub test: TestConfig,              // From malachitebft_config
}

impl NodeConfig for Config {
    fn moniker(&self) -> &str { &self.moniker }
    fn consensus(&self) -> &ConsensusConfig { &self.consensus }
    fn consensus_mut(&mut self) -> &mut ConsensusConfig { &mut self.consensus }
    fn value_sync(&self) -> &ValueSyncConfig { &self.sync }
    fn value_sync_mut(&mut self) -> &mut ValueSyncConfig { &mut self.sync }
}
```

**Key Changes**:
1. **Wrapper aggregates** all malachite config types
2. **Implements NodeConfig** to provide accessor methods
3. **Renamed**: `SyncConfig` → `ValueSyncConfig` (malachite renamed it)
4. **Removed**: `transport` field (no longer exists in malachite)
5. **Added defaults** for new required fields

### Impact on CLI

The CLI now:
- Loads this structured config from TOML
- Passes it to `start_engine` which extracts what it needs via NodeConfig trait
- Maintains backward compatibility with existing config files (mostly)

---

## 3. Changes in Consensus Messages (AppMsg Enum)

### Overview of Changes

The **AppMsg enum** defines how malachite communicates with your application. Malachite b205f425 made several breaking changes to this contract.

### Change 1: StartedRound - Added Crash Recovery Support

**What Changed**:

**Before**:
```rust
AppMsg::StartedRound {
    height,
    round,
    proposer
}
```

**After** (`malachite/code/crates/app-channel/src/msgs.rs:149-163`):
```rust
AppMsg::StartedRound {
    height: Ctx::Height,
    round: Round,
    proposer: Ctx::Address,
    role: Role,                               // NEW: Are we proposer/validator/observer?
    reply_value: Reply<Vec<ProposedValue<Ctx>>>, // NEW: Reply channel for crash recovery
}
```

**Why**:
- **role**: Tells the app what role this node is playing (proposer vs validator)
- **reply_value**: Implements **crash recovery protocol**
  - When consensus restarts after a crash, it asks: "Do you have any undecided values for this round?"
  - App replies with cached values or empty vec
  - Allows consensus to resume instead of restarting from scratch

**Our Implementation** (`crates/node/src/app.rs:90-103`):
```rust
AppMsg::StartedRound { height, round, proposer, role, reply_value } => {
    info!(%height, %round, %proposer, ?role, "Started round");

    state.current_height = height;
    state.current_round = round;
    state.current_proposer = Some(proposer);

    // Reply with undecided values for crash recovery
    if reply_value.send(vec![]).is_err() {
        error!("Failed to send StartedRound reply_value");
    }
}
```

Currently we send empty vec (no crash recovery implemented yet), but the protocol is ready.

---

### Change 2: ConsensusReady - Simplified Reply Format

**What Changed**:

**Before**:
```rust
AppMsg::ConsensusReady { reply } => {
    reply.send(ConsensusMsg::StartHeight(height, validator_set))
}
```

**After** (`malachite/code/crates/app-channel/src/msgs.rs:142-146`):
```rust
AppMsg::ConsensusReady {
    reply: Reply<(Ctx::Height, Ctx::ValidatorSet)>,  // Now expects a tuple
}
```

**Why**:
- **Simpler**: Just send data, not a wrapped message
- **Cleaner separation**: Reply types are now plain data, not internal message enums
- **Type safety**: Tuple makes it clear what's expected

**Our Implementation** (`crates/node/src/app.rs:74-78`):
```rust
AppMsg::ConsensusReady { reply } => {
    if reply.send((
        state.current_height,
        state.get_validator_set().clone(),
    )).is_err() { ... }
}
```

---

### Change 3: Decided - Uses Next Enum

**What Changed**:

**Before**:
```rust
AppMsg::Decided { certificate, reply } => {
    reply.send(ConsensusMsg::StartHeight(next_height, validator_set))
}
```

**After** (`malachite/code/crates/engine/src/host.rs:20-26`):
```rust
pub enum Next<Ctx: Context> {
    Start(Ctx::Height, Ctx::ValidatorSet),
    Restart(Ctx::Height, Ctx::ValidatorSet),
}

// Reply with:
reply.send(Next::Start(height, validator_set))
```

**Why**:
- **Start vs Restart**: Explicit distinction between normal progression and recovery
- **Type safety**: Can't accidentally send wrong message type
- **Better semantics**: `Next::Start` is clearer than `ConsensusMsg::StartHeight`

**Our Implementation** (`crates/node/src/app.rs:554-558`):
```rust
AppMsg::Decided { certificate, extensions: _, reply } => {
    // ... process decided value, update execution layer ...

    if reply.send(Next::Start(
        state.current_height,
        state.get_validator_set().clone(),
    )).is_err() { ... }
}
```

---

### Change 4: Removed Variants

**Removed**:
```rust
AppMsg::PeerJoined { peer_id }    // ❌ Removed
AppMsg::PeerLeft { peer_id }       // ❌ Removed
AppMsg::GetValidatorSet { height, reply }  // ❌ Removed
```

**Why They Were Removed**:

1. **PeerJoined/PeerLeft**:
   - Malachite decided apps don't need low-level peer events
   - Peer management is internal to consensus engine
   - Apps care about consensus state, not peer connections

2. **GetValidatorSet**:
   - Validator set is now provided via **ConsensusReady**
   - For dynamic validator sets, the app tells consensus when it changes
   - Removed redundant request/response pattern

**Impact**:
- We removed handlers for these (lines 276, 390 in `app.rs`)
- Peer tracking moved to internal state if needed
- Validator set management simplified

---

## Summary Table

| Change | Why | Impact |
|--------|-----|--------|
| **LivenessMsg Added** | Required by ConsensusCodec trait | Enables consensus liveness features (polka certs, round skipping) |
| **Config Split** | Type safety, modularity, clarity | Requires NodeConfig trait implementation, aggregates split configs |
| **StartedRound.role** | Know node's role in round | App can optimize behavior based on role |
| **StartedRound.reply_value** | Crash recovery protocol | Enables consensus to resume after crashes |
| **ConsensusReady tuple** | Simpler API, type safety | Cleaner reply format |
| **Decided uses Next enum** | Explicit Start vs Restart | Better semantics for height transitions |
| **Removed PeerJoined/PeerLeft** | Apps don't need peer events | Simpler message protocol |
| **Removed GetValidatorSet** | Provided via ConsensusReady | Less redundancy |

---

## The Big Picture

These changes reflect malachite's evolution toward:

1. **Better Modularity**: Split configs, typed codecs, clear boundaries
2. **Enhanced Reliability**: Crash recovery support, explicit state transitions
3. **Cleaner APIs**: Simpler reply formats, removed redundancy
4. **Type Safety**: Traits enforce correct implementations at compile time

The upgrade ensures ultramarine can leverage these improvements while maintaining compatibility with the new protocol contracts.

---

**References**:
- Malachite repo: `malachite/code/crates/`
- ConsensusCodec: `engine/src/consensus.rs:51`
- NodeConfig: `app/src/node.rs:36`
- AppMsg: `app-channel/src/msgs.rs:137`
- LivenessMsg: `core-consensus/src/types.rs:131`
