#!/usr/bin/env python3
"""Assemble a side-by-side route comparison HTML: trades (rows) x algorithms (cols).

Inlines the Classic Sankey SVGs and labels each cell with the route's output amount and, for
split_max, its gain over split. Self-contained, no external assets.
"""
import json
from decimal import Decimal
from pathlib import Path

BASE = Path("/Users/markusschmitt/Documents/GitHub/fynd-split-hardening")
LANED = BASE / "routes_out/laned"
CLASSIC = BASE / "routes_out/classic"
OUT = BASE / "routes_out/route_comparison.html"

ALGOS = [("most_liquid", "Most Liquid"), ("bellman_ford", "Bellman-Ford"),
         ("path_frank_wolfe", "Path Frank-Wolfe"), ("split", "split (incumbent)"),
         ("split_max", "split_max (hardened)")]
TRADES = [
    (2827, "51,300 sUSDe → WETH"),
    (2365, "333 WETH → wstETH"),
    (2478, "64,993 sUSDe → USDC"),
    (505, "102,621 sUSDe → USDT"),
]


def route(algo, idx):
    return json.load(open(LANED / f"{algo}__idx{idx}.json"))


def out_amount(r):
    sink = r["sink"]
    dec = next((t["decimals"] for t in r["tokens"] if t["id"] == sink), 18)
    sym = next((t["symbol"] for t in r["tokens"] if t["id"] == sink), "?")
    raw = sum(int(s["amount_out"]) for s in r["swaps"] if s["target"] == sink)
    human = Decimal(raw) / (Decimal(10) ** dec)
    return raw, f"{human:,.4f} {sym}", len(r.get("paths", [{}]))


def svg(algo, idx):
    p = CLASSIC / f"{algo}__idx{idx}.svg"
    return p.read_text() if p.exists() else "<p>no route</p>"


cells = []
for idx, label in TRADES:
    split_raw, _, _ = out_amount(route("split", idx))
    row = []
    for algo, disp in ALGOS:
        r = route(algo, idx)
        raw, human, lanes = out_amount(r)
        badge = ""
        if algo == "split_max" and split_raw:
            bps = (raw - split_raw) / split_raw * 10000
            badge = f'<span class="win">+{bps:.1f} bps vs split</span>'
        lanetag = f'<span class="lanes">{lanes} lane{"s" if lanes != 1 else ""}</span>'
        row.append(f'''<div class="cell{' hl' if algo=='split_max' else ''}">
          <div class="chd"><span class="algo">{disp}</span>{lanetag}</div>
          <div class="out">{human}{badge}</div>
          <div class="svg">{svg(algo, idx)}</div>
        </div>''')
    cells.append(f'''<section class="trade">
      <h2>Trade #{idx} — {label}</h2>
      <div class="grid">{''.join(row)}</div>
    </section>''')

html = f'''<div class="wrap">
<header>
  <h1>Route comparison on trades where split_max beats split</h1>
  <p>Five algorithms routing the same order on the frozen offline snapshot. Most Liquid, Bellman-Ford,
  and Path Frank-Wolfe route single-path or split lightly; <b>split</b> and <b>split_max</b> split
  across the same pools, but split_max's finer 256-chunk allocation extracts more output than split's
  20-chunk grid. Classic Sankey: token bars sized by flow, ribbon width = the leg's share of the
  order. Repeated intermediate tokens are unrolled per leg so the graph stays acyclic.</p>
</header>
{''.join(cells)}
<footer>Offline benchmark, market_snapshot.json. Classic Sankey via the fynd route-visualization skill.</footer>
</div>
<style>
  :root {{ color-scheme: light; }}
  * {{ box-sizing: border-box; }}
  body {{ margin: 0; }}
  .wrap {{ font-family: -apple-system, system-ui, sans-serif; color: #1a1a2e; background: #f6f7fb;
    padding: 24px; max-width: 1800px; margin: 0 auto; }}
  header h1 {{ font-size: 22px; margin: 0 0 8px; }}
  header p {{ color: #4a4a5e; line-height: 1.5; max-width: 1000px; }}
  .trade {{ margin: 28px 0; }}
  .trade h2 {{ font-size: 16px; margin: 0 0 12px; padding-bottom: 6px;
    border-bottom: 2px solid #e0e0ec; }}
  .grid {{ display: grid; grid-template-columns: repeat(5, 1fr); gap: 12px; }}
  @media (max-width: 1400px) {{ .grid {{ grid-template-columns: repeat(3, 1fr); }} }}
  @media (max-width: 800px) {{ .grid {{ grid-template-columns: 1fr; }} }}
  .cell {{ background: #fff; border: 1px solid #e4e4ee; border-radius: 10px; padding: 10px;
    overflow: hidden; }}
  .cell.hl {{ border-color: #2ecc71; box-shadow: 0 0 0 2px #2ecc7133; }}
  .chd {{ display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px;
    gap: 6px; }}
  .algo {{ font-weight: 600; font-size: 13px; }}
  .lanes {{ font-size: 11px; color: #888; background: #f0f0f6; padding: 1px 7px; border-radius: 999px;
    white-space: nowrap; }}
  .out {{ font-size: 13px; color: #333; margin-bottom: 8px; font-variant-numeric: tabular-nums; }}
  .win {{ color: #1a8c4a; font-weight: 600; margin-left: 8px; }}
  .svg :is(svg) {{ width: 100%; height: auto; }}
  footer {{ margin-top: 24px; color: #8a8a9a; font-size: 12px; }}
</style>'''

OUT.write_text(html)
print(f"wrote {OUT}")
