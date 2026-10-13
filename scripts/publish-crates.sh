#!/usr/bin/env bash
# Publishes the fynd crates to crates.io with one `cargo publish` command.
#
#   scripts/publish-crates.sh            publish (needs CARGO_REGISTRY_TOKEN)
#   scripts/publish-crates.sh --dry-run  package and verify every crate, upload nothing
#
# One command for all crates lets cargo verify them together and upload them in dependency
# order. release.yaml publishes with it, and publish-cache.yaml runs the --dry-run on main so the
# release job finds a warm build cache for the same command.
#
# A real publish skips crates whose version is already on crates.io, so a release that failed
# part-way can be finished by running the job again. It retries once after 30s, because a crate
# can take a moment to appear in the crates.io index.
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=(fynd-core fynd-rpc-types fynd-client fynd-rpc fynd-test-fixtures fynd-tools-common
  fynd-bench-harness fynd)

mode="${1:-}"
if [ -n "${mode}" ] && [ "${mode}" != "--dry-run" ]; then
  echo "usage: $0 [--dry-run]" >&2
  exit 2
fi

version="$(cargo metadata --format-version 1 --no-deps \
  | jq -r '.packages[] | select(.name == "fynd") | .version')"

published() {
  curl -sf "https://crates.io/api/v1/crates/$1/${version}" \
    -H "User-Agent: fynd-ci (github.com/propeller-heads/fynd)" >/dev/null
}

publish() {
  local args=()
  for crate in "${CRATES[@]}"; do
    if [ -z "${mode}" ] && published "${crate}"; then
      echo "${crate} ${version} already on crates.io, skipping"
      continue
    fi
    args+=(--package "${crate}")
  done
  if [ "${#args[@]}" -eq 0 ]; then
    echo "every crate is already on crates.io at ${version}"
    return 0
  fi
  cargo publish --locked ${mode} "${args[@]}"
}

if publish; then
  exit 0
fi
if [ -n "${mode}" ]; then
  exit 1
fi
echo "publish failed, re-checking the index and retrying in 30s"
sleep 30
publish
