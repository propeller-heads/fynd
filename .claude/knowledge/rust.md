# Coding in Rust

## Commands

During development, use simpler commands to help you iterate:

```bash
cargo check --all-features
cargo test
cargo clippy --all-features
```

After a task is done, run `./check.sh` (the same script runs as a precommit hook).

`check.sh` runs, in order:
1. `cargo +nightly fmt -- --check`
2. `cargo +nightly clippy --locked --all --all-features --all-targets -- -D warnings`
3. `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked --package fynd-core --package fynd-rpc-types --package fynd-rpc --package fynd-client`
4. `cargo nextest run --workspace --locked --all-targets --all-features --bin fynd`

## Coding Style

### General Rust guidelines

- Prefer `for` loops with mutable accumulators over long iterator chains when clearer
- Use `let...else` for early returns; keep happy path unindented
- No wildcard matches — explicit destructuring catches field changes at compile time
- Newtypes over primitives where the domain warrants it
- Enums for state machines, not boolean flags
- Write efficient code by default — correct algorithm, appropriate data structures, no unnecessary allocations. Profile
  before micro-optimizing.

### Comments

Code should be self-documenting. No commented-out code — delete it. If you need a comment to explain WHAT the code does,
refactor the code instead. Comments explain WHY, not WHAT. Wrap comment lines at 100 characters.

### Error handling

- Fail fast with clear, actionable messages.
- Never swallow errors silently.
- Prefer returning results to panicking.
- Include context: what operation failed, what input caused it, what to do next.

### Testing

- **Test behavior, not implementation.** If a refactor breaks tests but not code, the tests were wrong.
- **Test edges and errors.** Empty inputs, boundaries, malformed data — bugs live in edges.
- **Mock boundaries, not logic.** Only mock slow (network, filesystem), non-deterministic (time), or external things you
  don't control.
- **Verify tests catch failures.** Break the code, confirm the test fails, then fix.

After every task is done, run the `run-ci` skill. If that is done, check whether docs need updating with the sync-docs
skill.
