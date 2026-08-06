//! Loading orders from the aggregator trade dataset.
//!
//! The dataset is real aggregator trades, one JSON object per trade holding a single order.
//! `fynd-core/benches/dataset.sql` is the query that builds the current one and explains its
//! filters.

use std::{collections::HashSet, path::Path};

use fynd_core::types::{Order, OrderSide};
use num_bigint::BigUint;
use serde::Deserialize;
use tycho_simulation::{protocol::models::Update, tycho_common::models::Address};

/// A dataset order that can be solved against a recorded market.
pub struct TradeOrder {
    /// Row label for reports: position in the dataset plus abbreviated token addresses.
    pub id: String,
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
    /// Orders that survived filtering, before `limit` truncated them.
    pub eligible: usize,
    /// Orders returned.
    pub kept: usize,
    /// Dropped because the order's side was anything other than a sell.
    pub dropped_not_sell: usize,
    /// Dropped because the recorded market never saw one of the tokens.
    pub dropped_unknown_token: usize,
    /// Dropped because an address or amount would not parse, or the order was degenerate.
    pub dropped_malformed: usize,
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

/// Reads the dataset and keeps the sell orders whose tokens both appear in `known_tokens`.
///
/// The whole file is filtered before `limit` truncates it, so the drop counts in the returned
/// summary describe the dataset rather than the prefix that happened to be read. A `limit` of 0
/// keeps everything. Kept orders are the first ones in file order, not a random sample.
///
/// # Errors
///
/// Returns the reason the dataset could not be used: it cannot be read, or it does not parse.
pub fn load_trade_orders(
    path: &Path,
    known_tokens: &HashSet<Address>,
    limit: usize,
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
                    summary.eligible += 1;
                    orders.push(order);
                }
                Err(DropReason::NotSell) => summary.dropped_not_sell += 1,
                Err(DropReason::UnknownToken) => summary.dropped_unknown_token += 1,
                Err(DropReason::Malformed) => summary.dropped_malformed += 1,
            }
        }
    }

    if limit > 0 {
        orders.truncate(limit);
    }
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
    Ok(TradeOrder { id, amount_usd, token_in, token_out, amount_in })
}

/// First 8 hex characters of an address, enough to identify a token in a report.
pub fn abbreviate_address(address: &str) -> String {
    let stripped = address
        .strip_prefix("0x")
        .unwrap_or(address);
    stripped.chars().take(8).collect()
}
