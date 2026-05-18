//! Offline feature-collection dataset for predicting quote-to-execution drift.
//!
//! This crate implements feature extraction for the slippage-decay prediction
//! model described in ENG-5986. It processes historical Fynd quotes and computes
//! features across seven families (route, pool_state, token_pair, fynd_context,
//! chain_env, temporal, route_topology) with nullable extension points for v2
//! feature families (CEX, Dune, priors).

pub mod decay;
pub mod feature_vector;
pub mod features;
pub mod join;
pub mod quote_loader;
pub mod quote_record;
pub mod schema;

#[cfg(test)]
mod edge_cases;

pub use decay::{
    build_decay_from_replay_outcomes, build_replay_decay_array, compute_single_block_decay,
    DecayError, DecayResult, ReplayDecayArray, ReplayOutcome, RouteInvalidReason, MAX_BLOCK_OFFSET,
};
pub use join::{join_dataset, join_single, JoinDiagnostics, JoinResult, JoinedRecord};
pub use feature_vector::{
    assemble, CexDynamics, ChainEnv, ChainEnvInput, EnrichmentInput, FeatureVector, FyndAlgorithm,
    FyndAlgorithmInput, FyndContext, FyndContextInput, LabelDecay, OnchainFlow, PoolEnrichment,
    PoolState, PoolStateEntry, Priors, RouteTopology, Temporal, TokenPair, TokenPairInput,
    FIELD_ORDER,
};
pub use quote_loader::{
    load_records_from_path, load_records_from_reader, load_records_from_string, BatchResult,
    LoadError, RecordOutcome,
};
pub use quote_record::{
    parse_quote_record, parse_quote_records, BlockRecord, ParseError, QuoteRecord, RouteRecord,
    SwapRecord,
};
pub use schema::{validate_quote_record, SchemaViolation, ViolationKind};
