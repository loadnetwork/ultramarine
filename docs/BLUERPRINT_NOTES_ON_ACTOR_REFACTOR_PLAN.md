# Plan for Refactoring Ultramarine to an Actor-Based Architecture

This document outlines a plan to refactor the Ultramarine consensus node from its current channel-based implementation to a more robust, concurrent, and fault-tolerant architecture based on the Actor Model.

## Guiding Specification

The primary guide for this refactoring effort will be **ADR 002: Architecture of a Consensus Node using an Actor System** from the `malachite` repository. This document provides a detailed blueprint for structuring a consensus engine using actors, and we will adhere to its principles.

## Proposed Architecture

We will decompose the existing application logic into a set of distinct, message-driven actors. This will replace the current monolithic `app::run` function and direct channel communication with a formal actor system, likely using the `ractor` framework as suggested in the ADR.

The proposed actors are:

### 1. Node Actor (Supervisor)
- **Responsibilities:**
    - Top-level supervisor for all other actors.
    - System initialization, actor lifecycle management (start, stop, restart).
    - Overall fault tolerance and orchestration.

### 2. Consensus Actor
- **Responsibilities:**
    - Encapsulate the core consensus logic, holding the `Driver` instance.
    - Manage the consensus state (`height`, `round`, `step`).
    - Process consensus-related inputs (`Vote`, `Proposal`, `Timeout`) and handle effects yielded by the core state machine.
    - Interact with other actors to fulfill effects (e.g., request proposals, publish votes).

### 3. Network Actor (Replaces Gossip Actor from ADR)
- **Responsibilities:**
    - Manage all peer-to-peer network communication using `libp2p`.
    - Handle message serialization/deserialization.
    - Disseminate and receive consensus messages (proposals, votes) to/from the network.
    - Manage peer discovery and connections.

### 4. Persistence Actor
- **Responsibilities:**
    - Manage persistent storage of the consensus engine's state.
    - Handle reads and writes to the database (e.g., storing finalized blocks and state).
    - Implement snapshotting and data recovery mechanisms.

### 5. WAL (Write-Ahead-Log) Actor
- **Responsibilities:**
    - Log all critical consensus messages and state transitions to disk before they are processed.
    - Ensure durability and support crash recovery.

### 6. Host Actor
- **Responsibilities:**
    - Interface with the application-specific logic.
    - Construct proposals by interacting with a mempool or other transaction source.
    - Validate incoming values (`valid(v)` logic).

### 7. Timers Actor
- **Responsibilities:**
    - Manage all consensus-related timeouts (`TimeoutPropose`, `TimeoutPrevote`, `TimeoutPrecommit`).
    - Send timeout notifications to the `ConsensusActor`.

## High-Level Refactoring Plan

1.  **Introduce `ractor`:** Add the `ractor` crate as a dependency to the `ultramarine-node` crate.
2.  **Create Actor Shells:** Define the basic struct and message types for each actor listed above.
3.  **Implement the Node Actor:** Create the main supervisor actor responsible for starting and managing all other actors.
4.  **Migrate Logic Incrementally:**
    - Start by migrating the network logic from `app::run` into the `NetworkActor`.
    - Move the core consensus loop from `app::run` into the `ConsensusActor`.
    - Abstract database interactions into the `PersistenceActor`.
    - Continue this process for all other actors, replacing direct function calls and channel `send`/`recv` with actor message passing.
5.  **Establish Communication:** Define and implement the message protocols for inter-actor communication (e.g., `ConsensusActor` sending a `PublishVote` message to the `NetworkActor`).
6.  **Remove Old Structure:** Once all logic is migrated to the actor system, the existing `app::run` function and its associated channels can be removed.

## Benefits

- **Improved Fault Tolerance:** The actor model's supervision hierarchy allows for actors to be restarted upon failure without bringing down the entire node.
- **Enhanced Concurrency:** Each actor runs in its own lightweight process, allowing for better utilization of multi-core systems.
- **Increased Modularity & Maintainability:** Logic is cleanly separated into distinct, single-responsibility components, making the codebase easier to understand, test, and maintain.
- **Alignment with Best Practices:** This refactoring brings `ultramarine` in line with the formal architecture specified by `malachite`, its foundational library.
