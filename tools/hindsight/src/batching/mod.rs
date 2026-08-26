//! Batching-validation experiment: per block, run the block's settled trades through the APEX
//! batch solver against the same top-of-block (N-1) market state the monitor already holds, and
//! record what a uniform-clearing-price batch would have delivered versus per-order solving.
//!
//! Two runs per block, both on the permissive limit-price variant (every order may always fill):
//! - **S1** — one order per solve, control: same solver and pool snapshot, no batching.
//! - **S2** — the whole block as one batch, treatment.
//!
//! S0 (the settled on-chain outcome) comes from the decoder and is carried on every record.
//! Sandwiched trades never enter the batch — a solved batch is one joint clearing and cannot be
//! taken apart afterwards. Orders APEX did not clear count at S0 in the analysis. A partially
//! filled order executes fully for the user at the clearing price: the batcher acts as the
//! missing liquidity source, receiving the unsold remainder and supplying the buy-token
//! remainder (recorded as `batcher_bought`/`batcher_sold`).

mod dump;
mod pools;
mod snapshot;
mod solve;

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use alloy::primitives::{Address as AlloyAddress, TxHash, U256 as AlloyU256};
use apex_solver::{core::LimitOrder, types::U256 as ApexU256};
use fynd_core::Solver;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub(crate) use self::dump::{replay_dir, ReplayArgs};
use crate::decoder::DecodedTrade;

/// The trade fields the experiment actually uses, split out from the decoder's `DecodedTrade`
/// so a dumped batch can be re-solved later without reconstructing a full decode (which needs
/// the RPC, the traces and the address book). Everything a record carries about the settled
/// swap comes from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BatchTrade {
    pub tx_hash: TxHash,
    pub tx_index: u64,
    pub sender: AlloyAddress,
    pub venue: String,
    pub solver: String,
    /// `Address::ZERO` for native ETH, folded to WETH by `experiment_tokens`.
    pub token_in: AlloyAddress,
    pub token_out: AlloyAddress,
    pub amount_in: AlloyU256,
    pub amount_out: AlloyU256,
    /// The user's signed minimum output, when the decoder recovered it from calldata.
    pub min_amount_out: Option<AlloyU256>,
}

impl From<&DecodedTrade> for BatchTrade {
    fn from(trade: &DecodedTrade) -> Self {
        Self {
            tx_hash: trade.tx_hash,
            tx_index: trade.tx_index,
            sender: trade.sender,
            venue: trade.venue.clone(),
            solver: trade.solver.clone(),
            token_in: trade.token_in,
            token_out: trade.token_out,
            amount_in: trade.amount_in,
            amount_out: trade.amount_out,
            min_amount_out: trade.min_amount_out,
        }
    }
}

/// Mainnet hub tokens (Turbine's set): always added to the token universe so the solver can
/// route through liquid intermediaries the block's trades may not touch directly.
const HUB_TOKENS: [&str; 7] = [
    "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599", // WBTC
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7", // USDT
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // WETH
    "0x7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9", // AAVE
    "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984", // UNI
    "0x6b175474e89094c44da98b954eedeac495271d0f", // DAI
];

/// WETH stands in for native ETH: decoded trades use `Address::ZERO` for raw-ETH legs, but the
/// routers wrap anyway and the indexed pools quote WETH. Pools keyed on the zero address
/// (e.g. native Uniswap V4 pools) are excluded from the universe instead.
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

/// Whether an order got batched, and if not, where it fell out of the pipeline. Uncleared
/// orders count at S0 in the analysis (they would have executed standalone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InclusionStatus {
    /// Fully filled by the batch.
    Cleared,
    /// Partially filled by APEX: the user still executes fully at the clearing price, with the
    /// batcher supplying the buy-token remainder and receiving the unsold sell amount.
    Partial,
    /// A trade token had no derived price, so the order never entered APEX.
    OutOfUniverse,
    /// Entered APEX but has no clearing entry. APEX does not report why — cluster-pruned and
    /// limit-not-met look the same from outside — and we deliberately don't distinguish them.
    Unfilled,
}

/// One order's outcome in one run (S1 or S2). All amounts are raw native token units, serialized
/// as decimal strings.
#[derive(Debug, Serialize)]
pub(crate) struct OrderRecord {
    /// Record-format version. 2 = partial fills are topped up by the batcher (the user executes
    /// fully at clearing price); 1 (absent in old data) = partial fills counted at S0 with the
    /// batcher absorbing the cleared slice.
    pub schema: u32,
    pub block: u64,
    /// `"s1"` (per-order control) or `"s2"` (whole-block batch).
    pub run: &'static str,
    /// Limit-price variant: `"permissive"` or `"anchored"`.
    pub variant: &'static str,
    pub tx_hash: String,
    pub tx_index: u64,
    /// Stable join key across runs: one trade's S1 and S2 records share it.
    pub order_id: String,
    pub sender: String,
    pub venue: String,
    pub solver: String,
    pub sell_token: String,
    pub buy_token: String,
    pub sell_symbol: String,
    pub buy_symbol: String,
    pub sell_decimals: u32,
    pub buy_decimals: u32,
    /// S0: what actually entered/left the settled swap (venue-fee-adjusted, from the decoder).
    pub amount_in: String,
    pub settled_amount_out: String,
    /// ETH valuations at the block's derived prices (0.0 when the token is unpriced), so the
    /// report can aggregate across tokens without re-deriving prices.
    pub amount_in_eth: f64,
    pub settled_amount_out_eth: f64,
    pub status: InclusionStatus,
    /// What APEX cleared for this order (zero unless `cleared`/`partial`).
    pub apex_sold: String,
    pub apex_bought: String,
    /// The batcher's top-up of a partial fill (zero otherwise): it supplies `batcher_sold` of
    /// the **buy** token from inventory (the full clearing-price output minus APEX's fill) and
    /// receives `batcher_bought` of the **sell** token (the user's unsold remainder).
    pub batcher_sold: String,
    pub batcher_bought: String,
    /// For `user_limit` records: where the limit came from (`calldata` | `settled_fallback`);
    /// empty for the other variants.
    pub limit_source: &'static str,
    pub apex_bought_eth: f64,
    pub batcher_sold_eth: f64,
    pub batcher_bought_eth: f64,
}

/// Per-pool volume the S2 batch routed through AMMs, scaled down to raw native units.
/// `sold` is what the pool paid out (users received), `bought` what it took in.
#[derive(Debug, Serialize)]
pub(crate) struct PoolVolumeRecord {
    pub address: String,
    pub sell_token: String,
    pub buy_token: String,
    pub sold: String,
    pub bought: String,
    pub sold_eth: f64,
    pub bought_eth: f64,
}

/// One panel config's outcome on one solve. The winner alone says which config the panel
/// picked, not how the others did — and it says nothing at all when every config comes back
/// empty, since the tie at zero volume is broken by panel order. These records separate the
/// three ways a config can fail to clear: it ran out of time, it converged on prices that
/// cleared nothing, or it errored.
#[derive(Debug, Serialize)]
pub(crate) struct ConfigOutcomeRecord {
    /// The config's panel label (e.g. `"/1000 two-hops mixed"`).
    pub config: &'static str,
    /// `"ok"` | `"error"` | `"panic"`; the amounts below are zero unless `"ok"`.
    pub status: &'static str,
    pub solve_ms: u64,
    /// Whether this config's search was cut short by the shared deadline. A truncated solve
    /// returns prices but no clearings, so it always clears nothing.
    pub deadline_fired: bool,
    /// The panel's own score: ETH value of everything this config's limit-order clearings
    /// bought. The winner is the config with the most of it.
    pub cleared_eth: f64,
    pub orders_cleared: usize,
    pub pool_clearings: usize,
    /// True for the config whose result the block's order records were built from.
    pub winner: bool,
}

/// One panel config's totals over a block's S1 solves (there is one solve per order, so a
/// record each would multiply out to orders × configs per block).
#[derive(Debug, Default, Serialize)]
pub(crate) struct ConfigTotalsRecord {
    pub config: &'static str,
    /// Solves this config completed, errored on, and had cut short by the deadline.
    pub ok: usize,
    pub failed: usize,
    pub deadline_fired: usize,
    /// Solves this config won.
    pub wins: usize,
    pub solve_ms: u64,
    pub cleared_eth: f64,
    pub orders_cleared: usize,
    pub pool_clearings: usize,
}

/// One block's run summary: solve health and coverage diagnostics for the report.
#[derive(Debug, Serialize)]
pub(crate) struct BlockRecord {
    pub block: u64,
    /// Limit-price variant this record's solves ran under (one block record per variant).
    pub variant: &'static str,
    pub trades_decoded: usize,
    pub sandwiched_excluded: usize,
    pub out_of_universe: usize,
    pub orders_in: usize,
    pub universe_tokens: usize,
    pub pools_native_v2: usize,
    pub pools_native_v3: usize,
    pub pools_wrapped: usize,
    /// Components with no in-universe pair view (tokens outside the priced universe, or an
    /// ETH/WETH fold collision).
    pub pools_skipped: usize,
    pub s2_solve_ms: u64,
    pub s2_deadline_fired: bool,
    /// Which run config of the parallel panel won the S2 solve.
    pub s2_winning_config: String,
    /// Every panel config's outcome on the S2 solve, winner included — one entry per config.
    pub s2_configs: Vec<ConfigOutcomeRecord>,
    /// Wall-clock time the block's S1 solves took together. They run concurrently, so this is
    /// the span they occupied, not the sum of their solve times — `s1_configs[].solve_ms` adds
    /// those up.
    pub s1_solve_ms_total: u64,
    pub s1_deadline_fired: usize,
    /// Every panel config's outcome summed over the block's S1 solves: one entry per config,
    /// with `solve_ms` and the cleared amounts totalled across the orders and `wins` counting
    /// the solves that config won.
    pub s1_configs: Vec<ConfigTotalsRecord>,
    /// S2's AMM legs: which pools the batch actually traded, for the CoW-share metric
    /// (order volume that cleared without touching any pool netted user-to-user).
    pub s2_pool_volumes: Vec<PoolVolumeRecord>,
}

/// The limit-price variants each block runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    /// Limit ≈ 0: every order may always fill; measures raw price movement.
    Permissive,
    /// Limit = the actual settled execution price: APEX must beat reality to fill.
    Anchored,
    /// Limit = the user's signed slippage limit (minimum buy amount recovered from router
    /// calldata); orders whose limit could not be recovered fall back to the anchored limit —
    /// tighter than the true one (execution enforced min ≤ settled), so fills stay valid.
    UserLimit,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Permissive => "permissive",
            Variant::Anchored => "anchored",
            Variant::UserLimit => "user_limit",
        }
    }
}

/// One decoded trade prepared for APEX: one order per limit-price variant (same id) plus
/// everything needed to build records for it.
pub(crate) struct PreparedOrder {
    pub trade_ix: usize,
    pub permissive: LimitOrder,
    pub anchored: LimitOrder,
    pub user_limit: LimitOrder,
    /// Where the user-limit variant's limit came from: `"calldata"` (the signed minimum buy
    /// amount) or `"settled_fallback"` (anchored limit, when extraction failed).
    pub limit_source: &'static str,
    /// Apex-scaled (18-dec) sell amount, kept to detect partial fills exactly.
    pub scaled_sell: ApexU256,
    pub sell_token: AlloyAddress,
    pub buy_token: AlloyAddress,
}

impl PreparedOrder {
    pub fn order(&self, variant: Variant) -> &LimitOrder {
        match variant {
            Variant::Permissive => &self.permissive,
            Variant::Anchored => &self.anchored,
            Variant::UserLimit => &self.user_limit,
        }
    }
}

/// The APEX solve budgets one run uses.
#[derive(Clone, Copy)]
pub(crate) struct SolveBudget {
    /// Per-order budget for S1 solves, in milliseconds.
    pub s1_deadline_ms: u64,
    /// Whole-batch budget for the S2 solve, in milliseconds.
    pub s2_deadline_ms: u64,
    /// Override for APEX's price-search iteration cap.
    pub max_iterations: Option<u32>,
    /// How many of a variant's S1 order-solves may run at once. S2 solves always run alone —
    /// one batch at a time with the machine to itself — so that the S2 deadline means the same
    /// thing it would in production. S1 is only a baseline, so it may fan out.
    pub s1_workers: usize,
}

/// Appends order and block records to a run directory's JSONL files. Shared by the capture
/// path (when it solves inline) and by `apex-solve`, so both produce byte-identical output.
pub(crate) struct RecordWriter {
    orders_out: BufWriter<File>,
    blocks_out: BufWriter<File>,
}

impl RecordWriter {
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let open = |name: &str| -> anyhow::Result<BufWriter<File>> {
            Ok(BufWriter::new(
                File::options()
                    .create(true)
                    .append(true)
                    .open(dir.join(name))?,
            ))
        };
        Ok(Self { orders_out: open("apex-orders.jsonl")?, blocks_out: open("apex-blocks.jsonl")? })
    }

    pub fn write(
        &mut self,
        records: &[OrderRecord],
        block_records: &[BlockRecord],
    ) -> anyhow::Result<()> {
        for record in records {
            serde_json::to_writer(&mut self.orders_out, record)?;
            self.orders_out.write_all(b"\n")?;
        }
        for block_record in block_records {
            serde_json::to_writer(&mut self.blocks_out, block_record)?;
            self.blocks_out.write_all(b"\n")?;
            info!(
                block = block_record.block,
                variant = block_record.variant,
                orders = block_record.orders_in,
                out_of_universe = block_record.out_of_universe,
                s2_ms = block_record.s2_solve_ms,
                s1_ms = block_record.s1_solve_ms_total,
                "apex batching: block processed"
            );
            if block_record.s2_deadline_fired {
                warn!(
                    block = block_record.block,
                    variant = block_record.variant,
                    "apex batching: S2 deadline fired; batch result is partial"
                );
            }
        }
        self.orders_out.flush()?;
        self.blocks_out.flush()?;
        Ok(())
    }
}

/// The experiment's live half: snapshots each block at top-of-block state and writes it to the
/// capture directory. Solving is a separate process (`hindsight apex-solve`) reading the same
/// directory as a queue, because the snapshot is the only part that has to happen live and it
/// costs a fraction of a second, while solving costs ~50x a block time.
pub(crate) struct BatchingEngine {
    inputs_dir: PathBuf,
    blocks_captured: u64,
}

impl BatchingEngine {
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let inputs_dir = dir.join("inputs");
        std::fs::create_dir_all(&inputs_dir)?;
        std::fs::create_dir_all(dir.join("results"))?;
        Ok(Self { inputs_dir, blocks_captured: 0 })
    }

    /// Capture one block. Must be called while the solver still holds top-of-block state N-1
    /// (before the monitor's `advance`). Errors are the caller's to log-and-continue: a failed
    /// block must not kill the monitor session.
    pub async fn process_block(
        &mut self,
        block: u64,
        trades: &[DecodedTrade],
        solver: &Solver,
    ) -> anyhow::Result<()> {
        let market = solver.market_data();
        let derived = solver.derived_data();

        // Snapshot everything a solve needs while holding the locks, then release them.
        let snapshot = {
            let market_guard = market.read().await;
            let derived_guard = derived.read().await;
            let state = market_guard.base_market_state();
            let Some(token_prices) = derived_guard.token_prices() else {
                anyhow::bail!("derived token prices not ready yet; skipping block");
            };
            snapshot::build_snapshot(state, token_prices, trades)
        };

        let batch_trades: Vec<BatchTrade> = trades
            .iter()
            .map(BatchTrade::from)
            .collect();
        let dump = dump::BatchDump::capture(block, &batch_trades, &snapshot);
        dump::write_dump(&dump, &self.inputs_dir)?;
        self.blocks_captured += 1;
        info!(
            block,
            orders = snapshot.prepared.len(),
            out_of_universe = snapshot.out_of_universe.len(),
            captured_total = self.blocks_captured,
            "apex batching: block captured"
        );
        Ok(())
    }
}

/// Decimal-string helper for JSONL amounts.
pub(crate) fn dec<T: std::fmt::Display>(value: T) -> String {
    value.to_string()
}

/// Alloy address → apex address.
pub(crate) fn apex_addr(address: AlloyAddress) -> apex_solver::types::Address {
    apex_solver::types::Address(address.into_array())
}

/// Tycho address bytes → alloy address. `None` if not 20 bytes.
pub(crate) fn alloy_from_bytes(bytes: &[u8]) -> Option<AlloyAddress> {
    let array: [u8; 20] = bytes.try_into().ok()?;
    Some(AlloyAddress::from(array))
}

/// The trade's tokens as they enter the experiment: native ETH (zero address) is folded into
/// WETH on both legs, matching the WETH-quoted pool universe.
pub(crate) fn experiment_tokens(trade: &BatchTrade) -> (AlloyAddress, AlloyAddress) {
    fold_native(trade.token_in, trade.token_out)
}

/// Same fold, for callers that hold the raw decode rather than a `BatchTrade`.
pub(crate) fn fold_native(
    token_in: AlloyAddress,
    token_out: AlloyAddress,
) -> (AlloyAddress, AlloyAddress) {
    let weth: AlloyAddress = WETH
        .parse()
        .expect("static WETH address");
    let fold = |token: AlloyAddress| if token == AlloyAddress::ZERO { weth } else { token };
    (fold(token_in), fold(token_out))
}

/// The hub-token set as alloy addresses.
pub(crate) fn hub_tokens() -> Vec<AlloyAddress> {
    let mut hubs = Vec::with_capacity(HUB_TOKENS.len());
    for hub in HUB_TOKENS {
        hubs.push(hub.parse().expect("static hub address"));
    }
    hubs
}

/// Convert an alloy `U256` to an apex `U256` (different ruint instantiations, byte-identical).
pub(crate) fn apex_u256(value: alloy::primitives::U256) -> ApexU256 {
    ApexU256::from_le_bytes(value.to_le_bytes::<32>())
}

/// Convert an apex `U256` back to an alloy `U256`.
pub(crate) fn alloy_u256(value: ApexU256) -> alloy::primitives::U256 {
    alloy::primitives::U256::from_le_bytes(value.to_le_bytes::<32>())
}

/// Group prepared orders into APEX's per-pair order map, for one limit-price variant.
pub(crate) fn orders_by_pair(
    orders: &[PreparedOrder],
    variant: Variant,
) -> HashMap<apex_solver::core::PairAddresses, Vec<LimitOrder>> {
    let mut map: HashMap<apex_solver::core::PairAddresses, Vec<LimitOrder>> = HashMap::new();
    for prepared in orders {
        map.entry((apex_addr(prepared.sell_token), apex_addr(prepared.buy_token)))
            .or_default()
            .push(prepared.order(variant).clone());
    }
    map
}
