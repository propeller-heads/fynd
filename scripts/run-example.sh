#!/usr/bin/env bash
# Run a fynd-client example against a fresh local dev environment.
#
# Starts a forked Anvil node, wraps 1000 ETH to WETH, starts fynd serve,
# runs the example with RPC_URL pointed at Anvil, then tears everything down.
#
# Usage:
#   ./scripts/run-example.sh <example-name>
#
# Examples:
#   ./scripts/run-example.sh swap_erc20
#   ./scripts/run-example.sh swap_permit2
#   ./scripts/run-example.sh swap_client_fee
#   ./scripts/run-example.sh quote
#
# Options (env vars):
#   FORK_RPC_URL   Ethereum node to fork from
#                  (default: https://reth-ethereum.ithaca.xyz/rpc)
#   TYCHO_API_KEY  Required. Get one at: https://t.me/fynd_portal_bot
set -euo pipefail

EXAMPLE="${1:-}"
if [[ -z "$EXAMPLE" ]]; then
    echo "Usage: $0 <example-name>"
    echo ""
    echo "Available examples:"
    echo "  swap_erc20       ERC-20 approve + swap"
    echo "  swap_permit2     Permit2 approve + swap"
    echo "  swap_client_fee  Swap with client fee"
    echo "  quote            Quote only (no signing)"
    exit 1
fi

# shellcheck source=scripts/_env-setup.sh disable=SC1091
source "$(dirname "$0")/_env-setup.sh"

check_deps
start_anvil
wrap_weth
start_fynd

echo ""
echo "==> Running example: $EXAMPLE"
echo ""
RPC_URL="$ANVIL_RPC_URL" \
    cargo run \
        --manifest-path "$REPO_ROOT/Cargo.toml" \
        --example "$EXAMPLE" \
        --package fynd-client \
        --quiet
