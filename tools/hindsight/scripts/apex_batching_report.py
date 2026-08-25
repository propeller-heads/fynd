#!/usr/bin/env python3
"""Render the APEX batching-validation report.

Reads a monitor run's `apex-orders.jsonl` and `apex-blocks.jsonl` (written by
`hindsight monitor --apex-batching-dir <dir>`) and writes one self-contained
interactive HTML file.

Usage: apex_batching_report.py <dir> [-o report.html]

Accounting (matches the experiment plan):
- S0 is the settled on-chain outcome. Unfilled and out-of-universe orders count at S0.
- Schema-2 records (current): a partial fill executes fully at the clearing price —
  the batcher supplies the buy-token remainder and receives the unsold sell amount.
- Schema-1 records (old runs): a partial fill counted at S0, with the batcher
  absorbing the cleared slice.
- All cross-token aggregation uses ETH valuations at each block's derived
  prices, taken at solve time.
"""

import argparse
import html
import json
import statistics
from collections import defaultdict
from pathlib import Path


def load_jsonl(path: Path) -> list[dict]:
    records = []
    if not path.exists():
        return records
    with path.open() as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return records


def effective_out_eth(rec: dict) -> float:
    """The scenario outcome for one order in ETH. Cleared: the batch fill. Partial under
    schema 2: APEX's fill plus the batcher's buy-token top-up (a full fill at clearing
    price). Everything else (and schema-1 partials) counts at S0."""
    if rec["status"] == "cleared":
        return rec["apex_bought_eth"]
    if rec["status"] == "partial" and rec.get("schema", 1) >= 2:
        return rec["apex_bought_eth"] + rec["batcher_sold_eth"]
    return rec["settled_amount_out_eth"]


def user_bought_raw(rec: dict) -> int | None:
    """What the user receives in raw buy-token units, when the order executes in the batch."""
    if rec["status"] == "cleared":
        return int(rec["apex_bought"])
    if rec["status"] == "partial" and rec.get("schema", 1) >= 2:
        return int(rec["apex_bought"]) + int(rec["batcher_sold"])
    return None


def order_improvement_bps(rec: dict) -> float | None:
    """Per-order improvement of the batch execution vs settled, exact in raw token units."""
    bought = user_bought_raw(rec)
    settled = int(rec["settled_amount_out"])
    if bought is None or settled <= 0:
        return None
    return (bought - settled) / settled * 10_000


def s3_out_eth(rec: dict) -> float:
    """S3: per-order cherry-pick of the better execution, S2 vs S0, decided exactly in raw
    buy-token units and valued in ETH. Orders with no S2 execution count at S0."""
    bought = user_bought_raw(rec)
    if bought is not None and bought > int(rec["settled_amount_out"]):
        return effective_out_eth(rec)
    return rec["settled_amount_out_eth"]


def aggregate(orders: list[dict], blocks: list[dict]) -> dict:
    runs = {"s1": [r for r in orders if r["run"] == "s1"], "s2": [r for r in orders if r["run"] == "s2"]}
    agg: dict = {"blocks": blocks}

    # Headline: scenario totals in ETH over every order (uncleared ones count at S0, so
    # they contribute zero delta but stay in the denominator).
    for run, recs in runs.items():
        settled = sum(r["settled_amount_out_eth"] for r in recs)
        effective = sum(effective_out_eth(r) for r in recs)
        agg[f"{run}_settled_eth"] = settled
        agg[f"{run}_effective_eth"] = effective
        agg[f"{run}_delta_eth"] = effective - settled
        agg[f"{run}_delta_bps"] = (effective - settled) / settled * 10_000 if settled > 0 else 0.0

    s2 = runs["s2"]
    s3_effective = sum(s3_out_eth(r) for r in s2)
    agg["s3_delta_eth"] = s3_effective - agg["s2_settled_eth"]
    agg["s3_delta_bps"] = (
        agg["s3_delta_eth"] / agg["s2_settled_eth"] * 10_000 if agg["s2_settled_eth"] > 0 else 0.0
    )

    statuses = defaultdict(int)
    for r in s2:
        statuses[r["status"]] += 1
    agg["statuses"] = dict(statuses)
    agg["orders_total"] = len(s2)

    imps = [order_improvement_bps(r) for r in s2]
    imps = [i for i in imps if i is not None]
    agg["improvements_bps"] = imps
    in_universe = sum(1 for r in s2 if r["status"] != "out_of_universe")
    agg["in_universe"] = in_universe
    wins = sum(1 for i in imps if i > 0)
    agg["wins"] = wins
    agg["win_rate"] = wins / in_universe if in_universe else 0.0
    agg["median_improvement_bps"] = statistics.median(imps) if imps else 0.0

    agg["batcher_gross_eth"] = sum(r["batcher_sold_eth"] for r in s2)
    agg["batcher_bought_eth"] = sum(r["batcher_bought_eth"] for r in s2)

    # CoW share: cleared volume that never touched an AMM pool. Pool volumes come from the
    # block records (S2 pool clearings, valued at the same derived prices).
    cleared_bought_eth = sum(r["apex_bought_eth"] for r in s2 if r["status"] in ("cleared", "partial"))
    pool_bought_eth = sum(v["bought_eth"] for b in blocks for v in b["s2_pool_volumes"])
    agg["cleared_bought_eth"] = cleared_bought_eth
    agg["pool_bought_eth"] = pool_bought_eth
    agg["cow_share"] = max(0.0, 1.0 - pool_bought_eth / cleared_bought_eth) if cleared_bought_eth > 0 else 0.0

    # CoW diagnostics. Potential: opposing decoded flow on the same pair in the same block
    # (min of the two directions' settled input value), regardless of what APEX did with it.
    # Realized: pairs where both directions actually cleared in the batch.
    flow: dict[tuple, float] = defaultdict(float)
    for r in s2:
        flow[(r["block"], r["sell_token"], r["buy_token"])] += r["amount_in_eth"]
    cow_potential = 0.0
    potential_pairs = []
    seen = set()
    for (block, sell, buy), eth in flow.items():
        if (block, buy, sell) in seen or (block, sell, buy) in seen:
            continue
        seen.add((block, sell, buy))
        opposite = flow.get((block, buy, sell), 0.0)
        if opposite > 0 and eth > 0:
            matched = min(eth, opposite)
            cow_potential += matched
            potential_pairs.append({"block": block, "sell": sell, "buy": buy, "eth": matched})
    agg["cow_potential_eth"] = cow_potential
    agg["cow_potential_pairs"] = len(potential_pairs)
    cleared_flow: dict[tuple, float] = defaultdict(float)
    for r in s2:
        if r["status"] in ("cleared", "partial"):
            cleared_flow[(r["block"], r["sell_token"], r["buy_token"])] += r["apex_bought_eth"]
    realized_pairs = 0
    realized_eth = 0.0
    for (block, sell, buy), eth in cleared_flow.items():
        if sell < buy and eth > 0:
            opposite = cleared_flow.get((block, buy, sell), 0.0)
            if opposite > 0:
                realized_pairs += 1
                realized_eth += min(eth, opposite)
    agg["cow_realized_pairs"] = realized_pairs
    agg["cow_realized_eth"] = realized_eth

    # Per-block rollups keyed by block number.
    per_block: dict[int, dict] = {}
    for b in blocks:
        per_block[b["block"]] = {
            "block": b["block"],
            "orders": b["orders_in"],
            "oou": b["out_of_universe"],
            "sandwiched": b["sandwiched_excluded"],
            "pools": b["pools_native_v2"] + b["pools_native_v3"] + b["pools_wrapped"],
            "pools_split": f'{b["pools_native_v2"]}/{b["pools_native_v3"]}/{b["pools_wrapped"]}',
            "amm_legs": len(b["s2_pool_volumes"]),
            "s2_ms": b["s2_solve_ms"],
            "s1_ms": b["s1_solve_ms_total"],
            "deadline": bool(b["s2_deadline_fired"]) or b["s1_deadline_fired"] > 0,
            "s1_delta_bps": 0.0,
            "s2_delta_bps": 0.0,
            "s3_delta_bps": 0.0,
            "s2_surplus_eth": 0.0,
            "settled_eth": 0.0,
            "batcher_eth": 0.0,
            "statuses": defaultdict(int),
        }
    for run in ("s1", "s2"):
        for r in runs[run]:
            blk = per_block.get(r["block"])
            if blk is None:
                continue
            if run == "s2":
                blk["settled_eth"] += r["settled_amount_out_eth"]
                blk["batcher_eth"] += r["batcher_sold_eth"]
                blk["statuses"][r["status"]] += 1
    for run, out_eth in (("s1", effective_out_eth), ("s2", effective_out_eth), ("s3", s3_out_eth)):
        source = runs["s2"] if run == "s3" else runs[run]
        by_block_settled = defaultdict(float)
        by_block_eff = defaultdict(float)
        for r in source:
            by_block_settled[r["block"]] += r["settled_amount_out_eth"]
            by_block_eff[r["block"]] += out_eth(r)
        for block_num, blk in per_block.items():
            settled = by_block_settled.get(block_num, 0.0)
            blk[f"{run}_surplus_eth"] = by_block_eff[block_num] - settled
            if settled > 0:
                blk[f"{run}_delta_bps"] = (by_block_eff[block_num] - settled) / settled * 10_000
    for blk in per_block.values():
        blk["statuses"] = dict(blk["statuses"])
        blk["executed"] = blk["statuses"].get("cleared", 0) + blk["statuses"].get("partial", 0)
    agg["per_block"] = [per_block[k] for k in sorted(per_block)]
    agg["blocks_won"] = sum(1 for b in agg["per_block"] if b["s2_delta_bps"] > 0)
    agg["blocks_lost"] = sum(1 for b in agg["per_block"] if b["s2_delta_bps"] < 0)

    # The batcher only ever settles one batch at a time, so the inventory it must hold is
    # the largest single block's top-up total, not the run's sum.
    peak = max(agg["per_block"], key=lambda b: b["batcher_eth"], default=None)
    agg["batcher_peak_eth"] = peak["batcher_eth"] if peak else 0.0
    agg["batcher_peak_block"] = peak["block"] if peak else None

    # Sell-token volume, overall and per block (the report's per-token view).
    token_volume: dict[tuple, dict] = {}
    for r in s2:
        key = (r["sell_symbol"], r["sell_token"])
        entry = token_volume.setdefault(
            key, {"symbol": r["sell_symbol"], "token": r["sell_token"], "orders": 0, "eth": 0.0, "by_block": defaultdict(float)}
        )
        entry["orders"] += 1
        entry["eth"] += r["amount_in_eth"]
        entry["by_block"][r["block"]] += r["amount_in_eth"]
    vols = sorted(token_volume.values(), key=lambda e: -e["eth"])
    for v in vols:
        v["by_block"] = dict(v["by_block"])
    agg["token_volumes"] = vols

    # Batcher inventory per token (gross sold = what it must hold; net = sold - bought).
    inv: dict[tuple, dict] = {}
    for r in s2:
        if r["status"] != "partial":
            continue
        key = (r["sell_symbol"], r["sell_token"])
        e = inv.setdefault(key, {"symbol": r["sell_symbol"], "token": r["sell_token"], "sold_eth": 0.0, "orders": 0})
        e["sold_eth"] += r["batcher_sold_eth"]
        e["orders"] += 1
    agg["batcher_by_token"] = sorted(inv.values(), key=lambda e: -e["sold_eth"])

    # Order explorer: every S2 order, with its S1 outcome joined by order_id.
    s1_by_id = {r["order_id"]: r for r in runs["s1"]}
    explorer = []
    for r in sorted(s2, key=lambda x: -x["settled_amount_out_eth"]):
        s1r = s1_by_id.get(r["order_id"])
        explorer.append(
            {
                "block": r["block"],
                "tx": r["tx_hash"],
                "venue": short_label(r["venue"]),
                "venue_full": r["venue"],
                "solver": r["solver"],
                "pair": f'{short_label(r["sell_symbol"])}→{short_label(r["buy_symbol"])}',
                "pair_full": f'{r["sell_symbol"]}→{r["buy_symbol"]}',
                "in_eth": r["amount_in_eth"],
                "status": r["status"],
                "s1_status": s1r["status"] if s1r else "?",
                "s2_bps": order_improvement_bps(r),
                "s1_bps": order_improvement_bps(s1r) if s1r else None,
                "batcher_eth": r["batcher_sold_eth"],
            }
        )
    agg["explorer"] = explorer
    return agg


CSS = """
:root { color-scheme: light dark; }
body {
  margin: 0; padding: 24px; font: 14px/1.45 -apple-system, 'Segoe UI', Roboto, sans-serif;
  background: var(--surface-1); color: var(--text-primary); max-width: 1200px; margin-inline: auto;
}
.viz-root {
  --surface-1: #fcfcfb; --surface-2: #f1f0ee; --grid: #e4e3e0;
  --text-primary: #0b0b0b; --text-secondary: #52514e; --text-muted: #8a897f;
  --s1: #2a78d6; --s2: #eb6834; --s3: #1baf7a; --s4: #eda100;
  --good: #008300; --bad: #e34948;
}
@media (prefers-color-scheme: dark) {
  .viz-root {
    --surface-1: #1a1a19; --surface-2: #262624; --grid: #3a3936;
    --text-primary: #ffffff; --text-secondary: #c3c2b7; --text-muted: #8a897f;
    --s1: #3987e5; --s2: #d95926; --s3: #199e70; --s4: #c98500;
    --good: #30b830; --bad: #e66767;
  }
}
h1 { font-size: 20px; margin: 0 0 4px; }\n.variant-h { margin-top: 40px; padding-top: 16px; border-top: 2px solid var(--grid); text-transform: capitalize; }
h2 { font-size: 15px; margin: 32px 0 10px; color: var(--text-primary); }
h1, h2 { scroll-margin-top: 14px; }
/* Table of contents, parked in the left margin the centred page leaves free. It only fits
   once the viewport is wide enough for it; below that the page is unchanged. */
.toc {
  position: fixed; top: 20px; left: 14px; width: 168px; font-size: 12px; line-height: 1.5;
  max-height: calc(100vh - 40px); overflow-y: auto;
}
.toc a { display: block; padding: 1px 0; color: var(--text-muted); text-decoration: none; }
.toc a:hover { color: var(--text-primary); }
.toc .toc-v { margin-top: 9px; font-weight: 600; color: var(--text-secondary); text-transform: capitalize; }
.toc .toc-s { padding-left: 10px; }
.toc a.active { color: var(--s1); }
.toc .toc-s.active { border-left: 2px solid var(--s1); padding-left: 8px; }
@media (max-width: 1560px) { .toc { display: none; } }
.sub { color: var(--text-secondary); margin-bottom: 20px; }
.tiles { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 10px; }
.tile { background: var(--surface-2); border-radius: 8px; padding: 12px 14px; }
.tile .v { font-size: 22px; font-weight: 650; letter-spacing: -0.02em; }
.tile .l { color: var(--text-secondary); font-size: 12px; margin-top: 2px; }
.tile .d { color: var(--text-muted); font-size: 11px; margin-top: 2px; }
.tile a { color: var(--s1); text-decoration: none; }
.tile a:hover { text-decoration: underline; }
.tile .def { color: var(--text-muted); font-size: 10.5px; margin-top: 6px; line-height: 1.35; border-top: 1px solid var(--grid); padding-top: 5px; }
.pos { color: var(--good); } .neg { color: var(--bad); }
.chart-box { background: var(--surface-2); border-radius: 8px; padding: 14px; margin-top: 8px; }
.legend { display: flex; gap: 16px; font-size: 12px; color: var(--text-secondary); margin-bottom: 6px; flex-wrap: wrap; }
.legend .k { display: inline-block; width: 10px; height: 10px; border-radius: 3px; margin-right: 5px; vertical-align: -1px; }
svg text { fill: var(--text-secondary); font-size: 11px; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
td a { color: var(--s1); text-decoration: none; }
td a:hover { text-decoration: underline; }
th, td { text-align: right; padding: 6px 10px; border-bottom: 1px solid var(--grid); white-space: nowrap; }
th:first-child, td:first-child { text-align: left; }
th { color: var(--text-secondary); font-weight: 600; cursor: pointer; user-select: none; position: sticky; top: 0; background: var(--surface-1); }
th.asc::after { content: " ↑"; } th.desc::after { content: " ↓"; }
tr:hover td { background: var(--surface-2); }
.scroll { overflow-x: auto; max-height: 480px; overflow-y: auto; border: 1px solid var(--grid); border-radius: 8px; }
.filters { display: flex; gap: 10px; margin: 8px 0; flex-wrap: wrap; }
select, input[type=text] {
  background: var(--surface-2); color: var(--text-primary); border: 1px solid var(--grid);
  border-radius: 6px; padding: 5px 8px; font-size: 13px;
}
#tooltip {
  position: fixed; pointer-events: none; background: var(--surface-2); color: var(--text-primary);
  border: 1px solid var(--grid); border-radius: 6px; padding: 6px 9px; font-size: 12px;
  box-shadow: 0 2px 10px rgba(0,0,0,.18); display: none; z-index: 10; white-space: pre;
}
.note { color: var(--text-muted); font-size: 12px; margin-top: 6px; }
.foot { margin-top: 36px; padding-top: 14px; border-top: 1px solid var(--grid); color: var(--text-secondary); font-size: 12.5px; }
.foot li { margin-bottom: 4px; }
.badge { display: inline-block; padding: 1px 7px; border-radius: 10px; font-size: 11.5px; background: var(--surface-2); }
"""

JS = """
const $ = (s, el=document) => el.querySelector(s);
const fmt = (x, d=2) => x == null ? '—' : x.toLocaleString('en-US', {maximumFractionDigits: d, minimumFractionDigits: 0});
const bpsCls = x => x == null ? '' : (x >= 0 ? 'pos' : 'neg');
// Token symbols are arbitrary on-chain strings and rows are built with innerHTML.
const esc = s => String(s ?? '').replace(/&/g,'&amp;').replace(/</g,'&lt;')
  .replace(/>/g,'&gt;').replace(/"/g,'&quot;');
const tt = $('#tooltip');
function showTT(e, text) { tt.style.display='block'; tt.textContent=text; moveTT(e); }
function moveTT(e) { tt.style.left = (e.clientX+14)+'px'; tt.style.top = (e.clientY+10)+'px'; }
function hideTT() { tt.style.display='none'; }

// Grouped/stacked bar chart on inline SVG. series: [{name, color, values:[..]}], labels per group.
function barChart(el, labels, series, opts={}) {
  const W = el.clientWidth || 1100, H = opts.h || 220, padL = 56, padB = 26, padT = 8, padR = 8;
  const stacked = !!opts.stacked;
  const groups = labels.length;
  let maxV = 0, minV = 0;
  if (stacked) {
    for (let g=0; g<groups; g++) maxV = Math.max(maxV, series.reduce((a,s)=>a+s.values[g],0));
  } else {
    for (const s of series) for (const v of s.values) { maxV = Math.max(maxV, v); minV = Math.min(minV, v); }
  }
  if (maxV === 0 && minV === 0) maxV = 1;
  const span = maxV - minV || 1;
  const y = v => padT + (H-padT-padB) * (1 - (v-minV)/span);
  const y0 = y(0);
  const bandW = (W-padL-padR)/groups;
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns,'svg');
  svg.setAttribute('width','100%'); svg.setAttribute('viewBox',`0 0 ${W} ${H}`);
  // gridlines at clean steps
  const steps = 4;
  for (let i=0;i<=steps;i++) {
    const v = minV + span*i/steps;
    const line = document.createElementNS(ns,'line');
    line.setAttribute('x1',padL); line.setAttribute('x2',W-padR);
    line.setAttribute('y1',y(v)); line.setAttribute('y2',y(v));
    line.setAttribute('stroke','var(--grid)'); line.setAttribute('stroke-width','1');
    svg.appendChild(line);
    const t = document.createElementNS(ns,'text');
    t.setAttribute('x',padL-6); t.setAttribute('y',y(v)+4); t.setAttribute('text-anchor','end');
    t.textContent = fmt(v, Math.abs(span)<10?1:0);
    svg.appendChild(t);
  }
  const barW = Math.min(24, stacked ? bandW*0.6 : (bandW*0.7)/series.length);
  for (let g=0; g<groups; g++) {
    let acc = 0;
    series.forEach((s, si) => {
      const v = s.values[g];
      let x, yTop, h;
      if (stacked) {
        x = padL + bandW*g + (bandW-barW)/2;
        h = Math.max(0, y(acc) - y(acc+v));
        yTop = y(acc+v); acc += v;
        if (h > 2) { h -= 2; }           // 2px surface gap between segments
      } else {
        const gw = barW*series.length + 2*(series.length-1);
        x = padL + bandW*g + (bandW-gw)/2 + si*(barW+2);
        yTop = v >= 0 ? y(v) : y0;
        h = Math.abs(y(v) - y0);
      }
      if (h <= 0) return;
      const r = Math.min(4, barW/2, h);
      const yr = v >= 0 || stacked;      // round the data end, square at baseline
      const p = document.createElementNS(ns,'path');
      const x2 = x + barW, yB = yTop + h;
      const d = yr
        ? `M${x},${yB} L${x},${yTop+r} Q${x},${yTop} ${x+r},${yTop} L${x2-r},${yTop} Q${x2},${yTop} ${x2},${yTop+r} L${x2},${yB} Z`
        : `M${x},${yTop} L${x2},${yTop} L${x2},${yB-r} Q${x2},${yB} ${x2-r},${yB} L${x+r},${yB} Q${x},${yB} ${x},${yB-r} Z`;
      p.setAttribute('d', d);
      p.setAttribute('fill', s.color);
      p.addEventListener('mousemove', e => { moveTT(e); showTT(e, `${labels[g]}\\n${s.name}: ${fmt(v)}`); });
      p.addEventListener('mouseleave', hideTT);
      svg.appendChild(p);
    });
    if (groups <= 30 || g % Math.ceil(groups/20) === 0) {
      const t = document.createElementNS(ns,'text');
      t.setAttribute('x', padL + bandW*g + bandW/2); t.setAttribute('y', H-8);
      t.setAttribute('text-anchor','middle');
      t.textContent = labels[g];
      svg.appendChild(t);
    }
  }
  el.appendChild(svg);
}

function histogram(el, values, color) {
  if (!values.length) { el.textContent = 'No cleared orders.'; return; }
  // Clamp to the 2nd–98th percentile so dust-trade outliers don't stretch the axis;
  // clamped values fold into the edge bins.
  const sorted = [...values].sort((a,b)=>a-b);
  const q = p => sorted[Math.min(sorted.length-1, Math.floor(p*sorted.length))];
  const lo0 = q(0.02), hi0 = q(0.98);
  values = values.map(v => Math.min(hi0, Math.max(lo0, v)));
  const lo = Math.min(...values), hi = Math.max(...values);
  const n = Math.min(40, Math.max(8, Math.round(Math.sqrt(values.length)*2)));
  const w = (hi-lo)/n || 1;
  const bins = new Array(n).fill(0);
  for (const v of values) bins[Math.min(n-1, Math.floor((v-lo)/w))]++;
  const labels = bins.map((_,i)=>fmt(lo+w*(i+0.5),1));
  barChart(el, labels, [{name:'orders', color, values:bins}], {h:180});
}

// Light up the table-of-contents entry for whichever section the reader is in: the last
// heading to have passed the top of the viewport. A subsection also lights its variant.
function initTocSpy() {
  const links = [...document.querySelectorAll('.toc a')];
  const targets = links
    .map(a => ({a, el: document.getElementById(a.hash.slice(1))}))
    .filter(t => t.el);
  if (!targets.length) return;
  const groupOf = a => a.classList.contains('toc-s')
    ? links.find(l => l.hash === '#v-' + a.hash.slice(1).split('-')[0])
    : null;
  let queued = false;
  function update() {
    queued = false;
    let current = targets[0];
    for (const t of targets) if (t.el.getBoundingClientRect().top <= 100) current = t;
    // The last section is too short to reach the trigger line, so the end of the page is it.
    if (scrollY + innerHeight >= document.documentElement.scrollHeight - 2) {
      current = targets[targets.length - 1];
    }
    links.forEach(l => l.classList.remove('active'));
    current.a.classList.add('active');
    groupOf(current.a)?.classList.add('active');
  }
  addEventListener('scroll', () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(update);
  }, {passive: true});
  addEventListener('resize', update);
  update();
}

function sortable(table, data, render, defaultKey) {
  let key = defaultKey, dir = -1;
  const ths = table.querySelectorAll('th');
  ths.forEach(th => th.addEventListener('click', () => {
    const k = th.dataset.k; if (!k) return;
    dir = (k === key) ? -dir : -1; key = k;
    ths.forEach(t => t.classList.remove('asc','desc'));
    th.classList.add(dir === 1 ? 'asc' : 'desc');
    draw();
  }));
  function draw() {
    const rows = [...data()].sort((a,b) => {
      const va = a[key], vb = b[key];
      if (va == null) return 1; if (vb == null) return -1;
      return (va < vb ? -1 : va > vb ? 1 : 0) * dir;
    });
    table.querySelector('tbody').innerHTML = rows.map(render).join('');
  }
  draw();
  return draw;
}
"""


def esc(x):
    return html.escape(str(x))


def short_label(text: str) -> str:
    """An undecoded venue or token comes through as a raw address, which would stretch its
    column across the page; show it truncated (the full value stays in the cell's title)."""
    if text.startswith("0x") and len(text) >= 40:
        return f"{text[:6]}…{text[-4:]}"
    return text


BLOCK_ROWS_JS = """
  r => `<tr><td><a href="explorer/block_${r.block}.html#${name}">${r.block}</a></td><td>${r.orders}</td><td>${r.executed}</td><td>${r.oou}</td><td>${r.sandwiched}</td>
  <td>${r.pools} <span class="badge">${r.pools_split}</span></td>
  <td class="${bpsCls(r.s1_delta_bps)}">${fmt(r.s1_delta_bps)}</td>
  <td class="${bpsCls(r.s2_delta_bps)}">${fmt(r.s2_delta_bps)}</td>
  ${name === 'permissive' ? `<td class="${bpsCls(r.s3_delta_bps)}">${fmt(r.s3_delta_bps)}</td>` : ''}
  <td class="${bpsCls(r.s2_surplus_eth)}">${fmt(r.s2_surplus_eth,6)}</td>
  <td>${fmt(r.settled_eth,3)}</td><td>${fmt(r.batcher_eth,4)}</td>
  <td>${fmt(r.s2_ms,0)}</td><td>${fmt(r.s1_ms,0)}</td><td>${r.deadline?'⚠':''}</td></tr>`
"""
EXPLORER_ROWS_JS = """
  r => `<tr><td><a href="explorer/block_${r.block}.html#${name}">${r.block}</a></td><td><a href="https://etherscan.io/tx/${r.tx}" target="_blank" rel="noopener" title="${r.tx}">${r.tx.slice(0,10)}…</a></td><td title="${esc(r.venue_full)}">${esc(r.venue)}</td>
  <td title="${esc(r.pair_full)}">${esc(r.pair)}</td><td>${fmt(r.in_eth,4)}</td><td>${r.status}</td><td>${r.s1_status}</td>
  <td class="${bpsCls(r.s2_bps)}">${fmt(r.s2_bps)}</td>
  <td class="${bpsCls(r.s1_bps)}">${fmt(r.s1_bps)}</td>
  <td>${fmt(r.batcher_eth,4)}</td></tr>`
"""
TOKEN_ROWS_JS = """
  r => `<tr><td>${r.symbol} <span class="badge" title="${r.token}">${r.token.slice(0,8)}…</span></td>
  <td>${r.orders}</td><td>${fmt(r.eth,4)}</td></tr>`
"""


# Every variant's sections, as (anchor slug, heading, sidebar label). One source for both the
# headings and the table of contents, so the two can't drift apart.
SECTIONS = [
    ("chart", "Per-block improvement vs settled (bps of settled output value)", "Per-block Δbps"),
    ("hist", "Per-order S2 improvement distribution (cleared orders, bps vs settled)", "Δbps distribution"),
    ("status", "Order status mix per block", "Status mix"),
    ("blocks", "Blocks", "Blocks"),
    ("tokens", "Traded volume by sell token", "Token volume"),
    ("orders", "Order explorer (all S2 orders; S1 columns joined)", "Order explorer"),
]
HEADINGS = {slug: heading for slug, heading, _ in SECTIONS}


def h2(name: str, slug: str) -> str:
    return f'<h2 id="{name}-{slug}">{esc(HEADINGS[slug])}</h2>'


def toc(variants: list) -> str:
    items = ['<a class="toc-top" href="#top">Overview</a>']
    for name, _ in variants:
        items.append(f'<a class="toc-v" href="#v-{name}">{esc(name)}</a>')
        items += [f'<a class="toc-s" href="#{name}-{slug}">{esc(label)}</a>'
                  for slug, _, label in SECTIONS]
    items.append('<a class="toc-v" href="#notes">Accounting &amp; caveats</a>')
    return f'<nav class="toc">{"".join(items)}</nav>'


def variant_section(name: str, agg: dict) -> str:
    """One variant's full report section; element ids are suffixed with the variant name."""
    s2d = agg["s2_delta_bps"]
    s1d = agg["s1_delta_bps"]
    st = agg["statuses"]
    blocks = agg["per_block"]

    def tile(value, label, detail="", definition=""):
        # `value` and `detail` are markup (signed_bps spans, explorer links); both are built
        # here from the run's own numbers, never from decoded token or venue strings.
        return (
            f'<div class="tile"><div class="v">{value}</div>'
            f'<div class="l">{esc(label)}</div><div class="d">{detail}</div>'
            f'<div class="def">{esc(definition)}</div></div>'
        )

    def block_link(block: int) -> str:
        return f'<a href="explorer/block_{block}.html#{name}">{block}</a>'

    def pct(x):
        return f"{x*100:.1f}%"

    def signed_bps(x):
        cls = "pos" if x >= 0 else "neg"
        return f'<span class="{cls}">{x:+.2f} bps</span>'

    tiles = [
        tile(
            len(blocks),
            "blocks processed",
            definition="Blocks whose settled trades completed both APEX runs.",
        ),
        tile(
            agg["orders_total"],
            "orders total",
            f'= {agg["in_universe"]} in-universe + {st.get("out_of_universe",0)} out-of-universe',
            "One order per decoded, non-sandwiched settled swap; the total INCLUDES the out-of-universe ones. Out-of-universe = a trade token has no derived price; those never enter APEX and count at S0.",
        ),
        tile(
            signed_bps(s2d),
            "S2 vs S0 (batch vs settled)",
            f'{agg["s2_delta_eth"]:+.4f} ETH on {agg["s2_settled_eth"]:.2f} ETH',
            "(Σ effective output − Σ settled output) ÷ Σ settled output, ETH-valued at block prices, over all orders. Unfilled/out-of-universe orders count at S0; partial fills count at the full clearing-price execution (batcher-topped-up).",
        ),
        tile(
            signed_bps(s1d),
            "S1 vs S0 (per-order control)",
            f'{agg["s1_delta_eth"]:+.4f} ETH',
            "Same formula as S2 vs S0, but each order solved alone against the same pool snapshot.",
        ),
        tile(
            signed_bps(s2d - s1d),
            "S2 − S1 (batching effect)",
            "primary metric",
            "Difference of the two deltas above: what batching adds over the same solver run per order.",
        ),
        tile(
            signed_bps(agg["s3_delta_bps"]),
            "S3 vs S0 (per-order best of S2/S0)",
            f'{agg["s3_delta_eth"]:+.4f} ETH',
            "Unrealistic upper bound: per order, the better of the S2 permissive execution and the settled outcome — a cherry-pick that is NOT an executable batch (removing the losing orders would change the clearing).",
        ) if name == "permissive" else "",
        tile(
            pct(agg["win_rate"]),
            "per-order improve rate (S2)",
            f'{agg["wins"]} improved, median {agg["median_improvement_bps"]:+.2f} bps among executed',
            "S2 orders executed (cleared, or partial with batcher top-up) above their settled output ÷ all in-universe orders.",
        ),
        tile(
            pct(agg["blocks_won"] / max(1, len(blocks))),
            "per-block improve rate (S2)",
            f'{agg["blocks_won"]} improved, {agg["blocks_lost"]} worsened, {len(blocks)-agg["blocks_won"]-agg["blocks_lost"]} unchanged',
            "Blocks whose S2 effective output total (ETH-valued, uncleared orders at S0) exceeds their settled total ÷ blocks processed.",
        ),
        tile(
            f'{agg["batcher_gross_eth"]:.4f} ETH',
            "total batcher inventory used",
            f'received {agg["batcher_bought_eth"]:.4f} ETH',
            "Σ buy-token remainders the batcher supplies on partial fills (full clearing-price output minus APEX's fill), ETH-valued, over every block; in return it receives the unsold sell amounts.",
        ),
        tile(
            f'{agg["batcher_peak_eth"]:.4f} ETH',
            "max required batcher inventory",
            f'block {block_link(agg["batcher_peak_block"])}' if agg["batcher_peak_block"] else "",
            "The largest single block's top-up total: max over blocks of Σ that block's buy-token remainders, ETH-valued. Batches settle one at a time, so this is the inventory the batcher has to hold to run any block in the run.",
        ),
    ]

    s3_legend = (
        '<span><span class="k" style="background:var(--s3)"></span>S3 vs S0 (per-order best)</span>'
        if name == "permissive"
        else ""
    )
    s3_th = '<th data-k="s3_delta_bps">S3 Δbps</th>' if name == "permissive" else ""

    return f"""
<h1 class="variant-h" id="v-{name}">{esc(name)} variant</h1>
<div class="tiles">{''.join(tiles)}</div>

{h2(name, 'chart')}
<div class="chart-box">
  <div class="legend"><span><span class="k" style="background:var(--s1)"></span>S1 vs S0 (control)</span>
  <span><span class="k" style="background:var(--s2)"></span>S2 vs S0 (batch)</span>{s3_legend}</div>
  <div id="chart-blocks-{name}"></div>
</div>

{h2(name, 'hist')}
<div class="chart-box"><div id="chart-hist-{name}"></div>
<div class="note">Positive = the batch beat the settled output for that order.</div></div>

{h2(name, 'status')}
<div class="chart-box">
  <div class="legend">
    <span><span class="k" style="background:var(--s3)"></span>cleared</span>
    <span><span class="k" style="background:var(--s1)"></span>partial</span>
    <span><span class="k" style="background:var(--s2)"></span>unfilled</span>
    <span><span class="k" style="background:var(--bad)"></span>out of universe</span>
  </div>
  <div id="chart-status-{name}"></div>
</div>

{h2(name, 'blocks')}
<div class="filters">
  <label style="font-size:13px;color:var(--text-secondary)">
    <input type="checkbox" id="f-improved-{name}"> only blocks where batching improved execution (S2 Δbps &gt; 0)
  </label>
  <label style="font-size:13px;color:var(--text-secondary)">
    <input type="checkbox" id="f-amm-{name}"> only blocks where the clearing includes AMMs
  </label>
</div>
<div class="scroll"><table id="t-blocks-{name}"><thead><tr>
<th data-k="block">block</th><th data-k="orders">orders</th><th data-k="executed" title="orders APEX executed in S2: fully cleared + partially filled (topped up)">executed</th><th data-k="oou">o-o-u</th><th data-k="sandwiched">sandw.</th>
<th data-k="pools">pools (v2/v3/wrap)</th><th data-k="s1_delta_bps">S1 Δbps</th><th data-k="s2_delta_bps">S2 Δbps</th>{s3_th}
<th data-k="s2_surplus_eth" title="the block's S2 surplus over the settled outcome: Σ (effective output − settled output), ETH-valued at the block's prices">S2 surplus ETH</th>
<th data-k="settled_eth">settled ETH</th>
<th data-k="batcher_eth" title="ETH-valued sum of this block's batcher top-ups: the inventory the batcher has to hold to settle it">batcher ETH</th>
<th data-k="s2_ms">S2 ms</th><th data-k="s1_ms">S1 ms</th><th data-k="deadline">deadline</th>
</tr></thead><tbody></tbody></table></div>

{h2(name, 'tokens')}
<div class="filters"><select id="vol-block-{name}"><option value="">all blocks</option></select></div>
<div class="scroll"><table id="t-tokens-{name}"><thead><tr>
<th data-k="symbol">sell token</th><th data-k="orders">orders</th><th data-k="eth">volume (ETH)</th>
</tr></thead><tbody></tbody></table></div>

{h2(name, 'orders')}
<div class="filters">
  <select id="f-status-{name}"><option value="">all statuses</option><option>cleared</option><option>partial</option><option>unfilled</option><option>out_of_universe</option></select>
  <input type="text" id="f-block-{name}" placeholder="block…">
  <input type="text" id="f-search-{name}" placeholder="filter pair / venue / tx…">
</div>
<div class="scroll"><table id="t-orders-{name}"><thead><tr>
<th data-k="block">block</th><th data-k="tx">tx</th><th data-k="venue">venue</th><th data-k="pair">pair</th>
<th data-k="in_eth">in (ETH)</th><th data-k="status">S2 status</th><th data-k="s1_status">S1 status</th>
<th data-k="s2_bps" title="batch execution output vs settled on-chain output, in bps">S2 vs S0 Δbps</th>
<th data-k="s1_bps" title="single-order solve output vs settled on-chain output, in bps">S1 vs S0 Δbps</th>
<th data-k="batcher_eth">batcher ETH</th>
</tr></thead><tbody></tbody></table></div>
"""


def render(variants: list, out: Path) -> None:
    sections = []
    data = {}
    for name, agg in variants:
        sections.append(variant_section(name, agg))
        data[name] = {
            "per_block": agg["per_block"],
            "improvements": agg["improvements_bps"],
            "explorer": agg["explorer"],
            "token_volumes": agg["token_volumes"],
        }
    data_json = json.dumps(data, default=str)
    first = variants[0][1]["per_block"] if variants else []
    span = f"{first[0]['block']}–{first[-1]['block']}" if first else "—"

    page = f"""<!doctype html>
<meta charset="utf-8">
<title>APEX Batching Validation</title>
<style>{CSS}</style>
<body class="viz-root">
<div id="tooltip"></div>
{toc(variants)}
<h1 id="top">APEX batching validation — proof of concept</h1>
<div class="sub">Blocks {span} ·
S0 = settled on-chain, S1 = APEX per order (control), S2 = APEX whole-block batch (treatment).
Unfilled and out-of-universe orders count at S0; a partial fill executes fully at the clearing
price, with the batcher supplying the buy-token remainder as the missing liquidity source.
Variants: <b>permissive</b> (limit ≈ 0, every order may fill), <b>anchored</b> (limit = actual
settled price, APEX must beat reality to fill), and <b>user_limit</b> (limit = the user's signed
minimum buy amount recovered from calldata; anchored fallback where unrecoverable).</div>

{''.join(sections)}

<div class="foot" id="notes">
<b>Accounting & caveats.</b>
<ul>
<li>Gas is out of scope: every comparison is gross. Sandwiched trades are excluded before batch construction.</li>
<li>An order APEX did not clear (out-of-universe / unfilled) counts at its settled outcome S0; per-order Δbps is defined for executed orders (cleared, or partial with top-up).</li>
<li>Partial fills: the user executes their full size at the clearing price (originals are fill-or-kill, so no partial user execution exists). The batcher completes the fill as a liquidity source: it supplies the buy-token remainder ("Batcher ETH") and receives the unsold sell amount. Old (schema-1) records instead counted partials at S0.</li>
<li>S1 lets every order see untouched pools (liquidity is double-spent across orders), so S2 − S1 understates the batching benefit.</li>
<li>ETH valuations use each block's derived (top-of-block) prices; tokens without a price value at 0 and are excluded from bps denominators.</li>
<li>S3 (permissive section only) is an unrealistic optimistic bound: per order it takes the better of the S2 permissive execution and the settled outcome. Cherry-picking per order breaks batch consistency — removing the losing orders would change the clearing — so S3 is a ceiling, not an executable scenario.</li>
<li>Top-of-block state is optimistic for both S1 and S2. Solver deadlines can truncate solves (⚠ column).</li>
</ul>
</div>

<script>
const DATA = {data_json};
{JS}
for (const [name, d] of Object.entries(DATA)) {{
  const improvementSeries = [
    {{name:'S1 vs S0', color:'var(--s1)', values: d.per_block.map(b=>b.s1_delta_bps)}},
    {{name:'S2 vs S0', color:'var(--s2)', values: d.per_block.map(b=>b.s2_delta_bps)}},
  ];
  if (name === 'permissive') {{
    improvementSeries.push(
      {{name:'S3 vs S0', color:'var(--s3)', values: d.per_block.map(b=>b.s3_delta_bps)}});
  }}
  barChart($('#chart-blocks-'+name), d.per_block.map(b=>String(b.block)), improvementSeries);
  histogram($('#chart-hist-'+name), d.improvements, 'var(--s1)');
  barChart($('#chart-status-'+name), d.per_block.map(b=>String(b.block)), [
    {{name:'cleared', color:'var(--s3)', values: d.per_block.map(b=>b.statuses.cleared||0)}},
    {{name:'partial', color:'var(--s1)', values: d.per_block.map(b=>b.statuses.partial||0)}},
    {{name:'unfilled', color:'var(--s2)', values: d.per_block.map(b=>b.statuses.unfilled||0)}},
    {{name:'out of universe', color:'var(--bad)', values: d.per_block.map(b=>b.statuses.out_of_universe||0)}},
  ], {{stacked:true}});

  const fImproved = $('#f-improved-'+name), fAmm = $('#f-amm-'+name);
  const drawBlocks = sortable($('#t-blocks-'+name), () => d.per_block.filter(b =>
    (!fImproved.checked || b.s2_delta_bps > 0) &&
    (!fAmm.checked || b.amm_legs > 0)
  ), {BLOCK_ROWS_JS}, 'block');
  fImproved.addEventListener('change', drawBlocks);
  fAmm.addEventListener('change', drawBlocks);

  const volSel = $('#vol-block-'+name);
  d.per_block.forEach(b => {{ const o=document.createElement('option'); o.textContent=b.block; volSel.appendChild(o); }});
  const drawTokens = sortable($('#t-tokens-'+name), () => {{
    const blk = volSel.value;
    if (!blk) return d.token_volumes;
    return d.token_volumes
      .map(t => ({{...t, eth: t.by_block[blk] || 0}}))
      .filter(t => t.eth > 0);
  }}, {TOKEN_ROWS_JS}, 'eth');
  volSel.addEventListener('change', drawTokens);

  const fStatus = $('#f-status-'+name), fSearch = $('#f-search-'+name), fBlock = $('#f-block-'+name);
  const drawOrders = sortable($('#t-orders-'+name), () => d.explorer.filter(r =>
    (!fStatus.value || r.status === fStatus.value) &&
    (!fBlock.value || String(r.block).startsWith(fBlock.value.trim())) &&
    (!fSearch.value || (r.pair_full + r.venue_full + r.tx).toLowerCase().includes(fSearch.value.toLowerCase()))
  ), {EXPLORER_ROWS_JS}, 'in_eth');
  fStatus.addEventListener('change', drawOrders);
  fBlock.addEventListener('input', drawOrders);
  fSearch.addEventListener('input', drawOrders);
}}
initTocSpy();
</script>
"""
    out.write_text(page)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("dir", type=Path, help="the --apex-batching-dir of a monitor run")
    ap.add_argument("-o", "--out", type=Path, default=None)
    args = ap.parse_args()

    orders = load_jsonl(args.dir / "apex-orders.jsonl")
    blocks = load_jsonl(args.dir / "apex-blocks.jsonl")
    if not orders or not blocks:
        raise SystemExit(f"no records found in {args.dir}")

    # Old records predate the variant field on block records.
    for b in blocks:
        b.setdefault("variant", "permissive")
    variants = []
    for v in ("permissive", "anchored", "user_limit"):
        ov = [r for r in orders if r.get("variant", "permissive") == v]
        bv = [b for b in blocks if b["variant"] == v]
        if ov and bv:
            variants.append((v, aggregate(ov, bv)))

    out = args.out or (args.dir / "report.html")
    render(variants, out)

    for name, agg in variants:
        print(f"[{name}] blocks: {len(agg['blocks'])}  orders: {agg['orders_total']}")
        s3_note = f"   S3 vs S0: {agg['s3_delta_bps']:+.2f} bps" if name == "permissive" else ""
        print(f"[{name}] S1 vs S0: {agg['s1_delta_bps']:+.2f} bps   S2 vs S0: {agg['s2_delta_bps']:+.2f} bps   S2-S1: {agg['s2_delta_bps']-agg['s1_delta_bps']:+.2f} bps{s3_note}")
        print(f"[{name}] cleared: {agg['statuses'].get('cleared',0)}/{agg['orders_total']}  order improve rate: {agg['win_rate']*100:.1f}%  block improve rate: {agg['blocks_won']}/{len(agg['per_block'])}")
    print(f"report: {out}")


if __name__ == "__main__":
    main()
