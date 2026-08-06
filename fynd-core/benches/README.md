# Offline algorithm benchmark

Two programs that run the routing algorithms against a recorded market, so a change can be measured
without touching the network.

| what | file | what it does |
|---|---|---|
| benchmark | `algorithm_bench.rs` | Runs several configs over many orders and writes a report |
| profiler | `profile.rs` | Runs one config over a few orders on one thread, writes nothing |
| viewer | `viewer/index.html` | Reads the reports in a browser |

Both replay the same market fixture and read the same order dataset, so an order seen in the viewer
can be profiled by its id.

## What you need first

**The market fixture** is in Git LFS at `fynd-core/tests/fixtures/market_recording.json.zst`. Run
`git lfs pull` if it is a small text file instead of 771 KB of compressed JSON.

**The order dataset** is `aggregator_trades_50k_1k_usd.json` in the repository root. It is
gitignored because it is large. If you do not have it, ask someone for a copy — rebuilding it means
running `dataset.sql` against Dune, which needs an API key.

**`jq`** for either script: `brew install jq`. Both use it to read the built binary's path out of
cargo's JSON output, so it is needed even with `--no-record`.

**`python3`** for the viewer script, which serves the results with `python3 -m http.server`.

**`samply`** only if you want flamegraphs: `cargo install samply`. The profiler runs without it
using `--no-record`.

## Running the benchmark

```bash
./scripts/bench.sh --name my-run --orders 2000
```

That solves 2000 orders with every config and writes `bench-results/my-run/`.

Useful options:

| option | what it does |
|---|---|
| `--name NAME` | Names the output directory. Runs are kept, not overwritten |
| `--orders N` | How many orders to solve. `0` means every eligible one |
| `--configs A,B` | Only these configs. The baseline is always added |
| `--repeats N` | Passes over the same orders, for steadier timings |
| `--jobs N` | Orders solved at once. Defaults to one per core |
| `--timeout-ms N` | How long one solve may take |
| `--gas-price-gwei X` | Gas price the run solves at. Fractions allowed: `0.1` is roughly what the fixture's own block sat at |

`--help-bench` lists them all with the benchmark's own defaults; `--help` describes the script.
Anything the script does not consume is forwarded unchanged, so the benchmark validates its own
options.

A run writes five files:

| file | what is in it |
|---|---|
| `report.md` | The summary. Small enough to paste into a pull request |
| `orders.csv` | One row per order per config |
| `pairs.csv` | The same, aggregated by token pair |
| `routes.jsonl` | The full route each config found, for the viewer |
| `run.json` | The settings the run used, and when it finished |

## Reading the results

```bash
./scripts/bench-viewer.sh
```

Opens a browser on the results. The run name in the header opens a table of every run to switch
between them, and new runs appear on refresh.

The script exists only because browsers block the file reads the page needs when it is opened
straight from disk.

## Profiling one algorithm

```bash
./scripts/profile.sh --config water_fill_d3 --orders 200 --repeats 3
```

Records under `samply` and opens the flamegraph. One config, one solver thread, no output files, so
the flamegraph is the solve and almost nothing else.

| option | what it does |
|---|---|
| `--config NAME` | Which config to run. Required |
| `--order ID` | Profile one order. `2073` finds `2073_00000000_ae7ab965` |
| `--orders N` | Profile the first N instead |
| `--repeats N` | More passes, more samples, same work measured |
| `--no-record` | Run without `samply`, for timings only |
| `--save-only` | Write `profile.json` without opening the browser |

`--help-profile` lists the rest, including `--verbose`, `--timeout-ms`, `--gas-price-gwei` and
`--trades`.

Options for `samply` itself go after a bare `--`:

```bash
./scripts/profile.sh --config water_fill_d3 --orders 200 -- --rate 5000
```

**Two things about the flamegraph.** Building the solver replays the recording before the first
order, and those frames sit under `Solver::from_recording`. On a short run they can outweigh the
solving; the run says so if they do, and `--repeats` is the fix.

It runs one solver thread on purpose, so there is a single thread to read. Everything under
`find_best_route` is the algorithm.

At the end it prints the ten slowest solves with their ids, so the usual loop is a wide run to find
a slow order, then `--order <id>` to profile just that one.

## Changing what is measured

### Add a configuration

Add a file to `configs/`. Nothing else changes. The file name is the name you pass to `--configs`
and the label the report shows.

```toml
# configs/water_fill_d4.toml
algorithm = "water_fill"
max_hops = 4
```

The file is a flat table of `PoolConfig` fields. Only `algorithm` is required. See
`configs/README.md` for which fields the run overrides.

### Add an algorithm

Nothing here needs to change. The benchmark asks the solver for the algorithm by name, so once it
is registered, a config file naming it works. A config naming an algorithm the build does not have
is skipped and listed as skipped in the report, rather than failing the run.

### Exclude a token

Add it to `blocked_tokens.toml`. Every pool holding it is dropped from the market, and every order
naming it is dropped from the dataset.

The bar is high on purpose: only tokens the recording prices inconsistently against the rest of the
market, where a router exploiting the inconsistency scores a win it could never realise. The file
explains the reasoning for the two that are in it.

### Change the report

`algorithm_bench.rs` writes it. The section named "Reading this" at the bottom of every report
explains what the columns mean, and is worth updating alongside any new column.

## How the code is arranged

`common/mod.rs` holds what both programs need: loading the market, loading configs, applying the
blocklist, resolving token symbols, building the solver, and the percentile and median helpers the
reported numbers come from. `common/trades.rs` reads the order dataset. Anything used by both
belongs there, so the two cannot drift apart on what they measure — including the default gas
price, which is one constant so an order picked out of a report profiles the same solve.

Both are declared `harness = false` in `Cargo.toml`, which means they get a plain `main()` instead
of the test harness. That is why they parse their own arguments with `clap`.

Both need the `test-utils` feature, which is what makes `Solver::from_recording` available. The
scripts pass it; a bare `cargo bench` skips them.

Because they parse their own arguments, they cannot answer nextest's `--list`. CI and `check.sh`
exclude them by name (`-E 'not binary(algorithm_bench) and not binary(profile)'`) rather than
setting `test = false` in `Cargo.toml`, which would also drop them from
`cargo clippy --all-targets` and leave this code unlinted.
