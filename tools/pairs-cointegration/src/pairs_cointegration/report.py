"""Serialize analysis artifacts and render offline reports."""

import json
from dataclasses import asdict
from html import escape
from pathlib import Path
from typing import Any

import numpy as np
import pandas as pd
import plotly.graph_objects as go
import plotly.io as pio
from plotly.subplots import make_subplots

from pairs_cointegration.data import PricePanel
from pairs_cointegration.models import AnalysisConfig, AnalysisRun, PairResult

TOP_RESULT_COUNT = 20
TOP_PLOT_COUNT = 10


def _json_value(value: Any) -> Any:  # noqa: ANN401
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, float) and not np.isfinite(value):
        return None
    if isinstance(value, tuple):
        return [_json_value(item) for item in value]
    if isinstance(value, dict):
        return {key: _json_value(item) for key, item in value.items()}
    return value


def _result_records(results: list[PairResult]) -> list[dict[str, Any]]:
    return [_json_value(asdict(result)) for result in results]


def _heatmap(results: list[PairResult], assets: list[str]) -> go.Figure:
    values = np.full((len(assets), len(assets)), np.nan)
    positions = {asset: index for index, asset in enumerate(assets)}
    for result in results:
        i, j = positions[result.asset_a], positions[result.asset_b]
        values[i, j] = values[j, i] = result.q_value
    figure = go.Figure(
        go.Heatmap(
            z=values,
            x=assets,
            y=assets,
            zmin=0,
            zmax=0.1,
            colorscale="RdYlGn_r",
            colorbar={"title": "BH q-value"},
            hovertemplate="%{y} / %{x}<br>q=%{z:.4g}<extra></extra>",
        )
    )
    figure.update_layout(title="Cointegration search heatmap", height=560, template="plotly_white")
    return figure


def _pair_figure(result: PairResult, prices: pd.DataFrame) -> go.Figure:
    aligned = prices[[result.asset_a, result.asset_b]].dropna()
    log_a = np.log(aligned[result.asset_a])
    log_b = np.log(aligned[result.asset_b])
    spread = log_b - result.alpha - result.beta * log_a
    z_score = (spread - spread.mean()) / spread.std(ddof=1)
    figure = make_subplots(rows=2, cols=1, shared_xaxes=True, vertical_spacing=0.12)
    figure.add_trace(
        go.Scatter(x=aligned.index, y=log_a, name=f"log {result.asset_a}"), row=1, col=1
    )
    figure.add_trace(
        go.Scatter(x=aligned.index, y=log_b, name=f"log {result.asset_b}"), row=1, col=1
    )
    figure.add_trace(go.Scatter(x=aligned.index, y=z_score, name="spread z-score"), row=2, col=1)
    figure.add_hline(y=0, line_color="#64748b", row=2, col=1)
    figure.update_layout(
        title=f"{result.asset_a} / {result.asset_b}, q={result.q_value:.4g}",
        height=600,
        template="plotly_white",
    )
    return figure


def _table(results: list[PairResult]) -> str:
    rows = []
    for result in results:
        half_life = "" if result.half_life_blocks is None else f"{result.half_life_blocks:.2f}"
        rows.append(
            "<tr>"
            f"<td>{escape(result.asset_a)}</td><td>{escape(result.asset_b)}</td>"
            f"<td>{result.n_observations}</td><td>{result.pair_p_value:.4g}</td>"
            f"<td>{result.q_value:.4g}</td><td>{result.spread_adf_p:.4g}</td>"
            f"<td>{result.latest_z_score:.3f}</td><td>{half_life}</td>"
            f"<td>{result.integration_gate}</td><td>{result.fdr_significant}</td>"
            f"<td>{result.research_candidate}</td>"
            f"<td>{escape(result.sample_quality)}</td></tr>"
        )
    headings = (
        "Asset A",
        "Asset B",
        "N",
        "pair p",
        "BH q",
        "spread ADF p",
        "latest z",
        "half-life",
        "I(1) gate",
        "FDR signal",
        "candidate",
        "sample",
    )
    header = "".join(
        f"<th onclick='sortTable(this.cellIndex)'>{heading}</th>" for heading in headings
    )
    return (
        f"<table id='results'><thead><tr>{header}</tr></thead>"
        f"<tbody>{''.join(rows)}</tbody></table>"
    )


def _styles() -> str:
    return """<style>
body { font: 15px system-ui; margin: 0; background: #f8fafc; color: #172033; }
main { max-width: 1200px; margin: auto; padding: 32px; }
.warning { background: #fff7ed; border-left: 5px solid #f97316; padding: 16px; }
.cards { display: flex; gap: 12px; margin: 20px 0; }
.card { background: white; padding: 16px; border-radius: 10px; box-shadow: 0 1px 4px #ccd; }
table { border-collapse: collapse; width: 100%; background: white; }
th, td { padding: 8px; border-bottom: 1px solid #ddd; text-align: right; }
th { cursor: pointer; background: #102a43; color: white; }
th:first-child, th:nth-child(2), td:first-child, td:nth-child(2) { text-align: left; }
input { padding: 10px; width: 300px; margin: 12px 0; }
</style>"""


def _scripts() -> str:
    return """<script>
function filterRows() {
  const query = document.getElementById('filter').value.toLowerCase();
  for (const row of document.querySelectorAll('#results tbody tr')) {
    row.style.display = row.innerText.toLowerCase().includes(query) ? '' : 'none';
  }
}
function sortTable(column) {
  const table = document.getElementById('results');
  const body = table.tBodies[0];
  const rows = [...body.rows];
  const ascending = table.dataset.sort != column || table.dataset.dir != 'asc';
  rows.sort((left, right) => {
    const a = left.cells[column].innerText;
    const b = right.cells[column].innerText;
    const numericA = Number(a);
    const numericB = Number(b);
    const comparison = Number.isNaN(numericA) || Number.isNaN(numericB)
      ? a.localeCompare(b) : numericA - numericB;
    return comparison * (ascending ? 1 : -1);
  });
  rows.forEach((row) => body.appendChild(row));
  table.dataset.sort = column;
  table.dataset.dir = ascending ? 'asc' : 'desc';
}
</script>"""


def _dashboard(
    prices: pd.DataFrame,
    results: list[PairResult],
    config: AnalysisConfig,
    run: AnalysisRun,
) -> str:
    heatmap = pio.to_html(
        _heatmap(results, sorted(prices.columns)), full_html=False, include_plotlyjs="inline"
    )
    pair_plots = "".join(
        pio.to_html(_pair_figure(result, prices), full_html=False, include_plotlyjs=False)
        for result in results[:TOP_PLOT_COUNT]
    )
    candidates = sum(result.research_candidate for result in results)
    signals = sum(result.fdr_significant for result in results)
    maximum_observations = max(result.n_observations for result in results)
    warning = (
        f"This run has at most {maximum_observations} observations per pair. "
        f"Results below {config.recommended_observations} are exploratory and are not "
        "trade evidence."
    )
    cards = (
        f'<div class="cards"><div class="card"><b>{len(prices.columns)}</b><br>assets</div>'
        f'<div class="card"><b>{len(results)}</b><br>pairs tested</div>'
        f'<div class="card"><b>{signals}</b><br>exploratory FDR signals</div>'
        f'<div class="card"><b>{candidates}</b><br>adequate-sample candidates</div>'
        f'<div class="card"><b>{run.block_start}-{run.block_end}</b><br>blocks</div></div>'
    )
    return "".join(
        [
            '<!doctype html><html><head><meta charset="utf-8">',
            "<title>Fynd cointegration results</title>",
            _styles(),
            "</head><body><main><h1>Fynd cointegration search</h1>",
            f'<p class="warning">{escape(warning)}</p>',
            cards,
            heatmap,
            '<h2>All pair results</h2><input id="filter" onkeyup="filterRows()" ',
            'placeholder="Filter assets or status">',
            _table(results),
            "<h2>Top pair diagnostics</h2>",
            pair_plots,
            _scripts(),
            "</main></body></html>",
        ]
    )


def _result_row(result: PairResult) -> str:
    return (
        f"| {result.asset_a}/{result.asset_b} | {result.n_observations} | "
        f"{result.pair_p_value:.4g} | {result.q_value:.4g} | {result.spread_adf_p:.4g} | "
        f"{result.latest_z_score:.3f} | {result.integration_gate} | {result.fdr_significant} | "
        f"{result.research_candidate} |"
    )


def _markdown(results: list[PairResult], config: AnalysisConfig, run: AnalysisRun) -> str:
    candidates = [result for result in results if result.research_candidate]
    signals = [result for result in results if result.fdr_significant]
    lines = [
        "# Fynd cointegration search",
        "",
        "## Status",
        "",
        (
            f"Tested {len(results)} pairs from blocks {run.block_start} to {run.block_end} "
            f"at depth {run.depth_index}."
        ),
        (
            f"Found {len(signals)} BH FDR signals and {len(candidates)} adequate-sample research "
            "candidates."
        ),
        "",
        "## Data-quality warning",
        "",
        (
            f"The recommended minimum is {config.recommended_observations} aligned observations. "
            "Results below that threshold are exploratory and must not be used as trade evidence."
        ),
        "",
        "## Top results",
        "",
        "| Pair | N | Pair p | BH q | Spread ADF p | Latest z | I(1) | FDR | Candidate |",
        "|---|---:|---:|---:|---:|---:|---|---|---|",
    ]
    lines.extend(_result_row(result) for result in results[:TOP_RESULT_COUNT])
    lines.extend(
        [
            "",
            "## Method",
            "",
            (
                "Prices are geometric midpoints of executable bid and ask quotes against the "
                "selected numeraire. Each pair is tested in both Engle-Granger orientations; the "
                "larger p-value is corrected across the universe with Benjamini-Hochberg. The "
                "canonical spread is `log(B) - alpha - beta * log(A)`."
            ),
            "",
            "## Interpretation",
            "",
            (
                "A research candidate is only a statistical hypothesis. It still requires longer "
                "histories, rolling stability, held-out validation, cost modeling, and a separate "
                "trading design."
            ),
            "",
        ]
    )
    return "\n".join(lines)


def _series_frame(panel: PricePanel) -> pd.DataFrame:
    prices = (
        panel.prices.rename_axis("block_number")
        .reset_index()
        .melt(id_vars="block_number", var_name="asset", value_name="price")
    )
    spreads = (
        panel.execution_spread_bps.rename_axis("block_number")
        .reset_index()
        .melt(id_vars="block_number", var_name="asset", value_name="execution_spread_bps")
    )
    return prices.merge(spreads, on=["block_number", "asset"], validate="one_to_one")


def write_outputs(
    output_dir: Path,
    panel: PricePanel,
    results: list[PairResult],
    config: AnalysisConfig,
    run: AnalysisRun,
) -> list[Path]:
    """Write machine-readable results, documentation, and an offline dashboard."""
    if not results:
        msg = "no eligible pair results to report"
        raise ValueError(msg)
    output_dir.mkdir(parents=True, exist_ok=True)
    records = _result_records(results)
    pd.DataFrame(records).to_csv(output_dir / "results.csv", index=False)
    (output_dir / "results.json").write_text(json.dumps(records, indent=2) + "\n")
    run_record = _json_value(
        {
            "run": asdict(run),
            "config": asdict(config),
            "asset_addresses": dict(panel.asset_addresses),
        }
    )
    (output_dir / "run.json").write_text(json.dumps(run_record, indent=2) + "\n")
    _series_frame(panel).to_parquet(output_dir / "series.parquet", index=False)
    (output_dir / "report.md").write_text(_markdown(results, config, run))
    (output_dir / "dashboard.html").write_text(_dashboard(panel.prices, results, config, run))
    names = (
        "dashboard.html",
        "report.md",
        "results.csv",
        "results.json",
        "run.json",
        "series.parquet",
    )
    return [output_dir / name for name in names]
