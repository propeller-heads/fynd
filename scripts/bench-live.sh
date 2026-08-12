#!/usr/bin/env bash
# Compare the routing algorithms against a market captured live from Tycho.
#
# Same benchmark as ./scripts/bench.sh, same output, same viewer -- only the market differs. This
# one connects to Tycho, takes the snapshot of one block and solves against that, instead of
# replaying the recorded fixture.
#
# What that buys: the fixture's format cannot serialize VM-backed states, so every Uniswap v4,
# Balancer, Curve and Maverick pool in it is a component with no state, and unroutable. Captured
# live they are all there.
#
# What it costs: a live run is a point-in-time market. Its configs compare with each other, not
# with any other run. The viewer keeps the two kinds apart for that reason.
#
# Needs TYCHO_URL and TYCHO_API_KEY in the environment or on the command line. Set RPC_URL too and
# the run prices gas at whatever the chain is charging.
#
# Usage:
#   ./scripts/bench-live.sh [options]
#
# Live options:
#   --protocols A,B          Protocol systems to stream. Default: every one Tycho has for the chain
#   --chain NAME             Chain to capture. Default: ethereum
#   --min-tvl X              Minimum component TVL in ETH. The main lever on market size
#   --min-token-quality N    Minimum token quality score
#   --traded-n-days-ago N    Only tokens traded within this many days
#   --capture-timeout-secs N How long to wait for the snapshot
#   --tycho-url URL          Overrides TYCHO_URL
#   --tycho-api-key KEY      Overrides TYCHO_API_KEY
#   --rpc-url URL            Overrides RPC_URL, read for the live gas price
#
# Everything else is handed to ./scripts/bench.sh unchanged -- --name, --orders, --configs and the
# rest work exactly as they do offline.
#
# Examples:
#   ./scripts/bench-live.sh --name live-now --orders 500
#   ./scripts/bench-live.sh --name uni-only --protocols uniswap_v2,uniswap_v3,uniswap_v4
#   ./scripts/bench-live.sh --name deep --min-tvl 50 --orders 2000
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

usage() {
  sed -n '2,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

# Checked here rather than left to the benchmark: it would connect, fail and print a Rust error
# after a build, which is a slow way to learn that a variable is unset.
missing=()
[[ -n "${TYCHO_URL:-}" ]] || printf '%s\n' "$@" | grep -q -- '--tycho-url' || missing+=("TYCHO_URL")
[[ -n "${TYCHO_API_KEY:-}" ]] || printf '%s\n' "$@" | grep -q -- '--tycho-api-key' ||
  missing+=("TYCHO_API_KEY")
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "error: ${missing[*]} not set — export it or pass the matching flag" >&2
  echo "       (see ./scripts/bench-live.sh --help)" >&2
  exit 1
fi

if [[ -z "${RPC_URL:-}" ]] && ! printf '%s\n' "$@" | grep -q -- '--rpc-url'; then
  echo "note: no RPC_URL, so the run cannot read the chain's gas price."
  echo "      Without --gas-price-gwei it will solve at the default."
fi

# --market live is the only thing this script really adds. bench.sh owns the build and forwards
# everything it does not recognise, so the two stay in step by construction.
exec "$REPO_ROOT/scripts/bench.sh" --market live "$@"
