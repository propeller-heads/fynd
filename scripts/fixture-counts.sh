#!/usr/bin/env bash
# Prints every count pinned against the market recording fixture (expected -> actual) and the
# component count of each protocol system. Run it on main and on your branch after a change that
# can change how the recording decodes, for example a tycho-simulation bump.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p fynd-core --features test-utils --test integration --locked \
  fixture_counts::report_fixture_counts -- --ignored --nocapture --exact
