//! Reproduction test for ENG-5573: Curve tricrypto hop returns ~20 BTC for ~2000 USDT
//!
//! The Curve tricrypto2 pool (0xd51a44d3fae010294c616388b506acda1bfaae46) has 3 tokens:
//!   - coins[0]: USDT (6 decimals)
//!   - coins[1]: WBTC (8 decimals)
//!   - coins[2]: WETH (18 decimals)
//!
//! Original bug: A swap of ~2000 USDT should yield ~0.02 BTC (at ~$100k/BTC), but the
//! simulation returns ~20 BTC — a 1000x error. The raw amounts are nearly 1:1,
//! suggesting the VM simulation ignores the actual token values.
//!
//! # Prerequisites
//!
//! ```bash
//! export TYCHO_API_KEY="your-api-key"
//! export RPC_URL="https://eth-mainnet.g.alchemy.com/v2/..."
//! cargo run --package fynd-core --example curve_tricrypto_repro
//! ```

use std::{env, str::FromStr, sync::Arc, time::Duration};
use tracing_subscriber::EnvFilter;

use num_bigint::BigUint;
use tokio::sync::RwLock;
use tycho_simulation::{
    evm::tycho_models::Chain,
    tycho_core::{models::token::Token, Bytes},
};

use fynd_core::feed::{market_data::SharedMarketData, tycho_feed::TychoFeed, TychoFeedConfig};

const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";
const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const TRICRYPTO2: &str = "0xd51a44d3fae010294c616388b506acda1bfaae46";

fn make_token(address: &str, symbol: &str, decimals: u32) -> Token {
    Token {
        address: Bytes::from_str(address).expect("valid address"),
        symbol: symbol.to_string(),
        decimals,
        tax: Default::default(),
        gas: vec![],
        chain: Chain::Ethereum,
        quality: 100,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let tycho_url =
        env::var("TYCHO_URL").unwrap_or_else(|_| "tycho-beta.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").ok();

    let market_data = Arc::new(RwLock::new(SharedMarketData::new()));

    let tycho_feed_config = TychoFeedConfig::new(
        tycho_url,
        Chain::Ethereum,
        tycho_api_key,
        true,
        vec!["vm:curve".to_string(), "uniswap_v2".to_string()],
        1.0,
    );

    let tycho_feed = TychoFeed::new(tycho_feed_config, Arc::clone(&market_data));

    let feed_handle = tokio::spawn(async move {
        if let Err(e) = tycho_feed.run().await {
            eprintln!("Tycho feed error: {:?}", e);
        }
    });

    println!("Waiting for Curve pool data from Tycho...");
    let mut found = false;
    for attempt in 1..=90 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let market = market_data.read().await;
        if market.get_simulation_state(TRICRYPTO2).is_some() {
            found = true;
            println!("\nPool loaded after {}s", attempt * 2);
            break;
        }
        print!(".");
    }

    if !found {
        eprintln!("\nFailed to load tricrypto2 pool. Check TYCHO_API_KEY and connectivity.");
        feed_handle.abort();
        return Ok(());
    }

    let market = market_data.read().await;
    let state = market
        .get_simulation_state(TRICRYPTO2)
        .expect("tricrypto2 state should be loaded");

    let usdt = make_token(USDT, "USDT", 6);
    let wbtc = make_token(WBTC, "WBTC", 8);
    let weth = make_token(WETH, "WETH", 18);

    println!("\n=== Curve tricrypto2 reproduction (ENG-5573) ===\n");

    // Test amounts: use the exact bug report amount + reasonable amounts for other pairs
    let test_cases: Vec<(&str, &Token, &Token, BigUint)> = vec![
        (
            "USDT -> WBTC (bug report)",
            &usdt,
            &wbtc,
            BigUint::from(2_001_366_955u64), // ~2001 USDT
        ),
        (
            "WBTC -> USDT",
            &wbtc,
            &usdt,
            BigUint::from(2_000_000u64), // 0.02 WBTC
        ),
        (
            "USDT -> WETH",
            &usdt,
            &weth,
            BigUint::from(2_000_000_000u64), // 2000 USDT
        ),
        (
            "WETH -> USDT",
            &weth,
            &usdt,
            BigUint::from(1_000_000_000_000_000_000u128), // 1 WETH
        ),
        (
            "WBTC -> WETH",
            &wbtc,
            &weth,
            BigUint::from(2_000_000u64), // 0.02 WBTC
        ),
        (
            "WETH -> WBTC",
            &weth,
            &wbtc,
            BigUint::from(1_000_000_000_000_000_000u128), // 1 WETH
        ),
    ];

    for (label, token_in, token_out, amount_in) in &test_cases {
        let amount_in_human = amount_in.to_string().parse::<f64>().unwrap_or(0.0)
            / 10_f64.powi(token_in.decimals as i32);

        match state.get_amount_out(amount_in.clone(), token_in, token_out) {
            Ok(result) => {
                let amount_out_human = result.amount.to_string().parse::<f64>().unwrap_or(0.0)
                    / 10_f64.powi(token_out.decimals as i32);

                println!(
                    "{label}:\n  {amount_in_human:.6} {} -> {amount_out_human:.6} {} (gas: {})",
                    token_in.symbol, token_out.symbol, result.gas
                );

                // Sanity checks based on approximate market prices
                let is_suspicious = match (token_in.symbol.as_str(), token_out.symbol.as_str()) {
                    ("USDT", "WBTC") => {
                        // ~2000 USDT should give ~0.02 BTC, not ~20 BTC
                        amount_out_human > 1.0
                    }
                    ("WBTC", "USDT") => {
                        // 0.02 BTC should give ~2000 USDT, not ~0.02 USDT
                        amount_out_human < 1.0
                    }
                    ("USDT", "WETH") => {
                        // 2000 USDT should give ~0.5 WETH, not hundreds
                        amount_out_human > 10.0
                    }
                    ("WETH", "USDT") => {
                        // 1 WETH should give ~3000-4000 USDT, not tiny amounts
                        amount_out_human < 100.0
                    }
                    _ => false,
                };

                if is_suspicious {
                    println!("  *** BUG: output looks ~1000x off ***");
                }
            }
            Err(e) => {
                println!("{label}:\n  {amount_in_human:.6} {} -> ERROR: {e:?}", token_in.symbol);
            }
        }
        println!();
    }

    // Spot prices
    println!("=== Spot prices ===\n");
    let pairs: [(&Token, &Token); 6] = [
        (&usdt, &wbtc),
        (&wbtc, &usdt),
        (&usdt, &weth),
        (&weth, &usdt),
        (&wbtc, &weth),
        (&weth, &wbtc),
    ];
    for (tin, tout) in &pairs {
        match state.spot_price(tin, tout) {
            Ok(price) => {
                println!("{} -> {}: {price:.8}", tin.symbol, tout.symbol);
            }
            Err(e) => {
                println!("{} -> {}: ERROR: {e:?}", tin.symbol, tout.symbol);
            }
        }
    }

    feed_handle.abort();
    Ok(())
}
