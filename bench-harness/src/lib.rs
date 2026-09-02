//! Runs the routing algorithms over one market and reports what they paid.
//!
//! Two programs: [`bench::run`] solves many orders with several configurations and writes a
//! report, [`profile::run`] solves a few with one configuration under a profiler. Both replay the
//! same fixture, read the same configs and the same dataset; only what they do with the results
//! differs. Keeping the setup here stops the two drifting into measuring subtly different things.
//!
//! The configurations, the token table and the blocked list ship with this crate, in `configs/`
//! and `data/`, and are found relative to the crate rather than the working directory.

pub mod bench;
pub mod live;
pub mod profile;
pub mod trades;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use fynd_core::{derived::DerivedData, LiquidityScope, PoolConfig, Solver};
use fynd_test_fixtures::read_recording;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing_subscriber::EnvFilter;
use tycho_simulation::{
    protocol::models::Update,
    tycho_common::models::{Address, Chain},
};

use self::trades::TradeOrder;

/// Filter used by `--logs` when `RUST_LOG` is not set.
const DEFAULT_LOG_FILTER: &str = "fynd_core=debug";

/// Installs the log subscriber behind `--logs`.
///
/// `tracing` macros are dropped until a subscriber exists, so without this neither tool prints
/// anything the solver logs. `RUST_LOG` sets the filter when it is present — e.g.
/// `RUST_LOG=fynd_core::algorithm=trace` — and [`DEFAULT_LOG_FILTER`] applies otherwise.
///
/// Logging costs time in the solve it reports on. Under a profiler the formatting and the writes
/// land in the flamegraph alongside the algorithm.
pub fn init_logging(enabled: bool) {
    if !enabled {
        return;
    }
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();
}

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

/// A path inside this crate.
///
/// Resolved against the crate rather than the working directory, so a caller depending on this
/// crate from elsewhere reads the same configs, the same token table and the same blocked list.
fn crate_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// A file this crate ships in `data/`.
fn data_path(file: &str) -> PathBuf {
    crate_path("data").join(file)
}

/// The dataset a run reads when `--trades` names nothing else.
///
/// Only useful inside this repository: the file is gitignored for its size, so a checkout cargo
/// made for a git dependency does not hold it, and a caller from outside names its own copy.
pub fn default_trades_path() -> PathBuf {
    crate_path("../aggregator_trades_50k_1k_usd.json")
}

/// One solver configuration in the comparison, loaded from `configs/<label>.toml`.
pub struct BenchConfig {
    /// The config file's stem: what `--configs` takes, and what the reports show.
    pub label: String,
    /// Read back out of the file for the CSV columns.
    pub algorithm: String,
    pub max_hops: usize,
    /// The file's contents: a flat table of `PoolConfig` fields.
    pub worker_pool_fields: toml::Table,
}

/// Tokens dropped from the market before anything is solved.
///
/// Read from `data/blocked_tokens.toml`, which records why each one is there.
pub struct BlockedTokens {
    pub addresses: HashSet<Address>,
    pub symbols: Vec<String>,
    /// Components dropped because they hold one of these tokens. Filled in once the market is
    /// filtered.
    pub dropped_component_count: usize,
}

/// Deserialization shape of `data/blocked_tokens.toml`.
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
    let path = data_path("blocked_tokens.toml");
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

/// Every `protocol_system` present in the market, sorted, for validating `--exclude-protocols`.
pub fn market_protocol_names(updates: &[Update]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for update in updates {
        for component in update.new_pairs.values() {
            if seen.insert(component.protocol_system.as_str()) {
                names.push(component.protocol_system.clone());
            }
        }
    }
    names.sort();
    names
}

/// Drops every component belonging to one of `excluded`, and the states that belong to them.
///
/// Applied to the market rather than to a config, so it lands on every algorithm equally and an
/// offline fixture can be narrowed the same way a live capture can.
pub fn exclude_protocol_components(updates: &mut [Update], excluded: &HashSet<String>) -> usize {
    if excluded.is_empty() {
        return 0;
    }
    let mut dropped: HashSet<String> = HashSet::new();
    for update in updates.iter_mut() {
        update
            .new_pairs
            .retain(|id, component| {
                let keep = !excluded.contains(component.protocol_system.as_str());
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

/// Drops the requested protocols from the market, returning how many components went.
///
/// An unknown name stops the run. Excluding a protocol changes every number a run reports, so a
/// typo that quietly left it in would produce results that look valid and are not.
pub fn exclude_requested_protocols(market: &mut Market, requested: &[String]) -> usize {
    if requested.is_empty() {
        return 0;
    }
    let available = market_protocol_names(&market.updates);
    let unknown: Vec<&str> = requested
        .iter()
        .map(String::as_str)
        .filter(|name| !available.iter().any(|a| a == name))
        .collect();
    assert!(
        unknown.is_empty(),
        "--exclude-protocols: no protocol named {} in this market.\nAvailable: {}",
        unknown.join(", "),
        available.join(", ")
    );
    let excluded: HashSet<String> = requested.iter().cloned().collect();
    exclude_protocol_components(&mut market.updates, &excluded)
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
    crate_path("configs")
}

/// Loads `configs/<label>.toml`.
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

    Ok(BenchConfig { label: label.to_string(), algorithm, max_hops, worker_pool_fields })
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

/// The fixture an offline run replays.
///
/// Only useful inside this repository: the file is in Git LFS, so a checkout cargo made for a git
/// dependency holds the pointer rather than the market.
fn recording_path() -> PathBuf {
    crate_path("../fynd-core/tests/fixtures/market_recording.json.zst")
}

/// Builds the worker pools for one bench config: the file's fields, with the run-level ones forced.
///
/// `num_workers` and `task_queue_capacity` belong to the run rather than the algorithm — every
/// config has to be compared under the same concurrency. `timeout_ms` is only filled in when the
/// file is silent, so a config can deliberately ask for a different budget.
///
/// A config asking for exclusive liquidity gets two worker pools rather than one; see
/// [`build_public_twin`].
pub fn worker_pool_configs(
    config: &BenchConfig,
    workers: usize,
    timeout_ms: u64,
) -> HashMap<String, PoolConfig> {
    let mut fields = config.worker_pool_fields.clone();
    fields.insert("task_queue_capacity".to_string(), toml::Value::Integer(10_000));
    fields
        .entry("timeout_ms".to_string())
        .or_insert_with(|| toml::Value::Integer(timeout_ms as i64));

    let build = |mut fields: toml::map::Map<String, toml::Value>, workers: usize| -> PoolConfig {
        fields.insert("num_workers".to_string(), toml::Value::Integer(workers as i64));
        toml::Value::Table(fields)
            .try_into()
            .unwrap_or_else(|error| panic!("config {} is not a valid pool: {error}", config.label))
    };

    match build_public_twin(&fields) {
        // The twin splits the run's worker budget rather than adding to it, so a config carrying
        // one is compared on the same threads as every other. An odd budget gives the exclusive
        // pool the extra worker, and one worker each is the floor. `--workers 1` therefore runs
        // two threads for a config with a twin: a pool with no worker serves nothing.
        Some(public) => {
            let public_workers = (workers / 2).max(1);
            HashMap::from([
                ("bench".to_string(), build(fields, (workers - public_workers).max(1))),
                ("bench_public".to_string(), build(public, public_workers)),
            ])
        }
        None => HashMap::from([("bench".to_string(), build(fields, workers))]),
    }
}

/// The `worker_pools.toml` key that carries a pool's [`LiquidityScope`].
const LIQUIDITY_SCOPE: &str = "liquidity_scope";

/// The public worker pool that has to run alongside a config asking for exclusive liquidity, or
/// `None` for every other config.
///
/// Two reasons it exists. A solver whose every pool includes exclusive liquidity fails to build:
/// exclusive pools only serve requests granted access, so there would be no pool left to serve
/// anyone else, and none to establish the public output the exclusive candidate has to beat. And
/// the twin is what makes the run's surplus figure readable — it runs the same algorithm with the
/// same bounds, so the only thing separating the two pools is the liquidity they may route
/// through, and any extra output is the exclusive components' doing rather than the algorithm's.
///
/// The two pools split the run's worker budget between them, so the config's solve times stay
/// comparable with the configs that build one pool.
fn build_public_twin(fields: &toml::Table) -> Option<toml::Table> {
    // Read as the enum rather than compared as a string, so renaming a scope in fynd-core is a
    // compile error here rather than a config that silently builds one pool.
    let scope: LiquidityScope = fields
        .get(LIQUIDITY_SCOPE)?
        .clone()
        .try_into()
        .unwrap_or_else(|error| panic!("{LIQUIDITY_SCOPE} is not a liquidity scope: {error}"));
    if scope != LiquidityScope::IncludeExclusive {
        return None;
    }
    let mut public = fields.clone();
    // Absent is public: `PoolConfig::liquidity_scope` defaults to `PublicOnly`.
    public.remove(LIQUIDITY_SCOPE);
    Some(public)
}

/// Whether the process was started by a test runner asking what tests this target holds.
///
/// A test runner builds every target and asks each one to list its tests. Both benchmarks parse
/// their own options and hold no tests, so they answer with an empty list rather than failing on an
/// argument `clap` does not know.
pub fn asked_for_the_test_list() -> bool {
    std::env::args().any(|argument| argument == "--list")
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
pub async fn build_market(flags: LiveFlags) -> Result<Market, String> {
    match flags.market {
        MarketMode::Offline => Ok(load_market()),
        MarketMode::Live => live::capture_market(&flags.into_options()?).await,
    }
}

/// The market flags, parsed once and flattened into both binaries.
///
/// One declaration rather than two: the benchmark and the profiler have to agree on what a live
/// capture means, and two copies of a dozen `clap` attributes drift the first time one is edited.
#[derive(clap::Args, Debug, Clone)]
pub struct LiveFlags {
    /// Where the market comes from: the recorded fixture, or one block captured live from Tycho.
    ///
    /// Offline runs are reproducible and comparable with each other. A live run is a point-in-time
    /// market: its configs compare with each other, not with any other run.
    #[arg(long, value_enum, default_value_t = MarketMode::Offline)]
    pub market: MarketMode,

    /// Tycho host, with or without a scheme. Live runs only.
    #[arg(long, env = "TYCHO_URL")]
    pub tycho_url: Option<String>,

    /// Tycho API key. Live runs only.
    #[arg(long, env = "TYCHO_API_KEY")]
    pub tycho_api_key: Option<String>,

    /// Chain to capture. Live runs only.
    #[arg(long, default_value = "ethereum")]
    pub chain: String,

    /// Protocol systems to stream, comma separated. Defaults to every one Tycho has for the chain,
    /// including those it serves through the Dynamic Contract Indexer.
    #[arg(long, value_delimiter = ',')]
    pub protocols: Option<Vec<String>>,

    /// Protocol systems to add to the streamed list, comma separated. This is how a source Tycho
    /// does not list gets into a capture, e.g. `--include-protocols pricelevelstream:fermiswap`
    /// for a pAMM served from Titan's price level stream.
    ///
    /// A name that is already streamed, or that brings no component into the captured market,
    /// stops the run. Live runs only.
    #[arg(long, value_delimiter = ',')]
    pub include_protocols: Vec<String>,

    /// Minimum component TVL in ETH. The main lever on how big the captured market is.
    #[arg(long, default_value_t = 10.0)]
    pub min_tvl: f64,

    /// Minimum token quality score.
    #[arg(long, default_value_t = 100)]
    pub min_token_quality: i32,

    /// Only include tokens traded within this many days.
    #[arg(long, default_value_t = 3)]
    pub traded_n_days_ago: u64,

    /// How long to wait for Tycho's snapshot before giving up, in seconds.
    #[arg(long, default_value_t = 120)]
    pub capture_timeout_secs: u64,

    /// Chain RPC, read for the live gas price. Live runs only.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,
}

impl LiveFlags {
    /// Checks the flags a live capture cannot do without, and names the missing one.
    fn into_options(self) -> Result<live::LiveOptions, String> {
        let tycho_url = self
            .tycho_url
            .ok_or("--market live needs --tycho-url (or TYCHO_URL)")?;
        let tycho_api_key = self
            .tycho_api_key
            .ok_or("--market live needs --tycho-api-key (or TYCHO_API_KEY)")?;
        let chain = fynd_core::types::parse_chain(&self.chain)
            .map_err(|e| format!("unsupported chain `{}`: {e}", self.chain))?;

        Ok(live::LiveOptions {
            tycho_host: live::normalize_host(&tycho_url).to_string(),
            tycho_api_key,
            chain,
            chain_name: self.chain.to_ascii_lowercase(),
            protocols: self.protocols,
            include_protocols: self.include_protocols,
            min_tvl: self.min_tvl,
            min_token_quality: self.min_token_quality,
            traded_n_days_ago: self.traded_n_days_ago,
            capture_timeout_secs: self.capture_timeout_secs,
            rpc_url: self.rpc_url,
        })
    }
}

/// Where a run's market came from.
///
/// Carried on the [`Market`] itself so a report cannot be written without saying which it was: an
/// offline run is reproducible and comparable to every other offline run, a live one is a
/// point-in-time market that only compares within itself.
///
/// Serialized straight into `run.json`, which is what the viewer's run picker filters on.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MarketSource {
    /// Replayed from the recorded fixture.
    Offline {
        /// When the fixture was recorded, as unix seconds, so a report can be tied to the fixture
        /// that produced it.
        recorded_at_secs: u64,
        /// Chain the fixture was recorded on.
        chain_name: String,
    },
    /// Captured from Tycho at one block.
    Live {
        /// Chain the capture was taken from.
        chain_name: String,
        /// Block the snapshot sits at. Everything in the run was solved against this one block.
        block: u64,
        /// Components the snapshot carried.
        components: usize,
        /// Of those, the ones with a simulation state. A component without one cannot be routed
        /// through, so the two coming apart is worth seeing.
        states: usize,
        /// Protocol systems streamed, after discovery.
        protocols: Vec<String>,
        /// Minimum component TVL the capture filtered on, in ETH.
        min_tvl: f64,
    },
}

/// The market every config in a run solves against, held once and cloned per solver.
///
/// Built either by replaying the fixture or by capturing a live block; from here on the two are
/// the same thing, which is what keeps the two paths honest about measuring the same way.
pub struct Market {
    pub chain: Chain,
    /// The market's own gas price: recorded with the fixture, or read from the chain on a live
    /// capture. Used only when `--gas-price-gwei` is absent, and reported either way. `None` when
    /// the fixture carried none, or no `--rpc-url` was given.
    pub market_gas_price: Option<BigUint>,
    /// The node the market was captured through, so a solver built on it can read the
    /// PropAMMRouter's fee tiers. `None` for a fixture, which holds no pAMM component.
    pub rpc_url: Option<String>,
    pub updates: Vec<Update>,
    pub source: MarketSource,
}

pub fn load_market() -> Market {
    let recording = read_recording(&recording_path()).expect("market recording fixture");
    Market {
        chain: fynd_core::types::parse_chain(&recording.metadata.chain)
            .expect("fixture chain supported"),
        market_gas_price: recording
            .metadata
            .gas_price_as_biguint(),
        rpc_url: None,
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
    match &market.market_gas_price {
        Some(wei) => wei
            .to_f64()
            .map(|wei| wei / 1e9)
            .filter(|gwei| gwei.is_finite() && *gwei > 0.0)
            .unwrap_or(DEFAULT_GAS_PRICE_GWEI),
        None => DEFAULT_GAS_PRICE_GWEI,
    }
}

/// Prints one line of a run header: a label in a fixed column, then its value.
///
/// The width lives here rather than in each caller's format string, which is what kept the columns
/// lining up only by luck across four files.
pub fn header_line(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<18} {value}");
}

/// One protocol's share of the market, as the graph will see it.
pub struct ProtocolCount {
    pub protocol: String,
    /// Components the graph will hold.
    pub components: usize,
    /// Of those, the ones carrying a simulation state.
    pub with_state: usize,
    /// The components swappable only with off-chain authorization, sorted. Only a worker pool with
    /// `liquidity_scope = "include_exclusive"` may route through them, and they only reach the
    /// market at all when the protocol was streamed with the `exclusive:` prefix — so an empty
    /// list here is why an exclusive config scores exactly like its public twin.
    ///
    /// Printed because nothing downstream marks a route as having used exclusive liquidity: the
    /// surplus a config wins over its public twin is internal to the router. These are the ids to
    /// look for in `routes.jsonl`'s `component_id` fields to find the solutions that routed
    /// through one.
    pub exclusive_ids: Vec<String>,
}

/// Pools per protocol, and how many of them can actually be simulated.
///
/// Both numbers, because they come apart: a component with no state is in the graph and routable
/// on paper, but every attempt to swap through it fails, so it is dead liquidity. A total hides
/// that. The recorded fixture is missing every VM-backed state, which this makes visible on the
/// first screen of a run rather than after a puzzling report.
pub fn protocol_breakdown(updates: &[Update]) -> Vec<ProtocolCount> {
    let mut protocol_of: HashMap<&str, &str> = HashMap::new();
    let mut has_state: HashSet<&str> = HashSet::new();
    let mut is_exclusive: HashSet<&str> = HashSet::new();

    for update in updates {
        for (id, component) in &update.new_pairs {
            protocol_of.insert(id.as_str(), component.protocol_system.as_str());
            // The same attribute `feed::exclusivity::is_exclusive` classifies on, read here
            // straight off the component so the count cannot drift from what the workers see.
            if component
                .static_attributes
                .contains_key("is_exclusive")
            {
                is_exclusive.insert(id.as_str());
            }
        }
        for id in update.states.keys() {
            has_state.insert(id.as_str());
        }
        for id in update.removed_pairs.keys() {
            protocol_of.remove(id.as_str());
        }
    }

    let mut totals: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut exclusive_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, protocol) in &protocol_of {
        let entry = totals.entry(protocol).or_insert((0, 0));
        entry.0 += 1;
        if has_state.contains(id) {
            entry.1 += 1;
        }
        if is_exclusive.contains(id) {
            exclusive_of
                .entry(protocol)
                .or_default()
                .push(id);
        }
    }

    let mut counts: Vec<ProtocolCount> = totals
        .into_iter()
        .map(|(protocol, (components, with_state))| {
            let mut exclusive_ids: Vec<String> = exclusive_of
                .remove(protocol)
                .unwrap_or_default()
                .into_iter()
                .map(str::to_string)
                .collect();
            exclusive_ids.sort();
            ProtocolCount { protocol: protocol.to_string(), components, with_state, exclusive_ids }
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

/// Prints a breakdown already counted, so the market is walked once however many places read it.
pub fn print_protocol_breakdown(counts: &[ProtocolCount]) {
    let (components, with_state) = counts
        .iter()
        .fold((0, 0), |(c, s), row| (c + row.components, s + row.with_state));

    let exclusive: usize = counts
        .iter()
        .map(|row| row.exclusive_ids.len())
        .sum();

    println!();
    header_line(
        "market graph",
        format!("{components} pools, {with_state} simulatable, {exclusive} exclusive"),
    );
    println!("    {:<24}{:>10}{:>12}{:>12}", "protocol", "pools", "simulatable", "exclusive");
    for row in counts {
        // The zero is the whole point of the column, so it is called out on the row it happens on
        // rather than summarised again underneath.
        let flag = if row.with_state == 0 { "  <- none usable, cannot be routed" } else { "" };
        println!(
            "    {:<24}{:>10}{:>12}{:>12}{}",
            row.protocol,
            row.components,
            row.with_state,
            row.exclusive_ids.len(),
            flag
        );
    }

    // Nothing downstream marks a solution as having used exclusive liquidity, so the ids are the
    // only way to find those routes: grep them in `routes.jsonl`'s `component_id` fields.
    for row in counts
        .iter()
        .filter(|row| !row.exclusive_ids.is_empty())
    {
        header_line("exclusive in", format!("{}: {}", row.protocol, row.exclusive_ids.join(", ")));
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
    // The two halves of a slow start: replaying the market into a solver, then waiting for the
    // derived data every worker needs before it may answer. They are timed apart because the
    // second is the one that grows with the size of the market.
    let replaying = Instant::now();
    let solver = Solver::from_recording(
        market.chain,
        market.updates.clone(),
        worker_pool_configs(config, workers, timeout_ms),
        Some(gas_price_wei(gas_price_gwei)),
        market.rpc_url.as_deref(),
    )
    .await
    .map_err(|error| error.to_string())?;
    header_line("replayed in", format!("{:.1}s", replaying.elapsed().as_secs_f64()));

    let readying = Instant::now();
    solver
        .wait_until_ready(READY_TIMEOUT)
        .await
        .map_err(|error| format!("solver never became ready: {error}"))?;
    header_line("derived data in", format!("{:.1}s", readying.elapsed().as_secs_f64()));
    Ok(solver)
}

/// Token symbols, so the per-pair table reads as `WETH->USDC` rather than two hex strings.
///
/// `data/tokens.json` covers every token the market fixture knows about — pulled from Dune's
/// `tokens.erc20` — which is a superset of anything the dataset can name. Anything absent falls
/// back to a short address rather than failing the run.
pub fn symbol_table() -> HashMap<Address, String> {
    let path = data_path("tokens.json");
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
