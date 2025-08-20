# Ultramarine Execution Crate

This crate provides a client for interacting with an Ethereum Execution Layer node. It implements both the standard **Eth JSON-RPC API** and the **Engine API** used for communication with a Consensus Layer client.

## Architecture

The crate is designed to be modular, flexible, and resilient.

-   **Transport Layer (`transport/`)**: A `Transport` trait abstracts the underlying communication protocol. Initial implementations for `HTTP(S)` and `IPC` (Unix sockets) are provided. This design makes it easy to add new transports (e.g., WebSockets) in the future without changing the client logic.

-   **Engine API Client (`engine_api/`)**: A feature-rich client for the Engine API.
    -   **JWT Management**: Handles JWT generation and caching for secure authenticated communication.
    -   **Capability Negotiation**: Automatically negotiates and caches client capabilities to use the highest-supported version of API methods.

-   **Eth RPC Client (`eth_rpc/`)**: A client for the standard Ethereum JSON-RPC. This will be implemented using the `alloy` provider, separating it cleanly from the custom Engine API client.

-   **Error Handling (`error.rs`)**: The crate uses `color-eyre` for idiomatic, report-based error handling. Fallible functions return `eyre::Result`, and the `ExecutionError` enum is used to classify specific error kinds.

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