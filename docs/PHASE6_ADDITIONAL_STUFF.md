# Phase 6 – Additional Stuff (Follow-ups / Improvements)

This document lists pragmatic follow-ups on top of the current Phase 6 archive→prune pipeline.

Assumed contract:

- Ultramarine nodes do **not** re-download pruned blob bytes.
- After pruning, blob bytes are out-of-protocol; nodes only propagate/persist **locators** and metadata.
- External consumers (indexers, rollups, provers, explorers) use locators/tags to retrieve and verify bytes.

## Recommended additions

### 1) Put `versioned_hash` into `ArchiveNoticeBody` (signed)

Today `load.versioned_hash` is included as an upload tag to the provider, but it is not part of the signed notice/record.

Add a new field to `ArchiveNoticeBody`:

- `versioned_hash: B256`

Why:

- Makes `ArchiveNotice`/`ArchiveRecord` self-sufficient for external consumers.
- Avoids relying on provider indexing/tag-query to map `(height,index)` or `kzg_commitment` → `versioned_hash`.

### 2) Add network binding to the notice signing preimage

Add a chain/network discriminator into the signed notice body (one of):

- `chain_id: u64` (preferred if stable), or
- `network: String` (e.g. `fibernet`)

Why:

- Prevents cross-network replay/confusion if locators/tags are reused across devnets/testnets.
- Aligns with existing upload tags like `load.network=fibernet`, but makes it cryptographically bound.

### 3) Expose archive records for external tooling (CLI/RPC)

Provide an explicit interface to export archive records for height `H` (and optionally for `versioned_hash`):

Return shape (conceptually):

- `height`, `blob_index`
- `kzg_commitment`, `versioned_hash`, `blob_keccak`
- `provider_id`, `locator`
- `archived_by`, `archived_at`

Why:

- External consumers should not scrape logs or reverse-engineer internal DBs.
- This is the clean “handoff surface” for the AO/HyperBEAM machinery to consume.

### 4) Reference retriever/indexer (external, not in-node)

Create a small reference tool/library that:

- takes `versioned_hash` (or `(height,index)` + commitment),
- discovers `dataitem_id` via either:
  - `ArchiveNotice` locator, or
  - `load-s3-agent /tags/query` as a fallback,
- downloads bytes via the gateway,
- verifies bytes against:
  - `ArchiveNoticeBody.blob_keccak`
  - `ArchiveNoticeBody.kzg_commitment` (recompute commitment)

Why:

- Removes ambiguity for integrators.
- Demonstrates the “blob economics machinery” workflow without changing consensus.

### 5) Manual “re-archive height H now” and “rebroadcast notices” tooling

Add operator-facing commands to:

- enqueue an archive job for a specific height (proposer-only),
- rebroadcast stored archive notices for a height.

Why:

- Operational recovery when provider endpoints/tokens change.
- Useful for staged rollouts and incident response.

### 6) Multi-provider / failover (optional)

Allow configuring multiple providers and/or mirroring:

- upload to primary and optionally mirror to secondary,
- store multiple locators per blob index (or store a primary + alternates).

Why:

- Provider outages shouldn’t permanently stall pruning/operations.
- Lets AO/HyperBEAM and other mirrors coexist.

### 7) Better throughput observability (per-provider attribution)

Current Phase 6 metrics are global. Consider adding labels by `provider_id` (or separate registries) for:

- upload success/failure
- upload bytes
- upload latency histograms

Why:

- Required if you introduce mirroring or multiple providers.
- Makes Grafana actionable when debugging throughput regressions.

## Explicit non-goals (by design)

- Node-side automatic rehydration of pruned blob bytes from the provider.
  - If a blob is pruned, the node treats the bytes as not needed for protocol operation.
  - Locators are a handoff for external consumers only.
