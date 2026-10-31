//! Bebop's environment variables, book settings, and direct pricing connection.

use std::str::FromStr;

use alloy::primitives::Address;
use tycho_simulation::{
    evm::tycho_models::Chain,
    rfq::protocols::bebop::{
        feed::{BebopFeed, BebopFeedBuilder},
        BebopBookAssembler, BebopBookAssemblerBuilder,
    },
    tycho_core::Bytes,
};

use super::{get_env, stream_error, usd_quote_tokens, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// Bebop's settings from the environment: `BEBOP_KEY`, and the request origin from
/// `BEBOP_ORIGIN_ADDRESS`, `BEBOP_ORIGIN_TARGET` and `BEBOP_ORIGIN_SOURCE`.
pub(super) struct BebopEnv {
    key: String,
    origin_address: Option<Bytes>,
    origin_target: Option<Bytes>,
    origin_source: Option<String>,
}

impl BebopEnv {
    /// Reads Bebop's settings.
    ///
    /// # Errors
    ///
    /// [`DataFeedError::Config`] when `BEBOP_KEY` is unset, or when an origin address is set but
    /// is not an address.
    pub(super) fn read() -> Result<Self, DataFeedError> {
        Ok(Self {
            key: get_env("BEBOP_KEY")?,
            origin_address: optional_address_env("BEBOP_ORIGIN_ADDRESS")?,
            origin_target: optional_address_env("BEBOP_ORIGIN_TARGET")?,
            origin_source: std::env::var("BEBOP_ORIGIN_SOURCE").ok(),
        })
    }
}

/// Rejects Bebop on a chain where tycho-execution has no executor for it. The feed supports more
/// chains than that, so a deployment on another chain would publish books Fynd cannot encode.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    require_executor(chain, entry.name)
}

/// Builds Bebop's book assembler from the tokens, TVL floor, USD stablecoins, the all-pairs layout
/// with `--aggregate-rfqs`, the fallback router, and the request origin.
pub(super) fn assembler(
    context: &BookVenueContext,
    entry: &BookVenueEntry,
    env: &BebopEnv,
) -> Result<BebopBookAssembler, DataFeedError> {
    let mut builder = BebopBookAssemblerBuilder::new(
        context.book_config.clone(),
        usd_quote_tokens(context)?,
        env.key.clone(),
    );
    if let Some(address) = &env.origin_address {
        builder = builder.origin_address(address.clone());
    }
    if let Some(target) = &env.origin_target {
        builder = builder.origin_target(target.clone());
    }
    if let Some(source) = &env.origin_source {
        builder = builder.origin_source(source.clone());
    }
    if context.aggregate_rfqs {
        builder = builder.all_pairs();
    }
    if entry.uses_fallback_router {
        builder = builder.with_fallback_router();
    }
    builder.build().map_err(stream_error)
}

/// Connects directly to Bebop and uses `assembler` to build its books.
pub(super) fn direct_feed(
    context: &BookVenueContext,
    env: &BebopEnv,
    assembler: BebopBookAssembler,
) -> Result<BebopFeed, DataFeedError> {
    BebopFeedBuilder::new(context.book_config.clone(), usd_quote_tokens(context)?, env.key.clone())
        .with_assembler(assembler)
        .build()
        .map_err(stream_error)
}

/// The address `var` holds, or `None` when it is unset.
///
/// A set-but-unparseable value is a configuration error rather than an absent address: Bebop
/// identifies the caller by these, so silently quoting as nobody is worse than refusing to start.
fn optional_address_env(var: &str) -> Result<Option<Bytes>, DataFeedError> {
    let Ok(value) = std::env::var(var) else {
        return Ok(None);
    };
    Address::from_str(value.trim())
        .map(|address| Some(Bytes::from(address.into_array())))
        .map_err(|e| DataFeedError::Config(format!("{var} is not an address: {e}")))
}
