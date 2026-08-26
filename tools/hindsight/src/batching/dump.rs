//! Capturing a block's batch to disk, and re-solving it later.
//!
//! The experiment splits into two processes that share a directory:
//!
//! - **capture** — the monitor decodes and snapshots each block at top-of-block state and writes
//!   `inputs/apex_batch_<block>.json`. That is the only part that has to happen live: the snapshot
//!   reads the solver's in-memory market state, which is gone once the chain moves on. It costs
//!   well under a second per block, so it keeps pace with the chain indefinitely.
//! - **solve** — `hindsight apex-solve` treats that directory as a work queue, re-solving each dump
//!   into the same `apex-orders.jsonl` / `apex-blocks.jsonl` / `results/` the live path wrote
//!   before. It is ~50× slower than capture, so it lags and drains afterwards.
//!
//! A dump therefore has to carry everything `run_block` needs, which is more than the
//! `ApexInputData` the solver takes: the other two limit-price variants (that file holds only
//! the permissive orders), the settled-trade fields records are built from, and the token
//! metadata that values amounts in ETH. Wrapped Tycho pools survive the round trip because
//! `ProtocolSim` is a `#[typetag::serde]` trait — see `pools::rebuild_wrapped`.
//!
//! Which blocks are already done is read back from `apex-blocks.jsonl` rather than tracked
//! separately, so solving is resumable and re-running over a directory is idempotent.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use alloy::primitives::Address as AlloyAddress;
use apex_solver::{
    core::{Fraction, LimitOrder},
    serialization::ApexInputData,
    types::U256 as ApexU256,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{
    orders_by_pair, pools,
    snapshot::{Snapshot, TokenMeta},
    solve, BatchTrade, ChainTokens, PreparedOrder, SolveBudget, Variant,
};

/// One captured block: the solver input plus everything else `run_block` needs.
#[derive(Serialize, Deserialize)]
pub(crate) struct BatchDump {
    pub block: u64,
    /// Tokens, prices and pools, in apex-solver's own snapshot format. Its `limit_orders` hold
    /// the permissive variant; `orders` below carries the other two variants' limits.
    pub input: ApexInputData,
    pub orders: Vec<PreparedOrderJson>,
    pub trades: Vec<BatchTrade>,
    /// Indices into `trades` for trades that never entered APEX (a token had no derived price).
    pub out_of_universe: Vec<usize>,
    pub excluded_sandwiched: usize,
    pub pool_counts: PoolCountsJson,
    pub token_meta: HashMap<AlloyAddress, TokenMetaJson>,
    /// The chain's wrapped-native and hub addresses, so a re-solve folds native ETH the same
    /// way the capture did. Absent in dumps captured before this was recorded; those are all
    /// Ethereum, so they fall back to mainnet WETH.
    #[serde(default = "ethereum_chain_tokens")]
    pub chain: ChainTokens,
}

/// The fold a pre-`chain` dump was captured with: every such dump is from Ethereum mainnet.
fn ethereum_chain_tokens() -> ChainTokens {
    ChainTokens {
        wrapped_native: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            .parse()
            .expect("static mainnet WETH address"),
        hubs: Vec::new(),
    }
}

/// The limits a prepared order carries beyond the permissive one already in `input`. Joined
/// back by order id.
#[derive(Serialize, Deserialize)]
pub(crate) struct PreparedOrderJson {
    pub id: String,
    pub trade_ix: usize,
    /// Numerator/denominator of each variant's limit price, as decimal strings.
    pub anchored_price: [String; 2],
    pub user_limit_price: [String; 2],
    pub limit_source: String,
    pub scaled_sell: String,
    pub sell_token: AlloyAddress,
    pub buy_token: AlloyAddress,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct PoolCountsJson {
    pub native_v2: usize,
    pub native_v3: usize,
    pub wrapped: usize,
    pub skipped: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct TokenMetaJson {
    pub symbol: String,
    pub decimals: u32,
    pub eth_per_atomic: f64,
}

fn dec_apex(value: ApexU256) -> String {
    value.to_string()
}

fn parse_apex(text: &str) -> anyhow::Result<ApexU256> {
    ApexU256::from_str_radix(text, 10).map_err(|_| anyhow::anyhow!("not a decimal amount: {text}"))
}

impl BatchDump {
    pub fn capture(
        block: u64,
        trades: &[BatchTrade],
        snapshot: &Snapshot,
        chain: &ChainTokens,
    ) -> Self {
        let orders = snapshot
            .prepared
            .iter()
            .map(|prepared| PreparedOrderJson {
                id: prepared.permissive.id.clone(),
                trade_ix: prepared.trade_ix,
                anchored_price: [
                    dec_apex(prepared.anchored.limit_price.numerator),
                    dec_apex(
                        prepared
                            .anchored
                            .limit_price
                            .denominator,
                    ),
                ],
                user_limit_price: [
                    dec_apex(
                        prepared
                            .user_limit
                            .limit_price
                            .numerator,
                    ),
                    dec_apex(
                        prepared
                            .user_limit
                            .limit_price
                            .denominator,
                    ),
                ],
                limit_source: prepared.limit_source.to_string(),
                scaled_sell: dec_apex(prepared.scaled_sell),
                sell_token: prepared.sell_token,
                buy_token: prepared.buy_token,
            })
            .collect();
        Self {
            block,
            input: ApexInputData {
                batch_id: block,
                tokens: snapshot.apex_tokens.clone(),
                initial_prices: snapshot.initial_prices.clone(),
                limit_orders: orders_by_pair(&snapshot.prepared, Variant::Permissive),
                market_orders: HashMap::new(),
                pools: snapshot.pools.clone(),
                custom_pools: Vec::new(),
            },
            orders,
            trades: trades.to_vec(),
            out_of_universe: snapshot.out_of_universe.clone(),
            excluded_sandwiched: snapshot.excluded_sandwiched,
            pool_counts: PoolCountsJson {
                native_v2: snapshot.pool_counts.native_v2,
                native_v3: snapshot.pool_counts.native_v3,
                wrapped: snapshot.pool_counts.wrapped,
                skipped: snapshot.pool_counts.skipped,
            },
            token_meta: snapshot
                .token_meta
                .iter()
                .map(|(address, meta)| {
                    (
                        *address,
                        TokenMetaJson {
                            symbol: meta.symbol.clone(),
                            decimals: meta.decimals,
                            eth_per_atomic: meta.eth_per_atomic,
                        },
                    )
                })
                .collect(),
            chain: chain.clone(),
        }
    }

    /// Rebuild the snapshot this dump was taken from. Wrapped pools are reconstructed from their
    /// opaque payloads, so the pool set matches the live one rather than the V2/V3 subset
    /// apex-solver can parse on its own.
    pub fn into_snapshot(
        mut self,
    ) -> anyhow::Result<(u64, Vec<BatchTrade>, Snapshot, ChainTokens)> {
        let mut pools = std::mem::take(&mut self.input.pools);
        for custom in std::mem::take(&mut self.input.custom_pools) {
            pools.push(pools::rebuild_wrapped(&custom)?);
        }

        // The permissive orders live in the solver input, keyed by pair; the other two variants'
        // limits ride alongside. Join them by order id.
        let mut permissive: HashMap<String, LimitOrder> = HashMap::new();
        for orders in self.input.limit_orders.values() {
            for order in orders {
                permissive.insert(order.id.clone(), order.clone());
            }
        }
        let mut prepared = Vec::with_capacity(self.orders.len());
        for json in &self.orders {
            let base = permissive
                .get(&json.id)
                .ok_or_else(|| anyhow::anyhow!("dump has no permissive order for {}", json.id))?;
            let price = |raw: &[String; 2]| -> anyhow::Result<Fraction> {
                Ok(Fraction::new(parse_apex(&raw[0])?, parse_apex(&raw[1])?))
            };
            prepared.push(PreparedOrder {
                trade_ix: json.trade_ix,
                permissive: base.clone(),
                anchored: LimitOrder::new(
                    base.sell_amount,
                    price(&json.anchored_price)?,
                    base.id.clone(),
                    base.owner,
                ),
                user_limit: LimitOrder::new(
                    base.sell_amount,
                    price(&json.user_limit_price)?,
                    base.id.clone(),
                    base.owner,
                ),
                // Records only ever compare this against the two known values.
                limit_source: match json.limit_source.as_str() {
                    "calldata" => "calldata",
                    "settled_fallback" => "settled_fallback",
                    _ => "",
                },
                scaled_sell: parse_apex(&json.scaled_sell)?,
                sell_token: json.sell_token,
                buy_token: json.buy_token,
            });
        }

        let snapshot = Snapshot {
            apex_tokens: std::mem::take(&mut self.input.tokens),
            initial_prices: std::mem::take(&mut self.input.initial_prices),
            pools,
            pool_counts: pools::PoolCounts {
                native_v2: self.pool_counts.native_v2,
                native_v3: self.pool_counts.native_v3,
                wrapped: self.pool_counts.wrapped,
                skipped: self.pool_counts.skipped,
            },
            prepared,
            out_of_universe: std::mem::take(&mut self.out_of_universe),
            excluded_sandwiched: self.excluded_sandwiched,
            token_meta: self
                .token_meta
                .iter()
                .map(|(address, meta)| {
                    (
                        *address,
                        TokenMeta {
                            symbol: meta.symbol.clone(),
                            decimals: meta.decimals,
                            eth_per_atomic: meta.eth_per_atomic,
                        },
                    )
                })
                .collect(),
        };
        Ok((self.block, std::mem::take(&mut self.trades), snapshot, self.chain.clone()))
    }
}

/// Write a captured block. Written to a temporary file and renamed, so the solver process can
/// never pick up a half-written dump.
pub(crate) fn write_dump(dump: &BatchDump, inputs_dir: &Path) -> anyhow::Result<()> {
    let final_path = dump_path(inputs_dir, dump.block);
    let temp_path = final_path.with_extension("json.partial");
    let file = std::fs::File::create(&temp_path)?;
    serde_json::to_writer(std::io::BufWriter::new(file), dump)?;
    std::fs::rename(&temp_path, &final_path)?;
    Ok(())
}

pub(crate) fn dump_path(inputs_dir: &Path, block: u64) -> PathBuf {
    inputs_dir.join(format!("apex_batch_{block}.json"))
}

/// `hindsight apex-solve` — drain a capture directory.
#[derive(clap::Args)]
pub(crate) struct ReplayArgs {
    /// The `--apex-batching-dir` of a capture run: reads `inputs/`, appends to
    /// `apex-orders.jsonl` / `apex-blocks.jsonl` and writes `results/`
    pub dir: PathBuf,

    /// Whole-batch deadline for the S2 solve, in milliseconds. This is the production-realistic
    /// number — S2 solves run one at a time with the machine to themselves so the budget means
    /// the same thing it would in production
    #[arg(long, default_value_t = 6000)]
    pub s2_deadline_ms: u64,

    /// Per-order deadline for the S1 control solves, in milliseconds. S1 is only a baseline, so
    /// it gets a generous per-order budget and no cap across the block
    #[arg(long, default_value_t = 6000)]
    pub s1_deadline_ms: u64,

    /// How many of a variant's S1 order-solves may run at once. They never overlap an S2 solve,
    /// so this does not disturb the S2 measurement
    #[arg(long, default_value_t = 4)]
    pub s1_workers: usize,

    /// Iteration cap for APEX's price search (default 3000; the deadlines still bound
    /// wall-clock time)
    #[arg(long)]
    pub max_iterations: Option<u32>,

    /// Keep polling for new dumps instead of exiting when the queue is empty, for running
    /// alongside a live capture. Stop with Ctrl-C
    #[arg(long)]
    pub follow: bool,

    /// Seconds to wait before re-checking the queue in `--follow` mode
    #[arg(long, default_value_t = 30)]
    pub poll_secs: u64,
}

/// Solve every dump in `args.dir` that has no records yet, oldest block first.
pub(crate) fn replay_dir(args: &ReplayArgs) -> anyhow::Result<()> {
    let inputs_dir = args.dir.join("inputs");
    let results_dir = args.dir.join("results");
    std::fs::create_dir_all(&results_dir)?;
    let mut writer = super::RecordWriter::new(&args.dir)?;
    let budget = SolveBudget {
        s1_deadline_ms: args.s1_deadline_ms,
        s2_deadline_ms: args.s2_deadline_ms,
        max_iterations: args.max_iterations,
        s1_workers: args.s1_workers,
    };

    let mut done = solved_blocks(&args.dir)?;
    info!(already_solved = done.len(), "apex-solve: starting");
    loop {
        let queue = pending_dumps(&inputs_dir, &done)?;
        if queue.is_empty() && !args.follow {
            info!("apex-solve: queue empty, done");
            return Ok(());
        }
        if queue.is_empty() {
            std::thread::sleep(std::time::Duration::from_secs(args.poll_secs));
            continue;
        }
        info!(pending = queue.len(), "apex-solve: draining queue");
        for (block, path) in queue {
            match solve_one(&path, budget, &results_dir, &mut writer) {
                Ok(orders) => {
                    done.insert(block);
                    info!(block, orders, remaining_unknown = true, "apex-solve: block solved");
                }
                Err(error) => {
                    // A block that cannot be solved must not stop the queue; it stays out of
                    // `done` so a later run retries it.
                    warn!(block, %error, "apex-solve: block failed, skipping");
                }
            }
        }
    }
}

fn solve_one(
    path: &Path,
    budget: SolveBudget,
    results_dir: &Path,
    writer: &mut super::RecordWriter,
) -> anyhow::Result<usize> {
    let text = std::fs::read_to_string(path)?;
    let dump: BatchDump = serde_json::from_str(&text)?;
    let (block, trades, snapshot, chain) = dump.into_snapshot()?;
    let mut records = Vec::new();
    let block_records =
        solve::run_block(block, &trades, &snapshot, &chain, budget, results_dir, &mut records)?;
    let count = records.len();
    writer.write(&records, &block_records)?;
    Ok(count)
}

/// Blocks that already have records, read back from the block JSONL so solving is resumable.
fn solved_blocks(dir: &Path) -> anyhow::Result<HashSet<u64>> {
    let path = dir.join("apex-blocks.jsonl");
    let mut done = HashSet::new();
    if !path.exists() {
        return Ok(done);
    }
    for line in std::fs::read_to_string(&path)?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(block) = value
                .get("block")
                .and_then(serde_json::Value::as_u64)
            {
                done.insert(block);
            }
        }
    }
    Ok(done)
}

fn pending_dumps(inputs_dir: &Path, done: &HashSet<u64>) -> anyhow::Result<Vec<(u64, PathBuf)>> {
    let mut queue = Vec::new();
    if !inputs_dir.exists() {
        return Ok(queue);
    }
    for entry in std::fs::read_dir(inputs_dir)? {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
        else {
            continue;
        };
        let Some(block) = name
            .strip_prefix("apex_batch_")
            .and_then(|rest| rest.strip_suffix(".json"))
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        if !done.contains(&block) {
            queue.push((block, path));
        }
    }
    queue.sort_by_key(|(block, _)| *block);
    Ok(queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_skips_solved_and_sorts_by_block() {
        let dir = std::env::temp_dir().join(format!("hindsight-dump-test-{}", std::process::id()));
        let inputs = dir.join("inputs");
        std::fs::create_dir_all(&inputs).unwrap();
        for block in [7u64, 5, 6] {
            std::fs::write(dump_path(&inputs, block), "{}").unwrap();
        }
        // An unrelated file in the queue directory is ignored.
        std::fs::write(inputs.join("notes.txt"), "x").unwrap();

        let done = HashSet::from([6u64]);
        let queue = pending_dumps(&inputs, &done).unwrap();
        assert_eq!(
            queue
                .iter()
                .map(|(b, _)| *b)
                .collect::<Vec<_>>(),
            vec![5, 7]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_solved_blocks_reads_back_the_jsonl() {
        let dir =
            std::env::temp_dir().join(format!("hindsight-solved-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("apex-blocks.jsonl"),
            "{\"block\":11,\"variant\":\"permissive\"}\n\n{\"block\":11,\"variant\":\"anchored\"}\n{\"block\":12}\n",
        )
        .unwrap();
        let done = solved_blocks(&dir).unwrap();
        assert_eq!(done, HashSet::from([11, 12]));
        std::fs::remove_dir_all(&dir).ok();
    }
}
