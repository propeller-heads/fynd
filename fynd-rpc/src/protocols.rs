//! Tycho protocol system discovery.

use std::collections::HashMap;

use anyhow::{bail, Result};
use tracing::info;
use tycho_simulation::{
    tycho_client::rpc::{
        AllTokensParams, HttpRPCClient, HttpRPCClientOptions, ProtocolComponentsPaginatedParams,
        ProtocolSystemsParams, RPCClient,
    },
    tycho_common::models::Chain,
};

use crate::config::defaults::MIN_TOKEN_QUALITY;

/// Concurrency for the paginated Tycho component/token fetches in [`fetch_token_pool_stats`].
const FETCH_CONCURRENCY: usize = 8;

/// Expansion token: fetch every on-chain protocol system from Tycho.
const ALL_ONCHAIN: &str = "all_onchain";
/// Expansion token: like [`ALL_ONCHAIN`] but drop VM-simulated protocols (those prefixed `vm:`),
/// keeping only native-Rust protocols.
const NATIVE_ONCHAIN: &str = "native_onchain";
/// Prefix marking a VM-simulated (EVM) protocol system.
const VM_PREFIX: &str = "vm:";

/// Fetches all available protocol systems from the Tycho RPC.
pub async fn fetch_protocol_systems(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
) -> Result<Vec<String>> {
    info!("Fetching available protocol systems from Tycho RPC...");
    let rpc_url =
        if use_tls { format!("https://{tycho_url}") } else { format!("http://{tycho_url}") };
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options)?;

    let request = ProtocolSystemsParams::new(chain);
    let response = rpc_client
        .get_protocol_systems(request)
        .await?;
    let protocols = response
        .data()
        .protocol_systems()
        .to_vec();
    info!("Fetched {} protocol system(s) from Tycho RPC", protocols.len());
    Ok(protocols)
}

/// Resolves a requested protocol list into concrete Tycho protocol systems.
///
/// Expansion tokens:
/// - `all_onchain` (or an empty `requested` list) → fetch every on-chain protocol system.
/// - `native_onchain` → fetch every on-chain protocol system, then drop the VM-simulated ones
///   (those prefixed `vm:`), keeping only native-Rust protocols.
///
/// Explicit entries other than the expansion tokens (e.g. `rfq:bebop`, `uniswap_v3`) are appended
/// if not already present. When no expansion token is given the list is returned unchanged.
/// Returns an error if the resolved list is empty.
pub async fn resolve_protocols(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
    requested: &[String],
) -> Result<Vec<String>> {
    let want_native = requested
        .iter()
        .any(|p| p == NATIVE_ONCHAIN);
    let want_all = requested.is_empty() ||
        requested
            .iter()
            .any(|p| p == ALL_ONCHAIN);

    let protocols = if want_all || want_native {
        let mut fetched = fetch_protocol_systems(tycho_url, auth_key, use_tls, chain).await?;
        if want_native {
            fetched.retain(|p| !p.starts_with(VM_PREFIX));
        }
        for p in requested {
            if p != ALL_ONCHAIN && p != NATIVE_ONCHAIN && !fetched.contains(p) {
                fetched.push(p.clone());
            }
        }
        fetched
    } else {
        requested.to_vec()
    };

    if protocols.is_empty() {
        bail!("no supported protocols found. Provide --protocols or check Tycho connectivity.");
    }
    Ok(protocols)
}

/// Per-token liquidity statistics derived from Tycho protocol components.
///
/// `pool_count` mirrors the connector-token score in the `derive-connector-tokens` command: the
/// number of pools a token appears in, a proxy for how much traffic it attracts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenPoolStats {
    /// 0x-prefixed lowercase hex address.
    pub address: String,
    /// Token symbol as reported by Tycho (may be empty for exotic tokens).
    pub symbol: String,
    /// Number of decimals; used to scale synthetic amounts into raw units.
    pub decimals: u32,
    /// Number of pools the token appears in across the queried protocol systems.
    pub pool_count: usize,
}

/// Fetches per-token pool counts and metadata from the Tycho RPC.
///
/// Queries every protocol system in `protocols` for its components (paginated), counts how many
/// pools each token appears in, then joins against Tycho's token list for symbols and decimals.
/// Tokens that appear in a pool but have no metadata (or fail the quality filter) are dropped, as
/// their decimals are needed to scale amounts.
///
/// The result is sorted by descending pool count then address so callers get a deterministic order
/// regardless of RPC page ordering.
pub async fn fetch_token_pool_stats(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
    protocols: &[String],
) -> Result<Vec<TokenPoolStats>> {
    let rpc_url =
        if use_tls { format!("https://{tycho_url}") } else { format!("http://{tycho_url}") };
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options)?;

    // Count pool appearances per token across every requested protocol system.
    let mut pool_count: HashMap<String, usize> = HashMap::new();
    for protocol in protocols {
        info!("Fetching components for protocol system '{protocol}'...");
        let params = ProtocolComponentsPaginatedParams::new(chain, protocol, FETCH_CONCURRENCY);
        let components = rpc_client
            .get_protocol_components_paginated(params)
            .await?;
        for component in &components {
            for token in &component.tokens {
                *pool_count
                    .entry(token.to_string())
                    .or_insert(0) += 1;
            }
        }
    }

    info!("Fetching token metadata from Tycho RPC...");
    let token_params =
        AllTokensParams::new(chain, FETCH_CONCURRENCY).with_min_quality(MIN_TOKEN_QUALITY);
    let tokens = rpc_client
        .get_all_tokens(token_params)
        .await?;

    let mut stats: Vec<TokenPoolStats> = Vec::new();
    for token in &tokens {
        let address = token.address.to_string();
        let Some(&count) = pool_count.get(&address) else {
            continue;
        };
        stats.push(TokenPoolStats {
            address,
            symbol: token.symbol.clone(),
            decimals: token.decimals,
            pool_count: count,
        });
    }

    stats.sort_by(|a, b| {
        b.pool_count
            .cmp(&a.pool_count)
            .then_with(|| a.address.cmp(&b.address))
    });

    if stats.is_empty() {
        bail!(
            "no tokens with both pool membership and metadata found for the requested protocols; \
             check Tycho connectivity and the --protocols / --chain arguments"
        );
    }
    info!("Derived pool stats for {} token(s)", stats.len());
    Ok(stats)
}
