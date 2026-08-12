//! Pieces shared by the benchmark and the profiler.
//!
//! Both replay the same fixture, read the same configs and the same dataset; only what they do
//! with the results differs. Keeping the setup here stops the two drifting into measuring subtly
//! different things.
//!
//! Cargo compiles this once per bench target and each uses a subset, so the few items only one of
//! them reads carry their own `allow(dead_code)`.

pub mod live;
pub mod trades;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use fynd_core::{derived::DerivedData, PoolConfig, Solver};
use fynd_test_fixtures::read_recording;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_simulation::{
    protocol::models::Update,
    tycho_common::models::{Address, Chain},
};

use self::trades::TradeOrder;

/// Gas price both tools solve at unless `--gas-price-gwei` says otherwise.
///
/// One value for both, so an order picked out of the benchmark's report profiles the same solve.
pub const DEFAULT_GAS_PRICE_GWEI: f64 = 1.0;

/// Parses a `--gas-price-gwei` value.
///
/// Fractional prices are allowed because whole gwei is too coarse for the market this replays:
/// the fixture's own block sat near 0.1 gwei, and rounding that up to 1 charges routes ten times
/// the gas they really cost, which shifts every split decision.
///
/// # Errors
///
/// Returns a message for anything that is not a finite, non-negative number.
pub fn parse_gas_price_gwei(raw: &str) -> Result<f64, String> {
    let gwei: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if !gwei.is_finite() || gwei < 0.0 {
        return Err(format!("gas price must be finite and not negative, got `{raw}`"));
    }
    Ok(gwei)
}

/// `gwei` as wei, rounded to the nearest wei.
fn gas_price_wei(gwei: f64) -> BigUint {
    BigUint::from((gwei * 1e9).round() as u128)
}

/// How long a freshly built solver is given to ingest the recording and compute derived data.
const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Dataset the benchmark and the profiler both read.
pub fn default_trades_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("aggregator_trades_50k_1k_usd.json")
}

/// One solver configuration in the comparison, loaded from `benches/configs/<label>.toml`.
pub struct BenchConfig {
    /// The config file's stem: what `--configs` takes, and what the reports show.
    pub label: String,
    /// Read back out of the file for the CSV columns.
    pub algorithm: String,
    pub max_hops: usize,
    /// The file's contents: a flat table of `PoolConfig` fields.
    pub pool_fields: toml::Table,
}

/// Tokens dropped from the market before anything is solved.
///
/// Read from `benches/blocked_tokens.toml`, which records why each one is there.
pub struct BlockedTokens {
    pub addresses: HashSet<Address>,
    pub symbols: Vec<String>,
    /// Components dropped because they hold one of these tokens. Filled in once the market is
    /// filtered.
    #[allow(dead_code, reason = "the profiler prints `block_components` directly")]
    pub dropped_component_count: usize,
}

/// Deserialization shape of `benches/blocked_tokens.toml`.
#[derive(serde::Deserialize)]
pub struct BlockedTokensFile {
    #[serde(default)]
    tokens: Vec<BlockedToken>,
}

#[derive(serde::Deserialize)]
pub struct BlockedToken {
    address: String,
    symbol: String,
}

pub fn load_blocked_tokens() -> BlockedTokens {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/blocked_tokens.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return BlockedTokens {
            addresses: HashSet::new(),
            symbols: Vec::new(),
            dropped_component_count: 0,
        };
    };
    let parsed: BlockedTokensFile = toml::from_str(&contents)
        .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()));

    let mut addresses = HashSet::new();
    let mut symbols = Vec::new();
    for token in parsed.tokens {
        match token.address.parse::<Address>() {
            Ok(address) => {
                addresses.insert(address);
                symbols.push(token.symbol);
            }
            Err(error) => panic!("{}: bad address {}: {error}", path.display(), token.address),
        }
    }
    BlockedTokens { addresses, symbols, dropped_component_count: 0 }
}

/// Drops every component holding a blocked token, and the states that belong to them.
///
/// Done to the recording rather than through `connector_tokens` so it lands on every algorithm
/// equally: `path_frank_wolfe` never reads `connector_tokens`, so a routing-level filter would
/// quietly leave the bad pools available to it alone.
pub fn block_components(updates: &mut [Update], blocked: &HashSet<Address>) -> usize {
    if blocked.is_empty() {
        return 0;
    }
    let mut dropped: HashSet<String> = HashSet::new();
    for update in updates.iter_mut() {
        update
            .new_pairs
            .retain(|id, component| {
                let keep = !component
                    .tokens
                    .iter()
                    .any(|token| blocked.contains(&token.address));
                if !keep {
                    dropped.insert(id.clone());
                }
                keep
            });
    }
    // A state whose component is gone would otherwise linger as an orphan the graph cannot place.
    for update in updates.iter_mut() {
        update
            .states
            .retain(|id, _| !dropped.contains(id));
        update
            .removed_pairs
            .retain(|id, _| !dropped.contains(id));
    }
    dropped.len()
}

/// Directory holding one TOML file per available configuration.
pub fn configs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/configs")
}

/// Loads `benches/configs/<label>.toml`.
///
/// # Errors
///
/// Returns the reason the config could not be used — missing, unparseable, or without an
/// `algorithm` key — so the caller can print it and record it in the report rather than inventing
/// its own wording.
pub fn load_bench_config(label: &str) -> Result<BenchConfig, String> {
    let path = configs_dir().join(format!("{label}.toml"));
    let contents = std::fs::read_to_string(&path)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    let worker_pool_fields: toml::Table = toml::from_str(&contents)
        .map_err(|error| format!("{} does not parse: {error}", path.display()))?;

    let algorithm = worker_pool_fields
        .get("algorithm")
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("{} has no `algorithm` key", path.display()))?
        .to_string();
    let max_hops = worker_pool_fields
        .get("max_hops")
        .and_then(|value| value.as_integer())
        .unwrap_or(3) as usize;

    Ok(BenchConfig {
        label: label.to_string(),
        algorithm,
        max_hops,
        pool_fields: worker_pool_fields,
    })
}

/// Every configuration on disk, by file stem, sorted. The default when `--configs` is absent.
pub fn available_configs() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(configs_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "toml")
        })
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

fn recording_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/market_recording.json.zst")
}

/// Builds the pool config for one bench config: the file's fields, with the run-level ones forced.
///
/// `num_workers` and `task_queue_capacity` belong to the run rather than the algorithm — every
/// config has to be compared under the same concurrency. `timeout_ms` is only filled in when the
/// file is silent, so a config can deliberately ask for a different budget.
pub fn pool_configs(
    config: &BenchConfig,
    workers: usize,
    timeout_ms: u64,
) -> HashMap<String, PoolConfig> {
    let mut fields = config.pool_fields.clone();
    fields.insert("num_workers".to_string(), toml::Value::Integer(workers as i64));
    fields.insert("task_queue_capacity".to_string(), toml::Value::Integer(10_000));
    fields
        .entry("timeout_ms".to_string())
        .or_insert_with(|| toml::Value::Integer(timeout_ms as i64));

    let pool: PoolConfig = toml::Value::Table(fields)
        .try_into()
        .unwrap_or_else(|error| panic!("config {} is not a valid pool: {error}", config.label));
    HashMap::from([("bench".to_string(), pool)])
}

/// Which market a run solves against, as asked for on the command line.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum MarketMode {
    /// The recorded fixture. Reproducible, and comparable with every other offline run.
    Offline,
    /// One block captured live from Tycho.
    Live,
}

/// Builds the market a run solves against.
///
/// The two paths meet here and nowhere else: everything downstream takes a [`Market`] and cannot
/// tell which it was handed, which is what keeps the offline and live runs measuring the same way.
///
/// # Errors
///
/// Returns a message when a live capture cannot be made. The offline path panics instead -- a
/// missing fixture is a broken checkout, not a run-time condition.
pub async fn build_market(mode: MarketMode, live: LiveArgs<'_>) -> Result<Market, String> {
    match mode {
        MarketMode::Offline => Ok(load_market()),
        MarketMode::Live => live::capture_market(&live.into_options()?).await,
    }
}

/// The live flags, as both bench binaries parse them.
pub struct LiveArgs<'a> {
    pub tycho_url: Option<&'a str>,
    pub tycho_api_key: Option<&'a str>,
    pub chain: &'a str,
    pub protocols: Option<Vec<String>>,
    pub min_tvl: f64,
    pub min_token_quality: i32,
    pub traded_n_days_ago: u64,
    pub capture_timeout_secs: u64,
    pub rpc_url: Option<&'a str>,
}

impl LiveArgs<'_> {
    /// Checks the flags a live capture cannot do without, and names the missing one.
    fn into_options(self) -> Result<live::LiveOptions, String> {
        let tycho_url = self
            .tycho_url
            .ok_or("--market live needs --tycho-url (or TYCHO_URL)")?;
        let tycho_api_key = self
            .tycho_api_key
            .ok_or("--market live needs --tycho-api-key (or TYCHO_API_KEY)")?;
        let chain = fynd_core::types::parse_chain(self.chain)
            .map_err(|e| format!("unsupported chain `{}`: {e}", self.chain))?;

        Ok(live::LiveOptions {
            tycho_url: live::normalize_host(tycho_url).to_string(),
            tycho_api_key: tycho_api_key.to_string(),
            chain,
            chain_name: self.chain.to_ascii_lowercase(),
            protocols: self.protocols,
            min_tvl: self.min_tvl,
            min_token_quality: self.min_token_quality,
            traded_n_days_ago: self.traded_n_days_ago,
            capture_timeout_secs: self.capture_timeout_secs,
            rpc_url: self.rpc_url.map(str::to_string),
        })
    }
}

/// Where a run's market came from.
///
/// Carried on the [`Market`] itself so a report cannot be written without saying which it was: an
/// offline run is reproducible and comparable to every other offline run, a live one is a
/// point-in-time market that only compares within itself.
#[derive(Clone, Debug)]
pub enum MarketSource {
    /// Replayed from the recorded fixture.
    Offline {
        /// When the fixture was recorded, as unix seconds. Written to `run.json` so a report can
        /// be tied to the fixture that produced it.
        #[allow(dead_code, reason = "only the benchmark writes it; the profiler reads neither")]
        recorded_at_secs: u64,
        /// Chain the fixture was recorded on.
        chain_name: String,
    },
    /// Captured from Tycho at one block.
    Live {
        chain_name: String,
        block: u64,
        components: usize,
        states: usize,
        #[allow(dead_code, reason = "only the benchmark writes it")]
        protocols: Vec<String>,
        #[allow(dead_code, reason = "only the benchmark writes it")]
        min_tvl: f64,
    },
}

impl MarketSource {
    /// `"live"` or `"offline"`, for the report and the viewer's run picker.
    #[allow(dead_code, reason = "only the benchmark writes run.json")]
    pub fn label(&self) -> &'static str {
        match self {
            MarketSource::Offline { .. } => "offline",
            MarketSource::Live { .. } => "live",
        }
    }

    /// The chain the market is on.
    #[allow(dead_code, reason = "read by neither binary today; kept beside `block`")]
    pub fn chain_name(&self) -> &str {
        match self {
            MarketSource::Offline { chain_name, .. } | MarketSource::Live { chain_name, .. } => {
                chain_name
            }
        }
    }

    /// The block the market sits at, when that is known. The fixture does not record one.
    #[allow(dead_code, reason = "the report reads the variant's fields directly")]
    pub fn block(&self) -> Option<u64> {
        match self {
            MarketSource::Offline { .. } => None,
            MarketSource::Live { block, .. } => Some(*block),
        }
    }
}

/// The market every config in a run solves against, held once and cloned per solver.
///
/// Built either by replaying the fixture or by capturing a live block; from here on the two are
/// the same thing, which is what keeps the two paths honest about measuring the same way.
pub struct Market {
    pub chain: Chain,
    /// The market's own gas price: recorded with the fixture, or read from the chain on a live
    /// capture. Used only when `--gas-price-gwei` is absent, and reported either way.
    pub recorded_gas_price: Option<BigUint>,
    pub updates: Vec<Update>,
    pub source: MarketSource,
}

pub fn load_market() -> Market {
    let recording = read_recording(&recording_path()).expect("market recording fixture");
    Market {
        chain: fynd_core::types::parse_chain(&recording.metadata.chain)
            .expect("fixture chain supported"),
        recorded_gas_price: recording
            .metadata
            .gas_price_as_biguint(),
        source: MarketSource::Offline {
            recorded_at_secs: recording.metadata.recorded_at_secs,
            chain_name: recording.metadata.chain.clone(),
        },
        updates: recording.updates,
    }
}

/// The gas price a run solves at, in gwei: the flag if given, else the market's own, else the
/// default.
///
/// A live run without `--gas-price-gwei` prices at whatever the chain is charging, which is the
/// point of running live. An offline run falls back to the default, because the fixture carries no
/// gas price.
pub fn resolved_gas_price_gwei(flag: Option<f64>, market: &Market) -> f64 {
    if let Some(gwei) = flag {
        return gwei;
    }
    match &market.recorded_gas_price {
        Some(wei) => wei
            .to_f64()
            .map(|wei| wei / 1e9)
            .filter(|gwei| gwei.is_finite() && *gwei > 0.0)
            .unwrap_or(DEFAULT_GAS_PRICE_GWEI),
        None => DEFAULT_GAS_PRICE_GWEI,
    }
}

/// One protocol's share of the market, as the graph will see it.
pub struct ProtocolCount {
    pub protocol: String,
    /// Components the graph will hold.
    pub components: usize,
    /// Of those, the ones carrying a simulation state.
    pub with_state: usize,
}

/// Pools per protocol, and how many of them can actually be simulated.
///
/// Both numbers, because they come apart: a component with no state is in the graph and routable
/// on paper, but every attempt to swap through it fails, so it is dead liquidity. A total hides
/// that. The recorded fixture is missing every VM-backed state, which this makes visible on the
/// first screen of a run rather than after a puzzling report.
pub fn protocol_breakdown(updates: &[Update]) -> Vec<ProtocolCount> {
    let mut protocol_of: HashMap<&str, &str> = HashMap::new();
    let mut stated: HashSet<&str> = HashSet::new();

    for update in updates {
        for (id, component) in &update.new_pairs {
            protocol_of.insert(id.as_str(), component.protocol_system.as_str());
        }
        for id in update.states.keys() {
            stated.insert(id.as_str());
        }
        for id in update.removed_pairs.keys() {
            protocol_of.remove(id.as_str());
        }
    }

    let mut totals: HashMap<&str, (usize, usize)> = HashMap::new();
    for (id, protocol) in &protocol_of {
        let entry = totals.entry(protocol).or_insert((0, 0));
        entry.0 += 1;
        if stated.contains(id) {
            entry.1 += 1;
        }
    }

    let mut counts: Vec<ProtocolCount> = totals
        .into_iter()
        .map(|(protocol, (components, with_state))| ProtocolCount {
            protocol: protocol.to_string(),
            components,
            with_state,
        })
        .collect();
    // Biggest first, then by name so equal sizes do not reorder between runs.
    counts.sort_by(|a, b| {
        b.components
            .cmp(&a.components)
            .then_with(|| a.protocol.cmp(&b.protocol))
    });
    counts
}

/// Prints the breakdown, and says plainly when a protocol has no simulatable pool at all.
pub fn print_protocol_breakdown(updates: &[Update]) {
    let counts = protocol_breakdown(updates);
    let (components, with_state) = counts
        .iter()
        .fold((0, 0), |(c, s), row| (c + row.components, s + row.with_state));

    println!("\n  market graph       {components} pools, {with_state} simulatable");
    println!("    {:<24}{:>10}{:>12}", "protocol", "pools", "simulatable");
    for row in &counts {
        let flag = if row.with_state == 0 { "  <- none usable" } else { "" };
        println!("    {:<24}{:>10}{:>12}{}", row.protocol, row.components, row.with_state, flag);
    }

    let dead: Vec<&str> = counts
        .iter()
        .filter(|row| row.with_state == 0)
        .map(|row| row.protocol.as_str())
        .collect();
    if !dead.is_empty() {
        println!(
            "    note: no pool of {} carries a state, so nothing routes through them",
            dead.join(", ")
        );
    }
}

/// Builds a solver for one config and waits until it can answer.
///
/// # Errors
///
/// Returns the build failure verbatim. Whether that ends the run is the caller's call: the
/// benchmark records it and carries on with the remaining configs, the profiler has nothing left
/// to do.
pub async fn build_solver(
    config: &BenchConfig,
    market: &Market,
    workers: usize,
    timeout_ms: u64,
    gas_price_gwei: f64,
) -> Result<Solver, String> {
    let solver = Solver::from_recording(
        market.chain,
        market.updates.clone(),
        pool_configs(config, workers, timeout_ms),
        Some(gas_price_wei(gas_price_gwei)),
    )
    .await
    .map_err(|error| error.to_string())?;

    solver
        .wait_until_ready(READY_TIMEOUT)
        .await
        .map_err(|error| format!("solver never became ready: {error}"))?;
    Ok(solver)
}

/// Token symbols, so the per-pair table reads as `WETH->USDC` rather than two hex strings.
///
/// `benches/tokens.json` covers every token the market fixture knows about — pulled from Dune's
/// `tokens.erc20` — which is a superset of anything the dataset can name. Anything absent falls
/// back to a short address rather than failing the run.
pub fn symbol_table() -> HashMap<Address, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/tokens.json");
    let Ok(json) = std::fs::read_to_string(&path) else {
        println!("  no {} — pairs will show as addresses", path.display());
        return HashMap::new();
    };
    // `[symbol, decimals]` per address; the decimals are for the viewer, which reads this file
    // itself, so only the symbol is kept here
    let parsed: HashMap<String, (String, u32)> = serde_json::from_str(&json)
        .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()));

    let mut symbols = HashMap::new();
    for (address, (symbol, _decimals)) in parsed {
        let Ok(address) = address.parse::<Address>() else { continue };
        symbols.insert(address, symbol);
    }
    symbols
}

/// How a token reads in a report: its symbol, or the first 8 hex characters of its address.
pub fn token_label(address: &Address, symbols: &HashMap<Address, String>) -> String {
    symbols
        .get(address)
        .cloned()
        .unwrap_or_else(|| {
            address
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect()
        })
}

/// The solve-time distribution, so a caller cannot read a percentile off unsorted input.
pub struct Timings {
    pub p50_us: u128,
    pub p95_us: u128,
    pub slowest_us: u128,
}

/// Sorts `times_us` in place and reads the distribution off it.
pub fn timings_of(times_us: &mut [u128]) -> Timings {
    times_us.sort_unstable();
    let at = |fraction: f64| match times_us.len() {
        0 => 0,
        length => {
            let index = ((length - 1) as f64 * fraction).round() as usize;
            times_us[index.min(length - 1)]
        }
    };
    Timings { p50_us: at(0.5), p95_us: at(0.95), slowest_us: times_us.last().copied().unwrap_or(0) }
}

/// Mean and median of `values`, sorting in place — the two figures every bps table wants together.
#[allow(dead_code, reason = "the profiler reports timings only")]
pub fn mean_and_median(values: &mut [f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let middle = values.len() / 2;
    let median = if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    };
    (mean, median)
}

/// A microsecond count in whichever unit reads better.
#[allow(dead_code, reason = "the benchmark writes microseconds into the CSV instead")]
pub fn format_micros(us: u128) -> String {
    if us >= 1000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{us} us")
    }
}

/// What one atomic unit of a token is worth in wei, from the market's own derived prices.
///
/// `TokenGasPrices` is what the algorithms use to charge gas: it converts wei into a token's atomic
/// units, so inverting it values the token in wei. Taken from the solved market rather than from an
/// outside price feed, so the figure agrees with what the solver was optimising.
#[allow(dead_code, reason = "the profiler does not value routes")]
pub fn wei_per_token(store: &DerivedData) -> HashMap<Address, f64> {
    let Some(prices) = store.token_prices() else {
        return HashMap::new();
    };
    prices
        .iter()
        .filter_map(|(token, price)| {
            let numerator = price.numerator.to_f64()?;
            let denominator = price.denominator.to_f64()?;
            if numerator == 0.0 {
                return None;
            }
            Some((token.clone(), denominator / numerator))
        })
        .collect()
}

/// The order's output valued in the same currency as its input.
///
/// Both sides are converted to wei through the market's prices, and the dataset's own USD figure
/// supplies the scale — so the ratio comes from the market being solved and only the absolute
/// number comes from outside. `None` when either token has no derived price.
#[allow(dead_code, reason = "the profiler does not value routes")]
pub fn usd_out(
    order: &TradeOrder,
    amount_out: &BigUint,
    wei: &HashMap<Address, f64>,
) -> Option<f64> {
    let usd_in = order.amount_usd?;
    let wei_in = order.amount_in.to_f64()? * wei.get(&order.token_in)?;
    let wei_out = amount_out.to_f64()? * wei.get(&order.token_out)?;
    if wei_in == 0.0 {
        return None;
    }
    Some(usd_in * wei_out / wei_in)
}
