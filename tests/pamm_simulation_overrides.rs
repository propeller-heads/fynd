//! Live checks that a pAMM quote's simulation overrides reproduce the quote.
//!
//! A pAMM quote prices the block Titan is building, on venue state that is not on chain yet, so
//! Fynd returns, on each pAMM swap, the account state Titan priced the venue on. These tests check
//! that against mainnet:
//!
//! - `live_simulate_v1_needs_a_block_above_its_base` checks the node rule the simulator relies on:
//!   `eth_simulateV1` accepts a simulated block only when its number and time sit above its base
//!   block's, so a quoted block can only be simulated on its parent.
//! - `live_pamm_quotes_simulated_with_their_overrides` builds a solver with the price level stream
//!   and Uniswap V3 (the pool each pAMM swap falls back to), quotes single-swap pAMM routes with
//!   simulation on, and checks each against Fynd's own `simulation_result` and against a simulation
//!   built the way a client builds one: from the wire response alone, on the quoted block's parent
//!   with its number and time, the response's `accounts` passed through unchanged, and the sender
//!   funded.
//!
//! Ignored by default: they dial Titan, Tycho and the node in `RPC_URL`, which must serve
//! `eth_simulateV1` and `debug_traceCall`; the second also needs `TYCHO_API_KEY`. They only
//! simulate and never send a transaction. Run them with:
//!
//! ```text
//! cargo test -p fynd --test pamm_simulation_overrides -- --ignored --nocapture
//! ```

use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{map::AddressHashMap, Address, Bytes, TxKind, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::{AccountOverride, StateOverride},
        BlockOverrides, TransactionRequest,
    },
};
use fynd::{
    core::{
        simulation::token_layout::discover_layout, EncodingOptions, FyndBuilder, Order, OrderQuote,
        OrderSide, PoolConfig, QuoteOptions, QuoteRequest, QuoteStatus, SimulationResult,
    },
    rpc::api::dto,
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_simulation::{
    price_level_stream::config::default_served_pamms, tycho_common::models::Chain,
};

/// How far a simulated output may sit from the quoted output after fees, in basis points.
const TOLERANCE_BPS: f64 = 1.0;

/// How long to keep quoting for pAMM routes.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

/// The most quotes to check, one per venue and pair.
const MAX_CANDIDATES: usize = 12;

/// The sender of every quote: an account with no state, funded only in simulation.
const SENDER: Address = Address::repeat_byte(0x5e);

/// Gas the client-style simulation allows the call.
const CALL_GAS: u64 = 3_000_000;

const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
const WBTC: &str = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";

fn rpc_url() -> String {
    std::env::var("RPC_URL").expect("RPC_URL must name an Ethereum mainnet node")
}

fn provider() -> RootProvider {
    ProviderBuilder::default().connect_http(
        rpc_url()
            .parse()
            .expect("RPC_URL must be a URL"),
    )
}

fn address(hex: &str) -> Address {
    hex.parse()
        .expect("hardcoded address must parse")
}

fn tycho_bytes(address: Address) -> tycho_simulation::tycho_common::Bytes {
    tycho_simulation::tycho_common::Bytes::from(address.as_slice())
}

/// A call that does nothing: a value-less call to an account with no code.
fn empty_call() -> TransactionRequest {
    TransactionRequest {
        from: Some(Address::repeat_byte(0x11)),
        to: Some(TxKind::Call(Address::repeat_byte(0x22))),
        value: Some(U256::ZERO),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "dials the node in RPC_URL"]
async fn live_simulate_v1_needs_a_block_above_its_base() {
    let provider = provider();
    let base = provider
        .get_block_number()
        .await
        .expect("the node reports its head") -
        1;
    let base_timestamp = provider
        .get_block_by_number(BlockNumberOrTag::Number(base))
        .await
        .expect("the node answers")
        .expect("the node has the base block")
        .header
        .timestamp;

    // The accepted case runs first, so a node without `eth_simulateV1` fails here rather than
    // passing the rejected cases for the wrong reason.
    for (name, number, time, accepted) in [
        ("the next number and slot", base + 1, base_timestamp + 12, true),
        ("the base block's own number", base, base_timestamp + 12, false),
        ("a number below the base block's", base - 1, base_timestamp + 12, false),
        ("the next number, at the base block's time", base + 1, base_timestamp, false),
    ] {
        let payload = SimulatePayload::default().extend(
            SimBlock::default()
                .with_block_overrides(BlockOverrides {
                    number: Some(U256::from(number)),
                    time: Some(time),
                    ..Default::default()
                })
                .call(empty_call()),
        );
        let result = provider
            .simulate(&payload)
            .block_id(BlockNumberOrTag::Number(base).into())
            .await;
        println!("{name}: {}", if result.is_ok() { "accepted" } else { "rejected" });
        if let Err(error) = &result {
            println!("  {error}");
        }
        assert_eq!(result.is_ok(), accepted, "{name}");
    }
}

/// One quote whose route is a single pAMM swap carrying overrides.
struct Candidate {
    protocol: String,
    pair: String,
    quote: OrderQuote,
    /// The same quote as the wire carries it, parsed back from its JSON.
    wire: dto::OrderQuote,
}

/// The orders to quote: each sells a few thousand dollars, inside the depth Titan quotes.
fn orders() -> Vec<(&'static str, &'static str, &'static str, BigUint)> {
    let units =
        |amount: u64, decimals: u32| BigUint::from(amount) * BigUint::from(10u64).pow(decimals);
    vec![
        ("WETH", WETH, USDC, units(2, 18)),
        ("USDC", USDC, WETH, units(5_000, 6)),
        ("WETH", WETH, USDT, units(2, 18)),
        ("USDT", USDT, WETH, units(5_000, 6)),
        ("WBTC", WBTC, USDC, units(5, 6)),
        ("USDC", USDC, WBTC, units(5_000, 6)),
        ("USDC", USDC, USDT, units(10_000, 6)),
        ("USDT", USDT, USDC, units(10_000, 6)),
    ]
}

/// The router's output a quote promises: its raw output less the router and client fees, which the
/// router takes before it returns.
fn promised(quote: &OrderQuote) -> BigUint {
    let fees = quote
        .fee_breakdown()
        .expect("an encoded quote carries its fees");
    fees.min_amount_received() + fees.max_slippage()
}

/// How far `simulated` sits from what `quote` promises, in basis points.
fn deviation_bps(quote: &OrderQuote, simulated: &BigUint) -> f64 {
    let promised = promised(quote)
        .to_f64()
        .expect("finite");
    (simulated.to_f64().expect("finite") - promised) / promised * 10_000.0
}

/// Simulates `quote`'s transaction the way a client does: from the wire response alone, on the
/// quoted block's parent with its number and time, the swaps' `accounts` merged and passed through
/// unchanged, and the sender funded with the input token.
///
/// Returns the router's output, or the reason the call did not return one.
async fn simulate_as_a_client(
    provider: &RootProvider,
    candidate: &Candidate,
) -> Result<BigUint, String> {
    let wire = &candidate.wire;
    let transaction = wire
        .transaction()
        .ok_or("no transaction")?;
    let route = wire.route().ok_or("no route")?;
    let mut accounts: StateOverride = AddressHashMap::default();
    let mut block_number = None;
    let mut block_timestamp = None;
    for swap in route.swaps() {
        let Some(overrides) = swap.simulation_overrides() else { continue };
        // The wire encoding is the State Override Set encoding, so alloy reads it as is.
        let json = serde_json::to_value(overrides).map_err(|error| error.to_string())?;
        let swap_accounts: AddressHashMap<AccountOverride> =
            serde_json::from_value(json["accounts"].clone()).map_err(|error| error.to_string())?;
        accounts.extend(swap_accounts);
        if overrides.block_number() > block_number {
            block_number = overrides.block_number();
            block_timestamp = overrides.block_timestamp();
        }
    }
    let block_number = block_number.ok_or("the overrides name no block")?;

    let token_in = Address::from_slice(route.swaps()[0].token_in().as_ref());
    let router = Address::from_slice(transaction.to().as_ref());
    let layout = discover_layout(provider, token_in, SENDER, router)
        .await
        .map_err(|error| format!("token layout: {error:?}"))?;
    let funding = U256::from(10u64).pow(U256::from(30u64));
    let token_account = accounts
        .entry(layout.storage_contract())
        .or_default();
    let slots = token_account
        .state_diff
        .get_or_insert_with(Default::default);
    slots.insert(layout.balance_slot(SENDER), layout.balance_word(funding).into());
    slots.insert(layout.allowance_slot(SENDER, router), funding.into());
    accounts
        .entry(SENDER)
        .or_default()
        .balance = Some(funding);

    let call = TransactionRequest {
        from: Some(SENDER),
        to: Some(TxKind::Call(router)),
        value: Some(U256::from_be_slice(&transaction.value().to_bytes_be())),
        input: Bytes::copy_from_slice(transaction.data()).into(),
        gas: Some(CALL_GAS),
        ..Default::default()
    };
    let payload = SimulatePayload::default().extend(
        SimBlock::default()
            .with_state_overrides(accounts)
            .with_block_overrides(BlockOverrides {
                number: Some(U256::from(block_number)),
                time: block_timestamp,
                ..Default::default()
            })
            .call(call),
    );
    let blocks = provider
        .simulate(&payload)
        .block_id(BlockNumberOrTag::Number(block_number - 1).into())
        .await
        .map_err(|error| error.to_string())?;
    let result = blocks
        .first()
        .and_then(|block| block.calls.first())
        .ok_or("eth_simulateV1 returned no call")?;
    if !result.status {
        return Err(format!(
            "reverted: {}",
            result
                .error
                .as_ref()
                .map_or("no reason", |error| error.message.as_str())
        ));
    }
    Ok(BigUint::from_bytes_be(&result.return_data))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "dials Titan, Tycho and the node in RPC_URL; needs RPC_URL and TYCHO_API_KEY"]
async fn live_pamm_quotes_simulated_with_their_overrides() {
    // `RUST_LOG` shows what the feed does while the solver gets ready.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let tycho_api_key =
        std::env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY must authorize the Tycho stream");
    let tycho_url = fynd::rpc::config::defaults::default_tycho_url("ethereum")
        .expect("Ethereum has a hosted Tycho endpoint");
    let mut protocols = vec!["uniswap_v3".to_string()];
    for venue in default_served_pamms() {
        protocols.push(format!("pricelevelstream:{}", venue.protocol));
    }
    let pool = PoolConfig::new("most_liquid")
        .with_max_hops(1)
        .with_num_workers(2)
        .with_timeout_ms(500);
    // The hosted Tycho endpoint accepts only its own TVL filter.
    let solver = FyndBuilder::new(Chain::Ethereum, tycho_url, rpc_url(), protocols, 10.0)
        .tycho_api_key(tycho_api_key)
        .add_pool("single_hop", &pool)
        .expect("a valid pool")
        .simulation_enabled(true)
        .worker_router_timeout(Duration::from_secs(5))
        .build()
        .expect("the solver builds");
    solver
        .wait_until_ready(Duration::from_secs(300))
        .await
        .expect("the solver gets ready");

    let deadline = tokio::time::Instant::now() + CAPTURE_TIMEOUT;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut pamm_routes_without_overrides = 0usize;
    while tokio::time::Instant::now() < deadline && candidates.len() < MAX_CANDIDATES {
        for (symbol, sell, buy, amount) in orders() {
            let order = Order::new(
                tycho_bytes(address(sell)),
                tycho_bytes(address(buy)),
                amount,
                OrderSide::Sell,
                tycho_bytes(SENDER),
            );
            let options = QuoteOptions::default()
                .with_encoding_options(EncodingOptions::new(0.005).with_simulation());
            let Ok(quote) = solver
                .quote(QuoteRequest::new(vec![order], options))
                .await
            else {
                continue;
            };
            let Some(quote) = quote.into_orders().into_iter().next() else { continue };
            if quote.status() != QuoteStatus::Success {
                continue;
            }
            let Some(swap) = quote
                .route()
                .and_then(|route| route.swaps().first())
            else {
                continue;
            };
            if !swap.protocol().starts_with("fallback:") {
                continue;
            }
            if swap.simulation_overrides().is_none() {
                pamm_routes_without_overrides += 1;
                continue;
            }
            let pair = format!("{symbol} -> {buy}");
            if !seen.insert((swap.protocol().to_string(), pair.clone())) {
                continue;
            }
            let protocol = swap.protocol().to_string();
            let json = serde_json::to_string(&dto::OrderQuote::from(quote.clone()))
                .expect("the wire quote serializes");
            let wire = serde_json::from_str(&json).expect("the wire quote parses back");
            candidates.push(Candidate { protocol, pair, quote, wire });
        }
    }
    println!(
        "captured {} pAMM quote(s) with overrides; {pamm_routes_without_overrides} pAMM route(s) \
         had none",
        candidates.len()
    );
    if candidates.is_empty() {
        println!(
            "::warning::no single-swap pAMM route won within the capture window; nothing checked"
        );
        return;
    }

    let provider = provider();
    let mut within = 0usize;
    let mut results: Vec<String> = Vec::new();
    for candidate in &candidates {
        let quote = &candidate.quote;
        let overrides = quote.route().expect("a route").swaps()[0]
            .simulation_overrides()
            .expect("the candidate carries overrides");
        let quoted_block = overrides
            .block_number()
            .expect("Titan named the block");
        println!(
            "\n{} {}: quote block {}, quoted block {quoted_block}, promised {}",
            candidate.protocol,
            candidate.pair,
            quote.block().number(),
            promised(quote)
        );

        let fynd = match quote.simulation_result() {
            Some(SimulationResult::Success { amount_out, .. }) => {
                let bps = deviation_bps(quote, amount_out);
                println!("  Fynd's simulation: {amount_out} ({bps:+.2} bps)");
                Some(bps)
            }
            other => {
                println!("  Fynd's simulation: {other:?}");
                None
            }
        };
        let client = match simulate_as_a_client(&provider, candidate).await {
            Ok(amount_out) => {
                let bps = deviation_bps(quote, &amount_out);
                println!("  client simulation: {amount_out} ({bps:+.2} bps)");
                Some(bps)
            }
            Err(reason) => {
                println!("  client simulation: {reason}");
                None
            }
        };
        let passed = [fynd, client]
            .iter()
            .all(|bps| bps.is_some_and(|bps| bps.abs() <= TOLERANCE_BPS));
        within += usize::from(passed);
        results.push(format!(
            "{} {}: Fynd {:?} bps, client {:?} bps",
            candidate.protocol, candidate.pair, fynd, client
        ));
    }

    println!("\n{within} of {} quote(s) within {TOLERANCE_BPS} bps both ways:", candidates.len());
    for line in &results {
        println!("  {line}");
    }
    let by_protocol: HashMap<&str, usize> =
        candidates
            .iter()
            .fold(HashMap::new(), |mut counts, candidate| {
                *counts
                    .entry(candidate.protocol.as_str())
                    .or_default() += 1;
                counts
            });
    println!("venues: {by_protocol:?}");
    assert_eq!(within, candidates.len(), "every checked quote must reproduce within tolerance");
}
