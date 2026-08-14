# Integration Tests

Replay-based integration tests for the Fynd solver. Tests replay recorded Tycho market
state through `Solver::from_recording()` and verify quote results against an expected
baseline.

## Running

```bash
cargo nextest run -p fynd-core --features test-utils --test integration
```

The `--features test-utils` flag is required — without it the test binary compiles to
nothing and reports zero tests.

## Test Descriptions

| Test | What it checks |
|------|---------------|
| `test_all_expected_pairs_return_solutions` | Pairs that succeeded in the baseline also succeed in replay |
| `test_quality_within_expected_baseline` | Output amounts stay within 1% of baseline (regressions only) |
| `test_quality_invariants` | Successful quotes have positive output, gas, and a route |
| `test_unknown_token_returns_error` | Fake token addresses return error, not panic |
| `test_all_derived_fields_computed` | Spot prices, component depths, token prices all present |
| `test_derived_data_matches_expected` | Exact equality of derived data counts vs baseline |
| `test_solve_time_p95_within_threshold` | P95 solve time within 3x of baseline |
| `test_no_solve_exceeds_absolute_cap` | No solve exceeds max pool timeout + 1s margin |

## Debugging Failures

**Quality test fails (>1% degradation)**:
1. Check if an algorithm or pool config change caused it
2. If the change is intentional and improves routing, re-record:
   `cargo run -p record-market -- --tycho-url ... --duration-secs 30`
3. If unintentional, investigate the code change

**Derived data mismatch**:
Same recording + same code = same derived data. A mismatch means code changed how
derived data is computed. Re-record if the change is intentional.

**Timing violations**:
Timing depends on hardware. The threshold derives from `worker_pools.toml` max timeout.
CI runners may be slower — timing tests use a 3x multiplier.

## Fixtures

Located in `fynd-core/tests/fixtures/`:
- `market_recording.json.zst` — recorded Tycho stream (Git LFS)
- `expected_outputs.json` — expected quote results
- `pairs/<chain>.json` — trading-pair scenarios per chain (currently `ethereum.json`)

The tests read the chain from the recording's metadata and load the matching pairs file.
Generate fixtures with `cargo run -p record-market` (`--chain` selects the chain).
