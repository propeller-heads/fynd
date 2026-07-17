# record-market

Capture Tycho market state and generate expected test outputs for integration testing.

## Usage

```bash
RUST_LOG=info cargo run -p record-market -- \
  --tycho-url tycho-beta.propellerheads.xyz \
  --tycho-api-key "$TYCHO_API_KEY" \
  --rpc-url "$RPC_URL" \
  --duration-secs 30 \
  --output-dir fynd-core/tests/fixtures
```

This produces two files:
- `market_recording.json.zst` — zstd-compressed recording of Tycho stream updates
- `expected_outputs.json` — expected quote results for canonical trading pairs

The recording stores the chain in its metadata; replay (expected-output generation and the
integration tests) reads the chain from there. Trading-pair scenarios are chain-specific and
live in `fynd-core/tests/fixtures/pairs/<chain>.json` — recording a new chain requires adding
a pairs file for it.

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--tycho-url` | required | Tycho endpoint |
| `--tycho-api-key` | required | Tycho API key |
| `--rpc-url` | optional | Chain RPC for gas price capture |
| `--chain` | `ethereum` | Chain to record (any chain Tycho supports, e.g. `base`, `unichain`) |
| `--duration-secs` | 600 | Recording duration (30s is usually sufficient) |
| `--output-dir` | `fynd-core/tests/fixtures` | Where to write fixtures |
| `--protocols` | auto-discover | Comma-separated protocol filter |
| `--min-tvl` | 10.0 | Minimum TVL in ETH |
| `--min-token-quality` | 100 | Token quality threshold |
| `--traded-n-days-ago` | 3 | Token recency filter |

## Gas Price

If `--rpc-url` is provided, the tool fetches the current gas price and stores the
raw wei value in the recording. During replay, this value is injected so the solver
can compute gas cost deductions without a live RPC connection.

## When to Re-record

Re-run the recording tool when:
- Algorithm or pool configuration changes (`fynd.toml` pools)
- Solver code changes that intentionally improve quote quality
- Fixtures are stale (the tool stores a timestamp; tests warn if > 7 days old)

Do NOT re-record just because a quality test fails — investigate first to determine
if the change is a regression or an improvement.
