//! Atomic Searcher Showcase
//!
//! Demonstrates Janos Tapolcai's cyclic arbitrage algorithm running as an
//! autonomous searcher on top of Fynd's market data infrastructure.
//!
//! Unlike Fynd's solver (which finds A-to-B routes for incoming orders), the
//! atomic searcher proactively scans for profitable cycles:
//!   Token_A -> Token_1 -> ... -> Token_A
//! and optimizes the input amount via golden section search.
//!
//! This is a showcase/educational tool, not production MEV software.
//!
//! # Algorithm
//!
//! 1. Subscribe to Tycho market updates via Fynd's TychoFeed
//! 2. On each block, run a modified Bellman-Ford that finds cycles back to WETH
//! 3. For each candidate cycle, use golden section search to find optimal input
//! 4. Log all profitable cycles
//!
//! Based on: <https://github.com/jtapolcai/tycho-searcher>
//! Paper: <https://www.overleaf.com/read/ksqhzzmndmqh>

mod amount_optimizer;
mod cycle_detector;
mod types;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use clap::Parser;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use petgraph::graph::NodeIndex;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use tycho_simulation::tycho_core::models::token::Token;

use fynd::{
    feed::{
        events::MarketEventHandler,
        market_data::{SharedMarketData, SharedMarketDataRef},
        tycho_feed::TychoFeed,
        TychoFeedConfig,
    },
    graph::{petgraph::StableDiGraph, GraphManager, PetgraphStableDiGraphManager},
    native_token, parse_chain,
};

use crate::types::BlockSearchResult;

// ==================== CLI ====================

#[derive(Parser, Debug)]
#[command(name = "atomic_searcher", about = "Cyclic arbitrage searcher showcase")]
struct Args {
    /// Blockchain to search on.
    #[arg(long, default_value = "ethereum")]
    chain: String,

    /// Tycho WebSocket URL.
    #[arg(long, default_value = "tycho-beta.propellerheads.xyz")]
    tycho_url: String,

    /// Protocols to index (comma-separated).
    #[arg(long, default_value = "uniswap_v2,uniswap_v3,sushiswap_v2")]
    protocols: String,

    /// Minimum TVL in native token.
    #[arg(long, default_value_t = 10.0)]
    min_tvl: f64,

    /// Maximum hops in a cycle (not counting the closing edge).
    #[arg(long, default_value_t = 4)]
    max_hops: usize,

    /// Seed amount in ETH for the initial BF scan.
    #[arg(long, default_value_t = 1.0)]
    seed_eth: f64,

    /// GSS convergence tolerance (relative).
    #[arg(long, default_value_t = 0.001)]
    gss_tolerance: f64,

    /// GSS maximum iterations.
    #[arg(long, default_value_t = 30)]
    gss_max_iter: usize,
}

// ==================== Main ====================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("atomic_searcher=info,fynd=info")),
        )
        .init();

    let args = Args::parse();
    let chain = parse_chain(&args.chain)?;
    let source_token_addr = native_token(&chain)?;
    let protocols: Vec<String> = args
        .protocols
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    // Seed amount in wei (1 ETH = 1e18 wei)
    let seed_amount = BigUint::from((args.seed_eth * 1e18) as u128);

    info!(
        chain = ?chain,
        source_token = %hex::encode(&source_token_addr),
        seed_eth = args.seed_eth,
        max_hops = args.max_hops,
        protocols = ?protocols,
        "Starting atomic searcher"
    );

    // Set up shared market data
    let market_data: SharedMarketDataRef = Arc::new(RwLock::new(SharedMarketData::new()));

    // Set up Tycho feed
    let feed_config = TychoFeedConfig::new(
        args.tycho_url.clone(),
        chain,
        std::env::var("TYCHO_API_KEY").ok(),
        true,
        protocols,
        args.min_tvl,
    );

    let health_tracker = fynd::api::HealthTracker::new();
    let feed = TychoFeed::new(feed_config, market_data.clone(), health_tracker);
    let mut event_rx = feed.subscribe();

    // Spawn the feed in a background task
    tokio::spawn(async move {
        if let Err(e) = feed.run().await {
            error!("TychoFeed error: {}", e);
        }
    });

    info!("Waiting for initial market sync...");

    // Set up graph manager (same type as BellmanFord solver uses)
    let mut graph_manager = PetgraphStableDiGraphManager::<()>::default();
    let mut blocks_processed: u64 = 0;

    // Main event loop: react to each block update
    loop {
        match event_rx.recv().await {
            Ok(event) => {
                // Update graph
                if let Err(e) = graph_manager.handle_event(&event).await {
                    warn!("Graph event error: {:?}", e);
                    continue;
                }

                let block_number = {
                    let market = market_data.read().await;
                    market
                        .last_updated()
                        .map(|b| b.number)
                        .unwrap_or(0)
                };

                if blocks_processed == 0 {
                    let graph = graph_manager.graph();
                    info!(
                        block = block_number,
                        nodes = graph.node_count(),
                        edges = graph.edge_count(),
                        "Initial sync complete, starting search"
                    );
                }

                blocks_processed += 1;

                // Run the arbitrage search
                let result = search_block(
                    block_number,
                    &source_token_addr,
                    &seed_amount,
                    args.max_hops,
                    graph_manager.graph(),
                    &market_data,
                    args.gss_tolerance,
                    args.gss_max_iter,
                )
                .await;

                log_results(&result);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("Missed {} events (searcher too slow)", n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!("Feed closed, shutting down");
                break;
            }
        }
    }

    Ok(())
}

// ==================== Search Logic ====================

async fn search_block(
    block_number: u64,
    source_token_addr: &tycho_simulation::tycho_core::models::Address,
    seed_amount: &BigUint,
    max_hops: usize,
    graph: &StableDiGraph<()>,
    market_data: &SharedMarketDataRef,
    gss_tolerance: f64,
    gss_max_iter: usize,
) -> BlockSearchResult {
    let start = Instant::now();

    // Find source node in graph
    let source_node = match graph
        .node_indices()
        .find(|&n| graph[n] == *source_token_addr)
    {
        Some(n) => n,
        None => {
            return BlockSearchResult {
                block_number,
                candidates_found: 0,
                profitable_cycles: 0,
                cycles: vec![],
                search_time_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    // Acquire market data snapshot
    let market = market_data.read().await;

    // Extract subgraph around source
    let subgraph_edges = cycle_detector::extract_subgraph(source_node, max_hops, graph);

    if subgraph_edges.is_empty() {
        return BlockSearchResult {
            block_number,
            candidates_found: 0,
            profitable_cycles: 0,
            cycles: vec![],
            search_time_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Build token map for all nodes in subgraph
    let subgraph_nodes: HashSet<NodeIndex> = subgraph_edges
        .iter()
        .flat_map(|&(from, to, _)| [from, to])
        .collect();

    let token_map: HashMap<NodeIndex, Token> = subgraph_nodes
        .iter()
        .filter_map(|&node| {
            let addr = &graph[node];
            market
                .get_token(addr)
                .cloned()
                .map(|t| (node, t))
        })
        .collect();

    // Extract market subset for simulation
    let component_ids: HashSet<String> = subgraph_edges
        .iter()
        .map(|(_, _, cid)| cid.clone())
        .collect();
    let market_subset = market.extract_subset(&component_ids);

    debug!(
        block = block_number,
        subgraph_edges = subgraph_edges.len(),
        tokens = token_map.len(),
        "subgraph extracted for cycle search"
    );

    // Phase 1: Find candidate cycles via Bellman-Ford
    let candidates = cycle_detector::find_cycles(
        source_node,
        seed_amount,
        max_hops,
        graph,
        &market_subset,
        &token_map,
        &subgraph_edges,
    );

    let candidates_found = candidates.len();
    debug!(block = block_number, candidates = candidates_found, "BF scan complete");

    if candidates.is_empty() {
        return BlockSearchResult {
            block_number,
            candidates_found: 0,
            profitable_cycles: 0,
            cycles: vec![],
            search_time_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Get gas price for profit calculation
    let gas_price_wei = market_subset
        .gas_price()
        .map(|gp| gp.effective_gas_price())
        .unwrap_or_else(|| BigUint::from(30_000_000_000u64)); // 30 gwei fallback

    // For WETH-denominated cycles, gas cost in source token = gas cost in ETH = gas_price * gas_used
    // So gas_token_price is 1:1 (numerator = 1, denominator = 1)
    let gas_token_price_num = BigUint::from(1u64);
    let gas_token_price_den = BigUint::from(1u64);

    // Phase 2: Optimize each candidate via golden section search
    let mut cycles: Vec<_> = candidates
        .iter()
        .filter_map(|candidate| {
            amount_optimizer::optimize_amount(
                candidate,
                graph,
                &market_subset,
                &token_map,
                &gas_price_wei,
                &gas_token_price_num,
                &gas_token_price_den,
                seed_amount,
                gss_tolerance,
                gss_max_iter,
            )
        })
        .collect();

    // Sort by net profit descending
    cycles.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

    let profitable_cycles = cycles.iter().filter(|c| c.is_profitable).count();

    BlockSearchResult {
        block_number,
        candidates_found,
        profitable_cycles,
        cycles,
        search_time_ms: start.elapsed().as_millis() as u64,
    }
}

// ==================== Output ====================

fn log_results(result: &BlockSearchResult) {
    if result.candidates_found == 0 {
        debug!(
            block = result.block_number,
            time_ms = result.search_time_ms,
            "no cycles found"
        );
        return;
    }

    info!(
        block = result.block_number,
        candidates = result.candidates_found,
        profitable = result.profitable_cycles,
        time_ms = result.search_time_ms,
        "block search complete"
    );

    for (i, cycle) in result.cycles.iter().take(5).enumerate() {
        let path_str: Vec<String> = cycle
            .edges
            .iter()
            .map(|(from, _, cid)| format!("{}..{}", &hex::encode(from)[..6], &cid[..8.min(cid.len())]))
            .collect();

        let optimal_eth = cycle
            .optimal_amount_in
            .to_f64()
            .map(|v| v / 1e18)
            .unwrap_or(0.0);
        let profit_eth = cycle.net_profit as f64 / 1e18;
        let gas_eth = cycle
            .gas_cost
            .to_f64()
            .map(|v| v / 1e18)
            .unwrap_or(0.0);

        let status = if cycle.is_profitable { "PROFITABLE" } else { "unprofitable" };

        info!(
            "[{status}] #{i}: {path} | optimal_in: {optimal:.4} ETH | \
             net_profit: {profit:.6} ETH | gas: {gas:.6} ETH | hops: {hops}",
            status = status,
            i = i,
            path = path_str.join(" -> "),
            optimal = optimal_eth,
            profit = profit_eth,
            gas = gas_eth,
            hops = cycle.edges.len(),
        );
    }

    if result.cycles.len() > 5 {
        info!("... and {} more cycles", result.cycles.len() - 5);
    }
}
