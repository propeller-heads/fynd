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
use apex_solver::{
    core::{ApexConfig, Fraction, LimitOrder, Token as ApexToken, TradingPair},
    run_apex_with_config,
    types::{Address as ApexAddress, U256},
};
use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;

const WETH: &str = "0x4200000000000000000000000000000000000006";
const NATIVE: &str = "0x0000000000000000000000000000000000000000";
const WINDOWS: [u64; 5] = [1, 5, 15, 30, 150];
const LIMIT_BPS_CELLS: [u32; 3] = [50, 100, 200];
const PRICE_DEV_FACTOR: f64 = 5.0;
const USD_CAP: f64 = 10_000_000.0;
/// Sender-verified self-trading pair (cow_scan WASH_PAIRS); its orders never enter APEX but the
/// pair's volume stays in the intent denominator, mirroring the analytic scan.
const WASH_PAIR: (&str, &str) =
    ("0x3c5cd672b204ba0fc48e93b98c0922920a87912d", "0x3d66e6fe9a3cf698db5af3d70830b299c9235151");
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

/// One netted intent from the headline universe, in raw native units.
#[derive(Clone)]
struct Intent {
    block: u64,
    token_in: ApexAddress,
    token_out: ApexAddress,
    amount_in: U256,
    settled_out: U256,
    usd: f64,
    id: String,
    is_wash: bool,
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
    }
}

#[derive(Default, Clone, Serialize)]
struct CellResult {
    window_blocks: u64,
    limit_bps: u32,
    intent_usd: f64,
    apex_matched_usd: f64,
    apex_matched_pct: f64,
    apex_surplus_usd: f64,
    counters: Counters,
    wall_ms: u128,
}

fn parse_address(token: &str) -> Option<ApexAddress> {
    let hex = token.strip_prefix("0x")?;
    if hex.len() != 40 {
        return None;
    }
    let mut bytes = [0u8; 20];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(ApexAddress(bytes))
}

fn parse_u256_decimal(digits: &str) -> Option<U256> {
    let mut value = U256::ZERO;
    let ten = U256::from(10u64);
    for c in digits.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(ten)?
            .checked_add(U256::from((c - b'0') as u64))?;
    }
    Some(value)
}

fn u256_to_f64(value: U256) -> f64 {
    let mut result = 0.0f64;
    for limb in value.as_limbs().iter().rev() {
        result = result * 1.8446744073709552e19 + *limb as f64;
    }
    result
}

/// Mirror of cow_scan's `load_day` + `classify`: canonicalize, quarantine, USD-estimate, and
/// keep the headline (both-tokens-routable) slice. Returns intents plus the day's per-token
/// median price in USD per RAW UNIT — the same decimals-free price the analytic scan uses.
fn load_day_headline(path: &std::path::Path) -> Result<(Vec<Intent>, HashMap<ApexAddress, f64>)> {
    let wash_a = parse_address(WASH_PAIR.0).expect("static wash address parses");
    let wash_b = parse_address(WASH_PAIR.1).expect("static wash address parses");
    let weth = parse_address(WETH).expect("static WETH address parses");

    struct Raw {
        block: u64,
        token_in: ApexAddress,
        token_out: ApexAddress,
        amount_in: U256,
        settled_out: U256,
        usd: Option<f64>,
        routable_pair: bool,
        id: String,
    }

    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut raws: Vec<Raw> = Vec::new();
    let mut price_samples: HashMap<ApexAddress, Vec<f64>> = HashMap::new();
    let mut routable: std::collections::HashSet<ApexAddress> = Default::default();

    for line in content.lines() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let canon = |t: &str| {
            if t == NATIVE {
                weth
            } else {
                parse_address(t).unwrap_or(ApexAddress([0xFF; 20]))
            }
        };
        let (Some(tin_s), Some(tout_s)) = (rec["token_in"].as_str(), rec["token_out"].as_str())
        else {
            continue;
        };
        let (tin, tout) = (canon(tin_s), canon(tout_s));
        if tin == tout || tin.0 == [0xFF; 20] || tout.0 == [0xFF; 20] {
            continue;
        }
        let (Some(ain), Some(aout)) = (
            rec["amount_in"]
                .as_str()
                .and_then(parse_u256_decimal),
            rec["settled_amount_out"]
                .as_str()
                .and_then(parse_u256_decimal),
        ) else {
            continue;
        };
        if ain.is_zero() || aout.is_zero() {
            continue;
        }
        let verdict = rec["top"]["verdict"]
            .as_str()
            .unwrap_or("");
        let usd = rec["top"]["settled_value_usd"].as_f64();
        if verdict == "win" || verdict == "loss" {
            routable.insert(tin);
            routable.insert(tout);
        }
        if let Some(u) = usd {
            if u > 0.0 {
                price_samples
                    .entry(tout)
                    .or_default()
                    .push(u / u256_to_f64(aout));
                price_samples
                    .entry(tin)
                    .or_default()
                    .push(u / u256_to_f64(ain));
            }
        }
        let (Some(tx), Some(tx_index)) = (rec["settled_tx"].as_str(), rec["tx_index"].as_u64())
        else {
            continue;
        };
        raws.push(Raw {
            block: rec["block"].as_u64().unwrap_or(0),
            token_in: tin,
            token_out: tout,
            amount_in: ain,
            settled_out: aout,
            usd,
            routable_pair: false,
            id: format!("{tx}:{tx_index}"),
        });
    }

    let mut day_price: HashMap<ApexAddress, f64> = HashMap::new();
    for (token, mut samples) in price_samples {
        samples.sort_by(|a, b| {
            a.partial_cmp(b)
                .expect("finite price samples")
        });
        day_price.insert(token, samples[samples.len() / 2]);
    }

    let mut intents = Vec::new();
    for mut raw in raws {
        raw.routable_pair = routable.contains(&raw.token_in) && routable.contains(&raw.token_out);
        let pin = day_price.get(&raw.token_in).copied();
        let pout = day_price.get(&raw.token_out).copied();
        let usd_est = raw
            .usd
            .or_else(|| pin.map(|p| p * u256_to_f64(raw.amount_in)))
            .or_else(|| pout.map(|p| p * u256_to_f64(raw.settled_out)));
        let Some(usd_est) = usd_est else { continue };
        let mut bad = usd_est > USD_CAP;
        if !bad {
            if let Some(pin) = pin {
                if pin > 0.0 {
                    let dev = (usd_est / u256_to_f64(raw.amount_in)) / pin;
                    bad = !(1.0 / PRICE_DEV_FACTOR..=PRICE_DEV_FACTOR).contains(&dev);
                }
            }
        }
        if !bad {
            if let Some(pout) = pout {
                if pout > 0.0 {
                    let dev = (usd_est / u256_to_f64(raw.settled_out)) / pout;
                    bad = !(1.0 / PRICE_DEV_FACTOR..=PRICE_DEV_FACTOR).contains(&dev);
                }
            }
        }
        if bad || !raw.routable_pair {
            continue;
        }
        let pair = if raw.token_in.0 < raw.token_out.0 {
            (raw.token_in, raw.token_out)
        } else {
            (raw.token_out, raw.token_in)
        };
        intents.push(Intent {
            block: raw.block,
            token_in: raw.token_in,
            token_out: raw.token_out,
            amount_in: raw.amount_in,
            settled_out: raw.settled_out,
            usd: usd_est,
            id: raw.id,
            is_wash: pair == (wash_a, wash_b) || pair == (wash_b, wash_a),
        });
    }
    Ok((intents, day_price))
}

/// Solve one window batch (one tumbling window's headline intents) through APEX with zero
/// pools, one call per shared-token connected component.
fn solve_batch(
    intents: &[Intent],
    day_price: &HashMap<ApexAddress, f64>,
    limit_bps: u32,
) -> (f64, f64, Counters) {
    let mut counters = Counters::default();
    let mut matched_usd = 0.0f64;
    let mut surplus_usd = 0.0f64;

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
        return (0.0, 0.0, counters);
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
                }
                Some(_) => counters.unfilled_at_limit += 1,
                None if result.deadline_fired => counters.cluster_cut += 1,
                None => counters.unfilled_at_limit += 1,
            }
        }
    }
    (matched_usd, surplus_usd, counters)
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
            let batch_results: Vec<(f64, f64, Counters)> = days
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
            for (m, s, c) in &batch_results {
                matched += m;
                surplus += s;
                counters.absorb(c);
            }
            let wall_ms = started.elapsed().as_millis();
            eprintln!(
                "w={window:>3} bps={limit_bps:>3}: matched=${matched:>12.0} \
                 ({:.3}%) surplus=${surplus:>10.2} filled={} errors={:?} wall={wall_ms}ms",
                100.0 * matched / intent_usd,
                counters.filled + counters.partially_filled,
                counters.component_errors,
            );
            cells.push(CellResult {
                window_blocks: window,
                limit_bps,
                intent_usd,
                apex_matched_usd: matched,
                apex_matched_pct: 100.0 * matched / intent_usd,
                apex_surplus_usd: surplus,
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
    fn test_parse_u256_decimal_round_trips() {
        assert_eq!(parse_u256_decimal("0"), Some(U256::ZERO));
        assert_eq!(parse_u256_decimal("123456789"), Some(U256::from(123_456_789u64)));
        assert_eq!(parse_u256_decimal("12x"), None);
        let large =
            "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(parse_u256_decimal(large), Some(U256::MAX));
        assert_eq!(parse_u256_decimal(&format!("{large}0")), None);
    }

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
            },
        ];
        let day_price =
            HashMap::from([(token_a, 100.0 / 100_000_000.0), (token_b, 100.0 / 200_000_000.0)]);
        let (matched, _surplus, counters) = solve_batch(&intents, &day_price, 100);
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
            },
        ];
        let day_price: HashMap<ApexAddress, f64> = [1u8, 2, 3, 4]
            .into_iter()
            .map(|b| (ApexAddress([b; 20]), 0.1))
            .collect();
        let (matched, surplus, counters) = solve_batch(&intents, &day_price, 100);
        assert_eq!(matched, 0.0);
        assert_eq!(surplus, 0.0);
        assert_eq!(counters.singles_skipped, 2);
        assert_eq!(counters.components_solved, 0);
    }
}
