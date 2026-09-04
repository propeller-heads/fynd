"""Plot throughput (RPS) vs worker count from fynd-benchmark `scale` JSON reports.

Left panel  - absolute throughput vs workers (log y), with an ideal-linear
              reference per multi-point sweep.
Right panel - scaling efficiency (speedup vs the 1-worker point) for each sweep,
              against an ideal y=x diagonal.

Single-point reports (e.g. a fixed:16 fill-in) are drawn as standalone markers
on the throughput panel. The load-generator concurrency cap (parsed from the
`fixed:N` parallelization mode) is marked as a vertical line.
"""

import argparse
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

COLORS = ["#4C72B0", "#DD8452", "#55A868", "#C44E52", "#8172B3"]
GRAY = "#888"


def load(path):
    d = json.loads(Path(path).read_text())
    pts = sorted(d["points"], key=lambda p: p["total_workers"])
    workers = [p["total_workers"] for p in pts]
    rps = [p["throughput_rps"] for p in pts]
    return d["config"], workers, rps


def _cap_from_mode(mode):
    if isinstance(mode, str) and mode.startswith("fixed:"):
        try:
            return int(mode.split(":", 1)[1])
        except ValueError:
            return None
    return None


def plot(series, title, out):
    fig, (ax_abs, ax_eff) = plt.subplots(1, 2, figsize=(14, 5.5))
    all_workers = sorted({w for _, _, ws, _ in series for w in ws})
    caps = sorted({c for _, cfg, ws, _ in series if len(ws) > 1
                   and (c := _cap_from_mode(cfg.get("parallelization_mode")))})
    drew_ideal = False
    for i, (label, _cfg, workers, rps) in enumerate(series):
        color = COLORS[i % len(COLORS)]
        single = len(workers) == 1
        ax_abs.plot(workers, rps, "*" if single else "o-", color=color,
                    markersize=16 if single else 6, linewidth=1.6, label=label)
        if not single:
            base_w, base_rps = workers[0], rps[0]
            ax_abs.plot(workers, [base_rps * w / base_w for w in workers],
                        "--", color=color, linewidth=0.8, alpha=0.5)
            ax_eff.plot(workers, [r / base_rps for r in rps], "o-", color=color,
                        linewidth=1.6, markersize=6, label=label)
            ax_eff.plot(workers, [w / base_w for w in workers], "--", color=color,
                        linewidth=0.8, alpha=0.5,
                        label="ideal linear" if not drew_ideal else None)
            drew_ideal = True

    for cap in caps:
        ax_abs.axvline(cap, color=GRAY, linestyle=":", linewidth=1)
        ax_abs.annotate(f"load-gen cap (fixed:{cap})", xy=(cap, 0.98),
                        xycoords=ax_abs.get_xaxis_transform(), color=GRAY,
                        fontsize=8, va="top", ha="left", rotation=90)

    ax_abs.set_yscale("log")
    ax_abs.set_xscale("log", base=2)
    ax_abs.set_xticks(all_workers)
    ax_abs.set_xticklabels(all_workers)
    ax_abs.set_xlabel("worker pool size")
    ax_abs.set_ylabel("throughput (req/s, log scale)")
    ax_abs.set_title("Throughput vs workers  (dashed = ideal linear)", fontsize=11)
    ax_abs.legend(fontsize=9)
    ax_abs.grid(True, which="both", alpha=0.2)

    ax_eff.set_xscale("log", base=2)
    ax_eff.set_xticks(all_workers)
    ax_eff.set_xticklabels(all_workers)
    ax_eff.set_xlabel("worker pool size")
    ax_eff.set_ylabel("speedup vs smallest pool")
    ax_eff.set_title("Scaling efficiency", fontsize=11)
    ax_eff.legend(fontsize=9)
    ax_eff.grid(True, which="both", alpha=0.2)

    fig.suptitle(title, fontsize=13)
    fig.tight_layout(rect=(0, 0, 1, 0.96))
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("files", nargs="+", help="scale JSON report(s)")
    ap.add_argument("--labels", nargs="+", required=True, help="one label per file")
    ap.add_argument("-o", "--outdir", default=".")
    ap.add_argument("--name", default="scale_rps", help="output filename stem")
    ap.add_argument("--title", default="Throughput scaling")
    args = ap.parse_args()
    if len(args.labels) != len(args.files):
        ap.error("number of --labels must match number of files")
    Path(args.outdir).mkdir(parents=True, exist_ok=True)
    series = []
    for path, label in zip(args.files, args.labels):
        cfg, workers, rps = load(path)
        series.append((label, cfg, workers, rps))
    plot(series, args.title, Path(args.outdir) / f"{args.name}.png")


if __name__ == "__main__":
    main()
