# Benchmark algorithm configs

One file per algorithm configuration the benchmark can run. The file stem is the name you pass to
`--configs`, and the label the reports use:

```bash
./scripts/bench.sh --configs PFW_d3,WF_d3
```

Each file is a flat TOML table of [`PoolConfig`] fields. `algorithm` is required; everything else
has a default, so a file can be two lines. Anything `PoolConfig` accepts works here —
`connector_tokens`, `min_hops`, `max_routes`, `liquidity_scope`.

Two fields are set by the run rather than the file, so every config is compared under the same
conditions: `num_workers` (`--jobs`, capped at the core count, since a pool with more workers than
cores costs setup time without adding throughput) and `task_queue_capacity`. `timeout_ms` comes
from `--timeout-ms` unless a file sets it explicitly.

The stem is the algorithm's initials and the hop limit, and it is the label every report column,
`orders.csv` row and viewer card carries:

| stem | algorithm |
|---|---|
| `BF` | `bellman_ford` |
| `ML` | `most_liquid` |
| `PFW` | `path_frank_wolfe` |
| `WF` | `water_fill` |

`BF_d2.toml` is the baseline and is always included, listed or not.

## Exclusive liquidity

A config setting `liquidity_scope = "include_exclusive"` builds two worker pools instead of one: the
exclusive one and a public twin on the same algorithm. A solver whose every pool includes exclusive
liquidity does not build, and the twin is also what the exclusive candidate has to beat, so the
card's output is the better of the two and the gap between them is what the exclusive components
are worth.

It still reports as one card. Two things to keep in mind reading it: the config runs `2 × --jobs`
worker threads, and its solve time is the slower of the two pools, so compare its output with the
other cards rather than its speed.

Exclusive components only reach the market when the protocol is streamed with the prefix, so this
needs a config file carrying the setting, and a live capture naming the protocols itself:

```bash
echo 'algorithm = "water_fill"
max_hops = 3
liquidity_scope = "include_exclusive"' > fynd-core/benches/configs/WF_d3_exclusive.toml

./scripts/bench.sh --configs WF_d3_exclusive --market live \
  --protocols uniswap_v2,uniswap_v3,uniswap_v4,ekubo_v2,exclusive:ekubo_v3
```

No such file is checked in, because every config on disk runs by default and this one needs a live
capture to mean anything.

`ekubo_v3` is the only protocol the prefix accepts; anything else is a config error. Naming the
same system both ways in one list is rejected too, so `ekubo_v3,exclusive:ekubo_v3` fails rather
than streaming whichever entry came last.

`--protocols` here is a literal list of protocol systems. The `all_onchain` shorthand the server
takes is expanded by `fynd_rpc::protocols::resolve_protocols`, which the benchmark does not go
through — name the systems you want, and give Ekubo V3 the prefix. Leaving `--protocols` off
discovers every protocol Tycho has, none of them exclusive.

Against the recorded fixture, which carries no exclusive components, this config scores the same as
its public equivalent.

Adding a configuration is adding a file here. Nothing else needs to change.
