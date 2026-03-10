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
mod executor;
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

use crate::executor::CycleExecutor;
use crate::types::{BlockSearchResult, ExecutionMode};

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

    /// Path to blacklist.toml (pools to exclude from search).
    #[arg(long, default_value = "blacklist.toml")]
    blacklist: String,

    /// Token addresses to exclude (comma-separated). Cycles through these tokens
    /// are filtered out. Use for rebase tokens (AMPL) or other tokens with broken
    /// simulations.
    #[arg(
        long,
        default_value = "0xd46ba6d942050d489dbd938a2c909a5d5039a161"
    )]
    blacklist_tokens: String,

    /// Minimum net profit in basis points to attempt execution.
    /// Cycles below this threshold are logged but not executed.
    #[arg(long, default_value_t = 0)]
    min_profit_bps: i64,

    /// Slippage tolerance in basis points for execution encoding.
    #[arg(long, default_value_t = 50)]
    slippage_bps: u32,

    /// Percentage of profit to use as builder bribe (Flashbots).
    #[arg(long, default_value_t = 100)]
    bribe_pct: u32,

    /// Execution mode: log-only, simulate, execute-public, execute-protected.
    #[arg(long, default_value = "log-only")]
    execution_mode: String,

    /// Private key for signing transactions (hex, with or without 0x prefix).
    /// Also reads from PRIVATE_KEY env var.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: Option<String>,

    /// RPC URL for simulation/execution.
    /// Also reads from RPC_URL env var.
    #[arg(long, env = "RPC_URL")]
    rpc_url: Option<String>,
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

    if !args.seed_eth.is_finite() || args.seed_eth <= 0.0 {
        anyhow::bail!(
            "seed_eth must be a positive finite number, got: {}",
            args.seed_eth
        );
    }

    let execution_mode = ExecutionMode::from_str_arg(&args.execution_mode)
        .map_err(|e| anyhow::anyhow!(e))?;

    let chain = parse_chain(&args.chain)?;

    let cycle_executor = CycleExecutor::new(
        chain,
        args.slippage_bps,
        args.bribe_pct,
        args.rpc_url.clone(),
        args.private_key.clone(),
    )?;
    let source_token_addr = native_token(&chain)?;
    let protocols: Vec<String> = args
        .protocols
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    // Seed amount in wei (1 ETH = 1e18 wei)
    let seed_amount = BigUint::from((args.seed_eth * 1e18) as u128);

    // Load pool blacklist
    let blacklist = match fynd::BlacklistConfig::load_from_file(&args.blacklist) {
        Ok(config) => {
            info!(blacklisted_pools = config.components.len(), "loaded blacklist");
            config.components
        }
        Err(e) => {
            warn!(path = %args.blacklist, error = %e, "blacklist not loaded, continuing without");
            HashSet::new()
        }
    };

    // Parse token blacklist (rebase tokens, broken simulations, etc.)
    let blacklisted_tokens: HashSet<Vec<u8>> = args
        .blacklist_tokens
        .split(',')
        .filter_map(|s| {
            let s = s.trim().strip_prefix("0x").unwrap_or(s.trim());
            hex::decode(s).ok()
        })
        .collect();
    if !blacklisted_tokens.is_empty() {
        info!(
            blacklisted_tokens = blacklisted_tokens.len(),
            "token blacklist active (AMPL, etc.)"
        );
    }

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

    // Set up Tycho feed with blacklist
    let feed_config = TychoFeedConfig::new(
        args.tycho_url.clone(),
        chain,
        std::env::var("TYCHO_API_KEY").ok(),
        true,
        protocols,
        args.min_tvl,
    )
    .blacklisted_components(blacklist);

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
                    &blacklisted_tokens,
                    args.min_profit_bps,
                )
                .await;

                log_results(&result);

                // Execute best profitable cycle if mode != LogOnly.
                // The executor acquires the market lock only during
                // build_solution and drops it before any network I/O,
                // so TychoFeed writes are not blocked.
                if execution_mode != ExecutionMode::LogOnly {
                    if let Some(best) = result.cycles.iter().find(|c| c.is_profitable) {
                        let exec_result = cycle_executor
                            .execute_cycle(
                                best,
                                &market_data,
                                &source_token_addr,
                                &execution_mode,
                            )
                            .await;
                        info!(
                            mode = ?exec_result.mode,
                            success = exec_result.success,
                            tx_hash = ?exec_result.tx_hash,
                            gas_used = ?exec_result.gas_used,
                            msg = %exec_result.message,
                            "execution result"
                        );
                    }
                }
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

#[allow(clippy::too_many_arguments)]
async fn search_block(
    block_number: u64,
    source_token_addr: &tycho_simulation::tycho_core::models::Address,
    seed_amount: &BigUint,
    max_hops: usize,
    graph: &StableDiGraph<()>,
    market_data: &SharedMarketDataRef,
    gss_tolerance: f64,
    gss_max_iter: usize,
    blacklisted_tokens: &HashSet<Vec<u8>>,
    min_profit_bps: i64,
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

    // Pre-compute blacklisted node indices from token addresses
    let blacklisted_nodes: HashSet<NodeIndex> = if blacklisted_tokens.is_empty() {
        HashSet::new()
    } else {
        graph
            .node_indices()
            .filter(|&n| blacklisted_tokens.contains(graph[n].as_ref()))
            .collect()
    };

    // Extract subgraph around source (blacklisted tokens excluded at graph level)
    let subgraph_edges =
        cycle_detector::extract_subgraph(source_node, max_hops, graph, &blacklisted_nodes);

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

    // Filter by minimum profit threshold (in basis points of amount_in)
    if min_profit_bps > 0 {
        cycles.retain(|c| {
            if c.net_profit <= 0 {
                return false;
            }
            let amount_in_f = c.optimal_amount_in.to_f64().unwrap_or(1.0).max(1.0);
            let profit_bps = (c.net_profit as f64 / amount_in_f * 10_000.0) as i64;
            profit_bps >= min_profit_bps
        });
    }

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
    info!(
        block = result.block_number,
        candidates = result.candidates_found,
        profitable = result.profitable_cycles,
        time_ms = result.search_time_ms,
        "block search complete"
    );

    for (i, cycle) in result.cycles.iter().take(5).enumerate() {
        // Show tokens and pools in the cycle
        let path_parts: Vec<String> = cycle
            .edges
            .iter()
            .map(|(from, _to, cid)| {
                let token_hex = hex::encode(from);
                let pool_short = &cid[..10.min(cid.len())];
                format!("0x{}.. -[{}..]->", &token_hex[..8], pool_short)
            })
            .collect();
        // Close the cycle back to source
        let source_hex = cycle
            .edges
            .first()
            .map(|(from, _, _)| format!("0x{}..", &hex::encode(from)[..8]))
            .unwrap_or_default();
        let path_display = format!("{} {}", path_parts.join(" "), source_hex);

        // Full pool list for investigation
        let pools: Vec<&str> = cycle.edges.iter().map(|(_, _, cid)| cid.as_str()).collect();

        let optimal_eth = cycle
            .optimal_amount_in
            .to_f64()
            .map(|v| v / 1e18)
            .unwrap_or(0.0);
        let gross_eth = cycle
            .gross_profit
            .to_f64()
            .map(|v| v / 1e18)
            .unwrap_or(0.0);
        let net_eth = cycle.net_profit as f64 / 1e18;
        let gas_eth = cycle
            .gas_cost
            .to_f64()
            .map(|v| v / 1e18)
            .unwrap_or(0.0);

        let status = if cycle.is_profitable {
            "PROFITABLE"
        } else if gross_eth > 0.0 {
            "GROSS+"
        } else {
            "no-arb"
        };

        info!(
            "[{status}] #{i}: {path} | in: {optimal:.4} ETH | \
             gross: {gross:.6} ETH | gas: {gas:.6} ETH | net: {net:.6} ETH | hops: {hops}",
            status = status,
            i = i,
            path = path_display,
            optimal = optimal_eth,
            gross = gross_eth,
            gas = gas_eth,
            net = net_eth,
            hops = cycle.edges.len(),
        );
        info!("  pools: {:?}", pools);
        // For top cycle, show full token addresses for debugging
        if i == 0 {
            for (j, (from, to, cid)) in cycle.edges.iter().enumerate() {
                info!(
                    "  hop {j}: 0x{from} -> 0x{to} via {cid}",
                    j = j,
                    from = hex::encode(from),
                    to = hex::encode(to),
                    cid = cid,
                );
            }
        }
    }

    if result.cycles.len() > 5 {
        info!("... and {} more cycles", result.cycles.len() - 5);
    }
}
