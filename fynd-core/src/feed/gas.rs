use std::time::Duration;

use chrono::Utc;
use metrics::{counter, gauge};
use tokio::time::{interval, MissedTickBehavior};
use tracing::warn;
use tycho_simulation::{tycho_core::traits::FeePriceGetter, tycho_ethereum::gas::BlockGasPrice};

use crate::feed::market_data::MarketData;

// TODO: Refactor gas price fetching into a `DerivedComputation`.
/// Periodically fetches the latest block gas price from an RPC client and writes it
/// into the shared market data.
pub struct GasPriceFetcher<C: FeePriceGetter<FeePrice = BlockGasPrice>> {
    client: C,
    refresh_interval: Duration,
    shared_market_data: MarketData,
}

impl<C: FeePriceGetter<FeePrice = BlockGasPrice>> GasPriceFetcher<C> {
    /// Creates a new fetcher that refreshes gas prices at the given interval.
    pub fn new(
        client: C,
        shared_market_data: MarketData,
        refresh_interval: Duration,
    ) -> Self {
        Self { client, refresh_interval, shared_market_data }
    }

    /// Runs the refresh loop indefinitely; intended to be spawned in a dedicated task.
    pub async fn run(&mut self) {
        let mut ticker = interval(self.refresh_interval);
        // Skip missed ticks rather than catching up — fetches are best-effort.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            let fee_price = match self.client.get_latest_fee_price().await {
                Ok(price) => price,
                Err(e) => {
                    counter!("gas_price_fetch_failures_total").increment(1);
                    warn!(error = ?e, "Failed to fetch gas price, skipping update. Configure --gas-price-stale-threshold-secs to surface this in health checks");
                    continue;
                }
            };

            let mut lock = self.shared_market_data.write().await;
            let update_lag_ms =
                Utc::now().timestamp_millis() - (fee_price.block_timestamp as i64 * 1000);
            gauge!("gas_price_update_lag_ms").set(update_lag_ms as f64);
            if update_lag_ms > 60_000 {
                warn!(
                    lag_ms = update_lag_ms,
                    "gas price is more than 60s stale; RPC node may be behind"
                );
            }
            lock.update_gas_price(fee_price);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;
    use num_bigint::BigUint;
    use tycho_simulation::tycho_ethereum::gas::{BlockGasPrice, GasPrice};

    use super::*;
    use crate::feed::market_data::{MarketData, MarketDataView};

    /// Mock client that fails on the first `fail_count` calls then succeeds.
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

    /// Polls `predicate` against market data until it returns true or `timeout` elapses.
    async fn wait_for(
        market_data: &MarketData,
        timeout: Duration,
        predicate: impl Fn(&MarketDataView<'_>) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if predicate(&market_data.read().await) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for gas price");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn fetch_error_does_not_crash() {
        let market_data = MarketData::new_shared();
        let mut fetcher = GasPriceFetcher::new(
            MockFeePriceGetter::new(1),
            market_data.clone(),
            Duration::from_millis(5),
        );

        let handle = tokio::spawn(async move { fetcher.run().await });

        // First tick fails; second succeeds. Gas price is eventually set.
        wait_for(&market_data, Duration::from_secs(2), |m| m.gas_price().is_some()).await;

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn persistent_failure_keeps_loop_alive() {
        let market_data = MarketData::new_shared();
        let mut fetcher = GasPriceFetcher::new(
            MockFeePriceGetter::new(3),
            market_data.clone(),
            Duration::from_millis(5),
        );

        let handle = tokio::spawn(async move { fetcher.run().await });

        // First 3 ticks fail; 4th succeeds. Gas price is eventually set despite failures.
        wait_for(&market_data, Duration::from_secs(2), |m| m.gas_price().is_some()).await;

        handle.abort();
        let _ = handle.await;
    }
}
