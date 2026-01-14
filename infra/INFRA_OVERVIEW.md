# Load Network Infrastructure Overview

This document provides a comprehensive overview of the Load Network testnet infrastructure plane, covering architecture, deployment, and operations from both operator and developer perspectives.

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Component Relationships](#component-relationships)
3. [Deployment Model](#deployment-model)
4. [Operator Commands Reference](#operator-commands-reference)
5. [Storage Management](#storage-management)
6. [Monitoring and Observability](#monitoring-and-observability)
7. [Operational Workflows](#operational-workflows)
8. [Network Configuration](#network-configuration)
9. [BFT Considerations](#bft-considerations)

---

## Architecture Overview

### Design Philosophy

The Load Network infrastructure follows a **manifest-driven** approach where a single YAML file defines the entire network topology. This design provides:

- **Reproducibility**: Deterministic lockfiles ensure identical deployments across environments
- **Auditability**: All network configuration is version-controlled and reviewable
- **Simplicity**: Single source of truth eliminates configuration drift

### Core Principles

1. **CL+EL Co-location**: Each node pairs one Ultramarine (consensus layer) with one load-reth (execution layer) on the same host
2. **IPC-only Engine API**: Consensus-execution communication via Unix socket, never network-exposed
3. **Non-destructive by Default**: All operations preserve existing state unless explicitly overridden
4. **Fail-fast Validation**: Manifest changes require regenerating lockfiles; deploys verify checksums

### Directory Layout

```
ultramarine/infra/
├── gen/netgen/               # Rust network generator binary
├── manifests/                # Network manifest YAML files
│   ├── example.yaml
│   └── fibernet.yaml
├── networks/<net>/           # Per-network generated artifacts
│   ├── network.lock.json     # Deterministic lockfile
│   ├── inventory.yml         # Ansible inventory
│   ├── net.mk                # Optional per-network defaults
│   └── bundle/
│       ├── public/           # Safe to commit (genesis, configs)
│       └── private/          # Never commit (keys, secrets)
├── ansible/
│   ├── ansible.cfg
│   ├── playbooks/            # 13 operational playbooks
│   └── roles/                # 6 reusable roles
└── templates/
    └── systemd/              # Service unit templates
```

### Host Layout (Deployed)

```
/opt/loadnet/
├── current -> networks/<net>   # Active network symlink
└── networks/<net>/             # Network artifacts

/var/lib/
├── load-reth/<node>/           # EL persistent state
└── ultramarine/<node>/         # CL persistent state

/run/load-reth/<node>/
└── engine.ipc                  # Engine API socket
```

---

## Component Relationships

### CL-EL Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         Host                                │
│  ┌─────────────────────┐    ┌─────────────────────────┐    │
│  │    Ultramarine      │    │      load-reth          │    │
│  │   (Consensus)       │    │     (Execution)         │    │
│  │                     │    │                         │    │
│  │  ┌───────────────┐  │    │  ┌─────────────────┐   │    │
│  │  │ Malachite BFT │  │    │  │   Reth + EVM    │   │    │
│  │  └───────────────┘  │    │  └─────────────────┘   │    │
│  │         │           │    │          │             │    │
│  │  ┌───────────────┐  │    │  ┌─────────────────┐   │    │
│  │  │ Blob Engine   │  │    │  │  Blob Pool      │   │    │
│  │  └───────────────┘  │    │  └─────────────────┘   │    │
│  │         │           │    │          │             │    │
│  └─────────┼───────────┘    └──────────┼─────────────┘    │
│            │                           │                   │
│            └───────────┬───────────────┘                   │
│                        │                                   │
│              /run/load-reth/<node>/engine.ipc             │
└─────────────────────────────────────────────────────────────┘
```

### Engine API Flow (v4)

1. **forkchoiceUpdatedV4**: CL signals head block, EL starts building
2. **getPayloadV4**: CL retrieves execution payload + blob bundle
3. **newPayloadV4**: CL submits decided block, EL imports and validates

Key parameters:

- `prev_randao`: Constant `0x01` (Arbitrum-style, no on-chain randomness)
- `execution_requests`: Prague EIP-7685 requests hash
- `max_blobs`: 1024 per block (vs Ethereum's 6)

### Service Dependencies

```
network-online.target
        │
        ▼
  docker.service
        │
        ├──────────────────────┐
        ▼                      ▼
load-reth@<node>.service  ─►  ultramarine@<node>.service
        │                      │
        └──► engine.ipc ◄──────┘
```

---

## Deployment Model

### Netgen (Network Generator)

Netgen is a Rust binary that transforms manifests into deployable artifacts.

**Input**: `infra/manifests/<net>.yaml`

**Outputs**:

- `network.lock.json`: Deterministic lockfile with ports, enodes, checksums
- `genesis.json`: EL genesis (Prague/Cancun at genesis)
- `inventory.yml`: Ansible inventory derived from manifest
- Per-node environment files and P2P keys
- Ultramarine home skeleton with configs

**Key behaviors**:

- Reuses existing keys on subsequent runs (stable identity)
- Validates archiver tokens for validators (fail-fast)
- Embeds manifest checksum for drift detection

### Ansible Layer

**Roles** (6 total):

| Role          | Purpose                                                          |
| ------------- | ---------------------------------------------------------------- |
| `common`      | Base OS setup: Docker, chrony (time sync), sysctl, systemd units |
| `load_reth`   | EL deployment: P2P keys, env files, genesis validation           |
| `ultramarine` | CL deployment: validator keys, archiver config validation        |
| `storage`     | Data volume: RAID setup, mounts, Docker dataroot relocation      |
| `monitoring`  | Prometheus + Grafana installation and configuration              |
| `firewall`    | UFW rules for P2P ports (opt-in)                                 |

**Playbooks** (13 total):

| Playbook                        | Purpose                                 |
| ------------------------------- | --------------------------------------- |
| `deploy.yml`                    | Main deployment orchestrator            |
| `up.yml` / `down.yml`           | Start/stop services                     |
| `roll.yml`                      | Rolling restart (serial=1)              |
| `status.yml` / `logs.yml`       | Service inspection                      |
| `health.yml`                    | Health check (services + height moving) |
| `doctor.yml` / `doctor_pre.yml` | Pre/post-deploy diagnostics             |
| `storage.yml`                   | Storage bootstrap                       |
| `firewall.yml`                  | Apply firewall rules                    |
| `wipe.yml`                      | Destructive cleanup                     |
| `clean_logs.yml`                | Log vacuum and rotation                 |

### Systemd Service Model

Services use **template units** (`@.service`) for multi-instance management:

```
load-reth@node-0.service
load-reth@node-1.service
ultramarine@node-0.service
ultramarine@node-1.service
```

**Container hardening**:

- `--cap-drop=ALL`: No Linux capabilities
- `--security-opt=no-new-privileges`: Prevent privilege escalation
- `--user <uid>:<gid>`: Non-root execution
- `--network host`: Direct P2P port access
- `--log-driver local`: Rotation with 50MB max, 5 files

---

## Operator Commands Reference

### Network Selection

```bash
# Set default network (persists to infra/.net)
make net-use NET=<net>

# Clear default
make net-unset

# Show current config
make net-show
```

### Generation Phase

```bash
# Validate manifest syntax
make net-validate NET=<net>

# Generate without secrets (dry-run/bootstrap)
make net-plan NET=<net>

# Generate with secrets (bootable network)
make net-gen NET=<net> SECRETS_FILE=infra/networks/<net>/secrets.sops.yaml
```

### Deployment Workflows

**One-command workflows**:

```bash
# From scratch: bootstrap + gen + deploy + health
make net-launch NET=<net> SECRETS_FILE=...

# Update running network: gen + apply + roll + health
make net-update NET=<net> SECRETS_FILE=...

# Bootstrap only (no secrets needed)
make net-bootstrap NET=<net>
```

**Granular control**:

```bash
# Deploy without restarts
make net-apply NET=<net>

# Deploy + restart immediately
make net-redeploy NET=<net>

# Rolling restart (one host at a time)
make net-roll NET=<net> ROLL_CONFIRM=YES
```

### Service Management

```bash
# Start services
make net-up NET=<net>

# Stop services
make net-down NET=<net>

# Check status
make net-status NET=<net>

# Tail logs
make net-logs NET=<net> LINES=200
```

### Diagnostics

```bash
# Pre-deploy checks (OS, storage, docker)
make net-doctor-pre NET=<net>

# Post-deploy diagnostics (units, listeners, layout)
make net-doctor NET=<net>

# Health check (services active + height moving)
make net-health NET=<net>
```

### Host Targeting

Limit operations to specific hosts:

```bash
make net-deploy NET=<net> LIMIT=host-0
make net-storage NET=<net> LIMIT=host-1
```

### Common Variables

| Variable            | Default            | Description               |
| ------------------- | ------------------ | ------------------------- |
| `NET`               | (from `.net` file) | Target network name       |
| `SECRETS_FILE`      | auto-detect        | Path to secrets.sops.yaml |
| `LIMIT`             | (all hosts)        | Target specific host      |
| `SSH_KEY`           | (ssh-agent)        | Custom SSH key path       |
| `RESTART_ON_DEPLOY` | `false`            | Restart on deploy         |
| `APPLY_FIREWALL`    | `false`            | Apply UFW rules           |
| `EL_HTTP_BIND`      | `0.0.0.0`          | EL RPC bind address       |
| `PROMETHEUS_BIND`   | `127.0.0.1`        | Metrics bind address      |
| `GRAFANA_BIND`      | `127.0.0.1`        | Dashboard bind address    |

---

## Storage Management

### Data Volume Strategy

Chain state should live on dedicated storage, not the root filesystem:

```bash
# Non-destructive: adopt existing mount (e.g., /home)
make net-storage NET=<net>

# Destructive: format and configure RAID
make net-storage NET=<net> \
  STORAGE_WIPE=true \
  DATA_DEVICES="['/dev/disk/by-id/nvme-...','/dev/disk/by-id/nvme-...']" \
  DATA_RAID_LEVEL=1
```

**Mount points**:

- Data volume: `/var/lib/loadnet`
- EL state: `/var/lib/load-reth/<node>/` (symlinked)
- CL state: `/var/lib/ultramarine/<node>/` (symlinked)

### Docker Dataroot Relocation

Move Docker's storage to the data volume:

```bash
make net-storage NET=<net> MOVE_DOCKER_DATAROOT=true
```

### Log Management

Protect root disk from log growth:

```bash
# Move /var/log to data volume
make net-storage NET=<net> BIND_VAR_LOG=true

# Vacuum journald
make net-clean-logs NET=<net> JOURNAL_VACUUM_SIZE=1G
```

**Default limits** (applied by Ansible):

- Journald: 2GB total, 200MB per file, 7-day retention
- Docker logs: 50MB max-size, 5 files rotation

### Doctor Thresholds

Health checks fail if:

- Root filesystem > 85% full
- `/var/log` > 4GB

Override with `loadnet_root_fs_max_pct` and `loadnet_log_dir_max_mb`.

---

## Monitoring and Observability

### Prometheus + Grafana

Both bind to localhost by default for security. Access via SSH tunnel:

```bash
# Grafana (port 3000)
ssh -L 3000:127.0.0.1:3000 ubuntu@<host-ip>

# Prometheus (port 9090)
ssh -L 9090:127.0.0.1:9090 ubuntu@<host-ip>

# Both at once
ssh -L 3000:127.0.0.1:3000 -L 9090:127.0.0.1:9090 ubuntu@<host-ip>
```

### Metrics Endpoints

| Component   | Port               | Bind      | Description          |
| ----------- | ------------------ | --------- | -------------------- |
| load-reth   | `ports.el.metrics` | 127.0.0.1 | EL execution metrics |
| Ultramarine | `ports.cl.metrics` | 127.0.0.1 | CL consensus metrics |
| Prometheus  | 9090               | 127.0.0.1 | Metrics aggregation  |
| Grafana     | 3000               | 127.0.0.1 | Visualization        |

### Key Metrics

**Consensus (Ultramarine)**:

- `malachitebft_core_consensus_height`: Current block height
- `blob_engine_verification_duration_*`: Blob verification latency
- `blob_engine_storage_bytes_*`: Blob storage size
- `blob_engine_promoted_total`: Blobs moved to decided state

**Execution (load-reth)**:

- `load_reth_engine_*_duration_*`: Engine API latency
- `load_reth_blob_cache_items`: Blob cache occupancy
- `load_reth_engine_get_blobs_*`: Blob retrieval stats

### Health Checks

The `net-health` target verifies:

1. Services are active (systemd)
2. Engine IPC socket exists
3. Consensus height is advancing (scraped twice, 15s apart)

---

## Operational Workflows

### Initial Deployment

```bash
# 1. Set default network
make net-use NET=fibernet

# 2. Prepare hosts (storage, base packages)
make net-bootstrap

# 3. Deploy with secrets
make net-launch SECRETS_FILE=infra/networks/fibernet/secrets.sops.yaml
```

### Updating a Running Network

```bash
# Regenerate + rolling restart
make net-update SECRETS_FILE=infra/networks/fibernet/secrets.sops.yaml
```

### Container Image Upgrade

1. Update `images:` section in manifest
2. Run `make net-gen` to regenerate lockfile
3. Run `make net-roll ROLL_CONFIRM=YES` for rolling restart

### Archiver Token Rotation

```bash
# 1. Update secrets.sops.yaml with new tokens
# 2. Regenerate bundle (keys preserved)
make net-update-secrets SECRETS_FILE=infra/networks/fibernet/secrets.sops.yaml

# 3. Deploy updated secrets
make net-deploy

# 4. Rolling restart
make net-roll ROLL_CONFIRM=YES
```

### Emergency Procedures

**Stop all services**:

```bash
make net-down
```

**Wipe and rebuild**:

```bash
make net-wipe WIPE_CONFIRM=YES
make net-launch SECRETS_FILE=...
```

**Per-host recovery**:

```bash
make net-wipe LIMIT=host-0 WIPE_CONFIRM=YES
make net-deploy LIMIT=host-0
```

---

## Network Configuration

### Manifest Schema

```yaml
schema_version: 1

network:
  name: example
  chain_id: 16383

images:
  ultramarine: "ultramarine:local"
  load_reth: "docker.io/loadnetwork/load-reth:v0.1.2"

hosts:
  - id: host-0
    public_ip: "203.0.113.10"
    ssh_user: ubuntu

nodes:
  - id: node-0
    host: host-0
    role: validator    # or "full"

engine:
  mode: ipc
  ipc_path_template: "/run/load-reth/{node_id}/engine.ipc"

ports:
  allocation: host-block
  host_block_stride: 1000
  el:
    http: 8545
    p2p: 30303
    metrics: 9001
  cl:
    p2p: 27000
    mempool: 28000
    metrics: 29000

sync:
  enabled: true          # Required for multi-host

archiver:
  enabled: true
  provider_url: "https://..."
  provider_id: "..."

exposure:
  metrics_bind: "127.0.0.1"
```

### Port Allocation

With `host-block` allocation and `stride=1000`:

- Host 0: EL HTTP 8545, EL P2P 30303, CL P2P 27000
- Host 1: EL HTTP 9545, EL P2P 31303, CL P2P 28000
- Host 2: EL HTTP 10545, EL P2P 32303, CL P2P 29000

### Per-Network Defaults

Create `infra/networks/<net>/net.mk`:

```make
APPLY_FIREWALL = true
MOVE_DOCKER_DATAROOT = true
BIND_VAR_LOG = true
SSH_KEY = ~/.ssh/custom_key
EL_HTTP_BIND = 0.0.0.0
```

---

## BFT Considerations

### Quorum Math

Malachite BFT requires 2/3+ of validators to agree. For fault tolerance:

- **3 validators**: Tolerates 0 failures (any failure halts consensus)
- **4 validators**: Tolerates 1 failure
- **7 validators**: Tolerates 2 failures

Formula: `max_failures = floor((n - 1) / 3)`

### Rolling Restart Safety

The `roll.yml` playbook requires explicit confirmation if <=2 validators:

```bash
make net-roll ROLL_CONFIRM=YES
```

This prevents accidental chain halt when restarting more than 1/3 of validators.

### Validator Distribution

For single-host failure survival:

```
validators_per_host < n/3
```

Example: With 4 validators, place at most 1 per host.

For testnets that need higher density, you can opt out by setting
`validation.allow_unsafe_failure_domains: true` in the manifest; netgen will
warn and record the override in `network.lock.json`.

### Archiver Requirement

Validators **must** have archiver configuration. The system fails fast without it:

- `netgen gen` fails if any validator is missing `archiver_bearer_token`
- Ansible validates archiver secrets before starting services

---

## Appendix: Quick Reference

### Most Common Commands

```bash
# Deploy from scratch
make net-launch NET=<net> SECRETS_FILE=...

# Update running network
make net-update NET=<net> SECRETS_FILE=...

# Check health
make net-health NET=<net>

# Rolling restart
make net-roll NET=<net> ROLL_CONFIRM=YES

# View logs
make net-logs NET=<net> LINES=500
```

### Related Documentation

- `README.md`: Deploy layer quick reference
- `FIREWALL.md`: Port allocation guidance
- `SECRETS.md`: Secrets management with SOPS+age
- `../docs/DEV_WORKFLOW.md`: Local development guide
- `../docs/ARCHIVER_OPS.md`: Blob archival operations
