# Ultramarine Development Notes

## DevOps Operations - MANDATORY RULES

**CRITICAL**: Before performing ANY DevOps or operations tasks (deploy, wipe, restart, etc.), you MUST:

1. **Read the documentation first**:
   - `infra/README.md` - main operations guide
   - `infra/Makefile` - all available targets and parameters
   - Network manifest in `infra/manifests/<network>.yaml`

2. **Use existing Makefile targets** - NEVER write custom SSH commands for operations:
   ```bash
   # Network operations (from infra/ directory)
   make net-wipe NET=<network> WIPE_CONFIRM=YES    # Wipe all state
   make net-gen NET=<network> SECRETS_FILE=...     # Generate artifacts
   make net-deploy NET=<network>                    # Deploy to servers
   make net-up NET=<network>                        # Start services
   make net-down NET=<network>                      # Stop services
   make net-roll NET=<network> ROLL_CONFIRM=YES    # Rolling restart
   make net-health NET=<network>                    # Health check
   make net-redeploy NET=<network>                  # Gen + deploy + restart
   ```

3. **Key parameters**:
   - `NET=fibernet` - target network
   - `WIPE_CONFIRM=YES` - required for destructive operations
   - `SECRETS_FILE=infra/networks/<net>/secrets.sops.yaml` - for generation
   - `LIMIT=<host_id>` - run only on specific host
   - `WIPE_NODES=node-0,node-1` - wipe specific nodes only

4. **Full network restart sequence**:
   ```bash
   cd infra
   make net-wipe NET=fibernet WIPE_CONFIRM=YES
   make net-gen NET=fibernet SECRETS_FILE=networks/fibernet/secrets.sops.yaml
   make net-deploy NET=fibernet
   make net-up NET=fibernet
   make net-health NET=fibernet
   ```

---

## Building Docker Images on macOS

### Recommended Approach: `docker buildx`

On macOS (especially Apple Silicon), use `docker buildx` with QEMU emulation instead of the Makefile's `cross` toolchain:

```bash
# Build for linux/amd64 and push to Docker Hub
docker buildx build --platform linux/amd64 -t loadnetwork/ultramarine:TAG --push .

# Example with fibernet tag
docker buildx build --platform linux/amd64 -t loadnetwork/ultramarine:fibernet --push .
```

### Why NOT use `make docker-build-push`

The Makefile target `docker-build-push` uses `cross` for cross-compilation which requires pulling the `ghcr.io/cross-rs/x86_64-unknown-linux-gnu:main` image. On Apple Silicon Macs, this can fail with:

- "no match for platform in manifest" error
- QEMU emulation segfaults (exit code 139)

### Alternative: Build on server

If buildx is too slow or unreliable, transfer source to the target server and build natively:

```bash
# Create tarball (excluding .git and target)
tar --exclude='.git' --exclude='target' --exclude='dist' -czf /tmp/ultramarine-src.tar.gz .

# Transfer to server
scp /tmp/ultramarine-src.tar.gz user@server:/tmp/

# On server: extract and build
cd /tmp && tar -xzf ultramarine-src.tar.gz
docker build -t loadnetwork/ultramarine:TAG .
docker push loadnetwork/ultramarine:TAG
```

## ValueSync and Blob Pruning

### Archive-Based Pruning Policy (Load Network)

**IMPORTANT**: Load Network uses the **archive event as the boundary for blob pruning**, NOT the Ethereum DA window.

Key differences from Ethereum:

- **Ethereum**: Blobs pruned based on a time-based DA window (~18 days / 4096 epochs)
- **Load Network**: Blobs pruned only after successful archival to external storage + finality

### What Gets Pruned vs. Retained Forever

| Data Type                      | Retention Policy                      |
| ------------------------------ | ------------------------------------- |
| Blob bytes                     | Pruned after archive event + finality |
| Decided values                 | **Retained forever**                  |
| Certificates                   | **Retained forever**                  |
| Block data (execution payload) | **Retained forever**                  |
| BlobMetadata                   | **Retained forever**                  |
| Archive records/locators       | **Retained forever**                  |

### history_min_height Invariant

The `get_earliest_height()` function returns `Height(0)` when genesis metadata exists. This is an invariant:

- `history_min_height == 0` for all validators
- Ensures fullnodes can sync from genesis
- Validators advertise they can serve the complete chain history

### MetadataOnly Sync Pattern for Archived Blobs

When blobs have been pruned (archived), the sync mechanism uses `SyncedValuePackage::MetadataOnly`:

1. `GetDecidedValue` returns `MetadataOnly` with execution payload when blobs are pruned
2. `process_synced_package` imports blocks from `MetadataOnly` if execution payload is present
3. Archive notices with locators are included so external consumers can fetch blob bytes from the archive provider

This pattern ensures fullnodes can sync the complete chain even when blob bytes are no longer available locally.

### Reference: Lighthouse Pattern

The design follows Lighthouse's pattern where beacon blocks are kept forever, only blob sidecars are pruned:

- `data_availability_checker.rs:518-536` - `blobs_required_for_epoch()`
- `network_context.rs:1367-1382` - Request type selection based on DA window
- `block_sidecar_coupling.rs:573-577` - Empty blob response handling

In Load Network, we apply the same principle: consensus data (decided values, certificates, block data) is retained indefinitely, while blob bytes are pruned after archive verification.

---

## Engine API Design Decisions (Consensus Correctness)

These rules are consensus-critical for Ultramarine:

1. **Engine API is the oracle; HTTP RPC is not.**
   - `engine_forkchoiceUpdated` status (`VALID/INVALID/SYNCING`) is the only readiness signal.
   - `eth_getBlockByNumber` must not be used for consensus gating.

2. **Gate proposals and votes on FCU status.**
   - Before proposing or voting: `FCU(head=CL decided, attrs=None)` must be `VALID`.
   - If `SYNCING/INVALID` → treat proposal as invalid and **vote nil**.

3. **Proposer flow (build)**
   - `FCU(head, attrs=PayloadAttrs)` to start build.
   - `engine_getPayload` after valid FCU with attrs.
   - `ACCEPTED` is **not** a valid FCU status; treat it as an error.

4. **Post-decision execution**
   - After `Decided`, call `engine_newPayload` + `FCU` to drive EL.
   - EL lag does not invalidate consensus decision; it only delays execution finalization.

5. **Tendermint re-proposal requirement**
   - Proposer must be able to re-serve the same payload for the same height/round.
   - Store proposal payloads until height is decided.
