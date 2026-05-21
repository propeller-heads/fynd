use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use fynd_core::observer::{ObservedRoute, QuoteProducedEvent, SolverObserver};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::parquet_writer::{write_quote_log_parquet, QuoteLogRecord};

/// Observer that forwards quote events to a resim channel and buffers them for
/// periodic parquet output.
pub struct QuoteLogObserver {
    buffer: Mutex<Vec<QuoteLogRecord>>,
    output_dir: PathBuf,
    tx: mpsc::Sender<QuoteProducedEvent>,
    flush_threshold: usize,
    last_write: Mutex<Instant>,
}

impl QuoteLogObserver {
    pub fn new(
        output_dir: PathBuf,
        tx: mpsc::Sender<QuoteProducedEvent>,
        flush_threshold: usize,
    ) -> Self {
        Self {
            buffer: Mutex::new(Vec::with_capacity(flush_threshold)),
            output_dir,
            tx,
            flush_threshold,
            last_write: Mutex::new(Instant::now()),
        }
    }

    /// Flush buffered records if the buffer is non-empty and `max_age` has
    /// elapsed since the last write. Returns `true` if a flush occurred.
    pub fn flush_if_stale(&self, max_age: Duration) -> bool {
        let stale = {
            let last = self
                .last_write
                .lock()
                .expect("last_write lock poisoned");
            last.elapsed() >= max_age
        };
        if !stale {
            return false;
        }
        let records = {
            let mut buf = self
                .buffer
                .lock()
                .expect("buffer lock poisoned");
            buf.drain(..).collect::<Vec<_>>()
        };
        if records.is_empty() {
            return false;
        }
        self.flush_records(records);
        true
    }

    /// Flush any remaining buffered records. Call on shutdown.
    pub fn flush_remaining(&self) {
        let records = {
            let mut buf = self
                .buffer
                .lock()
                .expect("buffer lock poisoned");
            buf.drain(..).collect::<Vec<_>>()
        };
        if !records.is_empty() {
            self.flush_records(records);
        }
    }

    fn flush_records(&self, records: Vec<QuoteLogRecord>) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let filename = format!("quote_log_{timestamp}.parquet");
        let path = self.output_dir.join(filename);

        if let Err(e) = std::fs::create_dir_all(&self.output_dir) {
            error!(
                path = %self.output_dir.display(),
                error = %e,
                "cannot create output directory"
            );
            return;
        }

        match write_quote_log_parquet(&path, &records) {
            Ok(()) => {
                let mut last = self
                    .last_write
                    .lock()
                    .expect("last_write lock poisoned");
                *last = Instant::now();
                info!(
                    records = records.len(),
                    path = %path.display(),
                    "flushed quote log records"
                );
            }
            Err(e) => {
                error!(error = %e, "failed to write quote log parquet");
            }
        }
    }
}

impl SolverObserver for QuoteLogObserver {
    fn on_route_scored(&self, _route: &ObservedRoute, _score: f64, _rank: usize) {
        // Only on_quote_produced matters for the quote log.
    }

    fn on_quote_produced(&self, event: QuoteProducedEvent) {
        // Forward to resim channel (non-blocking to avoid stalling the solver).
        if let Err(e) = self.tx.try_send(event.clone()) {
            warn!(error = %e, "resim channel full or closed, dropping event");
        }

        let record = QuoteLogRecord::from(&event);
        let mut buf = self
            .buffer
            .lock()
            .expect("buffer lock poisoned");
        buf.push(record);

        if buf.len() >= self.flush_threshold {
            let records: Vec<_> = buf.drain(..).collect();
            drop(buf);
            self.flush_records(records);
        }
    }
}

/// Serialize an `ObservedRoute` to a JSON string for the parquet column.
pub fn route_to_json(route: &ObservedRoute) -> String {
    let swaps: Vec<serde_json::Value> = route
        .swaps
        .iter()
        .map(|s| {
            serde_json::json!({
                "component_id": s.component_id,
                "protocol": s.protocol,
                "token_in": format!("0x{}", hex::encode(&s.token_in)),
                "token_out": format!("0x{}", hex::encode(&s.token_out)),
                "amount_in": s.amount_in,
                "amount_out": s.amount_out,
                "gas_estimate": s.gas_estimate,
                "split": s.split,
            })
        })
        .collect();

    serde_json::json!({ "swaps": swaps }).to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use fynd_core::observer::{ObservedRoute, ObservedSwap, QuoteProducedEvent};
    use tycho_simulation::tycho_core::Bytes;

    use super::*;

    fn make_address(byte: u8) -> Bytes {
        Bytes::from([byte; 20].as_slice())
    }

    fn make_event(quote_id: &str) -> QuoteProducedEvent {
        QuoteProducedEvent {
            request_id: "req-1".into(),
            quote_id: quote_id.into(),
            solver_id: "solver-a".into(),
            is_winner: true,
            block_number: 100,
            chain_id: 1,
            route: ObservedRoute {
                swaps: vec![ObservedSwap {
                    component_id: "pool-1".into(),
                    protocol: "uniswap_v2".into(),
                    token_in: make_address(0x01),
                    token_out: make_address(0x02),
                    amount_in: "1000".into(),
                    amount_out: "990".into(),
                    gas_estimate: "50000".into(),
                    split: 0.0,
                }],
            },
            amount_in: "1000".into(),
            amount_out: "990".into(),
            gas_estimate: 100_000,
            calldata: vec![0xAB, 0xCD],
            algorithm_type: "most_liquid".into(),
            algorithm_settings: HashMap::new(),
            n_alternatives: 3,
            gap_to_second_best_bps: Some(10.0),
            score_dispersion: Some(0.5),
            slippage_tolerance: Some(0.005),
            all_candidates: vec![],
        }
    }

    #[test]
    fn observer_buffers_and_flushes_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 3);

        // Send 2 events — below threshold, no flush yet.
        observer.on_quote_produced(make_event("q-1"));
        observer.on_quote_produced(make_event("q-2"));

        let parquet_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();
        assert!(parquet_files.is_empty(), "should not flush before threshold");

        // Third event triggers flush.
        observer.on_quote_produced(make_event("q-3"));

        let parquet_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();
        assert_eq!(parquet_files.len(), 1, "should flush exactly once");
    }

    #[test]
    fn observer_forwards_events_to_channel() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(16);

        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 100);

        observer.on_quote_produced(make_event("q-fwd"));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.quote_id, "q-fwd");
    }

    #[test]
    fn flush_remaining_writes_buffered_records() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 100);

        observer.on_quote_produced(make_event("q-rem-1"));
        observer.on_quote_produced(make_event("q-rem-2"));

        // Not at threshold, but explicit flush should write.
        observer.flush_remaining();

        let parquet_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();
        assert_eq!(parquet_files.len(), 1);
    }

    #[test]
    fn flush_remaining_noop_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 100);
        observer.flush_remaining();

        let parquet_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();
        assert!(parquet_files.is_empty());
    }

    #[test]
    fn flush_if_stale_flushes_when_past_max_age() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 100);
        observer.on_quote_produced(make_event("q-stale-1"));
        let flushed = observer.flush_if_stale(Duration::ZERO);
        assert!(flushed, "should flush when max_age is zero");
        let parquet_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "parquet")
            })
            .collect();
        assert_eq!(parquet_files.len(), 1);
    }

    #[test]
    fn flush_if_stale_noop_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 100);
        observer.on_quote_produced(make_event("q-fresh-1"));
        let flushed = observer.flush_if_stale(Duration::from_secs(3600));
        assert!(!flushed, "should not flush when below max_age");
    }

    #[test]
    fn flush_if_stale_noop_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let observer = QuoteLogObserver::new(dir.path().to_path_buf(), tx, 100);
        let flushed = observer.flush_if_stale(Duration::ZERO);
        assert!(!flushed, "should not flush when buffer is empty");
    }

    #[test]
    fn route_to_json_produces_valid_json() {
        let route = ObservedRoute {
            swaps: vec![ObservedSwap {
                component_id: "pool-1".into(),
                protocol: "uniswap_v2".into(),
                token_in: make_address(0x01),
                token_out: make_address(0x02),
                amount_in: "1000".into(),
                amount_out: "990".into(),
                gas_estimate: "50000".into(),
                split: 0.5,
            }],
        };

        let json_str = route_to_json(&route);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let swaps = parsed["swaps"].as_array().unwrap();
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0]["component_id"], "pool-1");
        assert_eq!(swaps[0]["protocol"], "uniswap_v2");
        assert_eq!(swaps[0]["amount_in"], "1000");
    }
}
