//! Tycho protocol system discovery.

use anyhow::Result;
use tracing::info;
use tycho_simulation::{
    tycho_client::rpc::{HttpRPCClient, HttpRPCClientOptions, RPCClient},
    tycho_common::{
        dto::{PaginationParams, ProtocolSystemsRequestBody},
        models::Chain,
    },
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

    const PAGE_SIZE: i64 = 100;
    let mut all_protocols = Vec::new();
    let mut page = 0;
    loop {
        let request = ProtocolSystemsRequestBody {
            chain: chain.into(),
            pagination: PaginationParams { page, page_size: PAGE_SIZE },
        };
        let response = rpc_client.get_protocol_systems(&request).await?;
        let count = response.protocol_systems.len();
        all_protocols.extend(response.protocol_systems);
        if (count as i64) < PAGE_SIZE {
            break;
        }
        page += 1;
    }
    info!("Fetched {} protocol system(s) from Tycho RPC", all_protocols.len());
    Ok(all_protocols)
}
