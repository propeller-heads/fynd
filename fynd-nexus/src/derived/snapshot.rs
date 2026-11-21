//! The derived data of each default computation, kept in memory for the API to serve.

use fynd_core::{
    derived::types::{ComponentDepths, SpotPrices, TokenGasPrices},
    nexus::derived::{DerivedCursor, DerivedKind},
};
use metrics::Label;
use tokio::task::JoinError;
use tycho_simulation::evm::tycho_models::Chain;

use crate::derived::{history::DerivedHistory, models::DerivedAnswer};

/// The history of each derived computation.
pub struct DerivedSnapshots {
    chain: Chain,
    spot_prices: DerivedHistory<SpotPrices>,
    pool_depths: DerivedHistory<ComponentDepths>,
    token_prices: DerivedHistory<TokenGasPrices>,
}

impl DerivedSnapshots {
    /// An empty store for the service of `chain`, in the run that started at `run_id`.
    pub fn new(chain: Chain, run_id: u64) -> Self {
        Self {
            chain,
            spot_prices: DerivedHistory::new(run_id, labels(chain, DerivedKind::SpotPrices)),
            pool_depths: DerivedHistory::new(run_id, labels(chain, DerivedKind::PoolDepths)),
            token_prices: DerivedHistory::new(run_id, labels(chain, DerivedKind::TokenPrices)),
        }
    }

    /// The `chain` and `computation` label values every metric of `kind` carries.
    pub fn labels(&self, kind: DerivedKind) -> Vec<Label> {
        labels(self.chain, kind)
    }

    /// The history of the spot prices.
    pub fn spot_prices(&self) -> &DerivedHistory<SpotPrices> {
        &self.spot_prices
    }

    /// The history of the pool depths.
    pub fn pool_depths(&self) -> &DerivedHistory<ComponentDepths> {
        &self.pool_depths
    }

    /// The history of the token prices.
    pub fn token_prices(&self) -> &DerivedHistory<TokenGasPrices> {
        &self.token_prices
    }

    /// The block of the latest publish of `kind`, or `None` while there is none.
    pub fn block(&self, kind: DerivedKind) -> Option<u64> {
        match kind {
            DerivedKind::SpotPrices => self.spot_prices.block(),
            DerivedKind::PoolDepths => self.pool_depths.block(),
            DerivedKind::TokenPrices => self.token_prices.block(),
        }
    }

    /// The answer to a reader of `kind`. See [`DerivedHistory::answer`].
    ///
    /// # Errors
    ///
    /// Fails when the task that encodes a full update does not finish.
    pub async fn answer(
        &self,
        kind: DerivedKind,
        since: Option<DerivedCursor>,
        if_none_match: Option<DerivedCursor>,
    ) -> Result<DerivedAnswer, JoinError> {
        match kind {
            DerivedKind::SpotPrices => {
                self.spot_prices
                    .answer(since, if_none_match)
                    .await
            }
            DerivedKind::PoolDepths => {
                self.pool_depths
                    .answer(since, if_none_match)
                    .await
            }
            DerivedKind::TokenPrices => {
                self.token_prices
                    .answer(since, if_none_match)
                    .await
            }
        }
    }
}

fn labels(chain: Chain, kind: DerivedKind) -> Vec<Label> {
    vec![Label::new("chain", chain.to_string()), Label::new("computation", kind.name())]
}
