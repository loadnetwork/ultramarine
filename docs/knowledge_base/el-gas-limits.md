# EL Gas Limits Configuration

## Load Network Constants

- `LOAD_EXECUTION_GAS_LIMIT = 2,000,000,000` (2B)
- Location: `load-reth/src/chainspec/mod.rs:32`

## Critical: Reth Defaults vs Load Requirements

| Parameter                | Reth Default (custom chain) | Load Requirement | CLI Flag                     |
| ------------------------ | --------------------------- | ---------------- | ---------------------------- |
| Builder gas limit        | 36M                         | 2B               | `--builder.gaslimit`         |
| Txpool gas limit         | 30M                         | 2B               | `--txpool.gas-limit`         |
| Txpool max-account-slots | 16                          | 32               | `--txpool.max-account-slots` |
| Txpool pending-max-size  | 20MB                        | 512MB            | `--txpool.pending-max-size`  |

## Builder Configuration

### --builder.gaslimit

Target gas limit for built blocks. Reth defaults custom chains to 36M.

- Code: `reth/crates/node/core/src/cli/config.rs:44-55`
- Load fix: `load-reth/src/engine/builder.rs:83` defaults to `LOAD_EXECUTION_GAS_LIMIT`

### --builder.interval

Period between payload rebuilds. Default 1s, recommended 50ms for Load.

### --builder.deadline

Time budget for payload build. Default 12s, recommended 2s for Load.
Too low = underfilled blocks. Too high = CL can't propose in time.

### --builder.max-tasks

Concurrent payload builds. Default 3, recommended 10 for Load.

## Txpool Configuration

### --txpool.gas-limit

Max gas limit for accepted transactions. **Must match builder.gaslimit**.

- Code: `reth/crates/transaction-pool/src/config.rs:125`
- Default: 30M (`ETHEREUM_BLOCK_GAS_LIMIT_30M`)
- Txs with `gas > txpool.gas-limit` rejected at pool ingress.

### --txpool.max-account-slots

Max pending txs per sender. Must be >= load-blaster `max_pending` (32).

### --txpool.pending-max-size / queued-max-size

Pool size in MB. 512MB recommended for high throughput.

## Systemd Template

`ultramarine/infra/templates/systemd/load-reth@.service.j2`:

```bash
--builder.gaslimit={{ loadnet_el_builder_gaslimit | default(2000000000) }}
--builder.interval={{ loadnet_el_builder_interval | default("50ms") }}
--builder.deadline={{ loadnet_el_builder_deadline | default(2) }}
--builder.max-tasks={{ loadnet_el_builder_max_tasks | default(10) }}
--txpool.gas-limit={{ loadnet_el_txpool_gas_limit | default(2000000000) }}
--txpool.max-account-slots={{ loadnet_el_txpool_max_account_slots | default(32) }}
--txpool.pending-max-count={{ loadnet_el_txpool_pending_max_count | default(50000) }}
--txpool.queued-max-count={{ loadnet_el_txpool_queued_max_count | default(50000) }}
--txpool.pending-max-size={{ loadnet_el_txpool_pending_max_size | default(512) }}
--txpool.queued-max-size={{ loadnet_el_txpool_queued_max_size | default(512) }}
```

## Troubleshooting

### Empty/underfilled blocks despite pending txs

1. Check `builder.gaslimit` = 2B
2. Check `txpool.gas-limit` = 2B
3. Increase `builder.deadline` if builds timeout

### Txs rejected "exceeds gas limit"

`txpool.gas-limit` < tx gas limit. Set to 2B.

### Pool shows 0 pending during load

`max-account-slots` < load-blaster `max_pending`. Set to 32+.
