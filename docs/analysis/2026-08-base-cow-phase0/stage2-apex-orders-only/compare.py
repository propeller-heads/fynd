"""Merge stage2 (real APEX, zero AMMs) results with the analytic N-N ceiling into README.md.

Inputs: stage2_results.json (the stage2 binary's output) and ../cow_scan_results.json
(the analytic scan). Realized/ceiling ratios are only meaningful because both sides share
the same universe filtering, windowing, and USD conventions — see the stage2 binary's docs.
"""

import json
from pathlib import Path

here = Path(__file__).parent
cells = json.loads((here / "stage2_results.json").read_text())
analytic = json.loads((here / ".." / "cow_scan_results.json").read_text())["windows"]

lines = [
    "# Stage 2 — real APEX on Base trades, zero AMMs",
    "",
    "Markus's ladder, point 2: the analytic scan (cow_scan.py) gives the N-N matching",
    "*ceiling*; this run puts the same headline universe through the actual apex-solver",
    "(`tools/apex-batch/src/bin/stage2.rs`) with an empty pool set, so fills are pure",
    "order-vs-order clearing under APEX's real mechanism — uniform clearing price per pair,",
    "cluster formation, limit enforcement (synthetic floors: settled × (1 − bps)),",
    "10 s offline deadline per component.",
    "",
    "Realized vs ceiling, matched volume (share of the same intent USD denominator):",
    "",
    "| window | limit | APEX matched | APEX % | pairwise ceiling % | multilateral ceiling % |"
    " realized/pairwise | realized/multilateral | net surplus | analytic pairwise surplus |",
    "|---|---|---|---|---|---|---|---|---|---|",
]
for cell in cells:
    w = str(cell["window_blocks"])
    a = analytic[w]["headline"]
    rp = cell["apex_matched_pct"] / a["pairwise_pct"] if a["pairwise_pct"] else 0.0
    rm = cell["apex_matched_pct"] / a["multilateral_pct"] if a["multilateral_pct"] else 0.0
    net = cell["apex_surplus_usd"] - cell["counters"]["negative_gap_usd"]
    lines.append(
        f"| {w} | {cell['limit_bps']} bps | ${cell['apex_matched_usd']:,.0f} "
        f"| {cell['apex_matched_pct']:.3f}% | {a['pairwise_pct']}% | {a['multilateral_pct']}% "
        f"| {rp:.2f}× | {rm:.2f}× | ${net:,.0f} | ${a['pairwise_surplus_usd']:,.0f} |"
    )
lines += [
    "",
    "Net surplus = positive per-order gaps vs settled MINUS the negative gaps a uniform",
    "clearing price imposes on the other side within its limit slack. The positive-only sum",
    "grows mechanically with the allowed slack (50→200 bps) while the net is nearly invariant —",
    "the net column is the mechanism's real value creation vs what settled on-chain.",
]

lines += ["", "Counters (100 bps cells):", ""]
for cell in cells:
    if cell["limit_bps"] != 100:
        continue
    c = cell["counters"]
    lines.append(
        f"- w={cell['window_blocks']}: orders_in={c['orders_in']:,} filled={c['filled']:,} "
        f"partial={c['partially_filled']:,} unfilled_at_limit={c['unfilled_at_limit']:,} "
        f"cluster_cut={c['cluster_cut']:,} errored={c['component_errored']:,} "
        f"({c['component_errors']}) panics={c['solver_panics']} "
        f"singles_skipped={c['singles_skipped']:,} wash_excl={c['wash_orders_excluded']:,} "
        f"unpriced={c['token_unpriced']:,} underflow={c['price_underflow']:,} "
        f"neg_gaps={c['negative_fill_gaps']:,} solves={c['components_solved']:,} "
        f"wall={cell['wall_ms']}ms"
    )

lines += [
    "",
    "Conventions: matched volume counts each filled order at its own USD value (both sides of",
    "a cross), mirroring the analytic 2× convention; surplus is vs the settled baseline,",
    "positive per-order gaps only, valued at day-median USD prices (negatives counted apart).",
    "The intent USD denominator includes the wash pair (as in the analytic scan); its orders",
    "never enter APEX. Decimals-free scheme: zero pools ⇒ tokens declared 18-dec, raw amounts,",
    "per-raw-unit prices — exact, see the binary's module docs.",
    "",
]
(here / "README.md").write_text("\n".join(lines))
print(f"wrote {here / 'README.md'}")
