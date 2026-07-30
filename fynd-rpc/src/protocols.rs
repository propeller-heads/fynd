//! Tycho protocol system discovery.

use std::{collections::HashMap, future::Future, time::Duration};

use anyhow::{bail, Result};
use tokio::time::timeout;
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

/// Inputs to [`fetch_token_pool_stats`].
///
/// Bundles the Tycho connection parameters with the two capacity-testing knobs: a per-fetch
/// client-side `fetch_timeout` so a degraded indexer fails fast instead of hanging, and an optional
/// `min_tvl` passed to Tycho as `tvl_gt`. The dedicated `tycho-fynd-*` indexer endpoints plan-gate
/// component queries and reject them unless `min_tvl` is set.
pub struct TokenPoolStatsParams<'a> {
    /// Tycho RPC host:port (no scheme).
    pub tycho_url: &'a str,
    /// Optional Tycho auth key.
    pub auth_key: Option<&'a str>,
    /// Connect over HTTPS when true, HTTP otherwise.
    pub use_tls: bool,
    /// Target chain.
    pub chain: Chain,
    /// Protocol systems to query for components.
    pub protocols: &'a [String],
    /// When set, only pools whose TVL exceeds this value (native-token units) are counted, passed
    /// as `tvl_gt`. Required by the `tycho-fynd-*` endpoints, which plan-gate component queries.
    pub min_tvl: Option<f64>,
    /// Client-side deadline applied to each per-protocol component fetch and the token-metadata
    /// fetch.
    pub fetch_timeout: Duration,
}

/// Awaits a single Tycho fetch under a client-side deadline.
///
/// On completion the inner RPC error is surfaced unchanged; on a timeout the `on_timeout` message
/// is returned instead. Without this a degraded indexer leaves the fetch hanging indefinitely
/// (observed byte-flat stalls exceeding 18 minutes against `tycho-base-beta`).
async fn await_with_timeout<T, E, Fut>(
    fetch: Fut,
    timeout_after: Duration,
    on_timeout: impl FnOnce() -> String,
) -> Result<T>
where
    Fut: Future<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    match timeout(timeout_after, fetch).await {
        Ok(result) => result.map_err(anyhow::Error::from),
        Err(_elapsed) => Err(anyhow::anyhow!(on_timeout())),
    }
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
    params: &TokenPoolStatsParams<'_>,
) -> Result<Vec<TokenPoolStats>> {
    let rpc_url = if params.use_tls {
        format!("https://{}", params.tycho_url)
    } else {
        format!("http://{}", params.tycho_url)
    };
    let rpc_options =
        HttpRPCClientOptions::new().with_auth_key(params.auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options)?;

    // Count pool appearances per token across every requested protocol system.
    let mut pool_count: HashMap<String, usize> = HashMap::new();
    for protocol in params.protocols {
        info!("Fetching components for protocol system '{protocol}'...");
        let mut component_params =
            ProtocolComponentsPaginatedParams::new(params.chain, protocol, FETCH_CONCURRENCY);
        if let Some(min_tvl) = params.min_tvl {
            component_params = component_params.with_tvl_gt(min_tvl);
        }
        let components = await_with_timeout(
            rpc_client.get_protocol_components_paginated(component_params),
            params.fetch_timeout,
            || {
                format!(
                    "timed out after {}s fetching components for protocol system '{protocol}'; \
                     the Tycho indexer may be degraded. Exclude it via --protocols or raise \
                     --fetch-timeout-secs.",
                    params.fetch_timeout.as_secs()
                )
            },
        )
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
        AllTokensParams::new(params.chain, FETCH_CONCURRENCY).with_min_quality(MIN_TOKEN_QUALITY);
    let tokens =
        await_with_timeout(rpc_client.get_all_tokens(token_params), params.fetch_timeout, || {
            format!(
                "timed out after {}s fetching token metadata from Tycho; the indexer may be \
                 degraded. Raise --fetch-timeout-secs or retry.",
                params.fetch_timeout.as_secs()
            )
        })
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

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[tokio::test]
    async fn await_with_timeout_reports_deadline() {
        let fetch = std::future::pending::<std::result::Result<(), io::Error>>();
        let err = await_with_timeout(fetch, Duration::from_millis(10), || {
            "timed out fetching components for protocol system 'uniswap_v3'".to_string()
        })
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("protocol system 'uniswap_v3'"));
    }

    #[tokio::test]
    async fn await_with_timeout_passes_through_success() {
        let value =
            await_with_timeout(async { Ok::<u32, io::Error>(7) }, Duration::from_secs(30), || {
                "unused".to_string()
            })
            .await
            .unwrap();
        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn await_with_timeout_surfaces_inner_error() {
        let fetch =
            async { std::result::Result::<u32, io::Error>::Err(io::Error::other("rpc exploded")) };
        let err =
            await_with_timeout(fetch, Duration::from_secs(30), || "should not be used".to_string())
                .await
                .unwrap_err();
        assert!(err.to_string().contains("rpc exploded"));
        assert!(!err
            .to_string()
            .contains("should not be used"));
    }
}
