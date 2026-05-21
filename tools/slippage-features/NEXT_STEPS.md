# Slippage Feature Collection — Next Steps

## Phase 1: Data Collection (immediate)

### Deploy and run for >= 1 week

1. Deploy Fynd with `--features slippage-features` on a stable instance
   (Ethereum mainnet first, then Base)
2. Run the quote driver with the 10k trade dataset on a 12-second interval
3. Monitor for Tycho disconnections — restart if needed (data on disk is safe)
4. Target: >= 1 week of continuous data across different market conditions
   (weekdays, weekends, high-vol events)

### Validate data quality during collection

- Check hop decay parquet files are being produced (~100 per hour at 10 trades/block)
- Verify `requote_amount_out` fill rate (should be 50-70%)
- Verify `fee_tier` is populated (not NaN)
- Check for temporal gaps using the assembly binary's gap detection

## Phase 2: Exploratory Analysis (ENG-5993)

### Run the assembly pipeline

```bash
COINGECKO_API_KEY="..." cargo run -p slippage-features --release --bin assemble -- \
  --quote-log-dir ./slippage-data \
  --hop-decay-dir ./slippage-data/hop_decay \
  --route-decay-dir ./slippage-data/route_decay \
  --output-dir ./slippage-data/unified
```

### Build the analysis notebook

Key questions to answer:

1. **What features predict decay most?**
   - Univariate Spearman correlation for each feature vs `route_decay_bps`
   - SHAP importance from LightGBM/XGBoost baseline
   - Top-20 features ranked

2. **Market movement vs execution slippage breakdown**
   - What fraction of total decay is market movement (unavoidable)?
   - What fraction is execution slippage (route-specific, fixable)?
   - Does this vary by pair type, pool type, trade size?

3. **Tycho sim vs node eth_call cross-validation**
   - How accurate is Tycho simulation vs ground truth?
   - Which pool types have the largest simulation error?
   - Can we use Tycho sim confidence as a feature?

4. **Which routes revert most?**
   - Identify pair types, pool types, trade sizes with highest decay
   - Manual review of 10+ high-decay routes
   - Common patterns (low TVL? multi-hop? specific protocols?)

5. **Solver comparison** (if multi-solver data available)
   - Do different solver algorithms produce routes with different decay profiles?
   - Cross-solver comparison via `request_id` joins

6. **Chain comparison**
   - Ethereum vs Base decay profiles
   - Does Base's faster block time affect decay?

### Recommended tools

- **polars** for fast parquet reading and feature engineering
- **LightGBM** for baseline gradient boosting
- **SHAP** for feature importance
- **matplotlib/plotly** for visualization

## Phase 3: Improvements Based on Findings

### Fix the top revert causes

Based on Phase 2 findings, implement targeted fixes:

- If high `execution_slippage_bps` correlates with specific pool types → improve
  simulation accuracy for those pools
- If multi-hop routes decay more → consider route staleness timeout
- If large trades decay more → implement trade-size-aware slippage margins
- If specific tokens are problematic → add to blocklist or special-case handling

### Populate remaining features

- **marginal_liquidity**: implement v3/v4 tick-level liquidity reading from
  `ProtocolSim` — this captures how concentrated the liquidity is around the
  current price, which directly predicts slippage for large trades
- **concentration_gini**: compute Gini coefficient across initialized ticks
- **CEX dynamics** (ENG-5990): realized vol, CEX-DEX spread — strongest
  external predictor per the literature
- **Onchain flow** (ENG-5991): OFI, VWAP deviation — captures directional pressure

### Build the prediction model

Once features are validated:

1. Train a calibrated model (conformalized quantile regression recommended)
2. Predict P50/P90/P99 of route decay at quote time
3. Use predictions to:
   - Set tighter/wider slippage margins per route
   - Rank routes by expected reliability, not just output
   - Flag high-revert-risk quotes before submission

## Phase 4: Production Integration

### Merge findings to main

The `explore/slippage-features` branch is intentionally isolated. When the
research validates specific improvements:

1. Extract the proven fixes into focused PRs against main
2. The SolverObserver trait can be promoted to a permanent feature
3. Slippage prediction model can be integrated into the quote path
4. Data collection can become an always-on telemetry system

### Continuous monitoring

- Run the quote driver + resim as a permanent sidecar
- Dashboard tracking revert rate, decay distribution, feature drift
- Alerting when decay patterns change (new pool types, MEV shifts)

## Technical Debt

- **Router address hardcoded**: should be fetched from `/v1/info` at startup
  or passed dynamically from the quote log metadata
- **Re-quote HTTP call**: could be replaced with direct in-process solver call
  (saves HTTP overhead, more reliable)
- **Old quote log files**: the data directory accumulates files forever. Add
  rotation/retention policy
- **CoinGecko rate limiting**: the assembly binary makes API calls per-token
  without rate limiting. Add backoff for large datasets
- **Base chain support**: need to run a separate Fynd instance for Base and
  merge the datasets in assembly
