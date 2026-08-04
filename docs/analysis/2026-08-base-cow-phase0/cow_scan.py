#!/usr/bin/env python3
"""Base CoW potential scan (methodology v2) over hindsight comparison JSONL.

Input: comparisons-YYYY-MM-DD.jsonl files (hindsight monitor output, staging/base).
Each record is already a per-tx netted intent (hindsight decoder guarantees one-in-one-out,
venue fees backed out), so traps 1-3 of the methodology are handled upstream; this script
adds canonicalization, quarantine, USD estimation, solvable-universe restriction, and the
matched-volume / surplus decomposition per tumbling window.

Outputs cow_scan_results.json next to the data.
"""

import json
import math
import os
import sys
from collections import defaultdict
from statistics import median

DATA_DIR = os.path.dirname(os.path.abspath(__file__)) + "/base-comparisons"
OUT_PATH = os.path.dirname(os.path.abspath(__file__)) + "/cow_scan_results.json"

WETH = "0x4200000000000000000000000000000000000006"
NATIVE = "0x0000000000000000000000000000000000000000"
WINDOWS = [1, 5, 15, 30, 150]
# Quarantine: implied per-token price deviating by more than this factor from the day median
# (Tamara: arb decodes with nonsense amounts on Base right now), or a single intent valued
# above the USD cap.
PRICE_DEV_FACTOR = 5.0
USD_CAP = 10_000_000.0
BLUECHIPS = {
    WETH,
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",  # USDC
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",  # USDbC
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",  # DAI
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",  # cbBTC
}
# Pairs verified as bot self-trading (sender check 2026-08-03: 200 top-match txs resolved to 10
# addresses). Their matches are excluded from all matching/surplus metrics and counted apart.
WASH_PAIRS = {
    ("0x3c5cd672b204ba0fc48e93b98c0922920a87912d", "0x3d66e6fe9a3cf698db5af3d70830b299c9235151"),
}


def canon(token: str) -> str:
    return WETH if token == NATIVE else token


def load_day(path: str):
    """Parse one day file into intent tuples + per-token raw-unit price samples +
    routable token set. Price = settled_value_usd / raw_amount, decimals-free."""
    intents = []
    price_samples = defaultdict(list)
    routable = set()
    sandwiched = 0
    for line in open(path):
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        tin, tout = canon(d["token_in"]), canon(d["token_out"])
        if tin == tout:
            continue
        try:
            ain, aout = int(d["amount_in"]), int(d["settled_amount_out"])
        except (TypeError, ValueError):
            continue
        if ain <= 0 or aout <= 0:
            continue
        verdict = d["top"]["verdict"]
        usd = d["top"].get("settled_value_usd")
        if d.get("sandwich") is not None:
            sandwiched += 1
        fynd_out = None
        if verdict in ("win", "loss"):
            routable.add(tin)
            routable.add(tout)
            raw = d["top"].get("fynd_amount_out")
            if raw is not None:
                try:
                    fynd_out = int(raw)
                except (TypeError, ValueError):
                    fynd_out = None
        if usd is not None and usd > 0:
            price_samples[tout].append(usd / aout)
            price_samples[tin].append(usd / ain)
        intents.append(
            (d["block"], tin, tout, ain, aout, usd, verdict, d["venue"], d["solver"], fynd_out,
             d["settled_tx"])
        )
    day_price = {t: median(s) for t, s in price_samples.items()}
    return intents, day_price, routable, sandwiched


def classify(intents, day_price, routable, counters):
    """Split intents into headline (solvable universe), p2p slice (unsolvable tokens),
    and quarantine. Attach a USD estimate to every kept intent."""
    headline, p2p = [], []
    for block, tin, tout, ain, aout, usd, verdict, venue, solver, fynd_out, tx in intents:
        pin, pout = day_price.get(tin), day_price.get(tout)
        usd_est = usd
        if usd_est is None:
            if pin is not None:
                usd_est = pin * ain
            elif pout is not None:
                usd_est = pout * aout
        if usd_est is None:
            counters["unpriced"] += 1
            continue
        # Quarantine on implied-price deviation vs the day median (either side) or USD cap.
        bad = usd_est > USD_CAP
        if not bad and pin is not None:
            dev = (usd_est / ain) / pin if pin > 0 else 1.0
            bad = dev > PRICE_DEV_FACTOR or dev < 1.0 / PRICE_DEV_FACTOR
        if not bad and pout is not None:
            dev = (usd_est / aout) / pout if pout > 0 else 1.0
            bad = dev > PRICE_DEV_FACTOR or dev < 1.0 / PRICE_DEV_FACTOR
        if bad:
            counters["quarantined"] += 1
            counters["quarantined_usd"] += min(usd_est, USD_CAP)
            continue
        rec = (block, tin, tout, ain, aout, usd_est, venue, solver, fynd_out, tx)
        if tin in routable and tout in routable:
            headline.append(rec)
        else:
            p2p.append(rec)
    return headline, p2p


def pair_key(a: str, b: str):
    return (a, b) if a < b else (b, a)


def pair_category(a: str, b: str) -> str:
    """blue2 = both bluechip, blue1 = one side bluechip (the typical retail pair),
    tail2 = neither."""
    n = (a in BLUECHIPS) + (b in BLUECHIPS)
    return ("tail2", "blue1", "blue2")[n]


def scan_windows(intents, day_price, window: int, stats, pair_volume=None, collect_matches=None):
    """All matched-volume and surplus metrics for one tumbling window size.

    pair_volume: weekly USD per unordered pair, for volume-bucket attribution.
    collect_matches: list to append (surplus_usd, txs_ab, txs_ba) per match — used by the
    wash-check dump on the w=1 p2p run.
    """
    by_window = defaultdict(list)
    for rec in intents:
        by_window[rec[0] // window].append(rec)

    for _, rows in by_window.items():
        w_usd = sum(r[5] for r in rows)
        stats["w_usd"] += w_usd
        stats["windows"] += 1

        # Directed pair flows: raw qty both sides + usd, plus the fynd-covered subset
        # (indices 3-5: amount_in, executed out, fynd-quoted out — only trades fynd solved).
        flows = {}
        flow_txs = defaultdict(list) if collect_matches is not None else None
        token_buy = defaultdict(float)
        token_sell = defaultdict(float)
        for _, tin, tout, ain, aout, usd_est, _, _, fynd_out, tx in rows:
            f = flows.setdefault((tin, tout), [0, 0, 0.0, 0, 0, 0])
            f[0] += ain
            f[1] += aout
            f[2] += usd_est
            if fynd_out is not None and fynd_out > 0:
                f[3] += ain
                f[4] += aout
                f[5] += fynd_out
            if flow_txs is not None:
                flow_txs[(tin, tout)].append(tx)
            token_sell[tin] += usd_est
            token_buy[tout] += usd_est

        # Multilateral cap: only net per-token demand must touch AMMs.
        abs_net = sum(abs(token_buy[t] - token_sell[t]) for t in set(token_buy) | set(token_sell))
        multilateral = max(0.0, w_usd - 0.5 * abs_net)
        stats["multilateral_usd"] += multilateral

        # Pairwise: matched qty in A units = min(A sold A->B, A bought via B->A).
        pairwise = 0.0
        surplus = 0.0
        matched_any = False
        residual = {}
        for (a, b), (qa_in, qb_out, usd_ab, fa_in, fb_out_x, fb_out_q) in flows.items():
            if a > b or (b, a) not in flows:
                if (b, a) not in flows:
                    residual[(a, b)] = [usd_ab, qa_in, qb_out]
                continue
            qb_in, qa_out, usd_ba, gb_in, ga_out_x, ga_out_q = flows[(b, a)]
            matched_a = min(qa_in, qa_out)
            if matched_a <= 0:
                continue
            if pair_key(a, b) in WASH_PAIRS:
                price_a = day_price.get(a)
                wash_usd = 2 * matched_a * price_a if price_a else 2 * usd_ab * (matched_a / qa_in)
                stats["wash_excluded_usd"] += wash_usd
                continue
            matched_any = True
            price_a = day_price.get(a)
            m_usd = 2 * matched_a * price_a if price_a else 2 * usd_ab * (matched_a / qa_in)
            pairwise += m_usd
            # Executed vw prices in B-per-A raw units; crossing gain floored at zero.
            p_sell = qb_out / qa_in
            p_buy = qb_in / qa_out
            gap = p_buy - p_sell
            stats["gap_bps_samples"].append(1e4 * gap / p_buy if p_buy > 0 else 0.0)
            cat = pair_category(a, b)
            stats["cat_matched"][cat] += m_usd
            if pair_volume is not None:
                stats["bucket_matched"][volume_bucket(pair_volume.get(pair_key(a, b), 0.0))] += m_usd
            if gap > 0:
                s_b = matched_a * gap
                price_b = day_price.get(b)
                s_usd = s_b * price_b if price_b else usd_ab / qb_out * s_b
                surplus += s_usd
                key = pair_key(a, b)
                stats["pair_surplus"][key] += s_usd
                stats["pair_matched"][key] += m_usd
                stats["cat_surplus"][cat] += s_usd
                if pair_volume is not None:
                    stats["bucket_surplus"][volume_bucket(pair_volume.get(key, 0.0))] += s_usd
                if collect_matches is not None:
                    collect_matches.append(
                        (round(s_usd, 2), flow_txs[(a, b)][:20], flow_txs[(b, a)][:20])
                    )
            else:
                stats["neg_gap_matches"] += 1
            # Fynd-quoted baseline, on the fynd-covered subset of both sides: what crossing
            # adds ON TOP of Fynd's own quotes. Same-subset executed surplus alongside, so the
            # two baselines compare on identical flow.
            if fa_in > 0 and ga_out_q > 0:
                matched_af = min(fa_in, ga_out_q)
                price_a = day_price.get(a)
                m_usd_f = 2 * matched_af * price_a if price_a else 2 * usd_ab * (matched_af / qa_in)
                stats["fynd_matched_usd"] += m_usd_f
                p_sell_f = fb_out_q / fa_in
                p_buy_f = gb_in / ga_out_q
                gap_f = p_buy_f - p_sell_f
                price_b = day_price.get(b)

                def to_usd_b(qty_b):
                    return qty_b * price_b if price_b else usd_ab / qb_out * qty_b

                if gap_f > 0:
                    stats["fynd_surplus_usd"] += to_usd_b(matched_af * gap_f)
                p_sell_x = fb_out_x / fa_in
                p_buy_x = gb_in / ga_out_x if ga_out_x > 0 else 0.0
                gap_x = p_buy_x - p_sell_x
                if gap_x > 0 and ga_out_x > 0:
                    stats["exec_surplus_fynd_subset_usd"] += to_usd_b(min(fa_in, ga_out_x) * gap_x)
            # Residual after pairwise for ring stage.
            res_ab = usd_ab * (1 - matched_a / qa_in)
            res_ba = usd_ba * (1 - matched_a / qa_out)
            if res_ab > 1:
                residual[(a, b)] = [res_ab, qa_in - matched_a, qb_out * (1 - matched_a / qa_in)]
            if res_ba > 1:
                residual[(b, a)] = [res_ba, qb_in * (1 - matched_a / qa_out), qa_out - matched_a]
        stats["pairwise_usd"] += pairwise
        stats["surplus_usd"] += surplus
        if matched_any:
            stats["windows_with_match"] += 1
        if surplus > 0:
            stats["window_surplus"].append(surplus)

        # Ring stage: greedy 3-cycle cancellation on the residual directed graph.
        if len(residual) >= 3:
            ring_usd, ring_surplus = cancel_rings(residual)
            stats["ring_usd"] += ring_usd
            stats["ring_surplus_usd"] += ring_surplus


def cancel_rings(residual):
    """Greedy 3-cycle cancellation. Edge value = residual USD; leg price = vw raw
    out/in, whose product around a cycle is dimensionless (decimals cancel)."""
    ring_usd = 0.0
    ring_surplus = 0.0
    out_edges = defaultdict(set)
    for (a, b) in residual:
        out_edges[a].add(b)
    changed = True
    while changed:
        changed = False
        for (a, b) in list(residual.keys()):
            if residual.get((a, b), [0])[0] <= 1:
                continue
            for c in list(out_edges.get(b, ())):
                if c == a or (c, a) not in residual:
                    continue
                e1, e2, e3 = residual.get((a, b)), residual.get((b, c)), residual.get((c, a))
                if not e1 or not e2 or not e3:
                    continue
                cyc = min(e1[0], e2[0], e3[0])
                if cyc <= 1:
                    continue
                p = 1.0
                for e in (e1, e2, e3):
                    p *= (e[2] / e[1]) if e[1] > 0 else 0.0
                ring_usd += 3 * cyc
                if 0 < p < 1:
                    ring_surplus += cyc * (1 - p)
                for edge, e in (((a, b), e1), ((b, c), e2), ((c, a), e3)):
                    scale = 1 - cyc / e[0]
                    e[0] -= cyc
                    e[1] *= scale
                    e[2] *= scale
                    if e[0] <= 1:
                        residual.pop(edge, None)
                        out_edges[edge[0]].discard(edge[1])
                changed = True
                break
    return ring_usd, ring_surplus


VOLUME_BUCKETS = ["<$10k", "$10k-100k", "$100k-1M", "$1M-10M", ">$10M"]


def volume_bucket(weekly_pair_usd: float) -> str:
    if weekly_pair_usd >= 1e7:
        return ">$10M"
    if weekly_pair_usd >= 1e6:
        return "$1M-10M"
    if weekly_pair_usd >= 1e5:
        return "$100k-1M"
    if weekly_pair_usd >= 1e4:
        return "$10k-100k"
    return "<$10k"


def new_stats():
    return {
        "w_usd": 0.0, "pairwise_usd": 0.0, "multilateral_usd": 0.0,
        "surplus_usd": 0.0, "ring_usd": 0.0, "ring_surplus_usd": 0.0,
        "windows": 0, "windows_with_match": 0, "neg_gap_matches": 0,
        "fynd_matched_usd": 0.0, "fynd_surplus_usd": 0.0, "exec_surplus_fynd_subset_usd": 0.0,
        "wash_excluded_usd": 0.0,
        "gap_bps_samples": [], "window_surplus": [],
        "pair_surplus": defaultdict(float), "pair_matched": defaultdict(float),
        "cat_matched": defaultdict(float), "cat_surplus": defaultdict(float),
        "bucket_matched": defaultdict(float), "bucket_surplus": defaultdict(float),
    }


def finalize(stats, top_n=20):
    gaps = sorted(stats["gap_bps_samples"])
    ws = sorted(stats["window_surplus"])
    route_mediated = max(0.0, stats["multilateral_usd"] - stats["pairwise_usd"] - stats["ring_usd"])
    med_gap = gaps[len(gaps) // 2] if gaps else 0.0
    return {
        "intent_usd": round(stats["w_usd"]),
        "pairwise_usd": round(stats["pairwise_usd"]),
        "pairwise_pct": round(100 * stats["pairwise_usd"] / stats["w_usd"], 3) if stats["w_usd"] else 0,
        "ring_usd": round(stats["ring_usd"]),
        "multilateral_usd": round(stats["multilateral_usd"]),
        "multilateral_pct": round(100 * stats["multilateral_usd"] / stats["w_usd"], 3) if stats["w_usd"] else 0,
        "route_mediated_usd": round(route_mediated),
        "pairwise_surplus_usd": round(stats["surplus_usd"], 2),
        "ring_surplus_usd": round(stats["ring_surplus_usd"], 2),
        "route_mediated_surplus_proxy_usd": round(route_mediated * med_gap / 1e4, 2),
        "median_gap_bps": round(med_gap, 2),
        "wash_excluded_usd": round(stats["wash_excluded_usd"]),
        "fynd_matched_usd": round(stats["fynd_matched_usd"]),
        "fynd_baseline_surplus_usd": round(stats["fynd_surplus_usd"], 2),
        "exec_surplus_on_fynd_subset_usd": round(stats["exec_surplus_fynd_subset_usd"], 2),
        "windows": stats["windows"],
        "windows_with_match_pct": round(100 * stats["windows_with_match"] / stats["windows"], 2) if stats["windows"] else 0,
        "neg_gap_matches": stats["neg_gap_matches"],
        "window_surplus_p50": round(ws[len(ws) // 2], 2) if ws else 0,
        "window_surplus_p90": round(ws[int(len(ws) * 0.9)], 2) if ws else 0,
        "window_surplus_max": round(ws[-1], 2) if ws else 0,
        "top_pairs_by_surplus": sorted(
            ((round(v, 2), round(stats["pair_matched"][k]), k[0], k[1]) for k, v in stats["pair_surplus"].items()),
            reverse=True,
        )[:top_n],
        "by_category": {
            c: {"matched_usd": round(stats["cat_matched"][c]),
                "surplus_usd": round(stats["cat_surplus"][c], 2)}
            for c in ("blue2", "blue1", "tail2")
        },
        "by_volume_bucket": {
            b: {"matched_usd": round(stats["bucket_matched"][b]),
                "surplus_usd": round(stats["bucket_surplus"][b], 2)}
            for b in VOLUME_BUCKETS
        },
    }


def main():
    files = sorted(f for f in os.listdir(DATA_DIR) if f.endswith(".jsonl"))
    results = {"days": {}, "counters": defaultdict(float), "windows": {}}
    all_headline, all_p2p = {}, {}
    day_prices = {}
    for fname in files:
        day = fname.replace("comparisons-", "").replace(".jsonl", "")
        intents, day_price, routable, sandwiched = load_day(os.path.join(DATA_DIR, fname))
        counters = defaultdict(float)
        headline, p2p = classify(intents, day_price, routable, counters)
        counters["sandwiched"] = sandwiched
        counters["records"] = len(intents)
        counters["headline_intents"] = len(headline)
        counters["p2p_slice_intents"] = len(p2p)
        results["days"][day] = {k: round(v) for k, v in counters.items()}
        for k, v in counters.items():
            results["counters"][k] += v
        all_headline[day] = headline
        all_p2p[day] = p2p
        day_prices[day] = day_price
        print(f"{day}: records={len(intents)} headline={len(headline)} p2p={len(p2p)} "
              f"quarantined={counters['quarantined']:.0f} unpriced={counters['unpriced']:.0f}", flush=True)

    # Weekly USD per unordered pair (both slices), for volume-bucket attribution: "are CoWs
    # more common in high-volume pairs?"
    pair_volume = defaultdict(float)
    for group in (all_headline, all_p2p):
        for day, recs in group.items():
            for rec in recs:
                pair_volume[pair_key(rec[1], rec[2])] += rec[5]

    p2p_matches_w1 = []
    for w in WINDOWS:
        head_stats, p2p_stats = new_stats(), new_stats()
        for day in all_headline:
            scan_windows(all_headline[day], day_prices[day], w, head_stats, pair_volume)
            scan_windows(all_p2p[day], day_prices[day], w, p2p_stats, pair_volume,
                         collect_matches=p2p_matches_w1 if w == 1 else None)
        results["windows"][str(w)] = {
            "headline": finalize(head_stats),
            "p2p_slice": finalize(p2p_stats, top_n=10),
        }
        h = results["windows"][str(w)]["headline"]
        print(f"window={w:>3} blocks: W=${h['intent_usd']:,} pairwise={h['pairwise_pct']}% "
              f"multi={h['multilateral_pct']}% surplus=${h['pairwise_surplus_usd']:,.0f}", flush=True)

    results["counters"] = {k: round(v) for k, v in results["counters"].items()}
    with open(OUT_PATH, "w") as f:
        json.dump(results, f, indent=1, default=str)
    print(f"wrote {OUT_PATH}", flush=True)

    # Wash-check input: every single-block p2p match with the txs on both sides, sorted by
    # surplus so a sender lookup can be spent on the value-weighted head of the distribution.
    p2p_matches_w1.sort(reverse=True)
    dump = os.path.dirname(os.path.abspath(__file__)) + "/p2p_matches_w1.json"
    with open(dump, "w") as f:
        json.dump(p2p_matches_w1, f)
    print(f"wrote {dump} ({len(p2p_matches_w1)} matches, "
          f"top-500 surplus share: {sum(m[0] for m in p2p_matches_w1[:500]) / max(1e-9, sum(m[0] for m in p2p_matches_w1)):.1%})",
          flush=True)


if __name__ == "__main__":
    main()
