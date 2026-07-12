#!/usr/bin/env python3
"""Assemble a side-by-side route comparison HTML: trades (rows) x algorithms (cols).

Inlines the Classic Sankey SVGs. `split` is the hardened portfolio router. The badge shows split's
output gain over the best single-path algorithm (Most Liquid / Bellman-Ford). Self-contained.
"""
import json
from decimal import Decimal
from pathlib import Path

BASE = Path("/Users/markusschmitt/Documents/GitHub/fynd-split-hardening")
LANED = BASE / "routes_out/laned"
CLASSIC = BASE / "routes_out/classic"
OUT = BASE / "routes_out/route_comparison.html"

ALGOS = [("most_liquid", "Most Liquid"), ("bellman_ford", "Bellman-Ford"),
         ("path_frank_wolfe", "Path Frank-Wolfe"), ("split", "split (hardened)")]
# (index, label, section) — XXL synthetic trades first, then the dataset trades.
TRADES = [
    (4, "2,000 WBTC → USDC", "XXL synthetic"),
    (3, "5,000 WETH → USDC", "XXL synthetic"),
    (1, "1,000 WETH → USDC", "XXL synthetic"),
    (2, "1,000 WETH → USDT", "XXL synthetic"),
    (0, "1,000 WETH → WBTC", "XXL synthetic"),
    (2827, "51,300 sUSDe → WETH", "Dataset trades"),
    (2478, "64,993 sUSDe → USDC", "Dataset trades"),
    (505, "102,621 sUSDe → USDT", "Dataset trades"),
    (2365, "333 WETH → wstETH", "Dataset trades"),
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


sections = {}
for idx, label, sect in TRADES:
    ml_raw, _, _ = out_amount(route("most_liquid", idx))
    row = []
    for algo, disp in ALGOS:
        r = route(algo, idx)
        raw, human, lanes = out_amount(r)
        badge = ""
        if algo == "split" and ml_raw:
            bps = (raw - ml_raw) / ml_raw * 10000
            if bps >= 1:
                badge = f'<span class="win">+{bps:,.0f} bps vs single-path</span>'
        lanetag = f'<span class="lanes">{lanes} lane{"s" if lanes != 1 else ""}</span>'
        row.append(f'''<div class="cell{' hl' if algo=='split' else ''}">
          <div class="chd"><span class="algo">{disp}</span>{lanetag}</div>
          <div class="out">{human}{badge}</div>
          <div class="svg">{svg(algo, idx)}</div>
        </div>''')
    sections.setdefault(sect, []).append(f'''<section class="trade">
      <h3>{label}</h3>
      <div class="grid">{''.join(row)}</div>
    </section>''')

body = ""
for sect, rows in sections.items():
    body += f'<h2 class="sect">{sect}</h2>' + "".join(rows)

html = f'''<div class="wrap">
<header>
  <h1>How each algorithm routes large trades</h1>
  <p>Four algorithms routing the same order on the frozen offline snapshot. Most Liquid,
  Bellman-Ford, and Path Frank-Wolfe route single-path or split lightly; the hardened <b>split</b>
  (portfolio) fans large orders across up to four pool-disjoint paths on a fine 256-chunk allocation
  grid. On XXL trades the single-path algorithms suffer severe price impact — for 2,000 WBTC → USDC,
  splitting more than doubles the output. Classic Sankey: token bars sized by flow, ribbon width =
  the leg's share of the order; repeated intermediate tokens are unrolled per leg to stay acyclic.</p>
</header>
{body}
<footer>Offline benchmark, market_snapshot.json (uniswap_v2 + v3). Classic Sankey via the fynd
route-visualization skill. `split` is the hardened portfolio router.</footer>
</div>
<style>
  :root {{ color-scheme: light; }}
  * {{ box-sizing: border-box; }}
  body {{ margin: 0; }}
  .wrap {{ font-family: -apple-system, system-ui, sans-serif; color: #1a1a2e; background: #f6f7fb;
    padding: 24px; max-width: 1700px; margin: 0 auto; }}
  header h1 {{ font-size: 22px; margin: 0 0 8px; }}
  header p {{ color: #4a4a5e; line-height: 1.5; max-width: 1000px; }}
  h2.sect {{ font-size: 13px; text-transform: uppercase; letter-spacing: 0.08em; color: #8a8a9a;
    margin: 30px 0 4px; }}
  .trade {{ margin: 14px 0; }}
  .trade h3 {{ font-size: 15px; margin: 0 0 10px; padding-bottom: 6px;
    border-bottom: 2px solid #e0e0ec; }}
  .grid {{ display: grid; grid-template-columns: repeat(4, 1fr); gap: 12px; }}
  @media (max-width: 1200px) {{ .grid {{ grid-template-columns: repeat(2, 1fr); }} }}
  @media (max-width: 700px) {{ .grid {{ grid-template-columns: 1fr; }} }}
  .cell {{ background: #fff; border: 1px solid #e4e4ee; border-radius: 10px; padding: 10px;
    overflow: hidden; }}
  .cell.hl {{ border-color: #2ecc71; box-shadow: 0 0 0 2px #2ecc7133; }}
  .chd {{ display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px;
    gap: 6px; }}
  .algo {{ font-weight: 600; font-size: 13px; }}
  .lanes {{ font-size: 11px; color: #888; background: #f0f0f6; padding: 1px 7px; border-radius: 999px;
    white-space: nowrap; }}
  .out {{ font-size: 12.5px; color: #333; margin-bottom: 8px; font-variant-numeric: tabular-nums; }}
  .win {{ color: #1a8c4a; font-weight: 600; margin-left: 8px; white-space: nowrap; }}
  .svg :is(svg) {{ width: 100%; height: auto; }}
  footer {{ margin-top: 24px; color: #8a8a9a; font-size: 12px; }}
</style>'''

OUT.write_text(html)
print(f"wrote {OUT}")
