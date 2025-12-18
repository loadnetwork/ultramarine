# Infra Progress (Ultramarine + load-reth)

This doc is the living plan + progress log for multi-host testnet/bootnet automation.

Goal: make it easy, safe, and reproducible to generate a “network bundle”, deploy it to N machines over SSH, run Ultramarine + load-reth, and observe/operate the network.

---

## Executive summary (what we’re building)

- **Manifest-driven infra**: `ultramarine/infra/manifests/<net>.yaml` is the single source of truth for CL+EL topology/config.
- **Two canonical inputs**: manifest + `ultramarine/infra/networks/<net>/secrets.sops.yaml`; everything else is derived.
- **Atomic unit is a paired node**: one Ultramarine (CL) + one load-reth (EL) per instance, co-located on the same host.
- **Engine is local-only**: default **Engine IPC**; HTTP+JWT is an explicit opt-in and must bind localhost only.
- **Validators require archiver**: archiver config + bearer token are baseline requirements for validator nodes (generator enforces).
- **Strict persistence**: never wipe home dirs / WAL / DBs / keys unless explicitly requested.
- **Reproducible deploys**: generator emits `ultramarine/infra/networks/<net>/network.lock.json`; deploy/ops consume the lockfile and fail fast on drift.

Path note: this repo keeps infra under `ultramarine/infra/`. If extracted into a standalone “ops” repo later, drop the `ultramarine/` prefix in paths.

---

## Dev handoff (how to start building this)

- Treat `ultramarine/infra/manifests/<net>.yaml` + `ultramarine/infra/networks/<net>/secrets.sops.yaml` as the only human-edited inputs; everything else is derived output.
- Build in dependency order (so work parallelizes cleanly):
  1) **Manifest + validation**: define schema + add `ultramarine/infra/manifests/example.yaml` + implement `netgen validate` (reject unsafe failure-domain layouts unless explicitly allowed).
  2) **Lockfile**: define `network.lock.json` schema (include `schema_version` + tool version + resolved placements/ports/peers + artifact checksums) and ensure generation is deterministic.
  3) **Generator**: implement `ultramarine/infra/gen/netgen` (Rust) to emit `network.json`, `network.lock.json`, `bundle/public/` + `bundle/private/`, and an Ansible inventory from the lockfile (no dev defaults).
  4) **Deploy**: implement Ansible+systemd consumption of the lockfile/bundle; add `make net-deploy/net-up/net-health`.
- PR safety checklist (must hold for every infra change):
  - No secrets committed (private bundle stays gitignored; secrets only via SOPS + host-local root-owned files).
  - Engine API never exposed publicly (HTTP binds `127.0.0.1`; IPC preferred).
  - Deploy does not wipe validator/EL state unless an explicit wipe/rotate is requested.

---

## Reality check (constraints we must respect)

- **BFT liveness math matters**: if you run **4 validators across 2 machines** (2 validators per machine), losing one whole machine removes 2 validators, leaving `2/4` voting power online. With Malachite/Tendermint-style BFT, the default quorum threshold is **strictly `> 2/3` of total voting power**, so the chain **halts** (for `n=4`, quorum is `floor(2n/3)+1 = 3`).
  - Best-practice rule of thumb: if you want the network to survive losing any single failure-domain unit (e.g. one host), that unit must hold **< 1/3** of total voting power. With equal-weight validators, that means `validators_per_host <= floor((n-1)/3)` (equivalently `validators_per_host < n/3`).
  - Code reality: Malachite’s default quorum is `ThresholdParam::TWO_F_PLUS_ONE` with strict `>` semantics (`min_expected(total)` computes `1 + floor(2/3 * total)`) in `malachite/code/crates/core-types/src/threshold.rs`. Ultramarine uses default threshold params via `ConsensusParams { threshold_params: Default::default(), .. }` in Malachite app spawn code (`malachite/code/crates/app/src/spawn.rs`).
- **CL↔EL pairing is non-optional**: each Ultramarine node must reach a co-located Engine API endpoint on the same host.
  - Infra default: **Engine IPC** (local-only; Ultramarine does not require JWT for IPC), i.e. load-reth `--auth-ipc` / `--auth-ipc.path=...` and Ultramarine `--engine-ipc-path=...`.
  - HTTP Engine is supported but must be localhost-only and guarded by a shared `jwtsecret` file (same secret for CL and EL). (Note: local Docker may bind Engine to `0.0.0.0` for container networking; infra must not.)
  - Code reality: Ultramarine supports `--engine-http-url`, `--engine-ipc-path`, `--eth1-rpc-url`, `--jwt-path` (`ultramarine/crates/cli/src/cmd/start.rs`) and selects IPC over HTTP when both are provided (`ultramarine/crates/node/src/node.rs`). JWT is only read/required for HTTP Engine (`ultramarine/crates/node/src/node.rs`).
  - Code reality: Ultramarine has local-dev fallback defaults for Engine/Eth RPC if endpoints are not provided (moniker `test-0/1/2` → localhost ports) in `ultramarine/crates/node/src/node.rs`; infra must always provide explicit endpoints and must not rely on these dev defaults.
  - Code reality: Ultramarine performs fail-fast preflight checks for Engine capabilities and Eth RPC reachability during startup (`ultramarine/crates/node/src/node.rs`).
- **Archiver is mandatory for validators** (current Ultramarine behavior): validator nodes must have archiver enabled + configured (provider URL/ID + bearer token), or they will fail fast on startup.
  - Code reality: validator enforcement + strict archiver config validation live in `ultramarine/crates/node/src/node.rs` (`validate_validator_archiver_setting`, `validate_archiver_config_strict`). Operator contract and alerting guidance are documented in `ultramarine/docs/ARCHIVER_OPS.md`.
- **No “manual peer wiring”**: local docker workflow can use `admin_addPeer`, but multi-host must rely on explicit peer/bootnode configuration and correct dialable addresses.
  - Code reality: local-only peer wiring script exists at `ultramarine/scripts/add_peers.sh`. Multi-host should instead rely on stable identities + explicit bootnodes/persistent peers (see `ultramarine/compose*.yaml` for the devnet pattern).
- **Static host addressing (MVP)**: assume stable public IP/DNS per host. If IPs churn, regenerate and redeploy the lock/bundle.

---

## Principles (non-negotiable)

- **Single source of truth**: one manifest (`ultramarine/infra/manifests/<net>.yaml`) drives both CL and EL artifacts.
- **Only two human-edited inputs** (canonical state):
  - `ultramarine/infra/manifests/<net>.yaml`
  - `ultramarine/infra/networks/<net>/secrets.sops.yaml`
  Everything else is derived output and must be safe to delete/regenerate.
- **Bundle split**:
  - `public/` is safe to commit/share.
  - `private/` is never committed. It is derived output produced from the canonical inputs (manifest + encrypted secrets) and should be treated as an ephemeral deploy artifact.
    - If you must persist `private/` for handoffs, store it outside git (artifact store) and encrypt it (e.g. age); do not create a second “secrets source of truth” in the repo.
- **Secrets are handled as files (not flags)**: prefer root-owned files with `0600` perms (systemd `EnvironmentFile=` or `LoadCredential=`); Ansible tasks touching secrets must use `no_log: true`.
  - Code reality: Ultramarine’s archiver currently consumes secrets via env overrides (`ULTRAMARINE_ARCHIVER_*`) in `ultramarine/crates/cli/src/config_wrapper.rs`, so the “file” in practice is a root-owned environment file loaded by systemd.
- **Node is the atomic unit**: one logical node is always a **CL+EL pair** (Ultramarine + its paired load-reth).
- **Engine API is local-only**: Engine API must never be exposed publicly; bind to `127.0.0.1` (HTTP) or use IPC.
- **State persistence invariant**: redeploy must not overwrite node state unless explicitly requested (e.g. `--wipe` / `--rotate`).
  - Ultramarine validator state includes (at minimum): `<home>/wal/consensus.wal`, `<home>/store.db`, `<home>/blob_store.db` (RocksDB dir), and `<home>/config/{config.toml,genesis.json,priv_validator_key.json}`.
    - Code reality: Ultramarine opens `store.db` and `blob_store.db` under `home_dir` in `ultramarine/crates/node/src/node.rs`. Malachite WAL is created at `<home>/wal/consensus.wal` in `malachite/code/crates/app/src/spawn.rs`.
    - Code reality: Ultramarine derives its libp2p identity keypair from the validator private key (`ultramarine/crates/node/src/node.rs` `get_keypair`), so validator key material is also network identity material.
  - load-reth state includes its datadir and its P2P secret key (stable enode identity). If HTTP Engine is used, the `jwtsecret` file is also required state for the pair.
- **Provider-agnostic deploy**: deploy layer works anywhere with SSH (Latitude, AWS, DO, …).
- **Pinned versions**: prod-like runs pin binaries/images by version (prefer digests).
- **No config drift**: generated artifacts are derived; humans edit only the manifest.
- **Unsafe layouts require explicit acknowledgement**: the generator should refuse unsafe failure-domain layouts (e.g. “2/4 validators on one host”) unless the manifest explicitly allows halting on that failure.
- **Lockfile-based reproducibility**: generator emits `ultramarine/infra/networks/<net>/network.lock.json`; deploy/ops consume the lock (not raw manifest) so two operators get identical results.
- **Signer safety is explicit**: document and enforce the signer safety persistence contract (Malachite/Ultramarine-specific), so ops can’t accidentally create double-sign conditions via redeploy/reset.
  - Malachite’s crash-safety mechanism is the WAL actor, stored at `<home>/wal/consensus.wal` (see `malachite/code/crates/app/src/spawn.rs`). This file (and the node’s home directory generally) must be treated as critical validator state.

---

## Repo layout (target)

```
ultramarine/infra/
  gen/
    netgen/        # generator implementation (script or binary)
  manifests/
    <net>.yaml
  networks/
    <net>/
      secrets.sops.yaml
      network.json
      network.lock.json
      bundle/
        public/
        private/   # gitignored deploy artifact (derived)
      inventory.yml
  ansible/
    playbooks/
      deploy.yml
      up.yml
      down.yml
      status.yml
      logs.yml
      health.yml
    roles/
      common/
      ultramarine/
      load-reth/
      observability/
  templates/
    systemd/
      ultramarine@.service
      load-reth@.service
    env/
      ultramarine.env
      load-reth.env
```

Notes:
- Keep this tree self-contained so it can be extracted to a dedicated “ops” repo later if needed.
- Local dev workflows (`make all`, `make all-ipc`) stay as-is; infra is for multi-host.
- Secret material should live encrypted at `ultramarine/infra/networks/<net>/secrets.sops.yaml`; `bundle/private/` is a generated deploy artifact and is always treated as private.
- `network.lock.json` is the resolved, deterministic artifact used for deploy/ops (versions, ports, placements, peer lists, checksums).

---

## MVP scope (2 machines, 2 nodes each)

### Generate

- Input: `ultramarine/infra/manifests/<net>.yaml`
- Output:
  - Ultramarine node homes (config + genesis + keys, per node)
  - load-reth config per node (ports, datadir, p2p key, bootnodes; if HTTP Engine is selected, provision `jwtsecret` and wire both CL+EL to the same file)
  - `network.json` manifest (machine-readable inventory: endpoints, roles, ports)
  - `network.lock.json` (resolved, deterministic: versions/digests, placements, ports/endpoints, peer lists, checksums)

### Deploy

- Target runtime: **systemd + binaries** (preferred for multi-host reliability).
- Deploy method: **Ansible** over SSH.
- Health checks:
  - CL: process up + metrics reachable
  - EL: process up + JSON-RPC reachable
  - CL↔EL: Engine API reachable locally (HTTP or IPC)
  - Network: peer connectivity best-effort (at least one peer)
  - Progress: “height is moving” check (not just open sockets)
  - Safety: Engine API is not exposed publicly
  - Archiver (validators): archiver enabled, queue not growing unbounded, and uploads succeeding (see `ultramarine/docs/ARCHIVER_OPS.md`)

### Observe

- MVP requirement: metrics endpoints exist and `net-health` can detect stalls (“height not moving”) without needing a full monitoring stack.
- Baseline for public testnet UX: generate Prometheus scrape config + minimal dashboards/alerts for “height stalled”, “no peers”, “disk pressure”, “archiver backlog”.
  - Deployment mode (per-host vs central) is a profile choice; do not make Grafana’s local DB a source of truth.
  - Reuse existing local-dev assets as the baseline and generate a multi-host variant from the lockfile: `ultramarine/monitoring/prometheus*.yml` and `ultramarine/monitoring/config-grafana/`.

---

## Plan (milestones + tasks)

### M0 — Scaffolding

- [ ] Create `ultramarine/infra/` tree structure (dirs + placeholder files).
- [x] Add safe-by-default `.gitignore` for `bundle/private/`, keys, jwt secrets, `secrets.env`.
- [x] Add a secrets/keys policy doc (`ultramarine/infra/SECRETS.md`).
- [ ] Document prerequisites for operators/devs (Ansible, SSH, etc.).

### M1 — Manifest schema (network.yaml)

- [ ] Define schema fields (minimum viable):
  - [ ] network identity: `name` and human tags (e.g. `description`, `labels`)
  - [ ] execution genesis identity: `chain_id` (+ any additional genesis knobs we actually support)
    - Note: today `ultramarine-utils genesis` uses timestamp `0` and hardfork times at `0` for dev/testnets (`ultramarine/crates/utils/src/commands/genesis.rs`); support for “real timestamp genesis” is a generator feature, not a given.
  - [ ] archiver network tag (if desired): today `load.network` is hardcoded to `fibernet` in `ultramarine/crates/node/src/archiver.rs`; making it configurable requires a code change.
  - [ ] `schema_version` (so we can evolve config safely)
  - [ ] `profile`: `bootnet` | `public-testnet` | `mainnet`
    - [ ] profiles only set defaults (exposure, pinning strictness, sync defaults, logging), without changing the mental model
  - [ ] hosts (inventory facts, not consensus data):
    - [ ] `hosts[].id`, `hosts[].ssh_host`, `hosts[].ssh_user`
    - [ ] `hosts[].public_ip` (and optional `hosts[].private_ip`)
  - [ ] topology intent:
    - [ ] `failure_domain`: e.g. `host` (for now)
    - [ ] `allow_halt_on_failure_domain_loss`: `true|false` (explicit acknowledgement knob)
      - [ ] **Evaluation rule** (machine-checkable): when `false`, the generator must reject any layout where removing *any single* failure-domain unit (e.g. one host) would drop **online validator voting power below Malachite quorum** (strict `> 2/3`, see `malachite/code/crates/core-types/src/threshold.rs`). When `true`, allow such layouts but emit a prominent warning in generated artifacts.
  - [ ] instance list (atomic CL+EL pairs): `id`, `role` (validator/full), `host`, `index`
    - [ ] (future-proof) optional `voting_power` (default 1), so failure-domain/quorum checks can evolve without redesign
  - [ ] address/port contract:
    - [ ] CL P2P uses Malachite `listen_addr` + `persistent_peers` (multiaddrs). There is no explicit “advertise_addr” knob today in Malachite config; infra should generate dialable multiaddrs (static public IP/DNS) in `persistent_peers`.
    - [ ] EL P2P uses load-reth `--nat=extip:<public_ip>` when needed and stable P2P keys for stable enodes.
    - [ ] bind policy (`public`, `private`, `localhost`) per service (Engine always `localhost`/IPC)
  - [ ] port allocation mode: `by-index` vs `by-host-block` (host-local port blocks)
  - [ ] CL P2P transport selection (tcp/quic)
  - [ ] CL sync settings (ValueSync enabled + timeouts) for real networks
  - [ ] CL↔EL wiring (per node):
    - [ ] Engine transport: `ipc` or `http`
    - [ ] `engine_ipc_path` or `engine_http_url` (localhost/private only)
    - [ ] `eth1_rpc_url` (usually localhost/private)
    - [ ] `jwt_secret_ref` (private) and `jwt_path` wiring (if HTTP Engine auth)
  - [ ] archiver config (enabled/provider/token reference)
    - [ ] validated rule: `role=validator` implies archiver is enabled and configured (provider_url/id + token reference). Generator rejects missing/invalid archiver config for validators, matching Ultramarine startup strictness.
- [ ] logging/metrics exposure policy
- [x] Decide “public vs private” mapping per field (manifest is public; keys/tokens land in `bundle/private/`).
- [x] Implement the secrets mechanism early enough to unblock M2 (SOPS + age; netgen supports `sops -d`).
- [x] Add an example manifest: `ultramarine/infra/manifests/example.yaml`.

### M2 — Bundle generator (local)

- [x] Implement `ultramarine/infra/gen/netgen` (**Rust binary**; workspace package `ultramarine-netgen`).
  - netgen is the canonical generator for infra (it may call existing CL/EL generators internally, but infra does not rely on ad-hoc/manual generation flows).
  - [ ] Emits remote-ready Ultramarine node homes + `config.toml` directly from the manifest/lock (no dev-only defaults).
    - [ ] enable ValueSync for distributed/public testnets
    - [ ] enforce CL↔EL endpoint wiring per node (no “test-0/1/2 defaults” assumptions)
    - [ ] enforce TCP transport by default (unless explicitly configured)
    - [ ] do not rely on `ultramarine testnet` / `ultramarine distributed-testnet` output for multi-host; netgen owns remote-ready config generation.
  - [x] Emits EL genesis/chainspec deterministically (shared library `ultramarine-genesis`; `bundle/public/genesis.json`).
  - [x] Emits per-node *rendered runtime config*:
    - [x] systemd env files for Ultramarine (`--engine-*`, `--eth1-rpc-url`, `--jwt-path`, `ULTRAMARINE_ARCHIVER_*`)
    - [x] systemd env files for load-reth (ports, nat/extip, bootnodes/static peers)
  - [x] Derives stable EL `enode://…` bootnodes from generated EL P2P keys and emits them into the lockfile (no runtime `admin_nodeInfo` scraping in the happy path).
  - [ ] Eliminates “dev-only peer wiring” dependencies:
    - [ ] do not rely on `cast/jq/docker` peer-add scripts
    - [ ] configure EL peering via static bootnodes/trusted peers from the manifest
- [x] Produces `network.json` and `inventory.yml` (Ansible-friendly).
- [x] Produces `ultramarine/infra/networks/<net>/network.lock.json` (host placement, ports/endpoints, derived peer lists, artifact checksums).
- [x] Deploy/ops consume the lockfile and fail fast if the manifest has changed without regenerating the lock (manifest sha drift check in `infra/ansible/playbooks/deploy.yml`).
- [x] Add `make net-gen NET=<net>` target.

### M3 — Ansible deploy/run (systemd)

- [x] `roles/common`: docker deps, base dirs, systemd unit templates.
- [x] `roles/common`: hard requirements for reliability
  - [x] time sync (chrony)
  - [x] `LimitNOFILE` (systemd units), `fs.file-max` (sysctl)
- [x] `roles/load-reth`:
  - [x] pinned container image + systemd wrapper (host network, Engine IPC)
  - [x] lays down datadir, p2p key, env file
  - [x] Engine IPC uses stable `RuntimeDirectory` (`/run/load-reth/<node>/engine.ipc`)
  - [x] RPC/metrics bind to localhost by default
- [x] `roles/ultramarine`:
  - [x] pinned container image + systemd wrapper (Engine IPC)
  - [x] lays down per-node env + archiver token env (0600) + home skeleton
  - [x] never overwrites `priv_validator_key.json` on redeploy
- [x] `playbooks/deploy.yml` and `make net-deploy`.
- [x] Add operator commands:
  - [x] `make net-up`, `make net-down`, `make net-status`, `make net-logs`, `make net-health`
  - [x] `net-health` checks “height is moving” plus service/IPC checks
- [x] `net-deploy` restarts services by default to apply new artifacts/env (opt-out supported via `RESTART_ON_DEPLOY=false`)
- [x] `net-up` is restart-safe (always restarts instances to apply current env/artifacts).
- [x] Deploy verifies copied `genesis.json` sha256 against `network.lock.json`.
- [x] Add `net-doctor` preflight (fast diagnostics): time sync, limits, unit presence, and (optional) listener checks.
- [ ] MVP acceptance checklist (automation or a documented runbook step):
  - [ ] all nodes reach height `H`
  - [ ] restart one node → it catches up to `H`
  - [ ] CL↔EL coupling: each node can build + import at least one block via Engine API
  - [ ] EL JSON-RPC reachable on expected endpoints
  - [ ] CL metrics reachable and reports peers connected

### M4 — Hardening (testnet-ready)

- [ ] Version pinning policy:
  - [ ] ultramarine git SHA / release tag
  - [ ] load-reth image digest
- [ ] Secret handling:
  - [ ] implement **SOPS + age** end-to-end (Vault is optional later)
  - [ ] define a minimal operator flow:
    - [ ] generate age keypair per operator; keep private key only on the control machine(s)
    - [ ] add operator public keys as recipients in `ultramarine/infra/networks/<net>/secrets.sops.yaml`
    - [ ] require `sops` on the control machine; decrypt locally and copy secrets to hosts with `no_log: true`
  - [ ] ensure secrets never appear in logs (no flags, no stdout, no Ansible diffs)
- [ ] Key lifecycle:
  - [ ] do not overwrite keys/state on redeploy (unless explicit `--rotate` / `--wipe`)
  - [ ] include stable EL P2P keys for stable enodes (bootnode stability)
- [ ] Signer safety state:
  - [ ] document the Malachite/Ultramarine signer safety persistence contract (what must persist across restarts/redeploys)
  - [ ] ensure deploy does not wipe signer safety state unless explicitly forced (unsafe op)
- [ ] Restart policy:
  - [ ] rolling restarts guidance (don’t restart too much voting power at once)
- [ ] Firewall/ports runbook (provider-agnostic).
- [x] Firewall/ports runbook (provider-agnostic) (`infra/FIREWALL.md`).
- [x] Firewall automation (UFW) from `network.lock.json` (`make net-firewall` or `APPLY_FIREWALL=true`).
- [x] Add `make infra-checks` (netgen build + ansible syntax-checks if available).

### M5 — Scale workflows

- [ ] Add “add full node” workflow (new machine, no re-genesis):
  - [ ] generate node N+1 artifacts
  - [ ] deploy only that node
- [ ] Add “upgrade” workflow:
  - [ ] planned halt mode (optional)
  - [ ] rolling upgrade mode (when protocol/compat allows)

---

## Decisions log

- **Runtime for multi-host**: systemd is the supervisor; artifacts can be pinned binaries or pinned container images (both must preserve the same host layout + persistence invariants).
- **Secrets mechanism**: use **SOPS + age** (default).
  - Rationale: works well in public repos (encrypted-at-rest), supports multiple recipients, plays nicely with Ansible, and doesn’t require a central secrets server.
  - Fallbacks: Ansible Vault is acceptable for small teams; cloud secret managers (AWS SSM/Secrets Manager, etc.) can be added later without changing the bundle/public-private model.
- **Lockfile**: deploy consumes `ultramarine/infra/networks/<net>/network.lock.json` and fails fast on manifest/lock drift.
- **CL transport for remote**: default TCP; QUIC is an explicit opt-in (firewall/NAT considerations).
- **Engine transport default**: IPC (HTTP+JWT only if explicitly selected).
- **Archiver policy**: validators must have archiver configured (provider URL/ID + token), matching current Ultramarine startup strictness.
- **Quorum math (Malachite)**: strict `> 2/3` voting power (`ThresholdParam::TWO_F_PLUS_ONE`), so 3 validators require 3/3 to make progress (`malachite/code/crates/core-types/src/threshold.rs`).
- **Topology for 2 machines**: testnet-only; accept that losing one host may halt if it removes ≥2 validators.
- **Not using `distributed-testnet`**: infra generator owns multi-host config generation; `ultramarine distributed-testnet` is treated as a legacy/dev-only artifact.

---

## Progress log (append-only)

- 2025-12-18: Initialized infra planning doc (`ultramarine/infra/infra_progress.md`).
- 2025-12-18: Added explicit multi-host constraints (quorum/liveness, CL↔EL pairing, no manual peering).
- 2025-12-18: Incorporated canonical inputs, profiles, lockfile, and signer safety contract into the infra plan.
- 2025-12-18: Reviewed against current Malachite/Ultramarine/load-reth code; clarified strict quorum math, Engine IPC vs HTTP+JWT, archiver mandatory behavior, and on-disk state invariants.
- 2025-12-18: Added infra executive summary; clarified systemd supervision model and TCP default; documented SOPS/age workflow in `ultramarine/infra/SECRETS.md`.
- 2025-12-18: Implemented Rust `netgen` at `ultramarine/infra/gen/netgen/` and wired `make net-validate` / `make net-gen` to produce deterministic `ultramarine/infra/networks/<net>/network.lock.json`, `bundle/public/{genesis.json,network.json}`, and `inventory.yml`.
- 2025-12-18: Added stable load-reth P2P key generation under `ultramarine/infra/networks/<net>/bundle/private/` and derived `enode://…` bootnodes without runtime `admin_nodeInfo` scraping.
- 2025-12-18: Extracted EL genesis generation into `ultramarine-genesis` (`ultramarine/crates/genesis/`) and refactored `ultramarine-utils genesis`/`spam` to reuse the shared library.
- 2025-12-18: Implemented M3 deploy layer: Ansible roles + playbooks, systemd unit templates (Docker + Engine IPC), Makefile operator targets, and health checks for service status, Engine IPC socket, and “height moving”.
- 2025-12-18: Added deploy-time drift checks: Ansible refuses to deploy if the manifest sha differs from `network.lock.json`.
- 2025-12-18: Fixed netgen inventory output to be a standard static YAML inventory (no dynamic `_meta`), added `net-doctor`, and documented firewall/ports guidance.
