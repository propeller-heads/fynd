# Fynd Capacity Report — 2026-07-13

**Target:** `ethereum-fynd` release, `staging-fynd` namespace, EKS `propeller-prod-euc1`
(eu-central-1).
**Harness:** `fynd-benchmark capacity` (PR #301), image `0.90.0-rc.3`; backend
`0.90.0-rc.1` (identical solver code to `main` / `0.89.1`).
**Request set:** 10k aggregator trade dataset, same-token templates filtered,
`sha256 = 7bec74375dd9b7445791110a6d15b89a6c045016d9b3b233018cb378d56ea8aa`.
**Data:** [`docs/scaling/data/2026-07-13/`](data/2026-07-13/).

---

## TL;DR

Capacity per configuration. **strict-1.2×** is the agreed degradation line
(round-trip p95 ≤ 1.2× baseline p95); **3.0×** is the exploratory knee (p95 ≤ 3× baseline);
**wall** is the first step that hard-fails (latency collapse). All rates are achieved RPS,
error rate 0 and unsolved rate stable at ~30% across every step.

| Config (pod × workers) | Host | Baseline p95 | strict-1.2× | 3.0× | Wall | Source |
|---|---|---:|---:|---:|---:|---|
| 2 vCPU, 2×1 workers (today) | c6a-class | 46 ms | **20 rps** | **30 rps** | ~35 rps | `E1-exploratory.json` |
| 2 vCPU, 2×1, encoding on | c6a-class | 47 ms | 20 rps | 30 rps | ~35 rps | `E1-baseline-encoding.json` |
| 4 vCPU, 2×2 workers | c6a.xlarge | 46 ms | **45 rps** | **55 rps** | ~60 rps | `E2-4vcpu.json` |
| 7 vCPU, 2×3 workers | c6a.2xlarge (pinned) | 43 ms | **55 rps** | **90 rps** | ~95 rps | `E2-7vcpu-c6a.json` |
| 7 vCPU, 2×3 workers | t3a.2xlarge (burstable) | 83 ms | — | 55 rps | 60 rps | `E2-7vcpu.json` (anomaly) |
| 2 pods × 2 vCPU, 2×1 | mixed fleet | 93 ms\* | 70 rps\* | 80 rps | ~85 rps | `E3-2pods.json` |
| 4 pods × 2 vCPU, 2×1 | mixed fleet | 88 ms\* | 85 rps\* | 110 rps | ~115 rps | `E3-4pods.json` |

\* E3 baselines are polluted by node-placement (baselines ran on burstable/flex hosts);
the inflated baseline inflates the 1.2× limit, so the E3 strict numbers are **not**
comparable to the pinned-c6a rows. See Q2.

**Bottleneck verdict:** worker-pool serialization. Each pod saturates at ~15 rps per
worker (per pool) at the 3.0× knee; below saturation the tail is set by block-update
state stalls, not queueing. HTTP/tokio overhead is ~1 ms and never binds.

**Recommended prod config:** pods of **4 vCPU / 2 pools × 2 workers on a pinned c6a
node** (c6a.xlarge) — the cheapest configuration per unit of strict capacity
($2.83 /rps-month) and clear of the 7-vCPU node ceiling. Run **N+1** replicas (one pod
of headroom) behind an HPA driven by `worker_pool_queue_wait_seconds`, with a
`nodeSelector`/affinity that excludes burstable (t3a) and flex (c7i-flex) instances.

---

## Method

### Harness

`fynd-benchmark capacity` (design spec §1a, shipped in PR #301) establishes a baseline
with a low-rate sequential pass, then steps an RPS ladder, holding each step for 60 s
after a warm-up discard window. Every request is a quote-only call (no price guard, so
zero per-request RPC against the shared prod endpoints). Each step records round-trip
latency, server-reported solve time, the derived HTTP overhead, HTTP error rate, and the
unsolved-order rate. The in-cluster Kubernetes Job (helm-configuration PR #764) runs the
image against `http://ethereum-fynd.staging-fynd.svc.cluster.local:3000`, so there is no
internet jitter in the numbers.

The `--encoding` flag attaches fixed `EncodingOptions` to every request to exercise the
full quote+encode path; it is RPC-free (router fees are background-refreshed and
snapshot-read) and is recorded in the report so encoded and non-encoded runs are never
compared as like-for-like.

### SLO definitions

- **strict-1.2× (agreed upfront):** round-trip p95 ≤ 1.2× baseline p95, error/timeout
  rate < 0.1%. This is the degradation line from the design spec.
- **3.0× (exploratory):** round-trip p95 ≤ 3.0× baseline p95. Used to drive the ladder
  far enough to find the true knee. The harness `capacity_rps` field reports the 3.0×
  number for the exploratory runs.

The strict-1.2× verdicts in this report are computed analytically from the per-step data:
the limit is `1.2 × baseline_p95`, and the strict capacity is the highest step whose
round-trip p95 stays under it before the first breach. Worked example for today's config
(`E1-exploratory.json`, baseline p95 = 46 ms → limit 55.2 ms):

| Step (rps) | 5 | 10 | 15 | 20 | 25 |
|---|---:|---:|---:|---:|---:|
| round-trip p95 (ms) | 51 | 49 | 51 | 51 | **64** |
| ≤ 55.2 ms? | yes | yes | yes | yes | **no** |

First breach at 25 rps ⇒ **strict-1.2× = 20 rps**. The same run's 3.0× limit is 138 ms;
30 rps (p95 87) passes and 35 rps (p95 147) fails ⇒ **3.0× = 30 rps**, wall ≈ 35 rps.

### Request mix

Quote-only requests drawn from the 10k aggregator trade dataset, fixed seed. An early
exploratory run failed on the error-rate gate because the raw dataset contains
`token_in == token_out` trades that the server 400s (~0.6%/step); these templates are now
filtered at load (PR #301), so all runs in this report show 0 HTTP errors.

### Known measurement caveats

- **Interval quantization.** The generator's inter-request spacing quantizes to whole
  milliseconds above ~30 rps, so target and achieved RPS diverge (e.g. target 60 →
  achieved 62.5). **Every rate in this report is achieved RPS**, read from the `achieved_rps`
  field, not the target.
- **Baseline windows.** The first strict run (`E1-baseline.json`) used a 50-request
  baseline (~2 s) that never spans a block update; its p95 (36 ms) is optimistic and its
  first ladder step therefore "failed" against a too-tight limit (capacity `null`). All
  headline numbers use the 500-request baselines, which do span block updates.
- **Dataset sampling noise** on the unsolved-order rate is ±2 pp step-to-step; the runs
  use a 5 pp excess-unsolved tolerance so this noise does not trip the gate.
- **Node placement.** Under default Karpenter placement, staging pods land on a mix of
  c6a, c7i-flex and t3a instances. This dominates the E2b (t3a) and E3 numbers and is
  itself a finding (Q2).

---

## Q1 — Bottleneck

**Worker-pool serialization is the primary bottleneck, and it is linear.** Every quote
fans out to two pools (`bellman_ford_safe`, `path_frank_wolfe_3_hops`), each with
`num_workers` workers. Adding workers adds throughput at a near-constant ~15 rps per
worker at the 3.0× knee:

| Workers / pool | 3.0× capacity | rps per worker |
|---|---:|---:|
| 1 (2 vCPU) | 30 rps | 15.0 |
| 2 (4 vCPU) | 55 rps | 13.8 |
| 3 (7 vCPU) | 90 rps | 15.0 |

The single-pod hard wall (~35 rps) matches the one-worker-per-pool theory: 1000 ms /
~30 ms per solve ≈ 33 rps.

**Below saturation, the tail is set by block-update stalls, not queueing.** In the E6
sustained run (15 rps for 10 min, single pod, `E6-sustained-15rps.json` +
`E6-metrics-samples.csv`), queue depth stays 0 and mean/median queue wait is ~0 the whole
time, yet solve p99 oscillates between 57 and 106 ms. The `path_frank_wolfe` pool shows
sporadic queue-wait p99 blips to ~70–98 ms with depth still 0 — a solve occasionally
landing behind a `MarketState` write lock, not a growing backlog. The same signature
appears at baseline everywhere: p99 runs ~1.5–2× p95 even at 1 rps (e.g. `E1-exploratory`
baseline p95 46 / p99 86). Block updates arrive roughly every 12 s and briefly stall
in-flight solves; that is the tail below the knee.

**HTTP/tokio overhead is ~1 ms and never binds.** Across every run, round-trip mean minus
solve-time mean is ~1 ms (e.g. `E1-exploratory` baseline round-trip mean 24 ms vs solve
mean 23 ms). The transport layer is not a factor at any measured rate.

---

## Q2 — Scaling strategy

### Vertical: linear per worker, up to a 7-vCPU node ceiling

On a pinned c6a node, capacity scales linearly with worker count (table above:
30 → 55 → 90 rps at 3.0× for 1 → 2 → 3 workers/pool). Per-request latency does not improve
with size — solve is single-threaded (baseline p95 stays ~45 ms across 2/4/7 vCPU). Scale
for load, not for speed.

The ceiling is the node, not the software. The general-purpose nodepool caps
`instance-cpu` at 8; an 8-vCPU pod plus ~310 m of daemonsets does not schedule, so **7 vCPU
is the practical per-pod ceiling** without a nodepool change.

### Horizontal: sub-linear under default placement, dominated by the instance lottery

The staging pods request `cpu: 2` with **no CPU limit**, so a pod's real capacity is set
by the spare CPU of whatever node Karpenter picks, not by its request. This makes default
placement a lottery:

- **Same 7-vCPU config, two different instances:** on a **c6a.2xlarge** it reaches
  **90 rps** at 3.0× with a normal 43 ms baseline; on a **t3a.2xlarge** (burstable) the
  same config baselines at 83 ms and collapses to the 5 s timeout at 60 rps — CPU-credit
  throttling from six workers touching per-block graph updates at idle
  (`E2-7vcpu.json` vs `E2-7vcpu-c6a.json`). **55 vs 90 rps for the identical pod spec.**
- **E3 horizontal runs** landed on mixed fleets (2 pods: c6a.2xlarge + c7i-flex.2xlarge;
  4 pods: 2× t3a.2xlarge + c6a.2xlarge + c7i-flex.2xlarge). Their baselines (88–93 ms) are
  polluted by the burstable/flex members, and their walls (85, 115 rps) are inflated by
  the c6a members bursting past their 2-vCPU request. The 4-pod wall is 3.3× the single-pod
  wall — sub-linear — while the mixed placement makes the number unrepeatable.

The controlled scaling law is the pinned-c6a vertical ladder; horizontal scaling is only
predictable once placement is pinned to a non-burstable family.

### Readiness and HPA feasibility

Pod readiness (E4): **100 s on a warm node**, **169 s cold** (Karpenter provisioned the
node in 28 s; the rest is image pull + graph warm-up). This is minutes, not seconds ⇒
reactive HPA is fine for gradual growth but cannot absorb sub-minute spikes. Keep **N+1**
headroom so the fleet survives one pod of demand or one pod loss while a replacement warms.

---

## Q3 — Today's solving time and capacity

Today's pod is 2 vCPU, 2 pools × 1 worker. Baseline (500 requests, `E1-exploratory.json`):

| | p50 | p95 | p99 |
|---|---:|---:|---:|
| round-trip | 31 ms | 46 ms | 86 ms |
| solve time | 30 ms | 45 ms | 85 ms |

- **strict-1.2× capacity ≈ 20 rps per current pod** (limit 55.2 ms; 20 rps passes at
  p95 51, 25 rps breaches at 64). 3.0× knee 30 rps, wall ~35 rps.
- **Encoding adds ~1 ms and does not change capacity** (baseline p95 47 vs 46; same 20 /
  30 rps verdicts) but it **amplifies the past-knee collapse**: at 35 rps the encoded run
  hits p95 789 ms vs 147 ms without encoding, and shows a p99 spike to 795 ms as early as
  25 rps (`E1-baseline-encoding.json`).
- **~31% of aggregator trades go unsolved on the staging config.** This is stable across
  every step and every configuration (27–32%) and is a property of the staging config
  (connector-token restriction + 3-hop limit), not load. It is a **product finding**, not
  a capacity limit — the solver is fast on what it does solve.

---

## Q4 — Cost model

### Pricing

eu-central-1 (Frankfurt) on-demand, Linux, **priced 2026-07-13**, 730 h/month. The c6a
family is linear in Frankfurt (c6a.2xlarge is exactly 4× c6a.large), so c6a.xlarge is the
interpolated family rate.

| Instance | vCPU | $/hr | $/month |
|---|---:|---:|---:|
| c6a.large | 2 | $0.0873 | $63.73 |
| c6a.xlarge | 4 | $0.1746 | $127.46 |
| c6a.2xlarge | 8 | $0.3492 | $254.92 |
| t3a.2xlarge (comparator) | 8 | ~$0.3008 | ~$219.58 |

Sources: [DoiT Compute — c6a.large](https://compute.doit.com/spot/eu-central-1/c6a.large),
[DoiT Compute — c6a.2xlarge](https://www.doit.com/compute/spot/eu-central-1/c6a.2xlarge),
[Vantage — c6a.xlarge](https://instances.vantage.sh/aws/ec2/c6a.xlarge),
[Vantage — t3a.2xlarge](https://instances.vantage.sh/aws/ec2/t3a.2xlarge).

### Cost per tier (pinned c6a)

Each config priced on the c6a instance that matches its vCPU footprint. $/rps-month uses
the 3.0× capacity (primary) and strict-1.2× (secondary).

| Config | Instance | $/month | 3.0× rps | $/rps-mo @3× | strict rps | $/rps-mo strict |
|---|---|---:|---:|---:|---:|---:|
| 2 vCPU, 2×1 | c6a.large | $63.73 | 30 | **$2.12** | 20 | $3.19 |
| 4 vCPU, 2×2 | c6a.xlarge | $127.46 | 55 | **$2.32** | 45 | **$2.83** |
| 7 vCPU, 2×3 | c6a.2xlarge | $254.92 | 90 | **$2.83** | 55 | $4.64 |

### Curve shape

- **Linear-ish in workers when the instance family is pinned.** Capacity rises ~15 rps per
  worker; cost rises with vCPU. At the 3.0× knee, $/rps drifts up modestly ($2.12 → $2.83)
  because the 8-vCPU node only runs a 7-vCPU pod (one vCPU stranded under the node cap) and
  the fixed background tax (Tycho feed + graph updates) is paid once per pod regardless of
  size.
- **Stepped by node granularity.** Usable sizes are 2 / 4 / 7 vCPU = c6a.large / xlarge /
  2xlarge; there is no smooth dial between them.
- **For the agreed strict SLO, the 4-vCPU / c6a.xlarge tier is the sweet spot** ($2.83
  /rps-month strict, vs $3.19 at 2 vCPU and $4.64 at 7 vCPU).
- **Sub-linear / lottery under default placement.** Off a pinned family, per-pod capacity
  swings with the host instance (55 vs 90 rps for the same 7-vCPU pod on t3a vs c6a), so
  effective $/rps is unpredictable until placement is pinned.
- **Hyperbolic near saturation.** Past the knee, latency collapses (35 rps: 147 ms
  no-encoding, 789 ms encoded), so capacity bought in that region is unusable — effective
  $/rps rises steeply. Size to the knee, not the wall.

---

## Findings and follow-ups

1. **Encoder 0%-split bug** — under `--encoding`, ~0.1–0.3% of quotes fail to encode:
   `path_frank_wolfe` emits a split route where a leg rounds to 0% without being reordered
   to the last swap, which `tycho-execution`'s encoder rejects. The quote solves; only
   encoding fails, so quote-only traffic never sees it. Details and a copy-paste issue
   draft in [`data/2026-07-13/FINDING-encoder-0pct-split.md`](data/2026-07-13/FINDING-encoder-0pct-split.md).
   **Issue not yet filed.**
2. **~31% of aggregator trades unsolved** on the staging config (connector-token + 3-hop
   restriction). Product finding — worth a look at whether the restriction is too tight.
3. **Burstable/flex exclusion needed in prod.** t3a and c7i-flex distort capacity badly;
   the prod nodepool or a pod `nodeSelector`/affinity must pin to c6a (or another
   non-burstable c-family).
4. **OpenAPI slippage drift** (pre-existing): `EncodingOptions.slippage` is declared
   `number` in `clients/openapi.json` but serializes as a string
   (`serde_as DisplayFromStr`, `fynd-rpc-types/src/lib.rs:450`). Needs a JIRA ticket.
5. **E5 (auth-proxy / front-door overhead) was not measured** — it needs a fynd-api key
   with sufficient rate limits. Public-endpoint overhead is still unquantified.
6. **HPA custom metric.** `worker_pool_queue_wait_seconds` is now live (metrics PR #300)
   and is the natural scaling signal — it separates "workers saturated" (queue grows) from
   "solver slower" (solve time grows) and is not polluted by background CPU the way raw CPU
   utilization is.

---

## Prod recommendation

- **Pod size:** 4 vCPU, 2 pools × 2 workers. Cheapest per unit of strict capacity
  ($2.83 /rps-month), well clear of the 7-vCPU node ceiling, and ~45 rps strict / 55 rps
  at the 3.0× knee per pod.
- **Placement:** pin to c6a (or equivalent non-burstable c-family) via nodepool or a pod
  `nodeSelector`/affinity. Exclude t3a and c7i-flex. This is the single biggest lever on
  predictable capacity.
- **Replicas:** size active replicas to `ceil(peak_rps / 45)` and add **one pod of
  headroom** (N+1), because readiness is ~100 s warm / ~169 s cold — too slow to absorb a
  spike reactively. Staging today sits at ~11% CPU on one pod, so prod is greenfield-low;
  a sensible start is **2 active + 1 headroom = 3 replicas** and let the HPA grow it.
- **HPA policy:** scale on `worker_pool_queue_wait_seconds` (target a few ms p95, well
  below the ~30 ms solve time) rather than CPU. Bounds e.g. `minReplicas: 3`,
  `maxReplicas: 8`. Keep no CPU limit only if placement is pinned; otherwise a
  burstable host with a CPU limit will throttle.

---

## Reproducing

One in-cluster Job reproduces a full ladder. From the helm-configuration repo
(branch `tl/fynd-capacity-job`):

```
kubectl -n staging-fynd create -f jobs/fynd-capacity-benchmark.job.yaml
```

The Job runs, against the in-cluster Service:

```
fynd-benchmark capacity \
  --url http://ethereum-fynd.staging-fynd.svc.cluster.local:3000 \
  --requests-file /tmp/trades.json \
  --baseline-rps 1 --ladder 5:5:200 --step-duration 60s \
  --slo-multiplier 3.0 --output report.json
```

- **Runbook** (how to run the Job, run the config matrix, restore staging, read the
  report): helm-configuration `docs/runbooks/fynd-capacity-benchmark.md`, branch
  `tl/fynd-capacity-job`.
- **Data** for this report: [`docs/scaling/data/2026-07-13/`](data/2026-07-13/) —
  E1/E2/E3/E6 capacity-report JSONs, `E6-metrics-samples.csv`, and the encoder finding.
- Restore staging after a run by re-syncing helmwave (drift self-heals).

## Dashboard

A Grafana dashboard covering solve/queue latencies, queue depth, request rate, solver
failures, and pod CPU/memory/HPA replicas is provisioned from
`terraform-infrastructure/terraform/modules/k8s-addons/dashboards/fynd/fynd-capacity.json`
(Grafana folder "fynd", uid `fynd-capacity`) — it ships automatically with the
VictoriaMetrics k8s stack, no manual import. Its `namespace` variable covers
`staging-fynd` and, once prod exports metrics, `prod-fynd`. Caveat: the
`worker_router_solve_duration_seconds` and `worker_pool_queue_wait_seconds` series are
exported as Prometheus summaries, not histograms — there are no `_bucket` series,
`histogram_quantile()` does not apply, and quantiles cannot be aggregated across pods,
so those panels show per-pod series.
