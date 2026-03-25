#!/usr/bin/env bash
# Run all fynd-client examples against a single shared dev environment.
#
# Starts Anvil + fynd serve once, runs every example, then tears down.
# Intended for CI; for running a single example interactively use
# run-example.sh instead.
#
# Usage:
#   ./scripts/run-all-examples.sh
#
# Options (env vars):
#   FORK_RPC_URL   Ethereum node to fork from
#                  (default: https://reth-ethereum.ithaca.xyz/rpc)
#   TYCHO_API_KEY  Required. Get one at: https://t.me/fynd_portal_bot
#   TYCHO_URL      Tycho endpoint (optional, uses fynd default if unset)
set -euo pipefail

# shellcheck source=scripts/_env-setup.sh disable=SC1091
source "$(dirname "$0")/_env-setup.sh"

EXAMPLES=(swap_erc20 swap_permit2 swap_client_fee quote)

check_deps
start_anvil
wrap_weth
start_fynd

echo ""
failed=()
for example in "${EXAMPLES[@]}"; do
    echo "==> Running: $example"
    if cargo run \
        --manifest-path "$REPO_ROOT/Cargo.toml" \
        --example "$example" \
        --package fynd-client \
        --quiet 2>&1; then
        echo "    PASS: $example"
    else
        echo "    FAIL: $example"
        failed+=("$example")
    fi
    echo ""
done

if [[ ${#failed[@]} -gt 0 ]]; then
    printf "Failed examples: %s\n" "${failed[*]}"
    exit 1
fi

echo "All examples passed."
