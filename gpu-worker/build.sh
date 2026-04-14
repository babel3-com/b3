#!/bin/bash
# Build GPU worker Docker image.
# Usage: ./build.sh [version]
# Example: ./build.sh v42
#
# Configuration is read from .env file (not committed):
#   DOCKER_ORG=your-dockerhub-org
#   HF_TOKEN=hf_xxx
#
# Or set environment variables directly:
#   DOCKER_ORG=myorg HF_TOKEN=hf_xxx ./build.sh v42

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# --- Load .env if present ---
ENV_FILE="$SCRIPT_DIR/.env"
if [ -f "$ENV_FILE" ]; then
    set -a
    source "$ENV_FILE"
    set +a
fi

# --- Version ---
VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version>"
    echo "Example: $0 v42"
    exit 1
fi

# --- Docker Org ---
DOCKER_ORG="${DOCKER_ORG:-}"
if [ -z "$DOCKER_ORG" ]; then
    echo "ERROR: DOCKER_ORG not set."
    echo "Set it in .env (DOCKER_ORG=your-org) or as an environment variable."
    echo "Example: DOCKER_ORG=myorg $0 $VERSION"
    exit 1
fi

# --- HF Token ---
HF_TOKEN="${HF_TOKEN:-}"
if [ -z "$HF_TOKEN" ]; then
    # Fallback: read from legacy .hf_token file
    TOKEN_FILE="$SCRIPT_DIR/.hf_token"
    if [ -f "$TOKEN_FILE" ]; then
        HF_TOKEN="$(cat "$TOKEN_FILE" | tr -d '[:space:]')"
    fi
fi
if [ -z "$HF_TOKEN" ]; then
    echo "ERROR: HF_TOKEN not set."
    echo "Set it in .env (HF_TOKEN=hf_xxx) or as an environment variable."
    exit 1
fi

IMAGE="$DOCKER_ORG/gpu-worker:$VERSION"

echo ""
echo "Building $IMAGE ..."
echo ""

docker build \
    -t "$IMAGE" \
    --build-arg "HF_TOKEN=$HF_TOKEN" \
    --build-arg "GPU_WORKER_VERSION=$VERSION" \
    .

echo ""
echo "Build complete: $IMAGE"
echo ""
echo "Next steps:"
echo "  docker push $IMAGE"
echo "  Update your RunPod template to $IMAGE"
