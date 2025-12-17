# Phase 6 – Archiving & Pruning (Final V0, code-audited)

Goal: add a safe, deterministic archive→prune pipeline for blob sidecars with **no retention window for blob bytes**. Blobs are served locally until prune; after pruning they are no longer served. Consensus metadata and archive records remain forever.

This document reflects the current implementation and links each feature to the code that implements it.

**Implementation status legend**: ✅ implemented, 🟡 partial, ⚠️ mismatch / foot-gun, ⏳ not implemented.

## Summary (what the system does)

- Blobs are verified and stored locally (RocksDB) during proposal/sync.
- After commit, the proposer uploads decided blob bytes to an external provider (V0: `load-s3-agent`) using an async worker.
- The proposer signs and gossips an `ArchiveNotice` per blob (post-commit; off-FIN).
- Validators verify the notice (signature + commitment + local blob keccak), persist an `ArchiveRecord`, and once **all** blob indexes at a height have verified records **and** the height is finalized, they prune local blob bytes for that height.

## Architecture map (where to look)

- Types:
  - `crates/types/src/archive.rs` (`ArchiveNotice`, signing/verification, `ArchiveJob`)
  - `crates/types/src/blob_metadata.rs` (per-index `blob_keccak_hashes`, archive records, `pruned`)
  - `crates/types/src/proposal_part.rs` (`ProposalPart::ArchiveNotice`)
  - `crates/types/src/sync.rs` (`SyncedValuePackage` carries `archive_notices`)
- Consensus:
  - `crates/consensus/src/state.rs` (`handle_archive_notice`, `prune_archived_height`, `rehydrate_pending_prunes`)
  - `crates/consensus/src/store.rs` (`blob_archival` table + hydration into `BlobMetadata`)
  - `crates/consensus/src/archive_metrics.rs` (Phase 6 metrics)
- Node/runtime:
  - `crates/node/src/archiver.rs` (archiver worker: upload + retries + notice generation)
  - `crates/node/src/node.rs` (spawn worker, config validation, restart recovery)
  - `crates/node/src/app.rs` (broadcast notices, restream, GetDecidedValue packaging)
- Blob bytes store:
  - `crates/blob_engine/src/engine.rs` (`mark_archived`)
  - `crates/blob_engine/src/store/rocksdb.rs` (`delete_archived`)

## Protocol component: ArchiveNotice

### Payload

Per blob:

- `height`, `round`, `blob_index`
- `kzg_commitment`
- `blob_keccak` (keccak256 of the locally stored blob bytes)
- `provider_id`, `locator`
- `archived_by`, `archived_at`
- `signature` (Ed25519)

### Signature scheme (current implementation)

Signing preimage is:
`sha256("ArchiveNoticeV0" || protobuf(ArchiveNoticeBody))`

This is domain-tagged sha256 over the protobuf encoding of the notice body (not SSZ tree-hash).

Code: `crates/types/src/archive.rs` (`ARCHIVE_NOTICE_DOMAIN`, `ArchiveNoticeBody::signing_root`).

### Verification rules (current implementation)

Receiver checks:

- `archived_by` is a known validator address (used to find the public key)
- signature verifies against `archived_by` public key
- decided `BlobMetadata` exists at `height`
- `blob_index < blob_count`
- `kzg_commitment` matches decided `BlobMetadata`
- `blob_keccak` matches decided `BlobMetadata`
- no conflicting archive record already stored for `(height, blob_index)`

Code: `crates/consensus/src/state.rs` (`State::handle_archive_notice`).

✅ **Update**: Receiver-side verification now enforces proposer-only notices. `State::handle_archive_notice` resolves the expected proposer from `BlobMetadata.proposer_index_hint()` (falling back to `ConsensusBlockMetadata`) and rejects notices whose `archived_by` does not match. This guarantees only the block proposer’s signed locator is accepted even if other validators have local blob bytes (`crates/consensus/src/state.rs`).

## Storage

### BlobMetadata additions (Layer 2)

`BlobMetadata` stores:

- `blob_keccak_hashes: Vec<B256>` aligned with commitments
- `archival_records: Vec<Option<ArchiveRecord>>` (in-memory; hydrated from store)
- `pruned: bool`

Code: `crates/types/src/blob_metadata.rs`.

### Archive records table (consensus store)

Archive records are persisted in redb:

- table: `blob_archival`
- key: `(height, blob_index)`
- value: protobuf `ArchiveRecord`

On read, `BlobMetadata` is hydrated by scanning `blob_archival` for that height.

Code: `crates/consensus/src/store.rs` (`BLOB_ARCHIVAL_TABLE`, `insert_archive_record`, `hydrate_archival_records`).

### Where `blob_keccak` comes from

- Proposer path: computed from the bundle in `State::propose_value_with_blobs`.
- Sync path: computed from received sidecars in `State::process_synced_value` (Full package).

Code: `crates/consensus/src/state.rs` (`propose_value_with_blobs`, `process_synced_value`).

## Archiver worker (proposer duty)

### Duty model

- Only the proposer enqueues `ArchiveJob`s.
- Followers never upload; they only verify and persist notices.

Code:

- job building: `crates/consensus/src/state.rs` (`build_archive_job`)
- restart recovery: `crates/consensus/src/state.rs` (`recover_pending_archive_jobs`)
- worker spawn + config validation: `crates/node/src/node.rs`

### Upload contract (V0: load-s3-agent)

Ultramarine uploads via `POST /upload` (multipart form) and attaches blob metadata as tags.

Config: `crates/types/src/archiver_config.rs`
Worker implementation: `crates/node/src/archiver.rs` (`do_upload`)

Response parsing (required):

- JSON body with either `locator` or `dataitem_id`
- `success=false` fails the job
- if only `dataitem_id` is provided, locator is stored as `load-s3://{dataitem_id}`

Code: `crates/node/src/archiver.rs` (UploadResponse parsing).

### Retries and restart behavior

- Worker retries jobs with exponential backoff (capped) and keeps retrying even after “max retries” is exceeded (it logs a permanent failure but stays in the retry queue).
- On startup, pending archive jobs are recovered and enqueued again (proposer-only).

Code: `crates/node/src/archiver.rs` (retry queue), `crates/node/src/node.rs` (recovery enqueue).

⏳ Manual “re-archive this height now” CLI command is not implemented yet (restart recovery + auto retries only).

## Pruning (blob bytes)

### Prune gate (V0)

Prune a height when:

1. height is “finalized” by the app’s finality tracking, and
2. every blob index at that height has a verified archive record.

Code: `crates/consensus/src/state.rs` (`rehydrate_pending_prunes`, `flush_pending_prunes`, `prune_archived_height`).

When uploads are disabled (`archiver.enabled = false`), the node will not upload blobs when it is proposer, so it will not emit archive notices for its own proposed heights. If it receives a complete set of verified archive notices for some height from the network, it still prunes local blob bytes for that height once finalized. If this node is in the validator set, Ultramarine refuses to start with `archiver.enabled=false` (PoA strictness).

### Deletion mechanism

Deletion is per height and per blob index:

- consensus calls `blob_engine.mark_archived(height, indices)`
- blob engine deletes decided sidecar keys for those indices

Code: `crates/consensus/src/state.rs` (`prune_archived_height`), `crates/blob_engine/src/engine.rs` (`mark_archived`), `crates/blob_engine/src/store/rocksdb.rs` (`delete_archived`).

### Serving contract impact (CL)

- Before prune: blobs are served normally.
- After prune: CL serving paths return `BlobEngineError::BlobsPruned { locators }`, and restream/value-sync can ship archive notices/locators out-of-band.

Code: `crates/consensus/src/state.rs` (`get_blobs_with_status_check`, `get_undecided_blobs_with_status_check`), `crates/node/src/app.rs` (restream fallback, `SyncedValuePackage::MetadataOnly`).

### EL note (`engine_getBlobsV1`)

`engine_getBlobsV1` parity is enforced in `load-reth`, which returns `null` entries for missing/pruned blobs (no locators on Engine API).

Code: `load-reth/src/engine/rpc.rs`.

## Observability

Phase 6 metrics are registered under the `archiver_` prefix and tracked in:

- `crates/consensus/src/archive_metrics.rs`
- worker instrumentation: `crates/node/src/archiver.rs`
- notice propagation timing: `crates/node/src/app.rs`

## Tests

### Tier 0 (fast, in-process)

- `crates/consensus/tests/archiver_flow.rs`:
  - notice store/load + gating behavior (manual notice injection)
  - `archiver_enabled` gating expectations for “sync notice generation” (legacy behavior)

### Tier 1 (full-node harness; ignored by default)

- `crates/test/tests/full_node/node_harness.rs`:
  - mock provider smoke (uploads occur, pruned metadata observed)
  - multi-node follower pruning after proposer notices
  - provider failure retries
  - auth token propagation
  - restart recovery detection
  - helper config: `FullNodeTestBuilder::with_archiver(...)` / `with_mock_archiver()` keep archiver config consistent in tests (`with_mock_archiver()` uses `mock://` provider URLs and requires `ultramarine-node` built with `feature="test-harness"`, which the full-node harness enables)
  - harness hardening: panic-safe teardown (`Drop` aborts spawned node tasks), port allocation avoids TOCTOU races (retry plan), and read-only store access avoids incidental writes during polling

Run: `make itest-node-archiver` (or individual `cargo test ... -- --ignored` invocations).

🟡 Remaining to add/expand (for Phase 6 sign-off)

- Harness-level negative cases: invalid signature, non-proposer notices (if enforced), conflicting notices.
- Optional provider verification / receipt signatures (stronger than local keccak binding).

## Status checklist (Phase 6)

- ✅ `ArchiveNotice` type + signing/verifying: `crates/types/src/archive.rs`
- ✅ `ProposalPart::ArchiveNotice` transport: `crates/types/src/proposal_part.rs`
- ✅ Persistence table `blob_archival` + hydration: `crates/consensus/src/store.rs`
- ✅ Notice verification + conflict handling: `crates/consensus/src/state.rs` (`handle_archive_notice`)
- ✅ Archiver worker upload + retry/backoff: `crates/node/src/archiver.rs`
- ✅ Worker spawn + config validation + restart recovery: `crates/node/src/node.rs`
- ✅ App loop broadcasts worker notices: `crates/node/src/app.rs`
- ✅ Prune gate + deletion: `crates/consensus/src/state.rs` + `crates/blob_engine/src/engine.rs`
- ✅ Proposer-only notice acceptance enforced in `handle_archive_notice` (non-proposer signatures are rejected)
- ⏳ Manual retry CLI: not implemented

## Notes / foot-guns to keep docs honest

- ⚠️ “No retention window” applies to _blob bytes_. The consensus store still has its own pruning (`Store::prune()` in `State::commit`) that controls how much decided history is kept. These are separate mechanisms and should be configured consciously.
- ⚠️ If `archiver.enabled=true`, `archiver.bearer_token` must be set; production nodes fail fast on startup when missing.
- ⚠️ Validators are required to run with `archiver.enabled=true` in production; startup fails fast if a validator disables archiver, to prevent proposers from silently skipping upload duty. The full-node integration harness builds `ultramarine-node` with `feature="test-harness"` and may disable archiver by default for non-archiver tests.
