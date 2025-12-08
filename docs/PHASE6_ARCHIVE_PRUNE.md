# Phase 6 – Blob Archiving & Pruning (V0 Design)

**Goal:** add a minimal, correct, and observable archiving + pruning loop for blob sidecars so Ultramarine can keep consensus metadata while evicting blob bytes after they are durably copied to off-chain storage (initially `load-s3-agent`).

**Scope:** consensus-layer changes only (Ultramarine + Malachite wiring). Execution client (load-reth) is unchanged except for potential RPC diagnostics. The design keeps the consensus/execution split intact and assumes the existing blob engine/storage tables from Phases 1‑5. **Archival is asynchronous:** consensus finalizes first and stores blobs locally (as in Phase 5); archival happens later in a background task with retries, never blocking consensus or import. **Retention window is removed in V0:** blobs are pruned only after they are archived; no height-based deletion. Operators may add a safety delay later if desired.

**Serving contract:** Hot serving (proposal restream, `engine_getBlobsV1`) applies only to non-archived blobs. Once a blob is marked `Archived`, the node either prunes immediately or returns an explicit “archived/unavailable” error; callers must use the archival locator for historical retrieval. Consensus metadata + locator are retained indefinitely.

## 1) Principles & Constraints
- **Safety first:** never prune blob bytes unless the archive record is durable and locally acknowledged. Metadata must remain forever.
- **Single responsible party:** the **block proposer** for a decided block is the only validator allowed/obligated to archive its blobs (simplifies trust and avoids double-uploads).
- **On-chain-ish attestation:** the proposer emits a signed archive notice that every validator persists. No extra BFT vote is needed in V0; liveness relies on proposer honesty, safety relies on local verification against commitments and the proposer’s signature on the notice.
- **Asynchronous archival:** consensus and EL handoff run exactly as today; archive uploads happen later via background jobs and may lag several heights. Pruning merely waits for receipts + retention.
- **Determinism:** archival state changes are driven by consensus height/round, not wall-clock jitter; retries/backoff never reorder receipts for a given height.
- **Minimal dependencies:** storage provider is pluggable; V0 uses `load-s3-agent` via HTTP+Bearer token.

## 2) Roles
- **Proposer (height H):** uploads each blob sidecar to `load-s3-agent`, signs an `ArchiveNotice` per blob (or batch), and gossips it. Must retry until acknowledged locally.
- **Validators:** verify notice (height/round/proposer/address matches metadata + KZG commitment), persist archival pointer, and mark blob eligible for pruning when policy allows.
- **Pruner:** deterministic task that evicts blob bytes once `archived = true` and retention rules are satisfied; metadata + archival pointer are retained.

## 3) Data Model & State Machine
- Extend `BlobMetadata` (Layer 2) with:
  - `archival_status: Pending | Archived { locator, archived_at_height, signer, blob_keccak }`
  - `pruned: bool`
- New store table: `blob_archival` keyed by `(height, blob_index)` → `ArchivalPointer` (locator, signer, timestamp, blob_keccak). The decided path already collapses `(height, round)` into `height`, so receipts align with the canonical block.
- State transitions per blob:
  1. `Pending` (existing) → `Archived` (on verified notice) → `Pruned` (on retention trigger).
  2. Metadata is never deleted; `pruned` only affects sidecar bytes.

## 4) Message Surface (V0)
- **ArchiveNotice** (new app-level message on the consensus app channel; no change to ordered `ProposalPart` stream):
  - `height, round, proposer_address, blob_index`
  - `kzg_commitment` (or versioned hash) and `blob_data_keccak` to bind the uploaded bytes
  - `locator` (opaque string from `load-s3-agent`, e.g., DataItem ID or HTTPS URL)
  - `signature` by proposer over the tuple above (Ed25519, same validator set lookup as proposal verification)
- Verification by recipients:
  - Load decided `BlobMetadata` for `height`; ensure the stored proposer/commitments match the notice.
  - Recompute `blob_data_keccak` from their local RocksDB entry; if it differs, reject.
  - Validate the proposer’s signature against the validator set.
  - On success: persist the archival pointer keyed by `(height, index)` and set `archival_status=Archived`.
- Rationale: no extra BFT round; safety is maintained because pruning only depends on matching commitment + known proposer. The message stays small and asynchronous from proposal streaming/restream.

## 5) Happy-Path Flow
1. **Block decided:** existing Phase 5 code marks blobs `Decided` and persists metadata keyed by height. Nothing blocks on archive completion.
2. **Archiver queue:** after commit, if the local validator was the proposer for height `H`, enqueue an `ArchiveJob { height: H, blob_indices }`. Jobs run asynchronously on a dedicated task; queue depth and lag are exported as metrics.
3. **Proposer archives:** the worker fetches blobs from the blob engine, POSTs each sidecar to `load-s3-agent`, records the returned `locator`, and computes `blob_data_keccak`. Once the upload succeeds and the notice is persisted locally, transition to `archival_status=Archived`. Failed uploads stay `Pending` and retry with exponential backoff.
4. **Broadcast:** the proposer gossips `ArchiveNotice` messages (app channel) so peers learn the locator + hash. Consensus flow is unaffected if notices arrive late; pruning simply waits.
5. **Peers verify + persist:** each validator recomputes the blob hash, validates the proposer, and records the locator. When every blob index at height `H` is `Archived`, the height becomes prune-eligible (subject to retention).
6. **Pruning window:** when every blob is archived and the block is decided/finalized, the pruner deletes blob bytes (undecided/decided store) while retaining metadata + archival pointers. Hot serving is disabled for archived/pruned blobs; callers must use the locator.

## 6) Retention Policy (V0)
- **No time/height window:** prune immediately after a blob is archived and the block is decided/finalized. There is no height-based deletion. This maximizes proposer incentive to archive quickly to free space.
- Prune only if:
  - `archival_status=Archived`
  - Block is decided/finalized
- If notice missing → never prune (bounded risk: disk growth; mitigated by alerts).
- Serving semantics:
  - Non-archived blobs: served via restream and `engine_getBlobsV1`.
  - Archived-but-not-yet-pruned: either still served (graceful) or return `Archived` to force archive usage—pick and document. V0 can continue serving until pruned.
  - Archived + pruned: not served; return explicit `archived/unavailable` error so callers use the locator.

## 7) Provider Contract (`load-s3-agent`)
- Use `/upload` (public) or `/upload/private` with Authorization: Bearer `<token>`.
- Inputs: raw blob bytes (131072 bytes) + tags:
  - `height`, `round`, `blob_index`, `kzg_commitment`, `versioned_hash`, `proposer`
  - `content_type=application/octet-stream`
- Outputs: JSON `{ dataitem_id, message, offchain_provenance_proof?, custom_tags? }`. Use `dataitem_id` as the `locator` and derive a URL as `https://gateway.s3-node-1.load.network/resolve/{dataitem_id}`. Private uploads additionally need `bucket_name` / `x-dataitem-name`.
- The service does not return a checksum, so Ultramarine computes `blob_data_keccak` locally and places it in the notice for integrity.
- Archival retries: proposer retries locally (async task) until success; exponential backoff with max attempts per block so consensus threads are not blocked.
- Privacy: for now public uploads; private bucket path requires headers (`x-bucket-name`, `x-dataitem-name`) if needed later.

## 8) Serving Contract & Storage Changes
- **Protocol contract:** consensus/execution nodes only serve blob bytes while they are undecided/decided. Once a blob reaches `archival_status = Archived`, the node deletes the bytes and refuses to serve them over RPC/gossip—they are now “historical” and must be fetched via the archiver locator. Metadata (commitments, execution payload header, receipts) stays forever so verification remains possible.
- `BlobMetadata` protobuf schema: add optional `ArchivalPointer { locator: string, signer: bytes, blob_keccak: bytes32 }` and `pruned: bool`.
- Redb tables:
- `blob_archival` (new) or extend existing blob tables with an `archival` column keyed by `(height, blob_index)` storing locator + proposer + hash.
- `blob_metadata_meta` tracks latest archived height for quick scans.
- Blob engine API additions:
  - `mark_archived(height, index, locator, signer, blob_keccak)` — records pointer + blob hash, then deletes bytes.
  - `list_archivable(finalized_height)` → heights eligible for immediate pruning (finalized + receipts present).
  - `prune_archived_before(height_threshold)` — still used for CLI/manual cleanup but called with “threshold = finalized height + 1” once all blobs archived.

## 9) Pruner Task (deterministic)
- Runs in the consensus node task set (same runtime as blob engine):
  - Inputs: `finalized_height`, `RETAIN_BLOCKS`, and the archive index.
  - For each candidate height that satisfies retention and has all `archival_status=Archived`, delete sidecar bytes; keep metadata + archival pointers.
  - Metrics: `blob_pruned_total`, `blob_archived_total`, `blob_prune_failures_total`, `blob_pruner_backlog_height`.
- Idempotent: pruning a missing sidecar is allowed but increments a failure metric so ops notices drift.

## 10) Failure & Recovery
- **Archival upload fails:** proposer retries asynchronously; if retries exhaust, emit warning and keep `Pending`. No pruning occurs; operator can trigger manual retry.
- **Notice lost:** validators will not prune; proposer should re-broadcast until `Archived` is observed locally. Add gossip retries every N blocks until acks (optional V0: self-only).
- **Locator mismatch:** validators reject notice if commitment/proposer differs; emit `sync_failures_total`.
- **Provider unresponsive:** disk grows; ops can raise `RETAIN_BLOCKS` to avoid churn and watch `archiver_backlog_height` to know when to intervene.
- **Reorg:** blobs are tied to `(height, round)`; pruning gated on decided/finalized height prevents deleting data needed for alternative branches.

## 11) Observability
- Metrics (consensus):
  - `blob_archival_attempts_total{result=ok|err}` (proposer)
  - `blob_archived_total` (validators)
  - `blob_pruned_total`, `blob_prune_failures_total`
  - `blob_archival_lag_blocks` (decided height − last archived height)
  - `archiver_queue_len` / `archiver_backlog_height` (async worker pressure)
- Logs:
  - INFO on archive success (height, index, locator hash)
  - WARN on failed uploads/notices
- CLI/RPC:
  - `ultramarine blob status <height>` → show archived/pruned + locator
  - Optional: EL RPC add-on to fetch locator for diagnostics.

## 12) Testing Plan (V0)
- Unit: state machine transitions Pending→Archived→Pruned; reject mismatched proposer/commitment.
- Integration (existing harness):
  - Simulate proposer upload success, broadcast notice, peers mark archived, pruner evicts bytes after `RETAIN_BLOCKS`.
  - Negative: missing notice ⇒ no pruning; bad signature ⇒ reject; upload failure ⇒ stays pending.
  - Restart survival: archived/pruned flags persist across node restart.
- Smoke: run `make itest` with mock `load-s3-agent` (local HTTP stub) to validate end-to-end without external network.

## 13) Open Questions / Follow-ups
- Do we require 2/3 acknowledgements before pruning? V0 says no; V1 could add aggregated signature or vote-extension based confirmation.
- Should archive notices piggyback on vote extensions instead of proposal parts? V0 keeps a lightweight app message; final placement TBD after Malachite review.
- Retention parameter ownership: config vs. chain param. Suggest node config (ops-tunable) for V0.
- Private vs public uploads: V0 assumes public; finalize bucket strategy before mainnet.

## 14) Implementation Checklist (V0)
1. Extend proto/types: `BlobMetadata` + `ArchivalPointer`; add `ProposalPart::Archive`, update hashing/FIN verification, and update store schema to use `(height, blob_index)`.
2. Blob engine + store API changes for `mark_archived(height, index, locator, signer, blob_keccak)` and pruning with receipts.
3. Archiver queue: background worker that dequeues proposer duties, uploads via `load-s3-agent`, records locators, and emits notices.
4. Receiver path: verify notice (commitments + blob hash), persist locator, mark `Archived`.
5. Pruner task: height-based retention, delete sidecar bytes only after all receipts exist, keep metadata forever.
6. Metrics/tests/docs: cover async lag metrics, queue instrumentation, harness tests with mock provider.
