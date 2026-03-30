#!/bin/bash

# Exit on error
set -e

# Function to handle errors
handle_error() {
    echo -e "${RED}Error: $1${NC}"
    exit 1
}

# Configuration
ECR_REGISTRY="120569639765.dkr.ecr.eu-central-1.amazonaws.com"
ECR_REPOSITORY="fynd"
AWS_REGION="eu-central-1"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Validate arguments
if [ $# -gt 1 ]; then
    handle_error "Too many arguments. Usage: $0 [image_tag]"
fi

IMAGE_TAG=${1:-$(git rev-parse --short HEAD)}
FULL_IMAGE_NAME="$ECR_REGISTRY/$ECR_REPOSITORY:$IMAGE_TAG"

echo -e "${BLUE}Building and Pushing Docker Image${NC}"
echo -e "${BLUE}Image tag: ${YELLOW}${IMAGE_TAG}${NC}"
echo -e "${BLUE}Full image: ${YELLOW}${FULL_IMAGE_NAME}${NC}"
echo ""

# Login to ECR
echo -e "${BLUE}Logging into ECR...${NC}"
aws ecr get-login-password --region "$AWS_REGION" | docker login --username AWS --password-stdin "$ECR_REGISTRY" || handle_error "Failed to login to ECR"

# Build image
echo -e "${BLUE}Building image...${NC}"
docker build --platform linux/amd64 -t "$FULL_IMAGE_NAME" . || handle_error "Failed to build Docker image"

# Push image
echo -e "${BLUE}Pushing image...${NC}"
docker push "$FULL_IMAGE_NAME" || handle_error "Failed to push image $FULL_IMAGE_NAME"

echo -e "${GREEN}Image pushed successfully!${NC}"
echo -e "${BLUE}Image: ${YELLOW}${FULL_IMAGE_NAME}${NC}"
echo ""
echo -e "${BLUE}Deploy with:${NC}"
echo ""
echo -e "  ${YELLOW}./deploy.sh ${IMAGE_TAG}${NC}"
echo ""
