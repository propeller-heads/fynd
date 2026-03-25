# shellcheck shell=bash
# Shared setup for dev-env.sh and run-example.sh.
# Source this file; do not run it directly.
#
# Defines: check_deps, start_anvil, wrap_weth, start_fynd
# Sets:    REPO_ROOT, ANVIL_RPC_URL, FYND_URL, FORK_RPC_URL
# Registers an EXIT trap to kill background processes on exit.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FORK_RPC_URL="${FORK_RPC_URL:-https://reth-ethereum.ithaca.xyz/rpc}"
ANVIL_RPC_URL="http://localhost:8545"
FYND_URL="http://localhost:3000"

_MNEMONIC="undo about satisfy liberty crime forget extra erode fever peasant ability cotton"
_PRIVATE_KEY="0x02d483ff876e4d1d55ddc829a22df2707bd2574ba18d0d870ef9c9edd3c0fe29"
_WETH="0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
_ANVIL_PID=""
_FYND_PID=""

_cleanup() {
    [[ -n "$_ANVIL_PID" ]] && kill "$_ANVIL_PID" 2>/dev/null || true
    [[ -n "$_FYND_PID" ]] && kill "$_FYND_PID" 2>/dev/null || true
    echo "Dev environment stopped."
}
trap _cleanup EXIT

check_deps() {
    local missing=()
    command -v anvil &>/dev/null || missing+=("anvil")
    command -v cast  &>/dev/null || missing+=("cast")
    if [[ ${#missing[@]} -gt 0 ]]; then
        printf "Missing tools: %s\n" "${missing[*]}"
        echo "Install Foundry: curl -L https://foundry.paradigm.xyz | bash && foundryup"
        exit 1
    fi
    if [[ -z "${TYCHO_API_KEY:-}" ]]; then
        echo "TYCHO_API_KEY is not set. Get one at: https://t.me/fynd_portal_bot"
        exit 1
    fi
}

start_anvil() {
    echo "==> Starting Anvil (fork: $FORK_RPC_URL)..."
    anvil \
        --fork-url "$FORK_RPC_URL" \
        --port 8545 \
        --mnemonic "$_MNEMONIC" \
        --balance 10000 \
        --silent &
    _ANVIL_PID=$!

    local i
    for i in $(seq 30); do
        cast block-number --rpc-url "$ANVIL_RPC_URL" &>/dev/null && break
        sleep 0.5
        if [[ $i -eq 30 ]]; then
            echo "Anvil failed to start within 15s"
            exit 1
        fi
    done
    echo "    Ready at $ANVIL_RPC_URL"
}

wrap_weth() {
    echo "==> Wrapping 1000 ETH → WETH..."
    cast send "$_WETH" "deposit()" \
        --value 1000ether \
        --private-key "$_PRIVATE_KEY" \
        --rpc-url "$ANVIL_RPC_URL" \
        --quiet
    echo "    Done"
}

start_fynd() {
    echo "==> Building and starting fynd serve (RPC: $ANVIL_RPC_URL, TYCHO_URL: $TYCHO_URL)..."
    cargo run --manifest-path "$REPO_ROOT/Cargo.toml" --release --quiet -- \
        serve --rpc-url "$ANVIL_RPC_URL" --tycho-url "$TYCHO_URL" &
    _FYND_PID=$!

    echo "    Waiting for fynd to be healthy (this may take a minute on first run)..."
    local i
    for i in $(seq 90); do
        if curl -sf "$FYND_URL/v1/health" &>/dev/null; then
            echo "    Ready at $FYND_URL"
            return
        fi
        sleep 2
        if ! kill -0 "$_FYND_PID" 2>/dev/null; then
            echo "fynd process exited unexpectedly. Check TYCHO_API_KEY and logs."
            exit 1
        fi
    done
    echo "fynd failed to become healthy within 3 minutes"
    exit 1
}
