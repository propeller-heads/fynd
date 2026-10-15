#!/usr/bin/env bash
# Publishes the fynd crates to crates.io with one `cargo publish` command.
#
#   scripts/publish-crates.sh --dry-run    package and compile every crate, upload nothing
#   scripts/publish-crates.sh --no-verify  package and upload, compile nothing (needs the token)
#   scripts/publish-crates.sh              package, compile, and upload (needs the token)
#
# One command for all crates lets cargo package them together and upload them in dependency order.
#
# release.yaml runs --dry-run in a job with the build cache and no secrets, then --no-verify in a
# job with the token and no cache, so no cached build script or proc-macro runs next to the token.
# publish-cache.yaml runs --dry-run on main to keep that cache warm.
#
# An upload skips crates whose version is already on crates.io, so a release that failed part-way
# can be finished by running the job again. It retries once after 30s, because a crate can take a
# moment to appear in the crates.io index.
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=(fynd-core fynd-rpc-types fynd-client fynd-rpc fynd-test-fixtures fynd-tools-common
  fynd-bench-harness fynd)

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [--dry-run | --no-verify]" >&2
  exit 2
fi
mode="${1:-}"
case "${mode}" in
  "" | --dry-run | --no-verify) ;;
  *)
    echo "usage: $0 [--dry-run | --no-verify]" >&2
    exit 2
    ;;
esac

version="$(cargo metadata --format-version 1 --no-deps \
  | jq -r '.packages[] | select(.name == "fynd") | .version')"

# 0 when the version is on crates.io, 1 when it is not, and exit when crates.io cannot answer: a
# failed lookup must not be read as "not published".
published() {
  local status
  status="$(curl -s -o /dev/null -w '%{http_code}' \
    "https://crates.io/api/v1/crates/$1/${version}" \
    -H "User-Agent: fynd-ci (github.com/propeller-heads/fynd)")"
  case "${status}" in
    200) return 0 ;;
    404) return 1 ;;
    *)
      echo "crates.io lookup for $1 ${version} failed (HTTP ${status})" >&2
      exit 1
      ;;
  esac
}

publish() {
  local args=()
  for crate in "${CRATES[@]}"; do
    if [ "${mode}" != "--dry-run" ] && published "${crate}"; then
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
if [ "${mode}" = "--dry-run" ]; then
  exit 1
fi
echo "publish failed, re-checking the index and retrying in 30s"
sleep 30
publish
