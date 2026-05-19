//! Tycho protocol system discovery.

use anyhow::Result;
use tracing::info;
use tycho_simulation::{
    tycho_client::rpc::{HttpRPCClient, HttpRPCClientOptions, ProtocolSystemsParams, RPCClient},
    tycho_common::models::Chain,
};

/// Fetches all available protocol systems from the Tycho RPC, handling pagination.
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
