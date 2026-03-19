use chrono::Utc;
use metrics::{counter, gauge};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use tycho_simulation::{tycho_core::traits::FeePriceGetter, tycho_ethereum::gas::BlockGasPrice};

use crate::feed::{market_data::SharedMarketDataRef, DataFeedError};

// TODO: Refactor gas price fetching into a `DerivedComputation`.
pub const GAS_PRICE_DEPENDENCY_ID: &str = "gas_price";

pub(crate) struct GasPriceFetcher<C: FeePriceGetter<FeePrice = BlockGasPrice>> {
    client: C,
    signal_rx: mpsc::Receiver<oneshot::Sender<()>>,
    shared_market_data: SharedMarketDataRef,
}

impl<C: FeePriceGetter<FeePrice = BlockGasPrice>> GasPriceFetcher<C> {
    pub(crate) fn new(
        client: C,
        shared_market_data: SharedMarketDataRef,
    ) -> (Self, mpsc::Sender<oneshot::Sender<()>>) {
        let (signal_tx, signal_rx) = mpsc::channel(5);
        (Self { client, signal_rx, shared_market_data }, signal_tx)
    }

    pub(crate) async fn run(&mut self) -> Result<(), DataFeedError> {
        loop {
            let update_tx = self
                .signal_rx
                .recv()
                .await
                .ok_or(DataFeedError::GasPriceFetcherError("Trigger channel closed".to_string()))?;

            let fee_price = match self.client.get_latest_fee_price().await {
                Ok(price) => price,
                Err(e) => {
                    counter!("gas_price_fetch_failures_total").increment(1);
                    warn!(error = ?e, "Failed to fetch gas price, skipping update. Configure --gas-price-stale-threshold-secs to surface this in health checks");
                    if update_tx.send(()).is_err() {
                        warn!("Failed to send update notification");
                    }
                    continue;
                }
            };

            {
                let mut lock = self.shared_market_data.write().await;
                let update_block_number = fee_price.block_number;
                lock.update_gas_price(fee_price);
                if let Some(last_block_info) = lock.last_updated() {
                    let update_lag_ms =
                        Utc::now().timestamp_millis() - (last_block_info.timestamp() as i64 * 1000);
                    gauge!("gas_price_update_lag_ms").set(update_lag_ms as f64);

                    if last_block_info
                        .number()
                        .abs_diff(update_block_number) >
                        3
                    {
                        warn!("Gas price update is out of sync with the last block info. Gas price: {}, Last block info: {}", update_block_number, last_block_info.number());
                    }
                }
            }

            // Warn if the update notification is not correctly sent.
            if update_tx.send(()).is_err() {
                warn!("Failed to send update notification");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use async_trait::async_trait;
    use num_bigint::BigUint;
    use tokio::sync::{oneshot, RwLock};
    use tycho_simulation::tycho_ethereum::gas::{BlockGasPrice, GasPrice};

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    /// Mock client that returns errors for the first `fail_count` calls,
    /// then succeeds with a fixed gas price.
    struct MockFeePriceGetter {
        call_count: AtomicUsize,
        fail_count: usize,
    }

    impl MockFeePriceGetter {
        fn new(fail_count: usize) -> Self {
            Self { call_count: AtomicUsize::new(0), fail_count }
        }
    }

    #[async_trait]
    impl FeePriceGetter for MockFeePriceGetter {
        type Error = String;
        type FeePrice = BlockGasPrice;

        async fn get_latest_fee_price(&self) -> Result<BlockGasPrice, String> {
            let call = self
                .call_count
                .fetch_add(1, Ordering::SeqCst);
            if call < self.fail_count {
                return Err(format!("RPC timeout (call {})", call));
            }
            Ok(BlockGasPrice {
                block_number: 100 + call as u64,
                block_hash: Default::default(),
                block_timestamp: 1_700_000_000,
                pricing: GasPrice::Legacy { gas_price: BigUint::from(30_000_000_000u64) },
            })
        }
    }

    fn shared_market_data() -> SharedMarketDataRef {
        Arc::new(RwLock::new(SharedMarketData::new()))
    }

    /// Helper: send one signal and wait for the ack (with timeout).
    async fn trigger_and_await_ack(
        signal_tx: &mpsc::Sender<oneshot::Sender<()>>,
    ) -> Result<(), &'static str> {
        let (ack_tx, ack_rx) = oneshot::channel();
        signal_tx
            .send(ack_tx)
            .await
            .map_err(|_| "signal_tx send failed")?;
        tokio::time::timeout(std::time::Duration::from_secs(2), ack_rx)
            .await
            .map_err(|_| "ack timed out")?
            .map_err(|_| "ack channel dropped")
    }

    #[tokio::test]
    async fn fetch_error_does_not_crash_and_acks_oneshot() {
        let market_data = shared_market_data();
        let (mut fetcher, signal_tx) =
            GasPriceFetcher::new(MockFeePriceGetter::new(1), Arc::clone(&market_data));

        let handle = tokio::spawn(async move { fetcher.run().await });

        // First signal → mock returns error → should NOT panic, should ack
        trigger_and_await_ack(&signal_tx)
            .await
            .expect("ack should be received even on fetch error");

        // Gas price should still be None (error path skips update)
        assert!(
            market_data
                .read()
                .await
                .gas_price()
                .is_none(),
            "gas price should remain None after failed fetch"
        );

        // Second signal → mock succeeds → gas price should be updated
        trigger_and_await_ack(&signal_tx)
            .await
            .expect("ack should be received on successful fetch");

        assert!(
            market_data
                .read()
                .await
                .gas_price()
                .is_some(),
            "gas price should be set after successful fetch"
        );

        // Clean up: drop signal_tx so the loop exits
        drop(signal_tx);
        let result = handle
            .await
            .expect("task should not panic");
        assert!(result.is_err(), "run() should return Err when signal channel closes");
    }

    #[tokio::test]
    async fn persistent_failure_keeps_loop_alive() {
        let market_data = shared_market_data();
        // Fail 3 times, then succeed
        let (mut fetcher, signal_tx) =
            GasPriceFetcher::new(MockFeePriceGetter::new(3), Arc::clone(&market_data));

        let handle = tokio::spawn(async move { fetcher.run().await });

        // All 3 failures should ack without crashing
        for i in 0..3 {
            trigger_and_await_ack(&signal_tx)
                .await
                .unwrap_or_else(|e| panic!("ack failed on failure iteration {i}: {e}"));

            assert!(
                market_data
                    .read()
                    .await
                    .gas_price()
                    .is_none(),
                "gas price should remain None during persistent failure"
            );
        }

        // 4th call succeeds — solver recovers
        trigger_and_await_ack(&signal_tx)
            .await
            .expect("ack should succeed after recovery");

        let gas = market_data
            .read()
            .await
            .gas_price()
            .cloned();
        assert!(gas.is_some(), "gas price should be set after recovery");

        drop(signal_tx);
        let _ = handle
            .await
            .expect("task should not panic");
    }
}
