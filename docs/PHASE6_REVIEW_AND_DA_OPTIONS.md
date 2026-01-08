# Phase 6 Review + “Beyond 4844/PeerDAS” DA Options (Load / Ultramarine)

**Scope**: Review Phase 6 completeness + assess foundations, then propose concrete ways Load can push DA capacity far beyond Ethereum’s EIP-4844 + PeerDAS constraints, leveraging Tendermint-style BFT (Malachite) and the existing “consensus-on-metadata, data-on-side-channel” design.

Primary references:

- `docs/FINAL_PLAN.md` (project status + phase boundaries)
- `docs/PHASE5_TESTNET.md` and `docs/PHASE5_SUMMARY.md` (Phase 5 sign-off + validated behavior)
- `docs/PHASE6_ARCHIVE_PRUNE_FINAL.md` (Phase 6 “what’s implemented” spec)

---

## 1) Phase 6 completeness (what’s shipped vs. sign-off gaps)

### What Phase 6 _implements now_ (V0)

Per `docs/PHASE6_ARCHIVE_PRUNE_FINAL.md`, Phase 6 is real, end-to-end, and source-linked:

- **ArchiveNotice protocol**: proposer signs an `ArchiveNotice` per blob after upload; validators verify and persist an `ArchiveRecord`.
- **Proposer-only acceptance**: receivers reject notices not signed by the _expected proposer_ for that height (prevents “random validator publishes locator” ambiguity).
- **Prune gating**: prune local blob bytes for a height only when:
  1. height is “finalized” by the app’s finality tracking, and
  2. all blob indices at that height have verified archive records.
- **Restart recovery**: pending uploads / pending prunes are rehydrated on restart.
- **Operational strictness**:
  - production nodes fail fast if archiver is enabled but misconfigured;
  - validators refuse to start with `archiver.enabled=false` (so any validator can safely take proposer duty).
- **Test coverage**:
  - Tier 0: archive notice handling + gating smokes;
  - Tier 1: multi-node follower pruning, retries, auth propagation, restart recovery (`make itest-node-archiver`).

### Remaining gaps for a clean “Phase 6 sign-off”

These are explicitly called out in `docs/PHASE6_ARCHIVE_PRUNE_FINAL.md` as “Remaining to add/expand”:

1. **Negative-path harness coverage**:
   - invalid signature
   - non-proposer notices (should be rejected)
   - conflicting notices (same `(height, blob_index)` with different locator/provider)
2. **Retention policy clarity**:
   - “no retention window” currently applies to _blob bytes_ only; consensus store history pruning is separate and should be intentionally configured.
3. **Manual retry / ops UX**:
   - no manual retry CLI yet (useful for provider outages / stuck uploads).
4. **Provider-strengthening is optional but important**:
   - V0 binds locators to blob bytes via `blob_keccak`, but there is no provider receipt / provider signature requirement.

**Assessment**: Phase 6 is “implemented and usable”, but still “V0 hardening” rather than a final production-grade DA pipeline. The missing items are mostly _safety hardening and operability_, not architecture.

---

## 2) Foundations assessment (what you already have that’s strategically strong)

### The core design is correct for a DA-oriented sovereign chain

Ultramarine’s most important structural choice is already aligned with both Tendermint-style BFT and Ethereum blob semantics:

- **Consensus commits to small metadata** (commitments/hashes) rather than hauling blob bytes in votes.
- **Data moves on an app-defined side channel** (`ProposalPart::BlobSidecar` / `ProposalPart::ArchiveNotice`) without modifying Malachite core.
- **Validity is app-enforced**: the app refuses to finalize/import without required blob conditions (availability checks, versioned hash checks, etc.).

This matches the natural Tendermint separation: consensus finalizes a block ID; the application decides what “valid block” means and can gate commits on additional data conditions.

### Where the current foundation will hit scaling limits

Right now, validators ultimately behave like “full download” participants:

- For safety, they tend to **fetch/verify full blob bytes** (proposal sidecars or sync packages) before considering the block valid.
- That means validator bandwidth still scales with total DA throughput, which caps you well before “10–100×”.

Phase 6’s archive/prune pipeline improves _storage sustainability_, but it does not yet create a _scalable availability protocol_ (it mainly ensures “bytes can be moved out of RAM/disk after finality, and recovered elsewhere”).

### One key technical constraint to keep explicit

EIP-4844 (Ethereum) provides:

- fixed-size blobs (131,072 bytes),
- KZG commitments/proofs,
- an ecosystem that expects those semantics.

Load can remain compatible with EIP-4844 transactions while still defining **additional DA commitments and availability rules** at the consensus/app layer. “Compatibility” does not mean “inherit Ethereum throughput constraints.”

---

## 3) DA options that can plausibly deliver a 10×–100× leap

Below are options ordered by “highest leverage per complexity”, with notes on how they fit the existing Ultramarine architecture (proposal streaming + metadata consensus + blob engine).

### Option A — Deterministic “BFT Custody” with erasure coding (high leverage, simpler than PeerDAS)

**Idea**: instead of every validator downloading every blob, the proposer erasure-codes the blob bundle into many shards. Each validator is deterministically assigned a subset of shards (based on height/round + validator index) to download, store, and attest to.

- **Consensus commits**:
  - existing EIP-4844 commitments (for compatibility),
  - plus a **DA data root** (Merkle root of erasure-coded shards).
- **Validators do**:
  - download only their assigned shards,
  - verify shard inclusion via Merkle proofs,
  - sign a “custody attestation” for `(height, data_root, shard_indices)`.
- **Block validity rule**:
  - block is valid once attestations cover a threshold of shards (e.g. “≥ 2f+1 validators attest to their assigned shards”).

**Why this can be 10–50×**: per-validator bandwidth becomes ~`total_data / n` (or `/ (n * replication_factor)`) instead of `total_data`.

**How it uses Malachite/BFT well**: validator sets are explicit and stable enough to coordinate deterministic assignments; BFT signatures can provide fast finality on availability attestations.

**Main design choice**: deterministic shard assignment (coverage-guaranteed) vs. probabilistic sampling (coverage-probabilistic).

**How this fits Ultramarine today (integration sketch)**:

- Extend Layer 2 metadata (`BlobMetadata`) with `da_data_root` (and optionally `da_params`, e.g. `k`, `n`, chunk size).
- Add new streamed parts under the existing proposal streaming channel:
  - `ProposalPart::DaShard` (shard bytes + Merkle proof against `da_data_root`)
  - optionally `ProposalPart::DaAttestation` (validator signature over `(height, da_data_root, shard_indices)`), or ship attestations via the existing value-sync package path.
- Update the app’s “valid block” rule so a block is only finalized/imported once the app observes an **availability certificate**:
  - simplest form: 2f+1 signed attestations that each validator received its assigned shards;
  - more aggressive: an explicit “coverage map” proving enough unique shards exist to reconstruct.
- Storage:
  - keep full blob bytes optional (only for archivers/indexers),
  - keep shards in a prunable store keyed by `(height, shard_index)`; Phase 6’s pruning machinery generalizes naturally to “delete local shards once archived/replicated”.

### Option B — True sampling (PeerDAS-like) but tuned for BFT sets (max scaling, higher complexity)

**Idea**: implement sampling of erasure-coded data with proofs and aggregate attestations, but tune it to a _small-ish BFT validator set_.

- Similar commitment surface as Option A (a DA data root + sampling proofs).
- Instead of fixed custody assignments, validators sample pseudo-random shard indices.

**Why this can be 50–100×**: validators download only `O(log N)` shards while maintaining high confidence of reconstructability.

**Complexity**: significantly higher, especially if you want KZG-based sampling proofs (Ethereum-style) rather than Merkle proofs (Celestia-style). A pragmatic approach is:

- Merkle+ReedSolomon for DA distribution,
- keep KZG for EIP-4844 transaction compatibility and full verification by full nodes.

### Option C — Multi-writer archival with cryptographic receipts (robustness + ops, enables aggressive pruning)

Phase 6 makes the proposer the sole uploader and sole accepted notice signer. That is clean, but it creates a liveness dependency:

- if the proposer’s upload fails, pruning (and potentially “metadata-only sync”) is delayed.

Two upgrades:

1. **Provider receipts**: the external store signs a receipt binding `(blob_keccak, locator, provider_id, expiry)`; validators require receipt validity before accepting an archive record.
2. **Fallback uploaders**: after a timeout, allow _non-proposers_ to upload and present a provider receipt, but keep the “proposer is canonical locator authority” by:
   - allowing the proposer to sign _multiple_ locators over time, or
   - allowing a 2f+1 validator quorum to sign a “recovery locator” if the proposer is offline.

This doesn’t increase raw throughput, but it **enables much shorter on-node retention windows** safely, which becomes important once DA volume is large.

### Option D — Multiple DA “lanes” with distinct rules (throughput and UX)

Define multiple blob/DA classes:

- **Lane 0 (Ethereum-compatible)**: EIP-4844 blobs + KZG commitments, conservative policy.
- **Lane 1 (native DA)**: larger effective payloads via erasure-coded “superblobs”, aggressive throughput targets, different fee market.

Consensus commits to per-lane roots; the blob engine becomes a “pluggable DA engine” with per-lane verification/storage/retention.

This keeps compatibility while letting the protocol evolve beyond Ethereum’s constraints.

---

## 4) Practical roadmap suggestions (what to do next)

### Phase 6 hardening (finish V0 production sign-off)

- Add the missing negative-path integration scenarios described in `docs/PHASE6_ARCHIVE_PRUNE_FINAL.md`.
- Decide on and document the exact “finalized” semantics used for pruning under Tendermint finality assumptions.
- Add an operator retry surface (CLI or RPC) for stuck uploads.

### Phase 7+ (turn “archive/prune” into “real DA”)

If the goal is a step-function DA increase, prioritize Option A first:

1. Implement **erasure coding + DA root** for blob bundles.
2. Implement **deterministic custody assignments** + attestations.
3. Gate “valid block” on an **availability certificate** derived from BFT signatures.
4. Only then consider probabilistic sampling (Option B) if you need another order-of-magnitude.

---

## 5) Open questions to resolve early (they strongly affect design)

1. **Validator set size & churn**: how big do you expect the active set to be, and how frequently does it change?
2. **DA threat model**: are you optimizing for “validators are the DA layer” (like Celestia) or “validators just finalize, DA is outsourced” (like pure archival providers)?
3. **Light client goals**: do you want light clients to verify DA availability (sampling proofs), or is DA primarily for full nodes/apps?
4. **Compatibility boundary**: do you require that _all_ DA be expressible as EIP-4844 blobs, or can you introduce a native DA transaction format/lane?
