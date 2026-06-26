//! Captures a live market snapshot for offline algorithm benchmarking.
//!
//! Connects to Tycho, waits until the feed and derived computations are ready, then writes a JSON
//! snapshot of the market state (native protocols only) to disk. Replay it offline with the
//! `fynd-benchmark quality` subcommand or the `fynd_core::offline` harness.
//!
//! Only native-math protocols are captured because `ProtocolSim` serialization (typetag) is
//! reliable for them; VM-backed pools (Balancer/Curve) are intentionally excluded.
//!
//! # Prerequisites
//!
//! ```bash
//! export TYCHO_API_KEY="your-api-key"   # https://tycho.propellerheads.xyz
//! export RPC_URL="https://eth.llamarpc.com"
//! export TYCHO_URL="tycho-fynd-ethereum.propellerheads.xyz"  # optional
//! export SNAPSHOT_OUT="market_snapshot.json"                 # optional
//! export PROTOCOLS="uniswap_v2,uniswap_v3,uniswap_v4"        # optional, native only
//! export MIN_TVL="100"                                       # optional, in native token
//! cargo run --release --package fynd-core --example capture_snapshot
//! ```

use std::{collections::HashSet, env, time::Duration};

use fynd_core::FyndBuilder;
use tracing_subscriber::EnvFilter;
use tycho_simulation::evm::tycho_models::Chain;

/// Native, serialization-safe protocols captured by default.
const DEFAULT_PROTOCOLS: &str = "uniswap_v2,uniswap_v3,uniswap_v4,sushiswap_v2,pancakeswap_v2";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .compact()
        .init();

    let tycho_url = env::var("TYCHO_URL")
        .unwrap_or_else(|_| "tycho-fynd-ethereum.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY env var not set");
    let rpc_url = env::var("RPC_URL").expect("RPC_URL env var not set");
    let out_path = env::var("SNAPSHOT_OUT").unwrap_or_else(|_| "market_snapshot.json".to_string());
    let protocols_csv = env::var("PROTOCOLS").unwrap_or_else(|_| DEFAULT_PROTOCOLS.to_string());
    let min_tvl: f64 = env::var("MIN_TVL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100.0);

    let protocols: Vec<String> = protocols_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let allowed: HashSet<String> = protocols.iter().cloned().collect();

    println!("Connecting to Tycho ({tycho_url}) for protocols: {protocols:?}");
    let solver = FyndBuilder::new(Chain::Ethereum, tycho_url, rpc_url, protocols, min_tvl)
        .tycho_api_key(tycho_api_key)
        .algorithm("most_liquid")
        .build()?;

    println!("Waiting for market data and derived computations (up to 180s)...");
    solver
        .wait_until_ready(Duration::from_secs(180))
        .await?;

    let market = solver.market_data();
    let view = market.read().await;

    // Keep only components whose protocol is in the native allowlist.
    let native_ids: HashSet<String> = view
        .component_topology()
        .keys()
        .filter(|id| {
            view.get_component(id)
                .map(|c| allowed.contains(&c.protocol_system))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let subset = view.extract_subset(&native_ids);
    let block = subset
        .last_updated()
        .map(|b| b.number())
        .unwrap_or_default();
    let snapshot = subset.to_snapshot();
    let pool_count = snapshot.states.len();
    let token_count = snapshot.tokens.len();

    drop(view);

    let json = serde_json::to_vec(&snapshot)?;
    std::fs::write(&out_path, &json)?;

    println!(
        "Wrote snapshot: {out_path}\n  block: {block}\n  pools: {pool_count}\n  tokens: {token_count}\n  size: {:.1} MB",
        json.len() as f64 / 1_048_576.0
    );

    solver.shutdown();
    Ok(())
}
