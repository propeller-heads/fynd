#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["requests"]
# ///
"""Ask Tycho what it knows about the tokens that make orders unbatchable.

Two very different causes look identical from inside the solver:
  * Tycho has no record of the token at all — nothing to route with, unfixable here.
  * Tycho has it, but below fynd's `min_token_quality` (default 100) — a threshold choice.

Reads TYCHO_API_KEY from the environment directly (never via the shell).

Usage: token_quality_probe.py <token-address> [more addresses...]
"""

from __future__ import annotations

import os
import sys

import requests

URL = "https://tycho-base-beta.propellerheads.xyz/v1/tokens"


def main() -> int:
    addresses = [a.lower() for a in sys.argv[1:]]
    if not addresses:
        print("pass token addresses", file=sys.stderr)
        return 1
    key = os.environ.get("TYCHO_API_KEY")
    if not key:
        print("TYCHO_API_KEY not set in this environment", file=sys.stderr)
        return 1

    response = requests.post(
        URL,
        headers={"Authorization": key, "Content-Type": "application/json"},
        json={
            "chain": "base",
            "token_addresses": addresses,
            "min_quality": 0,
            "pagination": {"page": 0, "page_size": 100},
        },
        timeout=30,
    )
    response.raise_for_status()
    found = {t["address"].lower(): t for t in response.json().get("tokens", [])}

    print(f"{'token':<44} {'known':>6} {'quality':>8} {'decimals':>9}  symbol")
    for address in addresses:
        token = found.get(address)
        if token is None:
            print(f"{address:<44} {'NO':>6} {'-':>8} {'-':>9}  (not indexed by tycho)")
        else:
            print(
                f"{address:<44} {'yes':>6} {token.get('quality', '?'):>8} "
                f"{token.get('decimals', '?'):>9}  {token.get('symbol', '?')}"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
