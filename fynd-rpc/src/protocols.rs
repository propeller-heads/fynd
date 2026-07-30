//! Tycho protocol system discovery.

use anyhow::{bail, Result};
use fynd_core::feed::protocol_registry::ProtocolSpec;
use tracing::info;
use tycho_simulation::{
    tycho_client::rpc::{HttpRPCClient, HttpRPCClientOptions, ProtocolSystemsParams, RPCClient},
    tycho_common::models::Chain,
};

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
/// Explicit entries other than the expansion tokens (e.g. `rfq:bebop`, `uniswap_v3`,
/// `exclusive:ekubo_v3`) are merged in by protocol system, so `all_onchain,exclusive:ekubo_v3`
/// streams `ekubo_v3` exactly once — with its exclusive pools included. Requesting a protocol both
/// with and without the `exclusive:` prefix streams it with exclusive pools included.
///
/// # Errors
///
/// Returns an error if an entry requests exclusive liquidity for a protocol that has no exclusive
/// variant, or if the resolved list is empty.
pub async fn resolve_protocols(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
    requested: &[String],
) -> Result<Vec<String>> {
    // Parsed before the RPC call so a malformed entry fails without waiting on Tycho.
    let explicit = parse_explicit(requested)?;

    let want_native = requested
        .iter()
        .any(|p| p == NATIVE_ONCHAIN);
    let want_all = requested.is_empty() ||
        requested
            .iter()
            .any(|p| p == ALL_ONCHAIN);

    let mut protocols: Vec<ProtocolSpec> = if want_all || want_native {
        let mut fetched = fetch_protocol_systems(tycho_url, auth_key, use_tls, chain).await?;
        if want_native {
            fetched.retain(|p| !p.starts_with(VM_PREFIX));
        }
        fetched
            .into_iter()
            .map(ProtocolSpec::public)
            .collect()
    } else {
        Vec::new()
    };
    merge_explicit(&mut protocols, explicit);

    if protocols.is_empty() {
        bail!("no supported protocols found. Provide --protocols or check Tycho connectivity.");
    }
    Ok(protocols
        .iter()
        .map(ProtocolSpec::to_string)
        .collect())
}

/// Parses the requested entries that name a protocol system, skipping the expansion tokens.
fn parse_explicit(entries: &[String]) -> Result<Vec<ProtocolSpec>> {
    entries
        .iter()
        .filter(|entry| *entry != ALL_ONCHAIN && *entry != NATIVE_ONCHAIN)
        .map(|entry| ProtocolSpec::parse(entry).map_err(Into::into))
        .collect()
}

/// Merges the explicitly requested protocols into `protocols`, one entry per protocol system.
///
/// A protocol requested with the `exclusive:` prefix keeps its exclusive pools no matter how the
/// other entries for the same system are written, so no ordering silently downgrades it.
fn merge_explicit(protocols: &mut Vec<ProtocolSpec>, explicit: Vec<ProtocolSpec>) {
    for protocol in explicit {
        match protocols
            .iter_mut()
            .find(|existing| existing.system == protocol.system)
        {
            Some(existing) => existing.exclusive |= protocol.exclusive,
            None => protocols.push(protocol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(entries: &[&str]) -> Vec<String> {
        entries
            .iter()
            .map(|e| (*e).to_string())
            .collect()
    }

    /// Resolves `requested` the way `resolve_protocols` does, against a fixed expansion.
    fn merge(expanded: &[&str], requested: &[&str]) -> Vec<String> {
        let mut protocols = expanded
            .iter()
            .map(|system| ProtocolSpec::public(*system))
            .collect();
        merge_explicit(&mut protocols, parse_explicit(&strings(requested)).unwrap());
        protocols
            .iter()
            .map(ProtocolSpec::to_string)
            .collect()
    }

    #[test]
    fn test_merge_exclusive_replaces_expanded() {
        let merged = merge(&["uniswap_v3", "ekubo_v3"], &[ALL_ONCHAIN, "exclusive:ekubo_v3"]);
        assert_eq!(merged, strings(&["uniswap_v3", "exclusive:ekubo_v3"]));
    }

    #[test]
    fn test_merge_appends_unexpanded_entries() {
        let merged = merge(&["uniswap_v3"], &[ALL_ONCHAIN, "rfq:bebop"]);
        assert_eq!(merged, strings(&["uniswap_v3", "rfq:bebop"]));
    }

    #[test]
    fn test_merge_keeps_exclusive_regardless_of_order() {
        for requested in [["ekubo_v3", "exclusive:ekubo_v3"], ["exclusive:ekubo_v3", "ekubo_v3"]] {
            assert_eq!(merge(&[], &requested), strings(&["exclusive:ekubo_v3"]));
        }
    }

    #[test]
    fn test_merge_without_expansion() {
        let merged = merge(&[], &["uniswap_v2", "uniswap_v3"]);
        assert_eq!(merged, strings(&["uniswap_v2", "uniswap_v3"]));
    }

    #[tokio::test]
    async fn test_resolve_protocols_rejects_unsupported_exclusive() {
        let result = resolve_protocols(
            "localhost:0",
            None,
            false,
            Chain::Ethereum,
            &strings(&["exclusive:uniswap_v3"]),
        )
        .await;
        let Err(err) = result else {
            panic!("expected `exclusive:uniswap_v3` to be rejected");
        };
        assert!(err
            .to_string()
            .contains("has no exclusive-liquidity variant"));
    }
}
