#!/usr/bin/env python3
"""Build focused 3-column route charts (Path Frank-Wolfe | Split | Split Hardened) per trade.

One standalone HTML per selected trade, sized for a clean headless-Chrome screenshot. Split is the
legacy exhaustive split; Split Hardened is the portfolio router.
"""
import json
from decimal import Decimal
from pathlib import Path

BASE = Path("/Users/markusschmitt/Documents/GitHub/fynd-split-hardening")
LANED = BASE / "routes_out/fresh"
CLASSIC = BASE / "routes_out/fresh_svg"
OUTDIR = BASE / "routes_out/pr"
OUTDIR.mkdir(parents=True, exist_ok=True)

COLS = [("path_frank_wolfe", "Path Frank-Wolfe"), ("split_bounded", "Split Bounded"),
        ("split", "Portfolio Split")]
SELECTION = [
    ("idx4", "2,000 WBTC → USDC"),
    ("idx3", "5,000 WETH → USDC"),
    ("idx354", "17,000 CVX → USDC"),
    ("idx2478", "64,993 sUSDe → USDC"),
]


def out_amount(algo, idx):
    r = json.load(open(LANED / f"{algo}__{idx}.json"))
    sink = r["sink"]
    dec = next((t["decimals"] for t in r["tokens"] if t["id"] == sink), 18)
    sym = next((t["symbol"] for t in r["tokens"] if t["id"] == sink), "?")
    raw = sum(int(s["amount_out"]) for s in r["swaps"] if s["target"] == sink)
    lanes = sum(1 for s in r["swaps"] if s["source"] == r["source"])
    return raw, f"{Decimal(raw) / (Decimal(10) ** dec):,.2f} {sym}", lanes


def svg(algo, idx):
    p = CLASSIC / f"{algo}__{idx}.svg"
    return p.read_text() if p.exists() else "<p>no route</p>"


for idx, label in SELECTION:
    pfw_raw, _, _ = out_amount("path_frank_wolfe", idx)
    leg_raw, _, _ = out_amount("split_bounded", idx)
    cells = []
    for algo, disp in COLS:
        raw, human, lanes = out_amount(algo, idx)
        badge = ""
        if algo == "split" and leg_raw:
            bps = (raw - leg_raw) / leg_raw * 10000
            badge = f'<span class="win">+{bps:,.1f} bps vs Split Bounded</span>'
        elif algo == "split_bounded" and pfw_raw:
            bps = (raw - pfw_raw) / pfw_raw * 10000
            if bps >= 1:
                badge = f'<span class="win2">+{bps:,.0f} bps vs PFW</span>'
        hl = " hl" if algo == "split" else ""
        cells.append(f'''<div class="cell{hl}">
          <div class="chd"><span class="algo">{disp}</span><span class="lanes">{lanes} lane{"s" if lanes!=1 else ""}</span></div>
          <div class="out">{human}{badge}</div>
          <div class="svg">{svg(algo, idx)}</div>
        </div>''')
    html = f'''<div class="wrap">
  <h2>{label}</h2>
  <div class="grid">{''.join(cells)}</div>
</div>
<style>
  * {{ box-sizing: border-box; margin: 0; }}
  body {{ background: #fff; }}
  .wrap {{ font-family: -apple-system, system-ui, sans-serif; color: #1a1a2e; padding: 16px;
    width: 1360px; }}
  h2 {{ font-size: 16px; margin: 0 0 12px; }}
  .grid {{ display: grid; grid-template-columns: repeat(3, 1fr); gap: 14px; }}
  .cell {{ border: 1px solid #e4e4ee; border-radius: 10px; padding: 12px; }}
  .cell.hl {{ border-color: #2ecc71; box-shadow: 0 0 0 2px #2ecc7133; }}
  .chd {{ display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px; }}
  .algo {{ font-weight: 600; font-size: 13px; }}
  .lanes {{ font-size: 11px; color: #888; background: #f0f0f6; padding: 1px 7px; border-radius: 999px; }}
  .out {{ font-size: 13px; color: #333; margin-bottom: 8px; font-variant-numeric: tabular-nums; }}
  .win {{ color: #1a8c4a; font-weight: 700; margin-left: 8px; }}
  .win2 {{ color: #2a6cc4; font-weight: 600; margin-left: 8px; }}
  .svg svg {{ width: 100%; height: auto; }}
</style>'''
    (OUTDIR / f"chart_{idx}.html").write_text(html)
    print(f"wrote chart_{idx}.html — {label}")
