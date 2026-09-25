# fynd-test-fixtures

Shared types for recorded-market test fixtures: a `MarketRecording` of one block's pool states,
the expected outputs a solver should produce on it, and the test scenarios that pair the two.

The `record-market` tool writes them; the `fynd-core` integration tests and `fynd-bench-harness`
read them, the harness replaying a recording as its offline market. A crate outside this
repository that tests or benchmarks its own algorithm against a recording goes through the same
types.

Part of [Fynd](https://github.com/propeller-heads/fynd).
