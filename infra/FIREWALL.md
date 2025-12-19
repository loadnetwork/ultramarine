# Firewall & Ports (Public Testnet)

This document is provider-agnostic guidance for opening the minimum required ports for a public Load testnet.

## Principles

- Keep JSON-RPC, metrics, and debug endpoints off the public internet by default.
- Only expose P2P ports required for CL/EL connectivity.
- Prefer explicit allowlists and rate limits where your provider supports them.

## Inbound ports (required)

For each host running a node:

- EL (load-reth) P2P:
  - TCP: `ports.el.p2p`
  - UDP: `ports.el.p2p` (discovery)
- CL (Ultramarine) consensus P2P:
  - TCP: `ports.cl.p2p`
- CL (Ultramarine) mempool P2P:
  - TCP: `ports.cl.mempool`

The exact per-host port numbers are in `infra/networks/<net>/network.lock.json` (and `bundle/public/network.json`).

## Inbound ports (should stay private / localhost)

These are intentionally bound to `127.0.0.1` by default and should not be opened publicly:

- EL JSON-RPC HTTP: `ports.el.http`
- EL metrics: `ports.el.metrics`
- CL metrics: `ports.cl.metrics`

## Engine API

Deploys are IPC-only. The Engine socket lives at `/run/load-reth/<node>/engine.ipc` and is not network-exposed.

## Example: UFW (Ubuntu 22.04)

Given a host with:

- `el_p2p=30303`
- `cl_p2p=27000`
- `cl_mempool=28000`

Open only the required ports:

```bash
sudo ufw allow 30303/tcp
sudo ufw allow 30303/udp
sudo ufw allow 27000/tcp
sudo ufw allow 28000/tcp
sudo ufw enable
sudo ufw status verbose
```

## Automated (recommended)

Firewall rules can be applied automatically from `network.lock.json` via Ansible:

- Standalone: `make net-firewall NET=<net>`
- Integrated with deploy: `make net-deploy NET=<net> APPLY_FIREWALL=true`

Policy:

- Never lock out SSH (always allows `ansible_port`/22).
- Opens only P2P ports required for CL/EL connectivity.
- Keeps RPC/metrics closed by default (should remain bound to localhost).

## Storage note

For validator hosts, apply storage bootstrap before deploy so chain state lives on the data volume:

- `make net-storage NET=<net> ...`

## Notes

- If you change `ports.allocation` / `host_block_stride`, update firewall rules accordingly.
- If you later add RPC nodes, treat them separately: add authn/ratelimits/WAF, and never expose admin/debug APIs on the public internet.
