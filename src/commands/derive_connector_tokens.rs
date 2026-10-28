use std::{cmp::Ordering, collections::HashMap, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::Args;
use fynd_core::{
    feed::{market_data::MarketData, protocol_registry::is_tycho_system},
    solver::{FyndBuilder, SolverBuildError},
    types::ComponentId,
};
use fynd_rpc::parse_chain;
use tracing::info;
use tycho_simulation::{
    evm::protocol::native_wrapper::state::NATIVE_WRAPPER_ID,
    tycho_client::rpc::{
        ComponentTvlPaginatedParams, HttpRPCClient, HttpRPCClientOptions, RPCClient,
    },
    tycho_common::models::{chain_config::TvlThresholdTier, Address, Chain},
};

/// Concurrent TVL requests per protocol system.
const TVL_FETCH_CONCURRENCY: usize = 10;

/// Derives recommended connector tokens from live Tycho market data.
///
/// Connects to Tycho, waits for the initial market snapshot, then fetches the TVL of every
/// component in it and scores each token by the components it appears in and by their summed TVL.
/// The connector tokens are the top `--top-n` by pool count together with the top `--top-n` by TVL.
/// Pool count finds the hubs; TVL finds deep tokens that sit in few pools and drops spam tokens
/// spread over many tiny ones. The chain's native and wrapped native tokens are always added.
///
/// Outputs a ranked list and a ready-to-paste TOML snippet.
#[derive(Args, PartialEq, Debug)]
pub struct DeriveConnectorTokensArgs {
    /// Target chain (e.g. Ethereum)
    #[arg(short, long, default_value = "Ethereum")]
    pub chain: String,

    /// Tycho URL. Defaults to the Fynd endpoint for the selected chain.
    #[arg(long, env)]
    pub tycho_url: Option<String>,

    /// Tycho API key.
    #[arg(long, env)]
    pub tycho_api_key: Option<String>,

    /// Disable TLS for Tycho connection.
    #[arg(long)]
    pub disable_tls: bool,

    /// Node RPC URL for the target chain. Defaults to a public endpoint if not set.
    #[arg(long, env)]
    pub rpc_url: Option<String>,

    /// Protocols to index (comma-separated). Defaults to all on-chain protocols.
    #[arg(short, long, value_delimiter = ',', value_name = "PROTO1,PROTO2")]
    pub protocols: Vec<String>,

    /// Minimum TVL threshold in native token. Components below this threshold are excluded.
    /// Defaults to a chain-specific value if not set.
    #[arg(long)]
    pub min_tvl: Option<f64>,

    /// Number of top tokens taken from each ranking (pool count and TVL).
    #[arg(long, default_value_t = 10)]
    pub top_n: usize,

    /// Minimum number of components a token must appear in to be included.
    #[arg(long, default_value_t = 2)]
    pub min_component_count: usize,

    /// Output format: "toml", "json", or "text".
    #[arg(long, default_value = "toml")]
    pub output: String,

    /// How long to wait for the initial Tycho snapshot (seconds).
    #[arg(long, default_value_t = 120)]
    pub wait_secs: u64,

    /// Path to the custom-chains config (chains.yaml). Required to run a chain that Tycho does
    /// not know as a built-in. Uses the same file the indexer reads.
    #[arg(long, env = "TYCHO_CHAINS_CONFIG")]
    pub chains_config: Option<PathBuf>,
}

pub async fn run(args: DeriveConnectorTokensArgs) -> Result<()> {
    if let Some(path) = &args.chains_config {
        fynd_rpc::init_chain_registry_from_file(path)
            .map_err(|e| anyhow::anyhow!("failed to load chains config: {e}"))?;
        info!(?path, "installed custom chain registry");
    }

    let chain = parse_chain(&args.chain).context("invalid chain")?;
    let tycho_url = crate::serve::resolve_tycho_url(&args.chain, args.tycho_url.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rpc_url = crate::serve::resolve_rpc_url(&args.chain, args.rpc_url.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let protocols = fynd_rpc::protocols::resolve_protocols(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &args.protocols,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    info!(?protocols, "starting market data feed with {} protocol(s)", protocols.len());

    let min_tvl = args
        .min_tvl
        .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Low));
    let mut builder = FyndBuilder::new(chain, tycho_url.clone(), rpc_url, protocols, min_tvl)
        .algorithm("most_liquid");
    if let Some(key) = &args.tycho_api_key {
        builder = builder.tycho_api_key(key.clone());
    }
    if args.disable_tls {
        builder = builder.tycho_use_tls(false);
    }

    let solver = builder
        .build()
        .map_err(|e: SolverBuildError| anyhow::anyhow!("{e}"))?;
    let market_data = solver.market_data();

    // We only need component topology and token symbols — market data is sufficient.
    // wait_until_ready also waits for derived computations (spot prices, component depths, etc.)
    // which is expensive and unnecessary here.
    info!("Waiting for Tycho initial snapshot (up to {}s)…", args.wait_secs);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.wait_secs);
    loop {
        if market_data
            .read()
            .await
            .last_updated()
            .is_some()
        {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out after {}s waiting for initial Tycho snapshot",
            args.wait_secs
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let (components, symbols) = snapshot_components(&market_data).await;
    solver.shutdown();

    let tvl = fetch_component_tvl(
        &tycho_url,
        args.tycho_api_key.as_deref(),
        !args.disable_tls,
        chain,
        &components,
    )
    .await?;

    let scores = score_tokens(&components, &tvl, &symbols);
    let total = scores.len();
    let wrap_pair: Vec<Address> = std::iter::once(chain.native_token())
        .chain(chain.wrapped_native_token())
        .map(|token| token.address)
        .collect();
    let selected = select_connectors(scores, &wrap_pair, args.top_n, args.min_component_count);
    let tvl_unit = chain.native_token().symbol;

    match args.output.as_str() {
        "toml" => print_toml(&selected, &args.chain, &tvl_unit, args.top_n, total),
        "json" => print_json(&selected)?,
        _ => print_text(&selected, &tvl_unit),
    }

    Ok(())
}

/// A topology component: its id, the Tycho protocol system it belongs to and the tokens it holds.
struct SnapshotComponent {
    id: ComponentId,
    protocol_system: String,
    tokens: Vec<Address>,
}

/// What the ranking knows about one token.
#[derive(Clone, Debug, PartialEq)]
struct TokenScore {
    symbol: String,
    component_count: usize,
    /// Summed TVL, in the chain's native token, of every component holding the token.
    tvl: f64,
}

/// Which ranking put a token in the connector list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectedBy {
    PoolCount,
    Tvl,
    Both,
    WrapPair,
}

impl SelectedBy {
    fn as_str(self) -> &'static str {
        match self {
            SelectedBy::PoolCount => "pool_count",
            SelectedBy::Tvl => "tvl",
            SelectedBy::Both => "both",
            SelectedBy::WrapPair => "wrap_pair",
        }
    }
}

/// A token chosen as a connector, with its 1-based rank in each ranking.
#[derive(Clone, Debug, PartialEq)]
struct ConnectorSelection {
    address: Address,
    score: TokenScore,
    component_rank: usize,
    tvl_rank: usize,
    selected_by: SelectedBy,
}

async fn snapshot_components(
    market_data: &MarketData,
) -> (Vec<SnapshotComponent>, HashMap<Address, String>) {
    let guard = market_data.read().await;
    let mut components = Vec::new();
    let mut symbols = HashMap::new();
    for (id, topology) in guard.component_topology() {
        let Some(component) = guard.get_component(&id) else { continue };
        for addr in &topology.tokens {
            if let Some(token) = guard.get_token(addr) {
                symbols.insert(addr.clone(), token.symbol.clone());
            }
        }
        components.push(SnapshotComponent {
            id,
            protocol_system: component.protocol_system.clone(),
            tokens: topology.tokens,
        });
    }
    (components, symbols)
}

/// Fetches the TVL of the topology's components from Tycho, one request per protocol system.
///
/// Components of non-Tycho systems (RFQ, price level books, the synthetic native wrapper) and
/// components Tycho reports no TVL for are absent from the result.
async fn fetch_component_tvl(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
    components: &[SnapshotComponent],
) -> Result<HashMap<ComponentId, f64>> {
    let rpc_url =
        if use_tls { format!("https://{tycho_url}") } else { format!("http://{tycho_url}") };
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options)
        .context("failed to build Tycho RPC client for TVL fetch")?;

    let mut ids_by_system: HashMap<&str, Vec<ComponentId>> = HashMap::new();
    for component in components {
        if is_tycho_system(&component.protocol_system) &&
            component.protocol_system != NATIVE_WRAPPER_ID
        {
            ids_by_system
                .entry(&component.protocol_system)
                .or_default()
                .push(component.id.clone());
        }
    }

    let mut tvl = HashMap::new();
    for (system, ids) in ids_by_system {
        let params = ComponentTvlPaginatedParams::new(chain, TVL_FETCH_CONCURRENCY)
            .with_protocol_system(system)
            .with_component_ids(ids);
        let system_tvl = rpc_client
            .get_component_tvl_paginated(params)
            .await
            .with_context(|| {
                format!("failed to fetch component TVL for protocol system {system}")
            })?;
        info!(system, components = system_tvl.len(), "fetched component TVL");
        tvl.extend(system_tvl);
    }
    Ok(tvl)
}

/// Counts the components holding each token and sums their TVL; a component with no TVL entry
/// adds 0.
fn score_tokens(
    components: &[SnapshotComponent],
    tvl: &HashMap<ComponentId, f64>,
    symbols: &HashMap<Address, String>,
) -> Vec<(Address, TokenScore)> {
    let mut scores: HashMap<Address, TokenScore> = HashMap::new();
    for component in components {
        let component_tvl = tvl
            .get(&component.id)
            .copied()
            .unwrap_or(0.0);
        for addr in &component.tokens {
            let score = scores
                .entry(addr.clone())
                .or_insert_with(|| TokenScore {
                    symbol: symbols
                        .get(addr)
                        .cloned()
                        .unwrap_or_else(|| "?".to_string()),
                    component_count: 0,
                    tvl: 0.0,
                });
            score.component_count += 1;
            score.tvl += component_tvl;
        }
    }
    scores.into_iter().collect()
}

/// The 1-based rank of each score under `order`, ties broken by address.
fn rank_by(
    scores: &[(Address, TokenScore)],
    order: impl Fn(&TokenScore, &TokenScore) -> Ordering,
) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..scores.len()).collect();
    indices.sort_by(|&a, &b| {
        order(&scores[a].1, &scores[b].1).then_with(|| {
            scores[a]
                .0
                .as_ref()
                .cmp(scores[b].0.as_ref())
        })
    });
    let mut ranks = vec![0; scores.len()];
    for (position, index) in indices.into_iter().enumerate() {
        ranks[index] = position + 1;
    }
    ranks
}

/// Picks the top `top_n` tokens by pool count and by TVL, then the wrap pair if absent.
///
/// Only tokens in at least `min_component_count` components are ranked, and a token with no TVL
/// is never picked by TVL. The result is ordered by TVL rank.
fn select_connectors(
    mut scores: Vec<(Address, TokenScore)>,
    wrap_pair: &[Address],
    top_n: usize,
    min_component_count: usize,
) -> Vec<ConnectorSelection> {
    scores.retain(|(address, score)| {
        score.component_count >= min_component_count || wrap_pair.contains(address)
    });
    let component_ranks = rank_by(&scores, |a, b| {
        b.component_count
            .cmp(&a.component_count)
    });
    let tvl_ranks = rank_by(&scores, |a, b| b.tvl.total_cmp(&a.tvl));

    let mut selected = Vec::new();
    for (i, (address, score)) in scores.into_iter().enumerate() {
        let (component_rank, tvl_rank) = (component_ranks[i], tvl_ranks[i]);
        let meets_min = score.component_count >= min_component_count;
        let by_count = meets_min && component_rank <= top_n;
        let by_tvl = meets_min && score.tvl > 0.0 && tvl_rank <= top_n;
        let selected_by = match (by_count, by_tvl) {
            (true, true) => SelectedBy::Both,
            (true, false) => SelectedBy::PoolCount,
            (false, true) => SelectedBy::Tvl,
            (false, false) if wrap_pair.contains(&address) => SelectedBy::WrapPair,
            (false, false) => continue,
        };
        selected.push(ConnectorSelection { address, score, component_rank, tvl_rank, selected_by });
    }
    selected.sort_by_key(|entry| entry.tvl_rank);
    selected
}

fn print_toml(
    selected: &[ConnectorSelection],
    chain: &str,
    tvl_unit: &str,
    top_n: usize,
    total: usize,
) {
    use chrono::Utc;
    let date = Utc::now().format("%Y-%m-%d");
    println!("# Derived connector tokens for {chain} ({date})");
    println!(
        "# Top {top_n} by pool count + top {top_n} by TVL, plus the wrap pair: {} of {total} tokens.",
        selected.len()
    );
    println!("connector_tokens = [");
    for entry in selected {
        println!(
            "    \"0x{}\",  # {}  — {} pools (#{}), {:.0} {tvl_unit} TVL (#{}), by {}",
            hex::encode(entry.address.as_ref()),
            entry.score.symbol,
            entry.score.component_count,
            entry.component_rank,
            entry.score.tvl,
            entry.tvl_rank,
            entry.selected_by.as_str(),
        );
    }
    println!("]");
}

fn print_json(selected: &[ConnectorSelection]) -> Result<()> {
    let entries: Vec<serde_json::Value> = selected
        .iter()
        .map(|entry| {
            serde_json::json!({
                "address": format!("0x{}", hex::encode(entry.address.as_ref())),
                "symbol": entry.score.symbol,
                "component_count": entry.score.component_count,
                "tvl": entry.score.tvl,
                "component_rank": entry.component_rank,
                "tvl_rank": entry.tvl_rank,
                "selected_by": entry.selected_by.as_str(),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}

fn print_text(selected: &[ConnectorSelection], tvl_unit: &str) {
    println!(
        "{:<10} {:>6} {:>10} {:>16} {:>9}  {:<10} Address",
        "Symbol",
        "Pools",
        "Pool rank",
        format!("TVL ({tvl_unit})"),
        "TVL rank",
        "Selected"
    );
    println!("{:-<100}", "");
    for entry in selected {
        println!(
            "{:<10} {:>6} {:>10} {:>16.0} {:>9}  {:<10} 0x{}",
            entry.score.symbol,
            entry.score.component_count,
            entry.component_rank,
            entry.score.tvl,
            entry.tvl_rank,
            entry.selected_by.as_str(),
            hex::encode(entry.address.as_ref()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn score(symbol: &str, component_count: usize, tvl: f64) -> (Address, TokenScore) {
        let byte = symbol.as_bytes()[0];
        (addr(byte), TokenScore { symbol: symbol.to_string(), component_count, tvl })
    }

    fn component(id: &str, tokens: &[u8]) -> SnapshotComponent {
        SnapshotComponent {
            id: id.to_string(),
            protocol_system: "uniswap_v2".to_string(),
            tokens: tokens
                .iter()
                .map(|b| addr(*b))
                .collect(),
        }
    }

    fn selection(
        scores: Vec<(Address, TokenScore)>,
        wrap_pair: &[Address],
        top_n: usize,
        min_component_count: usize,
    ) -> Vec<(String, SelectedBy)> {
        select_connectors(scores, wrap_pair, top_n, min_component_count)
            .into_iter()
            .map(|entry| (entry.score.symbol, entry.selected_by))
            .collect()
    }

    #[test]
    fn test_score_tokens_sums_tvl_and_counts_missing_as_zero() {
        let components = [component("a", &[1, 2]), component("b", &[1, 3])];
        let tvl = HashMap::from([("a".to_string(), 10.0)]);

        let mut scores = score_tokens(&components, &tvl, &HashMap::new());
        scores.sort_by_key(|(address, _)| address.clone());

        let summary: Vec<(usize, f64)> = scores
            .iter()
            .map(|(_, s)| (s.component_count, s.tvl))
            .collect();
        assert_eq!(summary, vec![(2, 10.0), (1, 10.0), (1, 0.0)]);
    }

    #[test]
    fn test_select_connectors_by_each_ranking() {
        // Deep: few pools, large TVL. Spam: many pools, no TVL.
        let scores =
            vec![score("Deep", 2, 1000.0), score("Spam", 50, 0.0), score("Filler", 3, 1.0)];

        let picked = selection(scores, &[], 1, 2);

        assert_eq!(
            picked,
            vec![
                ("Deep".to_string(), SelectedBy::Tvl),
                ("Spam".to_string(), SelectedBy::PoolCount)
            ]
        );
    }

    #[test]
    fn test_select_connectors_token_in_both_lists_appears_once() {
        let scores = vec![score("Hub", 50, 1000.0), score("Small", 3, 1.0)];

        let picked = selection(scores, &[], 2, 2);

        assert_eq!(
            picked,
            vec![("Hub".to_string(), SelectedBy::Both), ("Small".to_string(), SelectedBy::Both)]
        );
    }

    #[test]
    fn test_select_connectors_min_component_count_applies_to_both() {
        let scores = vec![score("Lone", 1, 5000.0), score("Pair", 2, 10.0)];

        // With one slot per ranking, the ineligible "Lone" must not take the TVL slot.
        let picked = selection(scores, &[], 1, 2);

        assert_eq!(picked, vec![("Pair".to_string(), SelectedBy::Both)]);
    }

    #[test]
    fn test_select_connectors_adds_wrap_pair_when_unselected() {
        let scores = vec![score("Hub", 50, 1000.0), score("Weth", 1, 0.0), score("Other", 1, 0.0)];

        let picked = selection(scores, &[addr(b'W'), addr(b'X')], 1, 2);

        assert_eq!(
            picked,
            vec![("Hub".to_string(), SelectedBy::Both), ("Weth".to_string(), SelectedBy::WrapPair)]
        );
    }

    #[test]
    fn test_select_connectors_orders_by_tvl_rank_and_breaks_ties_by_address() {
        let scores = vec![score("B", 5, 10.0), score("A", 5, 10.0), score("C", 9, 20.0)];

        let ranked = select_connectors(scores, &[], 3, 1);

        let order: Vec<&str> = ranked
            .iter()
            .map(|entry| entry.score.symbol.as_str())
            .collect();
        assert_eq!(order, vec!["C", "A", "B"]);
        assert_eq!(ranked[1].component_rank, 2);
    }
}
