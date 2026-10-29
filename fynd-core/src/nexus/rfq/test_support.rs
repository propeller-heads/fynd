//! Assemblers and prices for the tests of the RFQ modules: WETH and USDC on Ethereum.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use num_bigint::BigUint;
use serde_json::json;
use tycho_simulation::{
    book::{Book, BookFeedConfig, Levels, PriceLevel},
    evm::tycho_models::Chain,
    pamm::protocols::metric::{
        feed::MetricBookAssemblerBuilder, MetricBidAskResponse, MetricBookAssembler,
        MetricMetadata, MetricPoolPrice, MetricPrices,
    },
    rfq::protocols::{
        bebop::{BebopBook, BebopBookAssembler, BebopBookAssemblerBuilder, BebopPrices},
        hashflow::{
            HashflowBookAssembler, HashflowBookAssemblerBuilder, HashflowMarketMakerLevels,
            HashflowPair, HashflowPrices,
        },
    },
    tycho_common::models::token::Token,
    tycho_core::Bytes,
};

pub(crate) const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
pub(crate) const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

pub(crate) fn token(address: &str, symbol: &str, decimals: u32) -> Token {
    let address: Bytes = address.parse().unwrap();
    Token::new(&address, symbol, decimals, 0, &[Some(10_000)], Chain::Ethereum, 100)
}

fn book_config() -> BookFeedConfig {
    let tokens = [token(WETH, "WETH", 18), token(USDC, "USDC", 6)]
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect();
    BookFeedConfig { chain: Chain::Ethereum, tokens: Arc::new(tokens), min_tvl_usd: 0.0 }
}

fn usd_quote_tokens() -> HashSet<Bytes> {
    HashSet::from([USDC.parse::<Bytes>().unwrap()])
}

pub(crate) fn bebop_assembler() -> BebopBookAssembler {
    BebopBookAssemblerBuilder::new(book_config(), usd_quote_tokens(), "test-key".to_string())
        .build()
        .unwrap()
}

pub(crate) fn hashflow_assembler() -> HashflowBookAssembler {
    HashflowBookAssemblerBuilder::new(
        book_config(),
        usd_quote_tokens(),
        "test-user".to_string(),
        "test-key".to_string(),
    )
    .build()
    .unwrap()
}

pub(crate) fn metric_assembler() -> MetricBookAssembler {
    MetricBookAssemblerBuilder::new(book_config()).build()
}

fn levels(price: f64) -> Levels {
    Levels::new(vec![PriceLevel { price, quantity: 5.0 }]).unwrap()
}

pub(crate) fn bebop_prices(bid: f64) -> BebopPrices {
    BebopPrices {
        pairs: vec![BebopBook {
            base: WETH.parse().unwrap(),
            quote: USDC.parse().unwrap(),
            last_update_ts: 1_761_550_000_123,
            bids: levels(bid),
            asks: levels(bid + 1.0),
        }],
    }
}

pub(crate) fn hashflow_prices() -> HashflowPrices {
    let pair =
        HashflowPair { base_token: WETH.parse().unwrap(), quote_token: USDC.parse().unwrap() };
    HashflowPrices {
        levels_by_maker: HashMap::from([(
            "mm1".to_string(),
            vec![HashflowMarketMakerLevels { pair, levels: levels(2999.1) }],
        )]),
    }
}

pub(crate) fn metric_prices() -> MetricPrices {
    MetricPrices {
        pools: vec![MetricPoolPrice {
            metadata: MetricMetadata {
                pool_address: Bytes::from([0x11; 20]),
                token0: WETH.parse().unwrap(),
                token1: USDC.parse().unwrap(),
                tvl_fiat: Some(1_234_567.89),
                pause_level: Some(0),
            },
            bid_ask: MetricBidAskResponse {
                bid_adj: BigUint::from(3000u32) * BigUint::from(10u32).pow(18),
                ask_adj: BigUint::from(3001u32) * BigUint::from(10u32).pow(18),
                total_token0_available: Some(BigUint::from(10u32).pow(20)),
                total_token1_available: Some(BigUint::from(10u32).pow(12)),
                server_ts: 1_761_550_000,
                price_provider_status: Some("healthy".to_string()),
                depth: serde_json::from_value(
                    json!({"asks": [depth_bin(3001)], "bids": [depth_bin(3000)]}),
                )
                .unwrap(),
            },
        }],
    }
}

fn depth_bin(price: u32) -> serde_json::Value {
    let price: BigUint = BigUint::from(price) << 64usize;
    json!({
        "binIdx": 0,
        "price": price.to_string(),
        "cumulativeVolume": 10u64.pow(12).to_string(),
        "cumulativeInputVolume": 10u128.pow(20).to_string(),
    })
}

/// The USDC amount returned by selling five WETH through `book`.
pub(crate) fn amount_out(book: &Book) -> BigUint {
    let weth = token(WETH, "WETH", 18);
    let usdc = token(USDC, "USDC", 6);
    book.state
        .get_amount_out(BigUint::from(5 * 10u64.pow(18)), &weth, &usdc)
        .unwrap()
        .amount
}
