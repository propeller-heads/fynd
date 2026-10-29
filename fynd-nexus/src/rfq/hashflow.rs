//! Hashflow's prices feed: its taker API credentials. Hashflow is tried on every chain.

use std::fmt;

use tycho_simulation::{
    evm::tycho_models::Chain,
    rfq::protocols::hashflow::feed::{HashflowPricesFeed, HashflowPricesFeedBuilder},
    snapshot_feed::errors::FeedError,
};

/// Hashflow's settings. Its `Debug` output says which values are set and never shows them.
#[derive(clap::Args, Clone, Default)]
pub struct HashflowArgs {
    /// Hashflow taker API user name.
    #[arg(
        id = "hashflow_user",
        long = "hashflow-user",
        env = "HASHFLOW_USER",
        hide_env_values = true
    )]
    pub user: Option<String>,

    /// Hashflow taker API key.
    #[arg(
        id = "hashflow_key",
        long = "hashflow-key",
        env = "HASHFLOW_KEY",
        hide_env_values = true
    )]
    pub key: Option<String>,
}

impl fmt::Debug for HashflowArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashflowArgs")
            .field("user", &self.user.is_some())
            .field("key", &self.key.is_some())
            .finish()
    }
}

/// Hashflow's credentials, once set.
#[derive(Clone)]
pub struct HashflowCredentials {
    user: String,
    key: String,
}

impl HashflowArgs {
    /// Hashflow's credentials.
    ///
    /// # Errors
    ///
    /// Returns `Err("HASHFLOW_USER and HASHFLOW_KEY")` if either credential is missing.
    pub fn credentials(&self) -> Result<HashflowCredentials, &'static str> {
        let (Some(user), Some(key)) = (self.user.clone(), self.key.clone()) else {
            return Err("HASHFLOW_USER and HASHFLOW_KEY");
        };
        Ok(HashflowCredentials { user, key })
    }
}

/// Whether Hashflow supports `chain`: always, because it reports whether it prices a chain on its
/// first poll.
pub fn supports_chain(_chain: Chain) -> bool {
    true
}

/// Hashflow's prices feed on `chain`.
///
/// # Errors
///
/// Fails when the feed cannot be built.
pub fn prices_feed(
    chain: Chain,
    credentials: &HashflowCredentials,
) -> Result<HashflowPricesFeed, FeedError> {
    HashflowPricesFeedBuilder::new(chain, credentials.user.clone(), credentials.key.clone()).build()
}
