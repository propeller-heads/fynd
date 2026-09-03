# Benchmark result viewer

A single static page that reads `bench-results/` and shows a run two ways: the aggregate report,
and one order at a time with each config's route drawn as a token flow.

```bash
./scripts/bench.sh --name my-run --orders 500   # produce a run
./scripts/bench-viewer.sh                       # serve and open the viewer
```

No build step and no dependencies. The script only exists because browsers block the reads this
page needs when it is opened from disk with `file://`; any static server over the repo root works
just as well.

## Which results it reads

`?root=` names the results directory, and defaults to this repository's `bench-results/`. A path is
resolved against this file, which sits at `bench-harness/viewer/`; a URL is taken as given.

A crate outside this repository benchmarks its own algorithm and writes its runs beside its own
tests. To read those, serve that directory and name it:

```bash
./scripts/bench-viewer.sh --results ../my-solver/bench-results
```

The script serves the directory at `/results` and opens `?root=/results`, so one server answers
both the page and the runs. Two servers would be two origins and the browser would refuse the
cross-origin reads. When the root is not the default, the run picker says which directory it
listed, and a failed read names it too.

## What it reads

| file | used for |
|---|---|
| `<root>/index.json` | the run picker. Rebuilt by every run, by scanning for `run.json` |
| `<run>/run.json` | header facts: which market, orders, baseline, configs, gas price, timeout |
| `<run>/orders.csv` | the whole Report tab — scorecards, distribution, speed, pairs |
| `<run>/routes.jsonl` | the Orders tab. Fetched only when that tab is opened, since it is the big one |

`run.json`'s `configs` is a flat list of config labels — every other file keys by label, and so does
this page. The file each label was read from is beside it under `config_files`.

Everything on the Report tab is derived from `orders.csv` at render time, so the trade-size slicer
moves every section together rather than just the histogram.

The run name in the header opens the picker: a table of every run in `index.json` with when it
finished, which market it solved, and its orders, pairs, configs, concurrency, timeout, gas price
and dataset. `index.json` embeds each run's whole `run.json`, so the table costs no extra reads.
Filter by name, config or dataset; `↑`/`↓` move, `enter` opens, `esc` closes.

The picker shows one kind of run at a time, offline or live, chosen by the toggle beside the
filter. They are not comparable — an offline run replays the same recorded block every time, a live
run is whatever the chain was doing when it ran — so mixing them in one list would invite reading
across them. It opens on the kind of run currently on screen. Runs written before live capture
existed carry no marker and count as offline, which is what they were.

Runs are listed newest first by `run.json`'s `finished_at`. That field was added after runs already
existed, so a run without it shows `—` and sits below the dated ones in reverse name order, which is
where it was before.

## Adding to it

The page is plain HTML, CSS and JavaScript in one file. Colours are CSS custom properties defined
once per theme at the top; both light and dark are token-defined, so a new component styled through
the tokens works in both without further thought.
