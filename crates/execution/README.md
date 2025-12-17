# Ultramarine Execution Crate

This crate provides a client for interacting with an Ethereum Execution Layer node. It implements both the standard **Eth JSON-RPC API** and the **Engine API** used for communication with a Consensus Layer client.

## Architecture

The crate is designed to be modular, flexible, and resilient.

- **Transport Layer (`transport/`)**: A `Transport` trait abstracts the underlying communication protocol. Initial implementations for `HTTP(S)` and `IPC` (Unix sockets) are provided. This design makes it easy to add new transports (e.g., WebSockets) in the future without changing the client logic.

- **Engine API Client (`engine_api/`)**: A feature-rich client for the Engine API.
  - **JWT Management**: Handles JWT generation and caching for secure authenticated communication.
  - **Capability Negotiation**: Automatically negotiates and caches client capabilities to use the highest-supported version of API methods.

- **Eth RPC Client (`eth_rpc/`)**: A client for the standard Ethereum JSON-RPC. This will be implemented using the `alloy` provider, separating it cleanly from the custom Engine API client.

- **Error Handling (`error.rs`)**: The crate uses `color-eyre` for idiomatic, report-based error handling. Fallible functions return `eyre::Result`, and the `ExecutionError` enum is used to classify specific error kinds.

## Engine API Contract

### Payload Attributes (Consensus Layer → Execution Layer)

When Ultramarine calls `engine_forkchoiceUpdatedV3`, it supplies these payload attributes:

| Field                      | Value                        | Implementation                                         |
| -------------------------- | ---------------------------- | ------------------------------------------------------ |
| `timestamp`                | `latest_block.timestamp + 1` | Monotonically increasing                               |
| **`prev_randao`**          | **Constant `0x01`**          | [`load_prev_randao()`](../types/src/engine_api.rs#L21) |
| `suggested_fee_recipient`  | Placeholder `0x2a...2a`      | TODO: Make validator-configurable                      |
| `withdrawals`              | Empty array `[]`             | Load Network has no withdrawals                        |
| `parent_beacon_block_root` | Previous `block_hash`        | EIP-4788 compatibility                                 |

### prevRandao: Constant Value Design

Load Network uses **constant `0x01`** for `prev_randao` (Arbitrum pattern):

**Rationale**:

- ✅ **Explicit signal**: Clearly indicates block-based randomness is unavailable
- ✅ **Fail-fast**: Smart contracts expecting randomness break in testing, not production
- ✅ **Identity property**: If mistakenly used in multiplication (`1 × x = x`), no corruption
- ✅ **Battle-tested**: Arbitrum uses `1`, zkSync uses `250000000000000000` (both constants)
- ✅ **Non-manipulatable**: Unlike Ethereum RANDAO (validators can bias ~1 bit per attestation)

**Enforcement**:

- **Generation**: [`client.rs:190,359`](./src/client.rs#L190) - always sends constant in `PayloadAttributes`
- **Validation**: [`state.rs:1026-1034`](../consensus/src/state.rs#L1026) - consensus rejects mismatches
- **Normalization**: [`alloy_impl.rs:95`](./src/eth_rpc/alloy_impl.rs#L95) - RPC client returns constant
- **Testing**:
  - Unit: [`state/tests/mod.rs:490`](../consensus/src/state/tests/mod.rs#L490) - `process_decided_certificate_rejects_mismatched_prev_randao`
  - Integration: [`node_harness.rs:1803`](../../test/tests/full_node/node_harness.rs#L1803) - `assert_prev_randao_constant()`

**For dApp developers**: Do not use `block.prevrandao` for security-critical randomness. Use VRF oracles (Chainlink VRF, API3 QRNG) or commit-reveal schemes.

**See also**: [FINAL_PLAN.md](../../docs/FINAL_PLAN.md#engine-api-contract-cl--el) for architecture overview.

## Public API

The public API is exposed through the `lib.rs` file and is designed to be minimal and user-friendly. The main entry point is the `ExecutionClient`.

```rust
// Example Usage
use ultramarine_execution::{ExecutionClient, ExecutionConfig};

// The config will be built out to load from a file or environment
let config = ExecutionConfig;

// The client will be instantiated via constructors
// let client = ExecutionClient::from_config(config)?;

// Access the underlying APIs
// let engine_api = client.engine();
// let eth_rpc = client.eth();
```
