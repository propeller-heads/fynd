# Algorithm benchmark

Two programs run the routing algorithms against one market, so a change can be measured; a viewer
and two analysis scripts read what they wrote.

| what | file | what it does |
|---|---|---|
| benchmark | `src/bench.rs` | Runs several configs over many orders and writes a report |
| profiler | `src/profile.rs` | Runs one config over a few orders on one thread, writes nothing |
| viewer | `viewer/index.html` | Reads the reports in a browser |
| analysis | `analysis/bench-analyze.py` | Breaks one run down by order size and route shape |
| analysis | `analysis/bench-setdiff.py` | Per lost order, the pools the winner used and we did not |

The benchmark and the profiler read the same order dataset and take their market the same way, so
an order seen in the viewer can be profiled by its id.

## Two kinds of market

| | offline (`--market offline`, the default) | live (`--market live`) |
|---|---|---|
| where it comes from | the recorded fixture | one block captured from Tycho |
| needs the network | no | yes, plus a Tycho API key |
| reproducible | yes — every offline run replays the same block | no — each run is its own block |
| comparable with | every other offline run | only the configs inside that same run |
| VM-backed pools | **missing** | present |

That last row is the reason live exists. `MarketRecording` cannot serialize VM-backed states, and
drops them silently. In the current fixture that means every Uniswap v4 (384), Balancer (42), Curve
(3) and Maverick (1) pool is a component with no state, so nothing can route through it — along
with about two thirds of Uniswap v3. A live capture never serializes, so they are all there.

The trade is reproducibility. An offline run is the same market every time, which is what makes a
change measurable against last week. A live run is whatever the chain was doing at that block. The
viewer keeps the two apart in its run picker for exactly that reason.

## What you need first

**The market fixture**, for offline runs, is in Git LFS at
`fynd-core/tests/fixtures/market_recording.json.zst`. Run `git lfs pull` if it is a small text file
instead of 771 KB of compressed JSON, or pass `--fixture` to name another copy. Live runs do not read it; they need `TYCHO_URL` and
`TYCHO_API_KEY` instead, and `RPC_URL` to price gas at the chain's rate.

**The order dataset** is `aggregator_trades_50k_1k_usd.json` in the repository root. It is
gitignored because it is large. If you do not have it, ask someone for a copy — rebuilding it means
running `data/dataset.sql` against Dune, which needs an API key.

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
| `--orders N` | How many orders to solve, from the top of the dataset. `0` means every eligible one |
| `--tail N` | The last N eligible orders instead of the first |
| `--random N` | N eligible orders drawn at random, from a fixed seed so two runs pick the same ones |
| `--baseline NAME` | Config every bps figure is measured against. Defaults to `BF_d2` |
| `--configs A,B` | Only these configs. The baseline is always added |
| `--repeats N` | Passes over the same orders, for steadier timings |
| `--jobs N` | Orders solved at once. Defaults to one per core |
| `--timeout-ms N` | How long one solve may take |
| `--gas-price-gwei X` | Gas price the run solves at. Fractions allowed: `0.1` is roughly what the fixture's own block sat at |
| `--logs` | Print the solver's own logs. Slows every config, so leave it off when reading timings |
| `--fixture PATH` | The recording an offline run replays. Defaults to this repository's; mainly for a caller outside it |
| `--configs-dir DIR` | A directory of config files to run on top of the built-in ones. Repeatable; mainly for a caller outside this repository |

`--help-bench` lists them all with the benchmark's own defaults; `--help` describes the script.
Anything the script does not consume is forwarded unchanged, so the benchmark validates its own
options.

## Running against a live market

```bash
export TYCHO_URL=... TYCHO_API_KEY=... RPC_URL=...
./scripts/bench.sh --market live --name live-now --orders 500
```

Same benchmark, same output, same viewer. It connects, takes the snapshot of one block, and solves
against that. One block is a whole market: the snapshot carries every component and state the
filters admit, and derived data — spot prices, depths, token gas prices — is computed locally from
it rather than streamed, so nothing is gained by waiting for a second block.

Every other option works as it does offline, plus:

| option | what it does |
|---|---|
| `--protocols A,B` | Protocol systems to stream. Defaults to every one Tycho has for the chain, including those it serves through the Dynamic Contract Indexer |
| `--include-protocols A,B` | Protocol systems to add to the streamed list — how a source Tycho does not list gets into a capture, see [pAMM price levels](#pamm-price-levels). A name already streamed, or one that brings no component into the market, stops the run. Unlike `--exclude-protocols`, which filters an already-captured market, this changes what is streamed, so it is live-only |
| `--chain NAME` | Chain to capture. Defaults to `ethereum` |
| `--min-tvl X` | Minimum component TVL in ETH. The main lever on how big the market is |
| `--min-token-quality N` | Minimum token quality score |
| `--traded-n-days-ago N` | Only tokens traded within this many days |
| `--capture-timeout-secs N` | How long to wait for the snapshot, and for the price level frame, before giving up |
| `--tycho-url HOST` | Overrides `TYCHO_URL`. Scheme optional |
| `--tycho-api-key KEY` | Overrides `TYCHO_API_KEY` |
| `--rpc-url URL` | Overrides `RPC_URL`. Read for the live gas price, and once for the PropAMMRouter's fee tiers |

Without `--gas-price-gwei` a live run prices gas at whatever the chain is charging, read from
`RPC_URL`. Pass the flag and it wins. An offline run has no such price to read — the fixture
carries none — so it keeps using the default.

The same node is read once more, for the PropAMMRouter's fee tiers. A route with a
`propammfallback:` leg needs them: without them the solver drops every such route. An offline run
reads no node, so a market holding those components cannot be routed through them.

### pAMM price levels

Titan streams a per-block quote ladder for a handful of proprietary AMMs. Those are not Tycho
protocol systems, so no `--protocols` list and no discovery brings them in — name them with
`--include-protocols`:

```bash
./scripts/bench.sh --market live --name price-levels \
  --include-protocols pricelevelstream:fermiswap \
  --exclude-protocols vm:fermiswap
```

The served venues are `fermiswap`, `kipseli`, `metric`, `bebop` and `taurusfi`, and the stream is
Ethereum-only. One frame is taken after the Tycho snapshot — last, so the ladders are as close to
the captured block as they get — and merged into it, which also means every config solves the same
frozen ladder. A venue that sends nothing before `--capture-timeout-secs` stops the run, and so
does one whose frame brings no component into the market — the venue is in the capture only
because of that frame, so a run that quietly loses it is benching a different market.

`--exclude-protocols vm:fermiswap` is not optional bookkeeping. FermiSwap is reachable both ways
and both price the same maker inventory, so leaving the VM one in double-counts it.

A venue on the PropAMMRouter's on-chain whitelist arrives labelled `propammfallback:{venue}`
instead, because its swaps execute through that router. The `--include-protocols` name stays
`pricelevelstream:{venue}` either way: it names the venue, and the stream picks the label. The
stream reads the whitelist through the node at the `RPC_URL` environment variable — `--rpc-url`
does not set it, so passing the flag alone leaves every venue on the direct path.

The profiler takes the same flags, so a slow order from a live run can be profiled against a fresh
market: `./scripts/profile.sh --market live --config WF_d3 --orders 200`. It will be a
different block, so it is a different market.

A run writes six files:

| file | what is in it |
|---|---|
| `report.md` | The summary. Small enough to paste into a pull request |
| `protocols.csv` | What each config routed through, one row per config and protocol |
| `orders.csv` | One row per order per config. `failure` names why an unsolved order came back empty, and is blank on a solve |
| `pairs.csv` | The same, aggregated by token pair |
| `routes.jsonl` | The full route each config found, for the viewer |
| `run.json` | The settings the run used, and when it finished |

### protocols.csv

One row per config and protocol, including protocols a config never touched — a zero against 400
available pools is the interesting answer, and it only exists as a row if every protocol is
listed.

| column | what it is |
|---|---|
| `pools_in_market` / `pools_simulatable` | the market, not the config, so they repeat down the rows. Usage means little without them |
| `pools_used` | distinct pools of that protocol the config routed through |
| `legs` / `legs_pct` | swap legs, and their share of that config's legs |
| `orders` / `orders_pct` | solves whose route touched the protocol at least once |
| `usd` | dataset USD of those orders |

A final `winner` block carries the same columns over whichever route won each order, so the
mix a deployment running every config side by side would execute can be read off directly.
Ties keep the earlier config, which puts the baseline first.

`orders` and `usd` count a route once per protocol it crosses, so across protocols they sum to more
than the run. `legs` and `pools_used` do not double count.

## Reading the results

```bash
./scripts/bench-viewer.sh
```

Opens a browser on the results. The run name in the header opens a table of every run to switch
between them, and new runs appear on refresh. The script exists only because browsers block the
file reads the page needs when it is opened straight from disk.

Two scripts read the same `bench-results/<run>/` directory for questions the report does not answer:

```bash
bench-harness/analysis/bench-analyze.py bench-results/my-run --config PFW_d3 --against WF_d3 --worst 10
bench-harness/analysis/bench-setdiff.py bench-results/my-run --config PFW_d3 --against WF_d3
```

`bench-analyze.py` says in which order-size bins a config won and which orders it lost worst, naming
order ids that can go straight into the profiler. `bench-setdiff.py` splits each loss into two kinds:
the winner used a pool that was never on the table, or every pool was there and the allocation was
worse.

## Profiling one algorithm

```bash
./scripts/profile.sh --config WF_d3 --orders 200 --repeats 3
```

Records under `samply` and opens the flamegraph. One config, one solver thread, no output files, so
the flamegraph is the solve and almost nothing else.

| option | what it does |
|---|---|
| `--config NAME` | Which config to run. Required |
| `--order ID` | Profile one order. `2073` finds `2073_00000000_ae7ab965` |
| `--orders N` | Profile the first N instead |
| `--tail N` | Profile the last N instead |
| `--random N` | Profile N drawn at random, from a fixed seed |
| `--repeats N` | More passes, more samples, same work measured |
| `--jobs N` | Orders solved at once, on that many workers. One by default |
| `--logs` | Print the solver's own logs. Pair with `--no-record` |
| `--no-record` | Run without `samply`, for timings only |
| `--save-only` | Write `profile.json` without opening the browser |

`--help-profile` lists the rest, including `--verbose`, `--timeout-ms`, `--gas-price-gwei` and
`--trades`.

`--logs` and `--jobs N` both change what the flamegraph shows. Logging puts its formatting and
writes next to the algorithm; `--jobs N` spreads the solve over N threads, which gets through a run
faster but leaves the timings measuring wall clock under load. Either way the default — one thread,
no logs — is what a readable profile wants.

Both tools take the filter from `RUST_LOG` when it is set, and use `fynd_core=debug` otherwise:

```bash
RUST_LOG=fynd_core::algorithm=trace ./scripts/profile.sh \
  --config WF_d3 --order 2073 --logs --no-record
```

Options for `samply` itself go after a bare `--`:

```bash
./scripts/profile.sh --config WF_d3 --orders 200 -- --rate 5000
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

Add a file to `configs/`, or to a directory passed with `--configs-dir`. Nothing else changes. The file name is the name you pass to `--configs`
and the label the report shows.

```toml
# configs/WF_d4.toml
algorithm = "water_fill"
max_hops = 4
```

The file is a flat table of `PoolConfig` fields. Only `algorithm` is required. See
`configs/README.md` for which fields the run overrides.

### Add an algorithm

Nothing here needs to change. The benchmark asks the solver for the algorithm by name, so once it
is registered, a config file naming it works. That holds for an algorithm in another crate too: the
registry `run` takes is what puts it in the same run as the built-ins. A config naming an algorithm the build does not have
is skipped and listed as skipped in the report, rather than failing the run.

### Exclude a token

Add it to `data/blocked_tokens.toml`. Every pool holding it is dropped from the market, and every
order naming it is dropped from the dataset.

The bar is high on purpose: only tokens the recording prices inconsistently against the rest of the
market, where a router exploiting the inconsistency scores a win it could never realise. The file
explains the reasoning for the two that are in it.

### Change the report

`src/bench.rs` writes it. The section named "Reading this" at the bottom of every report explains
what the columns mean, and is worth updating alongside any new column.

## How the code is arranged

Everything is a library, and the two targets in `benches/` are three lines each.

`src/lib.rs` holds what both programs need: building the market, loading configs, applying the
blocklist, resolving token symbols, building the solver, and the percentile and median helpers the
reported numbers come from. `src/trades.rs` reads the order dataset. `src/live.rs` captures a block
from Tycho. `src/bench.rs` is the benchmark and `src/profile.rs` the profiler, each a `run` taking
the algorithms to add to the built-in ones.

The two kinds of market meet in one function, `build_market`, and both come out as a `Market`.
Nothing downstream can tell which it was handed, which is what stops an offline and a live run
measuring differently. What each run was is carried on `Market::source`, written to `run.json`, and
shown in the report and the viewer.

Anything used by both programs belongs in `src/lib.rs`, so the two cannot drift apart on what they
measure. Only the two `run` functions are public; everything else is `pub(crate)`, so the interface
a caller depends on is the one the README describes. The shared flags are `clap` structs flattened into each program for the same reason: two
copies of a dozen attributes drift the first time one is edited. `MarketFlags` carries `--market`,
`--fixture` and the Tycho settings; `ConfigFlags` carries `--configs-dir`. The market flags are one `clap` struct, `LiveFlags`, flattened into each binary for the
same reason: two copies of a dozen attributes drift the first time one is edited.

The configs, the token table and the blocked list ship inside this crate, in `configs/` and
`data/`, and are found relative to the crate rather than the working directory. A caller depending
on this crate therefore reads the same ones without copying anything. The market fixture is the
exception: it is in Git LFS, so a checkout cargo made for a git dependency holds the pointer file,
and an outside caller names its own copy with `--fixture`.

Both targets are declared `harness = false` in `Cargo.toml`, which means they get a plain `main()`
instead of the test harness. That is why they parse their own arguments with `clap`.

The crate turns on `fynd-core`'s `test-utils` feature itself, because `Solver::from_recording_with`
is what every run goes through.

Because they parse their own arguments, they cannot answer nextest's `--list`. CI and `check.sh`
exclude them by name (`-E 'not binary(algorithm_bench) and not binary(profile)'`) rather than
setting `test = false` in `Cargo.toml`, which would also drop them from
`cargo clippy --all-targets` and leave this code unlinted.
