#!/bin/bash
# GPU Worker Entrypoint
#
# Required env vars for production:
#   GPU_SECRET          — authenticates billing calls to EC2 (admin-provisioned)
#   GPU_WORKER_KEY      — authenticates relay registration with EC2
#   B3_API_URL          — EC2 base URL for relay registration (e.g. https://babel3.com)
#   B3_AGENT_ID         — agent that owns this worker
#
# Optional:
#   BILLING_URL         — defaults to https://babel3.com
#   PORT                — defaults to 5125
#   B3_WORKER_ID        — unique worker identifier (defaults to RUNPOD_POD_ID or random UUID)
#   GPU_RELAY_PORT      — relay listen port (defaults to 5131)

# ── Reliability Relay + EC2 Registration ────────────────────────────
export B3_WORKER_ID=${B3_WORKER_ID:-${RUNPOD_POD_ID:-$(cat /proc/sys/kernel/random/uuid)}}
RELAY_PORT=${GPU_RELAY_PORT:-5131}
if [ -x /app/b3-gpu-relay ]; then
    echo "Starting GPU relay on port ${RELAY_PORT} (worker: ${B3_WORKER_ID})..."
    B3_API_URL=${B3_API_URL:-https://babel3.com} \
    GPU_WORKER_KEY=${GPU_WORKER_KEY:-} \
    B3_WORKER_ID=${B3_WORKER_ID} \
    LOCAL_GPU_URL=${LOCAL_GPU_URL:-http://localhost:${PORT:-5125}} \
    GPU_RELAY_PORT=${RELAY_PORT} \
    RUST_LOG=${RUST_LOG:-info} \
    /app/b3-gpu-relay > /tmp/b3-relay.log 2>&1 &
    RELAY_PID=$!
    echo "GPU relay started (PID: $RELAY_PID)"
    sleep 2  # give relay time to register with EC2
else
    echo "WARNING: /app/b3-gpu-relay not found — EC2 registration unavailable"
fi

# ── Launch GPU Worker ─────────────────────────────────────────────────
if [ -n "$RUNPOD_POD_ID" ] || [ -n "$RUNPOD_ENDPOINT_ID" ]; then
    echo "MODE: RunPod serverless (endpoint: ${RUNPOD_ENDPOINT_ID:-unknown})"
    exec python3 -u /app/handler.py
else
    echo "MODE: Local GPU server (port ${PORT:-5125})"
    exec python3 -u /app/local_server.py
fi
