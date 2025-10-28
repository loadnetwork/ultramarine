# Ultramarine Blob Integration – Progress Snapshot (2025-10-28)

## Executive Overview
- **Core blob sidecar pipeline is functional end-to-end**: execution layer produces blobs via Engine API v3, consensus carries only lightweight metadata, blob engine verifies/stores data, and restream rebuilds headers on demand.
- **Phases 1–5.2** of the FINAL_PLAN are delivered. Remaining work shifts to integration validation (Phase 5.3) and future enhancements (pruning, archiving, metrics).
- **Quality gates**: `cargo fmt --all`, `cargo clippy -p ultramarine-consensus`, and `cargo test -p ultramarine-consensus --lib` (23/23 tests) all succeed.

## Phase-by-Phase Status
| Phase | Date | Highlights |
|-------|------|------------|
| **1. Execution Bridge** | 2025-10-27 | Engine API v3 wired through `ultramarine-execution`; blob bundles retrieved alongside execution payloads; execution payload header extraction added to `ValueMetadata`. |
| **2. Three-Layer Metadata Architecture** | 2025-10-27 | Introduced `ConsensusBlockMetadata` (Layer 1), `BlobMetadata` (Layer 2), and blob engine persistence (Layer 3). Added redb tables (`consensus_block_metadata`, `blob_metadata_{undecided,decided}`, `blob_metadata_meta`) and protobuf codecs. |
| **3. Proposal Streaming** | 2025-10-28 | `ProposalPart::BlobSidecar` added with protobuf serialization; proposer/receiver paths stream blobs over `/proposal_parts`; restream helper hooks introduced. |
| **4. Cleanup & Storage Finalisation** | 2025-10-28 | Removed legacy header persistence (`BLOCK_HEADERS_TABLE`, storage helpers, state/node APIs). Consensus now relies exclusively on `BlobMetadata`; parent-root validation and restream rebuilds read metadata on demand. |
| **5.1 / 5.2 Consensus & Sync Fixes** | 2025-10-23 | Proposer now stores its own blobs before gossip; restream handler sealed; synced-value protobuf path corrected; Decided handler verifies blob availability. |

## Test & Tooling Coverage
- **Unit Tests**: 23/23 consensus tests (Store: 9, State: 14) covering metadata lifecycle, round cleanup, metadata fallback, restream reconstruction, proposer height-0 cases.
- **Clippy/Fmt**: `cargo fmt --all`, `cargo clippy -p ultramarine-consensus` (no warnings). Remaining clippy items exist only in upstream crates (`ultramarine-types`, `ultramarine-blob-engine`) from earlier TODOs.
- **Blob Engine**: existing suite (`cargo nextest run -p ultramarine-blob-engine`) continues to pass; no header persistence remains inside the engine.

## Architecture Snapshot (Post-Phase 4)
- **Consensus Layer**
  - Stores `ConsensusBlockMetadata` (Layer 1) and `BlobMetadata` (Layer 2) in redb.
  - `last_blob_sidecar_root` cache hydrated from decided metadata on startup; updated only after commit.
  - `commit_cleans_failed_round_blob_metadata` ensures undecided entries are removed in tandem with blob engine cleanup.
- **Restream Path**
  - Loads stored metadata + undecided blobs, rebuilds `BlobSidecar`s with freshly signed headers derived from `BlobMetadata`.
  - Uses corrected proposer address handling (`stream_proposal`/`make_proposal_parts`).
- **Blob Engine**
  - Verifies KZG proofs, stores undecided blobs keyed by (height, round), promotes to decided at commit, exposes `get_undecided_blobs` for restream.
- **Execution Layer**
  - `generate_block_with_blobs` returns payload + blobs bundle, feeding consensus’ metadata extraction.

## Remaining Work (Next Steps)
1. **Integration Validation (Phase 5.3)**
   - Restart survival test: ensure decided metadata reload produces correct parent roots.
   - Multi-validator blob sync testnet: validate proposer/receiver/committer flows under multi-node conditions.
   - Restream replay scenarios to verify metadata fallback and reconstruction in practice.
2. **Operational Visibility**
   - Add metrics for undecided/decided metadata counts, pointer hits, and blob promotions.
   - Consider lightweight dashboards/logging for sidecar health during testnet runs.
3. **Future Phases**
   - Phase 6: pruning policy (expired blob/metadata cleanup windows).
   - Phase 7 (optional): archival/export pipeline.
   - Phase 8: broader system tests once pruning/archiving land.

## References
- [PHASE4_PROGRESS.md](PHASE4_PROGRESS.md) — detailed logs & daily notes, including the 2025-10-28 cleanup entry.
- [docs/FINAL_PLAN.md](FINAL_PLAN.md) — updated master plan with per-phase outcomes.
- [STATUS_UPDATE_2025-10-21.md](STATUS_UPDATE_2025-10-21.md) — earlier consensus+sync fix logs (Phase 5.1/5.2).

---
*Prepared by Codex CLI – 2025-10-28*
