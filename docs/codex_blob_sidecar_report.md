  Technical Review – Ultramarine Blob Sidecar Integration

  - Architecture Snapshot
      - Consensus uses Malachite app-channel; proposals streamed as single byte stream of payload data only (ultramarine/crates/consensus/src/state.rs:336).
      - Storage (REDB) keeps undecided/decided payload bytes per (height, round) but no blob bundle separation (ultramarine/crates/consensus/src/store.rs:324).
      - Execution client negotiates and uses Cancun (engine_getPayloadV3, engine_newPayloadV3) and ignores the blobsBundle returned by EL (ultramarine/crates/execution/src/client.rs:171).
      - On commit, the node decodes payload, forwards versioned hashes, but never checks or forwards blob data (ultramarine/crates/node/src/app.rs:404).
      - Networking exposes consensus, proposal parts, sync, liveness channels only; no blob gossip subnets (malachite/code/crates/network/src/channel.rs:7).
  - Spec & Reference Insights
      - Cancun spec mandates blobsBundle in engine_getPayloadV3 response and expectedBlobVersionedHashes validation (execution-apis/src/engine/cancun.md:150).
      - Prague/V4 allows CL to push bundles during engine_newPayloadV4, adding executionRequests (execution-apis/src/engine/prague.md:27).
      - Deneb p2p spec requires dedicated blob sidecar gossipsub topics, validation of inclusion/KZG proofs, and serving history across blob_serve_range (consensus-specs/specs/deneb/p2p-interface.md:380).
      - Lighthouse workflow: fetch blobs from EL (engine_getBlobsV1), publish unseen blobs, maintain availability caches, and ensure sidecar storage (lighthouse/beacon_node/beacon_chain/src/fetch_blobs/mod.rs:69).
  - Current Gaps
      - Proposal IDs only cover payload bytes; adding blobs without adjusting hashing risks consensus divergence (ultramarine/crates/types/src/value.rs:56).
      - No storage or streaming path for blob commitments, proofs, or bodies; restream/sync handlers unaware of blobs (ultramarine/crates/node/src/app.rs:360).
      - Execution client doesn’t request V4 capabilities or engine_getBlobsV1; results in non-proposers failing to import blob blocks.
      - Networking lacks blob-specific channels, gossip validation, or anti-spam/availability tracking as per Deneb requirements.
      - No KZG or inclusion proof validation prior to calling engine_newPayload, leaving EL without guaranteed blob data.
  - Recommended Architecture Adjustments
      - Treat execution payload and blob sidecars as a single consensus value. Extend ProposalPart with variants for blob metadata and chunked blobs; update signing hash to include both.
      - Introduce separate REDB tables for undecided/decided blob bundles keyed by (height, round, blob_index); adjust pruning.
      - Add new Malachite network topic (or multiplexed streams) for blob sidecars with validation hooks matching blob_sidecar_{subnet_id} rules.
      - Extend ExecutionClient for V4: capability negotiation, engine_getPayloadV4, engine_newPayloadV4, engine_getBlobsV1. Capture and persist blobsBundle.
      - On commit, reconstruct payload + bundles, verify commitment ordering and (optionally) KZG proofs (via c-kzg-4844) before submitting to EL.
      - Implement restream/sync logic that retransmits both payload and blob bundles; add fallback to fetch bundles from EL when missing.
  - Step-by-Step Implementation Plan
      1. Engine API Layer
          - Define BlobsBundleV1, BlobAndProofV1 types in ultramarine_types.
          - Update capabilities to request V4 + getBlobs; implement client methods for engine_getPayloadV4, engine_newPayloadV4, engine_getBlobsV1, with Cancun fallback logic.
      2. Consensus Types & Hashing
          - Add blob-aware ProposalPart variants; update State::make_proposal_parts to stream payload and blob chunks separately while hashing all content before Fin.
          - Adjust Value hashing to include Merkle root / combined hash of payload & blob data to avoid ID collisions.
      3. Storage
          - Extend REDB schema (migrate) with blob tables for undecided/decided states; store bundles alongside payload bytes during propose/receive and promote on commit.
          - Persist metadata (commitments, proofs) to support proof verification and re-use.
      4. Networking
          - Create new network message for blob sidecars or encode multiplexing, align with Deneb topic naming, and enforce sequence validation/size limits.
          - Track observed blobs per (slot, index) to avoid rebroadcast spam.
      5. App Logic
          - Proposer: when receiving payload+bundle from EL, persist and stream both; handle round timeouts gracefully.
          - Non-proposer: reassemble payload + blobs before replying to GetValue/ReceivedProposalPart; verify commitments before storing.
          - Decided: validate blobs (commitment ordering, optional KZG) and call engine_newPayloadV4; if blobs missing, fetch via engine_getBlobsV1 or trigger restream.
      6. Sync & Restream
          - Implement RestreamProposal to resend payload + blob bundles; extend sync RPC/messages to serve blob sidecars across the required range with proofs.
          - Add pruning rules to retain blobs for at least MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS.
      7. Testing & Tooling
          - Write integration tests covering proposer/non-proposer consensus with blobs, missing blob recovery via EL fetch, restream flow, and DB migrations.
          - Instrument metrics/logging for blob availability, fetch latency, and verification failures.

  Let me know if you’d like this converted into a doc file once write access is available or if you want any part expanded.
