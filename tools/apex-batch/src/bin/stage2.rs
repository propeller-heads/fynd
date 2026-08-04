//! Stage 2 of the batching ladder: real APEX on decoded Base trades with ZERO AMMs.
//!
//! Mirrors `docs/analysis/2026-08-base-cow-phase0/cow_scan.py`'s universe exactly — same
//! canonicalization, quarantine, USD estimation, headline restriction, wash-pair handling, and
//! tumbling windows — then, instead of analytic optimal matching, hands each window's orders to
//! `apex_solver::run_apex_with_config` with an empty pool set. The output is "how much of the
//! analytic N-N ceiling does APEX's actual mechanism realize" per window size.
//!
//! Decimals-free by construction: with no pools there is no native-unit simulation anywhere, so
//! every token is declared to APEX with 18 decimals and amounts stay in raw native units. All
//! clearing comparisons are ratios of amounts, all prices are per RAW UNIT, and
//! `remove_extra_precision` is the identity at 18 — the scheme is exact, not an approximation.
//! (From stage 3 on, pools force the real 18-dec lift and the decimals map; not here.)
//!
//! The price scale is derived from the wrapping-overflow bound (grill r3 F1): APEX's objective
//! squares per-token value = amount × price, and `increase_precision` can multiply prices by
//! 10^max_precision_increases, so with P pinned to 2 the batch must satisfy
//! `S × total_usd × 10^P < 2^126`. S is chosen per batch from that bound; tokens whose scaled
//! price would round below MIN_PRICE_UNITS are excluded with their orders, counted.

use std::{
    collections::{BTreeMap, HashMap},
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use apex_batch::dataset::{load_day_headline, u256_to_f64, Intent};
use apex_solver::{
    core::{ApexConfig, Fraction, LimitOrder, Token as ApexToken, TradingPair},
    run_apex_with_config,
    types::{Address as ApexAddress, U256},
};
use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;

const WINDOWS: [u64; 5] = [1, 5, 15, 30, 150];
const LIMIT_BPS_CELLS: [u32; 3] = [50, 100, 200];
/// Overflow bound: S · total_usd · 10^P must stay below 2^126 ≈ 8.5e37, so with P=2 the scale
/// budget is 8.5e35 per USD of batch notional.
const SCALE_BUDGET: f64 = 8.5e35;
const SCALE_CAP: f64 = 1e33;
const MAX_PRECISION_INCREASES: u32 = 2;
const MIN_PRICE_UNITS: f64 = 1e3;
const SOLVE_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about = "APEX orders-only (zero AMM) run over hindsight comparison JSONL")]
struct Args {
    /// Directory of comparisons-YYYY-MM-DD.jsonl files.
    #[arg(
        long,
        default_value = "/Users/pistomat/Projects/propeller-heads/fynd/data/hindsight/base-comparisons"
    )]
    data_dir: PathBuf,
    /// Output directory for results JSON.
    #[arg(long, default_value = "docs/analysis/2026-08-base-cow-phase0/stage2-apex-orders-only")]
    out_dir: PathBuf,
    /// Restrict to these days (YYYY-MM-DD), default all.
    #[arg(long)]
    days: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
struct Counters {
    orders_in: u64,
    filled: u64,
    partially_filled: u64,
    unfilled_at_limit: u64,
    cluster_cut: u64,
    component_errored: u64,
    component_errors: BTreeMap<String, u64>,
    solver_panics: u64,
    deadline_fired_batches: u64,
    singles_skipped: u64,
    wash_orders_excluded: u64,
    token_unpriced: u64,
    price_underflow: u64,
    zero_limit_excluded: u64,
    negative_fill_gaps: u64,
    /// USD magnitude of fills below the settled baseline — the redistribution the uniform
    /// clearing price takes from one side within its limit slack. Net surplus = positive − this.
    negative_gap_usd: f64,
    components_solved: u64,
    components_multi_order: u64,
    /// APEX-filled orders with a recorded fynd quote to compare against.
    fynd_compared: u64,
    /// APEX-filled orders fynd never quoted (unsolvable / missing) — excluded from the
    /// engine-inclusive comparison, never silently.
    fynd_uncompared: u64,
}

impl Counters {
    fn absorb(&mut self, other: &Counters) {
        self.orders_in += other.orders_in;
        self.filled += other.filled;
        self.partially_filled += other.partially_filled;
        self.unfilled_at_limit += other.unfilled_at_limit;
        self.cluster_cut += other.cluster_cut;
        self.component_errored += other.component_errored;
        for (kind, count) in &other.component_errors {
            *self
                .component_errors
                .entry(kind.clone())
                .or_default() += count;
        }
        self.solver_panics += other.solver_panics;
        self.deadline_fired_batches += other.deadline_fired_batches;
        self.singles_skipped += other.singles_skipped;
        self.wash_orders_excluded += other.wash_orders_excluded;
        self.token_unpriced += other.token_unpriced;
        self.price_underflow += other.price_underflow;
        self.zero_limit_excluded += other.zero_limit_excluded;
        self.negative_fill_gaps += other.negative_fill_gaps;
        self.negative_gap_usd += other.negative_gap_usd;
        self.components_solved += other.components_solved;
        self.components_multi_order += other.components_multi_order;
        self.fynd_compared += other.fynd_compared;
        self.fynd_uncompared += other.fynd_uncompared;
    }
}

/// Engine-inclusive comparison: APEX clearings vs the dataset's per-trade fynd N−1 quotes
/// (`top.fynd_amount_out`), on the subset of APEX-filled orders fynd also quoted. Positive bps
/// = APEX delivered more than fynd's individual quote (plan item L).
#[derive(Default, Clone, Serialize)]
struct FyndComparison {
    compared_orders: u64,
    apex_ge_fynd_share: f64,
    mean_bps: f64,
    median_bps: f64,
    usd_delta: f64,
}

#[derive(Default, Clone, Serialize)]
struct CellResult {
    window_blocks: u64,
    limit_bps: u32,
    intent_usd: f64,
    apex_matched_usd: f64,
    apex_matched_pct: f64,
    apex_surplus_usd: f64,
    fynd: FyndComparison,
    counters: Counters,
    wall_ms: u128,
}

/// Solve one window batch (one tumbling window's headline intents) through APEX with zero
/// pools, one call per shared-token connected component.
fn solve_batch(
    intents: &[Intent],
    day_price: &HashMap<ApexAddress, f64>,
    limit_bps: u32,
) -> (f64, f64, Counters, Vec<f64>, f64) {
    let mut counters = Counters::default();
    let mut matched_usd = 0.0f64;
    let mut surplus_usd = 0.0f64;
    let mut fynd_bps_samples: Vec<f64> = Vec::new();
    let mut fynd_usd_delta = 0.0f64;

    // Order-level exclusions first: wash, unpriced tokens, dust limits.
    let mut orders: Vec<&Intent> = Vec::with_capacity(intents.len());
    for intent in intents {
        counters.orders_in += 1;
        if intent.is_wash {
            counters.wash_orders_excluded += 1;
            continue;
        }
        if !day_price.contains_key(&intent.token_in) || !day_price.contains_key(&intent.token_out) {
            counters.token_unpriced += 1;
            continue;
        }
        orders.push(intent);
    }
    if orders.len() < 2 {
        counters.singles_skipped += orders.len() as u64;
        return (0.0, 0.0, counters, fynd_bps_samples, fynd_usd_delta);
    }

    // Shared-token connected components (no pools, so components ARE apex's clusters).
    let mut parent: Vec<usize> = (0..orders.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut token_owner: HashMap<ApexAddress, usize> = HashMap::new();
    for (index, order) in orders.iter().enumerate() {
        for token in [order.token_in, order.token_out] {
            match token_owner.entry(token) {
                std::collections::hash_map::Entry::Occupied(seen) => {
                    let a = find(&mut parent, index);
                    let b = find(&mut parent, *seen.get());
                    parent[a] = b;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(index);
                }
            }
        }
    }
    let mut components: HashMap<usize, Vec<&Intent>> = HashMap::new();
    for (index, order) in orders.iter().enumerate() {
        components
            .entry(find(&mut parent, index))
            .or_default()
            .push(order);
    }

    for component in components.into_values() {
        if component.len() < 2 {
            counters.singles_skipped += 1;
            continue;
        }
        counters.components_multi_order += 1;

        // Per-component price scale from the overflow bound.
        let total_usd: f64 = component.iter().map(|o| o.usd).sum();
        let scale = (SCALE_BUDGET / total_usd.max(1.0)).min(SCALE_CAP);
        assert!(
            scale * total_usd * 10f64.powi(MAX_PRECISION_INCREASES as i32) < 2f64.powi(126),
            "scale bound violated: S={scale:e} total=${total_usd:e}"
        );

        // Price every token; a token whose scaled price rounds below the floor drops its orders.
        let mut component_tokens: HashMap<ApexAddress, ApexToken> = HashMap::new();
        let mut prices: HashMap<ApexAddress, U256> = HashMap::new();
        let mut underflow_tokens: std::collections::HashSet<ApexAddress> = Default::default();
        for order in &component {
            for token in [order.token_in, order.token_out] {
                if component_tokens.contains_key(&token) || underflow_tokens.contains(&token) {
                    continue;
                }
                let usd_per_raw = day_price[&token];
                let units = scale * usd_per_raw;
                if !units.is_finite() || units < MIN_PRICE_UNITS {
                    underflow_tokens.insert(token);
                    continue;
                }
                // Fits u128: units ≤ S·usd_per_raw and every kept token also appears in an
                // order, whose whole-order value S·usd is already bounded by 8.5e35 < 2^127.
                prices.insert(token, U256::from(units as u128));
                component_tokens.insert(
                    token,
                    ApexToken::new(token, &format!("T{:02x}{:02x}", token.0[18], token.0[19]), 18),
                );
            }
        }

        let mut limit_orders: HashMap<(ApexAddress, ApexAddress), Vec<LimitOrder>> = HashMap::new();
        let mut order_inputs: HashMap<String, &Intent> = HashMap::new();
        for order in &component {
            if underflow_tokens.contains(&order.token_in) ||
                underflow_tokens.contains(&order.token_out)
            {
                counters.price_underflow += 1;
                continue;
            }
            // Synthetic floor: settled × (1 − bps). Zero floors carry no commitment (and a zero
            // denominator would poison Fraction ordering), so dust declines.
            let min_out =
                order.settled_out * U256::from(10_000 - limit_bps as u64) / U256::from(10_000u64);
            if min_out.is_zero() {
                counters.zero_limit_excluded += 1;
                continue;
            }
            assert!(
                order_inputs
                    .insert(order.id.clone(), order)
                    .is_none(),
                "duplicate order id {} in one batch",
                order.id
            );
            let pair = TradingPair::new(
                component_tokens[&order.token_in],
                component_tokens[&order.token_out],
            );
            limit_orders
                .entry(pair.addresses())
                .or_default()
                .push(LimitOrder::new(
                    order.amount_in,
                    Fraction::new(min_out, order.amount_in),
                    order.id.clone(),
                    ApexAddress([0u8; 20]),
                ));
        }
        if order_inputs.len() < 2 {
            counters.singles_skipped += order_inputs.len() as u64;
            continue;
        }

        let tokens: Vec<ApexToken> = component_tokens
            .values()
            .copied()
            .collect();
        // PriceSearchConfig is not re-exported, so the nested field is set by mutation.
        let mut config = ApexConfig {
            enable_two_hops: false,
            max_workers: 1,
            collect_metrics: false,
            deadline: Some(Instant::now() + SOLVE_DEADLINE),
            ..ApexConfig::default()
        };
        config
            .price_search_config
            .max_precision_increases = MAX_PRECISION_INCREASES;

        counters.components_solved += 1;
        let solve = catch_unwind(AssertUnwindSafe(|| {
            run_apex_with_config(
                tokens,
                prices.clone(),
                limit_orders.clone(),
                HashMap::new(),
                Vec::new(),
                config,
            )
        }));
        let result = match solve {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                counters.component_errored += order_inputs.len() as u64;
                let kind = match &error {
                    apex_solver::core::ApexError::InvalidInput(_) => "invalid_input",
                    apex_solver::core::ApexError::MetricsCollectionError(_) => "metrics",
                    apex_solver::core::ApexError::TradeSolverError(_) => "trade_solver",
                    apex_solver::core::ApexError::MarketRouterError(_) => "market_router",
                    apex_solver::core::ApexError::ClearingUnderLimitPrice(_, _) => {
                        "clearing_under_limit"
                    }
                    apex_solver::core::ApexError::NegativeBalanceDelta(_, _) => {
                        "negative_balance_delta"
                    }
                    _ => "other",
                };
                *counters
                    .component_errors
                    .entry(kind.to_string())
                    .or_default() += 1;
                continue;
            }
            Err(_) => {
                counters.solver_panics += 1;
                counters.component_errored += order_inputs.len() as u64;
                continue;
            }
        };
        if result.deadline_fired {
            counters.deadline_fired_batches += 1;
        }

        let clearings: HashMap<&str, _> = result
            .limit_order_clearings
            .iter()
            .map(|c| (c.id.as_str(), c))
            .collect();
        for (id, order) in &order_inputs {
            match clearings.get(id.as_str()) {
                Some(clearing) if !clearing.sold_amount.is_zero() => {
                    let fill_ratio =
                        u256_to_f64(clearing.sold_amount) / u256_to_f64(order.amount_in);
                    if fill_ratio < 1.0 - 1e-9 {
                        counters.partially_filled += 1;
                    } else {
                        counters.filled += 1;
                    }
                    matched_usd += order.usd * fill_ratio.min(1.0);
                    // Surplus vs the settled baseline, mirroring the analytic convention:
                    // positive gaps only, negatives counted apart.
                    let settled_pro_rata = u256_to_f64(order.settled_out) * fill_ratio.min(1.0);
                    let gap_raw = u256_to_f64(clearing.bought_amount) - settled_pro_rata;
                    if gap_raw > 0.0 {
                        surplus_usd += gap_raw * day_price[&order.token_out];
                    } else if gap_raw < 0.0 {
                        counters.negative_fill_gaps += 1;
                        counters.negative_gap_usd += -gap_raw * day_price[&order.token_out];
                    }
                    // Engine-inclusive baseline: the same fill against fynd's own N−1 quote,
                    // pro-rata for partial fills (plan item L).
                    match order.fynd_out {
                        Some(fynd_out) if !fynd_out.is_zero() => {
                            counters.fynd_compared += 1;
                            let fynd_pro_rata = u256_to_f64(fynd_out) * fill_ratio.min(1.0);
                            let apex_out = u256_to_f64(clearing.bought_amount);
                            let relative_gap = (apex_out - fynd_pro_rata) / fynd_pro_rata;
                            fynd_bps_samples.push(10_000.0 * relative_gap);
                            // Valued against the order's own quarantined USD notional — re-pricing
                            // raw units lets one wrong-decimals quote dominate the whole sum.
                            fynd_usd_delta += relative_gap * order.usd * fill_ratio.min(1.0);
                        }
                        _ => counters.fynd_uncompared += 1,
                    }
                }
                Some(_) => counters.unfilled_at_limit += 1,
                None if result.deadline_fired => counters.cluster_cut += 1,
                None => counters.unfilled_at_limit += 1,
            }
        }
    }
    (matched_usd, surplus_usd, counters, fynd_bps_samples, fynd_usd_delta)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut day_files: Vec<PathBuf> = std::fs::read_dir(&args.data_dir)
        .with_context(|| format!("listing {}", args.data_dir.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e == "jsonl")
        })
        .filter(|p| {
            args.days.is_empty() ||
                args.days
                    .iter()
                    .any(|d| p.to_string_lossy().contains(d.as_str()))
        })
        .collect();
    day_files.sort();
    if day_files.is_empty() {
        bail!("no day files matched in {}", args.data_dir.display());
    }

    eprintln!("loading {} day files…", day_files.len());
    let days: Vec<(Vec<Intent>, HashMap<ApexAddress, f64>)> = day_files
        .par_iter()
        .map(|path| load_day_headline(path))
        .collect::<Result<_>>()?;
    let total_intents: usize = days.iter().map(|(i, _)| i.len()).sum();
    eprintln!("headline intents: {total_intents}");

    let mut cells: Vec<CellResult> = Vec::new();
    for window in WINDOWS {
        // Intent volume denominator: all headline intents including the wash pair, as in the
        // analytic scan's w_usd.
        let intent_usd: f64 = days
            .iter()
            .flat_map(|(intents, _)| intents.iter())
            .map(|i| i.usd)
            .sum();
        for limit_bps in LIMIT_BPS_CELLS {
            let started = Instant::now();
            let batch_results: Vec<(f64, f64, Counters, Vec<f64>, f64)> = days
                .par_iter()
                .flat_map(|(intents, day_price)| {
                    let mut by_window: BTreeMap<u64, Vec<Intent>> = BTreeMap::new();
                    for intent in intents {
                        by_window
                            .entry(intent.block / window)
                            .or_default()
                            .push(intent.clone());
                    }
                    by_window
                        .into_values()
                        .collect::<Vec<_>>()
                        .into_par_iter()
                        .map(|batch| solve_batch(&batch, day_price, limit_bps))
                })
                .collect();
            let mut counters = Counters::default();
            let mut matched = 0.0f64;
            let mut surplus = 0.0f64;
            let mut bps_samples: Vec<f64> = Vec::new();
            let mut fynd_usd_delta = 0.0f64;
            for (m, s, c, bps, delta) in &batch_results {
                matched += m;
                surplus += s;
                counters.absorb(c);
                bps_samples.extend_from_slice(bps);
                fynd_usd_delta += delta;
            }
            bps_samples.sort_by(|a, b| a.partial_cmp(b).expect("finite bps"));
            let fynd = FyndComparison {
                compared_orders: counters.fynd_compared,
                apex_ge_fynd_share: if bps_samples.is_empty() {
                    0.0
                } else {
                    bps_samples
                        .iter()
                        .filter(|b| **b >= 0.0)
                        .count() as f64 /
                        bps_samples.len() as f64
                },
                mean_bps: if bps_samples.is_empty() {
                    0.0
                } else {
                    bps_samples.iter().sum::<f64>() / bps_samples.len() as f64
                },
                median_bps: if bps_samples.is_empty() {
                    0.0
                } else {
                    bps_samples[bps_samples.len() / 2]
                },
                usd_delta: fynd_usd_delta,
            };
            let wall_ms = started.elapsed().as_millis();
            eprintln!(
                "w={window:>3} bps={limit_bps:>3}: matched=${matched:>12.0} \
                 ({:.3}%) surplus=${surplus:>10.2} filled={} errors={:?} wall={wall_ms}ms",
                100.0 * matched / intent_usd,
                counters.filled + counters.partially_filled,
                counters.component_errors,
            );
            eprintln!(
                "         fynd baseline: compared={} apex>=fynd {:.1}% median={:+.1}bps \
                 mean={:+.1}bps delta=${:+.0}",
                fynd.compared_orders,
                100.0 * fynd.apex_ge_fynd_share,
                fynd.median_bps,
                fynd.mean_bps,
                fynd.usd_delta,
            );
            cells.push(CellResult {
                window_blocks: window,
                limit_bps,
                intent_usd,
                apex_matched_usd: matched,
                apex_matched_pct: 100.0 * matched / intent_usd,
                apex_surplus_usd: surplus,
                fynd,
                counters,
                wall_ms,
            });
        }
    }

    std::fs::create_dir_all(&args.out_dir)?;
    let out_path = args.out_dir.join("stage2_results.json");
    std::fs::write(&out_path, serde_json::to_vec_pretty(&cells)?)?;
    eprintln!("wrote {}", out_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_two_crossing_orders_fill_through_apex() {
        // A sells 100 A-units for B (settled 200), B sells 200 B-units for A (settled 100) —
        // a perfect cross. With a 100 bps synthetic floor both fill at any clearing price
        // between the floors.
        let token_a = ApexAddress([1u8; 20]);
        let token_b = ApexAddress([2u8; 20]);
        let intents = vec![
            Intent {
                block: 1,
                token_in: token_a,
                token_out: token_b,
                amount_in: U256::from(100_000_000u64),
                settled_out: U256::from(200_000_000u64),
                usd: 100.0,
                id: "0xaa:1".to_string(),
                is_wash: false,
                fynd_out: None,
            },
            Intent {
                block: 1,
                token_in: token_b,
                token_out: token_a,
                amount_in: U256::from(200_000_000u64),
                settled_out: U256::from(100_000_000u64),
                usd: 100.0,
                id: "0xbb:2".to_string(),
                is_wash: false,
                fynd_out: None,
            },
        ];
        let day_price =
            HashMap::from([(token_a, 100.0 / 100_000_000.0), (token_b, 100.0 / 200_000_000.0)]);
        let (matched, _surplus, counters, _bps, _delta) = solve_batch(&intents, &day_price, 100);
        assert_eq!(counters.filled + counters.partially_filled, 2, "{counters:?}");
        assert!(matched > 190.0, "both sides counted: {matched}");
        assert_eq!(counters.component_errored, 0);
        assert_eq!(counters.solver_panics, 0);
    }

    #[test]
    fn test_disconnected_orders_are_separate_components_and_singles_skip() {
        let intents = vec![
            Intent {
                block: 1,
                token_in: ApexAddress([1u8; 20]),
                token_out: ApexAddress([2u8; 20]),
                amount_in: U256::from(100u64),
                settled_out: U256::from(100u64),
                usd: 10.0,
                id: "0xaa:1".to_string(),
                is_wash: false,
                fynd_out: None,
            },
            Intent {
                block: 1,
                token_in: ApexAddress([3u8; 20]),
                token_out: ApexAddress([4u8; 20]),
                amount_in: U256::from(100u64),
                settled_out: U256::from(100u64),
                usd: 10.0,
                id: "0xbb:2".to_string(),
                is_wash: false,
                fynd_out: None,
            },
        ];
        let day_price: HashMap<ApexAddress, f64> = [1u8, 2, 3, 4]
            .into_iter()
            .map(|b| (ApexAddress([b; 20]), 0.1))
            .collect();
        let (matched, surplus, counters, _bps, _delta) = solve_batch(&intents, &day_price, 100);
        assert_eq!(matched, 0.0);
        assert_eq!(surplus, 0.0);
        assert_eq!(counters.singles_skipped, 2);
        assert_eq!(counters.components_solved, 0);
    }
}
