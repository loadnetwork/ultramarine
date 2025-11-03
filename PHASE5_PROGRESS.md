# Phase 5: Testnet & Observability — Progress Log

**Status**: 🟡 In Progress  
**Focus**: Metrics instrumentation & testnet readiness for blob sidecars  
**Last Updated**: 2025-11-03

---

## 2025-11-03 (Monday) — Metrics Plan Review & Dashboard Reset

- ✅ Restored Grafana provisioning from malaketh baseline (`monitoring/config-grafana/provisioning/`), dashboards now load with the original panel set.
- 🔍 Verified current panels rely only on core `app_channel_*` and Reth metrics; blob-specific queries intentionally absent until instrumentation lands.
- 📄 Reviewed `docs/METRICS_PROGRESS.md` and aligned it with the codebase:
  - Metric table maps directly onto `BlobStore` and consensus (`State::commit`, `State::rebuild_blob_sidecars_for_restream`) touch points, with separate counters for success/failure to stay within the `malachitebft` metrics API.
  - Key design notes flag the need to capture serialized blob sizes when inserting/pruning so storage gauges stay accurate; store helpers will have to return byte counts before promotion/pruning.
  - Instrumentation checklist now calls out consensus hooks (commit, restream rebuild, proposal assembly) so dashboards can correlate Tendermint lifecycle events with blob activity once emitted.
- 🧭 Confirmed the metrics plan reuses the existing `SharedRegistry` wiring in `node.rs`, keeping it compatible with current Prometheus/Grafana setup; future Grafana panels just need the updated metric names (`*_success_total`, `*_failure_total`, etc.).

### Next Actions
1. Implement `BlobEngineMetrics` module under `crates/blob_engine`, following the `DbMetrics` pattern.
2. Thread metrics registration through `node/src/node.rs` and enable `.with_metrics()` on the blob engine/state constructors.
3. Instrument consensus paths (`State::assemble_and_store_blobs`, `State::commit`, restream rebuilds) so blob metrics carry height/round context.
4. After metrics land, reintroduce the blob dashboard row with live queries (see `docs/METRICS_PROGRESS.md` for panel hints).

---

## References
- `docs/PHASE5_TESTNET.md` — phase implementation plan
- `docs/METRICS_PROGRESS.md` — detailed metrics specification & checklist
- `monitoring/config-grafana/provisioning/` — restored baseline dashboards
