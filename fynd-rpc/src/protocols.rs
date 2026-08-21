//! Tycho protocol system discovery.

use anyhow::{bail, Result};
use fynd_core::feed::protocol_registry::{parse_exclusion, ProtocolSpec, EXCLUDE_PREFIX};
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
/// An `exclude:` entry drops that protocol system from the resolved list, so
/// `all_onchain,exclude:vm:fermiswap` streams everything on-chain except FermiSwap. This is how a
/// venue reached through a second integration path — the pAMM price level stream, say — is kept
/// from being streamed twice and having its liquidity double-counted.
///
/// # Errors
///
/// Returns an error if an entry requests exclusive liquidity for a protocol that has no exclusive
/// variant, if a protocol is both requested and excluded, if an exclusion names nothing or matches
/// no streamed protocol, or if the resolved list is empty.
pub async fn resolve_protocols(
    tycho_url: &str,
    auth_key: Option<&str>,
    use_tls: bool,
    chain: Chain,
    requested: &[String],
) -> Result<Vec<String>> {
    // Parsed before the RPC call so a malformed entry fails without waiting on Tycho.
    let (explicit, excluded) = split_requested(requested)?;
    reject_requested_and_excluded(&explicit, &excluded)?;

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
    apply_exclusions(&mut protocols, &excluded)?;

    if protocols.is_empty() {
        bail!("no supported protocols found. Provide --protocols or check Tycho connectivity.");
    }
    Ok(protocols
        .iter()
        .map(ProtocolSpec::to_string)
        .collect())
}

/// Splits the requested entries into the protocols to stream and the systems to drop, skipping
/// the expansion tokens.
///
/// One pass over the entries, so the two halves cannot disagree about which entry is which.
fn split_requested(entries: &[String]) -> Result<(Vec<ProtocolSpec>, Vec<String>)> {
    let mut streamed = Vec::new();
    let mut excluded = Vec::new();
    for entry in entries {
        if entry == ALL_ONCHAIN || entry == NATIVE_ONCHAIN {
            continue;
        }
        match parse_exclusion(entry) {
            Some(system) => {
                let system = system?;
                if system.is_empty() {
                    bail!("'{entry}' names no protocol system to exclude");
                }
                excluded.push(system);
            }
            None => streamed.push(ProtocolSpec::parse(entry)?),
        }
    }
    Ok((streamed, excluded))
}

/// Rejects a list naming the same protocol system as both streamed and excluded.
fn reject_requested_and_excluded(streamed: &[ProtocolSpec], excluded: &[String]) -> Result<()> {
    for protocol in streamed {
        if excluded.contains(&protocol.system) {
            bail!(
                "protocol '{}' is both requested and excluded with '{EXCLUDE_PREFIX}'",
                protocol.system
            );
        }
    }
    Ok(())
}

/// Drops every excluded protocol system from `protocols`.
///
/// # Errors
///
/// An exclusion that matches nothing is an error, not a warning. An exclusion is what keeps a
/// venue reachable through two integration paths from being counted twice, so a typo in one has
/// to stop the solver rather than quietly ship the market it exists to prevent.
fn apply_exclusions(protocols: &mut Vec<ProtocolSpec>, excluded: &[String]) -> Result<()> {
    for system in excluded {
        let before = protocols.len();
        protocols.retain(|protocol| &protocol.system != system);
        if protocols.len() == before {
            bail!(
                "excluded protocol '{system}' is not in the resolved list; available: {}",
                protocols
                    .iter()
                    .map(|protocol| protocol.system.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
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
    fn merge(expanded: &[&str], requested: &[&str]) -> Result<Vec<String>> {
        let mut protocols = expanded
            .iter()
            .map(|system| ProtocolSpec::public(*system))
            .collect();
        let (explicit, excluded) = split_requested(&strings(requested))?;
        reject_requested_and_excluded(&explicit, &excluded)?;
        merge_explicit(&mut protocols, explicit);
        apply_exclusions(&mut protocols, &excluded)?;
        Ok(protocols
            .iter()
            .map(ProtocolSpec::to_string)
            .collect())
    }

    #[test]
    fn test_merge_exclusive_replaces_expanded() {
        let merged =
            merge(&["uniswap_v3", "ekubo_v3"], &[ALL_ONCHAIN, "exclusive:ekubo_v3"]).unwrap();
        assert_eq!(merged, strings(&["uniswap_v3", "exclusive:ekubo_v3"]));
    }

    #[test]
    fn test_merge_appends_unexpanded_entries() {
        let merged = merge(&["uniswap_v3"], &[ALL_ONCHAIN, "rfq:bebop"]).unwrap();
        assert_eq!(merged, strings(&["uniswap_v3", "rfq:bebop"]));
    }

    #[test]
    fn test_merge_keeps_exclusive_regardless_of_order() {
        for requested in [["ekubo_v3", "exclusive:ekubo_v3"], ["exclusive:ekubo_v3", "ekubo_v3"]] {
            assert_eq!(merge(&[], &requested).unwrap(), strings(&["exclusive:ekubo_v3"]));
        }
    }

    #[test]
    fn test_merge_without_expansion() {
        let merged = merge(&[], &["uniswap_v2", "uniswap_v3"]).unwrap();
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

    #[test]
    fn test_exclusion_drops_expanded_protocol() {
        let merged = merge(
            &["uniswap_v3", "vm:fermiswap"],
            &[ALL_ONCHAIN, "exclude:vm:fermiswap", "pricelevelstream:fermiswap"],
        )
        .unwrap();
        assert_eq!(merged, strings(&["uniswap_v3", "pricelevelstream:fermiswap"]));
    }

    #[test]
    fn test_requesting_and_excluding_one_system_is_rejected() {
        let merged = merge(&["uniswap_v3"], &["exclusive:ekubo_v3", "exclude:ekubo_v3"]);
        let Err(err) = merged else {
            panic!("expected requesting and excluding one system to be rejected");
        };
        assert!(
            err.to_string()
                .contains("both requested and excluded"),
            "got {err}"
        );
    }

    #[test]
    fn test_exclusion_matches_an_expanded_exclusive_protocol() {
        let merged =
            merge(&["ekubo_v3", "uniswap_v3"], &[ALL_ONCHAIN, "exclude:exclusive:ekubo_v3"])
                .unwrap();
        assert_eq!(merged, strings(&["uniswap_v3"]));
    }

    #[test]
    fn test_exclusion_of_absent_protocol_is_rejected() {
        let Err(err) = merge(&["uniswap_v3"], &[ALL_ONCHAIN, "exclude:vm:fermiswap"]) else {
            panic!("expected an exclusion matching nothing to be rejected");
        };
        assert!(
            err.to_string()
                .contains("is not in the resolved list"),
            "got {err}"
        );
    }

    #[test]
    fn test_exclusion_without_protocol_is_rejected() {
        let Err(err) = merge(&["uniswap_v3"], &[ALL_ONCHAIN, "exclude:"]) else {
            panic!("expected an exclusion naming nothing to be rejected");
        };
        assert!(
            err.to_string()
                .contains("names no protocol system"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn test_resolve_protocols_rejects_requested_and_excluded() {
        let result = resolve_protocols(
            "localhost:0",
            None,
            false,
            Chain::Ethereum,
            &strings(&["vm:fermiswap", "exclude:vm:fermiswap"]),
        )
        .await;
        let Err(err) = result else {
            panic!("expected a protocol that is both requested and excluded to be rejected");
        };
        assert!(
            err.to_string()
                .contains("both requested and excluded"),
            "got {err}"
        );
    }
}
