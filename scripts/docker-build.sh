#!/usr/bin/env bash
set -euo pipefail

IMAGE="${1:-tycho-solver}"
TAG="${2:-latest}"
PUSH=false

for arg in "$@"; do
    if [ "$arg" = "--push" ]; then
        PUSH=true
    fi
done

echo "Building ${IMAGE}:${TAG} for linux/amd64..."

# Ensure buildx builder with QEMU support exists
if ! docker buildx inspect tycho-builder >/dev/null 2>&1; then
    echo "Creating buildx builder with QEMU support..."
    docker buildx create --name tycho-builder --use
    docker buildx inspect --bootstrap
fi

docker buildx use tycho-builder

if [ "$PUSH" = true ]; then
    echo "Building and pushing..."
    docker buildx build --platform linux/amd64 -t "${IMAGE}:${TAG}" --push .
else
    echo "Building and loading locally..."
    docker buildx build --platform linux/amd64 -t "${IMAGE}:${TAG}" --load .
fi

echo "Done: ${IMAGE}:${TAG}"
