use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use fynd_core::{
    derived::SharedDerivedDataRef,
    feed::market_data::MarketData,
    observer::{QuoteProducedEvent, MAX_BLOCK_OFFSET},
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::{debug, error, info, warn};

use crate::{
    cex_dynamics::{CexDynamicsHandle, FIFTEEN_MIN_MS, FIVE_MIN_MS},
    decay::compute_decay_bps,
    parquet_writer::{
        write_hop_decay_parquet, write_hop_static_parquet, write_tycho_route_decay_parquet,
        HopDecayRecord, HopStaticRecord, TychoRouteDecayRecord,
    },
};

/// Fetch a fresh quote from the solver via HTTP.
async fn requote(
    http_client: &reqwest::Client,
    fynd_url: &str,
    token_in: &str,
    token_out: &str,
    amount_in: &str,
    sender: &str,
) -> Option<String> {
    let body = serde_json::json!({
        "orders": [{
            "token_in": token_in,
            "token_out": token_out,
            "amount": amount_in,
            "side": "sell",
            "sender": sender,
        }]
    });
    let resp = http_client
        .post(format!("{fynd_url}/v1/quote"))
        .json(&body)
        .send()
        .await
        .ok()?;
    let data: serde_json::Value = resp.json().await.ok()?;
    data["orders"][0]["amount_out"]
        .as_str()
        .map(String::from)
}

/// Pending quote waiting for resimulation at future blocks.
struct PendingQuote {
    event: QuoteProducedEvent,
    hop_statics: Vec<HopStaticRecord>,
    hop_decays: Vec<HopDecayRecord>,
    route_decays: Vec<TychoRouteDecayRecord>,
    statics_collected: bool,
}

/// Background task that resimulates quotes at each new block for up to
/// `MAX_BLOCK_OFFSET` blocks after the quote was produced.
///
/// On each new block it walks every hop in the route, calls
/// `ProtocolSim::get_amount_out` against the current `MarketData`
/// state, and records per-hop decay. When a quote's observation window
/// closes, the accumulated records are flushed to a parquet file.
pub async fn run_tycho_resim(
    mut rx: tokio::sync::mpsc::Receiver<QuoteProducedEvent>,
    market_data: MarketData,
    derived_data: SharedDerivedDataRef,
    output_dir: PathBuf,
    requote_url: Option<String>,
    cex_handle: Option<CexDynamicsHandle>,
) {
    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        error!(path = %output_dir.display(), error = %e, "cannot create output directory");
        return;
    }

    let http_client = requote_url.as_ref().map(|_| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client for requote")
    });

    let mut pending: VecDeque<PendingQuote> = VecDeque::new();
    let mut last_seen_block: u64 = 0;

    loop {
        // Wait for a new event or a short timeout so we can poll for block changes.
        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;

        match event {
            Ok(Some(evt)) => {
                debug!(
                    quote_id = %evt.quote_id,
                    block = evt.block_number,
                    hops = evt.route.swaps.len(),
                    "received quote for resim"
                );
                pending.push_back(PendingQuote {
                    event: evt,
                    hop_statics: Vec::new(),
                    hop_decays: Vec::new(),
                    route_decays: Vec::new(),
                    statics_collected: false,
                });
            }
            Ok(None) => {
                info!("resim channel closed, flushing remaining quotes");
                flush_all(&mut pending, &output_dir);
                return;
            }
            Err(_timeout) => {
                // No new event — fall through to block-change check.
            }
        }

        if pending.is_empty() {
            continue;
        }

        let current_block = {
            let md = market_data.read().await;
            md.last_updated()
                .map(|b| b.number())
                .unwrap_or(0)
        };

        if current_block <= last_seen_block {
            continue;
        }
        last_seen_block = current_block;

        resim_at_block(
            &mut pending,
            &market_data,
            &derived_data,
            current_block,
            http_client.as_ref(),
            requote_url.as_deref(),
            cex_handle.as_ref(),
        )
        .await;
        flush_expired(&mut pending, current_block, &output_dir);
    }
}

/// Resimulate all pending quotes at `current_block`.
async fn resim_at_block(
    pending: &mut VecDeque<PendingQuote>,
    market_data: &MarketData,
    derived_data: &SharedDerivedDataRef,
    current_block: u64,
    http_client: Option<&reqwest::Client>,
    requote_url: Option<&str>,
    cex_handle: Option<&CexDynamicsHandle>,
) {
    let md = market_data.read().await;
    let dd = derived_data.read().await;

    for pq in pending.iter_mut() {
        let quote_block = pq.event.block_number;
        if current_block <= quote_block {
            continue;
        }
        let offset = current_block - quote_block;
        if offset > u64::from(MAX_BLOCK_OFFSET) {
            continue;
        }
        let block_offset = offset as u32;

        let mut route_amount = None;
        // Stores (hop_index, replay_amount_out) for each successfully simulated hop.
        let mut replay_amounts: Vec<(u32, BigUint)> = Vec::new();

        // Walk each hop, chaining the output of each hop as input to the next.
        let mut current_amount: Option<BigUint> = None;
        for (hop_idx, swap) in pq.event.route.swaps.iter().enumerate() {
            let amount_in = match &current_amount {
                Some(a) => a.clone(),
                None => {
                    let Some(parsed) = BigUint::parse_bytes(swap.amount_in.as_bytes(), 10) else {
                        warn!(
                            quote_id = %pq.event.quote_id,
                            hop = hop_idx,
                            value = %swap.amount_in,
                            "cannot parse hop amount_in"
                        );
                        break;
                    };
                    parsed
                }
            };

            let Some(state) = md.get_simulation_state(&swap.component_id) else {
                debug!(
                    quote_id = %pq.event.quote_id,
                    component = %swap.component_id,
                    "simulation state not found, skipping quote"
                );
                break;
            };

            let Some(token_in) = md.get_token(&swap.token_in) else {
                debug!(
                    quote_id = %pq.event.quote_id,
                    token = ?swap.token_in,
                    "token_in not found, skipping quote"
                );
                break;
            };
            let Some(token_out) = md.get_token(&swap.token_out) else {
                debug!(
                    quote_id = %pq.event.quote_id,
                    token = ?swap.token_out,
                    "token_out not found, skipping quote"
                );
                break;
            };

            match state.get_amount_out(amount_in, token_in, token_out) {
                Ok(result) => {
                    current_amount = Some(result.amount.clone());
                    replay_amounts.push((hop_idx as u32, result.amount));
                }
                Err(e) => {
                    debug!(
                        quote_id = %pq.event.quote_id,
                        component = %swap.component_id,
                        hop = hop_idx,
                        error = ?e,
                        "simulation failed for hop"
                    );
                    break;
                }
            }
        }

        // Only record if we successfully simulated all hops.
        if replay_amounts.len() != pq.event.route.swaps.len() {
            continue;
        }

        if let Some(last) = replay_amounts.last() {
            route_amount = Some(last.1.clone());
        }
        let route_out_str = route_amount
            .as_ref()
            .map_or_else(String::new, |a| a.to_string());

        let route_decay =
            compute_decay_bps(&pq.event.amount_out, &route_out_str).unwrap_or(f64::NAN);

        // Fetch a fresh re-quote if configured.
        let (requote_out, market_mvmt, exec_slip) = if let (Some(client), Some(url)) =
            (http_client, requote_url)
        {
            let first_swap = pq.event.route.swaps.first();
            let last_swap = pq.event.route.swaps.last();
            if let (Some(first), Some(last)) = (first_swap, last_swap) {
                let token_in = format!("0x{}", hex::encode(&first.token_in));
                let token_out = format!("0x{}", hex::encode(&last.token_out));
                let sender = "0x0000000000000000000000000000000000000001";
                match requote(client, url, &token_in, &token_out, &pq.event.amount_in, sender).await
                {
                    Some(fresh_out) if !fresh_out.is_empty() && fresh_out != "0" => {
                        let mm = compute_decay_bps(&pq.event.amount_out, &fresh_out).ok();
                        let es = mm.map(|m| route_decay - m);
                        (Some(fresh_out), mm, es)
                    }
                    _ => (None, None, None),
                }
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        // CEX dynamics: resolve token symbols and query Binance data.
        let (cex_mid, cex_dex_spread, vol_5m, vol_15m) = if let Some(cex) = cex_handle {
            let first_swap = pq.event.route.swaps.first();
            let sym_in = first_swap.and_then(|s| {
                md.get_token(&s.token_in).map(|t| t.symbol.clone())
            });
            let pair_symbol = sym_in.as_deref().and_then(|s| cex.resolve_pair_symbol(s, "USDT"));

            if let Some(ref pair) = pair_symbol {
                let mid = cex.mid_price(pair);
                let dex_spot = first_swap.and_then(|s| {
                    let key = (s.component_id.clone(), s.token_in.clone(), s.token_out.clone());
                    dd.spot_prices().and_then(|p| p.get(&key).copied())
                });
                let spread = mid.and_then(|m| {
                    dex_spot.map(|d| (m - d) / m * 10_000.0)
                });
                let v5 = cex.realized_vol_bps(pair, FIVE_MIN_MS);
                let v15 = cex.realized_vol_bps(pair, FIFTEEN_MIN_MS);
                (mid, spread, v5, v15)
            } else {
                (None, None, None, None)
            }
        } else {
            (None, None, None, None)
        };

        pq.route_decays.push(TychoRouteDecayRecord {
            quote_id: pq.event.quote_id.clone(),
            solver_id: pq.event.solver_id.clone(),
            block_offset,
            route_total_amount_out: route_out_str,
            route_decay_bps: route_decay,
            requote_amount_out: requote_out,
            market_movement_bps: market_mvmt,
            execution_slippage_bps: exec_slip,
            cex_mid_price: cex_mid,
            cex_dex_spread_bps: cex_dex_spread,
            realized_vol_5m_bps: vol_5m,
            realized_vol_15m_bps: vol_15m,
        });

        for (hop_idx, replay_hop_out) in &replay_amounts {
            let swap = &pq.event.route.swaps[*hop_idx as usize];

            // Collect static hop metadata once (first block_offset).
            if !pq.statics_collected {
                let fee_tier = md
                    .get_simulation_state(&swap.component_id)
                    .and_then(|sim| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sim.fee()))
                            .ok()
                    });

                pq.hop_statics.push(HopStaticRecord {
                    quote_id: pq.event.quote_id.clone(),
                    solver_id: pq.event.solver_id.clone(),
                    hop_index: *hop_idx,
                    component_id: swap.component_id.clone(),
                    protocol: swap.protocol.clone(),
                    fee_tier,
                });
            }

            let hop_decay = compute_decay_bps(&swap.amount_out, &replay_hop_out.to_string())
                .unwrap_or(f64::NAN);

            let depth_key =
                (swap.component_id.clone(), swap.token_in.clone(), swap.token_out.clone());

            let depth_at_1pct = dd
                .pool_depths()
                .and_then(|depths| depths.get(&depth_key))
                .map(|d| d.to_string());

            let depth_at_5pct = dd
                .pool_depths_5pct()
                .and_then(|depths| depths.get(&depth_key))
                .map(|d| d.to_string());

            let spot_price = dd
                .spot_prices()
                .and_then(|prices| prices.get(&depth_key).copied());

            let token_price_in_native = dd.token_prices().and_then(|prices| {
                let price = prices.get(&swap.token_in)?;
                let n = price.numerator.to_f64()?;
                let d = price.denominator.to_f64()?;
                if d == 0.0 {
                    None
                } else {
                    Some(n / d)
                }
            });

            pq.hop_decays.push(HopDecayRecord {
                quote_id: pq.event.quote_id.clone(),
                solver_id: pq.event.solver_id.clone(),
                block_offset,
                hop_index: *hop_idx,
                hop_amount_out: replay_hop_out.to_string(),
                hop_decay_bps: hop_decay,
                depth_at_1pct,
                depth_at_5pct,
                spot_price,
                token_price_in_native,
            });
        }
        pq.statics_collected = true;
    }
}

/// Flush quotes whose observation window has closed.
///
/// Scans the entire deque rather than stopping at the first non-expired
/// entry, so out-of-order quotes (earlier blocks arriving after later
/// blocks) are flushed correctly.
fn flush_expired(pending: &mut VecDeque<PendingQuote>, current_block: u64, output_dir: &Path) {
    let mut i = 0;
    while i < pending.len() {
        let max_block = pending[i].event.block_number + u64::from(MAX_BLOCK_OFFSET);
        if current_block > max_block {
            let pq = pending.remove(i).expect("index valid");
            flush_one(pq, output_dir);
        } else {
            i += 1;
        }
    }
}

/// Flush all remaining quotes regardless of window.
fn flush_all(pending: &mut VecDeque<PendingQuote>, output_dir: &Path) {
    while let Some(pq) = pending.pop_front() {
        flush_one(pq, output_dir);
    }
}

/// Write one quote's accumulated records to parquet files.
fn flush_one(pq: PendingQuote, output_dir: &Path) {
    if pq.hop_decays.is_empty() {
        debug!(quote_id = %pq.event.quote_id, "no records to flush");
        return;
    }

    let qid = &pq.event.quote_id;

    let hop_decay_dir = output_dir.join("hop_decay");
    let hop_static_dir = output_dir.join("hop_static");
    let tycho_route_dir = output_dir.join("tycho_route_decay");

    for dir in [&hop_decay_dir, &hop_static_dir, &tycho_route_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            error!(path = %dir.display(), error = %e, "cannot create subdirectory");
            return;
        }
    }

    if let Err(e) = write_hop_decay_parquet(
        &hop_decay_dir.join(format!("hop_decay_{qid}.parquet")),
        &pq.hop_decays,
    ) {
        error!(quote_id = %qid, error = %e, "failed to write hop decay parquet");
    }

    if let Err(e) = write_hop_static_parquet(
        &hop_static_dir.join(format!("hop_static_{qid}.parquet")),
        &pq.hop_statics,
    ) {
        error!(quote_id = %qid, error = %e, "failed to write hop static parquet");
    }

    if let Err(e) = write_tycho_route_decay_parquet(
        &tycho_route_dir.join(format!("tycho_route_decay_{qid}.parquet")),
        &pq.route_decays,
    ) {
        error!(quote_id = %qid, error = %e, "failed to write tycho route decay parquet");
    }

    info!(
        quote_id = %qid,
        hop_decays = pq.hop_decays.len(),
        hop_statics = pq.hop_statics.len(),
        route_decays = pq.route_decays.len(),
        "flushed resim records"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use fynd_core::{
        derived::DerivedData,
        feed::market_data::MarketData,
        observer::{ObservedRoute, ObservedSwap, QuoteProducedEvent},
        types::BlockInfo,
    };
    use tycho_simulation::tycho_core::Bytes;

    use super::*;

    fn make_address(byte: u8) -> Bytes {
        Bytes::from([byte; 20].as_slice())
    }

    fn make_event(
        quote_id: &str,
        block_number: u64,
        swaps: Vec<ObservedSwap>,
        amount_in: &str,
        amount_out: &str,
    ) -> QuoteProducedEvent {
        QuoteProducedEvent {
            request_id: "req-1".into(),
            quote_id: quote_id.into(),
            solver_id: "solver-a".into(),
            is_winner: true,
            block_number,
            chain_id: 1,
            route: ObservedRoute { swaps },
            amount_in: amount_in.into(),
            amount_out: amount_out.into(),
            gas_estimate: 100_000,
            calldata: vec![],
            algorithm_type: "most_liquid".into(),
            algorithm_settings: HashMap::new(),
            n_alternatives: 1,
            gap_to_second_best_bps: None,
            score_dispersion: None,
            slippage_tolerance: None,
            all_candidates: vec![],
        }
    }

    fn single_hop_swap() -> ObservedSwap {
        ObservedSwap {
            component_id: "pool-1".into(),
            protocol: "uniswap_v2".into(),
            token_in: make_address(0x01),
            token_out: make_address(0x02),
            amount_in: "1000".into(),
            amount_out: "2000".into(),
            gas_estimate: "50000".into(),
            split: 0.0,
        }
    }

    #[tokio::test]
    async fn channel_close_triggers_flush() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = tokio::sync::mpsc::channel::<QuoteProducedEvent>(16);

        let market = MarketData::new_shared();
        let derived = DerivedData::new_shared();

        let out = dir.path().to_path_buf();
        let handle = tokio::spawn(run_tycho_resim(rx, market, derived, out, None, None));

        // Close the channel immediately.
        drop(_tx);

        // Task should complete without panic.
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn missing_sim_state_skips_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let market = MarketData::new_shared();
        let derived = DerivedData::new_shared();

        // Market has a block but NO simulation states or tokens.
        {
            let mut md = market.write().await;
            md.update_last_updated(BlockInfo::new(100, "0x1".into(), 0));
        }

        let out = dir.path().to_path_buf();
        let handle = tokio::spawn(run_tycho_resim(rx, market.clone(), derived, out, None, None));

        // Send a quote referencing a non-existent pool.
        let swap = single_hop_swap();
        let event = make_event("q-miss", 100, vec![swap], "1000", "2000");
        tx.send(event).await.unwrap();

        // Wait for the event to be received.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Advance past the observation window.
        {
            let mut md = market.write().await;
            md.update_last_updated(BlockInfo::new(112, "0x2".into(), 0));
        }

        // Wait for the flush cycle.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        drop(tx);
        handle.await.unwrap();

        // No parquet file should be written since simulation skipped.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();

        assert!(entries.is_empty(), "expected no parquet files when sim state is missing");
    }

    #[tokio::test]
    async fn events_before_block_advance_are_queued() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let market = MarketData::new_shared();
        let derived = DerivedData::new_shared();

        // Block stays at 100.
        {
            let mut md = market.write().await;
            md.update_last_updated(BlockInfo::new(100, "0x1".into(), 0));
        }

        let out = dir.path().to_path_buf();
        let handle = tokio::spawn(run_tycho_resim(rx, market.clone(), derived, out, None, None));

        let swap = single_hop_swap();
        let event = make_event("q-queue", 100, vec![swap], "1000", "2000");
        tx.send(event).await.unwrap();

        // Wait a bit — no block advance so nothing should be flushed.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();

        assert!(entries.is_empty(), "no files until block advances past window");

        drop(tx);
        handle.await.unwrap();
    }
}
