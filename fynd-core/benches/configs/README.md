# Benchmark algorithm configs

One file per algorithm configuration the benchmark can run. The file stem is the name you pass to
`--configs`, and the label the reports use:

```bash
./scripts/bench.sh --configs path_frank_wolfe_d3,water_fill_d3
```

Each file is a flat TOML table of [`PoolConfig`] fields. `algorithm` is required; everything else
has a default, so a file can be two lines. Anything `PoolConfig` accepts works here —
`connector_tokens`, `min_hops`, `max_routes`, `liquidity_scope`.

Two fields are set by the run rather than the file, so every config is compared under the same
conditions: `num_workers` (`--jobs`, capped at the core count, since a pool with more workers than
cores costs setup time without adding throughput) and `task_queue_capacity`. `timeout_ms` comes
from `--timeout-ms` unless a file sets it explicitly.

`bellman_ford_d2.toml` is the baseline and is always included, listed or not.

Adding a configuration is adding a file here. Nothing else needs to change.
