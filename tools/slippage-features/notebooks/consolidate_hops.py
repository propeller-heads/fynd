"""Consolidate the per-quote hop parquet files into single files.

The collection writes one tiny parquet per quote (~1.7M files each for
hop_decay and hop_static). Reading that many files takes ~50 min per
directory, which the EDA notebook would pay on every run. This script
reads them once in batches and writes two consolidated parquet files
that the notebook can load in seconds.

Outputs:
  slippage-data/hop_consolidated/hop_decay.parquet
  slippage-data/hop_consolidated/hop_static.parquet
"""
import time
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
OUT_DIR = DATA_DIR / "hop_consolidated"


def consolidate(src: Path, out_file: Path, label: str):
    files = sorted(src.glob("*.parquet"))
    files = [f for f in files if "STALE" not in f.name]
    if not files:
        print(f"  {label}: no files found")
        return

    t0 = time.time()
    writer = None
    rows = 0
    for i in range(0, len(files), BATCH_SIZE):
        batch = files[i : i + BATCH_SIZE]
        frame = pl.concat([pl.read_parquet(f) for f in batch])
        rows += frame.shape[0]
        arrow_tbl = frame.to_arrow()
        if writer is None:
            import pyarrow.parquet as pq

            writer = pq.ParquetWriter(
                out_file, arrow_tbl.schema, compression="zstd"
            )
        writer.write_table(arrow_tbl)
        done = min(i + BATCH_SIZE, len(files))
        elapsed = time.time() - t0
        rate = done / elapsed if elapsed > 0 else 0
        eta = (len(files) - done) / rate if rate > 0 else 0
        print(
            f"  {label}: {done:,}/{len(files):,} files "
            f"({elapsed:.0f}s, ETA {eta:.0f}s)",
            end="\r",
        )
    if writer is not None:
        writer.close()
    print(
        f"  {label}: {len(files):,} files → {rows:,} rows "
        f"in {time.time() - t0:.0f}s → {out_file.name}" + " " * 20
    )


def main():
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    print("=== Consolidating hop parquet files ===\n")
    consolidate(DATA_DIR / "hop_decay", OUT_DIR / "hop_decay.parquet", "hop_decay")
    consolidate(DATA_DIR / "hop_static", OUT_DIR / "hop_static.parquet", "hop_static")
    print("\nDone.")


if __name__ == "__main__":
    main()
