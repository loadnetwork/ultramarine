# Phase 6 – Blob Archiving & Pruning (V0 Overview, current)

This file is the human-friendly overview of Phase 6.

For the code-audited, source-linked specification of what is implemented today, see:

- `docs/PHASE6_ARCHIVE_PRUNE_FINAL.md`
- `docs/ARCHIVER_OPS.md` (operator runbook)

## What Phase 6 does (current behavior)

- Blobs are verified and stored locally while they are needed for proposal/sync.
- After commit, the **proposer** uploads each decided blob (per `blob_index`) to an archive provider (V0: `load-s3-agent`) via `POST /upload` (multipart).
- The proposer signs and broadcasts an `ArchiveNotice` per blob containing an opaque `locator` (typically `load-s3://<dataitem_id>`).
- Validators verify the notice against locally computed blob keccak + decided metadata, persist an `ArchiveRecord`, and pruning becomes eligible.
- Once all blob indexes for a height have verified archive records and the height is finalized, blob bytes are deleted locally; metadata + archive records remain forever.

## Provider API (Load S3 Agent)

Ultramarine uses `POST /upload` only (there is no `/upload/blob` endpoint).

Multipart fields:

- `file`: raw blob bytes (EIP-4844 blobs are 131072 bytes)
- `content_type=application/octet-stream`
- `tags`: JSON array of `{key,value}` objects (ANS-104 tags)

Tags attached by Ultramarine:

- `load=true`
- `load.network=fibernet`
- `load.kind=blob`
- `load.height`, `load.round`, `load.blob_index`
- `load.kzg_commitment`, `load.versioned_hash`, `load.blob_keccak`
- `load.proposer`, `load.provider`

The provider returns `dataitem_id` (and sometimes `locator`). Ultramarine stores a locator as `load-s3://<dataitem_id>` when a locator is not explicitly returned.

## Retrieval (after pruning)

After pruning, Ultramarine will not serve blob bytes locally. Peers receive metadata + archive notices (locators), but Ultramarine does not automatically re-download pruned blobs.
External consumers (indexers, rollups, provers, explorers) can fetch from the provider using the locator.

### Receiver behavior example (Ultramarine node)

When an Ultramarine node receives a value-sync/restream package for height `H` and it is `MetadataOnly`, it should:

1. Accept and process `archive_notices` (verify signature/proposer/commitment/hash, persist `ArchiveRecord`).
2. Continue consensus sync without attempting to obtain blob bytes (bytes are out-of-protocol after prune).
3. Treat locators as an external hint for third-party tools:
   - expose the locator via logs/telemetry/CLI (optional future work), or
   - allow an external consumer to fetch blob bytes from the archive provider and verify them off-node.

Example decision flow:

- Received: `SyncedValuePackage::MetadataOnly { value, archive_notices }`
  - Node: store `value` + `archive_notices`; mark blob bytes for `H` as unavailable locally
  - External consumer: for each notice, use `notice.body.locator` (e.g. `load-s3://<dataitem_id>`) to download bytes and verify against:
    - `notice.body.kzg_commitment` / `load.versioned_hash`
    - `notice.body.blob_keccak`

Rust sketch (node-side; no re-download):

```rust
match synced_package {
    SyncedValuePackage::Full { value, blobs } => {
        // normal path: store value + blob bytes, verify, etc.
        store_value(value).await?;
        store_blobs(blobs).await?;
    }
    SyncedValuePackage::MetadataOnly { value, archive_notices } => {
        // pruned path: store metadata + archive locators, but do NOT fetch bytes.
        store_value(value).await?;

        for notice in archive_notices {
            state.handle_archive_notice(notice.clone()).await?;

            tracing::info!(
                height = %notice.body.height,
                blob_index = notice.body.blob_index,
                locator = %notice.body.locator,
                "received archive locator for pruned blob (external consumers may fetch)"
            );
        }
    }
}
```

For Load S3 Agent, a standard resolver URL is:
`https://gateway.s3-node-1.load.network/resolve/<dataitem_id>`

## Local testing quickstart

- Tier-0 component tests: `make itest` (or `make itest-quick`)
- Tier-1 full-node scenarios: `make itest-node`
- Tier-1 archiver/prune suite: `make itest-node-archiver`
- Note: the Tier‑1 harness defaults `archiver.enabled=false`; archiver tests opt in via `FullNodeTestBuilder::with_archiver(...)` / `with_mock_archiver()` (`mock://` requires `ultramarine-node` built with `feature="test-harness"`).
- Harness stability: panic-safe teardown (`Drop`), port allocation retries, and read-only store opens reduce flakiness across CI and under load.
- Spam blobs across EL RPCs: `make spam-blobs`
  - Uses different signers per RPC process to avoid nonce contention.

## Status notes (implementation deltas vs early design drafts)

- Archive notices are carried as `ProposalPart::ArchiveNotice` and are packaged for sync/restream.
- Archiver uploads use multipart `tags` (ANS-104) rather than custom header conventions.
- Retention is “archive-gated”: prune waits for verified archive records (no time-window deletion in V0).
- `archiver.enabled` means “this node uploads + emits notices when proposer duty”; prune eligibility is independent and depends only on verified notices + finality.
