//! Hashflow's environment variables, book settings, and direct price polling.

use tycho_simulation::{
    book::all_pairs::QuoteRule,
    evm::tycho_models::Chain,
    rfq::protocols::hashflow::{
        feed::{HashflowFeed, HashflowFeedBuilder},
        HashflowBookAssembler, HashflowBookAssemblerBuilder,
    },
};

use super::{get_env, stream_error, usd_quote_tokens, BookVenueContext, BookVenueEntry};
use crate::feed::{protocol_registry::require_executor, DataFeedError};

/// Hashflow's credentials from the environment: `HASHFLOW_USER` and `HASHFLOW_KEY`.
pub(super) struct HashflowEnv {
    user: String,
    key: String,
}

impl HashflowEnv {
    /// Reads Hashflow's credentials.
    ///
    /// # Errors
    ///
    /// Returns [`DataFeedError::Config`] when `HASHFLOW_USER` or `HASHFLOW_KEY` is unset.
    pub(super) fn read() -> Result<Self, DataFeedError> {
        Ok(Self { user: get_env("HASHFLOW_USER")?, key: get_env("HASHFLOW_KEY")? })
    }
}

/// Rejects Hashflow on a chain where tycho-execution has no executor for it. Its feed builder
/// accepts any chain with a numeric id and defers venue support to its first poll, so a deployment
/// on another chain would publish books Fynd cannot encode.
pub(super) fn check_supported(chain: Chain, entry: &BookVenueEntry) -> Result<(), DataFeedError> {
    require_executor(chain, entry.name)
}

/// Builds Hashflow's book assembler from the tokens, TVL floor, USD stablecoins, the all-pairs
/// layout (one quote per maker) with `--aggregate-rfqs`, and the fallback router.
pub(super) fn assembler(
    context: &BookVenueContext,
    entry: &BookVenueEntry,
    env: &HashflowEnv,
) -> Result<HashflowBookAssembler, DataFeedError> {
    let mut builder = HashflowBookAssemblerBuilder::new(
        context.book_config.clone(),
        usd_quote_tokens(context)?,
        env.user.clone(),
        env.key.clone(),
    );
    if context.aggregate_rfqs {
        builder = builder.all_pairs(QuoteRule::OncePerMaker);
    }
    if entry.uses_fallback_router {
        builder = builder.with_fallback_router();
    }
    builder.build().map_err(stream_error)
}

/// Polls Hashflow directly and uses `assembler` to build its books.
pub(super) fn direct_feed(
    context: &BookVenueContext,
    env: &HashflowEnv,
    assembler: HashflowBookAssembler,
) -> Result<HashflowFeed, DataFeedError> {
    HashflowFeedBuilder::new(
        context.book_config.clone(),
        usd_quote_tokens(context)?,
        env.user.clone(),
        env.key.clone(),
    )
    .with_assembler(assembler)
    .build()
    .map_err(stream_error)
}
