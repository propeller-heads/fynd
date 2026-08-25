#!/usr/bin/env python3
"""Generate per-block "Turbine explorer"-style pages for the APEX batching experiment.

Reads a run's `apex-orders.jsonl`, `apex-blocks.jsonl`, and `inputs/apex_input_<N>.json`
dumps, and writes one `explorer/block_<N>.html` per block into the data dir — the target
of the block-number links in the main report. The pages share two scripts written
alongside them, `token_flow.js` and `vis-network.min.js` (see copy_assets).

Each page presents the block's APEX batch the way the Turbine settlement explorer
(propellerswap-frontend `/explore`) presents a settlement: a dark carbon canvas with
frosted summary/detail cards, an orders list per limit-price variant, the batch's AMM
legs, the batcher's top-ups, and the same vis-network Token Flow graph. The palette,
type, and card treatment mirror that app (carbon #1D2021, cloud text, cloud-100 blocks
with 1px-gap dividers, aquamarine/folly accents, the explorer's per-protocol colors) so
the two read the same.

Usage: apex_block_explorer.py <data-dir> [...]
"""

import html
import json
import os
import shutil
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
  /* Wide enough that the orders table fits beside the summary column without
     scrolling (its natural width tops out around 1100px). */
  padding: 24px; max-width: 1560px; margin-inline: auto;
  font-variant-numeric: tabular-nums;
}
a { color: #F5F5F5; text-decoration: none; }
a:hover { color: #00FFBB; }
.top { display: flex; align-items: baseline; gap: 14px; margin-bottom: 18px; flex-wrap: wrap; }
.top h1 { font-size: 20px; font-weight: 600; letter-spacing: -0.01em; }
.top .back { color: rgba(245,245,245,0.64); font-size: 13px; }
.top .ext { color: rgba(245,245,245,0.64); font-size: 13px; }
/* minmax(0, …) so a wide table scrolls inside .scroll instead of stretching the
   column past the page and leaving the layout lopsided. */
.grid { display: grid; grid-template-columns: 340px minmax(0, 1fr); gap: 16px; align-items: start; }
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
.ctl { float: right; font-size: 12px; font-weight: 400; color: rgba(245,245,245,0.64); cursor: pointer; }
.flowgraph { position: relative; height: 460px; }
/* Strip vis-network's default tooltip chrome so only the styled element shows. */
div.vis-tooltip {
  background: transparent !important; border: none !important; border-radius: 0 !important;
  padding: 0 !important; box-shadow: none !important; color: inherit !important;
  font-family: inherit !important;
}
div.vis-network:focus, div.vis-network canvas:focus { outline: none !important; }
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


def venue_cell(venue: str) -> str:
    """An unattributed venue is the entry point's raw address; shorten it so one unknown
    venue can't widen the whole orders table."""
    if venue.startswith("0x") and len(venue) >= 40:
        return (f'<a class="mono" href="https://etherscan.io/address/{esc(venue)}"'
                f' target="_blank" rel="noopener" title="{esc(venue)}">{short(venue)}</a>')
    return esc(venue)


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
            f'<td class="l">{venue_cell(rec["venue"])}</td>'
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


def token_info(address: str, meta: dict, symbols: dict) -> tuple[str, int | None]:
    address = address.lower()
    symbol, decimals = meta["tokens"].get(address, (None, None))
    return display_symbol(symbols.get(address) or symbol or address), decimals


def flow_graph(s2_orders: list[dict], pool_volumes: list[dict], meta: dict,
               symbols: dict) -> dict:
    """The block's token flows in the frontend's TokenGraph shape (analyzer.types.ts).

    An edge points from the token its hub sells to the token it buys, the convention
    token_flow.js renders: a user sells `sell_token` and buys `buy_token`, while a pool
    sells what it pays out (the clearing's `sell_token`) and buys what the batch feeds
    into it (`buy_token`) — so a pool arrow runs opposite the user flow it services."""
    nodes: dict[str, dict] = {}
    edges = []

    def node(address: str, symbol: str, decimals: int | None) -> str:
        address = address.lower()
        entry = nodes.setdefault(
            address,
            {"id": address, "address": address, "symbol": symbol, "decimals": decimals},
        )
        if entry["decimals"] is None:
            entry["decimals"] = decimals
        return address

    for rec in s2_orders:
        if rec["status"] not in ("cleared", "partial"):
            continue
        src = node(rec["sell_token"], display_symbol(rec["sell_symbol"]), rec["sell_decimals"])
        dst = node(rec["buy_token"], display_symbol(rec["buy_symbol"]), rec["buy_decimals"])
        edges.append({
            "src": src,
            "dst": dst,
            "hub": rec["tx_hash"],
            "kind": "user",
            "protocol": "user",
            "src_amount": rec["amount_in"],
            "dst_amount": str(user_out_raw(rec)),
            "note": f'{rec["venue"]} · {fmt_eth(rec["amount_in_eth"])} ETH in'
                    + (" · incl. batcher top-up" if rec["status"] == "partial" else ""),
        })
    for vol in pool_volumes:
        proto = proto_of(vol["address"], meta)
        sell_symbol, sell_decimals = token_info(vol["sell_token"], meta, symbols)
        buy_symbol, buy_decimals = token_info(vol["buy_token"], meta, symbols)
        edges.append({
            "src": node(vol["sell_token"], sell_symbol, sell_decimals),
            "dst": node(vol["buy_token"], buy_symbol, buy_decimals),
            "hub": vol["address"],
            "kind": "pool",
            "protocol": proto,
            "src_amount": vol["sold"],
            "dst_amount": vol["bought"],
            "note": f'paid out {fmt_eth(vol.get("sold_eth", 0.0))} ETH · '
                    f'took in {fmt_eth(vol.get("bought_eth", 0.0))} ETH',
        })
    return {"nodes": list(nodes.values()), "edges": edges}


def flow_graph_card(name: str, graph: dict) -> str:
    """The Token Flow card: a canvas token_flow.js draws into when the tab is shown."""
    if not graph["edges"]:
        body = '<div class="note">No executed flows in this batch.</div>'
    else:
        body = (
            f'<div class="flowgraph" id="flow-{name}"></div>'
            '<div class="note">An arrow points from the token its hub sells to the token it '
            'buys: users (folly) sell into the batch, pools (colored by protocol) sell what '
            'they pay out — so a pool arrow runs opposite the user flow it services. Drag, '
            'zoom and hover for amounts.</div>'
        )
    return (
        '<div class="card section"><div class="heading">Token flow'
        f'<label class="ctl"><input type="checkbox" data-flow-amounts="{name}"> amounts</label>'
        f'</div>{body}</div>'
    )



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
    graph = flow_graph(s2_orders, pool_volumes, meta, symbols)
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
      {flow_graph_card(name, graph)}
    </div>
  </div>
</div>
<script>FLOW_DATA[{json.dumps(name)}] = {json.dumps(graph)};</script>"""


def render_block(block: int, orders: list[dict], block_recs: dict, meta: dict,
                 vis_src: str) -> str:
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
            f' onclick="location.hash = \'{name}\'; show(\'{name}\')">{esc(name)}</button>'
        )
    tab_html = f'<div class="tabs">{"".join(tabs)}</div>' if len(variants) > 1 else ""

    return f"""<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>APEX batch {block}</title>
<style>{CSS}</style>
<script src="{vis_src}"></script>
<script src="token_flow.js"></script>
<script>const FLOW_DATA = {{}}, FLOWS = {{}};</script>
<body>
<div class="top">
  <a class="back" href="../report.html">← report</a>
  <h1>APEX batch · block {block:,}</h1>
  <a class="ext" href="https://etherscan.io/block/{block}" target="_blank" rel="noopener">etherscan ↗</a>
</div>
{tab_html}
{"".join(sections)}
<script>
function show(name) {{
  document.querySelectorAll('.variant').forEach(el => el.style.display = 'none');
  document.getElementById('variant-' + name).style.display = '';
  document.querySelectorAll('.tabs button').forEach(b => b.classList.toggle('active', b.dataset.v === name));
  initFlow(name);
}}
// vis-network measures its container to lay the graph out, so a variant's graph is
// only built once its tab is on screen.
function initFlow(name) {{
  const el = document.getElementById('flow-' + name);
  if (!el || FLOWS[name]) return;
  const flow = renderTokenFlow(el, FLOW_DATA[name]);
  FLOWS[name] = flow;
  const amounts = document.querySelector('[data-flow-amounts="' + name + '"]');
  if (amounts) amounts.addEventListener('change', () => flow.setAmounts(amounts.checked));
}}
// The report links here with the variant it was clicked in as the fragment.
function showRequested() {{
  const wanted = decodeURIComponent(location.hash.slice(1));
  const first = document.querySelector('.variant').id.slice('variant-'.length);
  show(document.getElementById('variant-' + wanted) ? wanted : first);
}}
window.addEventListener('hashchange', showRequested);
showRequested();
</script>
"""


def copy_assets(out_dir: Path) -> str:
    """Put the graph's scripts next to the pages, and return the vis-network script src.

    vis-network itself isn't vendored here: it is taken from a propellerswap-frontend
    checkout so the pages stay pinned to the same version the frontend renders with,
    and falls back to the CDN when that checkout isn't around."""
    shutil.copyfile(Path(__file__).with_name("token_flow.js"), out_dir / "token_flow.js")

    bundle = "vis-network/standalone/umd/vis-network.min.js"
    candidates = [Path(p) for p in (os.environ.get("VIS_NETWORK_JS"),) if p]
    candidates += [
        repo / "propellerswap-frontend" / "node_modules" / bundle
        for repo in (Path(__file__).resolve().parents[3].parent, Path.home() / "repos")
    ]
    for candidate in candidates:
        if candidate.is_file():
            shutil.copyfile(candidate, out_dir / "vis-network.min.js")
            return "vis-network.min.js"
    print(f"{out_dir}: no local vis-network bundle found, pages will load it from the CDN")
    return f"https://unpkg.com/{bundle.replace('/', '@10.1.0/', 1)}"


def generate(data_dir: Path) -> int:
    orders = load_jsonl(data_dir / "apex-orders.jsonl")
    blocks = load_jsonl(data_dir / "apex-blocks.jsonl")
    if not orders:
        print(f"{data_dir}: no order records, skipped")
        return 0
    out_dir = data_dir / "explorer"
    out_dir.mkdir(exist_ok=True)
    vis_src = copy_assets(out_dir)

    orders_by_block = defaultdict(list)
    for rec in orders:
        orders_by_block[rec["block"]].append(rec)
    recs_by_block = defaultdict(dict)
    for rec in blocks:
        recs_by_block[rec["block"]][rec.get("variant", "permissive")] = rec

    all_blocks = set(orders_by_block) | set(recs_by_block)
    for block in all_blocks:
        meta = load_input_dump(data_dir, block)
        page = render_block(
            block, orders_by_block.get(block, []), recs_by_block.get(block, {}), meta, vis_src
        )
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
