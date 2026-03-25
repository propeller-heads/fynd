#!/usr/bin/env bash
# Start a local dev environment for running fynd-client examples.
#
# Starts a forked Anvil node with a funded account, wraps 1000 ETH to WETH,
# then runs fynd serve. Runs until interrupted (Ctrl-C).
#
# Usage:
#   ./scripts/dev-env.sh
#
# Options (env vars):
#   FORK_RPC_URL   Ethereum node to fork from
#                  (default: https://reth-ethereum.ithaca.xyz/rpc)
#   TYCHO_API_KEY  Required. Get one at: https://t.me/fynd_portal_bot
#
# The funded account:
#   Private key: 0x912a64d0474cbddb4afd9b1aa2e800c433a3e975fa858395e6134220cf2b4cd5
#   Balance:     10 000 ETH + 1 000 WETH
#
# Once running, point examples at the local stack:
#   RPC_URL=http://localhost:8545 cargo run --example swap_erc20 -p fynd-client
#
# Or use run-example.sh which handles this automatically:
#   ./scripts/run-example.sh swap_erc20
set -euo pipefail

# shellcheck source=scripts/_env-setup.sh
source "$(dirname "$0")/_env-setup.sh"

check_deps
start_anvil
wrap_weth
start_fynd

echo ""
echo "Dev environment ready."
echo "  Anvil RPC : $ANVIL_RPC_URL"
echo "  Fynd      : $FYND_URL"
echo ""
echo "Run an example:"
echo "  RPC_URL=$ANVIL_RPC_URL cargo run --example swap_erc20 -p fynd-client"
echo "  ./scripts/run-example.sh swap_erc20"
echo ""
echo "Press Ctrl-C to stop."

wait "$_FYND_PID"
