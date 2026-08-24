#!/usr/bin/env python3
"""Generate per-block "Turbine explorer"-style pages for the APEX batching experiment.

Reads a run's `apex-orders.jsonl`, `apex-blocks.jsonl`, and `inputs/apex_input_<N>.json`
dumps, and writes one self-contained `explorer/block_<N>.html` per block into the data
dir — the target of the block-number links in the main report.

Each page presents the block's APEX batch the way the Turbine settlement explorer
(propellerswap-frontend `/explore`) presents a settlement: a dark carbon canvas with
frosted summary/detail cards, an orders list per limit-price variant, the batch's AMM
legs, and the batcher's top-ups. The palette, type, and card treatment mirror that app
(carbon #1D2021, cloud text, cloud-100 blocks with 1px-gap dividers, aquamarine/folly
accents, the explorer's per-protocol colors) so the two read the same.

Usage: apex_block_explorer.py <data-dir> [...]
"""

import html
import json
import sys
from collections import defaultdict
from pathlib import Path

# The frontend's explorer palette (src/components/Explore/explore.constants.ts and
# src/theme/foundations/colors.ts), kept verbatim so the pages read as Turbine.
PROTO_COLORS = {
    "uniswap_v2": "#ff85b5",
    "sushiswap_v2": "#ff85b5",
    "pancakeswap_v2": "#ff85b5",
    "uniswap_v3": "#b692ff",
    "pancakeswap_v3": "#f0c75a",
    "uniswap_v4": "#00cfff",
    "ekubo_v3": "#d65dff",
    "ekubo_v2": "#d65dff",
    "vm:curve": "#00ffbb",
    "curve": "#00ffbb",
    "vm:balancer_v2": "#ffcc00",
    "balancer_v2": "#ffcc00",
    "fluid_v1": "#1e6bff",
    "fluid": "#1e6bff",
}
UNKNOWN_COLOR = "#6e7681"

STATUS_META = {
    "cleared": ("Cleared", "#00FFBB"),
    "partial": ("Partial + top-up", "#00cfff"),
    "unfilled": ("Unfilled", "#FFCC00"),
    "out_of_universe": ("Out of universe", "#FF3366"),
}

CSS = """
* { box-sizing: border-box; margin: 0; }
body {
  background: #1D2021; color: #F5F5F5; min-height: 100vh;
  font: 14px/1.45 "Geist Variable", -apple-system, "Segoe UI", Roboto, sans-serif;
  padding: 24px; max-width: 1180px; margin-inline: auto;
  font-variant-numeric: tabular-nums;
}
a { color: #F5F5F5; text-decoration: none; }
a:hover { color: #00FFBB; }
.top { display: flex; align-items: baseline; gap: 14px; margin-bottom: 18px; flex-wrap: wrap; }
.top h1 { font-size: 20px; font-weight: 600; letter-spacing: -0.01em; }
.top .back { color: rgba(245,245,245,0.64); font-size: 13px; }
.top .ext { color: rgba(245,245,245,0.64); font-size: 13px; }
.grid { display: grid; grid-template-columns: 340px 1fr; gap: 16px; align-items: start; }
@media (max-width: 900px) { .grid { grid-template-columns: 1fr; } }
.rows { display: flex; flex-direction: column; gap: 1px; border-radius: 12px; overflow: hidden; }
.block { background: rgba(245,245,245,0.06); backdrop-filter: blur(20px); padding: 10px 12px; }
.heading { font-size: 14px; font-weight: 500; color: #F5F5F5; margin-bottom: 4px; }
.statgrid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 12px 8px; padding-top: 8px; }
.statgrid .v { font-size: 20px; font-weight: 600; line-height: 1.1; }
.statgrid .l { font-size: 12px; color: rgba(245,245,245,0.64); margin-top: 2px; }
.detail { display: flex; justify-content: space-between; gap: 12px; }
.detail .k { color: rgba(245,245,245,0.64); }
.detail .v { text-align: right; }
.tabs { display: inline-flex; gap: 1px; border-radius: 12px; overflow: hidden; margin-bottom: 12px; }
.tabs button {
  background: rgba(245,245,245,0.06); color: rgba(245,245,245,0.64); border: 0;
  padding: 8px 16px; font: inherit; cursor: pointer;
}
.tabs button.active { background: rgba(245,245,245,0.20); color: #F5F5F5; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { text-align: right; padding: 8px 10px; white-space: nowrap; }
th:first-child, td:first-child, th.l, td.l { text-align: left; }
th { color: rgba(245,245,245,0.64); font-weight: 500; font-size: 12px; }
tbody tr { border-top: 1px solid #1D2021; background: rgba(245,245,245,0.06); }
.card { border-radius: 12px; overflow: hidden; background: rgba(245,245,245,0.03); }
.card .heading { padding: 12px 12px 8px; }
.scroll { overflow-x: auto; }
.pill {
  display: inline-block; padding: 2px 9px; border-radius: 9999px; font-size: 11.5px;
  font-weight: 500; color: #1D2021;
}
.pos { color: #00FFBB; } .neg { color: #FF3366; } .muted { color: rgba(245,245,245,0.40); }
.proto { display: inline-block; width: 9px; height: 9px; border-radius: 3px; margin-right: 6px; vertical-align: -1px; }
.pairarrow { color: rgba(245,245,245,0.40); padding: 0 4px; }
.section { margin-top: 16px; }
.mono { letter-spacing: 0.01em; }
.note { color: rgba(245,245,245,0.40); font-size: 12px; padding: 10px 12px; }
"""


def esc(x):
    return html.escape(str(x))


def short(addr_or_hash: str, n: int = 8) -> str:
    return addr_or_hash[: n + 2] + "…" if len(addr_or_hash) > n + 4 else addr_or_hash


def display_symbol(symbol: str) -> str:
    """Registry-unknown tokens fall back to their full hex address as symbol; shorten those."""
    if symbol.startswith("0x") and len(symbol) >= 40:
        return short(symbol)
    return symbol


def fmt_amount(raw: str, decimals: int) -> str:
    """Human token amount from a raw integer string: enough significant digits to be
    readable, commas on the integer part."""
    value = int(raw)
    if value == 0:
        return "0"
    whole, frac = divmod(value, 10**decimals)
    if whole >= 1000:
        return f"{whole:,}"
    text = f"{value / 10**decimals:.6g}"
    return text


def fmt_eth(x: float) -> str:
    if x == 0:
        return "0"
    return f"{x:,.4f}" if x >= 0.001 else f"{x:.2e}"


def load_jsonl(path: Path) -> list[dict]:
    records = []
    if not path.exists():
        return records
    with path.open() as fh:
        for line in fh:
            line = line.strip()
            if line:
                records.append(json.loads(line))
    return records


def user_out_raw(rec: dict) -> int:
    """What the user receives under the batch scenario, raw buy-token units."""
    if rec["status"] == "cleared":
        return int(rec["apex_bought"])
    if rec["status"] == "partial":
        return int(rec["apex_bought"]) + int(rec["batcher_sold"])
    return 0


def delta_bps(rec: dict) -> float | None:
    settled = int(rec["settled_amount_out"])
    if rec["status"] not in ("cleared", "partial") or settled <= 0:
        return None
    return (user_out_raw(rec) - settled) / settled * 10_000


def load_input_dump(data_dir: Path, block: int) -> dict:
    """Pool → protocol / token metadata from the block's ApexInputData dump, tolerant of a
    missing file. Wrapped multi-token pools get synthetic addresses whose last byte varies,
    so pool lookups also try a 19-byte prefix match."""
    path = data_dir / "inputs" / f"apex_input_{block}.json"
    meta = {"pool_proto": {}, "pool_proto_prefix": {}, "tokens": {}, "prices": {}}
    if not path.exists():
        return meta
    try:
        dump = json.loads(path.read_text())
    except json.JSONDecodeError:
        return meta
    for token in dump.get("tokens", []):
        meta["tokens"][token["address"].lower()] = (token["symbol"], token["decimals"])
    for address, price in dump.get("initialPrices", {}).items():
        meta["prices"][address.lower()] = int(price)
    for pool in dump.get("pools", []):
        proto = pool.get("type", "unknown")
        if proto == "custom":
            proto = pool.get("data", {}).get("protocol", "unknown")
        pool_id = pool.get("id", "").lower().removeprefix("0x")
        # APEX addresses a pool by the LAST 20 bytes of its id (Address::from truncates from
        # the left), and multi-token pair views mutate the last byte — so index by the last
        # 20 bytes and by the 19-byte prefix of those.
        tail = pool_id[-40:].rjust(40, "0")
        meta["pool_proto"]["0x" + tail] = proto
        meta["pool_proto_prefix"]["0x" + tail[:38]] = proto
    return meta


def proto_of(address: str, meta: dict) -> str:
    address = address.lower()
    if address in meta["pool_proto"]:
        return meta["pool_proto"][address]
    return meta["pool_proto_prefix"].get(address[:40], "unknown")



def status_pill(status: str) -> str:
    label, color = STATUS_META.get(status, (status, UNKNOWN_COLOR))
    return f'<span class="pill" style="background:{color}">{esc(label)}</span>'


def bps_cell(bps: float | None) -> str:
    if bps is None:
        return '<td class="muted">—</td>'
    cls = "pos" if bps >= 0 else "neg"
    return f'<td class="{cls}">{bps:+.2f}</td>'


def orders_table(orders: list[dict], s1_by_id: dict) -> str:
    if not orders:
        return '<div class="note">No decoded solver trades in this block.</div>'
    rows = []
    for rec in sorted(orders, key=lambda r: -r["settled_amount_out_eth"]):
        pair = (
            f'{esc(display_symbol(rec["sell_symbol"]))}<span class="pairarrow">→</span>'
            f'{esc(display_symbol(rec["buy_symbol"]))}'
        )
        settled = fmt_amount(rec["settled_amount_out"], rec["buy_decimals"])
        batch_out = user_out_raw(rec)
        batch = (
            fmt_amount(str(batch_out), rec["buy_decimals"])
            if rec["status"] in ("cleared", "partial")
            else "—"
        )
        s1 = s1_by_id.get(rec["order_id"])
        s1_status = s1["status"] if s1 else "—"
        topup = (
            f'{fmt_amount(rec["batcher_sold"], rec["buy_decimals"])} {esc(rec["buy_symbol"])}'
            if rec["status"] == "partial"
            else ""
        )
        rows.append(
            "<tr>"
            f'<td class="l"><a class="mono" href="https://etherscan.io/tx/{esc(rec["tx_hash"])}"'
            f' target="_blank" rel="noopener" title="{esc(rec["tx_hash"])}">{short(rec["tx_hash"])}</a></td>'
            f'<td class="l">{esc(rec["venue"])}</td>'
            f'<td class="l">{pair}</td>'
            f'<td class="l">{status_pill(rec["status"])}</td>'
            f'<td>{fmt_amount(rec["amount_in"], rec["sell_decimals"])}</td>'
            f"<td>{settled}</td>"
            f"<td>{batch}</td>"
            f"{bps_cell(delta_bps(rec))}"
            f'<td class="l muted">{esc(s1_status)}</td>'
            f'<td class="l">{topup}</td>'
            "</tr>"
        )
    return (
        '<div class="scroll"><table><thead><tr>'
        '<th class="l">tx</th><th class="l">venue</th><th class="l">pair</th><th class="l">status</th>'
        "<th>amount in</th><th>settled out (S0)</th><th>batch out (S2)</th>"
        '<th title="batch execution vs settled output">Δ bps</th>'
        '<th class="l">S1 status</th><th class="l">batcher top-up</th>'
        "</tr></thead><tbody>" + "".join(rows) + "</tbody></table></div>"
    )


def pools_table(volumes: list[dict], meta: dict, symbols: dict) -> str:
    if not volumes:
        return '<div class="note">The batch routed nothing through AMM pools.</div>'
    rows = []
    for vol in sorted(volumes, key=lambda v: -(v.get("sold_eth", 0.0))):
        proto = proto_of(vol["address"], meta)
        color = PROTO_COLORS.get(proto, UNKNOWN_COLOR)
        sell = display_symbol(symbols.get(vol["sell_token"].lower(), vol["sell_token"]))
        buy = display_symbol(symbols.get(vol["buy_token"].lower(), vol["buy_token"]))
        rows.append(
            "<tr>"
            f'<td class="l"><span class="proto" style="background:{color}"></span>{esc(proto)}</td>'
            f'<td class="l"><a class="mono" href="https://etherscan.io/address/{esc(vol["address"])}"'
            f' target="_blank" rel="noopener" title="{esc(vol["address"])}">{short(vol["address"])}</a></td>'
            f'<td class="l">{esc(sell)}<span class="pairarrow">→</span>{esc(buy)}</td>'
            f'<td>{fmt_eth(vol.get("sold_eth", 0.0))}</td>'
            f'<td>{fmt_eth(vol.get("bought_eth", 0.0))}</td>'
            "</tr>"
        )
    return (
        '<div class="scroll"><table><thead><tr>'
        '<th class="l">protocol</th><th class="l">pool</th><th class="l">pays out → takes in</th>'
        "<th>paid out (ETH)</th><th>took in (ETH)</th>"
        "</tr></thead><tbody>" + "".join(rows) + "</tbody></table></div>"
    )


def prices_rows(meta: dict) -> str:
    if not meta["prices"]:
        return ""
    rows = []
    for address, price in sorted(meta["prices"].items(), key=lambda kv: -kv[1]):
        symbol, _ = meta["tokens"].get(address, (short(address), 18))
        rows.append(
            f'<div class="block detail"><span class="k">{esc(symbol)}</span>'
            f'<span class="v">{price:,}</span></div>'
        )
    return (
        '<div class="rows section"><div class="block"><div class="heading">Initial prices'
        '</div><div class="note" style="padding:0">APEX scale: value per 18-dec unit, '
        "cluster minimum anchored at 1e8.</div></div>" + "".join(rows) + "</div>"
    )


import math


def flow_edges(s2_orders: list[dict], pool_volumes: list[dict], meta: dict,
               symbols: dict) -> list[dict]:
    """The block's token flows as directed edges, sold token -> bought token.

    User orders (cleared + partial incl. the batcher top-up) flow from the token the user
    sold to the token they received; AMM legs flow from the token the batch sold into the
    pool (the record's `bought` side) to the token the pool paid out (`sold`)."""
    edges = []
    for rec in s2_orders:
        if rec["status"] not in ("cleared", "partial"):
            continue
        out_raw = user_out_raw(rec)
        edges.append({
            "src": display_symbol(rec["sell_symbol"]),
            "dst": display_symbol(rec["buy_symbol"]),
            "kind": "order",
            "label": rec["venue"],
            "eth": rec["amount_in_eth"],
            "color": "#00FFBB",
            "tip": (f'{rec["venue"]} order {short(rec["tx_hash"])}: '
                    f'{fmt_amount(rec["amount_in"], rec["sell_decimals"])} '
                    f'{display_symbol(rec["sell_symbol"])} -> '
                    f'{fmt_amount(str(out_raw), rec["buy_decimals"])} '
                    f'{display_symbol(rec["buy_symbol"])}'
                    + (" (incl. batcher top-up)" if rec["status"] == "partial" else "")),
        })
    for vol in pool_volumes:
        proto = proto_of(vol["address"], meta)
        src = display_symbol(symbols.get(vol["buy_token"].lower(), vol["buy_token"]))
        dst = display_symbol(symbols.get(vol["sell_token"].lower(), vol["sell_token"]))
        edges.append({
            "src": src,
            "dst": dst,
            "kind": "pool",
            "label": proto,
            "eth": vol.get("bought_eth", 0.0),
            "color": PROTO_COLORS.get(proto, UNKNOWN_COLOR),
            "tip": (f'{proto} pool {short(vol["address"])}: batch sold '
                    f'{fmt_eth(vol.get("bought_eth", 0.0))} ETH of {src}, received '
                    f'{fmt_eth(vol.get("sold_eth", 0.0))} ETH of {dst}'),
        })
    return edges


def _bezier_point(p0, c, p1, t):
    mt = 1 - t
    return (mt * mt * p0[0] + 2 * mt * t * c[0] + t * t * p1[0],
            mt * mt * p0[1] + 2 * mt * t * c[1] + t * t * p1[1])


def flow_graph_svg(edges: list[dict]) -> str:
    """A static token-flow graph: tokens on a circle, one labeled curved arrow per flow —
    the frontend Token Flow tab's "tokens" layout, rendered as dependency-free inline SVG."""
    if not edges:
        return '<div class="note">No executed flows in this batch.</div>'
    tokens = []
    for e in edges:
        for t in (e["src"], e["dst"]):
            if t not in tokens:
                tokens.append(t)
    volume = {t: 0.0 for t in tokens}
    for e in edges:
        volume[e["src"]] += e["eth"]
        volume[e["dst"]] += e["eth"]
    vmax = max(volume.values()) or 1.0

    n = len(tokens)
    ring_r = max(115.0, 13.0 * n)
    cx, cy = 390.0, ring_r + 65.0
    width, height = 780.0, 2 * (ring_r + 65.0)
    pos = {}
    for i, t in enumerate(tokens):
        a = 2 * math.pi * i / n - math.pi / 2
        pos[t] = (cx + ring_r * math.cos(a), cy + ring_r * math.sin(a))
    radius = {t: 13.0 + 9.0 * math.sqrt(volume[t] / vmax) for t in tokens}

    # Curvature: edges sharing an unordered token pair fan out so none overlap; the two
    # directions bow to opposite sides.
    pair_ix = {}
    parts = []
    for e in sorted(edges, key=lambda x: -x["eth"]):
        p0, p1 = pos[e["src"]], pos[e["dst"]]
        key = tuple(sorted((e["src"], e["dst"])))
        k = pair_ix.get(key, 0)
        pair_ix[key] = k + 1
        dx, dy = p1[0] - p0[0], p1[1] - p0[1]
        dist = math.hypot(dx, dy) or 1.0
        # Perpendicular unit vector; flip for the reverse direction so A->B and B->A split.
        px, py = -dy / dist, dx / dist
        if (e["src"], e["dst"]) != key:
            px, py = -px, -py
        bow = 30.0 + 22.0 * k
        c = ((p0[0] + p1[0]) / 2 + px * bow, (p0[1] + p1[1]) / 2 + py * bow)
        # Trim endpoints to the node boundaries (approximate along chord-to-control dirs).
        def trim(point, toward, r):
            tx, ty = toward[0] - point[0], toward[1] - point[1]
            d = math.hypot(tx, ty) or 1.0
            return (point[0] + tx / d * r, point[1] + ty / d * r)
        a0 = trim(p0, c, radius[e["src"]] + 3)
        a1 = trim(p1, c, radius[e["dst"]] + 3)
        # Arrowhead at the target end, aligned with the curve tangent.
        tangx, tangy = a1[0] - c[0], a1[1] - c[1]
        td = math.hypot(tangx, tangy) or 1.0
        ux, uy = tangx / td, tangy / td
        wx, wy = -uy, ux
        tip = a1
        base = (tip[0] - ux * 9, tip[1] - uy * 9)
        head = (f'M{tip[0]:.1f},{tip[1]:.1f} '
                f'L{base[0] + wx * 4:.1f},{base[1] + wy * 4:.1f} '
                f'L{base[0] - wx * 4:.1f},{base[1] - wy * 4:.1f} Z')
        dash = ' stroke-dasharray="7 5"' if e["kind"] == "order" else ""
        stroke_w = 2.6 if e["kind"] == "order" else 2.0
        mid = _bezier_point(a0, c, a1, 0.5)
        label = f'{e["label"]} · {fmt_eth(e["eth"])}Ξ'
        parts.append(
            f'<g><title>{esc(e["tip"])}</title>'
            f'<path d="M{a0[0]:.1f},{a0[1]:.1f} Q{c[0]:.1f},{c[1]:.1f} {a1[0]:.1f},{a1[1]:.1f}"'
            f' fill="none" stroke="{e["color"]}" stroke-width="{stroke_w}"{dash} opacity="0.85"/>'
            f'<path d="{head}" fill="{e["color"]}"/>'
            f'<text x="{mid[0]:.1f}" y="{mid[1] - 6:.1f}" text-anchor="middle"'
            f' font-size="10.5" fill="rgba(245,245,245,0.64)">{esc(label)}</text></g>'
        )
    for t in tokens:
        x, y = pos[t]
        r = radius[t]
        parts.append(
            f'<g><title>{esc(t)}: {fmt_eth(volume[t])} ETH total flow</title>'
            f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r:.1f}" fill="rgba(245,245,245,0.14)"'
            f' stroke="#1D2021" stroke-width="2"/>'
            f'<text x="{x:.1f}" y="{y + r + 14:.1f}" text-anchor="middle" font-size="11.5"'
            f' font-weight="500" fill="#F5F5F5">{esc(t)}</text></g>'
        )
    legend = ('<div class="note" style="padding:6px 12px 10px">dashed aquamarine = user order '
              '(sold → bought); solid = AMM leg colored by protocol (batch sold → received). '
              'Hover for amounts.</div>')
    return (f'<div style="overflow-x:auto"><svg viewBox="0 0 {width:.0f} {height:.0f}"'
            f' width="100%" style="min-width:640px">{"".join(parts)}</svg></div>' + legend)



def variant_section(name: str, s2_orders: list[dict], s1_by_id: dict, block_rec: dict | None,
                    meta: dict, symbols: dict) -> str:
    counts = defaultdict(int)
    for rec in s2_orders:
        counts[rec["status"]] += 1
    settled_eth = sum(r["settled_amount_out_eth"] for r in s2_orders)
    effective_eth = sum(
        r["apex_bought_eth"] + r["batcher_sold_eth"]
        if r["status"] == "partial"
        else (r["apex_bought_eth"] if r["status"] == "cleared" else r["settled_amount_out_eth"])
        for r in s2_orders
    )
    delta = effective_eth - settled_eth
    stats = [
        (len(s2_orders), "orders"),
        (counts["cleared"], "cleared"),
        (counts["partial"], "partial"),
        (counts["unfilled"], "unfilled"),
        (counts["out_of_universe"], "out of universe"),
    ]
    stat_html = "".join(
        f'<div><div class="v">{v}</div><div class="l">{esc(l)}</div></div>'
        for v, l in stats
        if v
    )
    details = [
        ("Settled total (S0)", f"{fmt_eth(settled_eth)} ETH"),
        (
            "Batch total (S2)",
            f'<span class="{"pos" if delta >= 0 else "neg"}">{fmt_eth(effective_eth)} ETH '
            f"({delta:+.4f})</span>",
        ),
    ]
    if block_rec:
        details += [
            ("S2 solve", f'{block_rec["s2_solve_ms"]:,} ms'
                         + (" · deadline ⚠" if block_rec["s2_deadline_fired"] else "")),
            ("S1 solves total", f'{block_rec["s1_solve_ms_total"]:,} ms'
                                + (f' · {block_rec["s1_deadline_fired"]} deadline ⚠'
                                   if block_rec["s1_deadline_fired"] else "")),
            ("Pools (v2/v3/wrapped)",
             f'{block_rec["pools_native_v2"]}/{block_rec["pools_native_v3"]}/{block_rec["pools_wrapped"]}'),
            ("Universe tokens", block_rec["universe_tokens"]),
            ("Sandwiched excluded", block_rec["sandwiched_excluded"]),
        ]
    detail_html = "".join(
        f'<div class="block detail"><span class="k">{esc(k)}</span><span class="v">{v}</span></div>'
        for k, v in details
    )
    pool_volumes = block_rec["s2_pool_volumes"] if block_rec else []
    return f"""
<div class="variant" id="variant-{name}">
  <div class="grid">
    <div>
      <div class="rows">
        <div class="block"><div class="heading">Summary — {esc(name)}</div>
          <div class="statgrid">{stat_html}</div>
        </div>
        {detail_html}
      </div>
      {prices_rows(meta)}
    </div>
    <div>
      <div class="card"><div class="heading">Orders</div>{orders_table(s2_orders, s1_by_id)}</div>
      <div class="card section"><div class="heading">AMM legs (S2 pool executions)</div>
      {pools_table(pool_volumes, meta, symbols)}</div>
      <div class="card section"><div class="heading">Token flow</div>
      {flow_graph_svg(flow_edges(s2_orders, pool_volumes, meta, symbols))}</div>
    </div>
  </div>
</div>"""


def render_block(block: int, orders: list[dict], block_recs: dict, meta: dict) -> str:
    order = {"permissive": 0, "anchored": 1, "user_limit": 2}
    variants = sorted(
        {r["variant"] for r in orders} | set(block_recs),
        key=lambda v: order.get(v, 9),
    )
    symbols = {}
    for rec in orders:
        symbols[rec["sell_token"].lower()] = rec["sell_symbol"]
        symbols[rec["buy_token"].lower()] = rec["buy_symbol"]
    for address, (symbol, _) in meta["tokens"].items():
        symbols.setdefault(address, symbol)

    sections = []
    tabs = []
    for i, name in enumerate(variants):
        s2 = [r for r in orders if r["variant"] == name and r["run"] == "s2"]
        s1_by_id = {r["order_id"]: r for r in orders if r["variant"] == name and r["run"] == "s1"}
        sections.append(variant_section(name, s2, s1_by_id, block_recs.get(name), meta, symbols))
        tabs.append(
            f'<button data-v="{name}" class="{"active" if i == 0 else ""}"'
            f' onclick="show(\'{name}\', this)">{esc(name)}</button>'
        )
    tab_html = f'<div class="tabs">{"".join(tabs)}</div>' if len(variants) > 1 else ""

    return f"""<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>APEX batch {block}</title>
<style>{CSS}</style>
<body>
<div class="top">
  <a class="back" href="../report.html">← report</a>
  <h1>APEX batch · block {block:,}</h1>
  <a class="ext" href="https://etherscan.io/block/{block}" target="_blank" rel="noopener">etherscan ↗</a>
</div>
{tab_html}
{"".join(sections)}
<script>
function show(name, btn) {{
  document.querySelectorAll('.variant').forEach(el => el.style.display = 'none');
  document.getElementById('variant-' + name).style.display = '';
  document.querySelectorAll('.tabs button').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
}}
document.querySelectorAll('.variant').forEach((el, i) => {{ if (i > 0) el.style.display = 'none'; }});
</script>
"""


def generate(data_dir: Path) -> int:
    orders = load_jsonl(data_dir / "apex-orders.jsonl")
    blocks = load_jsonl(data_dir / "apex-blocks.jsonl")
    if not orders:
        print(f"{data_dir}: no order records, skipped")
        return 0
    out_dir = data_dir / "explorer"
    out_dir.mkdir(exist_ok=True)

    orders_by_block = defaultdict(list)
    for rec in orders:
        orders_by_block[rec["block"]].append(rec)
    recs_by_block = defaultdict(dict)
    for rec in blocks:
        recs_by_block[rec["block"]][rec.get("variant", "permissive")] = rec

    all_blocks = set(orders_by_block) | set(recs_by_block)
    for block in all_blocks:
        meta = load_input_dump(data_dir, block)
        page = render_block(block, orders_by_block.get(block, []), recs_by_block.get(block, {}), meta)
        (out_dir / f"block_{block}.html").write_text(page)
    return len(all_blocks)


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    for directory in sys.argv[1:]:
        count = generate(Path(directory))
        print(f"{directory}: {count} block pages")


if __name__ == "__main__":
    main()
