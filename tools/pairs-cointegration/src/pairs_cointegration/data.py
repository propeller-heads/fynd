"""Load collector output and construct common-numeraire executable prices.

Token addresses are identity throughout, matching the collector contract;
symbols are display metadata only and are disambiguated when they collide.
"""

from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import numpy as np
import pandas as pd

REQUIRED_COLUMNS = {
    "block_number",
    "block_hash",
    "block_timestamp",
    "depth_index",
    "quote_role",
    "status",
    "token_in",
    "token_in_symbol",
    "token_in_decimals",
    "token_out",
    "token_out_symbol",
    "token_out_decimals",
    "amount_in",
    "amount_out",
}


@dataclass(frozen=True)
class PricePanel:
    """Aligned executable midpoint prices and observed bid/ask widths.

    Columns are unique display labels; `asset_addresses` maps each label back
    to the token address that is the actual identity.
    """

    prices: pd.DataFrame
    execution_spread_bps: pd.DataFrame
    timestamps: pd.Series
    asset_addresses: dict[str, str] = field(default_factory=dict)


def discover_parquet_files(input_path: Path) -> list[Path]:
    """Resolve a Parquet file, collector run directory, or Parquet directory."""
    path = input_path.expanduser().resolve()
    if path.is_file() and path.suffix == ".parquet":
        return [path]
    if not path.exists():
        msg = f"input does not exist: {path}"
        raise FileNotFoundError(msg)
    quote_dir = path / "parquet" / "quote_points"
    search_dir = quote_dir if quote_dir.is_dir() else path
    files = sorted(search_dir.glob("*.parquet"))
    if not files:
        msg = f"no quote-point Parquet files found under: {path}"
        raise FileNotFoundError(msg)
    return files


def load_quote_points(files: list[Path]) -> pd.DataFrame:
    """Read and validate the collector columns required by the analysis."""
    if not files:
        msg = "at least one Parquet input file is required"
        raise ValueError(msg)
    frames = [pd.read_parquet(path, columns=sorted(REQUIRED_COLUMNS)) for path in files]
    frame = pd.concat(frames, ignore_index=True)
    missing = REQUIRED_COLUMNS.difference(frame.columns)
    if missing:
        msg = f"collector input is missing columns: {', '.join(sorted(missing))}"
        raise ValueError(msg)
    return frame


def _human_amount(raw: object, decimals: object) -> float:
    # float conversion is lossy above 2**53 base units; acceptable for price
    # ratios, never reuse these floats as executable amounts.
    if raw is None or pd.isna(raw):
        return np.nan
    value = int(str(raw)) / (10 ** int(str(decimals)))
    return float(value)


def _symbol_by_address(frame: pd.DataFrame) -> dict[str, str]:
    symbols: dict[str, str] = {}
    for address_column, symbol_column in (
        ("token_in", "token_in_symbol"),
        ("token_out", "token_out_symbol"),
    ):
        pairs = frame[[address_column, symbol_column]].drop_duplicates()
        for address, symbol in pairs.itertuples(index=False):
            symbols[str(address).lower()] = str(symbol)
    return symbols


def resolve_numeraire(frame: pd.DataFrame, numeraire: str) -> str:
    """Resolve a numeraire given as address or symbol to one token address.

    A symbol that maps to multiple addresses is ambiguous and must be replaced
    by an explicit address.
    """
    requested = numeraire.strip().lower()
    symbols = _symbol_by_address(frame)
    if requested.startswith("0x"):
        if requested not in symbols:
            msg = f"numeraire address {requested} does not appear in the quote data"
            raise ValueError(msg)
        return requested
    matches = sorted(
        address for address, symbol in symbols.items() if symbol.upper() == numeraire.upper()
    )
    if not matches:
        msg = f"no token with symbol {numeraire} in the quote data"
        raise ValueError(msg)
    if len(matches) > 1:
        listed = ", ".join(matches)
        msg = f"symbol {numeraire} is ambiguous across addresses: {listed}; pass an address"
        raise ValueError(msg)
    return matches[0]


def _side_record(row: Mapping[str, Any], numeraire_address: str) -> dict[str, object] | None:
    token_in = str(row["token_in"]).lower()
    token_out = str(row["token_out"]).lower()
    amount_in = _human_amount(row["amount_in"], row["token_in_decimals"])
    amount_out = _human_amount(row["amount_out"], row["token_out_decimals"])
    if amount_in <= 0 or amount_out <= 0:
        return None
    common = {
        "block_number": int(row["block_number"]),
        "block_hash": str(row["block_hash"]),
        "block_timestamp": int(row["block_timestamp"]),
    }
    if token_out == numeraire_address and token_in != numeraire_address:
        return {**common, "asset": token_in, "side": "bid", "price": amount_out / amount_in}
    if token_in == numeraire_address and token_out != numeraire_address:
        return {**common, "asset": token_out, "side": "ask", "price": amount_in / amount_out}
    return None


def _display_labels(addresses: list[str], symbols: Mapping[str, str]) -> dict[str, str]:
    """Map addresses to unique display labels, suffixing colliding symbols."""
    by_symbol: dict[str, list[str]] = {}
    for address in addresses:
        by_symbol.setdefault(symbols.get(address, address).upper(), []).append(address)
    labels = {}
    for symbol, members in by_symbol.items():
        if len(members) == 1:
            labels[members[0]] = symbol
        else:
            for address in members:
                labels[address] = f"{symbol}-{address[2:8]}"
    return labels


def build_price_panel(
    quote_points: pd.DataFrame,
    *,
    numeraire: str,
    depth_index: int,
) -> PricePanel:
    """Build geometric bid/ask midpoint prices from successful forward quotes."""
    missing = REQUIRED_COLUMNS.difference(quote_points.columns)
    if missing:
        msg = f"quote points are missing columns: {', '.join(sorted(missing))}"
        raise ValueError(msg)
    selected = quote_points.loc[
        (quote_points["status"] == "success")
        & (quote_points["quote_role"] == "ladder_forward")
        & (quote_points["depth_index"] == depth_index)
    ]
    numeraire_address = resolve_numeraire(quote_points, numeraire)
    records = [
        record
        for row in selected.to_dict(orient="records")
        if (record := _side_record(row, numeraire_address)) is not None
    ]
    if not records:
        msg = (
            f"no successful forward quotes found against {numeraire_address} at depth {depth_index}"
        )
        raise ValueError(msg)
    sides = pd.DataFrame(records)
    duplicate = sides.duplicated(["block_number", "asset", "side"], keep=False)
    if duplicate.any():
        sample = sides.loc[duplicate, ["block_number", "asset", "side"]].iloc[0]
        msg = "duplicate executable side at block {block_number} for {asset} {side}".format(
            **sample
        )
        raise ValueError(msg)
    _validate_block_hashes(sides)
    prices = sides.pivot_table(
        index="block_number", columns=["asset", "side"], values="price", aggfunc="first"
    )
    addresses = sorted(set(prices.columns.get_level_values("asset")))
    mids = pd.DataFrame(index=prices.index)
    spreads = pd.DataFrame(index=prices.index)
    for address in addresses:
        if (address, "bid") not in prices or (address, "ask") not in prices:
            continue
        bid = prices[(address, "bid")]
        ask = prices[(address, "ask")]
        mids[address] = np.sqrt(bid * ask)
        spreads[address] = (ask / bid - 1.0) * 10_000.0
    if mids.empty:
        msg = "no asset has both executable quote directions"
        raise ValueError(msg)
    labels = _display_labels(list(mids.columns), _symbol_by_address(quote_points))
    asset_addresses = {labels[address]: address for address in mids.columns}
    mids = mids.rename(columns=labels)
    spreads = spreads.rename(columns=labels)
    timestamp_rows = sides.drop_duplicates("block_number").set_index("block_number")
    timestamps = pd.Series(timestamp_rows["block_timestamp"], index=mids.index, dtype="int64")
    return PricePanel(mids.sort_index(), spreads.sort_index(), timestamps, asset_addresses)


def _validate_block_hashes(sides: pd.DataFrame) -> None:
    counts = sides.groupby("block_number")["block_hash"].nunique()
    invalid = counts[counts != 1]
    if not invalid.empty:
        msg = f"multiple block hashes found for block {int(invalid.index[0])}"
        raise ValueError(msg)
