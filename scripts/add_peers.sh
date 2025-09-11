#!/usr/bin/env bash

# Script to manually add peers (their enodes) to each node
set -euo pipefail

# --- Dependency checks -------------------------------------------------------
for dep in docker curl jq cast; do
  if ! command -v "$dep" >/dev/null 2>&1; then
    echo "ERROR: Required dependency not found: $dep" >&2
    exit 1
  fi
done

# --- Wait for HTTP JSON-RPC --------------------------------------------------
wait_http() {
  local url="$1"; shift
  local name="$1"; shift
  local tries=${1:-120}
  local delay=${2:-0.5}
  echo "Waiting for $name RPC at $url ..."
  for i in $(seq 1 "$tries"); do
    if curl -sS --fail --max-time 1 --connect-timeout 1 \
      -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"web3_clientVersion","params":[]}' \
      "$url" >/dev/null 2>&1; then
      echo "$name RPC is up."
      return 0
    fi
    sleep "$delay"
  done
  echo "ERROR: $name RPC did not become ready at $url" >&2
  return 1
}

# Resolve container IPs for enode substitution
RETH0_IP=$(docker inspect -f '{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}' reth0)
RETH1_IP=$(docker inspect -f '{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}' reth1)
RETH2_IP=$(docker inspect -f '{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}' reth2)

# Wait for HTTP RPC to accept connections (IPC mode still exposes HTTP for admin namespace)
wait_http http://127.0.0.1:8545 reth0
wait_http http://127.0.0.1:18545 reth1
wait_http http://127.0.0.1:28545 reth2

# Fetch enodes and rewrite 127.0.0.1 to container IPs
RETH0_ENODE=$(cast rpc --rpc-url http://127.0.0.1:8545 admin_nodeInfo | jq -r .enode | sed "s/127\.0\.0\.1/${RETH0_IP}/")
RETH1_ENODE=$(cast rpc --rpc-url http://127.0.0.1:18545 admin_nodeInfo | jq -r .enode | sed "s/127\.0\.0\.1/${RETH1_IP}/" )
RETH2_ENODE=$(cast rpc --rpc-url http://127.0.0.1:28545 admin_nodeInfo | jq -r .enode | sed "s/127\.0\.0\.1/${RETH2_IP}/" )

echo "RETH0_ENODE: ${RETH0_ENODE}"
cast rpc --rpc-url http://127.0.0.1:8545 admin_addTrustedPeer "${RETH1_ENODE}"
cast rpc --rpc-url http://127.0.0.1:8545 admin_addTrustedPeer "${RETH2_ENODE}"
cast rpc --rpc-url http://127.0.0.1:8545 admin_addPeer "${RETH1_ENODE}"
cast rpc --rpc-url http://127.0.0.1:8545 admin_addPeer "${RETH2_ENODE}"

echo "RETH1_ENODE: ${RETH1_ENODE}"
cast rpc --rpc-url http://127.0.0.1:18545 admin_addTrustedPeer "${RETH0_ENODE}"
cast rpc --rpc-url http://127.0.0.1:18545 admin_addTrustedPeer "${RETH2_ENODE}"
cast rpc --rpc-url http://127.0.0.1:18545 admin_addPeer "${RETH0_ENODE}"
cast rpc --rpc-url http://127.0.0.1:18545 admin_addPeer "${RETH2_ENODE}"

echo "RETH2_ENODE: ${RETH2_ENODE}"
cast rpc --rpc-url http://127.0.0.1:28545 admin_addTrustedPeer "${RETH0_ENODE}"
cast rpc --rpc-url http://127.0.0.1:28545 admin_addTrustedPeer "${RETH1_ENODE}"
cast rpc --rpc-url http://127.0.0.1:28545 admin_addPeer "${RETH0_ENODE}"
cast rpc --rpc-url http://127.0.0.1:28545 admin_addPeer "${RETH1_ENODE}"
