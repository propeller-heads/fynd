# Automated Benchmark Suite

`scripts/bench_suite.py` runs a configurable, multi-config benchmark on EC2 — the
audit (quote quality vs aggregators) and the scale sweep (throughput vs workers)
for each config — then renders all plots locally. One command, reproducible from a
single TOML file.

```bash
set -a && source .env && set +a          # TYCHO_*, RPC_URL, BEBOP_*, HASHFLOW_*, AWS_*
python3 scripts/bench_suite.py benchmark_suite.toml
```

## What it does

1. Parses the TOML suite, merges `[defaults]`/`[scale_defaults]` into each `[[config]]`,
   and validates it (auto-bumps `mode` to ≥ `max(worker_counts)`).
2. Renders a run directory `bench_runs/<timestamp>/` containing a
   `worker_pools_<name>.toml` and `env/<name>.env` per config, plus `configs.list`
   and a copy of the resolved suite.
3. Calls `scripts/bench-suite-remote.sh`, which provisions **one** EC2 instance,
   builds the solver, downloads the trade dataset, and for each config (sequentially)
   runs `serve` → `audit` → kill, then `scale`. Artifacts are pulled back into the
   run directory. The instance, security group, and key are torn down on any exit.
4. Renders plots into `bench_runs/<timestamp>/plots/`:
   - per config: `*_bps_histograms.png`, `*_why_winloss.png`, `*_gas.png`
   - across configs: `scale_rps.png` (overlaid throughput + scaling efficiency)

`bench_runs/` is gitignored; nothing is auto-committed.

## Suite file

See [`benchmark_suite.toml`](../../benchmark_suite.toml) for a complete example. Tables:

| Table | Purpose |
|---|---|
| `[suite]` | `title` for the scale plot. |
| `[remote]` | `region`, `instance_type`, `volume_size`. |
| `[defaults]` | Worker-pool + audit knobs applied to every config. |
| `[scale_defaults]` | Scale-sweep knobs applied to every config. |
| `[[config]]` | One per benchmark config; overrides any default. |

### Per-config keys

**Worker pool** (rendered into `worker_pools_<name>.toml`):
`algorithm`, `num_workers`, `task_queue_capacity`, `max_hops`, `pool_timeout_ms`,
`connector_tokens` (list of addresses — supplied verbatim, not derived).

**Routing:** `protocols` (supports `all_onchain` / `native_onchain` tokens), `min_tvl`.

**Audit:** `dataset`, `top_pairs`, `amounts_per_pair`, `block_stride`, `min_amount_usd`
(dust filter; `0` disables), `quote_timeout_ms`, `concurrency`, `eth_call_slippage_bps`,
`eth_call_baseline_fee_bps`, `nordstern_url`, `chain_id`.

**Aggregator rate limiting:** `nordstern_rps`, `kyberswap_rps`, `zerox_rps` (per-aggregator
request pacing in req/s; `0` disables pacing for that aggregator — Fynd is never paced),
`aggregator_max_retries`, `aggregator_retry_base_ms` (retry rate-limited/5xx responses with
exponential backoff so samples aren't dropped).

**Scale:** `worker_counts`, `num_requests`, `mode`, `warmup_secs`, `health_timeout_secs`,
`requests_file`.

**Shared:** `name`, `label`, `http_port`.

## Secrets

Never put credentials in the suite file. The workflow reads them from the environment
(or a local `.env`, auto-loaded without overriding existing vars):
`TYCHO_URL`, `TYCHO_API_KEY`, `RPC_URL`, `BEBOP_USER`/`BEBOP_KEY`,
`HASHFLOW_USER`/`HASHFLOW_KEY`, and AWS credentials for the EC2 lifecycle.

## Flags

| Flag | Effect |
|---|---|
| `--no-audit` | Skip the audit stage. |
| `--no-scale` | Skip the scale stage. |
| `--plots-only --run-dir bench_runs/<ts>` | Re-render plots from an existing run. |
| `--poll-timeout-secs N` | Per-stage remote completion timeout (default 7200). |

## Plot scripts (standalone)

The plotters are config-agnostic and usable on any audit/scale JSON:

```bash
python3 scripts/plot_bps_histograms.py audit.json -o out --label "my run"
python3 scripts/plot_audit_analysis.py audit.json -o out --label "my run"
python3 scripts/plot_scale_rps.py a.json b.json --labels "A" "B" -o out --name scale
```
