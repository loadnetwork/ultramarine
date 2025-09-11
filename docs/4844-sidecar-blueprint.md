# 4844 Sidecar Integration Blueprint (Ultramarine)

Goal: reliably import blocks with blob transactions (EIP‑4844) in a Tendermint/Malachite environment without relying on Ethereum’s Beacon/CL P2P for blob propagation.

## Background

- Cancun (Engine API V3):
  - `engine_getPayloadV3` returns `executionPayload` + `blobsBundle` to the CL when the EL builds a block.
  - `engine_newPayloadV3` accepts `executionPayloadV3`, `expectedBlobVersionedHashes`, and `parentBeaconBlockRoot` only — no sidecar push from CL.
  - Non‑proposer CL nodes cannot reliably import a blob block unless their EL already has the blobs (not true in our Tendermint setup).

- Prague (Engine API V4):
  - `engine_newPayloadV4` accepts `blobsBundle` as part of the request. This enables the CL to supply sidecars to the EL at import time.
  - This is the first standard path for a CL to push sidecars to EL directly.

References: see `execution-apis/src/engine/cancun.md`, `execution-apis/src/engine/prague.md`, `execution-apis/src/engine/osaka.md` in the workspace.

## Approach in Tendermint Environment

Treat the sidecar as part of the proposed value.

1) Extend proposal format
- Add proposal parts for `blobsBundle` (commitments, proofs, blobs). Stream them alongside payload bytes.
- Include both payload bytes and sidecar bytes into the proposal signing hash so the value ID covers the complete content.

2) Storage
- Persist undecided `executionPayload` bytes and `blobsBundle` per (height, round) for both proposer and non‑proposer.
- On decision, retrieve both to call `engine_newPayloadV4`.

3) App logic
- Proposer: when calling `getPayloadV4` (or V3), capture the returned `blobsBundle` and stream it with the proposal.
- Non‑proposer: reassemble payload + sidecar from parts before replying with `ProposedValue`; on `Decided`, call `engine_newPayloadV4(payload, blobsBundle, parentBeaconBlockRoot)`.

4) Error handling
- If sidecar is missing on non‑proposer import, do not call `newPayload`; log and fallback to sync (re‑request parts or trigger round restream).
- Validate lengths and hashes of sidecar content before passing to EL; optional: verify KZG cell proofs (Osaka APIs) pre‑submission for defense in depth.

## Implementation Plan

1. Engine API V4 support (execution crate)
- Add V4 request/response types and `engine_newPayloadV4`/`engine_getPayloadV4` methods.
- Keep V3 in place for Cancun‑compatible flows.

2. Types and codec (ultramarine_types)
- Define serde models for `BlobsBundleV1` (and optionally V2) compatible with Engine API.
- Add conversion/helpers to move between alloy payloads and JSON structs.

3. Proposal parts (consensus/state)
- Add ProposalPart variants to carry sidecar chunks.
- Update proposal signing hash to include both payload and sidecar streams deterministically.
- Update reassembly to produce `(executionPayload, blobsBundle)` pair.

4. Storage (consensus/store)
- Add tables for undecided and decided sidecar bytes keyed by (height, round) and/or value ID.
- Update pruning to remove historical sidecars accordingly.

5. App handlers (node/app.rs)
- Proposer: capture sidecar from `getPayloadV4` (or V3) and stream.
- Non‑proposer: reassemble and store; on `Decided`, call V4 `newPayload` with sidecar.
- Keep Cancun path for non‑blob blocks; prefer V4 when sidecar present.

## Scope & Effort Estimate

- Engine V4 client + types: 2–4 days
- Proposal parts + hashing + reassembly: 4–7 days
- Storage additions + pruning: 2–3 days
- App logic wiring (proposer/non‑proposer/Decided): 3–5 days
- Optional KZG verification (c‑kzg‑4844): 3–7 days

Total: ~2–3 weeks for a robust, shippable integration (without KZG verification), assuming existing testing and CI.

## Notes

- Until V4 is integrated, avoid blob txs in proposer payloads or accept that non‑proposers may fail to import blob blocks.
- Restreaming support should also resend sidecar parts.
