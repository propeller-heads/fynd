use fynd_core::recording::MarketRecording;

/// Connect to Tycho, capture raw Update messages for `duration_secs`,
/// and return a MarketRecording.
pub async fn record_market(
    tycho_url: &str,
    rpc_url: &str,
    tycho_api_key: &str,
    chain: &str,
    duration_secs: u64,
) -> anyhow::Result<MarketRecording> {
    // This function needs to:
    // 1. Build a ProtocolStreamBuilder (same as TychoFeed::start)
    // 2. Receive Update messages from the stream
    // 3. For each Update: convert to RecordedUpdate and store
    // 4. After duration_secs, stop and package into MarketRecording
    //
    // The stream setup mirrors fynd-rpc/src/builder.rs.
    // Key difference: instead of passing Updates to handle_tycho_message,
    // we store them raw.
    let _ = (tycho_url, rpc_url, tycho_api_key, chain, duration_secs);
    todo!("Implement using ProtocolStreamBuilder — requires live Tycho connection")
}
