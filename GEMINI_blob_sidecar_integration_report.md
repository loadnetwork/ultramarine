# Technical Report: EIP-4844 Blob Sidecar Integration in Ultramarine

**Date:** 2025-10-14

**Author:** Gemini

## 1. Executive Summary

This report outlines a technical plan for integrating EIP-4844 (Proto-Danksharding) blob sidecars into the `ultramarine` consensus client. The goal is to make `ultramarine` compatible with the Ethereum Deneb/Cancun upgrade, enabling it to process blocks with blobs and participate in the blob data availability network.

The integration is feasible and can be achieved by extending the existing modular architecture of `ultramarine` and its underlying `malachite` library. The core of the work involves:

1.  **Defining New Data Types:** Introducing Rust structs for `BlobSidecar`, `BlobsBundle`, and other related containers as specified in the Deneb/Cancun specifications.
2.  **Extending the P2P Network Layer:** Adding new gossip topics and request-response protocols for blob propagation.
3.  **Updating the Consensus Engine:** Modifying the block processing and validation logic to handle blob commitments and proofs.
4.  **Upgrading the Engine API:** Implementing the new Cancun-specific methods for communication with the execution layer.
5.  **Implementing Blob Management:** Creating logic for fetching, verifying, and storing blobs.

This report is based on a thorough review of the `ultramarine` codebase, the `malachite` library, the official EIP-4844 specifications (`consensus-specs` and `execution-apis`), and the `lighthouse` reference implementation.

## 2. Analysis of Existing Architecture

### 2.1. `ultramarine`
`ultramarine` is a Rust-based consensus client built on the `malachite` library. Its workspace is well-structured into the following crates:
- `crates/types`: For shared data structures.
- `crates/consensus`: For consensus logic.
- `crates/execution`: For Engine API communication.
- `crates/node`: For node orchestration.
- `crates/cli`: For the command-line interface.

This modular design is well-suited for the required changes, as the impact of EIP-4844 is distributed across these domains.

### 2.2. `malachite`
`malachite` provides the foundational components for building a consensus client. Our analysis of the `malachite/code` directory revealed the following key insights:
- **Networking (`crates/network`):** It uses `libp2p-rs` and provides a clear mechanism for adding new `gossipsub` topics by extending a `Channel` enum. This is ideal for implementing the `blob_sidecar_{subnet_id}` topics.
- **Synchronization (`crates/sync`):** It uses a generic, `Bytes`-based request-response protocol. New RPCs, such as `BlobSidecarsByRange` and `BlobSidecarsByRoot`, can be added by extending `Request` and `Response` enums located in `crates/sync/src/types.rs`.
- **Engine (`crates/engine`):** This crate encapsulates Engine API communication and will be the locus of changes for Cancun API support.

The `malachite` library is sufficiently extensible to support the required P2P networking changes without fundamental architectural modifications.

### 2.3. `lighthouse` (Reference Implementation)
Our review of the `lighthouse` codebase provided a practical roadmap for implementation:
- **`consensus/types/src/blob_sidecar.rs`**: Defines the core `BlobSidecar` struct, closely matching the specification.
- **`beacon_node/beacon_chain/src/blob_verification.rs`**: Implements a robust, multi-stage validation pipeline for blobs received over gossip. This is a model for our own verification logic.
- **`beacon_node/network/src/sync/block_sidecar_coupling.rs`**: Contains logic to handle the asynchronous arrival of blocks and sidecars during range syncs, coupling them into a complete `RpcBlock`.
- **`beacon_node/execution_layer/`**: Implements the client-side of the Cancun Engine API.

## 3. Proposed Architecture & Design

We propose the following architectural changes to integrate blob sidecar functionality into `ultramarine`.

### 3.1. Data Structures (`ultramarine/crates/types`)

New data structures will be added to the `types` crate, mirroring the specifications and the `lighthouse` implementation. All new types will derive `ssz::Encode`, `ssz::Decode`, and `tree_hash::TreeHash`.

- **`BlobSidecar`**: The primary container for a blob, its KZG proof, and the corresponding signed block header.
- **`BlobIdentifier`**: A simple struct (`block_root`, `index`) for uniquely identifying a blob.
- **`BlobsBundle`**: The structure returned by `engine_getPayloadV3`, containing commitments, proofs, and blobs.
- **Updated Block/State Containers**: `BeaconBlockBody`, `ExecutionPayload`, and `BeaconState` will be updated to include the new Deneb/Cancun fields (`blob_kzg_commitments`, `blob_gas_used`, etc.).

### 3.2. P2P Networking (`malachite/code/crates/network` & `sync`)

The networking changes will be implemented primarily within the `malachite` library to ensure they are reusable.

1.  **Gossip Topics:**
    -   The `Channel` enum in `malachite/code/crates/network` will be extended to include a `BlobSidecar(SubnetId)` variant.
    -   The `to_gossipsub_topic()` method will be updated to generate the correct topic string: `blob_sidecar_{subnet_id}`.
    -   The `ultramarine` node will subscribe to the required blob subnets upon startup.

2.  **Request-Response Protocols:**
    -   In `malachite/code/crates/sync/src/types.rs`, new request/response structs will be defined: `BlobSidecarsByRangeRequest`, `BlobSidecarsByRangeResponse`, `BlobSidecarsByRootRequest`, `BlobSidecarsByRootResponse`.
    -   The `Request` and `Response` enums in the same file will be extended with variants for these new types.
    -   The `sync` behaviour in `malachite/code/crates/sync/src/behaviour.rs` will be updated to handle these new request types, dispatching them to a new blob-handling logic module.

### 3.3. Consensus & Block Processing (`ultramarine/crates/consensus`)

The `consensus` crate will be updated to handle the new validation and processing rules.

1.  **Blob Verification Service:** A new service, `BlobVerifier`, will be created, modeled after `lighthouse`'s `blob_verification.rs`. It will be responsible for:
    -   Performing all gossip validation checks (timeliness, parent known, signature, etc.).
    -   Verifying the KZG commitment inclusion proof.
    -   Verifying the KZG proof against the blob and commitment (the most expensive step). This should be done in a dedicated thread pool to avoid blocking the main event loop, and batch verification should be used whenever possible.

2.  **Block-Sidecar Coupling:** A mechanism, similar to `lighthouse`'s `block_sidecar_coupling`, will be implemented to associate incoming blocks with their corresponding sidecars, which may arrive out of order. This service will hold "unpaired" blocks and sidecars for a short period before requesting the missing parts.

3.  **State Transition Function:** The state transition logic will be updated to:
    -   In `process_block`, verify the `blob_kzg_commitments` and pass the derived `versioned_hashes` to the execution engine via the updated Engine API.

### 3.4. Engine API (`ultramarine/crates/execution`)

The `execution` crate will be upgraded to support the Cancun Engine API.

1.  **New Data Structures:** `ExecutionPayloadV3`, `PayloadAttributesV3`, etc., will be implemented.
2.  **Updated Methods:** The clients for `engine_newPayload`, `engine_forkchoiceUpdated`, and `engine_getPayload` will be updated to their `V3` versions.
3.  **`engine_getPayloadV3` Handling:** The logic for `engine_getPayloadV3` will be updated to receive the `blobsBundle` and use it to construct the `BlobSidecar`s when producing a block.
4.  **`engine_getBlobsV1` Implementation:** A new method will be added to the `ExecutionApiClient` to call `engine_getBlobsV1`. This will be used as a fallback mechanism for data availability.

### 3.5. Blob Management (`ultramarine/crates/node`)

A central `BlobManager` service will be created within the `node` crate to orchestrate the entire lifecycle of a blob. Its responsibilities will include:

-   Receiving gossip blobs from the network and sending them to the `BlobVerifier`.
-   Receiving blocks and initiating the process of fetching missing blobs.
-   Deciding the fetching strategy: wait for gossip, request from peers via RPC, or request from the local EL via `engine_getBlobsV1`.
-   Storing verified blobs in a cache or database, indexed by `BlobIdentifier`.
-   Providing blobs to the consensus engine when needed for block processing.

## 4. Step-by-Step Implementation Plan

The implementation will be broken down into the following sequential steps. Each step should be accompanied by comprehensive unit and integration tests.

**Step 1: Data Structures and Types (`ultramarine/crates/types`)**
1.  Define the `BlobSidecar<E: EthSpec>` struct with all its fields.
2.  Define the `BlobIdentifier` struct.
3.  Define the `BlobsBundleV1` struct.
4.  Update `BeaconBlockBody`, `ExecutionPayload`, `ExecutionPayloadHeader`, and `BeaconState` to their Deneb versions, adding the new fields behind a `#[cfg(feature = "deneb")]` flag if necessary.
5.  Implement SSZ and TreeHash derivations for all new and modified types.

**Step 2: Engine API Upgrade (`ultramarine/crates/execution`)**
1.  Implement the `V3` versions of `ExecutionPayload`, `PayloadAttributes`, etc.
2.  Update the `EngineApiClient` trait and its implementation to support `engine_newPayloadV3`, `engine_forkchoiceUpdatedV3`, and `engine_getPayloadV3`.
3.  Add the `engine_getBlobsV1` method to the `EngineApiClient`.
4.  Write integration tests to ensure the new API calls work as expected against a real or mocked EL.

**Step 3: P2P Network Extensions (`malachite`)**
1.  **Gossip:**
    -   In `malachite/code/crates/network`, add a `BlobSidecar(SubnetId)` variant to the `Channel` enum.
    -   Update `to_gossipsub_topic()` to handle the new variant.
2.  **Req/Resp:**
    -   In `malachite/code/crates/sync/src/types.rs`, define the `BlobSidecarsByRange` and `BlobSidecarsByRoot` request/response structs.
    -   Add variants for these to the main `Request` and `Response` enums.
    -   In `malachite/code/crates/sync/src/behaviour.rs`, add the handler logic to process incoming blob requests and generate responses. This will involve fetching data from a new blob store.

**Step 4: Blob Verification and Storage (`ultramarine/crates/consensus`)**
1.  Create the `BlobVerifier` service.
2.  Implement the full gossip validation pipeline as described in section 3.3.
3.  Integrate a KZG library (e.g., `c-kzg`) for proof verification.
4.  Create a simple in-memory `BlobStore` for storing and retrieving verified blobs by `BlobIdentifier`. This can be extended to a persistent DB-backed store later.

**Step 5: Block and Sidecar Processing (`ultramarine/crates/consensus` & `node`)**
1.  Implement the block/sidecar coupling logic to handle unpaired items.
2.  Create the `BlobManager` service in the `node` crate to orchestrate blob fetching and verification.
3.  Modify the block processing logic in the `consensus` crate to use the `BlobManager` to ensure data availability before processing a block.
4.  Update the block production logic to get the `blobsBundle` from the EL and construct and gossip the `BlobSidecar`s.

**Step 6: Integration and End-to-End Testing**
1.  Integrate all the new components into the `ultramarine` node startup process.
2.  Create a suite of integration tests that simulate a small network of `ultramarine` nodes.
3.  Test the full lifecycle:
    -   A node produces a block with blobs.
    -   The block and sidecars are gossiped to other nodes.
    -   Peers verify the block and sidecars.
    -   A node successfully syncs a range of blocks and their corresponding blobs from a peer.
    -   A node successfully fetches a missing blob from its local EL.

## 5. Conclusion

The integration of EIP-4844 into `ultramarine` is a significant but manageable undertaking. The modular design of both `ultramarine` and `malachite` provides a clear path for extension. By following the proposed architecture and step-by-step plan, and by referencing the `lighthouse` implementation, we can deliver a performant, flexible, and viable solution that brings `ultramarine` to feature parity with other modern consensus clients.