use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use tycho_simulation::{tycho_core::traits::FeePriceGetter, tycho_ethereum::gas::GasPrice};

use crate::{DataFeedError, SharedMarketDataRef};

pub struct GasPriceFetcher<C: FeePriceGetter<FeePrice = GasPrice>> {
    client: C,
    signal_rx: mpsc::Receiver<oneshot::Sender<()>>,
    shared_market_data: SharedMarketDataRef,
}

impl<C: FeePriceGetter<FeePrice = GasPrice>> GasPriceFetcher<C> {
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

            let fee_price = self
                .client
                .get_latest_fee_price()
                .await
                .unwrap();

            self.shared_market_data
                .write()
                .await
                .update_gas_price(fee_price);

            // Warn if the update notification is not correctly sent.
            if update_tx.send(()).is_err() {
                warn!("Failed to send update notification");
            }
        }
    }
}
