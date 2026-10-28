use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

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
        ComponentTvlPaginatedParams, HttpRPCClient, HttpRPCClientOptions,
        ProtocolStatesPaginatedParams, RPCClient,
    },
    tycho_common::models::{chain_config::TvlThresholdTier, Address, Chain},
};

/// Concurrent TVL and balance requests per protocol system.
const TVL_FETCH_CONCURRENCY: usize = 10;

/// Components per balance request; states carry every attribute, so pages are kept small.
const BALANCE_FETCH_CHUNK_SIZE: usize = 100;

/// Derives recommended connector tokens from live Tycho market data.
///
/// Connects to Tycho, waits for the initial market snapshot, then fetches the TVL and token
/// balances of every component in it. Raw TVL can be faked: tokens that only trade against each
/// other at self-set prices report huge values. So each pool is credited only up to twice the
/// native value of the trusted tokens it holds, where the trusted tokens are the chain's native
/// and wrapped native tokens plus the `--trusted-tokens` tokens with the most native backing
/// (native balance held in pools that pair them with native). A token's backed TVL is its pools'
/// credit minus its largest pool's, since a middle hop needs depth on both sides.
///
/// The connector tokens are the top `--top-n` by pool count (among tokens whose backed TVL
/// reaches the minimum TVL) together with the top `--top-n-tvl` by backed TVL. Pool count finds
/// the hubs; backed TVL finds deep tokens that sit in few pools. The chain's native and wrapped
/// native tokens are always added.
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

    /// Number of top tokens taken from the pool-count ranking.
    #[arg(long, default_value_t = 10)]
    pub top_n: usize,

    /// Number of top tokens taken from the backed TVL ranking. Defaults to `--top-n`.
    #[arg(long)]
    pub top_n_tvl: Option<usize>,

    /// Minimum number of components a token must appear in to be included.
    #[arg(long, default_value_t = 2)]
    pub min_component_count: usize,

    /// Number of tokens, besides the native and wrapped native tokens, whose price is trusted to
    /// credit pool TVL. They are the ones with the most native balance in native pools.
    #[arg(long, default_value_t = 25)]
    pub trusted_tokens: usize,

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

    let (components, mut tokens) = snapshot_components(&market_data).await;
    solver.shutdown();

    let anchor_tokens: Vec<_> = std::iter::once(chain.native_token())
        .chain(chain.wrapped_native_token())
        .collect();
    let wrap_pair: Vec<Address> = anchor_tokens
        .iter()
        .map(|token| token.address.clone())
        .collect();
    for token in anchor_tokens {
        tokens
            .entry(token.address)
            .or_insert(TokenInfo { symbol: token.symbol, decimals: token.decimals });
    }

    let rpc_client =
        build_rpc_client(&tycho_url, args.tycho_api_key.as_deref(), !args.disable_tls)?;
    let tvl = fetch_component_tvl(&rpc_client, chain, &components).await?;
    let balances = fetch_component_balances(&rpc_client, chain, &components, &tokens).await?;
    let pools = PoolData { components: &components, tvl: &tvl, balances: &balances };

    let backing = native_backing(&pools, &wrap_pair);
    let prices = trusted_prices(&pools, &wrap_pair, &backing, args.trusted_tokens);
    let credit = backed_credit(&pools, &prices);
    let scores = score_tokens(&components, &credit, &tokens);
    let total = scores.len();
    let sizes = RankingSizes { pool_count: args.top_n, tvl: args.top_n_tvl.unwrap_or(args.top_n) };
    let selected = select_connectors(scores, &wrap_pair, sizes, args.min_component_count, min_tvl);
    let tvl_unit = chain.native_token().symbol;

    match args.output.as_str() {
        "toml" => print_toml(&selected, &args.chain, &tvl_unit, sizes, total),
        "json" => print_json(&selected)?,
        _ => print_text(&selected, &tvl_unit),
    }

    Ok(())
}

/// How many top tokens to take from each ranking.
#[derive(Clone, Copy, Debug)]
struct RankingSizes {
    pool_count: usize,
    tvl: usize,
}

/// A topology component: its id, the Tycho protocol system it belongs to and the tokens it holds.
struct SnapshotComponent {
    id: ComponentId,
    protocol_system: String,
    tokens: Vec<Address>,
}

/// The symbol and decimals of a token in the snapshot.
#[derive(Clone, Debug)]
struct TokenInfo {
    symbol: String,
    decimals: u32,
}

/// Token balances of each component, in whole token units.
type Balances = HashMap<ComponentId, HashMap<Address, f64>>;

/// The components with their TVL (in native token) and token balances.
///
/// A component missing from `tvl` or `balances` has none known.
struct PoolData<'a> {
    components: &'a [SnapshotComponent],
    tvl: &'a HashMap<ComponentId, f64>,
    balances: &'a Balances,
}

/// What the ranking knows about one token.
#[derive(Clone, Debug, PartialEq)]
struct TokenScore {
    symbol: String,
    component_count: usize,
    /// Credited TVL, in the chain's native token, of the token's components outside the largest.
    backed_tvl: f64,
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

/// A token chosen as a connector, with its 1-based rank in each ranking, if it qualifies for it.
#[derive(Clone, Debug, PartialEq)]
struct ConnectorSelection {
    address: Address,
    score: TokenScore,
    component_rank: Option<usize>,
    tvl_rank: Option<usize>,
    selected_by: SelectedBy,
}

async fn snapshot_components(
    market_data: &MarketData,
) -> (Vec<SnapshotComponent>, HashMap<Address, TokenInfo>) {
    let guard = market_data.read().await;
    let mut components = Vec::new();
    let mut tokens = HashMap::new();
    for (id, topology) in guard.component_topology() {
        let Some(component) = guard.get_component(&id) else { continue };
        for addr in &topology.tokens {
            if let Some(token) = guard.get_token(addr) {
                tokens.insert(
                    addr.clone(),
                    TokenInfo { symbol: token.symbol.clone(), decimals: token.decimals },
                );
            }
        }
        components.push(SnapshotComponent {
            id,
            protocol_system: component.protocol_system.clone(),
            tokens: topology.tokens,
        });
    }
    (components, tokens)
}

fn build_rpc_client(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
) -> Result<HttpRPCClient> {
    let rpc_url =
        if use_tls { format!("https://{tycho_url}") } else { format!("http://{tycho_url}") };
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    HttpRPCClient::new(&rpc_url, rpc_options).context("failed to build Tycho RPC client")
}

/// Groups the ids of the components Tycho serves by protocol system.
///
/// Non-Tycho systems (RFQ, price level books, the synthetic native wrapper) are left out.
fn tycho_ids_by_system(components: &[SnapshotComponent]) -> HashMap<&str, Vec<ComponentId>> {
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
    ids_by_system
}

/// Fetches the TVL of the topology's components from Tycho, one request per protocol system.
///
/// Components of non-Tycho systems and components Tycho reports no TVL for are absent from the
/// result.
async fn fetch_component_tvl(
    rpc_client: &HttpRPCClient,
    chain: Chain,
    components: &[SnapshotComponent],
) -> Result<HashMap<ComponentId, f64>> {
    let mut tvl = HashMap::new();
    for (system, ids) in tycho_ids_by_system(components) {
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

/// Fetches the token balances of the topology's components from Tycho, one request series per
/// protocol system.
///
/// Balances are in whole token units. Components of non-Tycho systems are absent, and so are
/// balances of tokens whose decimals are unknown.
async fn fetch_component_balances(
    rpc_client: &HttpRPCClient,
    chain: Chain,
    components: &[SnapshotComponent],
    tokens: &HashMap<Address, TokenInfo>,
) -> Result<Balances> {
    let mut balances = Balances::new();
    for (system, ids) in tycho_ids_by_system(components) {
        let params = ProtocolStatesPaginatedParams::new(chain, system, TVL_FETCH_CONCURRENCY)
            .with_protocol_ids(ids)
            .with_include_balances(true)
            .with_chunk_size(BALANCE_FETCH_CHUNK_SIZE);
        let states = rpc_client
            .get_protocol_states_paginated(params)
            .await
            .with_context(|| {
                format!("failed to fetch component balances for protocol system {system}")
            })?;
        info!(system, components = states.len(), "fetched component balances");
        for state in states {
            let mut whole_balances = HashMap::new();
            for (token, raw) in state.balances {
                let Some(info) = tokens.get(&token) else { continue };
                whole_balances.insert(token, raw_to_whole_units(raw.as_ref(), info.decimals));
            }
            balances.insert(state.component_id, whole_balances);
        }
    }
    Ok(balances)
}

/// Converts a big-endian raw token amount to whole token units.
fn raw_to_whole_units(raw: &[u8], decimals: u32) -> f64 {
    let mut amount = 0.0;
    for byte in raw {
        amount = amount * 256.0 + f64::from(*byte);
    }
    amount / 10f64.powi(decimals as i32)
}

/// The summed balance of the anchor tokens in one component.
fn anchor_balance(balances: &HashMap<Address, f64>, anchors: &[Address]) -> f64 {
    anchors
        .iter()
        .filter_map(|anchor| balances.get(anchor))
        .sum()
}

/// The anchor balance, in native units, held in the components of each non-anchor token.
fn native_backing(pools: &PoolData, anchors: &[Address]) -> HashMap<Address, f64> {
    let mut backing: HashMap<Address, f64> = HashMap::new();
    for component in pools.components {
        let Some(balances) = pools.balances.get(&component.id) else { continue };
        let held = anchor_balance(balances, anchors);
        if held <= 0.0 {
            continue;
        }
        for token in component
            .tokens
            .iter()
            .filter(|token| !anchors.contains(token))
        {
            *backing
                .entry(token.clone())
                .or_default() += held;
        }
    }
    backing
}

/// The `count` tokens with the most backing, ties broken by address.
fn most_backed(backing: &HashMap<Address, f64>, count: usize) -> HashSet<&Address> {
    let mut ranked: Vec<(&Address, f64)> = backing
        .iter()
        .map(|(token, amount)| (token, *amount))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.0.cmp(b.0))
    });
    ranked
        .into_iter()
        .take(count)
        .map(|(token, _)| token)
        .collect()
}

/// The native price per whole token of the anchors (1) and of the `trusted_count` most backed
/// tokens.
///
/// A trusted token is priced from its two-token component with the largest anchor balance: the
/// component's TVL minus the anchor side, over the token's balance. A trusted token with no such
/// component is left unpriced.
fn trusted_prices(
    pools: &PoolData,
    anchors: &[Address],
    backing: &HashMap<Address, f64>,
    trusted_count: usize,
) -> HashMap<Address, f64> {
    let trusted = most_backed(backing, trusted_count);
    let mut prices: HashMap<Address, f64> = anchors
        .iter()
        .map(|anchor| (anchor.clone(), 1.0))
        .collect();
    let mut deepest_anchor_side: HashMap<&Address, f64> = HashMap::new();
    for component in pools.components {
        let [first, second] = component.tokens.as_slice() else { continue };
        let Some(balances) = pools.balances.get(&component.id) else { continue };
        let anchor_side = anchor_balance(balances, anchors);
        let pool_tvl = pools
            .tvl
            .get(&component.id)
            .copied()
            .unwrap_or(0.0);
        for token in [first, second] {
            let amount = balances
                .get(token)
                .copied()
                .unwrap_or(0.0);
            let deepest = deepest_anchor_side
                .get(token)
                .copied()
                .unwrap_or(0.0);
            if trusted.contains(token) && amount > 0.0 && anchor_side > deepest {
                deepest_anchor_side.insert(token, anchor_side);
                prices.insert(token.clone(), (pool_tvl - anchor_side).max(0.0) / amount);
            }
        }
    }
    prices
}

/// Each component's TVL capped at twice the native value of the priced tokens it holds.
///
/// A component holding no priced token is credited 0.
fn backed_credit(pools: &PoolData, prices: &HashMap<Address, f64>) -> HashMap<ComponentId, f64> {
    let mut credit = HashMap::new();
    for component in pools.components {
        let mut held_value = 0.0;
        if let Some(balances) = pools.balances.get(&component.id) {
            for token in &component.tokens {
                if let (Some(price), Some(amount)) = (prices.get(token), balances.get(token)) {
                    held_value += price * amount;
                }
            }
        }
        let pool_tvl = pools
            .tvl
            .get(&component.id)
            .copied()
            .unwrap_or(0.0);
        credit.insert(component.id.clone(), pool_tvl.min(2.0 * held_value));
    }
    credit
}

/// Counts the components holding each token and sums their credit minus the largest one's; a
/// component with no credit entry adds 0.
fn score_tokens(
    components: &[SnapshotComponent],
    credit: &HashMap<ComponentId, f64>,
    tokens: &HashMap<Address, TokenInfo>,
) -> Vec<(Address, TokenScore)> {
    let mut totals: HashMap<Address, (usize, f64, f64)> = HashMap::new();
    for component in components {
        let component_credit = credit
            .get(&component.id)
            .copied()
            .unwrap_or(0.0);
        for addr in &component.tokens {
            let (count, sum, largest) = totals.entry(addr.clone()).or_default();
            *count += 1;
            *sum += component_credit;
            *largest = largest.max(component_credit);
        }
    }
    totals
        .into_iter()
        .map(|(addr, (component_count, sum, largest))| {
            let symbol = tokens
                .get(&addr)
                .map_or_else(|| "?".to_string(), |info| info.symbol.clone());
            (addr, TokenScore { symbol, component_count, backed_tvl: sum - largest })
        })
        .collect()
}

/// The 1-based rank of each score that `qualifies` under `order`, ties broken by address.
fn rank_by(
    scores: &[(Address, TokenScore)],
    qualifies: impl Fn(&TokenScore) -> bool,
    order: impl Fn(&TokenScore, &TokenScore) -> Ordering,
) -> Vec<Option<usize>> {
    let mut indices: Vec<usize> = (0..scores.len())
        .filter(|&i| qualifies(&scores[i].1))
        .collect();
    indices.sort_by(|&a, &b| {
        order(&scores[a].1, &scores[b].1).then_with(|| {
            scores[a]
                .0
                .as_ref()
                .cmp(scores[b].0.as_ref())
        })
    });
    let mut ranks = vec![None; scores.len()];
    for (position, index) in indices.into_iter().enumerate() {
        ranks[index] = Some(position + 1);
    }
    ranks
}

/// Picks the top tokens by pool count and by backed TVL, as many as `sizes` says, then the wrap
/// pair if absent.
///
/// Only tokens in at least `min_component_count` components are ranked. A token needs backed TVL
/// above 0 to be ranked by TVL, and at least `min_tvl` to be ranked by pool count. The result is
/// ordered by TVL rank, then pool-count rank.
fn select_connectors(
    scores: Vec<(Address, TokenScore)>,
    wrap_pair: &[Address],
    sizes: RankingSizes,
    min_component_count: usize,
    min_tvl: f64,
) -> Vec<ConnectorSelection> {
    let component_ranks = rank_by(
        &scores,
        |score| score.component_count >= min_component_count && score.backed_tvl >= min_tvl,
        |a, b| {
            b.component_count
                .cmp(&a.component_count)
        },
    );
    let tvl_ranks = rank_by(
        &scores,
        |score| score.component_count >= min_component_count && score.backed_tvl > 0.0,
        |a, b| b.backed_tvl.total_cmp(&a.backed_tvl),
    );

    let mut selected = Vec::new();
    for (i, (address, score)) in scores.into_iter().enumerate() {
        let (component_rank, tvl_rank) = (component_ranks[i], tvl_ranks[i]);
        let by_count = component_rank.is_some_and(|rank| rank <= sizes.pool_count);
        let by_tvl = tvl_rank.is_some_and(|rank| rank <= sizes.tvl);
        let selected_by = match (by_count, by_tvl) {
            (true, true) => SelectedBy::Both,
            (true, false) => SelectedBy::PoolCount,
            (false, true) => SelectedBy::Tvl,
            (false, false) if wrap_pair.contains(&address) => SelectedBy::WrapPair,
            (false, false) => continue,
        };
        selected.push(ConnectorSelection { address, score, component_rank, tvl_rank, selected_by });
    }
    selected.sort_by(|a, b| {
        a.tvl_rank
            .unwrap_or(usize::MAX)
            .cmp(&b.tvl_rank.unwrap_or(usize::MAX))
            .then_with(|| {
                a.component_rank
                    .unwrap_or(usize::MAX)
                    .cmp(&b.component_rank.unwrap_or(usize::MAX))
            })
            .then_with(|| {
                a.address
                    .as_ref()
                    .cmp(b.address.as_ref())
            })
    });
    selected
}

fn rank_label(rank: Option<usize>) -> String {
    rank.map_or_else(|| "-".to_string(), |rank| rank.to_string())
}

fn print_toml(
    selected: &[ConnectorSelection],
    chain: &str,
    tvl_unit: &str,
    sizes: RankingSizes,
    total: usize,
) {
    use chrono::Utc;
    let date = Utc::now().format("%Y-%m-%d");
    println!("# Derived connector tokens for {chain} ({date})");
    println!(
        "# Top {} by pool count + top {} by backed TVL, plus the wrap pair: {} of {total} tokens.",
        sizes.pool_count,
        sizes.tvl,
        selected.len()
    );
    println!("connector_tokens = [");
    for entry in selected {
        println!(
            "    \"0x{}\",  # {}  — {} pools (#{}), {:.0} {tvl_unit} backed TVL (#{}), by {}",
            hex::encode(entry.address.as_ref()),
            entry.score.symbol,
            entry.score.component_count,
            rank_label(entry.component_rank),
            entry.score.backed_tvl,
            rank_label(entry.tvl_rank),
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
                "backed_tvl": entry.score.backed_tvl,
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
        "{:<10} {:>6} {:>10} {:>24} {:>9}  {:<10} Address",
        "Symbol",
        "Pools",
        "Pool rank",
        format!("Backed TVL ({tvl_unit})"),
        "TVL rank",
        "Selected"
    );
    println!("{:-<100}", "");
    for entry in selected {
        println!(
            "{:<10} {:>6} {:>10} {:>24.0} {:>9}  {:<10} 0x{}",
            entry.score.symbol,
            entry.score.component_count,
            rank_label(entry.component_rank),
            entry.score.backed_tvl,
            rank_label(entry.tvl_rank),
            entry.selected_by.as_str(),
            hex::encode(entry.address.as_ref()),
        );
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn score(symbol: &str, component_count: usize, backed_tvl: f64) -> (Address, TokenScore) {
        let byte = symbol.as_bytes()[0];
        (addr(byte), TokenScore { symbol: symbol.to_string(), component_count, backed_tvl })
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

    fn pool_balances(id: &str, held: &[(u8, f64)]) -> (ComponentId, HashMap<Address, f64>) {
        (
            id.to_string(),
            held.iter()
                .map(|(b, amount)| (addr(*b), *amount))
                .collect(),
        )
    }

    const ANCHOR: u8 = 1;

    fn selection(
        scores: Vec<(Address, TokenScore)>,
        wrap_pair: &[Address],
        top_n: usize,
        min_component_count: usize,
    ) -> Vec<(String, SelectedBy)> {
        let sizes = RankingSizes { pool_count: top_n, tvl: top_n };
        select_connectors(scores, wrap_pair, sizes, min_component_count, 0.0)
            .into_iter()
            .map(|entry| (entry.score.symbol, entry.selected_by))
            .collect()
    }

    #[test]
    fn test_raw_to_whole_units() {
        assert_eq!(raw_to_whole_units(&[0x01, 0x00], 2), 2.56);
        assert_eq!(raw_to_whole_units(&[], 6), 0.0);
    }

    #[rstest]
    #[case::capped_at_tvl(&[(2, 100.0)], 50.0, 50.0)]
    #[case::capped_at_twice_trusted_value(&[(2, 10.0)], 100.0, 20.0)]
    #[case::untrusted_pair_gets_nothing(&[(3, 1e9), (4, 1e9)], 1e6, 0.0)]
    #[case::fake_side_not_counted(&[(2, 10.0), (3, 1e9)], 1e6, 20.0)]
    fn test_backed_credit(#[case] held: &[(u8, f64)], #[case] tvl: f64, #[case] expected: f64) {
        let tokens: Vec<u8> = held.iter().map(|(b, _)| *b).collect();
        let components = [component("p", &tokens)];
        let balances = Balances::from([pool_balances("p", held)]);
        let tvl = HashMap::from([("p".to_string(), tvl)]);
        let pools = PoolData { components: &components, tvl: &tvl, balances: &balances };
        let prices = HashMap::from([(addr(2), 1.0)]);

        let credit = backed_credit(&pools, &prices);

        assert_eq!(credit["p"], expected);
    }

    #[test]
    fn test_trusted_prices_use_deepest_two_token_anchor_pool() {
        let components = [
            component("shallow", &[ANCHOR, 2]),
            component("deep", &[2, ANCHOR]),
            component("triple", &[ANCHOR, 2, 3]),
            component("other", &[3, 4]),
        ];
        let balances = Balances::from([
            pool_balances("shallow", &[(ANCHOR, 1.0), (2, 100.0)]),
            pool_balances("deep", &[(ANCHOR, 10.0), (2, 50.0)]),
            pool_balances("triple", &[(ANCHOR, 99.0), (2, 1.0), (3, 1.0)]),
            pool_balances("other", &[(3, 5.0), (4, 5.0)]),
        ]);
        let tvl = HashMap::from([
            ("shallow".to_string(), 4.0),
            ("deep".to_string(), 30.0),
            ("triple".to_string(), 200.0),
        ]);
        let pools = PoolData { components: &components, tvl: &tvl, balances: &balances };
        let anchors = [addr(ANCHOR)];
        let backing = native_backing(&pools, &anchors);

        let prices = trusted_prices(&pools, &anchors, &backing, 5);

        // Token 3 only has a three-token pool, token 4 no anchor pool: neither is priced.
        assert_eq!(prices, HashMap::from([(addr(ANCHOR), 1.0), (addr(2), 0.4)]));
        assert_eq!(trusted_prices(&pools, &anchors, &backing, 0).len(), 1);
    }

    #[test]
    fn test_score_tokens_backed_tvl_excludes_largest_component() {
        let components = [component("a", &[1, 2]), component("b", &[1, 3]), component("c", &[1])];
        let credit = HashMap::from([("a".to_string(), 10.0), ("b".to_string(), 4.0)]);

        let mut scores = score_tokens(&components, &credit, &HashMap::new());
        scores.sort_by_key(|(address, _)| address.clone());

        let summary: Vec<(usize, f64)> = scores
            .iter()
            .map(|(_, s)| (s.component_count, s.backed_tvl))
            .collect();
        assert_eq!(summary, vec![(3, 4.0), (1, 0.0), (1, 0.0)]);
    }

    #[test]
    fn test_select_connectors_by_each_ranking() {
        // Deep: few pools, large backed TVL. Spam: many pools, no backed TVL.
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
    fn test_select_connectors_pool_count_blocked_below_min_tvl() {
        let scores = vec![score("Spam", 50, 0.5), score("Hub", 40, 6.0), score("Deep", 2, 900.0)];
        let sizes = RankingSizes { pool_count: 1, tvl: 1 };

        let picked = select_connectors(scores, &[], sizes, 2, 5.0);

        let by: Vec<(&str, SelectedBy)> = picked
            .iter()
            .map(|entry| (entry.score.symbol.as_str(), entry.selected_by))
            .collect();
        assert_eq!(by, vec![("Deep", SelectedBy::Tvl), ("Hub", SelectedBy::PoolCount)]);
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
    fn test_select_connectors_takes_each_ranking_size() {
        let scores = vec![
            score("Hub", 50, 1.0),
            score("Busy", 40, 2.0),
            score("Deep", 2, 900.0),
            score("Mid", 3, 500.0),
        ];
        let sizes = RankingSizes { pool_count: 1, tvl: 2 };

        let picked: Vec<(String, SelectedBy)> = select_connectors(scores, &[], sizes, 2, 0.0)
            .into_iter()
            .map(|entry| (entry.score.symbol, entry.selected_by))
            .collect();

        assert_eq!(
            picked,
            vec![
                ("Deep".to_string(), SelectedBy::Tvl),
                ("Mid".to_string(), SelectedBy::Tvl),
                ("Hub".to_string(), SelectedBy::PoolCount),
            ]
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

        let ranked = select_connectors(scores, &[], RankingSizes { pool_count: 3, tvl: 3 }, 1, 0.0);

        let order: Vec<&str> = ranked
            .iter()
            .map(|entry| entry.score.symbol.as_str())
            .collect();
        assert_eq!(order, vec!["C", "A", "B"]);
        assert_eq!(ranked[1].component_rank, Some(2));
    }
}
