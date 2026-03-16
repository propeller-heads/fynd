#!/usr/bin/env bash
set -euo pipefail

# ---------- configurable variables ----------
REGION="${AWS_REGION:-eu-west-1}"
INSTANCE_TYPE="${INSTANCE_TYPE:-c7a.8xlarge}" # 32 vCPU, compute-optimized
AMI_ID="${AMI_ID:-}"                          # auto-resolved if empty (Amazon Linux 2023)
WORKER_COUNTS="${WORKER_COUNTS:-1,2,4,8,12,16,20,24}"
NUM_REQUESTS="${NUM_REQUESTS:-200}"
CONCURRENCY_MODE="${CONCURRENCY_MODE:-fixed:48}"
WARMUP_SECS="${WARMUP_SECS:-30}"
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-600}"
PROTOCOLS="${PROTOCOLS:-uniswap_v2,uniswap_v3}"
TYCHO_URL="${TYCHO_URL:?TYCHO_URL must be set}"
# Strip protocol prefix — the solver handles TLS internally
TYCHO_URL="${TYCHO_URL#https://}"
TYCHO_URL="${TYCHO_URL#http://}"
TYCHO_API_KEY="${TYCHO_API_KEY:?TYCHO_API_KEY must be set}"
HTTP_PORT="${HTTP_PORT:-3456}"
POOL_CONFIG="${POOL_CONFIG:-single_pool.toml}"
REQUESTS_FILE="${REQUESTS_FILE:-tools/benchmark/requests_set.json}"
VOLUME_SIZE="${VOLUME_SIZE:-60}" # GB, needs space for Rust toolchain + build
KEY_NAME="bench-remote-$$"
SG_NAME="bench-remote-sg-$$"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# ---------- derived ----------
KEY_FILE="/tmp/${KEY_NAME}.pem"
REMOTE_DIR="/home/ec2-user/fynd"
CLEANUP_ITEMS=()

cleanup() {
	echo ""
	echo "=== Cleanup ==="
	for item in "${CLEANUP_ITEMS[@]}"; do
		case "$item" in
		instance:*)
			local iid="${item#instance:}"
			echo "Terminating instance ${iid}..."
			aws ec2 terminate-instances \
				--region "$REGION" \
				--instance-ids "$iid" \
				--output text >/dev/null 2>&1 || true
			echo "Waiting for instance termination..."
			aws ec2 wait instance-terminated \
				--region "$REGION" \
				--instance-ids "$iid" 2>/dev/null || true
			;;
		sg:*)
			local sgid="${item#sg:}"
			echo "Deleting security group ${sgid}..."
			aws ec2 delete-security-group \
				--region "$REGION" \
				--group-id "$sgid" 2>/dev/null || true
			;;
		key:*)
			local kn="${item#key:}"
			echo "Deleting key pair ${kn}..."
			aws ec2 delete-key-pair \
				--region "$REGION" \
				--key-name "$kn" 2>/dev/null || true
			rm -f "$KEY_FILE"
			;;
		esac
	done
	echo "Cleanup complete."
}
trap cleanup EXIT

echo "=== Remote Benchmark Runner ==="
echo "Region:        ${REGION}"
echo "Instance type: ${INSTANCE_TYPE}"
echo "Worker counts: ${WORKER_COUNTS}"
echo "Requests:      ${NUM_REQUESTS} per iteration, mode ${CONCURRENCY_MODE}"
echo ""

# ---------- resolve AMI ----------
if [[ -z "$AMI_ID" ]]; then
	echo "Resolving latest Amazon Linux 2023 AMI..."
	AMI_ID=$(aws ec2 describe-images \
		--region "$REGION" \
		--owners amazon \
		--filters \
		"Name=name,Values=al2023-ami-2023*-x86_64" \
		"Name=state,Values=available" \
		--query 'sort_by(Images, &CreationDate)[-1].ImageId' \
		--output text)
	echo "AMI: ${AMI_ID}"
fi

# ---------- create key pair ----------
echo "Creating key pair ${KEY_NAME}..."
aws ec2 create-key-pair \
	--region "$REGION" \
	--key-name "$KEY_NAME" \
	--query 'KeyMaterial' \
	--output text >"$KEY_FILE"
chmod 600 "$KEY_FILE"
CLEANUP_ITEMS+=("key:${KEY_NAME}")

# ---------- create security group ----------
echo "Creating security group..."
VPC_ID=$(aws ec2 describe-vpcs \
	--region "$REGION" \
	--filters "Name=isDefault,Values=true" \
	--query 'Vpcs[0].VpcId' \
	--output text)

SG_ID=$(aws ec2 create-security-group \
	--region "$REGION" \
	--group-name "$SG_NAME" \
	--description "Temporary SG for benchmark runner" \
	--vpc-id "$VPC_ID" \
	--query 'GroupId' \
	--output text)
CLEANUP_ITEMS+=("sg:${SG_ID}")

MY_IP=$(curl -s https://checkip.amazonaws.com)
aws ec2 authorize-security-group-ingress \
	--region "$REGION" \
	--group-id "$SG_ID" \
	--protocol tcp \
	--port 22 \
	--cidr "${MY_IP}/32" \
	--output text >/dev/null
echo "Security group ${SG_ID} allows SSH from ${MY_IP}"

# ---------- launch instance ----------
echo "Launching ${INSTANCE_TYPE} instance..."
INSTANCE_ID=$(aws ec2 run-instances \
	--region "$REGION" \
	--image-id "$AMI_ID" \
	--instance-type "$INSTANCE_TYPE" \
	--key-name "$KEY_NAME" \
	--security-group-ids "$SG_ID" \
	--block-device-mappings "DeviceName=/dev/xvda,Ebs={VolumeSize=${VOLUME_SIZE},VolumeType=gp3}" \
	--tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=fynd-benchmark-runner}]" \
	--query 'Instances[0].InstanceId' \
	--output text)
CLEANUP_ITEMS+=("instance:${INSTANCE_ID}")
echo "Instance: ${INSTANCE_ID}"

echo "Waiting for instance to be running..."
aws ec2 wait instance-running \
	--region "$REGION" \
	--instance-ids "$INSTANCE_ID"

PUBLIC_IP=$(aws ec2 describe-instances \
	--region "$REGION" \
	--instance-ids "$INSTANCE_ID" \
	--query 'Reservations[0].Instances[0].PublicIpAddress' \
	--output text)
echo "Public IP: ${PUBLIC_IP}"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"
SSH="ssh ${SSH_OPTS} -i ${KEY_FILE} ec2-user@${PUBLIC_IP}"
SCP="scp ${SSH_OPTS} -i ${KEY_FILE}"

echo "Waiting for SSH to become available..."
for i in $(seq 1 30); do
	if $SSH true 2>/dev/null; then
		break
	fi
	if [[ $i -eq 30 ]]; then
		echo "ERROR: SSH not available after 30 attempts"
		exit 1
	fi
	sleep 5
done
echo "SSH connected."

# ---------- install dependencies ----------
echo ""
echo "=== Installing dependencies on remote ==="
$SSH <<'INSTALL_EOF'
set -euo pipefail
sudo dnf install -y gcc gcc-c++ make openssl-devel pkg-config git rsync --quiet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"
rustc --version
cargo --version
INSTALL_EOF
echo "Rust toolchain installed."

# ---------- sync code ----------
echo ""
echo "=== Syncing code to remote ==="
rsync -az --progress \
	-e "ssh ${SSH_OPTS} -i ${KEY_FILE}" \
	--exclude target/ \
	--exclude .git/ \
	--exclude .idea/ \
	--exclude node_modules/ \
	"${REPO_ROOT}/" \
	"ec2-user@${PUBLIC_IP}:${REMOTE_DIR}/"
echo "Code synced."

# ---------- build ----------
echo ""
echo "=== Building fynd-benchmark (release) ==="
$SSH <<'BUILD_EOF'
set -euo pipefail
source "$HOME/.cargo/env"
cd ~/fynd
cargo build -p fynd-benchmark --release 2>&1 | tail -5
echo "Build complete."
BUILD_EOF

# ---------- run benchmark ----------
echo ""
echo "=== Running scale benchmark ==="
$SSH <<BENCH_EOF
set -euo pipefail
source "\$HOME/.cargo/env"
cd ~/fynd

RUST_LOG=info cargo run -p fynd-benchmark --release -- scale \\
    --base-config "${POOL_CONFIG}" \\
    --worker-counts "${WORKER_COUNTS}" \\
    --protocols "${PROTOCOLS}" \\
    --tycho-url "${TYCHO_URL}" \\
    --tycho-api-key "${TYCHO_API_KEY}" \\
    --http-port ${HTTP_PORT} \\
    -n ${NUM_REQUESTS} \\
    -m "${CONCURRENCY_MODE}" \\
    --requests-file "${REQUESTS_FILE}" \\
    --warmup-secs ${WARMUP_SECS} \\
    --health-timeout-secs ${HEALTH_TIMEOUT} \\
    --output-file scale_results_remote.json 2>&1 | \
    grep -E '(--- Testing|=== CPU|Pool:|Requests per|Workers|--------|^\s+[0-9]|INFO.*(Scale results|healthy|warming|Loaded|Worker counts))'
BENCH_EOF

# ---------- fetch results ----------
echo ""
echo "=== Fetching results ==="
$SCP "ec2-user@${PUBLIC_IP}:${REMOTE_DIR}/scale_results_remote.json" \
	"${REPO_ROOT}/scale_results_remote.json"
echo "Results saved to: ${REPO_ROOT}/scale_results_remote.json"

echo ""
echo "=== Done ==="
echo "Instance will be terminated during cleanup."
