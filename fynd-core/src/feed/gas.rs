use chrono::Utc;
use metrics::{counter, gauge};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use tycho_simulation::{tycho_core::traits::FeePriceGetter, tycho_ethereum::gas::BlockGasPrice};

use crate::feed::{market_data::SharedMarketDataRef, DataFeedError};

// TODO: Refactor gas price fetching into a `DerivedComputation`.
pub const GAS_PRICE_DEPENDENCY_ID: &str = "gas_price";

pub struct GasPriceFetcher<C: FeePriceGetter<FeePrice = BlockGasPrice>> {
    client: C,
    signal_rx: mpsc::Receiver<oneshot::Sender<()>>,
    shared_market_data: SharedMarketDataRef,
}

impl<C: FeePriceGetter<FeePrice = BlockGasPrice>> GasPriceFetcher<C> {
    pub fn new(
        client: C,
        shared_market_data: SharedMarketDataRef,
    ) -> (Self, mpsc::Sender<oneshot::Sender<()>>) {
        let (signal_tx, signal_rx) = mpsc::channel(5);
        (Self { client, signal_rx, shared_market_data }, signal_tx)
    }

    pub async fn run(&mut self) -> Result<(), DataFeedError> {
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
                    warn!(error = ?e, "Failed to fetch gas price, skipping update");
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
