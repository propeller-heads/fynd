//! Loading orders from the aggregator trade dataset.
//!
//! The dataset is real aggregator trades, one JSON object per trade holding a single order.
//! `fynd-core/benches/dataset.sql` is the query that builds the current one and explains its
//! filters.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use fynd_core::types::{Order, OrderSide};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::Deserialize;
use tycho_simulation::{protocol::models::Update, tycho_common::models::Address};

/// A dataset order that can be solved against a recorded market.
pub struct TradeOrder {
    /// Row label for reports: position in the dataset plus abbreviated token addresses.
    pub id: String,
    /// Position in the dataset, so a shuffled selection can be put back in file order.
    pub dataset_ix: usize,
    /// The trade's USD value as the dataset recorded it, when it carried one.
    ///
    /// Carried through so results can be sliced by trade size — the thing that decides whether a
    /// split is worth its gas.
    pub amount_usd: Option<f64>,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Amount to sell, in the input token's smallest unit.
    pub amount_in: BigUint,
}

impl TradeOrder {
    /// Converts to an [`Order`] for quoting.
    pub fn to_order(&self) -> Order {
        Order::new(
            self.token_in.clone(),
            self.token_out.clone(),
            self.amount_in.clone(),
            OrderSide::Sell,
            Address::zero(20),
        )
    }
}

/// Why the dataset shrank, so a low solve rate can be attributed to the market rather than the
/// algorithm under test.
#[derive(Debug, Default)]
pub struct TradeLoadSummary {
    /// Orders in the file.
    pub seen: usize,
    /// Orders that survived every drop, before the selection narrowed them.
    pub eligible: usize,
    /// Orders returned.
    pub kept: usize,
    /// Dropped because the order's side was anything other than a sell.
    pub dropped_not_sell: usize,
    /// Dropped because the recorded market never saw one of the tokens.
    pub dropped_unknown_token: usize,
    /// Dropped because an address or amount would not parse, or the order was degenerate.
    pub dropped_malformed: usize,
    /// Dropped because the recorded amount and the recorded USD value cannot both be right. See
    /// [`drop_inconsistent_usd`].
    pub dropped_inconsistent_usd: usize,
}

/// Every token address a recording announced on a component.
///
/// Orders naming anything outside this set cannot be routed, and counting them as algorithm
/// failures would be wrong.
pub fn recorded_tokens(updates: &[Update]) -> HashSet<Address> {
    let mut tokens = HashSet::new();
    for update in updates {
        for component in update.new_pairs.values() {
            for token in &component.tokens {
                tokens.insert(token.address.clone());
            }
        }
    }
    tokens
}

/// Which eligible orders a run solves, once the dataset has been filtered.
///
/// The dataset is in the aggregator's own order, so the head and the tail are different slices of
/// trading, not two samples of one. Comparing a change on both is what says whether a result holds
/// or only describes the orders it was tuned on.
#[derive(Clone, Copy, Debug)]
pub enum OrderSelection {
    /// Every eligible order.
    All,
    /// The first `n` in dataset order.
    Head(usize),
    /// The last `n` in dataset order.
    Tail(usize),
    /// `n` drawn at random, from a fixed seed, so two runs pick the same orders.
    Random(usize),
}

/// The three command-line flags that choose the orders, so both binaries offer the same ones and
/// read them the same way.
///
/// The default for `--orders` is the binary's own — a profiling run wants fewer than a benchmark —
/// so it is passed to [`OrderFlags::selection`] rather than declared here.
#[derive(clap::Args, Debug)]
pub struct OrderFlags {
    /// Orders to solve, taken from the top of the dataset. 0 means every eligible order.
    #[arg(long, visible_alias = "head", conflicts_with_all = ["tail", "random"])]
    pub orders: Option<usize>,

    /// The last N eligible orders instead of the first.
    ///
    /// The dataset is in the aggregator's own order, so the head and the tail are different
    /// slices of trading. A result that holds on both is not an artefact of the orders it was
    /// tuned on.
    #[arg(long, conflicts_with_all = ["orders", "random"])]
    pub tail: Option<usize>,

    /// N eligible orders drawn at random, from a fixed seed so two runs pick the same ones.
    #[arg(long, conflicts_with_all = ["orders", "tail"])]
    pub random: Option<usize>,
}

impl OrderFlags {
    /// What the flags ask for, with `default_orders` standing in for an absent `--orders`.
    pub fn selection(&self, default_orders: usize) -> OrderSelection {
        match (self.tail, self.random) {
            (Some(n), _) => OrderSelection::Tail(n),
            (_, Some(n)) => OrderSelection::Random(n),
            _ => match self.orders.unwrap_or(default_orders) {
                0 => OrderSelection::All,
                n => OrderSelection::Head(n),
            },
        }
    }
}

impl OrderSelection {
    /// Narrows `orders` to what the run will solve.
    pub fn narrow(self, orders: &mut Vec<TradeOrder>) {
        match self {
            Self::All => {}
            Self::Head(n) => orders.truncate(n),
            Self::Tail(n) => {
                let skip = orders.len().saturating_sub(n);
                orders.drain(..skip);
            }
            Self::Random(n) => {
                shuffle(orders);
                orders.truncate(n);
                // Back into dataset order, so a report reads the same way whatever the selection.
                orders.sort_by_key(|order| order.dataset_ix);
            }
        }
    }

    /// How `run.json` and the profiler's header name this selection.
    pub fn label(self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Head(n) => format!("head {n}"),
            Self::Tail(n) => format!("tail {n}"),
            Self::Random(n) => format!("random {n}"),
        }
    }
}

/// Fisher-Yates over a xorshift64 stream from a fixed seed.
///
/// Written here rather than taken from `rand` because the benchmark needs one property the crate
/// does not promise across versions: the same seed picks the same orders, so two runs months apart
/// still compare.
fn shuffle(orders: &mut [TradeOrder]) {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for ix in (1..orders.len()).rev() {
        orders.swap(ix, (next() % (ix as u64 + 1)) as usize);
    }
}

/// Reads the dataset and keeps the sell orders whose tokens both appear in `known_tokens`.
///
/// The whole file is filtered before `selection` narrows it, so the drop counts in the returned
/// summary describe the dataset rather than the slice that was read.
///
/// # Errors
///
/// Returns the reason the dataset could not be used: it cannot be read, or it does not parse.
pub fn load_trade_orders(
    path: &Path,
    known_tokens: &HashSet<Address>,
    selection: OrderSelection,
) -> Result<(Vec<TradeOrder>, TradeLoadSummary), String> {
    let json = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "could not read the trade dataset at {}: {error}. Build one with \
             fynd-core/benches/dataset.sql, or point --trades at your own copy.",
            path.display()
        )
    })?;
    let trades: Vec<DatasetTrade> = serde_json::from_str(&json)
        .map_err(|error| format!("{} does not parse: {error}", path.display()))?;

    let mut summary = TradeLoadSummary::default();
    let mut orders = Vec::new();

    for trade in &trades {
        for raw in &trade.orders {
            summary.seen += 1;
            match to_trade_order(raw, trade.amount_usd, known_tokens, summary.seen - 1) {
                Ok(order) => {
                    orders.push(order);
                }
                Err(DropReason::NotSell) => summary.dropped_not_sell += 1,
                Err(DropReason::UnknownToken) => summary.dropped_unknown_token += 1,
                Err(DropReason::Malformed) => summary.dropped_malformed += 1,
            }
        }
    }

    summary.dropped_inconsistent_usd = drop_inconsistent_usd(&mut orders);
    summary.eligible = orders.len();

    selection.narrow(&mut orders);
    summary.kept = orders.len();

    Ok((orders, summary))
}

/// Dataset schema. Only the fields the offline benchmarks read.
#[derive(Deserialize)]
struct DatasetTrade {
    orders: Vec<DatasetOrder>,
    /// Present on datasets built by `fynd-core/benches/dataset.sql`; absent on older ones.
    #[serde(default)]
    amount_usd: Option<f64>,
}

#[derive(Deserialize)]
struct DatasetOrder {
    token_in: String,
    token_out: String,
    amount: String,
    #[serde(default)]
    side: Option<String>,
}

enum DropReason {
    NotSell,
    UnknownToken,
    Malformed,
}

/// Anything that is not a sell is dropped rather than converted: a buy's output is fixed, so a
/// comparison on output net of gas would not be measuring the same quantity as the sells.
fn to_trade_order(
    raw: &DatasetOrder,
    amount_usd: Option<f64>,
    known_tokens: &HashSet<Address>,
    index: usize,
) -> Result<TradeOrder, DropReason> {
    if let Some(side) = &raw.side {
        if !side.eq_ignore_ascii_case("sell") {
            return Err(DropReason::NotSell);
        }
    }

    let token_in: Address = raw
        .token_in
        .parse()
        .map_err(|_| DropReason::Malformed)?;
    let token_out: Address = raw
        .token_out
        .parse()
        .map_err(|_| DropReason::Malformed)?;
    let amount_in: BigUint = raw
        .amount
        .parse()
        .map_err(|_| DropReason::Malformed)?;

    if token_in == token_out || amount_in == BigUint::from(0u8) {
        return Err(DropReason::Malformed);
    }
    if !known_tokens.contains(&token_in) || !known_tokens.contains(&token_out) {
        return Err(DropReason::UnknownToken);
    }

    let id = format!(
        "{index}_{}_{}",
        abbreviate_address(&raw.token_in),
        abbreviate_address(&raw.token_out)
    );
    Ok(TradeOrder { id, dataset_ix: index, amount_usd, token_in, token_out, amount_in })
}

/// Minimum orders on one token before its median unit value is trusted to judge an outlier.
///
/// A median over a handful of orders is itself mostly noise, and dropping a real order costs more
/// than keeping a corrupt one: the corrupt ones are rare and flagrant.
const MIN_ORDERS_PER_TOKEN_MEDIAN: usize = 8;

/// How far an order's USD per atomic unit may sit either side of its token's median before the row
/// is treated as corrupt.
///
/// Deliberately far. A token really can move by a large factor inside the dataset's two-week
/// window, and the corruption this catches is off by ten orders of magnitude, not by two.
const MAX_UNIT_PRICE_FACTOR: f64 = 100.0;

/// Drops orders whose amount and USD value cannot both be right, returning how many went.
///
/// `dex_aggregator.trades` occasionally records `token_sold_amount_raw` scaled for a different
/// token's decimals than `token_sold_address` names. Order `3242_2260fac5_dac17f95` sells WBTC — 8
/// decimals — and carries `26058859149200193346` against `$54768`: an 18-decimal amount, and so an
/// order for 260 billion WBTC.
///
/// Such a row poisons the report twice. The solver is handed a size no market can fill, so the
/// route it returns is not an answer to any real question. And [`usd_out`](super::usd_out) values
/// the output by the ratio of output to input, so an input inflated by eleven orders of magnitude
/// values every route at about zero — which the viewer then shows as a route that bought nothing
/// and lost 100% of the order.
///
/// The test needs no price feed and no decimals. Within one token, `amount_usd / amount` is the
/// same figure for every order whatever the decimals are, so that token's own median is the
/// reference and the dataset judges itself.
fn drop_inconsistent_usd(orders: &mut Vec<TradeOrder>) -> usize {
    let mut unit_values: HashMap<Address, Vec<f64>> = HashMap::new();
    for order in orders.iter() {
        if let Some(unit_value) = usd_per_atomic_unit(order) {
            unit_values
                .entry(order.token_in.clone())
                .or_default()
                .push(unit_value);
        }
    }

    let mut medians: HashMap<Address, f64> = HashMap::new();
    for (token, mut values) in unit_values {
        if values.len() < MIN_ORDERS_PER_TOKEN_MEDIAN {
            continue;
        }
        values.sort_by(f64::total_cmp);
        medians.insert(token, values[values.len() / 2]);
    }

    let before = orders.len();
    orders.retain(|order| {
        let Some(median) = medians.get(&order.token_in) else {
            return true;
        };
        let Some(unit_value) = usd_per_atomic_unit(order) else {
            return true;
        };
        unit_value <= median * MAX_UNIT_PRICE_FACTOR && unit_value >= median / MAX_UNIT_PRICE_FACTOR
    });
    before - orders.len()
}

/// What one atomic unit of the input token was worth, as this order records it.
///
/// `None` when the order carries no USD value, or when the amount is too large for an `f64` — in
/// both cases there is nothing to compare and the order is left alone.
fn usd_per_atomic_unit(order: &TradeOrder) -> Option<f64> {
    let amount = order.amount_in.to_f64()?;
    if amount <= 0.0 || !amount.is_finite() {
        return None;
    }
    let unit_value = order.amount_usd? / amount;
    if unit_value > 0.0 && unit_value.is_finite() {
        Some(unit_value)
    } else {
        None
    }
}

/// First 8 hex characters of an address, enough to identify a token in a report.
pub fn abbreviate_address(address: &str) -> String {
    let stripped = address
        .strip_prefix("0x")
        .unwrap_or(address);
    stripped.chars().take(8).collect()
}
