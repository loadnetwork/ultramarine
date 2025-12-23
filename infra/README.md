# Infra (multi-host) tooling

This folder contains the manifest-driven infra scaffolding described in `infra_progress.md`.

Docs:

- Firewall/ports guidance: `FIREWALL.md`

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
- `infra/networks/<net>/bundle/private/ultramarine/homes/<node>/config/{config.toml,genesis.json,priv_validator_key.json}` (Ultramarine home skeleton; `priv_validator_key.json` is generated for every node; only `role=validator` nodes are in the genesis validator set; never commit)

Notes:

- Deploys are **Engine IPC-only**.
- Validators require archiver config; bearer tokens are expected via decrypted secrets (see `SECRETS.md`).

## Deploy (M3, systemd + Docker)

Ansible is the deploy layer. It copies artifacts to hosts and installs systemd units that run pinned container images.

Host layout:

- Network artifacts: `/opt/loadnet/networks/<net>/...`
- Active network symlink: `/opt/loadnet/current -> /opt/loadnet/networks/<net>`
- Persistent EL state: `/var/lib/load-reth/<node>/`
- Persistent CL state: `/var/lib/ultramarine/<node>/`
- Engine IPC: `/run/load-reth/<node>/engine.ipc`

Operator commands (from `ultramarine/`):

- Dry-run / bootstrap (no secrets yet): `make net-plan NET=<net>` (generates inventory/lockfile/bundles but the network won’t be bootable without archiver tokens for validators)
- Generate artifacts: `make net-gen NET=<net> SECRETS_FILE=infra/networks/<net>/secrets.sops.yaml`
- One-command bootstrap (plan + storage + pre-doctor): `make net-bootstrap NET=<net> MOVE_DOCKER_DATAROOT=true`
- One-command go-live (gen + storage + deploy + post-doctor + health): `make net-launch NET=<net> SECRETS_FILE=infra/networks/<net>/secrets.sops.yaml APPLY_FIREWALL=true MOVE_DOCKER_DATAROOT=true`
- One-command update (gen + deploy + health): `make net-update NET=<net> SECRETS_FILE=infra/networks/<net>/secrets.sops.yaml`
- Deploy to hosts (default: restarts services to apply new config): `make net-deploy NET=<net>`
- Start/restart: `make net-up NET=<net>` / Stop: `make net-down NET=<net>`
- Inspect: `make net-status NET=<net>` / `make net-logs NET=<net> LINES=200`
- Health: `make net-health NET=<net>`
- Preflight (pre-deploy): `make net-doctor-pre NET=<net>`
- Diagnostics (post-deploy): `make net-doctor NET=<net>`
- Firewall: `make net-firewall NET=<net>` (or `make net-deploy NET=<net> APPLY_FIREWALL=true`)
- Storage bootstrap: `make net-storage NET=<net>` (see notes below)
- Wipe network from hosts (destructive): `make net-wipe NET=<net> WIPE_CONFIRM=WIPE` (tune with `WIPE_STATE=true|false`, `WIPE_FIREWALL=true|false`, and `LIMIT=<host_id>`)
- Limit any Ansible run to a single host: add `LIMIT=<host_id>` (e.g. `make net-storage NET=<net> LIMIT=lon2-0`)
- SSH key: pass `SSH_KEY=/path/to/key` (or use ssh-agent / `~/.ssh/config`).
- Local checks: `make infra-checks NET=<net>`

Notes:

- Controller requirement: use an Ansible version compatible with your controller Python.
  - `ansible-core 2.15.x` is not compatible with Python `3.14+`. If you used `pipx` and see errors involving `ast.Str`, reinstall Ansible with Python 3.11/3.12 (example: `pipx reinstall --python python3.12 ansible-core`).
- Storage bootstrap is intentionally separate from deploy.
- If your host image enforces a broken apt proxy, set `APT_DISABLE_PROXY=true` (writes `/etc/apt/apt.conf.d/99loadnet-no-proxy`).
- In non-destructive mode, `net-storage` expects the data volume to be mounted at `DATA_MOUNTPOINT` (default: `/var/lib/loadnet`).
- If your provider image mounts the data volume elsewhere (common: `/home`), `net-storage` auto-adopts it by bind-mounting `DATA_SOURCE_DIR` (default: `/home/loadnet`) into `DATA_MOUNTPOINT` and persists it in `/etc/fstab` with systemd mount ordering (`x-systemd.requires-mounts-for=/home`).
- Destructive provisioning requires explicit device IDs and an explicit flag, e.g.:
  - `make net-storage NET=<net> STORAGE_WIPE=true DATA_DEVICES="['/dev/disk/by-id/nvme-...','/dev/disk/by-id/nvme-...']" DATA_RAID_LEVEL=1 MOVE_DOCKER_DATAROOT=true`
- You don’t need to care about the mdadm “device number” (e.g. `/dev/md127`): the playbook defaults to creating `/dev/md/loadnet-data` and mounts by filesystem UUID in `/etc/fstab`. The only per-host detail is selecting the underlying NVMe devices (by-id is safest); discover them with e.g. `ssh <host> 'ls -la /dev/disk/by-id | grep nvme | grep -v part'`.
- `net-deploy` fails fast if `infra/manifests/<net>.yaml` changed without regenerating `network.lock.json`.
- To deploy without restarting running nodes: `make net-deploy NET=<net> RESTART_ON_DEPLOY=false`
- `net-deploy` verifies `bundle/public/genesis.json` checksum against the lockfile on each host.
- Firewall automation is idempotent, keeps SSH allowed, and opens only P2P ports by default.
