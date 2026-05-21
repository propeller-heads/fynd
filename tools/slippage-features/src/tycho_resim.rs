use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use fynd_core::{
    derived::SharedDerivedDataRef,
    feed::market_data::SharedMarketDataRef,
    observer::{QuoteProducedEvent, MAX_BLOCK_OFFSET},
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::{debug, error, info, warn};

use crate::{
    decay::compute_decay_bps,
    parquet_writer::{write_hop_decay_parquet, HopDecayRecord},
};

/// Pending quote waiting for resimulation at future blocks.
struct PendingQuote {
    event: QuoteProducedEvent,
    /// Accumulated hop decay records across all block offsets.
    records: Vec<HopDecayRecord>,
}

/// Background task that resimulates quotes at each new block for up to
/// `MAX_BLOCK_OFFSET` blocks after the quote was produced.
///
/// On each new block it walks every hop in the route, calls
/// `ProtocolSim::get_amount_out` against the current `SharedMarketData`
/// state, and records per-hop decay. When a quote's observation window
/// closes, the accumulated records are flushed to a parquet file.
pub async fn run_tycho_resim(
    mut rx: tokio::sync::mpsc::Receiver<QuoteProducedEvent>,
    market_data: SharedMarketDataRef,
    derived_data: SharedDerivedDataRef,
    output_dir: PathBuf,
) {
    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        error!(path = %output_dir.display(), error = %e, "cannot create output directory");
        return;
    }

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
                pending.push_back(PendingQuote { event: evt, records: Vec::new() });
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

        resim_at_block(&mut pending, &market_data, &derived_data, current_block).await;
        flush_expired(&mut pending, current_block, &output_dir);
    }
}

/// Resimulate all pending quotes at `current_block`.
async fn resim_at_block(
    pending: &mut VecDeque<PendingQuote>,
    market_data: &SharedMarketDataRef,
    derived_data: &SharedDerivedDataRef,
    current_block: u64,
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

        for (hop_idx, replay_hop_out) in &replay_amounts {
            let swap = &pq.event.route.swaps[*hop_idx as usize];

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

            let fee_tier = md
                .get_simulation_state(&swap.component_id)
                .map(|sim| sim.fee());

            pq.records.push(HopDecayRecord {
                quote_id: pq.event.quote_id.clone(),
                solver_id: pq.event.solver_id.clone(),
                request_id: pq.event.request_id.clone(),
                block_offset,
                hop_index: *hop_idx,
                component_id: swap.component_id.clone(),
                protocol: swap.protocol.clone(),
                hop_amount_out: replay_hop_out.to_string(),
                hop_decay_bps: hop_decay,
                depth_at_1pct,
                depth_at_5pct,
                spot_price,
                token_price_in_native,
                fee_tier,
                marginal_liquidity: None,
                concentration_gini: None,
                route_total_amount_out: route_out_str.clone(),
                route_decay_bps: route_decay,
            });
        }
    }
}

/// Flush quotes whose observation window has closed.
fn flush_expired(pending: &mut VecDeque<PendingQuote>, current_block: u64, output_dir: &Path) {
    while let Some(front) = pending.front() {
        let max_block = front.event.block_number + u64::from(MAX_BLOCK_OFFSET);
        if current_block <= max_block {
            break;
        }

        let Some(pq) = pending.pop_front() else {
            break;
        };
        flush_one(pq, output_dir);
    }
}

/// Flush all remaining quotes regardless of window.
fn flush_all(pending: &mut VecDeque<PendingQuote>, output_dir: &Path) {
    while let Some(pq) = pending.pop_front() {
        flush_one(pq, output_dir);
    }
}

/// Write one quote's accumulated records to a parquet file.
fn flush_one(pq: PendingQuote, output_dir: &Path) {
    if pq.records.is_empty() {
        debug!(quote_id = %pq.event.quote_id, "no records to flush");
        return;
    }

    let filename = format!("hop_decay_{}.parquet", pq.event.quote_id);
    let path = output_dir.join(filename);

    match write_hop_decay_parquet(&path, &pq.records) {
        Ok(()) => {
            info!(
                quote_id = %pq.event.quote_id,
                records = pq.records.len(),
                path = %path.display(),
                "flushed hop decay records"
            );
        }
        Err(e) => {
            error!(
                quote_id = %pq.event.quote_id,
                error = %e,
                "failed to write hop decay parquet"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use fynd_core::{
        derived::DerivedData,
        feed::market_data::SharedMarketData,
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

        let market = SharedMarketData::new_shared();
        let derived = DerivedData::new_shared();

        let out = dir.path().to_path_buf();
        let handle = tokio::spawn(run_tycho_resim(rx, market, derived, out));

        // Close the channel immediately.
        drop(_tx);

        // Task should complete without panic.
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn missing_sim_state_skips_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let market = SharedMarketData::new_shared();
        let derived = DerivedData::new_shared();

        // Market has a block but NO simulation states or tokens.
        {
            let mut md = market.write().await;
            md.update_last_updated(BlockInfo::new(100, "0x1".into(), 0));
        }

        let out = dir.path().to_path_buf();
        let handle = tokio::spawn(run_tycho_resim(rx, market.clone(), derived, out));

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
        let market = SharedMarketData::new_shared();
        let derived = DerivedData::new_shared();

        // Block stays at 100.
        {
            let mut md = market.write().await;
            md.update_last_updated(BlockInfo::new(100, "0x1".into(), 0));
        }

        let out = dir.path().to_path_buf();
        let handle = tokio::spawn(run_tycho_resim(rx, market.clone(), derived, out));

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
