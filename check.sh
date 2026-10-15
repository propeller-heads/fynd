#!/usr/bin/env bash
set -e
cd "$(git rev-parse --show-toplevel)"

# The lint and format jobs in CI use a pinned nightly. Read that pin from its
# `toolchain: nightly-YYYY-MM-DD` line in the CI workflow, so a local run cannot use a different
# rustfmt or clippy than CI.
NIGHTLY="$(sed -nE 's/^[[:space:]]*toolchain:[[:space:]]*(nightly-[0-9]{4}-[0-9]{2}-[0-9]{2})[[:space:]]*$/\1/p' \
  .github/workflows/ci.yaml | head -n 1)"
if [ -z "${NIGHTLY}" ]; then
  echo "check.sh: no 'toolchain: nightly-YYYY-MM-DD' line found in .github/workflows/ci.yaml" >&2
  exit 1
fi

# The integration tests read Git LFS fixtures. Without `git lfs pull` the files are small pointer
# files, and the tests fail with "Unknown frame descriptor" instead of a clear error.
while IFS= read -r -d '' file; do
  if head -c 100 "${file}" | grep -q '^version https://git-lfs.github.com/spec/'; then
    echo "check.sh: ${file} is a Git LFS pointer. Run: git lfs install && git lfs pull" >&2
    exit 1
  fi
done < <(git ls-files -z ':(attr:filter=lfs)')

cargo +"${NIGHTLY}" fmt -- --check
cargo +"${NIGHTLY}" clippy --locked --all --all-features --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked \
  --package fynd-core --package fynd-rpc-types --package fynd-rpc --package fynd-client \
  --package fynd-bench-harness --package fynd-test-fixtures --package fynd-tools-common
cargo nextest run --workspace --locked --all-targets --all-features --bin fynd \
  -E 'not binary(algorithm_bench) and not binary(profile) and not binary(derived)'
