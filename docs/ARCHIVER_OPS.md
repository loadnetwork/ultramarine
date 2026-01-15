# Archiver Operations Guide

This document covers the operational aspects of Ultramarine's blob archiver, which uploads decided blob sidecars to external storage providers (e.g., Load S3 Agent) and enables local pruning after archival.

For the full Phase 6 design and its code-audited status, see `docs/PHASE6_ARCHIVE_PRUNE_FINAL.md`.

## Archive-Based Pruning Policy

**IMPORTANT**: Load Network uses the **archive event as the boundary for blob pruning**, NOT the Ethereum DA window.

| What Gets Pruned | What Gets Retained Forever |
|------------------|---------------------------|
| Blob bytes (after archive + finality) | Decided values, certificates, block data |
| | BlobMetadata, archive records/locators |

**Key invariant**: `history_min_height == 0` for all validators, enabling fullnode sync from genesis.

## Overview

The archiver is part of Phase 6 (Archive/Prune) and provides:

- **Blob archival**: Upload decided blobs to external storage providers
- **Archive notices**: Cryptographically signed receipts that prove archival
- **Archive-gated pruning**: Remove local blob bytes only after archival + finality (NOT time-based DA window)
- **Serving contract**: Return archive locators when blobs are pruned
- **MetadataOnly sync**: Peers receive execution payload + archive notices for pruned heights

## Configuration

### Config File (`config.toml`)

```toml
[archiver]
# Enable/disable the archiver worker (uploads + notice emission on proposer duty)
enabled = true

# Storage provider URL
# Production: "https://load-s3-agent.load.network"
provider_url = "https://load-s3-agent.load.network"

# Upload path appended to provider_url (default: /upload for Load S3 Agent cloud)
# Usually leave unset. Only set when your deployment mounts the route under a prefix.
# upload_path = "/upload"

# Provider identifier used in archive notices
provider_id = "load-s3-agent"

# Bearer token for authenticated uploads (obtain from Load S3 Agent)
# See: https://docs.load.network/load-cloud-platform-lcp/ls3-with-load_acc
# REQUIRED when `enabled = true`. Startup fails fast if missing.
bearer_token = "your-load-acc-api-key"

# Number of retry attempts for failed uploads (default: 3)
retry_attempts = 3

# Base backoff duration in milliseconds (default: 1000)
# Uses exponential backoff: 1s, 2s, 4s, ...
retry_backoff_ms = 1000

# Maximum jobs in queue before dropping oldest (default: 1000)
max_queue_size = 1000
```

### Environment Variables

The archiver config can also be set via environment variables:

- `ULTRAMARINE_ARCHIVER_ENABLED`
- `ULTRAMARINE_ARCHIVER_PROVIDER_URL`
- `ULTRAMARINE_ARCHIVER_UPLOAD_PATH`
- `ULTRAMARINE_ARCHIVER_PROVIDER_ID`
- `ULTRAMARINE_ARCHIVER_BEARER_TOKEN`
- `ULTRAMARINE_ARCHIVER_RETRY_ATTEMPTS`
- `ULTRAMARINE_ARCHIVER_RETRY_BACKOFF_MS`
- `ULTRAMARINE_ARCHIVER_MAX_QUEUE_SIZE`

### Environment Setup

1. Copy `.env.example` to `.env` inside the `ultramarine/` directory.
2. Set `ULTRAMARINE_ARCHIVER_BEARER_TOKEN` to your Load Cloud Platform (`load_acc`) API key. See [LS3 with load_acc](https://docs.load.network/load-cloud-platform-lcp/ls3-with-load_acc) for token issuance.
3. Run `make all` / `make all-ipc` from the `ultramarine/` directory so Docker Compose automatically loads `.env`.\
   If you run `docker compose` from the repo root, prefix the command with `env $(cat ultramarine/.env | xargs)` (or export the variables manually) so the containers inherit the credentials.

## Behavior

### When Archiver is Enabled

1. **Proposer duty**: After committing a block with blobs, the proposer enqueues an archive job
2. **Upload**: The archiver worker uploads each blob to the provider with metadata headers
3. **Notice generation**: On success, an `ArchiveNotice` is signed and broadcast to peers
4. **Verification**: Followers verify the notice (signature, commitment, blob_keccak)
5. **Pruning**: Once all blobs at a height are archived AND the height is finalized, local blobs are pruned

### When Archiver is Disabled

- **No uploads by this node**: When `archiver.enabled=false`, this node will not upload blobs when it is proposer, so it will also not emit archive notices for its own proposed heights.
- **Pruning still happens when archived**: If the node receives a complete, proposer-signed set of archive notices from the network for some height, it will prune local blob bytes for that height once finalized.
- **Validators fail fast (production)**: if this node is in the validator set and `archiver.enabled=false`, Ultramarine refuses to start (so proposers cannot silently skip upload duty). The full-node integration harness builds `ultramarine-node` with `feature="test-harness"` and may disable archiver by default for non-archiver tests.

This is useful for:

- Testing/development environments
- Nodes without external storage access (note: proposer duty will not be fulfilled if `enabled=false`)

## Security / Verification Model (V0)

### What an ArchiveNotice proves (today)

An `ArchiveNotice` is an Ed25519-signed statement that includes:

- `height, round, blob_index`
- `kzg_commitment`
- `blob_keccak` (keccak256 of locally stored bytes)
- `provider_id, locator`
- `archived_by, archived_at`

Signing preimage is domain-tagged sha256 over protobuf:
`sha256("ArchiveNoticeV0" || protobuf(ArchiveNoticeBody))`.

On receipt, validators verify:

- the signature against the validator set (using `archived_by`)
- the `kzg_commitment` and `blob_keccak` match the decided `BlobMetadata` for that height/index
- conflicting notices are rejected (same `(height, index)` but different locator/provider/hash)

### Proposer-only acceptance (enforced)

Phase 6 duty is proposer-only uploads, and the verifier now enforces the same rule when processing notices. `State::handle_archive_notice` resolves the expected proposer from `BlobMetadata.proposer_index_hint` (falling back to consensus metadata) and rejects notices whose `archived_by` differs, so only the block proposer’s signed locator is accepted.

## Operator Checklist

- If `archiver.enabled = true`, ensure `archiver.bearer_token` is set; the node fails fast on startup when missing.
- Watch `archiver_queue_len` and `archiver_backlog_height`; sustained growth means provider errors or networking issues.
- Expect `archiver_jobs_success_total` to increase on blobbed heights; `archiver_upload_failures_total` should be near 0.
- Expect `archiver_pruned_total` to increase once notices are verified and the height is finalized.

### Serving Contract

When blobs are requested (via restream or value-sync):

| Blob Status       | Response                                                        |
| ----------------- | --------------------------------------------------------------- |
| Available locally | Full blob data                                                  |
| Pruned (archived) | `MetadataOnly` package with archive notices containing locators |
| Not found         | Error                                                           |

Peers receiving `MetadataOnly` should treat blob bytes as unavailable over p2p (they were pruned on the sender).
Ultramarine does not automatically re-download pruned blobs; it only propagates and persists archive notices/locators.
External consumers (indexers, rollups, provers, explorers) can fetch blob bytes from the archive provider using the locator.

## Storage Provider Contract

Ultramarine archives blobs via `POST /upload` (multipart).

- Endpoint: `POST /upload` (multipart form)
- Fields:
  - `file` (raw blob bytes; 131072 bytes for EIP-4844)
  - `content_type=application/octet-stream`
  - `tags` (JSON array of `{key,value}`), including:
    - `load=true`
    - `load.network=fibernet`
    - `load.height`, `load.round`, `load.blob_index`
    - `load.kzg_commitment`, `load.versioned_hash`, `load.blob_keccak`
    - `load.proposer`, `load.provider`
- Header: `Authorization: Bearer <token>`

Response includes `dataitem_id` (and sometimes `locator`); Ultramarine stores a locator as `load-s3://<dataitem_id>` if one isn’t provided.

### Retrieving Archived Blobs

Load S3 Agent exposes an HTTPS gateway that serves archived blobs by `dataitem_id`.\
Use the locator returned in the archive notice (e.g. `load-s3://abc123…`), strip the prefix, and fetch via:

```
https://gateway.s3-node-1.load.network/resolve/<dataitem_id>
```

Operators should rely on this gateway (or their own mirrored buckets) when a blob is marked as pruned/unavailable in Ultramarine logs or metrics.

## Metrics

All metrics are registered under the `archiver_` prefix.

### Counters

| Metric                            | Description                                   |
| --------------------------------- | --------------------------------------------- |
| `archiver_jobs_success_total`     | Total successful archive jobs                 |
| `archiver_jobs_failure_total`     | Total failed archive jobs                     |
| `archiver_upload_failures_total`  | Total upload failures to provider             |
| `archiver_receipt_mismatch_total` | Archive notices with commitment/hash mismatch |
| `archiver_pruned_total`           | Total blobs pruned after archival             |
| `archiver_served_total`           | Total blobs served from local storage         |
| `archiver_served_archived_total`  | Total requests hitting pruned/archived status |

### Gauges

| Metric                    | Description                                 |
| ------------------------- | ------------------------------------------- |
| `archiver_queue_len`      | Current job queue length                    |
| `archiver_backlog_height` | Oldest height with pending jobs (0 if none) |
| `archiver_archived_bytes` | Cumulative bytes archived                   |
| `archiver_pruned_bytes`   | Cumulative bytes pruned                     |

### Histograms

| Metric                                | Description                           |
| ------------------------------------- | ------------------------------------- |
| `archiver_upload_duration_seconds`    | Upload latency to provider            |
| `archiver_notice_propagation_seconds` | Time from notice emission to peer ack |

## Alerting Recommendations

### Critical Alerts

1. **Archiver queue backlog**
   ```
   archiver_queue_len > 100 for 5m
   ```
   Indicates uploads are failing or provider is slow.

2. **Upload failure rate**
   ```
   rate(archiver_upload_failures_total[5m]) > 0.1
   ```
   Provider may be down or credentials invalid.

3. **Backlog height stale**
   ```
   archiver_backlog_height > 0 AND
   archiver_backlog_height unchanged for 10m
   ```
   Archive jobs are stuck.

### Warning Alerts

1. **High upload latency**
   ```
   histogram_quantile(0.95, archiver_upload_duration_seconds) > 5
   ```
   Provider or network issues.

2. **Receipt mismatches**
   ```
   rate(archiver_receipt_mismatch_total[5m]) > 0
   ```
   Possible data corruption or malicious notices.

## Troubleshooting

### Blobs Not Being Pruned

1. Check archiver is enabled: `archiver.enabled = true`
2. Check upload success: `archiver_jobs_success_total` should increase
3. Check finality: Blobs only prune after height is finalized
4. Check notice propagation: All blobs at a height must have valid notices

### Upload Failures

1. Verify `provider_url` is correct
2. Verify `bearer_token` is valid (check Load S3 Agent docs)
3. Check network connectivity to provider
4. Review logs for specific error messages

### High Queue Length

1. Provider may be slow or rate-limiting
2. Increase `retry_backoff_ms` to reduce provider load
3. Consider `max_queue_size` if queue is dropping jobs

### Recovery After Restart

On startup, `recover_pending_archive_jobs()` scans decided heights and re-enqueues any blobs that:

- Were decided by this node (proposer)
- Have not yet received valid archive notices

This ensures no blobs are lost if the node crashes mid-archival.

## Testing

For local testing, point `provider_url` at a staging provider endpoint and run the standard `make all` / `make spam-blobs` flow.
