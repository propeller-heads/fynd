from pathlib import Path

import pandas as pd
import pytest

from pairs_cointegration.data import build_price_panel, discover_parquet_files

ADDRESSES = {
    "USDC": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
    "WETH": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
}
DECIMALS = {"USDC": 6, "WETH": 18}


def quote(
    block: int,
    symbol_in: str,
    symbol_out: str,
    amount_in: str,
    amount_out: str,
    **overrides: object,
) -> dict[str, object]:
    record: dict[str, object] = {
        "block_number": block,
        "block_hash": f"0x{block:064x}",
        "block_timestamp": 1_700_000_000 + block,
        "depth_index": 0,
        "quote_role": "ladder_forward",
        "status": "success",
        "token_in": ADDRESSES[symbol_in],
        "token_in_symbol": symbol_in,
        "token_in_decimals": DECIMALS[symbol_in],
        "token_out": ADDRESSES[symbol_out],
        "token_out_symbol": symbol_out,
        "token_out_decimals": DECIMALS[symbol_out],
        "amount_in": amount_in,
        "amount_out": amount_out,
    }
    record.update(overrides)
    return record


def test_build_price_panel_uses_executable_bid_ask_midpoint() -> None:
    frame = pd.DataFrame(
        [
            quote(10, "WETH", "USDC", str(10**18), "1990000000"),
            quote(10, "USDC", "WETH", "2010000000", str(10**18)),
            quote(11, "WETH", "USDC", str(10**18), "2089500000"),
            quote(11, "USDC", "WETH", "2110500000", str(10**18)),
        ]
    )

    panel = build_price_panel(frame, numeraire="USDC", depth_index=0)

    assert panel.prices.loc[10, "WETH"] == pytest.approx((1990 * 2010) ** 0.5)
    assert panel.prices.loc[11, "WETH"] == pytest.approx((2089.5 * 2110.5) ** 0.5)
    assert panel.execution_spread_bps.loc[10, "WETH"] == pytest.approx((2010 / 1990 - 1) * 10_000)
    assert panel.asset_addresses == {"WETH": ADDRESSES["WETH"]}


def test_build_price_panel_rejects_duplicate_sides() -> None:
    rows = [
        quote(10, "WETH", "USDC", str(10**18), "1990000000"),
        quote(10, "WETH", "USDC", str(10**18), "1990000001"),
        quote(10, "USDC", "WETH", "2010000000", str(10**18)),
    ]

    with pytest.raises(ValueError, match="duplicate executable side"):
        build_price_panel(pd.DataFrame(rows), numeraire="USDC", depth_index=0)


def test_symbol_collisions_stay_separate_assets() -> None:
    impostor = "0x000000000000000000000000000000000000dead"
    rows = [
        quote(10, "WETH", "USDC", str(10**18), "1990000000"),
        quote(10, "USDC", "WETH", "2010000000", str(10**18)),
        quote(10, "WETH", "USDC", str(10**18), "990000000", token_in=impostor),
        quote(10, "USDC", "WETH", "1010000000", str(10**18), token_out=impostor),
    ]

    panel = build_price_panel(pd.DataFrame(rows), numeraire="USDC", depth_index=0)

    assert len(panel.prices.columns) == 2
    assert set(panel.asset_addresses.values()) == {ADDRESSES["WETH"], impostor}
    labels = sorted(panel.prices.columns)
    assert all(label.startswith("WETH-") for label in labels)


def test_ambiguous_numeraire_symbol_requires_address() -> None:
    impostor = "0x000000000000000000000000000000000000dead"
    rows = [
        quote(10, "WETH", "USDC", str(10**18), "1990000000"),
        quote(10, "USDC", "WETH", "2010000000", str(10**18)),
        quote(10, "WETH", "USDC", str(10**18), "990000000", token_out=impostor),
    ]

    with pytest.raises(ValueError, match="ambiguous"):
        build_price_panel(pd.DataFrame(rows), numeraire="USDC", depth_index=0)


def test_numeraire_accepts_explicit_address() -> None:
    frame = pd.DataFrame(
        [
            quote(10, "WETH", "USDC", str(10**18), "1990000000"),
            quote(10, "USDC", "WETH", "2010000000", str(10**18)),
        ]
    )

    panel = build_price_panel(frame, numeraire=ADDRESSES["USDC"], depth_index=0)

    assert list(panel.prices.columns) == ["WETH"]


def test_discover_parquet_files_accepts_collector_root(tmp_path: Path) -> None:
    quote_dir = tmp_path / "parquet" / "quote_points"
    quote_dir.mkdir(parents=True)
    first = quote_dir / "part-1.parquet"
    second = quote_dir / "part-2.parquet"
    first.touch()
    second.touch()

    assert discover_parquet_files(tmp_path) == [first, second]
