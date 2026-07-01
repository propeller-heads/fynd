"""Assemble unified dataset from raw parquet files using Polars.

Replaces the Rust `assemble` binary for large datasets (>100k files)
where loading all files into memory at once causes OOM.

Produces: slippage-data/unified/chain_id=1/unified.parquet
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import polars as pl

BATCH_SIZE = 5000


def find_workspace_root() -> Path:
    p = Path(__file__).resolve().parent if "__file__" in dir() else Path.cwd()
    while p != p.parent:
        cargo = p / "Cargo.toml"
        if cargo.exists() and "[workspace]" in cargo.read_text():
            return p
        p = p.parent
    raise FileNotFoundError("workspace root not found")


PROJECT_ROOT = find_workspace_root()
DATA_DIR = PROJECT_ROOT / "slippage-data"
OUTPUT_DIR = DATA_DIR / "unified" / "chain_id=1"


def load_parquet_batched(directory: Path, label: str) -> pl.DataFrame:
    files = sorted(directory.glob("*.parquet"))
    files = [f for f in files if "STALE" not in f.name]
    if not files:
        print(f"  {label}: no files found")
        return pl.DataFrame()

    t0 = time.time()
    frames = []
    for i in range(0, len(files), BATCH_SIZE):
        batch = files[i : i + BATCH_SIZE]
        frames.append(pl.concat([pl.read_parquet(f) for f in batch]))
        done = min(i + BATCH_SIZE, len(files))
        elapsed = time.time() - t0
        rate = done / elapsed if elapsed > 0 else 0
        eta = (len(files) - done) / rate if rate > 0 else 0
        print(
            f"  {label}: {done:,}/{len(files):,} files "
            f"({elapsed:.0f}s, ETA {eta:.0f}s)",
            end="\r",
        )

    result = pl.concat(frames)
    elapsed = time.time() - t0
    print(
        f"  {label}: {len(files):,} files → "
        f"{result.shape[0]:,} rows in {elapsed:.0f}s"
        + " " * 20
    )
    return result


def load_hop_max_agg(directory: Path) -> pl.DataFrame:
    """Stream hop_decay files, aggregating max decay per key per batch.

    Only the max ``hop_decay_bps`` per (quote_id, solver_id, block_offset)
    is needed downstream. Aggregating within each batch and combining the
    partial maxima keeps peak memory bounded to one batch rather than the
    full ~20M+ row hop_decay table, which OOMs at >1M files.
    """
    files = sorted(directory.glob("*.parquet"))
    files = [f for f in files if "STALE" not in f.name]
    if not files:
        print("  hop_decay: no files found")
        return pl.DataFrame()

    keys = ["quote_id", "solver_id", "block_offset"]
    t0 = time.time()
    partials = []
    for i in range(0, len(files), BATCH_SIZE):
        batch = files[i : i + BATCH_SIZE]
        frame = pl.concat([pl.read_parquet(f, columns=keys + ["hop_decay_bps"]) for f in batch])
        # Aggregate within the batch only; defer the global merge to a single
        # pass at the end (re-aggregating a growing accumulator each batch is
        # quadratic and dominates wall-clock at >1M files).
        partials.append(
            frame.group_by(keys).agg(
                pl.col("hop_decay_bps").max().alias("max_hop_decay_bps")
            )
        )
        done = min(i + BATCH_SIZE, len(files))
        elapsed = time.time() - t0
        rate = done / elapsed if elapsed > 0 else 0
        eta = (len(files) - done) / rate if rate > 0 else 0
        print(
            f"  hop_decay (streaming max): {done:,}/{len(files):,} files "
            f"({elapsed:.0f}s, ETA {eta:.0f}s)",
            end="\r",
        )

    # Single global merge of all per-batch partials.
    acc = pl.concat(partials).group_by(keys).agg(
        pl.col("max_hop_decay_bps").max()
    )
    elapsed = time.time() - t0
    print(f"  hop_decay (streaming max): {len(files):,} files → {acc.shape[0]:,} keys in {elapsed:.0f}s" + " " * 20)
    return acc


def extract_hop_count(route_json: str) -> int:
    try:
        data = json.loads(route_json)
        return len(data.get("swaps", []))
    except (json.JSONDecodeError, TypeError):
        return 0


def extract_split_count(route_json: str) -> int:
    try:
        data = json.loads(route_json)
        swaps = data.get("swaps", [])
        return sum(
            1
            for s in swaps
            if isinstance(s.get("split"), (int, float))
            and 0 < s["split"] < 1
        )
    except (json.JSONDecodeError, TypeError):
        return 0


STABLECOINS = {
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": (
        "stable",
        43e9,
    ),
    "0xdac17f958d2ee523a2206206994597c13d831ec7": (
        "stable",
        140e9,
    ),
    "0x6b175474e89094c44da98b954eedeac495271d0f": (
        "stable",
        5e9,
    ),
    "0x853d955acef822db058eb8505911ed77f175b99e": (
        "stable",
        0.6e9,
    ),
    "0x4fabb145d64652a948d72533023f6e7a623c7c53": (
        "stable",
        0.1e9,
    ),
    "0x0000000000085d4780b73119b644ae5ecd22b376": (
        "stable",
        0.1e9,
    ),
    "0x8e870d67f660d95d5be530380d0ec0bd388289e1": (
        "stable",
        0.8e9,
    ),
    "0x1a7e4e63778b4f12a199c062f3efdd288afcbce8": (
        "stable",
        0.1e9,
    ),
    "0x5f98805a4e8be255a32880fdec7f6728c6568ba0": (
        "stable",
        0.3e9,
    ),
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": (
        "blue_chip",
        300e9,
    ),
}

BLUE_CHIP_THRESHOLD = 10e9
MID_CAP_THRESHOLD = 100e6
LONG_TAIL_THRESHOLD = 1e6

COINGECKO_API_BASE = "https://api.coingecko.com/api/v3"
COINGECKO_RATE_LIMIT_SLEEP = 2.5
COINGECKO_MAX_RETRIES = 5


def load_coingecko_cache(cache_path: Path) -> dict:
    """Load cached CoinGecko metadata from disk."""
    if cache_path.exists():
        with open(cache_path) as f:
            cache = json.load(f)
        print(f"  Loaded {len(cache):,} tokens from CoinGecko cache")
        return cache
    return {}


def save_coingecko_cache(cache: dict, cache_path: Path):
    """Persist CoinGecko metadata cache to disk."""
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    with open(cache_path, "w") as f:
        json.dump(cache, f, indent=2)
    print(f"  Saved {len(cache):,} tokens to CoinGecko cache")


def fetch_coingecko_token(address: str, api_key: str) -> dict:
    """Fetch token metadata from CoinGecko API with retry on 429."""
    url = (
        f"{COINGECKO_API_BASE}/coins/ethereum/contract/{address}"
    )
    req = urllib.request.Request(url)
    req.add_header("x-cg-demo-api-key", api_key)
    req.add_header("accept", "application/json")

    for attempt in range(COINGECKO_MAX_RETRIES):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                data = json.loads(resp.read().decode())

            market_data = data.get("market_data", {})
            mcap_obj = market_data.get("market_cap", {})
            fdv_obj = market_data.get(
                "fully_diluted_valuation", {}
            )
            market_cap = mcap_obj.get("usd") if mcap_obj else None
            fdv = fdv_obj.get("usd") if fdv_obj else None
            category = classify_mcap(market_cap)

            return {
                "market_cap": market_cap,
                "fdv": fdv,
                "category": category,
            }
        except urllib.error.HTTPError as e:
            if e.code == 429:
                backoff = COINGECKO_RATE_LIMIT_SLEEP * (
                    2**attempt
                )
                print(
                    f"\n  Rate limited (429), "
                    f"backing off {backoff:.0f}s "
                    f"(attempt {attempt + 1}/"
                    f"{COINGECKO_MAX_RETRIES})..."
                )
                time.sleep(backoff)
                continue
            return {
                "market_cap": None,
                "fdv": None,
                "category": "long_tail",
            }
        except (urllib.error.URLError, TimeoutError, OSError):
            return {
                "market_cap": None,
                "fdv": None,
                "category": "long_tail",
            }

    return {
        "market_cap": None,
        "fdv": None,
        "category": "long_tail",
    }


def format_eta(seconds: float) -> str:
    """Format seconds as human-readable duration."""
    total = int(seconds)
    if total < 60:
        return f"{total}s"
    if total < 3600:
        return f"{total // 60}m{total % 60}s"
    hours = total // 3600
    mins = (total % 3600) // 60
    return f"{hours}h{mins}m"


def enrich_tokens_from_coingecko(
    all_tokens: set[str],
    api_key: str,
    cache_path: Path,
) -> dict[str, dict]:
    """Fetch CoinGecko metadata for tokens not in STABLECOINS.

    Returns a dict mapping address -> {market_cap, fdv, category}.
    """
    cache = load_coingecko_cache(cache_path)
    result = {}

    for addr in all_tokens:
        normalized = addr.lower()
        if normalized in STABLECOINS:
            cat, mcap = STABLECOINS[normalized]
            result[normalized] = {
                "market_cap": mcap,
                "fdv": mcap,
                "category": cat,
            }
        elif normalized in cache:
            result[normalized] = cache[normalized]

    tokens_to_fetch = [
        addr.lower()
        for addr in all_tokens
        if addr.lower() not in result
    ]

    if not tokens_to_fetch:
        print("  All tokens already cached or hardcoded")
        return result

    print(
        f"  Fetching {len(tokens_to_fetch):,} tokens "
        f"from CoinGecko API..."
    )
    t0 = time.time()

    for i, addr in enumerate(tokens_to_fetch):
        metadata = fetch_coingecko_token(addr, api_key)
        result[addr] = metadata
        cache[addr] = metadata

        elapsed = time.time() - t0
        done = i + 1
        rate = done / elapsed if elapsed > 0 else 0
        remaining = len(tokens_to_fetch) - done
        eta = remaining / rate if rate > 0 else 0
        print(
            f"  Fetching CoinGecko metadata: "
            f"{done}/{len(tokens_to_fetch)} tokens "
            f"(ETA {format_eta(eta)})",
            end="\r",
        )

        if i < len(tokens_to_fetch) - 1:
            time.sleep(COINGECKO_RATE_LIMIT_SLEEP)

        if done % 100 == 0:
            save_coingecko_cache(cache, cache_path)

    print(
        f"\n  Fetched {len(tokens_to_fetch):,} tokens "
        f"in {format_eta(time.time() - t0)}"
    )
    save_coingecko_cache(cache, cache_path)
    return result


def classify_token(
    address: str,
    enriched: dict[str, dict] | None = None,
) -> tuple[str, float | None]:
    """Classify a token by address using enriched data or hardcoded list."""
    addr = address.lower()
    if addr in STABLECOINS:
        cat, mcap = STABLECOINS[addr]
        return (cat, float(mcap))
    if enriched and addr in enriched:
        meta = enriched[addr]
        mcap = meta["market_cap"]
        return (meta["category"], float(mcap) if mcap is not None else None)
    return ("long_tail", None)


def classify_mcap(mcap: float | None) -> str:
    if mcap is None:
        return "unknown"
    if mcap >= BLUE_CHIP_THRESHOLD:
        return "blue_chip"
    if mcap >= MID_CAP_THRESHOLD:
        return "mid_cap"
    if mcap >= LONG_TAIL_THRESHOLD:
        return "long_tail"
    return "meme"


def pair_bucket(cat_a: str, cat_b: str) -> str:
    cats = sorted([cat_a, cat_b])
    mapping = {
        ("blue_chip", "blue_chip"): "large-large",
        ("blue_chip", "long_tail"): "large-longtail",
        ("blue_chip", "meme"): "large-meme",
        ("blue_chip", "mid_cap"): "large-mid",
        ("blue_chip", "stable"): "stable-large",
        ("blue_chip", "unknown"): "large-unknown",
        ("long_tail", "long_tail"): "longtail-longtail",
        ("long_tail", "meme"): "longtail-meme",
        ("long_tail", "mid_cap"): "mid-longtail",
        ("long_tail", "stable"): "stable-longtail",
        ("long_tail", "unknown"): "longtail-unknown",
        ("meme", "meme"): "meme-meme",
        ("meme", "mid_cap"): "mid-meme",
        ("meme", "stable"): "stable-meme",
        ("meme", "unknown"): "meme-unknown",
        ("mid_cap", "mid_cap"): "mid-mid",
        ("mid_cap", "stable"): "stable-mid",
        ("mid_cap", "unknown"): "mid-unknown",
        ("stable", "stable"): "stable-stable",
        ("stable", "unknown"): "stable-unknown",
        ("unknown", "unknown"): "unknown-unknown",
    }
    return mapping.get(tuple(cats), f"{cats[0]}-{cats[1]}")


ETH_REF_BLOCK = 17_000_000
ETH_REF_TS = 1_681_340_400
ETH_BLOCK_TIME = 12


def estimate_temporal(block_number: int) -> tuple[int | None, int | None]:
    if block_number < ETH_REF_BLOCK:
        return (None, None)
    ts = ETH_REF_TS + (block_number - ETH_REF_BLOCK) * ETH_BLOCK_TIME
    from datetime import datetime, timezone

    dt = datetime.fromtimestamp(ts, tz=timezone.utc)
    return (dt.hour, dt.weekday())


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Assemble unified slippage dataset"
    )
    parser.add_argument(
        "--coingecko-api-key",
        default=None,
        help=(
            "CoinGecko API key. "
            "Falls back to COINGECKO_API_KEY env var."
        ),
    )
    parser.add_argument(
        "--skip-coingecko",
        action="store_true",
        help="Skip CoinGecko enrichment entirely",
    )
    return parser.parse_args()


def main():
    args = parse_args()
    print("=== Polars Assembly ===\n")

    print("Loading raw parquet files...")
    ql = load_parquet_batched(DATA_DIR, "quote_log")
    ql = ql.filter(pl.col("quote_id").is_not_null())

    trd = load_parquet_batched(
        DATA_DIR / "tycho_route_decay", "tycho_route_decay"
    )

    # Stream hop_decay → max per key (avoids holding the full ~20M+ row table).
    hop_agg = load_hop_max_agg(DATA_DIR / "hop_decay")

    print(
        f"\nSource counts: quote_log={ql.shape[0]:,}, "
        f"tycho_route={trd.shape[0]:,}, hop_agg_keys={hop_agg.shape[0]:,}"
    )

    # ------------------------------------------------------------------
    # Compute ALL per-quote features on the small quote_log (~tens of
    # thousands of rows) BEFORE expanding to the ~17M-row offset table.
    # This keeps route_json (a large per-row string) and every string-key
    # join off the big frame, which is what made the naive ordering OOM /
    # run for hours.
    # ------------------------------------------------------------------
    print("Computing per-quote features on quote log...")
    ql = ql.with_columns(
        [
            pl.col("route_json")
            .map_elements(extract_hop_count, return_dtype=pl.Int64)
            .alias("hop_count"),
            pl.col("route_json")
            .map_elements(extract_split_count, return_dtype=pl.Int64)
            .alias("split_count"),
            (pl.col("chain_id") == 8453).alias("is_l2"),
        ]
    )

    print("Classifying tokens...")
    all_tokens = set(
        ql.select("token_in").unique().to_series().to_list()
        + ql.select("token_out").unique().to_series().to_list()
    )
    print(f"  Found {len(all_tokens):,} unique tokens")

    enriched = None
    api_key = args.coingecko_api_key or os.environ.get(
        "COINGECKO_API_KEY", ""
    )
    cache_path = DATA_DIR / "coingecko_cache.json"

    if args.skip_coingecko:
        print("  Skipping CoinGecko enrichment (--skip-coingecko)")
    elif not api_key:
        print(
            "  No CoinGecko API key provided, "
            "skipping enrichment"
        )
        if cache_path.exists():
            print("  Loading cached enrichment data...")
            enriched = {}
            cache = load_coingecko_cache(cache_path)
            for addr in all_tokens:
                normalized = addr.lower()
                if normalized in STABLECOINS:
                    cat, mcap = STABLECOINS[normalized]
                    enriched[normalized] = {
                        "market_cap": mcap,
                        "fdv": mcap,
                        "category": cat,
                    }
                elif normalized in cache:
                    enriched[normalized] = cache[normalized]
    else:
        enriched = enrich_tokens_from_coingecko(
            all_tokens, api_key, cache_path
        )

    token_cat_map = {}
    token_mcap_map = {}
    for addr in all_tokens:
        cat, mcap = classify_token(addr, enriched)
        token_cat_map[addr] = cat
        token_mcap_map[addr] = float(mcap) if mcap is not None else None

    enriched_count = sum(
        1 for m in token_mcap_map.values() if m is not None
    )
    print(
        f"  Tokens with market cap: "
        f"{enriched_count}/{len(all_tokens)} "
        f"({100 * enriched_count / max(len(all_tokens), 1):.1f}%)"
    )

    # Vectorized token classification via lookup DataFrame join
    token_lookup = pl.DataFrame({
        "token_addr": list(token_cat_map.keys()),
        "category": list(token_cat_map.values()),
        "mcap": [token_mcap_map[a] for a in token_cat_map],
    }).with_columns(pl.col("mcap").cast(pl.Float64))

    ql = (
        ql
        .join(
            token_lookup.rename({"token_addr": "token_in", "category": "token_in_category", "mcap": "token_in_mcap"}),
            on="token_in",
            how="left",
        )
        .join(
            token_lookup.rename({"token_addr": "token_out", "category": "token_out_category", "mcap": "token_out_mcap"}),
            on="token_out",
            how="left",
        )
    )
    ql = ql.with_columns(
        [
            pl.col("token_in_category").fill_null("unknown"),
            pl.col("token_out_category").fill_null("unknown"),
        ]
    )

    # Vectorized pair_bucket via concatenated string key + .replace()
    pair_bucket_map = {}
    all_categories = sorted(
        {v for v in token_cat_map.values()} | {"unknown"}
    )
    for cat_a in all_categories:
        for cat_b in all_categories:
            key = f"{cat_a}||{cat_b}"
            pair_bucket_map[key] = pair_bucket(cat_a, cat_b)

    mcap_in = pl.col("token_in_mcap")
    mcap_out = pl.col("token_out_mcap")
    both_valid = (
        mcap_in.is_not_null()
        & mcap_out.is_not_null()
        & (mcap_in > 0)
        & (mcap_out > 0)
    )
    max_mcap_expr = pl.max_horizontal(mcap_in, mcap_out)
    min_mcap_expr = pl.min_horizontal(mcap_in, mcap_out)

    ql = ql.with_columns(
        [
            pl.concat_str(
                [
                    pl.col("token_in_category"),
                    pl.col("token_out_category"),
                ],
                separator="||",
            )
            .replace_strict(pair_bucket_map, default="unknown-unknown")
            .alias("pair_bucket"),
            pl.when(both_valid)
            .then((max_mcap_expr / min_mcap_expr).log())
            .otherwise(None)
            .alias("log_mcap_ratio"),
            pl.when(mcap_in.is_not_null() & mcap_out.is_not_null())
            .then(min_mcap_expr)
            .otherwise(None)
            .alias("min_mcap"),
            pl.when(mcap_in.is_not_null() & mcap_out.is_not_null())
            .then(max_mcap_expr)
            .otherwise(None)
            .alias("max_mcap"),
        ]
    )

    # ------------------------------------------------------------------
    # Now expand to the offset table. The per-quote feature frame (ql) is
    # small; the joins below carry only compact columns (no route_json,
    # no string-key joins) into the ~17M-row result.
    # ------------------------------------------------------------------
    ql_compact = ql.select(
        [
            "quote_id", "solver_id", "request_id", "is_winner",
            "block_number", "chain_id", "amount_in", "amount_out",
            "gas_estimate", "algorithm_type", "n_alternatives",
            "gap_to_second_best_bps", "token_in", "token_out",
            "token_in_category", "token_out_category", "pair_bucket",
            "log_mcap_ratio", "min_mcap", "max_mcap",
            "hop_count", "split_count", "is_l2",
        ]
    )

    print("Joining tycho_route_decay with hop aggregates...")
    unified = trd.join(
        hop_agg,
        on=["quote_id", "solver_id", "block_offset"],
        how="left",
    )
    print("Joining with per-quote features...")
    unified = unified.join(
        ql_compact, on=["quote_id", "solver_id"], how="inner"
    )
    print(f"After join: {unified.shape[0]:,} rows")

    # Vectorized temporal features via native Polars arithmetic
    print("Computing temporal features...")
    estimated_ts = (
        pl.lit(ETH_REF_TS)
        + (pl.col("block_number") - pl.lit(ETH_REF_BLOCK)) * pl.lit(ETH_BLOCK_TIME)
    )
    estimated_dt = pl.from_epoch(estimated_ts, time_unit="s")
    unified = unified.with_columns(
        [
            pl.when(pl.col("block_number") >= pl.lit(ETH_REF_BLOCK))
            .then(estimated_dt.dt.hour())
            .otherwise(None)
            .cast(pl.UInt32)
            .alias("hour_of_day"),
            pl.when(pl.col("block_number") >= pl.lit(ETH_REF_BLOCK))
            .then(estimated_dt.dt.weekday())
            .otherwise(None)
            .cast(pl.UInt32)
            .alias("day_of_week"),
        ]
    )

    final_cols = [
        "quote_id",
        "solver_id",
        "request_id",
        "is_winner",
        "block_number",
        "chain_id",
        "amount_in",
        "amount_out",
        "gas_estimate",
        "algorithm_type",
        "n_alternatives",
        "gap_to_second_best_bps",
        "token_in",
        "token_out",
        "token_in_category",
        "token_out_category",
        "pair_bucket",
        "log_mcap_ratio",
        "min_mcap",
        "max_mcap",
        "block_offset",
        "max_hop_decay_bps",
        "route_decay_bps",
        "market_movement_bps",
        "execution_slippage_bps",
        "hop_count",
        "split_count",
        "is_l2",
        "hour_of_day",
        "day_of_week",
    ]

    existing = [c for c in final_cols if c in unified.columns]
    unified = unified.select(existing)

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    output_path = OUTPUT_DIR / "unified.parquet"
    unified.write_parquet(output_path, compression="zstd")

    print(f"\nWrote {unified.shape[0]:,} rows × {unified.shape[1]} cols")
    print(f"Output: {output_path}")

    print("\n=== Data summary ===")
    print(
        f"Unique quotes: {unified['quote_id'].n_unique():,}"
    )
    blocks = unified["block_number"]
    span = blocks.max() - blocks.min()
    print(
        f"Block range: {blocks.min():,} → {blocks.max():,} "
        f"({span:,} blocks, ~{span * 12 / 3600:.1f}h)"
    )
    print(
        f"Offsets: "
        f"{sorted(unified['block_offset'].unique().to_list())}"
    )
    print(f"Pair buckets: {unified['pair_bucket'].n_unique()}")

    mm_nulls = unified["market_movement_bps"].null_count()
    mm_total = unified.shape[0]
    print(
        f"Decomposition fill: "
        f"{mm_total - mm_nulls:,}/{mm_total:,} "
        f"({100 * (mm_total - mm_nulls) / mm_total:.1f}%)"
    )

    print("\nNull rates:")
    for col in unified.columns:
        nulls = unified[col].null_count()
        if nulls > 0:
            print(
                f"  {col:>30}: "
                f"{nulls:,}/{mm_total:,} "
                f"({100 * nulls / mm_total:.1f}%)"
            )


if __name__ == "__main__":
    main()
