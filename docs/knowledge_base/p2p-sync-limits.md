# P2P and Sync Size Limits

Terms:

- `P2P` = peer-to-peer gossip and request/response transport between nodes.
- `RPC` here refers to P2P request/response limits (not JSON-RPC to EL).
- `ValueSync` = protocol path used to transfer decided values during catch-up.

## Problem

During high-throughput load tests, blocks can grow to **~5-12 MB** with many transactions
(2026-02-10 baseline hot segment peaked at ~11.6 MB).
Default P2P/sync limits (~1-10 MB) cause:

- Sync requests rejected at P2P layer
- Nodes falling behind and unable to catch up
- `WARN: Beacon client online, but no consensus updates received`

Log meaning:

- This warning usually indicates consensus is running but block update propagation/sync is unhealthy.

## Configuration Parameters

| Parameter           | Location                       | Default           | Load Test Value | Purpose                 |
| ------------------- | ------------------------------ | ----------------- | --------------- | ----------------------- |
| `pubsub_max_size`   | `consensus.p2p`, `mempool.p2p` | 4 MiB (~4.2 MB)   | **50 MiB**      | P2P gossip message size |
| `rpc_max_size`      | `consensus.p2p`, `mempool.p2p` | 10 MiB (~10.5 MB) | **100 MiB**     | P2P RPC response size   |
| `max_request_size`  | `[sync]`                       | 1 MiB             | **50 MiB**      | ValueSync request size  |
| `max_response_size` | `[sync]`                       | 10 MiB (~10.5 MB) | **500 MiB**     | ValueSync response size |

## Manifest Configuration

In `manifests/<network>.yaml`:

```yaml
sync:
  enabled: true
  max_request_size: "50 MiB"
  max_response_size: "500 MiB"
  request_timeout: "60s"
  parallel_requests: 100
  batch_size: 5
  fullnode:
    parallel_requests: 100
    request_timeout: "60s"
    max_response_size: "500 MiB"
    batch_size: 10

p2p:
  pubsub_max_size: "50 MiB"
  rpc_max_size: "100 MiB"
```

## Code Locations

### Netgen (config generation)

- `infra/gen/netgen/src/main.rs:145-165` - Sync struct definition
- `infra/gen/netgen/src/main.rs:188-200` - P2pConfig struct
- `infra/gen/netgen/src/main.rs:956-998` - Size limit application

### Config files

- `infra/networks/<network>/bundle/private/ultramarine/homes/node-*/config/config.toml`

## Critical Insight

**ALL nodes** must have large limits — not just receivers.

The SENDER also needs high `rpc_max_size` to send large sync responses. If only receiving
nodes have large limits, sync still fails because sending nodes cannot serialize responses.

## Capacity Planning

With 1024 blobs per block (high DA capacity):

- Max block size: 1024 × 131,072 bytes = ~134 MB
- Requires: `max_response_size: "500 MiB"` (includes batch overhead)

For regular load tests (no blobs, many transactions):

- Observed block size in hot segments: ~5-11.6 MB (PERF-SUMMARY, 2026-02-10 baseline)
- Requires: `pubsub_max_size: "50 MiB"`, `rpc_max_size: "100 MiB"`

## Manual Fix (without netgen)

Preferred path:

- Update `manifests/<network>.yaml` and regenerate configs via netgen.
- Use the manual script below only as a recovery shortcut.

If nodes are stuck after load test, update configs on ALL hosts:

```bash
# Create update script
cat > /tmp/update-sync-sizes.sh << 'EOF'
#!/bin/bash
for config in /var/lib/ultramarine/*/config/config.toml; do
    sed -i.bak \
        -e 's/pubsub_max_size = ".*"/pubsub_max_size = "50 MiB"/g' \
        -e 's/rpc_max_size = ".*"/rpc_max_size = "100 MiB"/g' \
        -e 's/max_request_size = ".*"/max_request_size = "50 MiB"/g' \
        -e 's/max_response_size = ".*"/max_response_size = "500 MiB"/g' \
        "$config"
done
EOF

# Run on ALL hosts, then restart
for host in LON2 AMS FRA2 RPC; do
    scp /tmp/update-sync-sizes.sh ubuntu@$host:/tmp/
    ssh ubuntu@$host 'sudo bash /tmp/update-sync-sizes.sh'
    ssh ubuntu@$host 'sudo systemctl restart ultramarine@*'
done
```

## Troubleshooting

### Sync stalls after load test

1. Check node logs for `Sync tip unchanged for too long`
2. Verify all nodes have matching large size limits
3. Restart nodes after config update

### P2P messages rejected

Symptom: Messages dropped silently, no error in logs.

Fix: Increase `pubsub_max_size` and `rpc_max_size` on ALL nodes.

### ValueSync timeout

Symptom: `Sync request timed out` in logs.

Fix:

1. Increase `request_timeout` to 60s+
2. Increase `max_response_size` to accommodate large blocks
3. Ensure sending nodes also have large `rpc_max_size`

## Validation

**Historical validation (2026-02-06):** after applying these fixes, AMS + FRA2 achieved **0% errors** during the 60k‑target sharded run (~40k+ documented in captured logs; LON2 output missing).

**Latest validation (2026-02-10):** PERF-SUMMARY shows a clean 20k/60s probe on all three hosts with **0 errors** and `txpool pending=0, queued=0` everywhere. Total submission: **1,823,713 tx** (~30,395 TPS). See PERF-SUMMARY for current baseline evidence and consolidated phase metrics.

| Shard     | Submitted     | Avg TPS     | Errors |
| --------- | ------------- | ----------- | ------ |
| LON2      | 924,008       | 15,384.19   | **0**  |
| AMS       | 321,788       | 5,352.32    | **0**  |
| FRA2      | 577,917       | 9,631.21    | **0**  |
| **Total** | **1,823,713** | **~30,395** | **0**  |

See `../journal/PERF-SUMMARY-fibernet-throughput-journey.md` for full baseline details.

## Related

- [CL Runtime](./cl-runtime.md) - logging and threading config
- [EL Gas Limits](./el-gas-limits.md) - builder and txpool config
- [Block Timing](./block-timing.md) - timestamp invariants
