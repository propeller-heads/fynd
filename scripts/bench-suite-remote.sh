#!/usr/bin/env bash
set -euo pipefail

# Single-process remote driver for the benchmark suite. Provisions ONE EC2 box,
# then runs each config's audit + scale sequentially, fetching artifacts into the
# run directory. Invoked by scripts/bench_suite.py, which renders the run dir
# (worker_pools_<name>.toml, env/<name>.env, configs.list) beforehand.
#
# Required env: OUTPUT_DIR_REL, TYCHO_URL, TYCHO_API_KEY.
# Optional env: REGION, INSTANCE_TYPE, VOLUME_SIZE, AUDIT_ENABLED, SCALE_ENABLED,
#               POLL_TIMEOUT_SECS, RPC_URL, BEBOP_USER/KEY, HASHFLOW_USER/KEY.

# ---------- inputs ----------
OUTPUT_DIR_REL="${OUTPUT_DIR_REL:?OUTPUT_DIR_REL must be set}"
REGION="${REGION:-eu-west-1}"
INSTANCE_TYPE="${INSTANCE_TYPE:-c7a.8xlarge}"
VOLUME_SIZE="${VOLUME_SIZE:-60}"
AMI_ID="${AMI_ID:-}"
AUDIT_ENABLED="${AUDIT_ENABLED:-1}"
SCALE_ENABLED="${SCALE_ENABLED:-1}"
POLL_TIMEOUT_SECS="${POLL_TIMEOUT_SECS:-7200}"

TYCHO_URL="${TYCHO_URL:?TYCHO_URL must be set}"
TYCHO_URL="${TYCHO_URL#https://}"
TYCHO_URL="${TYCHO_URL#http://}"
TYCHO_API_KEY="${TYCHO_API_KEY:?TYCHO_API_KEY must be set}"
RPC_URL="${RPC_URL:-}"
BEBOP_USER="${BEBOP_USER:-}"
BEBOP_KEY="${BEBOP_KEY:-}"
HASHFLOW_USER="${HASHFLOW_USER:-}"
HASHFLOW_KEY="${HASHFLOW_KEY:-}"

KEY_NAME="bench-suite-$$"
SG_NAME="bench-suite-sg-$$"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY_FILE="/tmp/${KEY_NAME}.pem"
REMOTE_DIR="/home/ec2-user/fynd"
CLEANUP_ITEMS=()

# ---------- cleanup (trap) ----------
cleanup() {
	echo ""
	echo "=== Cleanup ==="
	if [[ ${#CLEANUP_ITEMS[@]} -eq 0 ]]; then
		echo "Nothing to clean up."
		return
	fi
	for item in "${CLEANUP_ITEMS[@]}"; do
		case "$item" in
		instance:*)
			local iid="${item#instance:}"
			echo "Terminating instance ${iid}..."
			aws ec2 terminate-instances --region "$REGION" --instance-ids "$iid" \
				--output text >/dev/null 2>&1 || true
			aws ec2 wait instance-terminated --region "$REGION" --instance-ids "$iid" 2>/dev/null || true
			;;
		sg:*)
			local sgid="${item#sg:}"
			echo "Deleting security group ${sgid}..."
			aws ec2 delete-security-group --region "$REGION" --group-id "$sgid" 2>/dev/null || true
			;;
		key:*)
			local kn="${item#key:}"
			echo "Deleting key pair ${kn}..."
			aws ec2 delete-key-pair --region "$REGION" --key-name "$kn" 2>/dev/null || true
			rm -f "$KEY_FILE"
			;;
		esac
	done
	echo "Cleanup complete."
}
trap cleanup EXIT

# ---------- helper: run a remote command detached, poll until done ----------
# Args: $1 = path prefix under REMOTE_DIR for .out/.done, $2 = remote script path.
remote_detached() {
	local prefix="$1" script="$2"
	$SSH "cd ${REMOTE_DIR} && rm -f ${prefix}.done ${prefix}.out && \
		nohup bash -c 'bash ${script} > ${prefix}.out 2>&1; echo \$? > ${prefix}.done' \
		>/dev/null 2>&1 & echo launched" >/dev/null
	local deadline=$(($(date +%s) + POLL_TIMEOUT_SECS))
	while :; do
		if [[ $(date +%s) -gt $deadline ]]; then
			echo "ERROR: ${prefix} did not finish within ${POLL_TIMEOUT_SECS}s"
			return 1
		fi
		sleep 30
		local rc progress
		rc="$($SSH "cat ${REMOTE_DIR}/${prefix}.done 2>/dev/null" 2>/dev/null || true)"
		progress="$($SSH "tail -1 ${REMOTE_DIR}/${prefix}.out 2>/dev/null" 2>/dev/null || true)"
		[[ -n "$progress" ]] && echo "  [${prefix##*/}] ${progress}"
		if [[ -n "$rc" ]]; then
			[[ "$rc" != "0" ]] && {
				echo "ERROR: ${prefix} exited ${rc}"
				return 1
			}
			return 0
		fi
	done
}

# ---------- provision ----------
echo "=== Benchmark Suite Runner (${REGION}, ${INSTANCE_TYPE}) ==="
if [[ -z "$AMI_ID" ]]; then
	AMI_ID=$(aws ec2 describe-images --region "$REGION" --owners amazon \
		--filters "Name=name,Values=al2023-ami-2023*-x86_64" "Name=state,Values=available" \
		--query 'sort_by(Images, &CreationDate)[-1].ImageId' --output text)
fi
echo "AMI: ${AMI_ID}"

aws ec2 create-key-pair --region "$REGION" --key-name "$KEY_NAME" \
	--query 'KeyMaterial' --output text >"$KEY_FILE"
chmod 600 "$KEY_FILE"
CLEANUP_ITEMS+=("key:${KEY_NAME}")

VPC_ID=$(aws ec2 describe-vpcs --region "$REGION" --filters "Name=isDefault,Values=true" \
	--query 'Vpcs[0].VpcId' --output text)
SG_ID=$(aws ec2 create-security-group --region "$REGION" --group-name "$SG_NAME" \
	--description "Temporary SG for benchmark suite" --vpc-id "$VPC_ID" \
	--query 'GroupId' --output text)
CLEANUP_ITEMS+=("sg:${SG_ID}")

MY_IP=$(curl -s https://checkip.amazonaws.com)
aws ec2 authorize-security-group-ingress --region "$REGION" --group-id "$SG_ID" \
	--protocol tcp --port 22 --cidr "${MY_IP}/32" --output text >/dev/null
echo "SG ${SG_ID} allows SSH from ${MY_IP}"

INSTANCE_ID=$(aws ec2 run-instances --region "$REGION" --image-id "$AMI_ID" \
	--instance-type "$INSTANCE_TYPE" --key-name "$KEY_NAME" --security-group-ids "$SG_ID" \
	--block-device-mappings "DeviceName=/dev/xvda,Ebs={VolumeSize=${VOLUME_SIZE},VolumeType=gp3}" \
	--tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=fynd-benchmark-suite}]" \
	--query 'Instances[0].InstanceId' --output text)
CLEANUP_ITEMS+=("instance:${INSTANCE_ID}")
echo "Instance: ${INSTANCE_ID}"

aws ec2 wait instance-running --region "$REGION" --instance-ids "$INSTANCE_ID"
PUBLIC_IP=$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
	--query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "Public IP: ${PUBLIC_IP}"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR -o ServerAliveInterval=15 -o ServerAliveCountMax=8"
SSH="ssh ${SSH_OPTS} -i ${KEY_FILE} ec2-user@${PUBLIC_IP}"

echo "Waiting for SSH..."
for i in $(seq 1 30); do
	$SSH true 2>/dev/null && break
	[[ $i -eq 30 ]] && {
		echo "ERROR: SSH not available"
		exit 1
	}
	sleep 5
done
echo "SSH connected."

# ---------- install + sync + build ----------
echo "=== Installing toolchain ==="
$SSH <<'INSTALL_EOF'
set -euo pipefail
sudo dnf install -y gcc gcc-c++ make openssl-devel pkg-config git rsync --quiet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
INSTALL_EOF

echo "=== Syncing code ==="
rsync -az -e "ssh ${SSH_OPTS} -i ${KEY_FILE}" \
	--exclude target/ --exclude .git/ --exclude .idea/ --exclude node_modules/ --exclude .venv-bench/ \
	"${REPO_ROOT}/" "ec2-user@${PUBLIC_IP}:${REMOTE_DIR}/"

echo "=== Building (fynd + fynd-benchmark, release) ==="
$SSH <<'BUILD_EOF'
set -euo pipefail
source "$HOME/.cargo/env"
cd ~/fynd
cargo build --release -p fynd-benchmark 2>&1 | tail -3
cargo build --release --bin fynd 2>&1 | tail -3
echo "Build complete."
BUILD_EOF

if [[ "$AUDIT_ENABLED" == "1" ]]; then
	echo "=== Downloading trade dataset ==="
	$SSH "source ~/.cargo/env && cd ${REMOTE_DIR} && ./target/release/fynd-benchmark download-trades"
fi

# ---------- per-config loop ----------
# Read names into an array first (bash 3.2 has no mapfile), then iterate without a
# stdin redirect so the inner ssh calls don't consume the loop's input.
CONFIG_NAMES=()
while IFS= read -r line || [[ -n "$line" ]]; do
	[[ -n "$line" ]] && CONFIG_NAMES+=("$line")
done <"${REPO_ROOT}/${OUTPUT_DIR_REL}/configs.list"

for NAME in "${CONFIG_NAMES[@]}"; do
	[[ -z "$NAME" ]] && continue
	echo ""
	echo "########## config: ${NAME} ##########"
	# shellcheck source=/dev/null
	source "${REPO_ROOT}/${OUTPUT_DIR_REL}/env/${NAME}.env"
	HTTP_PORT="${HTTP_PORT:-3000}"

	if [[ "$AUDIT_ENABLED" == "1" ]]; then
		echo "--- audit: starting solver ---"
		MIN_TVL_FLAG=""
		[[ -n "${MIN_TVL:-}" ]] && MIN_TVL_FLAG="--min-tvl ${MIN_TVL}"
		RPC_FLAG=""
		[[ -n "$RPC_URL" ]] && RPC_FLAG="--rpc-url ${RPC_URL}"
		$SSH "cat > ${REMOTE_DIR}/serve_${NAME}.sh" <<SERVE_EOF
#!/usr/bin/env bash
set -euo pipefail
source "\$HOME/.cargo/env"
cd ${REMOTE_DIR}
RPC_URL="${RPC_URL}" TYCHO_API_KEY="${TYCHO_API_KEY}" \\
BEBOP_USER="${BEBOP_USER}" BEBOP_KEY="${BEBOP_KEY}" \\
HASHFLOW_USER="${HASHFLOW_USER}" HASHFLOW_KEY="${HASHFLOW_KEY}" RUST_LOG=info \\
nohup ./target/release/fynd serve \\
    --worker-pools-config "${POOL_CONFIG}" \\
    --protocols "${PROTOCOLS}" ${MIN_TVL_FLAG} ${RPC_FLAG} \\
    --tycho-url "${TYCHO_URL}" --tycho-api-key "${TYCHO_API_KEY}" \\
    --http-port ${HTTP_PORT} \\
    > ${OUTPUT_DIR_REL}/serve_${NAME}.log 2>&1 &
echo \$! > serve_${NAME}.pid
SERVE_EOF
		$SSH "cd ${REMOTE_DIR} && bash serve_${NAME}.sh && echo started"

		echo "--- audit: waiting for solver health ---"
		HEALTH_N=$(((${HEALTH_TIMEOUT_SECS:-600}) / 5))
		$SSH "for i in \$(seq 1 ${HEALTH_N}); do curl -sf localhost:${HTTP_PORT}/v1/health 2>/dev/null | grep -q '\"healthy\":true' && { echo healthy; exit 0; }; sleep 5; done; echo 'ERROR: health timeout'; exit 1"

		echo "--- audit: running ---"
		audit_cmd="./target/release/fynd-benchmark audit --fynd-url http://localhost:${HTTP_PORT} --output \"${AUDIT_OUTPUT}\""
		[[ -n "${TRADE_DATA:-}" ]] && audit_cmd+=" --trade-data \"${TRADE_DATA}\""
		[[ -n "${TOP_PAIRS:-}" ]] && audit_cmd+=" --top-pairs ${TOP_PAIRS}"
		[[ -n "${AMOUNTS_PER_PAIR:-}" ]] && audit_cmd+=" --amounts-per-pair ${AMOUNTS_PER_PAIR}"
		[[ -n "${BLOCK_STRIDE:-}" ]] && audit_cmd+=" --block-stride ${BLOCK_STRIDE}"
		[[ -n "${MIN_AMOUNT_USD:-}" ]] && audit_cmd+=" --min-amount-usd ${MIN_AMOUNT_USD}"
		[[ -n "${QUOTE_TIMEOUT_MS:-}" ]] && audit_cmd+=" --timeout-ms ${QUOTE_TIMEOUT_MS}"
		[[ -n "${CONCURRENCY:-}" ]] && audit_cmd+=" --concurrency ${CONCURRENCY}"
		[[ -n "${NORDSTERN_RPS:-}" ]] && audit_cmd+=" --nordstern-rps ${NORDSTERN_RPS}"
		[[ -n "${KYBERSWAP_RPS:-}" ]] && audit_cmd+=" --kyberswap-rps ${KYBERSWAP_RPS}"
		[[ -n "${ZEROX_RPS:-}" ]] && audit_cmd+=" --zerox-rps ${ZEROX_RPS}"
		[[ -n "${AGGREGATOR_MAX_RETRIES:-}" ]] && audit_cmd+=" --aggregator-max-retries ${AGGREGATOR_MAX_RETRIES}"
		[[ -n "${AGGREGATOR_RETRY_BASE_MS:-}" ]] && audit_cmd+=" --aggregator-retry-base-ms ${AGGREGATOR_RETRY_BASE_MS}"
		[[ -n "${ETH_CALL_SLIPPAGE_BPS:-}" ]] && audit_cmd+=" --eth-call-slippage-bps ${ETH_CALL_SLIPPAGE_BPS}"
		[[ -n "${ETH_CALL_BASELINE_FEE_BPS:-}" ]] && audit_cmd+=" --eth-call-baseline-fee-bps ${ETH_CALL_BASELINE_FEE_BPS}"
		[[ -n "${NORDSTERN_URL:-}" ]] && audit_cmd+=" --nordstern-url \"${NORDSTERN_URL}\""
		[[ -n "${CHAIN_ID:-}" ]] && audit_cmd+=" --chain-id ${CHAIN_ID}"
		[[ -n "$RPC_URL" ]] && audit_cmd+=" --rpc-url \"${RPC_URL}\""
		$SSH "cat > ${REMOTE_DIR}/audit_${NAME}.sh" <<AUDIT_EOF
#!/usr/bin/env bash
set -euo pipefail
source "\$HOME/.cargo/env"
cd ${REMOTE_DIR}
RPC_URL="${RPC_URL}" RUST_LOG=info ${audit_cmd}
AUDIT_EOF
		remote_detached "${OUTPUT_DIR_REL}/audit_${NAME}" "audit_${NAME}.sh"

		echo "--- audit: stopping solver ---"
		$SSH "kill \$(cat ${REMOTE_DIR}/serve_${NAME}.pid) 2>/dev/null || true; sleep 3"
	fi

	if [[ "$SCALE_ENABLED" == "1" ]]; then
		echo "--- scale: running ---"
		MIN_TVL_FLAG=""
		[[ -n "${MIN_TVL:-}" ]] && MIN_TVL_FLAG="--min-tvl ${MIN_TVL}"
		REQ_FLAG=""
		[[ -n "${REQUESTS_FILE:-}" ]] && REQ_FLAG="--requests-file ${REQUESTS_FILE}"
		$SSH "cat > ${REMOTE_DIR}/scale_${NAME}.sh" <<SCALE_EOF
#!/usr/bin/env bash
set -euo pipefail
source "\$HOME/.cargo/env"
cd ${REMOTE_DIR}
RPC_URL="${RPC_URL}" TYCHO_API_KEY="${TYCHO_API_KEY}" \\
BEBOP_USER="${BEBOP_USER}" BEBOP_KEY="${BEBOP_KEY}" \\
HASHFLOW_USER="${HASHFLOW_USER}" HASHFLOW_KEY="${HASHFLOW_KEY}" RUST_LOG=info \\
./target/release/fynd-benchmark scale \\
    --base-config "${POOL_CONFIG}" \\
    --worker-counts "${WORKER_COUNTS}" \\
    --protocols "${PROTOCOLS}" ${MIN_TVL_FLAG} ${REQ_FLAG} \\
    --tycho-url "${TYCHO_URL}" --tycho-api-key "${TYCHO_API_KEY}" \\
    --http-port ${HTTP_PORT} \\
    -n ${NUM_REQUESTS} -m "${MODE}" \\
    --warmup-secs ${WARMUP_SECS:-30} \\
    --health-timeout-secs ${HEALTH_TIMEOUT_SECS:-600} \\
    --output-file "${SCALE_OUTPUT}"
SCALE_EOF
		remote_detached "${OUTPUT_DIR_REL}/scale_${NAME}" "scale_${NAME}.sh"
	fi
done

# ---------- fetch artifacts ----------
echo ""
echo "=== Fetching artifacts ==="
rsync -az -e "ssh ${SSH_OPTS} -i ${KEY_FILE}" \
	"ec2-user@${PUBLIC_IP}:${REMOTE_DIR}/${OUTPUT_DIR_REL}/" \
	"${REPO_ROOT}/${OUTPUT_DIR_REL}/"
echo "Artifacts in ${REPO_ROOT}/${OUTPUT_DIR_REL}/"
echo "=== Remote run complete; instance will be torn down ==="
