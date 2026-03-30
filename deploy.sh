#!/bin/bash

# Exit on error
set -e

# Function to handle errors
handle_error() {
    echo -e "${RED}Error: $1${NC}"
    exit 1
}

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
NAMESPACE="prod-turbine"
AWS_SECRET_NAME="prod/turbine/fynd"
HELM_RELEASE_NAME="fynd"

if [ -z "$1" ]; then
    handle_error "Image tag is required. Usage: $0 <image_tag>"
fi
IMAGE_TAG="$1"

echo -e "${BLUE}Deploying Fynd${NC}"
echo -e "${BLUE}Namespace: ${YELLOW}${NAMESPACE}${NC}"
echo -e "${BLUE}Image tag: ${YELLOW}${IMAGE_TAG}${NC}"
echo -e "${BLUE}Release name: ${YELLOW}${HELM_RELEASE_NAME}${NC}"
echo -e "${BLUE}AWS Secret: ${YELLOW}${AWS_SECRET_NAME}${NC}"
echo ""

# Copy config files into chart directory (single source of truth is repo root)
cp blacklist.toml deployment/blacklist.toml
cp worker_pools.toml deployment/worker_pools.toml

# Deploy with Helm
echo -e "${YELLOW}Deploying with Helm...${NC}"
helm upgrade --install "$HELM_RELEASE_NAME" deployment \
    --namespace "$NAMESPACE" \
    -f deployment/values.yaml \
    --set "image.tag=${IMAGE_TAG}" \
    --set "externalSecrets.enabled=true" \
    --set "externalSecrets.awsSecretName=${AWS_SECRET_NAME}" || handle_error "Failed to deploy with Helm"

echo -e "${GREEN}Deployment completed!${NC}"
echo ""

# Show status
echo -e "${YELLOW}Checking deployment status...${NC}"
kubectl get pods -n "$NAMESPACE" -l "app.kubernetes.io/instance=${HELM_RELEASE_NAME}"

echo ""
echo -e "${BLUE}Useful commands:${NC}"
echo -e "${BLUE}  Check logs: ${YELLOW}kubectl logs -n ${NAMESPACE} -l app.kubernetes.io/instance=${HELM_RELEASE_NAME}${NC}"
echo -e "${BLUE}  Check status: ${YELLOW}kubectl get pods -n ${NAMESPACE} -l app.kubernetes.io/instance=${HELM_RELEASE_NAME}${NC}"
echo -e "${BLUE}  Port forward: ${YELLOW}kubectl port-forward -n ${NAMESPACE} svc/${HELM_RELEASE_NAME} 3000:80${NC}"
