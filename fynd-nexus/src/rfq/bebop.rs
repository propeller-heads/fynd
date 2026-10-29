//! Bebop's prices feed: its API key and the chains its pricing client accepts.

use std::fmt;

use tycho_simulation::{
    evm::tycho_models::Chain,
    rfq::protocols::bebop::feed::{BebopPricesFeed, BebopPricesFeedBuilder},
    snapshot_feed::errors::FeedError,
};

/// Bebop's settings. Its `Debug` output says whether the key is set and never shows it.
#[derive(clap::Args, Clone, Default)]
pub struct BebopArgs {
    /// Bebop API key.
    #[arg(id = "bebop_key", long = "bebop-key", env = "BEBOP_KEY", hide_env_values = true)]
    pub key: Option<String>,
}

impl fmt::Debug for BebopArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BebopArgs")
            .field("key", &self.key.is_some())
            .finish()
    }
}

/// Bebop's credentials, once set.
#[derive(Clone)]
pub struct BebopCredentials {
    key: String,
}

impl BebopArgs {
    /// Bebop's credentials.
    ///
    /// # Errors
    ///
    /// Returns `Err("BEBOP_KEY")` if the API key is missing.
    pub fn credentials(&self) -> Result<BebopCredentials, &'static str> {
        let key = self.key.clone().ok_or("BEBOP_KEY")?;
        Ok(BebopCredentials { key })
    }
}

/// Whether Bebop supports `chain`: the chains its pricing client accepts.
pub fn supports_chain(chain: Chain) -> bool {
    BebopPricesFeedBuilder::new(chain, String::new())
        .build()
        .is_ok()
}

/// Bebop's prices feed on `chain`.
///
/// # Errors
///
/// Fails when the feed cannot be built.
pub fn prices_feed(
    chain: Chain,
    credentials: &BebopCredentials,
) -> Result<BebopPricesFeed, FeedError> {
    BebopPricesFeedBuilder::new(chain, credentials.key.clone()).build()
}
