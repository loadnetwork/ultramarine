# CL Runtime Configuration

## Config Location

`config/config.toml` in each node's home directory.
Fibernet: `ultramarine/infra/networks/fibernet/bundle/private/ultramarine/homes/node-*/config/config.toml`

## Runtime Settings

### [runtime] flavor

- `single_threaded` - development (default)
- `multi_threaded` - **production**

Code: `malachite/code/crates/config/src/lib.rs:687`

Single-threaded adds overhead under high load.

### [logging] log_level

- `debug` - default, high I/O overhead
- `info` - **production**
- `warn` / `error` - minimal logging

Code: `malachite/code/crates/config/src/lib.rs:707`

## Production Config

```toml
[logging]
log_level = "info"
log_format = "plaintext"

[runtime]
flavor = "multi_threaded"
```

## Consensus Timeouts

| Parameter           | Default | Notes                                |
| ------------------- | ------- | ------------------------------------ |
| `timeout_propose`   | 3s      | Safety valve, not throughput limiter |
| `timeout_prevote`   | 1s      | Voting phase                         |
| `timeout_precommit` | 1s      | Voting phase                         |
| `timeout_*_delta`   | 500ms   | Increase per round                   |

**Note:** Timeouts do NOT limit happy-path throughput. Blocks broadcast immediately when ready.

## Troubleshooting

### High CPU in consensus

1. Set `log_level = "info"` (not debug)
2. Set `runtime.flavor = "multi_threaded"`

### Log I/O bottleneck

Symptoms: high disk write, slow consensus.
Fix: `log_level = "info"` or `"warn"`

### Proposals timing out

1. Check EL (load-reth) health
2. Verify Engine API IPC responsive
3. Check network latency between validators

## Related

- [Block Timing](./block-timing.md) - timestamp invariants
- [EL Gas Limits](./el-gas-limits.md) - builder/txpool config
