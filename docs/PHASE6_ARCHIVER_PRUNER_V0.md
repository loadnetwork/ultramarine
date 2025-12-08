# Phase 6 – Archiver & Pruner V0 Design

## Motivation

Ultramarine Phase 5 leaves blob lifecycle management at “decided → delete”:

- `BlobEngineImpl` exposes lifecycle hooks (`verify_and_store`, `mark_decided`, `mark_archived`, `prune_archived_before`) but only `prune_archived_before` is exercised today (`ultramarine/crates/consensus/src/state.rs:1473`).
- The RocksDB store deletes decided blobs permanently and has no notion of external archival (`ultramarine/crates/blob_engine/src/store/rocksdb.rs:374-420`).
- Validators therefore cannot prove that a blob was mirrored before pruning, and there is no coordination primitive for “this blob is archived, it is now safe to prune”.

Phase 6 must add a minimal archiving+pruning pipeline so decided blobs can be exported to long-term storage (initially the in-repo `load-s3-agent`) and pruned deterministically once every validator knows the archive pointer.

## Scope & Goals

1. **Archiver V0:** Slot/block proposer uploads its decided blobs to a single provider (load-s3-agent) and produces an attested `ArchiveReceipt`.
2. **Distribution:** All validators learn the receipt via consensus-safe messaging, persist it alongside blob metadata, and mark the blob as “archived”.
3. **Pruner V0:** Nodes prune blob bytes only when (a) the block is finalized and (b) every blob is archived. No additional retention horizon is enforced in V0 (optional knobs can be added later).
4. **Observability & Ops:** Provide Prometheus metrics, logs, and documentation so operators can monitor archive progress/failures.
5. **Extensibility:** Architect the solution so providers, policies, and on-chain enforcement can evolve after Phase 6.

## Assumptions & Constraints

- **Single archiver provider:** Phase 6 integrates only with `load-s3-agent` (REST API described in `load-s3-agent/README.md`). Future releases may add S3, Arweave, Filecoin, etc.
- **Duty = proposer:** The slot proposer is the only validator allowed/expected to archive the blobs for that height. Non-proposers only verify receipts and request re-uploads if the proposer fails.
- **Metadata retention:** Blob metadata (`ultramarine/crates/types/src/blob_metadata.rs`) remains “keep forever”; the archiver only moves *blob bodies* out of RocksDB.
- **Consensus transport:** Re-use Malachite’s value-sync primitives for reliable point-to-point delivery (`malachite/docs/architecture/adr-005-value-sync.md:1-210`); avoid inventing custom gossip.
- **Security model:** 2f + 1 validators already signed the block. The archiver adds data-availability accountability: the proposer signs an `ArchiveReceipt`, and followers verify by hashing the blob bytes they already store.
- **Async by default:** archival can lag finalized heights; the consensus path only queues work and never blocks on provider availability. Metrics expose backlog/lag for ops.

## Current State Overview

| Layer | What exists today | Gap for Phase 6 |
| --- | --- | --- |
| Consensus | After commit, `State::commit_certificate` promotes decided blobs and hard-prunes everything older than `height-5` (`ultramarine/crates/consensus/src/state.rs:1473-1507`). | No hook to trigger uploads or wait for receipts before pruning. |
| Blob Engine | `BlobEngineImpl::mark_archived` deletes per-blob entries from the decided column family (`ultramarine/crates/blob_engine/src/engine.rs:335-358`). | Never called; cannot store archive pointers or “archived” flags. |
| Metadata | `BlobMetadata` holds execution header + KZG commitments forever (`ultramarine/crates/types/src/blob_metadata.rs:1-230`). | Needs a place to stash archive proofs/pointers for sync/restream consumers. |
| Providers | load-s3-agent can ingest data items and returns IDs / metadata (`load-s3-agent/src/core/registry.rs`, `README.md`). | No Ultramarine component drives uploads or consumes returned IDs. |

## Requirements

### Functional

1. **Upload orchestration**
   - Archive duties are scheduled when the local node is proposer for a finalized block.
   - Upload happens *after* consensus marks blobs decided to ensure bytes match canonical commitments, and it runs asynchronously via a background queue so the commit path never stalls.
   - Provider response (dataitem ID, bucket, checksums) is captured.
2. **Receipt propagation**
   - Proposer publishes a signed `ArchiveReceipt` / `ArchiveNotice` that includes: block height, blob indices, blob commitment root, provider ID, object key (e.g., load-s3 dataitem ID), and a deterministic blob hash (Keccak of the sidecar bytes).
   - Receipts are delivered via reliable request/response to every validator (similar to decided value sync) so lagging nodes catch up.
3. **Verification**
   - Followers recompute both the blob commitment (already stored in `BlobMetadata`) and the blob byte Keccak from their local RocksDB entry (keyed by height + index) and verify both match the notice before marking the blob archived.
   - If verification fails or receipt never arrives, followers can request a retransmit or mark the proposer faulty for ops visibility.
4. **Pruning policy**
   - Per-block pruning fires as soon as every blob at height H has a verified receipt and the block is finalized; there is no additional height/time delay in V0 (operators may configure one later if desired).
   - Metadata, receipts, and provider pointers remain.
5. **Metadata preservation**
   - `BlobMetadata` gains an optional `archive_records` array persisted forever.
   - Each record carries the provider key returned by load-s3-agent (see `load-s3-agent/src/core/registry.rs:8-40`).
6. **Observability**
   - Counters: `archiver_jobs_total`, `archiver_failures_total`.
   - Gauges: `archiver_inflight_jobs`, `blob_engine_archived_bytes`.
   - Histograms: upload latency, receipt dissemination latency.

### Non-Functional

- Serialized receipts must be ≤ 1 KiB so they fit inside Malachite proposal messages without impacting bandwidth.
- Upload retries exponential backoff with jitter; max concurrent uploads configurable (default 1).
- Provider credentials (load-s3-agent API token) stored via existing secret plumbing (env vars or config file).
- Support deterministic unit tests and integration harness coverage similar to Phase 5 `ultramarine-test`.

## Proposed Architecture

```
┌───────────────────────────────┐
│ Consensus State Machine       │
│  (State::commit_certificate)  │
└────────────┬──────────────────┘
             │ schedule duty
┌────────────▼────────────┐
│ ArchiverCoordinator     │
│  - duty queue           │
│  - retry policies       │
│  - metrics/logging      │
└───────┬──────────┬──────┘
        │upload    │receipt gossip
┌───────▼──────┐   │   ┌───────────────┐
│ Provider SDK │   │   │ Receipt Hub   │
│ (load-s3)    │   │   │ (Value Sync)  │
└───────┬──────┘   │   └──────┬────────┘
        │metadata  │          │deliver receipts
┌───────▼──────────▼──────────▼─────────┐
│ Blob Store + Metadata Store           │
│ - Record archive pointer              │
│ - Mark archived (no delete yet)       │
│ - Prune once policy satisfied         │
└───────────────────────────────────────┘
```

### Component 1: `ArchiverCoordinator`

- Runs inside the Ultramarine consensus service.
- Subscribes to `commit_certificate` events (same place we call `mark_decided`).
- Keeps a bounded queue of `ArchiveJob { height, proposer, blob_indices }`.
- Only enqueues when `local_validator_id == proposer_id`.
- Drives uploads sequentially (Phase 6) to simplify error handling; future: parallelization.
- On success, produces `ArchiveReceipt` and hands it to `ReceiptHub`.

### Component 2: Provider abstraction

Define a trait under `ultramarine/crates/blob_engine/src/archive.rs` (new module):

```rust
#[async_trait]
pub trait ArchiveProvider {
    type ReceiptMetadata;
    async fn archive(
        &self,
        height: Height,
        blobs: &[BlobSidecar],
    ) -> Result<Self::ReceiptMetadata, ArchiveError>;
    fn provider_id(&self) -> &'static str;
}
```

Implementation `LoadS3AgentProvider` calls:

1. `POST /upload` with multipart payload (see `load-s3-agent/README.md`).
2. Collects `dataitem_id`, bucket, optional tags (maybe encode block height, blob index, KZG commitment).
3. The service does not return a checksum, so the provider client computes `blob_data_keccak` locally from the bytes it uploaded and stores it in the notice.

The provider also exposes health probes (e.g., `GET /stats`) so the coordinator can short-circuit when the agent is down.

### Component 3: Receipt propagation (`ReceiptHub`)

- Introduce `ProposalPart::Archive` so archive notices ride on the same channel as payload/blobs and are hashed into the FIN signature (restreamers cannot mutate locator/hash without invalidating the proposal).
- `ReceiptHub` remains responsible for replaying those notices to lagging peers via the existing ADR-005 request/response path (similar to how missed proposal parts are restreamed today).
- Receipt/notice schema (SSZ/Protobuf):

```
message ArchiveNotice {
  uint64 height;
  uint32 blob_index;
  bytes kzg_commitment;
  bytes blob_keccak;
  string provider_id;
  bytes provider_pointer; // e.g. load-s3 dataitem_id
  bytes archived_by;      // proposer address
  uint64 timestamp;
}
```

- `ProposalPart::Archive` embeds one or more notices before the `Fin` part so the Keccak accumulator covers them without extra signatures.
- Followers store the notice in RocksDB alongside blob metadata and mark the indicated blobs as archived without deleting bytes yet. `ReceiptHub` ensures notice replay for peers that missed the original stream.

### Component 4: Archival metadata persistence

Add a new column family or table, e.g., `blob_archive_records`, keyed by `(height, blob_index)`. Each entry stores:

```
struct ArchiveRecord {
    provider_id: ProviderId,
    pointer: Vec<u8>,          // dataitem ID / URL
    checksum: B256,            // blob_data_keccak
    archived_by: ValidatorId,
    archived_at: u64,          // unix seconds
    receipt_signature: Bytes,  // proposer signature
}
```

Expose this in the store API (`BlobStore::record_archive_receipt(...)`) so both consensus and sync paths see consistent data.

### Component 5: Pruning policy engine

- Replace the hardcoded “retain 5 heights” logic with a policy struct that primarily encode two knobs:

```rust
struct PruningPolicy {
    retain_heights: Option<u64>, // None (or 0) in V0
    require_archive: bool,
}
```

- Default Phase 6 settings: `retain_heights = None` (prune immediately once archived + finalized) and `require_archive = true`. Operators can opt into a non-zero delay later via config if they want extra buffer.
- `BlobEngine::prune_archived_before` now checks both decided-store bytes and archive table to ensure receipts exist before deletion; with `retain_heights = None`, it simply walks finalized heights whose blobs are archived.

## Flow Details (Protocol Contract)

**Serving guarantee:** Nodes serve blob bytes only while `archival_status = Pending` (undecided/decided). Once a proposer uploads and a notice is verified (`Archived`), the node deletes the blob bytes locally and relies on the locator for any historical fetch. Metadata (commitments, execution payload header, signed archive notice) remains available forever for verification/sync.

### 1. Proposer archiving

1. Consensus finalizes height *H*; `State::commit_certificate` identifies `proposer_id` from the certificate.
2. If `self.validator_id == proposer_id`, enqueue an async job (`ArchiveJob { height: H, blob_indices }`) in the coordinator queue.
3. When the worker pops the job it pulls decided blobs via `BlobEngine::get_for_import(H)` (already used for EL handoff).
4. Build deterministic metadata: include block hash, blob indices, total byte length, and compute `blob_data_keccak` for each sidecar. These hashes will later be the only way to verify archived blobs, because RPCs stop serving the raw bytes once archived.
5. Call `ArchiveProvider::archive` (load-s3-agent client) and obtain the provider pointer.
6. On success, build an `ArchiveNotice` and persist it locally (so restarts retain progress). The notice hash is the Keccak accumulator that already includes Archive parts when we restream them.
7. Feed the notice into `ReceiptHub` / restream path so other validators learn it; existing FIN signature already covers the notice contents.
8. Once every blob index in the block has a recorded notice, call `BlobEngine::mark_archived(H, index, locator, blob_keccak)` per blob (or batch API) to clear the decided-bytes table and free disk.

### 2. Follower handling

1. On receipt arrival, verify proposer signature and that `height` matches the block’s decided metadata.
2. Recompute `blob_commitment_root` from local RocksDB entries:
   - Each `BlobSidecar` already carries KZG commitment; derive the same hash the proposer attested.
3. If valid, write an `ArchiveRecord` for `(H, idx)`.
4. Maintain a per-height tracker: once all blob indices have receipts *and* the block is finalized, schedule local pruning immediately (after pruning the node will no longer serve these blobs over consensus RPCs).
5. If verification fails, emit an alert metric (`archiver_receipt_mismatch_total`) and request retransmission.

### 3. Retry & fault handling

- Coordinator retries failed uploads with exponential backoff capped at e.g. 5 minutes. After N failures, mark job as “needs manual intervention” and expose via metrics.
- Followers who never receive a receipt continue to store the blob indefinitely, gossip a `missing_archive_receipt` status, and rely on ops to trigger a manual archive/retry. There is no automatic deadline; disk pressure surfaces the fault.
- Since only proposers upload, we avoid contention. If a proposer goes offline before uploading, block data remains available via consensus sync (blobs stay in RocksDB). Operators can re-run the archiver job manually using the stored blobs.

### 4. Sync / restream compatibility

- When value sync (Malachite ADR-005) transfers a decided block to a lagging node, include the archive receipts in the payload.
- During restream, the proposer reuses archived metadata to avoid re-uploading; only missing receipts trigger a re-archive.

### 5. Prune loop

- Run `Pruner` task every X seconds:
  1. Determine highest finalized height `H_f`.
  2. Gather heights `H ≤ H_f` whose blobs all have receipts.
  3. Call `blob_engine.prune_archived_before(H + 1)` (or delete per-height) so those blobs are removed immediately after archival.
  4. Emit metrics for bytes reclaimed / backlog remaining.

## Observability Plan

- Metrics (Prometheus):
  - `archiver_jobs_total{result="success|failure"}` increment per job.
  - `archiver_upload_duration_seconds` histogram.
  - `archiver_receipts_missing` gauge per height.
  - `blob_engine_archived_bytes{state="decided|archived|pruned"}` gauge.
- Logs:
  - `info`: successful archive (height, blob_count, provider_id, object IDs).
  - `warn`: retry/backoff events.
  - `error`: verification mismatches, provider errors.
- Grafana:
  - Extend existing blob lifecycle row with “Archive backlog”, “Upload latency”, “Pruned bytes”.

## Testing Strategy

1. **Unit tests**
   - Mock `ArchiveProvider` to simulate success/failure.
   - Receipt verification & signature tests.
   - Pruning policy evaluation (immediate prune vs. optional delayed configs).
2. **Integration tests (`ultramarine-test`)**
   - Extend `full_node` harness to spin up a fake load-s3-agent (hyper server) and ensure proposer uploads.
   - Roundtrip test: submit block with blobs, archive, gossip receipts, prune immediately after receipts + finality.
3. **End-to-end harness**
   - Compose stack with real load-s3-agent container (already in repo).
   - Scenario: kill proposer before upload to confirm restream/retry path.
4. **Failure injection**
   - Simulate provider returning 500 to test exponential backoff.
   - Corrupt receipt pointer to ensure followers refuse to prune.

## Open Questions & Future Work

- **Shared duties:** Should we allow committee-based archiving (multiple validators) for redundancy? V0 sticks to “proposer only” per requirement.
- **On-chain attestation:** Future phases might anchor archive receipts on-chain for user-facing proofs.
- **Provider diversity:** Introduce multi-provider strategy (e.g., S3 + Arweave) with quorum attestation.
- **Archiver rewards/penalties:** Integrate with staking economics so proposers are rewarded for timely archival or penalized for failure.
- **Data confidentiality:** load-s3-agent currently stores public data; encryption or ACLs may be required later.

## Feasibility Statement

Nothing in the existing Ultramarine or load-reth codebases blocks this design:

- Blob bytes are already persisted and addressable per height/index, so uploading them is straightforward.
- Stores and consensus already separate metadata (kept forever) from blob data, enabling safe pruning once receipts exist.
- load-s3-agent provides a usable REST API; we only need to wrap it in a provider client.
- Malachite’s networking stack already supports reliable point-to-point sync suitable for disseminating receipts.

Therefore the Phase 6 archiver/pruner V0 is achievable with incremental changes described above. The key risks lie in operational robustness (provider availability, retries), which we mitigate through metrics and retry policies.
