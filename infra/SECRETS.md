# Secrets & Keys (Infra)

This document defines how we manage secrets/keys for multi-host networks.

## Goals

- Keep private material out of git (even in an open-source repo).
- Support team workflows where secrets can be stored **encrypted-at-rest**.
- Ensure redeploys are reproducible and do not rotate identities by accident.

## What is “secret” in this stack

- Ultramarine validator private key (consensus signing key).
- Ultramarine node secrets (P2P identity key, if/when separate from validator key).
- load-reth P2P key (stable `enode://…` identity).
- Engine API JWT secret (only when Engine API uses HTTP+JWT; IPC doesn’t need it).
- Archiver bearer token (`ULTRAMARINE_ARCHIVER_BEARER_TOKEN`).
- Grafana admin password (if using hosted monitoring).

## Storage model (recommended)

- **Do not commit raw secrets.**
- Store encrypted secrets per network as:
  - `ultramarine/infra/networks/<net>/secrets.sops.yaml` (committable; encrypted)
- Treat generated deploy artifacts as ephemeral:
  - `ultramarine/infra/networks/<net>/bundle/private/` (always gitignored)

## Encryption model (recommended)

- Use **SOPS + age**.
  - Each operator has an age keypair.
  - Recipients (public keys) are embedded in `secrets.sops.yaml` metadata.
  - Decryption occurs on the deploy controller (the machine running Ansible), not on remote hosts.
  - Optional (UX): commit an `ultramarine/infra/.sops.yaml` so creating new per-network secrets files is consistent and non-interactive. This file is not secret.

## Recipient management (age)

- Add a new recipient (operator) by updating the SOPS recipients on `secrets.sops.yaml` (e.g. `sops updatekeys` or `sops --add-age ...`), then re-encrypt.
- Removing a recipient is also a re-encrypt operation; treat it like access revocation and rotate any leaked tokens as needed.

## Deploy-time handling (Ansible)

- Ansible decrypts `secrets.sops.yaml` on the controller and writes per-node files on the target host:
  - root-owned, `0600` permissions
  - never passed as CLI flags
  - tasks must use `no_log: true`

## Netgen secrets input

`netgen` accepts `--secrets-file` pointing either to:

- a plaintext YAML file (never commit), or
- a SOPS-encrypted `secrets.sops.yaml` (requires `sops` available on PATH).

`netgen gen` fails by default if any validator is missing `archiver_bearer_token` (Ultramarine
validators fail fast without it). Use `--allow-missing-archiver-tokens` only for non-bootable
dry-runs.

When `--secrets-file` is provided, `netgen` emits derived per-node secret files under
`ultramarine/infra/networks/<net>/bundle/private/ultramarine/secrets/` (gitignored), suitable to
copy to hosts as root-owned `0600` environment files.

If `grafana_admin_password` is provided, `netgen` also emits:

- `ultramarine/infra/networks/<net>/bundle/private/monitoring/grafana_admin_password.env` (0600)

## Consensus node keys

`netgen gen` also maintains per-node consensus keys under:

- `ultramarine/infra/networks/<net>/bundle/private/ultramarine/homes/<node>/config/priv_validator_key.json`

Key rules:

- Keys are generated if missing, and reused on subsequent `netgen gen` runs for stable node identity.
- Only `role=validator` nodes are included in the genesis validator set; other nodes still need a key because Ultramarine derives its libp2p identity from it.
- Deploy copies keys to `/var/lib/ultramarine/<node>/config/priv_validator_key.json` **only if missing** (never overwrites).
- Treat these keys as secrets: keep them out of git and restrict permissions (`0600`).

Schema (v1):

```yaml
schema_version: 1
grafana_admin_password: "REDACTED"
nodes:
  node-0:
    archiver_bearer_token: "REDACTED"
  node-1:
    archiver_bearer_token: "REDACTED"
```

## Key persistence invariant

- “Deploy” must not overwrite any existing key material unless a rotate action is explicitly requested.
- Rotation is always an explicit op (e.g. `make net-rotate NET=<net> WHAT=archiver_token`).

## Rotation guidance (V0)

- Archiver token: safe to rotate; requires restart of Ultramarine validators.
- Engine JWT: safe to rotate; requires coordinated restart of paired CL+EL on the same host (HTTP engine mode only).
- Validator key rotation: treat as a **protocol/governance event** (often requires on-chain membership update or re-genesis); infra should refuse by default unless explicitly forced.
