//! Captures one block of live Tycho market data, in memory.
//!
//! The offline path reads a recorded fixture; this one connects to Tycho, takes the first message
//! and stops. That message is the snapshot: every component and every state the filters admit, at
//! one block. Nothing after it is needed -- later messages are per-block deltas, and derived data
//! (spot prices, depths, token gas prices) is computed locally from whatever state is present, not
//! streamed.
//!
//! Nothing is serialized, which is the point. `MarketRecording` drops states it cannot write --
//! every Uniswap v4, Balancer, Curve and Maverick pool in the recorded fixture is a component with
//! no state, and so unroutable. Captured live they are all there.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use fynd_core::feed::protocol_registry::{
    open_price_level_stream_for_recording, register_exchanges_for_recording,
};
use num_bigint::BigUint;
use tokio_stream::StreamExt;
use tycho_simulation::{
    evm::stream::ProtocolStreamBuilder,
    protocol::models::Update,
    tycho_client::{
        feed::component_tracker::ComponentFilter,
        rpc::{HttpRPCClient, HttpRPCClientOptions, ProtocolSystemsParams, RPCClient},
    },
    tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    },
    tycho_core::traits::FeePriceGetter,
    tycho_ethereum::rpc::EthereumRpcClient,
    utils::load_all_tokens,
};

use super::{header_line, Market, MarketSource};

/// Times each step of the capture and prints what it cost.
///
/// A live start spends minutes between two lines of output and the lines alone do not say which
/// step is the slow one. Every step is timed rather than the ones currently suspected, so the
/// answer does not depend on having guessed right.
struct Steps {
    started: Instant,
    step: Instant,
}

impl Steps {
    fn new() -> Self {
        let now = Instant::now();
        Self { started: now, step: now }
    }

    /// Prints how long the step just finished took, and starts timing the next one.
    fn done(&mut self, label: &str) {
        header_line(label, format!("{:.1}s", self.step.elapsed().as_secs_f64()));
        self.step = Instant::now();
    }

    /// Seconds since the capture began.
    fn total(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// Strips a scheme and any trailing slash from a Tycho host.
///
/// Everything downstream — the RPC call here, the stream builder, `fynd-rpc` — expects a bare
/// host and adds its own scheme, so `https://host` would otherwise become `https://https://host`
/// and fail after a long retry.
pub fn normalize_host(raw: &str) -> &str {
    let host = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .or_else(|| raw.strip_prefix("wss://"))
        .or_else(|| raw.strip_prefix("ws://"))
        .unwrap_or(raw);
    host.trim_end_matches('/')
}

/// What to capture, and from where.
pub struct LiveOptions {
    /// Tycho host without a scheme, e.g. `tycho-beta.propellerheads.xyz`. The scheme is added per
    /// use: `https://` for the RPC, and by the stream builder for the socket.
    pub tycho_host: String,
    pub tycho_api_key: String,
    pub chain: Chain,
    pub chain_name: String,
    /// Protocol systems to stream. `None` asks Tycho for every one it has.
    pub protocols: Option<Vec<String>>,
    /// Protocol systems to stream on top of `protocols`, including sources Tycho does not list
    /// such as `pricelevelstream:{venue}`.
    pub include_protocols: Vec<String>,
    pub min_tvl: f64,
    pub min_token_quality: i32,
    pub traded_n_days_ago: u64,
    /// How long to wait for the snapshot. Tycho builds it server-side, so the wait is not the
    /// block time.
    pub capture_timeout_secs: u64,
    /// Chain RPC, for the gas price the market is actually running at.
    pub rpc_url: Option<String>,
}

/// Connects, takes the snapshot, and returns it as a [`Market`] the rest of the bench already
/// knows how to use.
///
/// # Errors
///
/// Returns a message for a failed connection, an unusable filter, or a snapshot that does not
/// arrive inside `capture_timeout_secs`.
pub async fn capture_market(opts: &LiveOptions) -> Result<Market, String> {
    let mut steps = Steps::new();
    header_line("tycho", &opts.tycho_host);
    let mut protocols = match &opts.protocols {
        Some(list) if !list.is_empty() => list.clone(),
        _ => discover_protocols(&opts.tycho_host, &opts.tycho_api_key, opts.chain).await?,
    };
    add_streamed_protocols(&mut protocols, &opts.include_protocols)?;
    header_line("protocols", protocols.join(", "));
    steps.done("protocols in");

    let tokens = load_all_tokens(
        &opts.tycho_host,
        false,
        Some(&opts.tycho_api_key),
        true,
        opts.chain,
        Some(opts.min_token_quality),
        Some(opts.traded_n_days_ago),
    )
    .await
    .map_err(|e| format!("failed to load the token list: {e}"))?;
    header_line("tokens", tokens.len());
    steps.done("token list in");

    let filter = ComponentFilter::with_tvl_range(opts.min_tvl, opts.min_tvl);
    let builder = register_exchanges_for_recording(
        ProtocolStreamBuilder::new(&opts.tycho_host, opts.chain),
        filter,
        &protocols,
    )
    .map_err(|e| format!("failed to register exchanges: {e}"))?;

    // Split from the build because the two fail differently on a slow start: handing the token
    // list over is local work over 8k tokens, while the build reaches out for every VM protocol's
    // contract code.
    let builder = builder
        .auth_key(Some(opts.tycho_api_key.clone()))
        .skip_state_decode_failures(true)
        .set_tokens(tokens.clone())
        .await;
    steps.done("tokens set in");

    let mut stream = Box::pin(
        builder
            .build()
            .await
            .map_err(|e| format!("failed to build the Tycho stream: {e}"))?,
    );
    steps.done("stream built in");

    header_line("waiting", format!("for the snapshot, up to {}s", opts.capture_timeout_secs));
    let mut snapshot =
        match tokio::time::timeout(Duration::from_secs(opts.capture_timeout_secs), stream.next())
            .await
        {
            Ok(Some(Ok(update))) => update,
            Ok(Some(Err(e))) => return Err(format!("Tycho stream error: {e}")),
            Ok(None) => return Err("Tycho stream ended before sending a snapshot".to_string()),
            Err(_) => {
                return Err(format!(
                    "no snapshot within {}s -- raise --capture-timeout-secs or narrow --min-tvl",
                    opts.capture_timeout_secs
                ))
            }
        };

    // Taken after the Tycho snapshot rather than alongside it: the ladders are quoted for the
    // block being built, so the freshest frame is the one asked for last.
    if let Some(levels) =
        capture_price_levels(opts.chain, &protocols, &tokens, opts.capture_timeout_secs).await?
    {
        header_line(
            "price levels",
            format!(
                "{} pairs quoted for block {}",
                levels.new_pairs.len(),
                levels.block_number_or_timestamp
            ),
        );
        snapshot
            .new_pairs
            .extend(levels.new_pairs);
        snapshot.states.extend(levels.states);
        steps.done("price levels in");
    }

    let block = snapshot.block_number_or_timestamp;
    let components = snapshot.new_pairs.len();
    let states = snapshot.states.len();
    header_line("captured block", format!("{block} ({components} components, {states} states)"));
    steps.done("snapshot in");

    // A protocol that was streamed but brought no pool is either unindexed on this endpoint or
    // filtered out by --min-tvl. Either way it is worth saying, because the market silently
    // missing a protocol looks the same in every number downstream.
    let present: std::collections::HashSet<&str> = snapshot
        .new_pairs
        .values()
        .map(|component| component.protocol_system.as_str())
        .collect();
    let empty: Vec<&str> = protocols
        .iter()
        .map(String::as_str)
        .filter(|protocol| !present.contains(protocol))
        .collect();
    // An explicitly included protocol bringing nothing is a different case: the run was asked for
    // it by name, and a typo there benches a market silently missing the venue -- especially with
    // the companion `--exclude-protocols` dropping the on-chain path to the same one.
    let missing: Vec<&str> = opts
        .include_protocols
        .iter()
        .map(String::as_str)
        .filter(|protocol| !present.contains(protocol))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "--include-protocols: {} brought no component into the market.\nStreamed: {}",
            missing.join(", "),
            protocols.join(", ")
        ));
    }

    if !empty.is_empty() {
        header_line(
            "no pools from",
            format!("{} (unindexed here, or below --min-tvl {})", empty.join(", "), opts.min_tvl),
        );
    }

    let live_gas_price = match &opts.rpc_url {
        Some(url) => fetch_gas_price_wei(url).await,
        None => None,
    };
    steps.done("gas price in");
    header_line("captured in", format!("{:.1}s", steps.total()));

    Ok(Market {
        chain: opts.chain,
        market_gas_price: live_gas_price,
        updates: vec![snapshot],
        source: MarketSource::Live {
            chain_name: opts.chain_name.clone(),
            block,
            components,
            states,
            protocols,
            min_tvl: opts.min_tvl,
        },
    })
}

/// Appends the `--include-protocols` entries to the streamed list.
///
/// A name that is already streamed stops the run, because an entry adding nothing to the market
/// is a mistake worth seeing rather than a silent no-op.
fn add_streamed_protocols(protocols: &mut Vec<String>, requested: &[String]) -> Result<(), String> {
    for protocol in requested {
        if protocols.contains(protocol) {
            return Err(format!(
                "--include-protocols: {protocol} is already streamed.\nStreamed: {}",
                protocols.join(", ")
            ));
        }
        protocols.push(protocol.clone());
    }
    Ok(())
}

/// Takes one frame from the pAMM price level stream, or `None` when no `pricelevelstream:` venue
/// was asked for.
///
/// A venue named here is in the market solely because of this frame, so a stream that says
/// nothing is an error rather than something to capture without.
///
/// # Errors
///
/// Returns a message for an unserved venue, a stream that ends, or one that sends no frame inside
/// `timeout_secs`.
async fn capture_price_levels(
    chain: Chain,
    protocols: &[String],
    tokens: &HashMap<Bytes, Token>,
    timeout_secs: u64,
) -> Result<Option<Update>, String> {
    let Some(stream) = open_price_level_stream_for_recording(chain, protocols, tokens)? else {
        return Ok(None);
    };
    let mut stream = Box::pin(stream);
    header_line("waiting", format!("for pAMM price levels, up to {timeout_secs}s"));
    match tokio::time::timeout(Duration::from_secs(timeout_secs), stream.next()).await {
        Ok(Some(update)) => Ok(Some(update)),
        Ok(None) => Err("the pAMM price level stream ended before sending a frame".to_string()),
        Err(_) => Err(format!(
            "no pAMM price levels within {timeout_secs}s -- check the venues named in \
             --include-protocols"
        )),
    }
}

/// Every protocol system Tycho has for this chain, streamable and DCI alike.
///
/// Tycho answers with two lists. The second holds protocols served through the Dynamic Contract
/// Indexer, and on some deployments that is the only place Curve and Balancer appear. The stream
/// client turns DCI on per extractor by itself (it checks the same list when registering), so both
/// belong in the request -- dropping the second silently costs a market its VM liquidity.
///
/// The `all_onchain` / `native_onchain` expansion lives in `fynd-rpc`, which fynd-core cannot
/// depend on, so the plain listing is done here.
async fn discover_protocols(
    host: &str,
    api_key: &str,
    chain: Chain,
) -> Result<Vec<String>, String> {
    let rpc_url = format!("https://{host}");
    let options = HttpRPCClientOptions::new().with_auth_key(Some(api_key.to_string()));
    let client = HttpRPCClient::new(&rpc_url, options)
        .map_err(|e| format!("failed to reach the Tycho RPC at {rpc_url}: {e}"))?;

    let systems = client
        .get_protocol_systems(ProtocolSystemsParams::new(chain))
        .await
        .map_err(|e| format!("failed to list protocol systems: {e}"))?;

    let systems = systems.into_data();
    let streamable = systems.protocol_systems();
    let dci = systems.dci_protocols();

    let mut all: Vec<String> = streamable.to_vec();
    let dci_only: Vec<String> = dci
        .iter()
        .filter(|protocol| !streamable.contains(protocol))
        .cloned()
        .collect();
    if !dci_only.is_empty() {
        header_line("via dci", dci_only.join(", "));
        all.extend(dci_only);
    }

    if all.is_empty() {
        return Err(format!("Tycho reports no protocol systems for {chain}"));
    }
    // Sorted, so two captures from the same deployment register in the same order.
    all.sort();
    Ok(all)
}

/// The chain's current gas price, or `None` with a warning if the RPC will not say.
async fn fetch_gas_price_wei(rpc_url: &str) -> Option<BigUint> {
    let client = match EthereumRpcClient::new(rpc_url) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("  warning: gas price RPC unusable: {e}");
            return None;
        }
    };
    match client.get_latest_fee_price().await {
        Ok(price) => Some(price.effective_gas_price().clone()),
        Err(e) => {
            eprintln!("  warning: could not read the gas price: {e}");
            None
        }
    }
}
