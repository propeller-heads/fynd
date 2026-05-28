//! `audit` subcommand — compare Fynd quotes against external DEX aggregators.
//!
//! Samples the top-N token pairs from the trade dataset, fires quotes at Fynd and
//! each configured aggregator in parallel across multiple blocks, and writes a
//! per-trade comparison table plus a console summary.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy::{
    hex,
    network::Ethereum,
    primitives::{map::B256HashMap, Address, TxKind, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        BlockId, RpcBlockHash, TransactionRequest,
    },
    transports::{RpcError, TransportErrorKind},
};
use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use fynd_client::{
    EncodingOptions, FyndClient, FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams,
    QuoteStatus, RetryConfig,
};
use num_bigint::BigUint;
use serde::Serialize;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::sleep,
};
use tracing::{info, warn};

use crate::{
    aggregator::{
        gas_adjusted_bps_diff, raw_bps_diff, AggregatorCalldata, AggregatorClient, AggregatorQuote,
        AggregatorStatus, KyberswapClient, NordsternClient, ZeroExClient,
    },
    erc20,
    pair_selector::{select_top_pairs, PairSpec},
    requests::{load_all_embedded_templates, load_all_templates_from_file},
};

// ─── CLI args ─────────────────────────────────────────────────────────────────

/// Compare Fynd quote performance against external DEX aggregators across multiple blocks.
#[derive(Parser, Debug)]
#[command(
    about = "Audit Fynd quote quality against external aggregators",
    long_about = "Audit Fynd quote quality against external aggregators.\n\n\
        Samples the top-N token pairs from the trade dataset, fires quotes at both Fynd and \
        the configured aggregator(s) for each (pair, amount) combination once per block, and \
        writes a per-trade JSON table plus a per-pair console summary.\n\n\
        Requires a healthy Fynd solver on --fynd-url."
)]
pub struct Args {
    /// Fynd solver base URL.
    #[arg(long, env = "FYND_URL", default_value = "http://localhost:3000")]
    pub fynd_url: String,

    /// Nordstern Finance base URL.
    #[arg(long, default_value = "https://api.nordstern.finance")]
    pub nordstern_url: String,

    /// KyberSwap Aggregator API base URL.
    #[arg(long, default_value = "https://aggregator-api.kyberswap.com")]
    pub kyberswap_url: String,

    /// KyberSwap chain slug (must match the URL path segment, e.g. "ethereum", "base").
    #[arg(long, default_value = "ethereum")]
    pub kyberswap_chain: String,

    /// 0x Swap API v2 base URL.
    #[arg(long, default_value = "https://api.0x.org")]
    pub zerox_url: String,

    /// 0x API key (env: ZRX_API_KEY).
    #[arg(long, env = "ZRX_API_KEY", default_value = "")]
    pub zerox_api_key: String,

    /// EVM chain ID passed to aggregator APIs (1 = Ethereum mainnet).
    #[arg(long, default_value_t = 1)]
    pub chain_id: u64,

    /// Number of blocks to spread trades across. The full trade list is divided into this
    /// many chunks; each chunk fires on its own block. Larger values slow the run down but
    /// keep per-block request counts low for rate-limited aggregator APIs.
    #[arg(long, default_value_t = 10)]
    pub blocks: usize,

    /// Top-N token pairs ranked by trade frequency in the dataset.
    #[arg(long, default_value_t = 25)]
    pub top_pairs: usize,

    /// Number of representative amounts per pair (evenly-spaced percentiles).
    #[arg(long, default_value_t = 10)]
    pub amounts_per_pair: usize,

    /// Path to the 10k aggregator trade dataset (run `download-trades` to fetch it).
    /// Falls back to the embedded 50-trade sample when omitted.
    #[arg(long)]
    pub trade_data: Option<String>,

    /// Per-quote timeout in milliseconds for Fynd.
    #[arg(long, default_value_t = 10_000)]
    pub timeout_ms: u64,

    /// Maximum parallel in-flight requests (controls load on aggregator APIs).
    #[arg(long, default_value_t = 5)]
    pub concurrency: usize,

    /// Sample every Nth block. 1 = every block (~12 s apart), 5 = every ~60 s, etc.
    /// Use a higher value for long runs to reduce aggregator API load.
    #[arg(long, default_value_t = 1)]
    pub block_stride: usize,

    /// Minimum milliseconds between successive aggregator requests per aggregator.
    /// Increase (e.g. 500) to stay well under rate limits on long runs.
    #[arg(long, default_value_t = 0)]
    pub aggregator_delay_ms: u64,

    /// Path to write the full per-trade JSON results.
    #[arg(long, default_value = "audit_results.json")]
    pub output: String,

    /// Ethereum RPC URL for eth_call validation of Fynd quotes.
    ///
    /// When set, each successful Fynd quote is re-executed via `eth_call` at the
    /// quote block using state overrides to inject token balance and allowance.
    /// The on-chain result is compared against Fynd's quoted `amount_out` and
    /// stored as `eth_call_amount_out` / `eth_call_diff_bps` in the JSON output.
    /// Omit to skip validation entirely.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,

    /// Slippage tolerance passed to `EncodingOptions` when `--rpc-url` is set (basis points).
    /// Controls the `min_amount_out` encoded in the router calldata.
    #[arg(long, default_value_t = 50)]
    pub eth_call_slippage_bps: u32,

    /// Minimum approximate USD value for trade amounts sampled from the dataset.
    ///
    /// Trades below this threshold are excluded before percentile sampling, preventing
    /// sub-dust amounts from appearing as representative test cases. Prices are
    /// approximate and hardcoded for well-known tokens; unknown tokens are unaffected.
    /// Set to 0 to disable filtering (default).
    #[arg(long, default_value_t = 0.0)]
    pub min_amount_usd: f64,

    /// Upward adjustment applied to Fynd's eth_call amount-out (basis points).
    ///
    /// Scales the on-chain result up before storing `eth_call_amount_out` and recomputing
    /// `eth_call_diff_bps`. Use this to simulate zero-fee output: e.g. 10 removes an
    /// approximate 1 bps taker fee from the on-chain comparison.
    #[arg(long, default_value_t = 0)]
    pub eth_call_baseline_fee_bps: u32,
}

// ─── output types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AuditOutput {
    config: AuditConfig,
    results: Vec<TradeResult>,
}

#[derive(Serialize)]
struct AuditConfig {
    fynd_url: String,
    nordstern_url: String,
    chain_id: u64,
    top_pairs: usize,
    amounts_per_pair: usize,
    blocks_used: usize,
    trades_per_block: usize,
    block_stride: usize,
    total_trades: usize,
}

/// Per-participant result — identical shape for Fynd and every external aggregator.
///
/// The first entry in `TradeResult::participants` is always `"fynd"` (the baseline).
/// `amount_out_net_gas` is populated only by Fynd; all other aggregators leave it `null`.
#[derive(Serialize)]
struct ParticipantResult {
    name: String,
    status: String,
    amount_out: Option<String>,
    /// Net-of-gas output estimate (Fynd only; aggregators set this to `null`).
    amount_out_net_gas: Option<String>,
    /// Reported gas units (Fynd `gas_estimate` or aggregator self-reported gas).
    gas_units: Option<u64>,
    protocols: Vec<String>,
    /// Parallel sub-routes (aggregators only; `null` for Fynd).
    num_splits: Option<usize>,
    /// Wall-clock time from request dispatch to first byte of response.
    response_time_ms: Option<u64>,
    /// Amount of `token_out` returned by `eth_call` at the quote block.
    eth_call_amount_out: Option<String>,
    /// Difference between `eth_call_amount_out` and `amount_out` in bps.
    eth_call_diff_bps: Option<f64>,
    /// Actual gas consumed by the swap as reported by `eth_simulateV1`.
    eth_call_gas_used: Option<u64>,
    /// Raw bps diff vs Fynd (positive = Fynd better). `null` for the Fynd entry itself.
    raw_diff_bps: Option<f64>,
    /// Gas-adjusted diff using self-reported gas (Fynd `gas_units` and aggregator `gas_units`).
    gas_adjusted_diff_bps_reported: Option<f64>,
    /// Gas-adjusted diff using on-chain gas from `eth_simulateV1`.
    gas_adjusted_diff_bps_onchain: Option<f64>,
}

#[derive(Serialize)]
struct TradeResult {
    block_sample: usize,
    /// Hex-encoded block hash at which all quotes and eth_calls were pinned.
    block_hash: Option<String>,
    pair: String,
    token_in: String,
    token_out: String,
    amount_in: String,
    /// Index into the percentile array (0 = min, amounts_per_pair-1 = max traded amount).
    amount_percentile_idx: usize,
    /// All participants: first entry is `"fynd"`, remainder are aggregators.
    participants: Vec<ParticipantResult>,
}

// ─── internal task payload ────────────────────────────────────────────────────

struct QuoteTask {
    block_sample: usize,
    pair: PairSpec,
    amount: String,
    amount_percentile_idx: usize,
}

// ─── eth_call validation ──────────────────────────────────────────────────────

// ─── eth_simulateV1 helpers ───────────────────────────────────────────────────

/// Sentinel address for the inline ETH-balance reader injected into eth_simulateV1 state.
/// Chosen to be obviously synthetic and not a real mainnet contract.
const ETH_BALANCE_READER_ADDR: Address = Address(alloy::primitives::FixedBytes([
    0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0,
    0xf0, 0xf0, 0xf0, 0xf0,
]));

/// EVM bytecode: CALLER BALANCE PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
/// Returns the ETH balance of the calling address as a uint256.
const ETH_BALANCE_READER_CODE: [u8; 10] =
    [0x33, 0x31, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

/// Validates quotes by re-executing encoded calldata via simulation at the relevant block,
/// using state overrides to inject token balance and allowance.
///
/// Prefers `eth_simulateV1` (balance-delta: swap + `balanceOf` in one block) for an exact
/// output amount independent of router ABI conventions. Falls back to a plain `eth_call`
/// with return-data heuristics when the RPC does not support `eth_simulateV1`.
struct EthCallRunner {
    provider: Arc<RootProvider<Ethereum>>,
    /// Fixed sender used in all audit quotes — overridden in state to hold sufficient balance.
    sender: Address,
    /// Per-token balance storage slot (probed once, cached for the run).
    balance_slots: Arc<Mutex<HashMap<Address, B256>>>,
    /// Per-(token, spender) allowance storage slot (probed once, cached for the run).
    allowance_slots: Arc<Mutex<HashMap<(Address, Address), B256>>>,
    /// Set to `false` after the first `eth_simulateV1` "method not found" error so subsequent
    /// calls skip straight to the `eth_call` fallback without retrying.
    simulate_supported: Arc<AtomicBool>,
}

impl EthCallRunner {
    fn new(provider: RootProvider<Ethereum>) -> Self {
        // Generate a fresh random address so it has no pre-existing token balances.
        let mut bytes = [0u8; 20];
        fastrand::fill(&mut bytes);
        let sender = Address::from(bytes);
        Self {
            provider: Arc::new(provider),
            sender,
            balance_slots: Arc::new(Mutex::new(HashMap::new())),
            allowance_slots: Arc::new(Mutex::new(HashMap::new())),
            simulate_supported: Arc::new(AtomicBool::new(true)),
        }
    }

    fn sender_hex(&self) -> String {
        format!("0x{}", hex::encode(self.sender.as_slice()))
    }

    /// Execute `calldata` against `router` at `block`, returning the actual `token_out`
    /// amount received by `self.sender` and, when `eth_simulateV1` is used, the gas consumed.
    ///
    /// Tries `eth_simulateV1` first (balance-delta approach). Falls back to `eth_call` +
    /// return-data parsing when the RPC does not support `eth_simulateV1`. Gas is `None`
    /// on the fallback path.
    async fn execute(
        &self,
        token_in: Address,
        token_out: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<(BigUint, Option<u64>)>> {
        if self
            .simulate_supported
            .load(Ordering::Relaxed)
        {
            match self
                .try_simulate(token_in, token_out, router, calldata, value, block)
                .await
            {
                Ok(result) => return Ok(result.map(|(amount, gas)| (amount, Some(gas)))),
                Err(e) => {
                    if is_method_not_found(&e) {
                        self.simulate_supported
                            .store(false, Ordering::Relaxed);
                        warn!(
                            "eth_simulateV1 not supported on this RPC — falling back to eth_call"
                        );
                    } else {
                        warn!("eth_simulateV1 failed: {e}");
                    }
                }
            }
        }
        let (amount_res, gas_res) = tokio::join!(
            self.try_eth_call(token_in, router, calldata, value, block),
            self.try_estimate_gas(token_in, router, calldata, value, block),
        );
        let gas = gas_res.ok().flatten();
        amount_res.map(|opt| opt.map(|amount| (amount, gas)))
    }

    /// Run the swap and a balance measurement in the same simulated block via `eth_simulateV1`,
    /// returning the exact amount of `token_out` received and the gas consumed by the swap call.
    ///
    /// For ERC-20 output: second call is `balanceOf(sender)` on the token contract.
    /// For native ETH output: second call reads `sender`'s ETH balance via an inline helper
    /// contract injected into the simulation state. Sender's ETH balance is zeroed and
    /// `gasPrice=0` is set on the swap so that final balance == ETH received exactly.
    /// This approach works for any router regardless of its return-data ABI conventions.
    async fn try_simulate(
        &self,
        token_in: Address,
        token_out: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<(BigUint, u64)>> {
        let mut overrides = self
            .build_overrides(token_in, router)
            .await?;

        let (swap_call_json, second_call_json) = if token_out == Address::ZERO {
            // Zero the sender's ETH balance so (balance after swap) == ETH received.
            // gasPrice=0 ensures gas cost doesn't reduce that delta.
            // Inject a 10-byte balance-reader at a reserved address: calling it returns
            // the caller's ETH balance as a uint256.
            overrides.insert(
                self.sender,
                AccountOverride { balance: Some(U256::ZERO), ..Default::default() },
            );
            overrides.insert(
                ETH_BALANCE_READER_ADDR,
                AccountOverride {
                    code: Some(alloy::primitives::Bytes(Bytes::from_static(
                        &ETH_BALANCE_READER_CODE,
                    ))),
                    ..Default::default()
                },
            );
            (
                serde_json::json!({
                    "from":     format!("0x{}", hex::encode(self.sender)),
                    "to":       format!("0x{}", hex::encode(router)),
                    "data":     format!("0x{}", hex::encode(calldata)),
                    "value":    format!("0x{:x}", value),
                    "gasPrice": "0x0",
                }),
                serde_json::json!({
                    "from":     format!("0x{}", hex::encode(self.sender)),
                    "to":       format!("0x{}", hex::encode(ETH_BALANCE_READER_ADDR)),
                    "gasPrice": "0x0",
                }),
            )
        } else {
            // ERC-20 output: standard balanceOf(sender) on token_out.
            let mut bal_data = vec![0x70u8, 0xa0, 0x82, 0x31];
            bal_data.extend_from_slice(&[0u8; 12]);
            bal_data.extend_from_slice(self.sender.as_slice());
            (
                serde_json::json!({
                    "from":  format!("0x{}", hex::encode(self.sender)),
                    "to":    format!("0x{}", hex::encode(router)),
                    "data":  format!("0x{}", hex::encode(calldata)),
                    "value": format!("0x{:x}", value),
                }),
                serde_json::json!({
                    "from": "0x0000000000000000000000000000000000000000",
                    "to":   format!("0x{}", hex::encode(token_out)),
                    "data": format!("0x{}", hex::encode(&bal_data)),
                }),
            )
        };

        let state_overrides =
            serde_json::to_value(&overrides).context("serialise state overrides")?;

        let block_param = serde_json::json!({
            "blockHash": format!("0x{}", hex::encode(block.as_slice())),
            "requireCanonical": false,
        });

        let params = serde_json::json!([
            {
                "blockStateCalls": [{
                    "stateOverrides": state_overrides,
                    "calls": [swap_call_json, second_call_json]
                }]
            },
            block_param
        ]);

        let result: serde_json::Value = self
            .provider
            .raw_request("eth_simulateV1".into(), params)
            .await
            .context("eth_simulateV1")?;

        let calls = result
            .get(0)
            .and_then(|b| b.get("calls"))
            .and_then(|c| c.as_array())
            .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: missing calls array in result"))?;

        // Check that the swap call succeeded (status 0x1).
        let swap_call_result = calls
            .first()
            .ok_or_else(|| anyhow::anyhow!("eth_simulateV1: calls array is empty"))?;
        let swap_ok = swap_call_result
            .get("status")
            .map(|s| s == "0x1" || s.as_u64() == Some(1))
            .unwrap_or(false);
        if !swap_ok {
            warn!("eth_simulateV1: swap call reverted");
            return Ok(None);
        }

        // gasUsed may be a hex string ("0x...") or a JSON integer depending on the RPC provider.
        let gas_used = swap_call_result
            .get("gasUsed")
            .and_then(|g| {
                g.as_str()
                    .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                    .or_else(|| g.as_u64())
            })
            .unwrap_or(0);

        // Both paths use the same decoding: calls[1].returnData is a uint256 (ERC-20
        // balance or ETH balance from the inline reader).
        let amount_hex = calls
            .get(1)
            .and_then(|c| c.get("returnData"))
            .and_then(|d| d.as_str())
            .unwrap_or("0x");
        let amount_bytes =
            hex::decode(amount_hex.trim_start_matches("0x")).context("decode amount return")?;

        if amount_bytes.len() >= 32 {
            Ok(Some((BigUint::from_bytes_be(&amount_bytes[..32]), gas_used)))
        } else {
            Ok(None)
        }
    }

    /// Fallback when `eth_simulateV1` is unavailable: plain `eth_call` with heuristic
    /// return-data decoding.
    async fn try_eth_call(
        &self,
        token_in: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<BigUint>> {
        let overrides = self
            .build_overrides(token_in, router)
            .await?;

        let value_u128: u128 = value.to_string().parse().unwrap_or(0);
        let tx = TransactionRequest {
            from: Some(self.sender),
            to: Some(TxKind::Call(router)),
            input: alloy::primitives::Bytes::copy_from_slice(calldata).into(),
            value: Some(alloy::primitives::U256::from(value_u128)),
            max_fee_per_gas: Some(0),
            max_priority_fee_per_gas: Some(0),
            ..Default::default()
        };

        let result = self
            .provider
            .call(tx)
            .overrides(overrides)
            .block(BlockId::Hash(RpcBlockHash {
                block_hash: block,
                require_canonical: Some(false),
            }))
            .await;

        let return_data = match result {
            Ok(d) => d,
            Err(e) => {
                warn!("eth_call at block {block} reverted: {e}");
                return Ok(None);
            }
        };

        Ok(parse_return_amount(&return_data))
    }

    /// Gas estimate via `eth_estimateGas` with state overrides.
    ///
    /// Used as a fallback when `eth_simulateV1` is unavailable. Returns the estimated gas
    /// units including the 21 000-unit base transaction cost.
    async fn try_estimate_gas(
        &self,
        token_in: Address,
        router: Address,
        calldata: &[u8],
        value: &BigUint,
        block: B256,
    ) -> anyhow::Result<Option<u64>> {
        let overrides = self
            .build_overrides(token_in, router)
            .await?;
        let state_overrides =
            serde_json::to_value(&overrides).context("serialise state overrides")?;

        let block_param = serde_json::json!({
            "blockHash": format!("0x{}", hex::encode(block.as_slice())),
            "requireCanonical": false,
        });

        let params = serde_json::json!([
            {
                "from": format!("0x{}", hex::encode(self.sender)),
                "to":   format!("0x{}", hex::encode(router)),
                "data": format!("0x{}", hex::encode(calldata)),
                "value": format!("0x{:x}", value),
            },
            block_param,
            state_overrides,
        ]);

        let result: serde_json::Value = match self
            .provider
            .raw_request("eth_estimateGas".into(), params)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("eth_estimateGas at block {block} failed: {e}");
                return Ok(None);
            }
        };

        let gas = result
            .as_str()
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .or_else(|| result.as_u64());

        Ok(gas)
    }

    /// Build a `StateOverride` that injects sufficient balance and allowance for `sender`
    /// to spend `token_in` via `router`. Probes storage slots on first use and caches.
    ///
    /// Always injects a large ETH balance for `sender` so that `eth_simulateV1` can charge
    /// gas without rejecting the transaction. For ERC-20 tokens the ERC-20 balance and
    /// allowance storage slots are also overridden. For native ETH (zero address) only the
    /// ETH balance override is needed.
    async fn build_overrides(
        &self,
        token_in: Address,
        router: Address,
    ) -> anyhow::Result<StateOverride> {
        // Use MAX >> 1: avoids triggering tokens that pack metadata into bit 255 (e.g. USDC).
        let max_val = B256::from(U256::MAX >> 1);
        let eth_balance = U256::MAX >> 1;

        let mut overrides = StateOverride::default();

        // Give sender unlimited ETH — required for eth_simulateV1 to charge gas without
        // failing. Harmless for eth_call which does not deduct gas from the sender balance.
        overrides.insert(
            self.sender,
            AccountOverride { balance: Some(eth_balance), ..Default::default() },
        );

        if token_in != Address::ZERO {
            let balance_slot = self.balance_slot(token_in).await?;
            let allowance_slot = self
                .allowance_slot(token_in, router)
                .await?;

            let mut state_diff = B256HashMap::default();
            state_diff.insert(balance_slot, max_val);
            state_diff.insert(allowance_slot, max_val);

            overrides.insert(
                token_in,
                AccountOverride { state_diff: Some(state_diff), ..Default::default() },
            );
        }

        Ok(overrides)
    }

    async fn balance_slot(&self, token: Address) -> anyhow::Result<B256> {
        {
            let cache = self.balance_slots.lock().await;
            if let Some(&slot) = cache.get(&token) {
                return Ok(slot);
            }
        }
        let slot = erc20::find_balance_slot(&self.provider, token, self.sender).await?;
        self.balance_slots
            .lock()
            .await
            .insert(token, slot);
        Ok(slot)
    }

    async fn allowance_slot(&self, token: Address, spender: Address) -> anyhow::Result<B256> {
        {
            let cache = self.allowance_slots.lock().await;
            if let Some(&slot) = cache.get(&(token, spender)) {
                return Ok(slot);
            }
        }
        let slot = erc20::find_allowance_slot(&self.provider, token, self.sender, spender).await?;
        self.allowance_slots
            .lock()
            .await
            .insert((token, spender), slot);
        Ok(slot)
    }
}

// ─── Fynd as AggregatorClient ─────────────────────────────────────────────────

/// Wraps [`FyndClient`] so it participates in the same quote loop as external aggregators.
///
/// Kept separate from the solver `FyndClient` used for health-checks and block-waiting so
/// those concerns don't bleed into the aggregator abstraction.
struct FyndAggregator {
    client: Arc<FyndClient>,
    timeout_ms: u64,
    encoding_slippage: f64,
}

#[async_trait::async_trait]
impl AggregatorClient for FyndAggregator {
    fn name(&self) -> &str {
        "fynd"
    }

    async fn quote(
        &self,
        token_in: &str,
        token_out: &str,
        amount: &str,
        wallet: Option<&str>,
    ) -> anyhow::Result<AggregatorQuote> {
        use std::time::Instant;
        let start = Instant::now();

        let params = make_quote_params(
            token_in,
            token_out,
            amount,
            self.timeout_ms,
            wallet,
            wallet
                .is_some()
                .then(|| EncodingOptions::new(self.encoding_slippage)),
        )?;

        let q = self
            .client
            .quote(params)
            .await
            .map_err(|e| anyhow::anyhow!("Fynd quote failed: {e}"))?;

        let response_time_ms = start.elapsed().as_millis() as u64;
        let success = q.status() == QuoteStatus::Success;

        let protocols = q
            .route()
            .map(|r| {
                r.swaps()
                    .iter()
                    .map(|s| s.protocol().to_string())
                    .collect()
            })
            .unwrap_or_default();

        let calldata = success
            .then(|| q.transaction())
            .flatten()
            .and_then(|tx| {
                if tx.to().len() != 20 {
                    return None;
                }
                Some(AggregatorCalldata {
                    to: format!("0x{}", hex::encode(tx.to())),
                    data: format!("0x{}", hex::encode(tx.data())),
                    value: tx.value().to_string(),
                })
            });

        let nonzero_str = |s: String| (s != "0").then_some(s);

        Ok(AggregatorQuote {
            status: fynd_status_to_agg(q.status()),
            amount_out: success
                .then(|| nonzero_str(q.amount_out().to_string()))
                .flatten(),
            amount_out_net_gas: success
                .then(|| nonzero_str(q.amount_out_net_gas().to_string()))
                .flatten(),
            gas_units: q
                .gas_estimate()
                .to_string()
                .parse::<u64>()
                .ok()
                .filter(|&v| v > 0),
            protocols,
            num_splits: None,
            response_time_ms,
            calldata,
        })
    }
}

fn fynd_status_to_agg(status: QuoteStatus) -> AggregatorStatus {
    match status {
        QuoteStatus::Success => AggregatorStatus::Success,
        QuoteStatus::NoRouteFound | QuoteStatus::InsufficientLiquidity => AggregatorStatus::NoRoute,
        QuoteStatus::Timeout | QuoteStatus::NotReady | QuoteStatus::PriceCheckFailed => {
            AggregatorStatus::NoAmount
        }
    }
}

// ─── entry point ──────────────────────────────────────────────────────────────

pub async fn run(args: Args) -> Result<()> {
    // Load trade dataset to derive pairs and amounts.
    let trades = match &args.trade_data {
        Some(path) => load_all_templates_from_file(path)
            .map_err(|e| anyhow::anyhow!("failed to load trade data from {path}: {e}"))?,
        None => {
            warn!(
                "No --trade-data provided; using embedded 50-trade sample. \
                   Run `download-trades` for the full 10k dataset."
            );
            load_all_embedded_templates()
        }
    };

    let pairs =
        select_top_pairs(&trades, args.top_pairs, args.amounts_per_pair, args.min_amount_usd);
    if pairs.is_empty() {
        anyhow::bail!("no valid pairs found in trade dataset");
    }

    info!("Derived {} pairs from trade dataset ({} trades).", pairs.len(), trades.len());
    for p in &pairs {
        info!(
            "  {} ({} amounts: {}..{})",
            p.label,
            p.amounts.len(),
            p.amounts
                .first()
                .unwrap_or(&String::new()),
            p.amounts
                .last()
                .unwrap_or(&String::new())
        );
    }

    // Build the FyndClient — used for health-checks and block-waiting only.
    let fynd = Arc::new(
        FyndClientBuilder::new(&args.fynd_url, "")
            .with_timeout(Duration::from_millis(args.timeout_ms))
            .with_retry(RetryConfig::new(1, Duration::from_millis(0), Duration::from_millis(0)))
            .build_quote_only()
            .map_err(|e| anyhow::anyhow!("failed to build Fynd client: {e}"))?,
    );

    // Build the eth_call runner when --rpc-url is provided.
    let eth_call_slippage = args.eth_call_slippage_bps as f64 / 10_000.0;
    let eth_call_runner: Option<Arc<EthCallRunner>> = match &args.rpc_url {
        Some(rpc_url) => {
            let url = rpc_url
                .parse::<reqwest::Url>()
                .with_context(|| format!("invalid --rpc-url: {rpc_url}"))?;
            let provider = ProviderBuilder::default().connect_http(url);
            let runner = Arc::new(EthCallRunner::new(provider));
            info!(
                "eth_call validation enabled (rpc: {rpc_url}, sender: {}, slippage: {:.2}%)",
                runner.sender_hex(),
                eth_call_slippage * 100.0
            );
            Some(runner)
        }
        None => None,
    };

    // Fynd is the first participant (baseline); external aggregators follow.
    let mut participants: Vec<Arc<dyn AggregatorClient>> = vec![Arc::new(FyndAggregator {
        client: fynd.clone(),
        timeout_ms: args.timeout_ms,
        encoding_slippage: eth_call_slippage,
    })];
    participants.push(Arc::new(NordsternClient::new(&args.nordstern_url, args.chain_id)?));
    participants.push(Arc::new(KyberswapClient::new(&args.kyberswap_url, &args.kyberswap_chain)?));
    if !args.zerox_api_key.is_empty() {
        participants.push(Arc::new(ZeroExClient::new(
            &args.zerox_url,
            args.chain_id,
            &args.zerox_api_key,
        )?));
    }

    // Health-check Fynd.
    let health = fynd
        .health()
        .await
        .context("Fynd health check failed — is the solver running?")?;
    if !health.healthy() {
        anyhow::bail!(
            "Fynd solver is not healthy (last_update={}ms, pools={})",
            health.last_update_ms(),
            health.num_solver_pools()
        );
    }
    info!(
        "Fynd healthy — {} solver pools, last update {}ms ago.",
        health.num_solver_pools(),
        health.last_update_ms()
    );

    // Divide the full trade list into per-block chunks.
    // Each trade fires exactly once on its assigned block, keeping per-block request
    // counts low so we stay within aggregator rate limits.
    let all_specs: Vec<(usize, usize)> = (0..pairs.len())
        .flat_map(|pi| (0..pairs[pi].amounts.len()).map(move |ai| (pi, ai)))
        .collect();

    let total_trades = all_specs.len();
    let n_blocks = args.blocks.min(total_trades).max(1);
    let chunk_size = total_trades.div_ceil(n_blocks);
    let chunks: Vec<&[(usize, usize)]> = all_specs.chunks(chunk_size).collect();
    let actual_blocks = chunks.len();

    info!(
        "Spreading {} trades across {} blocks (~{} trades/block, ~{} agg requests/block).",
        total_trades,
        actual_blocks,
        chunk_size,
        chunk_size * (participants.len() - 1),
    );

    let mut all_results: Vec<TradeResult> = Vec::with_capacity(total_trades);

    for (block_idx, chunk) in chunks.iter().enumerate() {
        if block_idx == 0 {
            info!("Waiting for first block…");
            wait_for_fresh_block(&fynd).await?;
        } else {
            info!("Waiting for block {} of {}…", block_idx + 1, actual_blocks);
            wait_for_next_sample(&fynd, args.block_stride).await?;
        }

        // Fetch the canonical block hash once for this batch so every quote and
        // eth_call in the chunk is pinned to the same block state.
        let quote_block: Option<B256> = if let Some(runner) = &eth_call_runner {
            match runner
                .provider
                .get_block_by_number(Default::default())
                .await
            {
                Ok(Some(block)) => Some(block.header.hash),
                Ok(None) => {
                    warn!("could not fetch latest block for eth_call validation");
                    None
                }
                Err(e) => {
                    warn!("could not fetch block for eth_call validation: {e}");
                    None
                }
            }
        } else {
            None
        };

        info!("  Block ready — firing {} quote pairs…", chunk.len());

        let sem = Arc::new(Semaphore::new(args.concurrency));
        let mut jset: JoinSet<Result<TradeResult>> = JoinSet::new();

        for &(pair_idx, amount_idx) in chunk.iter() {
            let pair = pairs[pair_idx].clone();
            let amount = pair.amounts[amount_idx].clone();
            let participants = participants.clone();
            let sem = sem.clone();
            let block_sample = block_idx + 1;
            let aggregator_delay_ms = args.aggregator_delay_ms;
            let baseline_fee_bps = args.eth_call_baseline_fee_bps;
            let runner = eth_call_runner.clone();

            jset.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .expect("semaphore acquire");
                execute_trade(
                    participants,
                    QuoteTask { block_sample, pair, amount, amount_percentile_idx: amount_idx },
                    TradeConfig {
                        aggregator_delay_ms,
                        eth_call_runner: runner,
                        quote_block,
                        baseline_fee_bps,
                    },
                )
                .await
            });
        }

        let mut block_results = 0usize;
        while let Some(result) = jset.join_next().await {
            match result {
                Ok(Ok(trade)) => {
                    all_results.push(trade);
                    block_results += 1;
                }
                Ok(Err(e)) => warn!("trade task error: {e}"),
                Err(e) => warn!("task panicked: {e}"),
            }
        }
        info!("  Collected {block_results}/{} results.", chunk.len());
    }

    // Write JSON output.
    let output = AuditOutput {
        config: AuditConfig {
            fynd_url: args.fynd_url.clone(),
            nordstern_url: args.nordstern_url.clone(),
            chain_id: args.chain_id,
            top_pairs: args.top_pairs,
            amounts_per_pair: args.amounts_per_pair,
            blocks_used: actual_blocks,
            trades_per_block: chunk_size,
            block_stride: args.block_stride,
            total_trades: all_results.len(),
        },
        results: all_results,
    };

    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(&args.output, &json)?;
    info!("Full results saved to: {}", args.output);

    print_summary(&output.results);

    Ok(())
}

// ─── per-trade execution ───────────────────────────────────────────────────────

struct TradeConfig {
    aggregator_delay_ms: u64,
    eth_call_runner: Option<Arc<EthCallRunner>>,
    quote_block: Option<B256>,
    baseline_fee_bps: u32,
}

async fn execute_trade(
    participants: Vec<Arc<dyn AggregatorClient>>,
    task: QuoteTask,
    cfg: TradeConfig,
) -> Result<TradeResult> {
    let TradeConfig { aggregator_delay_ms, eth_call_runner, quote_block, baseline_fee_bps } = cfg;

    // When validation is enabled all calldata must route to the runner's sender so the
    // balance-delta check in try_simulate reads the right account.
    let wallet: Option<String> = eth_call_runner
        .as_ref()
        .map(|r| r.sender_hex());

    // Fire all participants concurrently; delay all except the first (Fynd) to rate-limit
    // external aggregator APIs.
    let futs: Vec<_> = participants
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let p = p.clone();
            let token_in = task.pair.token_in.clone();
            let token_out = task.pair.token_out.clone();
            let amount = task.amount.clone();
            let wallet = wallet.clone();
            async move {
                if i > 0 && aggregator_delay_ms > 0 {
                    sleep(Duration::from_millis(aggregator_delay_ms)).await;
                }
                let name = p.name().to_string();
                let result = p
                    .quote(&token_in, &token_out, &amount, wallet.as_deref())
                    .await;
                (name, result)
            }
        })
        .collect();

    let raw_results: Vec<(String, anyhow::Result<AggregatorQuote>)> =
        futures::future::join_all(futs).await;

    // Run eth_call validation for all participants that carry calldata.
    let eth_calls: Vec<(Option<String>, Option<f64>, Option<u64>)> =
        futures::future::join_all(raw_results.iter().map(|(_, r)| {
            let (calldata, quoted) = match r.as_ref().ok() {
                Some(q) => (q.calldata.as_ref(), q.amount_out.as_deref()),
                None => (None, None),
            };
            run_eth_call_for_calldata(
                calldata,
                quoted,
                eth_call_runner.as_deref(),
                &task.pair.token_in,
                &task.pair.token_out,
                quote_block,
            )
        }))
        .await;

    // Baseline is participants[0] (Fynd). Diffs for index > 0 are vs the baseline.
    let (baseline_amount, baseline_net_gas, baseline_gas_u, baseline_eth_call_gas) =
        match raw_results.first() {
            Some((_, Ok(q))) if q.is_success() => (
                q.amount_out
                    .as_deref()
                    .unwrap_or("0")
                    .to_string(),
                q.amount_out_net_gas.clone(),
                q.gas_units.unwrap_or(0),
                eth_calls
                    .first()
                    .and_then(|(_, _, g)| *g),
            ),
            _ => (String::new(), None, 0, None),
        };
    // Fee-adjusted Fynd eth_call amount — used only for the gas-adjusted onchain diff so
    // that the onchain comparison reflects what Fynd would return without taker fees.
    let baseline_ec_fee_adj: String = eth_calls
        .first()
        .and_then(|(amt, _, _)| amt.as_deref())
        .map(|s| {
            if baseline_fee_bps > 0 {
                s.parse::<BigUint>()
                    .ok()
                    .map(|v| (v * (10_000u32 + baseline_fee_bps) / 10_000u32).to_string())
                    .unwrap_or_else(|| s.to_string())
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default();
    let baseline_ok = !baseline_amount.is_empty() && baseline_amount != "0";

    let participant_results: Vec<ParticipantResult> = raw_results
        .into_iter()
        .zip(eth_calls)
        .enumerate()
        .map(|(idx, ((name, result), (ec_amount, ec_diff, ec_gas)))| {
            // For the baseline (Fynd), optionally scale up the eth_call result to simulate
            // zero-fee output. Recomputes ec_diff against the (unchanged) quoted amount_out.
            let (ec_amount, ec_diff) = if idx == 0 && baseline_fee_bps > 0 {
                let parsed_actual: Option<BigUint> = ec_amount
                    .as_deref()
                    .and_then(|s| s.parse().ok());
                match parsed_actual {
                    Some(actual) => {
                        let scaled = &actual * (10_000u32 + baseline_fee_bps) / 10_000u32;
                        let new_diff = result
                            .as_ref()
                            .ok()
                            .and_then(|q| q.amount_out.as_deref())
                            .and_then(|s| s.parse::<BigUint>().ok())
                            .filter(|q| *q > BigUint::ZERO)
                            .and_then(|q| eth_call_bps_diff(&scaled, &q));
                        (Some(scaled.to_string()), new_diff)
                    }
                    None => (ec_amount, ec_diff),
                }
            } else {
                (ec_amount, ec_diff)
            };

            let (raw_diff, gas_reported, gas_onchain) = if idx > 0 && baseline_ok {
                if let Ok(q) = &result {
                    if q.is_success() {
                        let other_raw = q.amount_out.as_deref().unwrap_or("0");
                        (
                            raw_bps_diff(&baseline_amount, other_raw),
                            gas_adjusted_bps_diff(
                                &baseline_amount,
                                baseline_net_gas.as_deref(),
                                baseline_gas_u,
                                other_raw,
                                q.gas_units.unwrap_or(0),
                            ),
                            gas_adjusted_bps_diff(
                                &baseline_ec_fee_adj,
                                baseline_net_gas.as_deref(),
                                baseline_eth_call_gas.unwrap_or(0),
                                ec_amount.as_deref().unwrap_or("0"),
                                ec_gas.unwrap_or(0),
                            ),
                        )
                    } else {
                        (None, None, None)
                    }
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };

            match result {
                Ok(q) => ParticipantResult {
                    name,
                    status: q.status.to_string(),
                    amount_out: q.amount_out,
                    amount_out_net_gas: q.amount_out_net_gas,
                    gas_units: q.gas_units,
                    protocols: q.protocols,
                    num_splits: q.num_splits,
                    response_time_ms: Some(q.response_time_ms),
                    eth_call_amount_out: ec_amount,
                    eth_call_diff_bps: ec_diff,
                    eth_call_gas_used: ec_gas,
                    raw_diff_bps: raw_diff,
                    gas_adjusted_diff_bps_reported: gas_reported,
                    gas_adjusted_diff_bps_onchain: gas_onchain,
                },
                Err(e) => {
                    warn!("participant {name} request failed: {e:#}");
                    ParticipantResult {
                        name,
                        status: format!("error: {e}"),
                        amount_out: None,
                        amount_out_net_gas: None,
                        gas_units: None,
                        protocols: vec![],
                        num_splits: None,
                        response_time_ms: None,
                        eth_call_amount_out: None,
                        eth_call_diff_bps: None,
                        eth_call_gas_used: None,
                        raw_diff_bps: None,
                        gas_adjusted_diff_bps_reported: None,
                        gas_adjusted_diff_bps_onchain: None,
                    }
                }
            }
        })
        .collect();

    Ok(TradeResult {
        block_sample: task.block_sample,
        block_hash: quote_block.map(|h| format!("0x{}", hex::encode(h.as_slice()))),
        pair: task.pair.label.clone(),
        token_in: task.pair.token_in,
        token_out: task.pair.token_out,
        amount_in: task.amount,
        amount_percentile_idx: task.amount_percentile_idx,
        participants: participant_results,
    })
}

// ─── eth_call helper ─────────────────────────────────────────────────────────

/// Decode a 0x-prefixed hex string into a 20-byte `Address`, returning `None` on any error.
fn parse_address_hex(hex: &str) -> Option<Address> {
    let bytes = hex::decode(hex.trim_start_matches("0x")).ok()?;
    (bytes.len() == 20).then(|| Address::from_slice(&bytes))
}

/// Returns `true` when `e` indicates that the called JSON-RPC method does not exist on this RPC.
///
/// Prefers the standard error code `-32601` (method not found). Falls back to narrow
/// phrase matching for providers that return non-standard error shapes.
fn is_method_not_found(e: &anyhow::Error) -> bool {
    // Walk the anyhow error chain and try to downcast to a typed RPC error.
    for cause in e.chain() {
        if let Some(rpc) = cause.downcast_ref::<RpcError<TransportErrorKind>>() {
            return matches!(rpc, RpcError::ErrorResp(p) if p.code == -32601);
        }
    }
    // Fallback for providers that surface the error as plain text.
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("method not found") || msg.contains("does not exist")
}

/// Run eth_call validation for a participant that carries encoded calldata.
///
/// Returns `(eth_call_amount_out, eth_call_diff_bps, eth_call_gas_used)`. All `None` when
/// the runner is absent, calldata is absent, addresses are invalid, or the call reverts.
async fn run_eth_call_for_calldata(
    calldata: Option<&AggregatorCalldata>,
    quoted_amount: Option<&str>,
    runner: Option<&EthCallRunner>,
    token_in_hex: &str,
    token_out_hex: &str,
    block: Option<B256>,
) -> (Option<String>, Option<f64>, Option<u64>) {
    let Some(runner) = runner else {
        return (None, None, None);
    };
    let Some(block) = block else {
        return (None, None, None);
    };
    let Some(cd) = calldata else {
        return (None, None, None);
    };

    let Some(token_in) = parse_address_hex(token_in_hex) else {
        warn!("eth_call: invalid token_in address {token_in_hex}");
        return (None, None, None);
    };
    let Some(token_out) = parse_address_hex(token_out_hex) else {
        warn!("eth_call: invalid token_out address {token_out_hex}");
        return (None, None, None);
    };

    // Aggregators use 0xeeee…eeee as a sentinel for native ETH; normalise to zero address
    // so build_overrides skips the ERC-20 slot probe and try_simulate uses the ETH path.
    let eth_sentinel = Address::from([0xeeu8; 20]);
    let token_in = if token_in == eth_sentinel { Address::ZERO } else { token_in };
    let token_out = if token_out == eth_sentinel { Address::ZERO } else { token_out };

    let Some(router) = parse_address_hex(&cd.to) else {
        warn!("eth_call: invalid router address '{}'", cd.to);
        return (None, None, None);
    };

    let calldata_bytes = hex::decode(cd.data.trim_start_matches("0x")).unwrap_or_default();
    let value: BigUint = cd.value.parse().unwrap_or_default();

    match runner
        .execute(token_in, token_out, router, &calldata_bytes, &value, block)
        .await
    {
        Ok(Some((actual, gas_used))) => {
            let diff_bps = quoted_amount
                .and_then(|q| q.parse::<BigUint>().ok())
                .filter(|q| *q > BigUint::ZERO)
                .and_then(|q| eth_call_bps_diff(&actual, &q));
            (Some(actual.to_string()), diff_bps, gas_used)
        }
        Ok(None) => (None, None, None),
        Err(e) => {
            warn!("eth_call validation failed: {e}");
            (None, None, None)
        }
    }
}

// ─── block-detection helpers ──────────────────────────────────────────────────

/// Poll until `last_update_ms < 3 s`, indicating a fresh block just processed.
async fn wait_for_fresh_block(fynd: &FyndClient) -> Result<()> {
    loop {
        sleep(Duration::from_millis(500)).await;
        let health = fynd
            .health()
            .await
            .context("health check failed while waiting for block")?;
        if health.last_update_ms() < 3_000 {
            return Ok(());
        }
    }
}

/// Poll until `last_update_ms > 8 s` so the next `wait_for_fresh_block` catches a new block.
async fn wait_until_stale(fynd: &FyndClient) -> Result<()> {
    loop {
        sleep(Duration::from_secs(2)).await;
        let health = fynd
            .health()
            .await
            .context("health check failed while waiting for stale")?;
        if health.last_update_ms() > 8_000 {
            return Ok(());
        }
    }
}

/// Wait until stale, then skip `stride - 1` additional Ethereum block periods (~12 s each),
/// then sync to the next fresh block. With stride=1 this is identical to the original behaviour.
async fn wait_for_next_sample(fynd: &FyndClient, stride: usize) -> Result<()> {
    wait_until_stale(fynd).await?;
    if stride > 1 {
        sleep(Duration::from_secs(12 * (stride - 1) as u64)).await;
    }
    wait_for_fresh_block(fynd).await
}

// ─── summary printing ─────────────────────────────────────────────────────────

fn print_summary(results: &[TradeResult]) {
    let total = results.len();
    info!("{}", "=".repeat(80));
    info!("  FYND AUDIT RESULTS  ({total} trades)");
    info!("{}", "=".repeat(80));

    let baseline_name: String = results
        .iter()
        .find_map(|r| {
            r.participants
                .first()
                .map(|p| p.name.clone())
        })
        .unwrap_or_else(|| "fynd".to_string());

    // Collect unique non-baseline participant names, sorted.
    let agg_names: Vec<String> = results
        .iter()
        .flat_map(|r| r.participants.iter())
        .filter(|p| p.name != baseline_name)
        .map(|p| p.name.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    for name in &agg_names {
        info!("\n  vs {name}:");

        let agg_rows: Vec<&ParticipantResult> = results
            .iter()
            .flat_map(|r| r.participants.iter())
            .filter(|p| &p.name == name)
            .collect();

        let raw_diffs: Vec<f64> = agg_rows
            .iter()
            .filter_map(|a| a.raw_diff_bps)
            .collect();
        let gas_reported_diffs: Vec<f64> = agg_rows
            .iter()
            .filter_map(|a| a.gas_adjusted_diff_bps_reported)
            .collect();
        let gas_onchain_diffs: Vec<f64> = agg_rows
            .iter()
            .filter_map(|a| a.gas_adjusted_diff_bps_onchain)
            .collect();

        if raw_diffs.is_empty() {
            info!("    No comparable trades.");
            continue;
        }

        let summarise = |diffs: &[f64]| -> (usize, usize, f64, f64) {
            let n = diffs.len();
            let wins = diffs
                .iter()
                .filter(|&&d| d > 0.0)
                .count();
            let avg = diffs.iter().sum::<f64>() / n as f64;
            let mut s = diffs.to_vec();
            s.sort_by(|a, b| {
                a.partial_cmp(b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let med = s[n / 2];
            (n, wins, avg, med)
        };

        let (rn, rw, ravg, rmed) = summarise(&raw_diffs);
        info!(
            "    Raw (no gas):       Fynd better {rw}/{rn} ({:.1}%)  avg {ravg:+.2} bps  median {rmed:+.2} bps",
            rw as f64 / rn as f64 * 100.0
        );

        if gas_reported_diffs.is_empty() {
            info!("    Gas-adj (reported): (no reported gas data)");
        } else {
            let (gn, gw, gavg, gmed) = summarise(&gas_reported_diffs);
            info!(
                "    Gas-adj (reported): Fynd better {gw}/{gn} ({:.1}%)  avg {gavg:+.2} bps  median {gmed:+.2} bps",
                gw as f64 / gn as f64 * 100.0
            );
        }

        if gas_onchain_diffs.is_empty() {
            info!("    Gas-adj (onchain):  (no on-chain gas data)");
        } else {
            let (gn, gw, gavg, gmed) = summarise(&gas_onchain_diffs);
            info!(
                "    Gas-adj (onchain):  Fynd better {gw}/{gn} ({:.1}%)  avg {gavg:+.2} bps  median {gmed:+.2} bps",
                gw as f64 / gn as f64 * 100.0
            );
        }

        // Per-pair breakdown.
        info!("\n  Per-pair breakdown:");
        let mut pairs: Vec<&str> = results
            .iter()
            .map(|r| r.pair.as_str())
            .collect();
        pairs.sort_unstable();
        pairs.dedup();

        info!(
            "  {:<30}  {:>5}  {:>5}  {:>10}  {:>12}  {:>12}",
            "Pair", "n", "Win%", "Raw med", "Gas-rep med", "Gas-chain med"
        );
        info!("  {}", "-".repeat(80));

        for pair in &pairs {
            let rows: Vec<&ParticipantResult> = results
                .iter()
                .filter(|r| r.pair.as_str() == *pair)
                .flat_map(|r| r.participants.iter())
                .filter(|p| &p.name == name)
                .collect();

            let pr: Vec<f64> = rows
                .iter()
                .filter_map(|a| a.raw_diff_bps)
                .collect();
            if pr.is_empty() {
                continue;
            }

            let med_of = |vals: &mut Vec<f64>| -> Option<f64> {
                if vals.is_empty() {
                    return None;
                }
                vals.sort_by(|a, b| {
                    a.partial_cmp(b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                Some(vals[vals.len() / 2])
            };

            let pn = pr.len();
            let pw = pr.iter().filter(|&&d| d > 0.0).count();
            let mut sr = pr.clone();
            let rmed = med_of(&mut sr).unwrap_or(0.0);

            let mut pg_rep: Vec<f64> = rows
                .iter()
                .filter_map(|a| a.gas_adjusted_diff_bps_reported)
                .collect();
            let mut pg_chain: Vec<f64> = rows
                .iter()
                .filter_map(|a| a.gas_adjusted_diff_bps_onchain)
                .collect();

            let fmt_med = |v: Option<f64>| match v {
                Some(x) => format!("{:>+12.2}", x),
                None => "         n/a".to_string(),
            };

            info!(
                "  {:<30}  {:>5}  {:>4.1}%  {:>+10.2}  {}  {}",
                pair,
                pn,
                pw as f64 / pn as f64 * 100.0,
                rmed,
                fmt_med(med_of(&mut pg_rep)),
                fmt_med(med_of(&mut pg_chain)),
            );
        }
    }

    info!("\n{}", "=".repeat(80));
}

// ─── quote helpers ────────────────────────────────────────────────────────────

/// Some trade datasets use 0xeeee…eeee as a sentinel for native ETH; Fynd expects ZERO_ADDRESS.
const ETH_SENTINEL: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

fn fynd_addr(addr: &str) -> &str {
    if addr.eq_ignore_ascii_case(ETH_SENTINEL) {
        ZERO_ADDRESS
    } else {
        addr
    }
}

fn make_quote_params(
    token_in: &str,
    token_out: &str,
    amount: &str,
    timeout_ms: u64,
    sender_hex: Option<&str>,
    encoding: Option<EncodingOptions>,
) -> anyhow::Result<QuoteParams> {
    let sender = parse_addr(sender_hex.unwrap_or("0x0000000000000000000000000000000000000001"))?;
    let order = Order::new(
        parse_addr(fynd_addr(token_in))?,
        parse_addr(fynd_addr(token_out))?,
        BigUint::from_str(amount).unwrap_or_default(),
        OrderSide::Sell,
        sender,
        None,
    );
    let opts = QuoteOptions::default().with_timeout_ms(timeout_ms);
    let opts = match encoding {
        Some(enc) => opts.with_encoding_options(enc),
        None => opts,
    };
    Ok(QuoteParams::new(order, opts))
}

fn parse_addr(hex_str: &str) -> anyhow::Result<Bytes> {
    let stripped = hex_str
        .strip_prefix("0x")
        .unwrap_or(hex_str);
    hex::decode(stripped)
        .map(Bytes::from)
        .map_err(|_| anyhow::anyhow!("bad address '{hex_str}'"))
}

/// Extract `uint256 amountOut` from raw `eth_call` return bytes.
///
/// Most routers return a bare `uint256` as the first word. The 0x AllowanceHolder wraps
/// the inner swap result in ABI-encoded `bytes memory`, so the layout is:
///   word 0: offset = 0x20 (32)
///   word 1: inner length
///   word 2: inner uint256 amountOut
///
/// When the first word equals 32 and the data is long enough, we skip the ABI wrapper.
fn parse_return_amount(data: &[u8]) -> Option<BigUint> {
    if data.len() < 32 {
        return None;
    }
    // Detect ABI `bytes memory` wrapper: first word is exactly 0x0000…0020.
    if data.len() >= 96 && data[..31] == [0u8; 31] && data[31] == 0x20 {
        let inner = BigUint::from_bytes_be(&data[64..96]);
        if inner > BigUint::ZERO {
            return Some(inner);
        }
    }
    Some(BigUint::from_bytes_be(&data[..32]))
}

/// Diff between `eth_call` on-chain result and Fynd's quoted amount, in basis points.
///
/// Positive = on-chain result exceeds quoted amount (Fynd was conservative).
/// Negative = on-chain result is less than quoted (Fynd was optimistic).
/// `None` when either amount is zero.
fn eth_call_bps_diff(actual: &BigUint, quoted: &BigUint) -> Option<f64> {
    if *actual == BigUint::ZERO || *quoted == BigUint::ZERO {
        return None;
    }
    let a: f64 = actual
        .to_string()
        .parse()
        .unwrap_or(0.0);
    let q: f64 = quoted
        .to_string()
        .parse()
        .unwrap_or(0.0);
    if q == 0.0 {
        return None;
    }
    Some((a - q) / q * 10_000.0)
}
