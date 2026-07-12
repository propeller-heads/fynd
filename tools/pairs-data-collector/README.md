# Pairs Data Collector

Collects block-by-block, direction-specific, size-dependent executable curves from an embedded Fynd solver. The source dataset contains raw integer quote results and stable route projections with component IDs, protocols, token addresses, amounts, gas estimates, and split fractions. Runtime simulation state is intentionally excluded because some protocol implementations cannot serialize their VM dependencies. The collector does not derive mid-prices, interpolate curves, test cointegration, or trade.

## Safety

Development and load tests must use a Tycho dev or beta endpoint. Configuration validation rejects other Tycho endpoints. Never point a capacity probe at a production Fynd or Tycho service.

The checked-in pilot uses a named native-protocol subset and Bellman-Ford with three hops. Its results mean "best route within this configured universe." They are not a claim about the complete Ethereum market. Add economically important protocols, particularly Curve or Balancer for stable pairs, only after measuring their capacity impact.

## Observation contract

The collector combines two state sources:

1. Ethereum RPC `newHeads` is the independent ledger of expected blocks, headers, base fees, and reorgs.
2. Embedded Fynd supplies executable routes. Every request targets the current numeric Fynd state label and waits for all configured solver pools.

A quote wave is accepted only when the target head, state label, returned quote block, and Fynd block identity immediately before and after the wave agree. A state transition invalidates the wave as `block_race`. If Fynd has already advanced past an RPC-observed head, the collector writes explicit `missed_state` rows because live routed history cannot be reconstructed.

Every successful route is also replayed swap by swap against the protocol simulation states attached to that route. Each replayed integer output must exactly equal the output recorded by the solver. A mismatch becomes an explicit failed point and cannot enter the research dataset as a success.

## Configuration

Start from `configs/pilot.toml`. Token addresses are identity. Symbols are display metadata only. Each pair has fixed exact-input integer ladders for both directions. Changing a ladder, pair, protocol, algorithm, or worker setting requires a new run.

Required environment variables are named in the config:

- `TYCHO_API_KEY_BETA`: raw Tycho authorization key.
- `RPC_URL`: HTTP Ethereum RPC endpoint used by Fynd and gap reconciliation.
- `RPC_WS_URL`: WebSocket Ethereum RPC endpoint supporting `eth_subscribe`.

Secrets are read from environment variables and are never written to manifests or rows.

## Commands

Validate configuration without reading any secrets:

```bash
cargo run -p pairs-data-collector -- check-config \
  --config tools/pairs-data-collector/configs/pilot.toml
```

Run a bounded three-head capacity probe:

```bash
cargo run --release -p pairs-data-collector -- collect \
  --config tools/pairs-data-collector/configs/pilot.toml \
  --output-dir /tmp/pairs-data-collector \
  --max-heads 3
```

Run continuously by omitting `--max-heads`. Ctrl-C closes and compacts the active hour.

Every configured pair is sampled on every block. There is no sub-sampling or rotation: the whole
point of the dataset is a complete price for every asset at every block, so pairwise analyses
always share observation blocks. If the universe does not fit the per-head budget, the collector
writes explicit `capacity_skipped` rows and logs an error; the fix is a smaller universe or a
bigger machine (measure with `deploy/benchmark_capacity.sh`), never silent sampling.

Generate a liquidity-tier-ranked WETH-star universe from Tycho with:

```bash
TYCHO_API_KEY_BETA=... python3 \
  tools/pairs-data-collector/scripts/build_weth_star_config.py \
  --output-dir /path/to/universe \
  --count 2000
```

The generator keeps quality-100 tokens traded within three days, ranks them by summed lower-bound
TVL across Tycho component tiers, and calibrates one token-side amount to the 0.01 WETH notional
where DefiLlama has a current price. Unpriced tokens use one whole token and should be reviewed or
filtered during analysis.

Validate a WAL independently:

```bash
cargo run -p pairs-data-collector -- validate \
  --wal /tmp/pairs-data-collector/wal/<run>-<hour>.ndjson
```

Recompact a valid WAL:

```bash
cargo run -p pairs-data-collector -- compact \
  --wal /tmp/pairs-data-collector/wal/<run>-<hour>.ndjson \
  --output-dir /tmp/pairs-data-collector/recovered
```

## Output

The durable source is JSON Lines under `wal/`. Each append is flushed and synchronized. A torn final line is ignored during recovery; corruption anywhere else fails validation.

Closed hours are validated and compacted with Zstd into:

- `parquet/quote_points/`
- `parquet/block_runs/`
- `parquet/block_status_events/`
- `parquet/manifests/`

Each finalized file receives a sibling `.sha256` checksum. WAL files are retained after compaction. Canonicality changes are append-only status events, so finalized quote partitions are never rewritten after a reorg.

All Ethereum amounts are canonical decimal strings. This avoids lossy `u64` conversion and keeps the full uint256 range available to DuckDB or another analysis layer.

## Production deployment

The `deploy/` directory contains the Linux systemd and S3 setup used for persistent collectors:

- `fynd-pairs-collector.service` runs the collector as the unprivileged `agent` user. It restarts only after failure, so a deliberately bounded run does not enter a restart loop.
- `fynd-pairs-upload.timer` runs every five minutes. The uploader copies active WAL snapshots to a recovery prefix and uploads finalized WAL, Parquet, manifests, and checksums immutably.
- A remote `complete/<segment>.json` marker is written only after every finalized file passes its local checksum and uploads successfully. A segment is never considered complete before that marker exists.
- Finalized local segments are retained for 48 hours by default and pruned only when their remote completion marker exists. Set `LOCAL_RETENTION_HOURS` to change the safety window.
- `run-with-secrets.sh` resolves credentials at process start through the cloud host's scoped 1Password service account. Secret values never enter systemd units, config TOML, or S3 objects.

Install without starting collection:

```bash
COLLECTOR_BINARY=/path/to/pairs-data-collector \
COLLECTOR_CONFIG=/path/to/production.toml \
S3_DEST=s3:fyndquotes/pairs-data/ethereum/weth-star-top-2000/v1 \
tools/pairs-data-collector/deploy/install.sh
```

Start and inspect the services:

```bash
sudo systemctl enable --now fynd-pairs-collector.service fynd-pairs-upload.timer
systemctl status fynd-pairs-collector.service fynd-pairs-upload.timer
journalctl -u fynd-pairs-collector.service -f
```

Run an exact collector capacity matrix with CPU pinning:

```bash
COLLECTOR_BINARY=/path/to/pairs-data-collector \
BASE_CONFIG=/path/to/universe.toml \
BENCHMARK_DIR=/var/lib/fynd-pairs-benchmark \
BENCHMARK_CASES='2:2:200 4:4:200 8:8:200 16:16:200' \
tools/pairs-data-collector/deploy/benchmark_capacity.sh
```

Each case is `CPUS:WORKERS:PAIRS`, where `PAIRS` truncates the base config to its first N
`[[pairs]]` sections so a case measures a smaller full universe, never a sampled one.
`summarize_run.py` reports collection latency, head-to-finish latency, coverage, observation
throughput, and attempted solver quote throughput directly from the durable WAL.

For saturated request throughput and server-reported per-query solve time, use `deploy/benchmark_rps.sh`. It pins the complete in-process Fynd server to each CPU allocation, runs repeated fixed-concurrency tests, suppresses per-request logging and synchronization overhead, and records request RPS, solved-order RPS, route coverage, round-trip latency, and solve-time percentiles. Use `summarize_rps.py` to aggregate repetitions. Run a hot-pair lane and a realistic request-file lane separately; the hot lane measures the upper bound, while the realistic lane prevents fast no-route responses from being mistaken for useful capacity.

## Pilot acceptance

Before expanding the universe, run at least 100 blocks and measure:

- p50, p95, and p99 time from head receipt to persisted block completion.
- Worker queue occupancy and any `capacity_skipped`, timeout, or block-race rows.
- Successful quote coverage by pair, direction, and depth.
- Compressed bytes per quote row.
- Correct handling of RPC reconnects and same-height hash replacement.

Do not infer routing capacity from Fynd's general RPS claims. Measure this exact graph, algorithm, protocol universe, grid, and hardware.
