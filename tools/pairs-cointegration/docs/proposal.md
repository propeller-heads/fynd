# tldr;

Build a standalone Python research tool that reads `pairs-data-collector` Parquet output,
constructs block-aligned executable asset prices in a common numeraire, searches every asset pair
for cointegration, and emits auditable CSV/JSON/Parquet results plus Markdown and self-contained
HTML reports. The first 97-observation dataset is useful to verify the pipeline only; its findings
must be labeled exploratory.

# Motivation

The collector now produces block-consistent executable quotes. The next step is to turn those
quotes into reproducible statistical evidence without coupling research code to collection or
trading execution.

# Background

The methodology follows Quantopian Lectures 43 through 46: test integration, estimate an OLS
hedge relationship, test residual stationarity with Engle-Granger, inspect standardized spreads,
and search a universe of pairs. The implementation adds bidirectional tests, Benjamini-Hochberg
false-discovery-rate control, explicit data-quality gates, and machine-readable provenance.

# Goal

Provide one command that searches, documents, and displays cointegration results from a completed
collector run.

# Status

DONE on 2026-07-11. The initial run analyzed 55 pairs across 97 blocks and generated all planned
artifacts. Three pairs were exploratory FDR signals; zero met the adequate-sample candidate gate.

# Condition for completion

Given the research collector output, the tool produces validated asset series, analyzes all
eligible pairs, writes deterministic result artifacts, renders an offline HTML dashboard, clearly
labels insufficient samples, and passes tests, linting, formatting, and type checking.

# Specification

## Definitions

- **Executable bid**: numeraire received per asset sold by a successful `ladder_forward` quote.
- **Executable ask**: numeraire spent per asset received by the opposite successful quote.
- **Midpoint**: geometric mean of executable bid and ask at one block and depth.
- **Pair p-value**: the larger of the two Engle-Granger orientation p-values, a conservative rule
  that requires evidence in both regression directions.
- **q-value**: Benjamini-Hochberg adjusted pair p-value across the searched universe.
- **Exploratory result**: any result based on fewer than the recommended observation count.

## Requirements

### Essential

- DONE: Read one Parquet file, a Parquet directory, or a collector run directory.
- DONE: Use successful `ladder_forward` rows only and reject duplicate or non-positive quotes.
- DONE: Construct common-numeraire log midpoint series at a selected depth.
- DONE: Align each candidate independently on common blocks and enforce a configurable minimum.
- DONE: Run ADF tests on levels and first differences for both assets.
- DONE: Run Engle-Granger in both orientations and fit a documented canonical OLS spread.
- DONE: Correct searched pair p-values with Benjamini-Hochberg FDR.
- DONE: Report hedge coefficients, spread ADF, z-score, correlation, half-life, and gates.
- DONE: Write `results.csv`, `results.json`, `series.parquet`, `run.json`, and `report.md`.
- DONE: Render a self-contained `dashboard.html` with warnings, heatmap, result table, and pair
  diagnostics.
- DONE: Fail fast with actionable errors and retain input/configuration provenance.

### Important

- DONE: Keep output deterministic for identical inputs and options.
- DONE: Distinguish statistical significance from a valid trading candidate.
- DONE: Document assumptions, formulas, limitations, and Quantopian references.
- DONE: Unit-test price normalization, midpoint construction, statistics, correction, and reporting.

### Nice-to-have

- Add rolling stability, structural-break, and out-of-sample analysis after more data exists.

## Not included

Signal generation, position sizing, transaction costs, backtesting, live monitoring, and execution.

# Implementation

1. Create `tools/pairs-cointegration` as a Python 3.13 `uv` project with strict quality tooling.
2. Implement typed configuration and collector input discovery.
3. Normalize integer token amounts and build bid/ask/midpoint asset series.
4. Implement pairwise statistical tests and FDR correction with explicit numerical failure states.
5. Serialize artifacts and generate Markdown and offline Plotly HTML reports.
6. Test synthetic stationary/non-stationary cases and the real collector schema.
7. Run the real 97-point dataset with a lowered exploratory minimum and review its dashboard.

# Rationale

A common numeraire creates comparable asset price levels. A direct exchange rate is already a
ratio, so treating every DEX pair quote as a separate asset level would test a different and less
interpretable hypothesis. Midpoints reduce one-sided execution bias while preserving the observed
bid/ask cost as a diagnostic. Requiring both Engle-Granger orientations avoids silently depending
on regression ordering. FDR correction addresses the multiple-comparisons bias created by scanning
all pairs.

# Considerations

- ADF and Engle-Granger tests have weak power in short samples.
- Consecutive Ethereum blocks are high-frequency observations and can be strongly autocorrelated.
- Executable quotes depend on chosen depth, routing state, fees, and gas policy.
- Cointegration can break; later work needs rolling and held-out validation.
- A significant result is a research candidate, not authorization to trade.
- Token addresses are identity; symbols are display labels and are suffixed with an address
  prefix when they collide. An ambiguous numeraire symbol must be given as an address.
- The depth-index midpoint mixes notionals: the bid comes from the token-side ladder amount and
  the ask from the numeraire-side ladder amount, and both are fixed in token units, so the USD
  notional drifts with price. The collector's matched-reverse rows measure the same-notional
  round trip and are not yet used here.
- Amount normalization converts uint256 strings to float64, which is lossy above 2^53 base
  units. That is acceptable for price ratios; never reuse these floats as executable amounts.

# References

- Quantopian Lecture 43, Integration, Cointegration, and Stationarity.
- Quantopian Lectures 44 to 46, Introduction to and examples of pairs trading.
- statsmodels ADF, Engle-Granger cointegration, and multiple-testing APIs.

# Risks

- False discoveries from multiple tests or short histories.
- Misleading prices if token decimals or direction are handled incorrectly.
- Numerical failures for constant, nearly collinear, or sparse series.
- Data snooping if screening and evaluation reuse the same interval.
