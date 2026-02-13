#!/usr/bin/env bash
set -euo pipefail

IMAGE="tycho-solver"
TAG="latest"
PLATFORM=""
PUSH=false

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Build the tycho-solver Docker image.

Options:
  -i, --image NAME     Image name (default: tycho-solver)
  -t, --tag TAG        Image tag (default: latest)
  -p, --platform PLAT  Target platform, e.g. linux/amd64, linux/arm64
                       (default: host platform)
      --push           Push to registry instead of loading locally
  -h, --help           Show this help message
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -i|--image)    IMAGE="$2"; shift 2 ;;
        -t|--tag)      TAG="$2"; shift 2 ;;
        -p|--platform) PLATFORM="$2"; shift 2 ;;
        --push)        PUSH=true; shift ;;
        -h|--help)     usage ;;
        *) echo "Unknown option: $1"; usage ;;
    esac
done

PLATFORM_ARGS=()
if [ -n "$PLATFORM" ]; then
    PLATFORM_ARGS=(--platform "$PLATFORM")
    # Cross-compilation needs buildx with QEMU
    if ! docker buildx inspect tycho-builder >/dev/null 2>&1; then
        echo "Creating buildx builder with QEMU support..."
        docker buildx create --name tycho-builder --use
        docker buildx inspect --bootstrap
    fi
    docker buildx use tycho-builder
fi

PLATFORM_LABEL="${PLATFORM:-$(docker info --format '{{.OSType}}/{{.Architecture}}')}"
echo "Building ${IMAGE}:${TAG} for ${PLATFORM_LABEL}..."

if [ "$PUSH" = true ]; then
    echo "Building and pushing..."
    docker buildx build ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} -t "${IMAGE}:${TAG}" --push .
else
    echo "Building and loading locally..."
    docker buildx build ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} -t "${IMAGE}:${TAG}" --load .
fi

echo "Done: ${IMAGE}:${TAG}"
