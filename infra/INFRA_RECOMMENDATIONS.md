# Infrastructure Improvement Recommendations

This document outlines suggested improvements for the Load Network infrastructure plane, organized by priority and category.

---

## High Priority

### 1. Add Prometheus Alerting Rules

**Category**: Observability, Incident Response

**Current State**: Prometheus scrapes metrics but no alerting rules are defined.

**Recommendation**: Ship default alert rules for critical conditions:

- Consensus height not advancing (stall detection)
- Validator missing blocks (participation rate)
- EL-CL communication failures
- Disk space warnings
- Service restart loops

**Implementation**:

- Add `prometheus/alerts/` directory with rule files
- Include in monitoring role deployment
- Document alert tuning in README

---

### 2. Implement Graceful Validator Shutdown

**Category**: Operations, Chain Safety

**Current State**: `docker stop -t 10` gives 10 seconds for shutdown.

**Recommendation**: Implement vote-aware graceful shutdown:

- Ultramarine should finish current round before exiting
- Add pre-stop hook to check if mid-vote
- Increase stop timeout for large validator sets
- Consider "leaving validator set" announcement

**Benefits**:

- Prevents partial vote scenarios
- Reduces consensus disruption during rolling restarts

---

### 3. Add Health Check Endpoints

**Category**: Orchestration, Load Balancing

**Current State**: Health is checked via metrics scraping and height comparison.

**Recommendation**: Add dedicated health check HTTP endpoints:

- `/health/live`: Process is running
- `/health/ready`: Node is synced and ready for traffic
- `/health/consensus`: Participating in consensus (validators only)

**Benefits**:

- Kubernetes-compatible readiness probes
- Load balancer integration for RPC nodes
- Faster detection than metrics polling

---

## Medium Priority

### 4. Add Backup and Restore Tooling

**Category**: Disaster Recovery

**Current State**: No automated backup/restore procedures.

**Recommendation**: Add backup and restore playbooks:

```bash
make net-backup NET=<net> BACKUP_DIR=/backup/
make net-restore NET=<net> BACKUP_FILE=/backup/2024-01-15.tar.gz
```

**Scope**:

- Validator keys (critical, separate from state)
- Consensus DB (can be rebuilt but slow)
- Configuration and environment files
- Document recovery time objectives (RTO)

---

### 5. Add Network Topology Visualization

**Category**: UX, Documentation

**Current State**: Network topology only visible in lockfile JSON.

**Recommendation**: Generate visual network diagram:

- Auto-generate from `network.lock.json`
- Show hosts, nodes, port allocations
- Indicate validator vs full node roles
- Include in generated bundle

**Implementation**:

- Add `make net-diagram NET=<net>` target
- Output SVG or Mermaid diagram
- Update on each `net-gen`

---

### 6. Implement Configuration Validation Before Restart

**Category**: Operations, Safety

**Current State**: Config changes are applied and services restart; validation happens at startup.

**Recommendation**: Add pre-restart validation step:

- Parse config files before systemd restart
- Verify P2P key format
- Check port availability
- Validate environment variables

**Implementation**:

- Add `ExecStartPre` validation script
- Include `--dry-run` mode in Ultramarine CLI
- Fail deployment if validation fails (before restart)

---

### 7. Add Structured Logging Configuration

**Category**: Operations, Debugging

**Current State**: Logs go to Docker's local driver; format controlled per-container.

**Recommendation**: Standardize JSON structured logging:

- Enable JSON log format for both CL and EL
- Add log aggregation option (e.g., Loki, CloudWatch)
- Include correlation IDs for CL-EL communication
- Add log level runtime configuration

**Benefits**:

- Easier log analysis and searching
- Better debugging of cross-component issues
- Integration with existing log infrastructure

---

### 8. Add Node Synchronization Status Endpoint

**Category**: Operations, UX

**Current State**: Sync status requires manual metrics inspection.

**Recommendation**: Add sync status command:

```bash
make net-sync-status NET=<net>
```

Output:

```
host-0 (node-0): SYNCED height=12345 peers=5
host-1 (node-1): SYNCING height=12340 target=12345 peers=3
host-2 (node-2): SYNCED height=12345 peers=6
```

---

### 9. Implement Canary Deployment Support

**Category**: DevOps, Risk Mitigation

**Current State**: All nodes are updated together or via serial=1 rolling restart.

**Recommendation**: Add canary deployment workflow:

- Deploy to single node first
- Verify health for N minutes
- Proceed with remaining nodes or rollback

**Implementation**:

```bash
make net-canary NET=<net> CANARY_HOST=host-0 WAIT_MINUTES=10
```

---

## Lower Priority

### 10. Add Ansible Check Mode Support

**Category**: Operations, Safety

**Current State**: Playbooks don't fully support `--check` mode.

**Recommendation**: Enable dry-run for all playbooks:

```bash
make net-deploy NET=<net> DRY_RUN=true
```

**Benefits**:

- Preview changes before applying
- Safer in production environments
- Better change management audit trail

---

### 11. Add Multi-Region Deployment Examples

**Category**: Documentation, Operations

**Current State**: Example manifests assume single-region deployment.

**Recommendation**: Add multi-region manifest examples:

- Document latency considerations
- Recommend validator placement per region
- Include cloud-specific networking notes (VPC peering, etc.)
- Add region-aware port allocation

---

### 12. Implement Container Image Pinning by Digest

**Category**: Supply Chain, Reproducibility

**Current State**: Images are pinned by tag (e.g., `v0.1.2`).

**Recommendation**: Support SHA256 digest pinning:

```yaml
images:
  ultramarine: "ultramarine@sha256:abc123..."
  load_reth: "docker.io/loadnetwork/load-reth@sha256:def456..."
```

**Benefits**:

- Immutable deployments
- Protection against tag mutation
- Reproducible builds

---

### 13. Add Ansible Inventory Dynamic Generation

**Category**: Scalability, Cloud Integration

**Current State**: Inventory is generated once by netgen.

**Recommendation**: Support dynamic inventory plugins:

- AWS EC2 dynamic inventory
- GCP compute instances
- Generic cloud-init integration

**Benefits**:

- Auto-scaling support
- Easier cloud migrations
- Reduced manual inventory management

---

### 14. Add Performance Benchmarking Tooling

**Category**: Testing, Capacity Planning

**Current State**: No standardized performance benchmarks.

**Recommendation**: Add benchmark suite:

```bash
make net-benchmark NET=<net> DURATION=10m
```

**Metrics to capture**:

- Transaction throughput (TPS)
- Block production rate
- Blob verification latency
- Memory and CPU utilization
- Network bandwidth consumption

---

### 15. Implement Secrets Rotation Automation

**Category**: Operations, Compliance

**Current State**: Archiver token rotation is manual.

**Recommendation**: Add automated rotation workflow:

- Generate new tokens
- Rolling update of secrets
- Verify connectivity after rotation
- Log rotation events for audit

```bash
make net-rotate-secrets NET=<net> COMPONENT=archiver
```

---

### 16. Add Service Mesh / mTLS Option

**Category**: Network Security

**Current State**: Inter-node communication relies on P2P encryption.

**Recommendation**: Document/support optional service mesh:

- mTLS between all components
- Network policies for Kubernetes deployments
- Alternative to host-based firewall

---

### 17. Implement Configuration Drift Detection

**Category**: Compliance, Operations

**Current State**: Manifest drift is detected at deploy time.

**Recommendation**: Add continuous drift detection:

- Compare running state vs expected state
- Alert on configuration drift
- Auto-remediation option

```bash
make net-drift-check NET=<net>
```

---

## UX Improvements

### 18. Add Interactive Deployment Mode

**Category**: UX

**Recommendation**: Add interactive mode for first-time users:

```bash
make net-wizard
```

Prompts for:

- Network name
- Number of validators
- Host IPs
- SSH credentials
- Storage preferences

Generates manifest and guides through deployment.

---

### 19. Improve Error Messages

**Category**: UX, Debugging

**Current State**: Some errors require reading Ansible output.

**Recommendation**: Add wrapper that surfaces common errors:

- "Manifest changed since lockfile" → suggest `make net-gen`
- "Archiver token missing" → show which node
- "SSH connection failed" → verify network/keys
- "Port already in use" → show conflicting process

---

### 20. Add Command Completion

**Category**: UX

**Recommendation**: Add shell completion for make targets:

```bash
# bash
source <(make --print-data-base | grep '^[a-z].*:' | cut -d: -f1 | ...)

# Or ship completion scripts
make install-completions
```

---

### 21. Add Deployment Progress Visualization

**Category**: UX

**Current State**: Ansible output can be verbose.

**Recommendation**: Add progress indicator:

```
Deploying to 3 hosts...
[█████████░░░░░░░░░░░] 45% - Configuring host-1 (ultramarine role)
```

---

## Summary Matrix

| #  | Recommendation              | Priority | Effort | Category      |
| -- | --------------------------- | -------- | ------ | ------------- |
| 1  | Prometheus alerting         | High     | Low    | Observability |
| 2  | Graceful validator shutdown | High     | Medium | Safety        |
| 3  | Health check endpoints      | High     | Medium | Orchestration |
| 4  | Backup/restore tooling      | Medium   | Medium | DR            |
| 5  | Network topology diagram    | Medium   | Low    | UX            |
| 6  | Pre-restart validation      | Medium   | Low    | Safety        |
| 7  | Structured logging          | Medium   | Medium | Operations    |
| 8  | Sync status endpoint        | Medium   | Low    | UX            |
| 9  | Canary deployments          | Medium   | Medium | DevOps        |
| 10 | Ansible check mode          | Low      | Low    | Safety        |
| 11 | Multi-region examples       | Low      | Low    | Docs          |
| 12 | Image digest pinning        | Low      | Low    | Supply Chain  |
| 13 | Dynamic inventory           | Low      | Medium | Scalability   |
| 14 | Benchmarking tools          | Low      | Medium | Testing       |
| 15 | Secrets rotation            | Low      | Medium | Compliance    |
| 16 | Service mesh docs           | Low      | Low    | Network       |
| 17 | Drift detection             | Low      | Medium | Compliance    |
| 18 | Interactive wizard          | Low      | Medium | UX            |
| 19 | Error message improvement   | Low      | Low    | UX            |
| 20 | Shell completion            | Low      | Low    | UX            |
| 21 | Progress visualization      | Low      | Low    | UX            |
