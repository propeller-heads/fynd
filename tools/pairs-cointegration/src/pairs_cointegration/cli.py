"""Command-line entry point for cointegration analysis."""

import argparse
import sys
from datetime import UTC, datetime
from pathlib import Path

from pairs_cointegration.data import build_price_panel, discover_parquet_files, load_quote_points
from pairs_cointegration.models import AnalysisConfig, AnalysisRun
from pairs_cointegration.report import write_outputs
from pairs_cointegration.statistics import analyze_universe


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Search Fynd quote histories for exploratory cointegration candidates."
    )
    parser.add_argument(
        "input", type=Path, help="Collector run directory or quote-point Parquet path"
    )
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument(
        "--numeraire",
        default="USDC",
        help="Numeraire token address (0x...) or unambiguous symbol",
    )
    parser.add_argument("--depth-index", type=int, default=0)
    parser.add_argument("--min-observations", type=int, default=60)
    parser.add_argument("--recommended-observations", type=int, default=500)
    parser.add_argument("--significance-level", type=float, default=0.05)
    parser.add_argument("--fdr-level", type=float, default=0.05)
    return parser


def main() -> None:
    """Run one complete collector-to-report analysis."""
    args = _parser().parse_args()
    config = AnalysisConfig(
        min_observations=args.min_observations,
        recommended_observations=args.recommended_observations,
        significance_level=args.significance_level,
        fdr_level=args.fdr_level,
    )
    files = discover_parquet_files(args.input)
    panel = build_price_panel(
        load_quote_points(files),
        numeraire=args.numeraire,
        depth_index=args.depth_index,
    )
    results = analyze_universe(panel.prices, config)
    run = AnalysisRun(
        input_files=tuple(files),
        numeraire=args.numeraire,
        depth_index=args.depth_index,
        block_start=int(panel.prices.index.min()),
        block_end=int(panel.prices.index.max()),
        generated_at=datetime.now(UTC).isoformat(),
    )
    paths = write_outputs(args.output_dir, panel, results, config, run)
    dashboard = next(path for path in paths if path.name == "dashboard.html")
    sys.stdout.write(
        f"Analyzed {len(results)} pairs across {len(panel.prices)} blocks.\n"
        f"Dashboard: {dashboard}\n"
    )


if __name__ == "__main__":
    main()
