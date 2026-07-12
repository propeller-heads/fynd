#!/usr/bin/env python3
"""Build a WETH-star collector universe from Tycho liquidity tiers.

Every configured pair is collected on every block. Size --count to what the
target machine can quote inside one block; the collector reports capacity
misses as explicit errors instead of sub-sampling.
"""

import argparse
import csv
import json
import os
import urllib.parse
import urllib.request
from collections import defaultdict
from pathlib import Path
from typing import Any

TYCHO_URL = "https://tycho-beta.propellerheads.xyz"
LLAMA_URL = "https://coins.llama.fi/prices/current/"
WETH = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
NATIVE = "0x0000000000000000000000000000000000000000"
PROTOCOLS = ("uniswap_v2", "uniswap_v3", "uniswap_v4", "pancakeswap_v3", "sushiswap_v2")
TVL_TIERS = (10_000.0, 3_000.0, 1_000.0, 300.0, 100.0, 30.0, 10.0, 3.0)
PAGE_SIZE = 100


def post_tycho(path: str, body: dict[str, Any], api_key: str) -> dict[str, Any]:
    """Post one authenticated Tycho RPC request."""
    request = urllib.request.Request(
        f"{TYCHO_URL}{path}",
        data=json.dumps(body).encode(),
        headers={
            "authorization": api_key,
            "content-type": "application/json",
            "accept": "text/json",
        },
    )
    with urllib.request.urlopen(request, timeout=60) as response:  # noqa: S310
        return json.load(response)


def paginated_components(
    protocol: str, tvl: float, api_key: str
) -> list[dict[str, Any]]:
    """Fetch every component for one protocol and TVL floor."""
    components = []
    page = 0
    while True:
        result = post_tycho(
            "/v1/protocol_components",
            {
                "chain": "ethereum",
                "protocol_system": protocol,
                "tvl_gt": tvl,
                "pagination": {"page": page, "page_size": PAGE_SIZE},
            },
            api_key,
        )
        batch = result["protocol_components"]
        components.extend(batch)
        if len(components) >= result["pagination"]["total"] or not batch:
            return components
        page += 1


def rank_tokens(api_key: str) -> tuple[list[str], dict[str, float]]:
    """Rank tokens by summed lower-bound TVL across Tycho component tiers."""
    scores: dict[str, float] = defaultdict(float)
    seen_components: set[str] = set()
    for tier in TVL_TIERS:
        for protocol in PROTOCOLS:
            for component in paginated_components(protocol, tier, api_key):
                component_key = f"{protocol}:{component['id']}"
                if component_key in seen_components:
                    continue
                seen_components.add(component_key)
                for address in component["tokens"]:
                    normalized = address.lower()
                    if normalized not in {WETH, NATIVE}:
                        scores[normalized] += tier
    ranked = sorted(scores, key=lambda address: (-scores[address], address))
    return ranked, dict(scores)


def fetch_metadata(addresses: list[str], api_key: str) -> dict[str, dict[str, Any]]:
    """Fetch standard, recently traded token metadata for candidate addresses."""
    metadata = {}
    for start in range(0, len(addresses), PAGE_SIZE):
        batch = addresses[start : start + PAGE_SIZE]
        result = post_tycho(
            "/v1/tokens",
            {
                "chain": "ethereum",
                "min_quality": 100,
                "traded_n_days_ago": 3,
                "token_addresses": batch,
                "pagination": {"page": 0, "page_size": PAGE_SIZE},
            },
            api_key,
        )
        for token in result["tokens"]:
            metadata[token["address"].lower()] = token
    return metadata


def fetch_prices(addresses: list[str]) -> dict[str, float]:
    """Fetch current USD prices in URL-safe batches from DefiLlama."""
    prices = {}
    for start in range(0, len(addresses), 50):
        batch = addresses[start : start + 50]
        coins = ",".join(f"ethereum:{address}" for address in batch)
        url = f"{LLAMA_URL}{urllib.parse.quote(coins, safe=':,')}"
        with urllib.request.urlopen(url, timeout=60) as response:  # noqa: S310
            result = json.load(response)
        for key, value in result.get("coins", {}).items():
            prices[key.split(":", 1)[1].lower()] = float(value["price"])
    return prices


def token_amount(token: dict[str, Any], price: float | None, target_usd: float) -> int:
    """Return base units close to the WETH-side target notional."""
    decimals = int(token["decimals"])
    if price is None or price <= 0:
        return 10**decimals
    return max(1, round(target_usd / price * 10**decimals))


def toml_token(token_id: str, token: dict[str, Any]) -> str:
    return "\n".join(
        [
            "[[tokens]]",
            f'id = "{token_id}"',
            f'address = "{token["address"].lower()}"',
            f"symbol = {json.dumps(token['symbol'], ensure_ascii=False)}",
            f"decimals = {token['decimals']}",
        ]
    )


def toml_pair(token_id: str, amount: int) -> str:
    return "\n".join(
        [
            "[[pairs]]",
            f'id = "weth-{token_id}"',
            'token_a = "weth"',
            f'token_b = "{token_id}"',
            'amounts_a = ["10000000000000000"]',
            f'amounts_b = ["{amount}"]',
        ]
    )


def config_header(count: int) -> str:
    protocols = ", ".join(json.dumps(protocol) for protocol in PROTOCOLS)
    return f'''run_name = "ethereum-weth-star-top-{count}"

[fynd]
tycho_url = "tycho-beta.propellerheads.xyz"
tycho_api_key_env = "TYCHO_API_KEY_BETA"
rpc_http_url_env = "RPC_URL"
rpc_ws_url_env = "RPC_WS_URL"
protocols = [{protocols}]
min_tvl = 3.0
algorithm = "bellman_ford"
num_workers = 16
task_queue_capacity = 4000
max_hops = 3
algorithm_timeout_ms = 750

[collection]
sender = "0x0000000000000000000000000000000000000001"
request_chunk_size = 64
state_wait_timeout_ms = 3000
quote_timeout_ms = 8000
collection_budget_ms = 10000
confirmation_depth = 12

[[tokens]]
id = "weth"
address = "{WETH}"
symbol = "WETH"
decimals = 18
'''


def write_outputs(
    output_dir: Path,
    selected: list[dict[str, Any]],
    scores: dict[str, float],
    prices: dict[str, float],
    weth_price: float,
) -> None:
    """Write the collector TOML and an auditable ranked universe CSV."""
    output_dir.mkdir(parents=True, exist_ok=True)
    target_usd = weth_price * 0.01
    token_sections = []
    pair_sections = []
    with (output_dir / "universe.csv").open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(
            [
                "rank",
                "address",
                "symbol",
                "decimals",
                "tvl_score",
                "usd_price",
                "amount",
            ]
        )
        for rank, token in enumerate(selected, start=1):
            address = token["address"].lower()
            token_id = f"t_{address[2:]}"
            amount = token_amount(token, prices.get(address), target_usd)
            writer.writerow(
                [
                    rank,
                    address,
                    token["symbol"],
                    token["decimals"],
                    scores[address],
                    prices.get(address),
                    amount,
                ]
            )
            token_sections.append(toml_token(token_id, token))
            pair_sections.append(toml_pair(token_id, amount))
    config = (
        "\n\n".join([config_header(len(selected)), *token_sections, *pair_sections])
        + "\n"
    )
    (output_dir / f"weth-star-top-{len(selected)}.toml").write_text(config)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--count", type=int, default=2000)
    args = parser.parse_args()
    api_key = os.environ.get("TYCHO_API_KEY_BETA")
    if not api_key:
        raise SystemExit("TYCHO_API_KEY_BETA is required")
    ranked, scores = rank_tokens(api_key)
    metadata = fetch_metadata(ranked, api_key)
    eligible = [metadata[address] for address in ranked if address in metadata]
    if len(eligible) < args.count:
        raise SystemExit(
            f"only {len(eligible)} eligible tokens found, requested {args.count}"
        )
    selected = eligible[: args.count]
    addresses = [token["address"].lower() for token in selected]
    prices = fetch_prices([WETH, *addresses])
    if WETH not in prices:
        raise SystemExit("DefiLlama did not return a WETH price")
    write_outputs(args.output_dir, selected, scores, prices, prices[WETH])
    priced = sum(address in prices for address in addresses)
    print(f"wrote {len(selected)} tokens ({priced} priced) to {args.output_dir}")


if __name__ == "__main__":
    main()
