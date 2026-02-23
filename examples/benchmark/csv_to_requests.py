#!/usr/bin/env python3
"""Convert Dune dex.trades CSV to fynd benchmark JSON requests."""
import csv
import json
import sys

# Token decimals lookup (common Ethereum tokens)
DECIMALS = {
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": 18,  # WETH
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": 6,   # USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7": 6,   # USDT
    "0x6b175474e89094c44da98b954eedeac495271d0f": 18,  # DAI
    "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599": 8,   # WBTC
    "0x6982508145454ce325ddbe47a25d4ec3d2311933": 18,  # PEPE
    "0x95ad61b0a150d79219dcf64e1e6cc01f0b64c4ce": 18,  # SHIB
    "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984": 18,  # UNI
    "0x514910771af9ca656af840dff83e8264ecf986ca": 18,  # LINK
    "0x7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9": 18,  # AAVE
    "0x0000000000000000000000000000000000000000": 18,  # ETH (native)
}


def to_checksum_ish(addr):
    """Normalize address to lowercase hex with 0x prefix."""
    return addr.lower().strip()


def amount_to_base_units(amount_str, token_addr):
    """Convert decimal amount string to integer base units."""
    addr = to_checksum_ish(token_addr)
    decimals = DECIMALS.get(addr, 18)  # default to 18
    amount = float(amount_str)
    return str(int(amount * (10 ** decimals)))


def convert_csv(csv_path, output_path, limit=None):
    requests = []
    skipped = 0

    with open(csv_path) as f:
        reader = csv.DictReader(f)
        for i, row in enumerate(reader):
            if limit and i >= limit:
                break

            # The CSV has token_sold as input (sell side)
            token_in = to_checksum_ish(row["token_sold_address"])
            token_out = to_checksum_ish(row["token_bought_address"])

            # Handle native ETH (0x000...000) -> WETH
            weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            if token_in == "0x0000000000000000000000000000000000000000":
                token_in = weth
            if token_out == "0x0000000000000000000000000000000000000000":
                token_out = weth

            try:
                amount = amount_to_base_units(
                    row["token_sold_amount"], token_in
                )
            except (ValueError, KeyError):
                skipped += 1
                continue

            if int(amount) <= 0:
                skipped += 1
                continue

            req = {
                "orders": [{
                    "id": "",
                    "token_in": token_in,
                    "token_out": token_out,
                    "amount": amount,
                    "side": "sell",
                    "sender": "0x0000000000000000000000000000000000000001",
                    "receiver": None,
                }],
                "options": {
                    "timeout_ms": 5000,
                    "min_responses": None,
                    "max_gas": None,
                },
            }
            requests.append(req)

    with open(output_path, "w") as f:
        json.dump(requests, f)

    print(f"Converted {len(requests)} trades, skipped {skipped}")
    return len(requests)


if __name__ == "__main__":
    csv_path = sys.argv[1] if len(sys.argv) > 1 else "trades_10k_dune_eth_feb2026.csv"
    output_path = sys.argv[2] if len(sys.argv) > 2 else "trades_10k_requests.json"
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else None
    convert_csv(csv_path, output_path, limit)
