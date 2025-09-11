#!/usr/bin/env bash

# This script takes:
# - a number of nodes to run as an argument,
# - the home directory for the nodes configuration folders

function help {
    echo "Usage: spawn.sh [--help] --nodes NODES_COUNT --home NODES_HOME [--app APP_BINARY] [--no-reset] [--engine-ipc-base DIR] [--jwt-path FILE] [--extra-args '...'] [--no-build] [--ignore-propose-timeout]"
}

# Parse arguments
while [[ "$#" -gt 0 ]]; do
    case $1 in
        --help) help; exit 0 ;;
        --nodes) NODES_COUNT="$2"; shift ;;
        --home) NODES_HOME="$2"; shift ;;
        --app) APP_BINARY="$2"; shift ;;
        --engine-ipc-base) ENGINE_IPC_BASE="$2"; shift ;;
        --jwt-path) JWT_PATH="$2"; shift ;;
        --extra-args) EXTRA_ARGS="$2"; shift ;;
        --no-reset) NO_RESET=1; shift ;;
        --ignore-propose-timeout) IGNORE_PROPOSE_TIMEOUT=1; shift ;;
        --no-build) NO_BUILD=1; shift ;;
        *) echo "Unknown parameter passed: $1"; help; exit 1 ;;
    esac
    shift
done

# Check required arguments
if [[ -z "$NODES_COUNT" ]]; then
    help
    exit 1
fi

if [[ -z "$NODES_HOME" ]]; then
    help
    exit 1
fi

if [[ -z "$APP_BINARY" ]]; then
    APP_BINARY="ultramarine"
fi

if [[ -z "$NO_BUILD" ]]; then
    echo "Compiling '$APP_BINARY'..."
    cargo build -p $APP_BINARY
else
    echo "Skipping build for '$APP_BINARY' (--no-build set)"
fi

export RUST_BACKTRACE=full

# Create nodes and logs directories, run nodes
for NODE in $(seq 0 $((NODES_COUNT - 1))); do
    if [[ -z "$NO_RESET" ]]; then
        echo "[Node $NODE] Resetting the database..."
        rm -rf "$NODES_HOME/$NODE/db"
        mkdir -p "$NODES_HOME/$NODE/db"
        rm -rf "$NODES_HOME/$NODE/wal"
        mkdir -p "$NODES_HOME/$NODE/wal"
    fi

    rm -rf "$NODES_HOME/$NODE/logs"
    mkdir -p "$NODES_HOME/$NODE/logs"

    rm -rf "$NODES_HOME/$NODE/traces"
    mkdir -p "$NODES_HOME/$NODE/traces"

    echo "[Node $NODE] Spawning node..."

    # Optional behavior toggles
    if [[ -n "$IGNORE_PROPOSE_TIMEOUT" ]]; then
        export ULTRAMARINE_IGNORE_PROPOSE_TIMEOUT=1
    fi
    # Compose per-node CLI args
    NODE_ARGS=(start --home "$NODES_HOME/$NODE")
    if [[ -n "$ENGINE_IPC_BASE" ]]; then
        NODE_ARGS+=(--engine-ipc-path "$ENGINE_IPC_BASE/$NODE/engine.ipc")
    fi
    if [[ -n "$JWT_PATH" ]]; then
        NODE_ARGS+=(--jwt-path "$JWT_PATH")
    fi
    if [[ -n "$EXTRA_ARGS" ]]; then
        # shellcheck disable=SC2206
        NODE_ARGS+=( $EXTRA_ARGS )
    fi
    # When using Engine IPC, wait for the IPC socket to exist to avoid race conditions
    if [[ -n "$ENGINE_IPC_BASE" ]]; then
        ENGINE_IPC_TIMEOUT="${ENGINE_IPC_TIMEOUT:-60}"
        ENGINE_IPC_PATH="$ENGINE_IPC_BASE/$NODE/engine.ipc"
        # Ensure directory exists (harmless if already present)
        mkdir -p "$(dirname "$ENGINE_IPC_PATH")"

        echo "[Node $NODE] Waiting for Engine IPC socket: $ENGINE_IPC_PATH (timeout: ${ENGINE_IPC_TIMEOUT}s)"
        SECS_WAITED=0
        while [[ ! -S "$ENGINE_IPC_PATH" && $SECS_WAITED -lt $ENGINE_IPC_TIMEOUT ]]; do
            sleep 1
            SECS_WAITED=$((SECS_WAITED + 1))
        done
        if [[ ! -S "$ENGINE_IPC_PATH" ]]; then
            echo "[Node $NODE] ERROR: Engine IPC socket not found after ${ENGINE_IPC_TIMEOUT}s: $ENGINE_IPC_PATH"
            echo "[Node $NODE] Hint: ensure Docker stack (compose.ipc.yaml) is running and reth has started."
            exit 1
        fi
        echo "[Node $NODE] Found Engine IPC socket. Launching node..."
    fi

    cargo run --bin $APP_BINARY -q -- "${NODE_ARGS[@]}" > "$NODES_HOME/$NODE/logs/node.log" 2>&1 &
    echo $! > "$NODES_HOME/$NODE/node.pid"
    echo "[Node $NODE] Logs are available at: $NODES_HOME/$NODE/logs/node.log"
done

# Function to handle cleanup on interrupt
function exit_and_cleanup {
    echo "Stopping all nodes..."
    for NODE in $(seq 0 $((NODES_COUNT - 1))); do
        NODE_PID=$(cat "$NODES_HOME/$NODE/node.pid")
        echo "[Node $NODE] Stopping node (PID: $NODE_PID)..."
        kill "$NODE_PID"
    done
    exit 0
}

# Trap the INT signal (Ctrl+C) to run the cleanup function
trap exit_and_cleanup INT

echo "Spawned $NODES_COUNT nodes."
echo "Press Ctrl+C to stop the nodes."

# Keep the script running
while true; do sleep 1; done
