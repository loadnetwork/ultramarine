# Infra (multi-host) tooling

This folder contains the manifest-driven infra scaffolding for multi-host
Load Network deployments.

Primary references:

- `infra/README.md` (this document) for operator workflows and commands.
- `docs/FINAL_PLAN.md` for architecture context.
- `docs/knowledge_base/p2p-sync-limits.md` for size-limit tuning under load.

## Netgen

Netgen is a Rust binary in the workspace (source at `infra/gen/netgen/`).

- Validates a manifest:
  - `cargo run -p ultramarine-netgen --bin netgen -- validate --manifest infra/manifests/<net>.yaml`
- Generates `infra/networks/<net>/network.lock.json` + public bundle outputs:
  - `cargo run -p ultramarine-netgen --bin netgen -- gen --manifest infra/manifests/<net>.yaml --out-dir infra/networks/<net>`
- Optionally provides per-node secrets (plaintext or sops-encrypted):
  - `cargo run -p ultramarine-netgen --bin netgen -- gen --manifest infra/manifests/<net>.yaml --out-dir infra/networks/<net> --secrets-file infra/networks/<net>/secrets.sops.yaml`
  - By default, `gen` fails if any validator is missing an archiver bearer token (required for a bootable testnet). For non-bootable dry-runs (e.g. to bootstrap storage), use `make net-plan NET=<net>` (or pass `--allow-missing-archiver-tokens` directly).
  - To create the encrypted file from a plaintext `secrets.yaml`, use:
    - `make net-secrets-encrypt NET=<net> SOPS_AGE_RECIPIENT=<age1...> REMOVE_PLAINTEXT=true`

Generated outputs (current):

- `infra/networks/<net>/network.lock.json` (deterministic lockfile: placements, ports, enodes/bootnodes, artifact checksums)
- `infra/networks/<net>/bundle/public/genesis.json` (EL genesis; Prague/Cancun at genesis)
- `infra/networks/<net>/bundle/public/network.json` (machine-readable network description)
- `infra/networks/<net>/inventory.yml` (Ansible inventory)
- `infra/networks/<net>/bundle/private/load-reth/p2p-keys/<node>.key` (stable EL identity; never commit)
- `infra/networks/<net>/bundle/private/env/ultramarine-<node>.env` (runtime variables for systemd/Ansible)
- `infra/networks/<net>/bundle/private/env/load-reth-<node>.env` (runtime variables for systemd/Ansible)
- `infra/networks/<net>/bundle/private/ultramarine/secrets/<node>.env` (archiver bearer token env file; derived from secrets; never commit)
- `infra/networks/<net>/bundle/private/monitoring/grafana_admin_password.env` (Grafana admin password; base64-encoded, derived from secrets or auto-generated on deploy; never commit)
- `infra/networks/<net>/bundle/private/ultramarine/homes/<node>/config/{config.toml,genesis.json,priv_validator_key.json}` (Ultramarine home skeleton; `priv_validator_key.json` is generated for every node; only `role=validator` nodes are in the genesis validator set; never commit)

Notes:

- Deploys are **Engine IPC-only**.
- Validators require archiver config; bearer tokens are expected via decrypted
  secrets in `infra/networks/<net>/secrets.sops.yaml`.
- If `blockscout.enabled=true` in the manifest, `net-deploy` / `net-launch` will also deploy Blockscout + nginx on the configured host.

## Ansible Design Choices (Intentional)

- Services are managed as systemd units that invoke `docker run` directly for explicit host-level control.
- Secrets are managed via SOPS instead of ansible-vault to match team workflows and rotation practices.
- Some checks still use `command/shell` where no reliable module exists (e.g., UFW or socket inspection).

## Deploy (M3, systemd + Docker)

Ansible is the deploy layer. It copies artifacts to hosts and installs systemd units that run pinned container images.

Host layout:

- Network artifacts: `/opt/loadnet/networks/<net>/...`
- Active network symlink: `/opt/loadnet/current -> /opt/loadnet/networks/<net>`
- Persistent EL state: `/var/lib/load-reth/<node>/`
- Persistent CL state: `/var/lib/ultramarine/<node>/`
- Engine IPC: `/run/load-reth/<node>/engine.ipc`

Operator commands (from `ultramarine/`):

- Set default network (so you can omit `NET=`): `make net-use NET=<net>` (clears via `make net-unset`)
- If you prefer not to set a default, append `NET=<net>` to any command below.
- Dry-run / bootstrap (no secrets yet): `make net-plan` (generates inventory/lockfile/bundles but the network won’t be bootable without archiver tokens for validators)
- Generate artifacts: `make net-gen` (auto-uses `infra/networks/<net>/secrets.sops.yaml` if present)
- One-command bootstrap (plan + storage + pre-doctor): `make net-bootstrap`
- One-command go-live (gen + storage + deploy + post-doctor + health): `make net-launch`
- One-command update (gen + apply + roll + health): `make net-update`
- Deploy to hosts (default: ensures services are running; no restarts if already running): `make net-deploy`
- Apply + restart immediately (disruptive): `make net-redeploy`
- Rolling restart (disruptive; may halt small nets): `make net-roll ROLL_CONFIRM=YES`
- Start/restart: `make net-up` / Stop: `make net-down`
- Inspect: `make net-status` / `make net-logs LINES=200`
- Log cleanup (vacuum journald; optional syslog rotation/truncation): `make net-clean-logs JOURNAL_VACUUM_SIZE=1G`
- Health: `make net-health`
- Preflight (pre-deploy): `make net-doctor-pre`
- Diagnostics (post-deploy): `make net-doctor`
- Firewall: `make net-firewall` (or `make net-deploy APPLY_FIREWALL=true`)
- Storage bootstrap: `make net-storage` (see notes below)
- Wipe network from hosts (destructive): `make net-wipe WIPE_CONFIRM=YES` (tune with `WIPE_STATE=true|false`, `WIPE_MONITORING=true|false`, `WIPE_CONTAINERS=true|false`, `WIPE_FIREWALL=true|false`, `WIPE_NODES=node-0`, and `LIMIT=<host_id>`)
- Limit any Ansible run to a single host: add `LIMIT=<host_id>` (e.g. `make net-storage LIMIT=lon2-0`)
- SSH key: pass `SSH_KEY=/path/to/key` (or use ssh-agent / `~/.ssh/config`).
- Local checks: `make infra-checks`

Notes:

- Controller requirement: use an Ansible version compatible with your controller Python.
  - `ansible-core 2.15.x` is not compatible with Python `3.14+`. If you used `pipx` and see errors involving `ast.Str`, reinstall Ansible with Python 3.11/3.12 (example: `pipx reinstall --python python3.12 ansible-core`).
- Storage bootstrap is intentionally separate from deploy.
- `net-gen` auto-uses `infra/networks/<net>/secrets.sops.yaml` if present. To skip secrets, run `make net-gen SECRETS_FILE=`.
- Optional per-network defaults: create `infra/networks/<net>/net.mk` (Makefile syntax) to avoid long `VAR=...` overrides. Example:

```make
APPLY_FIREWALL = true
MOVE_DOCKER_DATAROOT = true
BIND_VAR_LOG = true
PROMETHEUS_BIND = 127.0.0.1
GRAFANA_BIND = 127.0.0.1
EL_HTTP_BIND = 0.0.0.0
SSH_KEY = ~/.ssh/your_key
```

- If your host image enforces a broken apt proxy, set `APT_DISABLE_PROXY=true` (writes `/etc/apt/apt.conf.d/99loadnet-no-proxy`).
- In non-destructive mode, `net-storage` expects the data volume to be mounted at `DATA_MOUNTPOINT` (default: `/var/lib/loadnet`).
- If your provider image mounts the data volume elsewhere (common: `/home`), `net-storage` auto-adopts it by bind-mounting `DATA_SOURCE_DIR` (default: `/home/loadnet`) into `DATA_MOUNTPOINT` and persists it in `/etc/fstab` with systemd mount ordering (`x-systemd.requires-mounts-for=/home`).
- Destructive provisioning requires explicit device IDs and an explicit flag, e.g.:
  - `make net-storage NET=<net> STORAGE_WIPE=true DATA_DEVICES="['/dev/disk/by-id/nvme-...','/dev/disk/by-id/nvme-...']" DATA_RAID_LEVEL=1 MOVE_DOCKER_DATAROOT=true`
- You don’t need to care about the mdadm “device number” (e.g. `/dev/md127`): the playbook defaults to creating `/dev/md/loadnet-data` and mounts by filesystem UUID in `/etc/fstab`. The only per-host detail is selecting the underlying NVMe devices (by-id is safest); discover them with e.g. `ssh <host> 'ls -la /dev/disk/by-id | grep nvme | grep -v part'`.
- `net-deploy` fails fast if `infra/manifests/<net>.yaml` changed without regenerating `network.lock.json`.
- To deploy without restarting running nodes: `make net-deploy NET=<net>` (or `make net-apply NET=<net>`)
- To restart after a deploy: `make net-roll NET=<net> ROLL_CONFIRM=YES` (recommended over restarting all at once)
- `net-deploy` verifies `bundle/public/genesis.json` checksum against the lockfile on each host.
- Firewall automation is idempotent, keeps SSH allowed, and opens only P2P ports by default.
- For non-`example` networks, `netgen validate` rejects placeholder archiver URLs (e.g. `archiver.example.com`).

## Logging and disk pressure

Defaults (applied by Ansible):

- Journald is capped to protect the root disk (defaults: 2G total, 200M files, 7 days). If you need different values, override `loadnet_journal_max_use`, `loadnet_journal_max_file_size`, or `loadnet_journal_max_retention` in your Ansible vars.
- Docker logs use the `local` driver with rotation (defaults: `max-size=50m`, `max-file=5`).
- If you enable journald forwarding to syslog (`loadnet_forward_journal_to_syslog=true`), rsyslog logrotate policy is installed (defaults: 100M × 5).
- Doctor checks fail if `/` > 85% or `/var/log` > 4GB by default. Override with `loadnet_root_fs_max_pct` and `loadnet_log_dir_max_mb`.

Operational tips:

- Keep `/var/log` off the root disk by running `make net-storage BIND_VAR_LOG=true` (optionally set `LOG_DIR=`).
- Recovery shortcut: `make net-clean-logs JOURNAL_VACUUM_SIZE=1G` (vacuum journald; rotates/truncates syslog only when forwarding is enabled).

## Accessing Monitoring (Grafana / Prometheus)

For security, Grafana and Prometheus bind to `127.0.0.1` (localhost only) by default. Access them via SSH tunnel:

```bash
# Grafana (port 3000)
ssh -L 3000:127.0.0.1:3000 ubuntu@<host-ip>
# Then open http://localhost:3000 in your browser

# Prometheus (port 9090)
ssh -L 9090:127.0.0.1:9090 ubuntu@<host-ip>
# Then open http://localhost:9090 in your browser

# Both at once (different local ports if needed)
ssh -L 3000:127.0.0.1:3000 -L 9090:127.0.0.1:9090 ubuntu@<host-ip>
```

To override binding (not recommended for production):

- Grafana: `make net-deploy NET=<net> GRAFANA_BIND=0.0.0.0`
- Prometheus: `make net-deploy NET=<net> PROMETHEUS_BIND=0.0.0.0`

## Rotating Archiver Secrets

To rotate archiver bearer tokens without regenerating validator keys:

```bash
# 1. Update your secrets.sops.yaml with new tokens
# 2. Regenerate bundle (keys are preserved)
make net-update-secrets NET=<net> SECRETS_FILE=infra/networks/<net>/secrets.sops.yaml

# 3. Deploy the updated secrets
make net-deploy NET=<net>
```
