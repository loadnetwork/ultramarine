# Load Network Infra Plane Assessment

> Independent review of the Ultramarine infrastructure deployment system from a web3/blockchain operations perspective.

---

## Executive Summary

The infra plane is a **production-grade, manifest-driven deployment system** built on Ansible. It demonstrates strong DevOps practices tailored for blockchain validator operations, including BFT-aware rolling restarts, deterministic configuration generation, and comprehensive observability.

**Key Strengths:**

- Manifest-driven reproducibility with lockfile verification
- One-command deployment workflows
- BFT-safe rolling restart with quorum awareness
- Comprehensive health checks and diagnostics

**Areas for Enhancement:**

- Backup/restore automation (planned, not implemented)
- Multi-region deployment patterns
- Chaos engineering / fault injection testing

---

## 1. Architecture Overview

### Design Principles

| Principle           | Implementation                                             |
| ------------------- | ---------------------------------------------------------- |
| **Manifest-Driven** | Single YAML source of truth per network                    |
| **Deterministic**   | Lockfile captures port allocations, checksums, topology    |
| **Reproducible**    | Same manifest + generated artifacts = identical deployment |
| **Non-Destructive** | Deploy never overwrites existing node identity artifacts   |
| **Fail-Fast**       | Validation before deployment (netgen validate)             |

### Component Hierarchy

```
infra/
├── manifests/           # Network topology definitions (YAML)
│   ├── example.yaml     # Test network (3 validators)
│   └── fibernet.yaml    # Production network (6 validators, 3 hosts)
├── gen/netgen/          # Network generation binary (Rust)
├── networks/            # Per-network generated artifacts
│   └── <network>/
│       ├── network.lock.json      # Deterministic lockfile
│       ├── inventory.yml          # Ansible inventory
│       └── bundle/                # Deployment artifacts
│           ├── public/            # Genesis, network.json (committable)
│           └── private/           # Non-committed artifacts (git-ignored)
├── ansible/
│   ├── playbooks/       # 13 operational playbooks
│   └── roles/           # 6 reusable roles
└── templates/           # Systemd unit templates
```

### Data Flow

```
Manifest YAML → netgen validate → netgen plan/gen → Ansible Deploy → Running Network
                    ↓                    ↓                ↓
              Schema validation    Lockfile + Bundle    Health checks
```

---

## 2. Deployment Workflow

### Phase-Based Deployment

| Phase         | Command              | Purpose                                     |
| ------------- | -------------------- | ------------------------------------------- |
| **Validate**  | `make net-validate`  | Check manifest syntax                       |
| **Plan**      | `make net-plan`      | Generate without sensitive inputs (dry-run) |
| **Bootstrap** | `make net-bootstrap` | Prepare hosts (storage, pre-checks)         |
| **Generate**  | `make net-gen`       | Generate with sensitive inputs (bootable)   |
| **Deploy**    | `make net-deploy`    | Apply configuration                         |
| **Health**    | `make net-health`    | Verify services running                     |

### One-Command Workflows

```bash
# Full deployment from scratch
make net-launch NET=fibernet SECRETS_FILE=...

# Update running network (regenerate + rolling restart)
make net-update NET=fibernet SECRETS_FILE=...

# Prepare new hosts
make net-bootstrap NET=fibernet
```

### Deployment Safety

1. **Lockfile verification** - Fails if manifest changed without regeneration
2. **Genesis checksum** - Prevents accidental genesis drift
3. **Identity persistence** - Never overwrites existing validator artifacts
4. **Quorum warning** - Rolling restart warns if <= 2 validators

---

## 3. Ansible Structure

### Playbooks (13 total)

| Playbook                        | Purpose                              | BFT-Safe |
| ------------------------------- | ------------------------------------ | -------- |
| `deploy.yml`                    | Main deployment orchestrator         | -        |
| `up.yml` / `down.yml`           | Start/stop services                  | Yes      |
| `roll.yml`                      | Rolling restart (serial=1)           | Yes      |
| `status.yml` / `logs.yml`       | Service inspection                   | -        |
| `health.yml`                    | Validate services + height advancing | -        |
| `doctor.yml` / `doctor_pre.yml` | Pre/post deploy diagnostics          | -        |
| `storage.yml`                   | Storage bootstrap (RAID, mounts)     | -        |
| `firewall.yml`                  | UFW rules for P2P ports              | -        |
| `wipe.yml`                      | Destructive cleanup                  | -        |
| `clean_logs.yml`                | Log rotation/vacuum                  | -        |

### Roles (6 total, applied in order)

1. **common** - Base OS setup (Docker, chrony, sysctl, journald limits)
2. **load_reth** - EL deployment (genesis, env files, P2P identity artifacts)
3. **ultramarine** - CL deployment (config, validator artifacts, archiver config)
4. **storage** - Data volume management (RAID, mounts, Docker dataroot)
5. **monitoring** - Prometheus + Grafana containers
6. **firewall** - UFW automation for P2P ports

### Role Execution Order (deploy.yml)

```
common → firewall → load_reth → ultramarine → monitoring
```

---

## 4. Makefile UX Assessment

### Strengths

| Feature             | Rating    | Notes                                   |
| ------------------- | --------- | --------------------------------------- |
| **Discoverability** | Excellent | `make help` provides full reference     |
| **Defaults**        | Good      | `make net-use` sets default network     |
| **Safety**          | Excellent | Destructive ops require `*_CONFIRM=YES` |
| **Flexibility**     | Good      | `LIMIT=host` for targeted operations    |
| **Idempotency**     | Good      | Re-running deploy is safe               |

### Common Operations

```bash
# Set default network (writes to infra/.net)
make net-use NET=fibernet

# After setting default, no NET= needed
make net-status
make net-logs LINES=500
make net-health

# Target specific host
make net-deploy LIMIT=lon2-0
make net-logs LIMIT=lon2-0
```

### Advanced Variables

| Variable            | Purpose                    | Default       |
| ------------------- | -------------------------- | ------------- |
| `SECRETS_FILE`      | Path to encrypted inputs   | Auto-detected |
| `LIMIT`             | Target specific host       | All hosts     |
| `RESTART_ON_DEPLOY` | Restart after deploy       | false         |
| `APPLY_FIREWALL`    | Enable firewall automation | false         |
| `EL_HTTP_BIND`      | EL JSON-RPC bind address   | 0.0.0.0       |
| `STORAGE_WIPE`      | Destructive storage setup  | false         |

---

## 5. Configuration Management

### Per-Network Configuration

```
infra/networks/<net>/
├── network.lock.json      # Generated: ports, enodes, checksums
├── inventory.yml          # Generated: host → node mapping
├── net.mk                 # Optional: network-specific overrides
└── secrets.sops.yaml      # Encrypted: runtime inputs
```

### Environment Injection

- **EL runtime**: Per-node env file generated during netgen
- **CL runtime**: Per-node env file generated during netgen
- **Archiver runtime**: Per-node env file generated during netgen

### Configuration Drift Detection

```
Manifest YAML hash → stored in network.lock.json
                  ↓
Deploy checks manifest_sha256 → fails if changed
```

---

## 6. Monitoring Stack

### Components

| Component      | Purpose            | Default Bind   |
| -------------- | ------------------ | -------------- |
| **Prometheus** | Metrics collection | 127.0.0.1:9090 |
| **Grafana**    | Dashboards         | 127.0.0.1:3000 |

### Metrics Exposure

- **EL metrics**: `ports.el.metrics` (localhost by default)
- **CL metrics**: `ports.cl.metrics` (localhost by default)

### Available Dashboards

- Malachite consensus metrics (height, round, votes)
- Reth execution metrics (block processing, gas)
- Blob engine metrics (verification, storage, lifecycle)

### Alerting Patterns (Recommended)

```yaml
# Height stall detection
malachitebft_core_consensus_height increase(1m) == 0

# Round spinning (consensus stuck)
malachitebft_core_consensus_round rate(5m) > 10

# Blob accumulation (pre-fix indicator)
blob_engine_undecided_blob_count > 100
```

---

## 7. Operational Procedures

### Rolling Restart (BFT-Safe)

```bash
# Serial=1 ensures one node at a time
make net-roll NET=fibernet ROLL_CONFIRM=YES

# Warning issued if <= 2 validators (quorum risk)
```

### Log Management

```bash
# Tail logs
make net-logs NET=fibernet LINES=500

# Vacuum journald (free disk space)
make net-clean-logs NET=fibernet JOURNAL_VACUUM_SIZE=1G
```

### Diagnostics

```bash
# Pre-deploy checks (OS, storage, Docker)
make net-doctor-pre NET=fibernet

# Post-deploy diagnostics (units, listeners, disk)
make net-doctor NET=fibernet

# Health validation (services + height advancing)
make net-health NET=fibernet
```

### Emergency Procedures

```bash
# Stop all services
make net-down NET=fibernet

# Wipe and redeploy (destructive)
make net-wipe NET=fibernet WIPE_CONFIRM=YES
make net-launch NET=fibernet SECRETS_FILE=...

# Per-host recovery
make net-wipe LIMIT=lon2-0 WIPE_CONFIRM=YES
make net-deploy LIMIT=lon2-0
```

---

## 8. Web3/Blockchain Specific Considerations

### Validator Key Lifecycle

| Operation            | Automation                  | Risk Level |
| -------------------- | --------------------------- | ---------- |
| Initial seeding      | Automated (if missing)      | Low        |
| Identity persistence | Enforced (never overwrite)  | -          |
| Identity rotation    | Manual (protocol event)     | High       |
| Identity backup      | Recommended (operator duty) | -          |

### Consensus Safety

- **Rolling restarts**: Serial=1 ensures BFT quorum maintained
- **Quorum warnings**: Alerts when <= 2 validators
- **Height gating**: StartedHeight prevents premature consensus participation

### P2P Network

- **Stable identity**: P2P identity artifacts persisted across restarts
- **Bootnode coordination**: Enodes embedded in lockfile
- **Port allocation**: Deterministic based on host block + stride

---

## 9. Recommendations

### Implemented Well

1. **Manifest-driven deployment** - Reproducible, auditable
2. **Lockfile verification** - Prevents configuration drift
3. **BFT-safe operations** - Rolling restarts with quorum awareness
4. **Fail-fast validation** - Catches errors before deployment

### Recommended Enhancements

| Priority | Enhancement               | Rationale                     |
| -------- | ------------------------- | ----------------------------- |
| High     | **Backup automation**     | Protect against data loss     |
| High     | **Alerting integration**  | Proactive incident detection  |
| Medium   | **Multi-region patterns** | Geographic redundancy         |
| Medium   | **Chaos testing**         | Validate fault tolerance      |
| Low      | **GitOps workflow**       | Automated deployment pipeline |

### Backup Strategy (Recommended)

```bash
# Planned commands (not yet implemented)
make net-backup NET=fibernet BACKUP_DIR=/backup/
make net-restore NET=fibernet BACKUP_FILE=/backup/snapshot.tar.gz

# Critical data to backup:
# - Validator identity artifacts
# - P2P identity artifacts
# - Consensus state (optional, can rebuild from peers)
```

### Monitoring Enhancements (Recommended)

```yaml
# Add to Prometheus rules
groups:
  - name: consensus
    rules:
      - alert: ConsensusStalled
        expr: increase(malachitebft_core_consensus_height[5m]) == 0
        for: 10m

      - alert: RoundSpinning
        expr: rate(malachitebft_core_consensus_round[5m]) > 10
        for: 5m

      - alert: BlobAccumulation
        expr: blob_engine_undecided_blob_count > 50
        for: 5m
```

---

## 10. Conclusion

The Ultramarine infra plane is a **mature, production-ready deployment system** that demonstrates deep understanding of blockchain validator operations. The manifest-driven approach with lockfile verification provides strong reproducibility guarantees, while BFT-aware operations ensure safe deployments.

Key architectural decisions (lockfile verification, serial rolling restarts) reflect operational rigor appropriate for blockchain infrastructure.

The primary gaps are in recovery automation (backup/restore) and proactive monitoring (alerting). These are reasonable deferrals for an M3 milestone and should be addressed before mainnet deployment.

---

_Assessment Date: 2026-01-12_
_Reviewed By: Claude Code_
