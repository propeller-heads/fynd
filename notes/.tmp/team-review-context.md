## Intent Summary

**PR:** (not yet opened) Atomic Searcher Showcase for Fynd
**Jira:** N/A (showcase/example, not a tracked ticket)
**Goal:** Add examples/atomic_searcher/ demonstrating Janos Tapolcai's cyclic arbitrage algorithm as an autonomous searcher (Token A -> ... -> Token A), complementing the BellmanFord solver in PR #43.

**Acceptance Criteria:**
- Example compiles and runs against live Tycho feed
- Finds real arbitrage cycles on Ethereum mainnet
- Golden section search optimizes input amounts
- README documents algorithm, usage, and references
- No regressions in existing tests

## Branch Info

- Branch: ms/atomic-searcher-showcase (based on ms/bellman-ford-algorithm)
- 3 commits:
  - feat: add atomic searcher showcase example
  - fix: add blacklist support and token-level filtering
  - improve: show gross profit and cycle status labels

## Test Results

- cargo check: PASS (2 dead_code warnings for unused struct fields)
- cargo test --lib: 236 passed, 2 failed (pre-existing CLI test failures due to TYCHO_API_KEY env var, not caused by this branch)
- Live testing: Runs successfully against Tycho mainnet, finds real GROSS+ cycles (WETH/USDT/WBTC triangle), correctly filters AMPL rebase token

## Files Changed (10 files, +1274/-8)

- Cargo.toml: +7 (dev-deps, example registration)
- examples/atomic_searcher/README.md: +91 (documentation)
- examples/atomic_searcher/amount_optimizer.rs: +230 (golden section search)
- examples/atomic_searcher/cycle_detector.rs: +395 (BF cycle detection)
- examples/atomic_searcher/main.rs: +494 (CLI, feed setup, block loop)
- examples/atomic_searcher/types.rs: +49 (data types)
- src/feed/events.rs: +3/-3 (pub(crate) -> pub for MarketEvent, EventError, MarketEventHandler)
- src/feed/mod.rs: +2/-2 (pub(crate) -> pub for TychoFeedConfig, DataFeedError)
- src/feed/tycho_feed.rs: +1/-1 (pub(crate) -> pub for TychoFeed)
- src/graph/mod.rs: +2/-2 (pub(crate) -> pub for GraphManager, GraphError)
