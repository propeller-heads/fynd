#!/usr/bin/env python3
"""
Bellman-Ford A/B comparison benchmark.

Sends trades from a JSON requests file to a running fynd solver,
records per-trade results, and saves to a JSON output file.

Usage:
    python3 bf_comparison.py <requests_file> <output_file> [--url URL] [--limit N]
"""
import argparse
import json
import sys
import time
from statistics import mean, median

import requests as http


def check_health(url):
    try:
        r = http.get(f"{url}/v1/health", timeout=5)
        data = r.json()
        if data.get("healthy"):
            print(f"Solver healthy: {data['num_solver_pools']} pools, "
                  f"last update {data['last_update_ms']}ms ago")
            return True
    except Exception as e:
        print(f"Health check failed: {e}")
    return False


def run_benchmark(solver_url, requests_data, limit=None):
    results = []
    total = len(requests_data)
    if limit:
        total = min(total, limit)

    successes = 0
    failures = 0
    no_routes = 0
    solve_times = []

    start_all = time.time()

    for i in range(total):
        req = requests_data[i]
        token_in = req["orders"][0]["token_in"]
        token_out = req["orders"][0]["token_out"]
        amount_in = req["orders"][0]["amount"]

        try:
            start = time.time()
            resp = http.post(
                f"{solver_url}/v1/solve",
                json=req,
                timeout=30,
            )
            rtt_ms = (time.time() - start) * 1000

            if resp.status_code != 200:
                failures += 1
                results.append({
                    "index": i,
                    "token_in": token_in,
                    "token_out": token_out,
                    "amount_in": amount_in,
                    "status": "http_error",
                    "http_status": resp.status_code,
                    "rtt_ms": rtt_ms,
                })
                continue

            data = resp.json()
            order = data["orders"][0]
            status = order["status"]
            solve_time = data.get("solve_time_ms", 0)

            result = {
                "index": i,
                "token_in": token_in,
                "token_out": token_out,
                "amount_in": amount_in,
                "status": status,
                "amount_out": order.get("amount_out", "0"),
                "amount_out_net_gas": order.get("amount_out_net_gas", "0"),
                "gas_estimate": order.get("gas_estimate", "0"),
                "solve_time_ms": solve_time,
                "rtt_ms": rtt_ms,
            }

            if status == "success":
                successes += 1
                solve_times.append(solve_time)
                route = order.get("route")
                if route:
                    result["hops"] = len(route.get("swaps", []))
                    result["route"] = " -> ".join(
                        s["component_id"][:16] for s in route.get("swaps", [])
                    )
            else:
                no_routes += 1

            results.append(result)

        except Exception as e:
            failures += 1
            results.append({
                "index": i,
                "token_in": token_in,
                "token_out": token_out,
                "amount_in": amount_in,
                "status": "error",
                "error": str(e),
            })

        # Progress
        if (i + 1) % 100 == 0 or i + 1 == total:
            elapsed = time.time() - start_all
            rate = (i + 1) / elapsed if elapsed > 0 else 0
            print(
                f"  [{i+1}/{total}] "
                f"success={successes} no_route={no_routes} fail={failures} "
                f"rate={rate:.1f} req/s"
            )

    total_time = time.time() - start_all

    summary = {
        "total_trades": total,
        "successes": successes,
        "no_routes": no_routes,
        "failures": failures,
        "total_time_s": round(total_time, 2),
        "rate_rps": round(total / total_time, 2) if total_time > 0 else 0,
    }

    if solve_times:
        solve_times.sort()
        summary["solve_time_p50_ms"] = solve_times[len(solve_times) // 2]
        summary["solve_time_p95_ms"] = solve_times[int(len(solve_times) * 0.95)]
        summary["solve_time_mean_ms"] = round(mean(solve_times), 1)
        summary["solve_time_max_ms"] = max(solve_times)

    return summary, results


def main():
    parser = argparse.ArgumentParser(description="BF comparison benchmark")
    parser.add_argument("requests_file", help="JSON requests file")
    parser.add_argument("output_file", help="Output JSON file")
    parser.add_argument("--url", default="http://localhost:3000",
                        help="Solver URL")
    parser.add_argument("--limit", type=int, default=None,
                        help="Max trades to send")
    args = parser.parse_args()

    print(f"Loading requests from {args.requests_file}...")
    with open(args.requests_file) as f:
        requests_data = json.load(f)
    print(f"Loaded {len(requests_data)} requests")

    if not check_health(args.url):
        print("ERROR: Solver is not healthy")
        sys.exit(1)

    print(f"\nRunning benchmark against {args.url}...")
    summary, results = run_benchmark(args.url, requests_data, args.limit)

    output = {"summary": summary, "results": results}
    with open(args.output_file, "w") as f:
        json.dump(output, f, indent=2)

    print(f"\n=== Summary ===")
    for k, v in summary.items():
        print(f"  {k}: {v}")
    print(f"\nResults saved to {args.output_file}")


if __name__ == "__main__":
    main()
