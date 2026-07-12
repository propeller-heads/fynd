# Fynd pairs cointegration

Research-only statistical screening for block-consistent executable quotes produced by
`pairs-data-collector`. The tool constructs asset prices in a common numeraire, searches all
eligible pairs, controls the false discovery rate, and writes offline reports.

This module identifies hypotheses for later analysis. It does not generate trades or establish
that a pair is profitable.

See [`docs/proposal.md`](docs/proposal.md) for the specification and assumptions.

## Run

From this directory:

```bash
uv sync
uv run pairs-cointegration \
  /path/to/collector-run \
  --output-dir /path/to/results \
  --numeraire USDC \
  --depth-index 0
```

The input can be a collector run directory, a `quote_points` Parquet directory, or one Parquet
file. The default minimum is 60 aligned observations so short collection tests can exercise the
pipeline. Results remain `exploratory` until they reach the recommended 500 observations.

## Outputs

| File | Purpose |
|---|---|
| `dashboard.html` | Self-contained interactive heatmap, sortable table, and top-pair diagnostics |
| `report.md` | Concise methodology, quality warning, and ranked results |
| `results.csv` / `results.json` | Complete pair statistics and decision gates |
| `series.parquet` | Reusable asset midpoint and execution-spread series |
| `run.json` | Input paths, block range, numeraire, depth, thresholds, and generation time |

## Statistical method

1. Keep successful `ladder_forward` quotes at the selected depth. Matched-reverse quotes measure
   round trips and are not independent price observations.
2. Normalize token decimals and pair both executable directions against the numeraire per block.
3. Compute `sqrt(bid * ask)` and retain `(ask / bid - 1) * 10,000` as execution width.
4. Test log levels and first differences with ADF, requiring evidence consistent with I(1).
5. Run Engle-Granger in both orientations. Use the larger p-value so both must support the result.
6. Apply Benjamini-Hochberg correction across every eligible pair in the universe.
7. Fit `log(B) - alpha - beta * log(A)` and report spread ADF, latest z-score, return
   correlation, and AR(1) half-life.

Near-perfect collinearity is reported as numerically untestable. A research candidate must pass
integration, spread-stationarity, bidirectional FDR, and recommended-sample gates. Even then it is
not a trading signal; rolling stability, held-out evaluation, and cost analysis are future work.

## Quality checks

```bash
uv run ruff format --check .
uv run ruff check .
uv run ty check
uv run pytest -q
```

## References

- [Quantopian Lecture 43](https://github.com/quantopian/research_public/tree/master/notebooks/lectures/Integration_Cointegration_and_Stationarity)
- [Quantopian Lectures 44 to 46](https://github.com/quantopian/research_public/tree/master/notebooks/lectures/Introduction_to_Pairs_Trading)
- [statsmodels Engle-Granger API](https://www.statsmodels.org/stable/generated/statsmodels.tsa.stattools.coint.html)
- [statsmodels ADF API](https://www.statsmodels.org/stable/generated/statsmodels.tsa.stattools.adfuller.html)
- [statsmodels multiple-testing API](https://www.statsmodels.org/stable/generated/statsmodels.stats.multitest.multipletests.html)
