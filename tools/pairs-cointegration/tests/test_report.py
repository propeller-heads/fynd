from pathlib import Path

import pandas as pd

from pairs_cointegration.data import PricePanel
from pairs_cointegration.models import AnalysisConfig, AnalysisRun
from pairs_cointegration.report import write_outputs
from pairs_cointegration.statistics import analyze_universe
from tests.test_statistics import synthetic_prices


def test_write_outputs_creates_auditable_offline_artifacts(tmp_path: Path) -> None:
    prices = synthetic_prices().iloc[:120]
    config = AnalysisConfig(min_observations=60, recommended_observations=400)
    results = analyze_universe(prices, config)
    spreads = pd.DataFrame(5.0, index=prices.index, columns=prices.columns)
    panel = PricePanel(prices=prices, execution_spread_bps=spreads, timestamps=pd.Series())
    run = AnalysisRun(
        input_files=(Path("quotes.parquet"),),
        numeraire="USDC",
        depth_index=0,
        block_start=1,
        block_end=120,
        generated_at="2026-07-11T12:00:00Z",
    )

    paths = write_outputs(tmp_path, panel, results, config, run)

    assert {path.name for path in paths} == {
        "dashboard.html",
        "report.md",
        "results.csv",
        "results.json",
        "run.json",
        "series.parquet",
    }
    assert "plotly" in (tmp_path / "dashboard.html").read_text().lower()
    assert "exploratory" in (tmp_path / "report.md").read_text().lower()
