//! Reproduces `DeltaTransitionError ... curve getter call failed: StorageError` on the TriCRV
//! pool (`vm:curve`, 0x4ebdf703948ddcea3b11f675b4d1fba9d2414a14).
//!
//! TriCRV is its own LP token, and Tycho lists that LP token (quality 100). If the token is not in
//! the decoder's token map when the pool's snapshot is decoded, and later arrives in a block's
//! `new_tokens`, the next delta that touches the pool's account takes the decoder's token branch:
//! the pool account is replaced by an ERC20 proxy whose implementation account
//! (`0x…badbabe`) is never created, because the delta carries storage but no code. Every Curve
//! getter then fails with `MissingAccount`, which the decoder logs as `StorageError`, and the
//! component is removed until a fresh snapshot.
//!
//! The example replays `examples/data/tricrv_feed.jsonl`, a `tycho-client` capture of the TriCRV
//! component (snapshot at block 26132642, then 49 blocks; the pool's account changes in blocks
//! 26132643, 26132646, 26132650 and 26132663). By default the LP token is left out of the startup
//! token map and delivered through `new_tokens` with block 26132643, which makes every pool delta
//! fail. `--control` puts the LP token in the startup map instead, and every delta decodes.
//!
//! No Tycho connection or API key is needed. The Curve decoder reads the pool's MATH contract
//! code over RPC, so `RPC_URL` must point at an Ethereum mainnet node.
//!
//! ```bash
//! export RPC_URL="https://ethereum-rpc.publicnode.com"
//! RUST_LOG=warn cargo run --package fynd-core --example tricrv_storage_error            # fails
//! RUST_LOG=warn cargo run --package fynd-core --example tricrv_storage_error -- --control  # ok
//! ```

use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address;
use tracing_subscriber::EnvFilter;
use tycho_simulation::{
    evm::{
        decoder::TychoStreamDecoder,
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::{curve::CurveState, filters::curve_filter},
        simulation::PendingOverrides,
    },
    protocol::models::Update,
    tycho_client::feed::{dto, BlockHeader, FeedMessage},
    tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    },
};

const TRICRV: &str = "0x4ebdf703948ddcea3b11f675b4d1fba9d2414a14";
const LP_TOKEN_ARRIVAL_BLOCK: u64 = 26_132_643;
const FEED: &str = include_str!("data/tricrv_feed.jsonl");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .init();

    let control = std::env::args().any(|arg| arg == "--control");
    let tricrv = Bytes::from_str(TRICRV).expect("pool address");

    let mut decoder = TychoStreamDecoder::<BlockHeader>::new(Chain::Ethereum);
    decoder.register_decoder::<CurveState>("vm:curve");
    decoder.register_filter("vm:curve", curve_filter);
    decoder.skip_state_decode_failures(true);

    let mut tokens: HashMap<Bytes, Token> = [
        ("0x0000000000000000000000000000000000000000", "ETH"),
        ("0xd533a949740bb3306d119cc777fa900ba034cd52", "CRV"),
        ("0xf939e0a03fb07f59a73314e73794be0e57ac1b4e", "crvUSD"),
    ]
    .into_iter()
    .map(|(address, symbol)| token(address, symbol))
    .collect();
    if control {
        tokens.extend([token(TRICRV, "crvUSDETHCRV")]);
    }
    decoder.set_tokens(tokens).await;
    println!(
        "TriCRV LP token {}",
        if control { "in the startup token map (control)" } else { "delivered via new_tokens" }
    );

    for line in FEED
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let mut message: dto::FeedMessage<BlockHeader> =
            serde_json::from_str(line).expect("feed line is a FeedMessage");
        if !control {
            deliver_lp_token(&mut message, &tricrv);
        }
        match decoder
            .decode(&FeedMessage::from(message))
            .await
        {
            Ok(update) => report(&update),
            Err(error) => println!("decode error: {error:?}"),
        }
    }
}

fn token(address: &str, symbol: &str) -> (Bytes, Token) {
    let address = Bytes::from_str(address).expect("token address");
    let token = Token::new(&address, symbol, 18, 0, &[Some(30_000)], Chain::Ethereum, 100);
    (address, token)
}

/// Adds the TriCRV LP token to `new_tokens` of the block that first changes the pool's account,
/// as Tycho does when the token first appears after a client has started.
fn deliver_lp_token(message: &mut dto::FeedMessage<BlockHeader>, tricrv: &Bytes) {
    let Some(curve) = message.state_msgs.get_mut("vm:curve") else { return };
    if curve.header.number != LP_TOKEN_ARRIVAL_BLOCK {
        return;
    }
    if let Some(deltas) = curve.deltas.as_mut() {
        let (_, token) = token(TRICRV, "crvUSDETHCRV");
        deltas
            .new_tokens
            .insert(tricrv.clone(), token.into());
    }
}

fn report(update: &Update) {
    println!(
        "block {} TriCRV: state_updated={} removed={} getters: {}",
        update.block_number_or_timestamp,
        update.states.contains_key(TRICRV),
        update
            .removed_pairs
            .contains_key(TRICRV),
        probe_getters()
    );
}

/// Calls three of the getters `curve/vm.rs` uses against the shared VM database and prints the
/// full engine error, which the decoder log shortens to `StorageError`.
fn probe_getters() -> String {
    let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("engine");
    let pool = Address::from_str(TRICRV).expect("pool address");
    let getters: [(&str, Vec<u8>); 3] = [
        ("balances(0)", [&[0x49, 0x03, 0xb0, 0xd1][..], &[0u8; 32]].concat()),
        ("D()", vec![0x0f, 0x52, 0x9b, 0xa2]),
        ("gamma()", vec![0xb1, 0x37, 0x39, 0x29]),
    ];
    getters
        .into_iter()
        .map(|(name, calldata)| {
            match engine.simulate(&PendingOverrides::default().view_call(pool, calldata)) {
                Ok(_) => format!("{name}=ok"),
                Err(error) => format!("{name}=ERR({error:?})"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
