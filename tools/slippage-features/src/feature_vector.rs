//! Feature vector assembly: combines individual feature outputs into a single
//! record ready for offline analysis.
//!
//! The [`assemble`] function is the main entry point. It takes a validated
//! [`QuoteRecord`] plus optional enrichment data and produces a deterministic
//! [`FeatureVector`] whose field order matches the ontology families.

use serde::{Deserialize, Serialize};

use crate::{features, QuoteRecord};

// ═══════════════════════════════════════════════════════════════════════
// v1 feature family structs
// ═══════════════════════════════════════════════════════════════════════

/// Route topology features derived from the swap path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteTopology {
    pub hop_count: u32,
    pub split_count: u32,
    pub gas_estimate: Option<f64>,
    pub pool_type_diversity: Option<f64>,
}

/// Temporal features derived from the block timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Temporal {
    pub hour_of_day: Option<u32>,
    pub day_of_week: Option<u32>,
    pub minutes_since_hour: Option<u32>,
}

/// Chain environment features at quote time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainEnv {
    pub chain_id: u64,
    pub is_l2: bool,
    pub base_fee_log: Option<f64>,
    pub block_utilization: Option<f64>,
    pub priority_gas_p50: Option<f64>,
    pub priority_gas_p95: Option<f64>,
    pub sequencer_lag: Option<f64>,
}

/// Per-hop pool state snapshot. One entry per swap in the route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolStateEntry {
    pub component_id: String,
    pub protocol: String,
    pub fee_tier: Option<f64>,
    pub tvl_usd_log: Option<f64>,
    pub reserve_imbalance: Option<f64>,
}

/// Aggregated pool state across all hops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolState {
    pub pools: Vec<PoolStateEntry>,
}

/// Token pair features derived from quote amounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPair {
    pub log_amount_ratio: Option<f64>,
    pub gas_share_of_trade: Option<f64>,
    pub log_mcap_ratio: Option<f64>,
    pub min_mcap: Option<f64>,
    pub max_mcap: Option<f64>,
}

/// Fynd solver context features.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FyndContext {
    pub n_alternatives: Option<u32>,
    pub gap_to_second_best_bps: Option<f64>,
    pub score_dispersion: Option<f64>,
    pub quoted_amount_in: String,
    pub quoted_amount_out: String,
    pub requested_slippage_tolerance: Option<f64>,
}

/// Fynd algorithm type and settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FyndAlgorithm {
    pub algorithm_type: Option<String>,
    pub max_hops: Option<u32>,
    pub max_splits: Option<u32>,
}

// ═══════════════════════════════════════════════════════════════════════
// v2 extension point structs (nullable in v1)
// ═══════════════════════════════════════════════════════════════════════

/// CEX market dynamics (v2 — always `None` in v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CexDynamics {
    pub realized_vol: Option<f64>,
    pub skew: Option<f64>,
    pub kurtosis: Option<f64>,
    pub cex_dex_spread: Option<f64>,
    pub short_term_trend: Option<f64>,
}

/// On-chain flow metrics (v2 — always `None` in v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnchainFlow {
    pub pool_ofi: Option<f64>,
    pub vwap_deviation: Option<f64>,
    pub swap_count: Option<u64>,
    pub aggregator_share: Option<f64>,
}

/// Historical priors (v2 — always `None` in v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Priors {
    pub route_class_decay_mean: Option<f64>,
    pub pool_decay_sensitivity: Option<f64>,
}

/// Replay-based decay labels for k in {1..10}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelDecay {
    pub decay_bps: [Option<f64>; 10],
}

// ═══════════════════════════════════════════════════════════════════════
// Complete feature vector
// ═══════════════════════════════════════════════════════════════════════

/// Complete feature record for one quote, combining all families.
///
/// Field order follows the ontology: identity → v1 families → v2 extensions
/// → labels. Serialization preserves this order via `serde(rename_all)` on
/// each sub-struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureVector {
    // Identity
    pub quote_id: String,
    pub block_number: u64,
    pub chain_id: u64,

    // v1 feature families (always populated)
    pub route_topology: RouteTopology,
    pub temporal: Temporal,
    pub chain_env: ChainEnv,
    pub pool_state: PoolState,
    pub token_pair: TokenPair,
    pub fynd_context: FyndContext,
    pub fynd_algorithm: FyndAlgorithm,

    // v2 extension points (nullable — None in v1)
    pub cex_dynamics: Option<CexDynamics>,
    pub onchain_flow: Option<OnchainFlow>,
    pub priors: Option<Priors>,

    // Labels
    pub label_decay: Option<LabelDecay>,
}

// ═══════════════════════════════════════════════════════════════════════
// Optional enrichment inputs
// ═══════════════════════════════════════════════════════════════════════

/// Optional chain environment data not present in the quote record itself.
#[derive(Debug, Clone, Default)]
pub struct ChainEnvInput {
    pub base_fee_gwei: Option<f64>,
    pub gas_used: Option<u64>,
    pub gas_limit: Option<u64>,
    pub priority_gas_p50: Option<f64>,
    pub priority_gas_p95: Option<f64>,
    pub sequencer_lag: Option<f64>,
}

/// Optional per-pool enrichment data keyed by component_id.
#[derive(Debug, Clone, Default)]
pub struct PoolEnrichment {
    pub fee_bps: Option<u32>,
    pub tvl_usd: Option<f64>,
    pub reserve_a: Option<f64>,
    pub reserve_b: Option<f64>,
}

/// Optional token pair enrichment (CoinGecko-sourced in v1).
#[derive(Debug, Clone, Default)]
pub struct TokenPairInput {
    pub log_mcap_ratio: Option<f64>,
    pub min_mcap: Option<f64>,
    pub max_mcap: Option<f64>,
}

/// Optional Fynd solver context not in the base quote record.
#[derive(Debug, Clone, Default)]
pub struct FyndContextInput {
    pub n_alternatives: Option<u32>,
    pub gap_to_second_best_bps: Option<f64>,
    pub score_dispersion: Option<f64>,
    pub requested_slippage_tolerance: Option<f64>,
}

/// Optional Fynd algorithm metadata.
#[derive(Debug, Clone, Default)]
pub struct FyndAlgorithmInput {
    pub algorithm_type: Option<String>,
    pub max_hops: Option<u32>,
    pub max_splits: Option<u32>,
}

/// All optional enrichment data bundled for assembly.
#[derive(Debug, Clone, Default)]
pub struct EnrichmentInput {
    pub chain_env: ChainEnvInput,
    /// Pool enrichment keyed by `component_id`. Missing entries yield
    /// `None` for that pool's enriched fields.
    pub pools: Vec<(String, PoolEnrichment)>,
    pub token_pair: TokenPairInput,
    pub fynd_context: FyndContextInput,
    pub fynd_algorithm: FyndAlgorithmInput,
    pub label_decay_bps: Option<[Option<f64>; 10]>,
}

// ═══════════════════════════════════════════════════════════════════════
// Assembly
// ═══════════════════════════════════════════════════════════════════════

/// Assemble a complete [`FeatureVector`] from a validated quote record and
/// optional enrichment data.
///
/// This function is **deterministic**: given identical `record` and
/// `enrichment` inputs, it always produces the same output. All feature
/// computations are pure functions with no external state.
///
/// Fields that cannot be computed from the available inputs are set to
/// `None`. v2 extension families (`cex_dynamics`, `onchain_flow`, `priors`)
/// are always `None` in v1.
pub fn assemble(record: &QuoteRecord, enrichment: &EnrichmentInput) -> FeatureVector {
    let route_topology = build_route_topology(record);
    let temporal = build_temporal(record);
    let chain_env = build_chain_env(record, &enrichment.chain_env);
    let pool_state = build_pool_state(record, &enrichment.pools);
    let token_pair = build_token_pair(record, &enrichment.token_pair);
    let fynd_context = build_fynd_context(record, &enrichment.fynd_context);
    let fynd_algorithm = build_fynd_algorithm(&enrichment.fynd_algorithm);
    let label_decay = enrichment
        .label_decay_bps
        .map(|decay_bps| LabelDecay { decay_bps });

    FeatureVector {
        quote_id: record.quote_id.clone(),
        block_number: record.block.number,
        chain_id: record.chain_id,
        route_topology,
        temporal,
        chain_env,
        pool_state,
        token_pair,
        fynd_context,
        fynd_algorithm,
        cex_dynamics: None,
        onchain_flow: None,
        priors: None,
        label_decay,
    }
}

fn build_route_topology(record: &QuoteRecord) -> RouteTopology {
    RouteTopology {
        hop_count: features::hop_count(&record.route.swaps),
        split_count: features::split_count(&record.route.swaps),
        gas_estimate: features::gas_estimate_f64(&record.gas_estimate),
        pool_type_diversity: features::pool_type_diversity(&record.route.swaps),
    }
}

fn build_temporal(record: &QuoteRecord) -> Temporal {
    let ts = record.block.timestamp;
    Temporal {
        hour_of_day: features::hour_of_day(ts),
        day_of_week: features::day_of_week(ts),
        minutes_since_hour: features::minutes_since_hour(ts),
    }
}

fn build_chain_env(record: &QuoteRecord, input: &ChainEnvInput) -> ChainEnv {
    ChainEnv {
        chain_id: record.chain_id,
        is_l2: features::is_l2(record.chain_id),
        base_fee_log: features::gas_price_feature(input.base_fee_gwei),
        block_utilization: features::block_utilization_feature(input.gas_used, input.gas_limit),
        priority_gas_p50: input.priority_gas_p50,
        priority_gas_p95: input.priority_gas_p95,
        sequencer_lag: input.sequencer_lag,
    }
}

fn build_pool_state(
    record: &QuoteRecord,
    pool_enrichments: &[(String, PoolEnrichment)],
) -> PoolState {
    let pools = record
        .route
        .swaps
        .iter()
        .map(|swap| {
            let enrichment = pool_enrichments
                .iter()
                .find(|(id, _)| *id == swap.component_id)
                .map(|(_, e)| e);

            let (fee_tier, tvl_usd_log, reserve_imbalance) = match enrichment {
                Some(e) => (
                    features::fee_tier_feature(e.fee_bps),
                    features::pool_liquidity_feature(e.tvl_usd),
                    features::reserve_imbalance_ratio(e.reserve_a, e.reserve_b),
                ),
                None => (None, None, None),
            };

            PoolStateEntry {
                component_id: swap.component_id.clone(),
                protocol: swap.protocol.clone(),
                fee_tier,
                tvl_usd_log,
                reserve_imbalance,
            }
        })
        .collect();

    PoolState { pools }
}

fn build_token_pair(record: &QuoteRecord, input: &TokenPairInput) -> TokenPair {
    TokenPair {
        log_amount_ratio: features::log_amount_ratio(&record.amount_in, &record.amount_out),
        gas_share_of_trade: features::gas_share_of_trade(&record.gas_estimate, &record.amount_in),
        log_mcap_ratio: input.log_mcap_ratio,
        min_mcap: input.min_mcap,
        max_mcap: input.max_mcap,
    }
}

fn build_fynd_context(record: &QuoteRecord, input: &FyndContextInput) -> FyndContext {
    FyndContext {
        n_alternatives: input.n_alternatives,
        gap_to_second_best_bps: input.gap_to_second_best_bps,
        score_dispersion: input.score_dispersion,
        quoted_amount_in: record.amount_in.clone(),
        quoted_amount_out: record.amount_out.clone(),
        requested_slippage_tolerance: input.requested_slippage_tolerance,
    }
}

fn build_fynd_algorithm(input: &FyndAlgorithmInput) -> FyndAlgorithm {
    FyndAlgorithm {
        algorithm_type: input.algorithm_type.clone(),
        max_hops: input.max_hops,
        max_splits: input.max_splits,
    }
}

/// Ordered list of field names as they appear in `FeatureVector` serialization.
///
/// Useful for verifying column ordering in downstream consumers (notebooks,
/// Parquet writers, etc.).
pub const FIELD_ORDER: &[&str] = &[
    "quote_id",
    "block_number",
    "chain_id",
    "route_topology",
    "temporal",
    "chain_env",
    "pool_state",
    "token_pair",
    "fynd_context",
    "fynd_algorithm",
    "cex_dynamics",
    "onchain_flow",
    "priors",
    "label_decay",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockRecord, QuoteRecord, RouteRecord, SwapRecord};

    /// Build a deterministic single-hop Ethereum quote record.
    fn eth_record() -> QuoteRecord {
        QuoteRecord {
            quote_id: "test-quote-001".to_owned(),
            chain_id: 1,
            block: BlockRecord {
                number: 21_000_000,
                hash: "0xabcdef1234567890abcdef1234567890\
                       abcdef1234567890abcdef1234567890"
                    .to_owned(),
                timestamp: 1_730_000_000,
            },
            route: RouteRecord {
                swaps: vec![SwapRecord {
                    component_id: "0xpool1".to_owned(),
                    protocol: "uniswap_v2".to_owned(),
                    token_in: "0xaaa".to_owned(),
                    token_out: "0xbbb".to_owned(),
                    amount_in: "1000000000000000000".to_owned(),
                    amount_out: "3500000000".to_owned(),
                    gas_estimate: "150000".to_owned(),
                    split: 1.0,
                }],
            },
            amount_in: "1000000000000000000".to_owned(),
            amount_out: "3500000000".to_owned(),
            gas_estimate: "150000".to_owned(),
        }
    }

    /// Build a deterministic multi-hop Base quote record.
    fn base_multi_hop_record() -> QuoteRecord {
        QuoteRecord {
            quote_id: "test-quote-002".to_owned(),
            chain_id: 8453,
            block: BlockRecord {
                number: 5_000_000,
                hash: "0x111111111111111111111111111111111\
                       1111111111111111111111111111111"
                    .to_owned(),
                timestamp: 1_700_000_000,
            },
            route: RouteRecord {
                swaps: vec![
                    SwapRecord {
                        component_id: "0xpoolA".to_owned(),
                        protocol: "uniswap_v3".to_owned(),
                        token_in: "0xaaa".to_owned(),
                        token_out: "0xbbb".to_owned(),
                        amount_in: "500".to_owned(),
                        amount_out: "400".to_owned(),
                        gas_estimate: "80000".to_owned(),
                        split: 0.6,
                    },
                    SwapRecord {
                        component_id: "0xpoolB".to_owned(),
                        protocol: "uniswap_v2".to_owned(),
                        token_in: "0xbbb".to_owned(),
                        token_out: "0xccc".to_owned(),
                        amount_in: "400".to_owned(),
                        amount_out: "350".to_owned(),
                        gas_estimate: "70000".to_owned(),
                        split: 0.4,
                    },
                ],
            },
            amount_in: "500".to_owned(),
            amount_out: "350".to_owned(),
            gas_estimate: "150000".to_owned(),
        }
    }

    fn full_enrichment() -> EnrichmentInput {
        EnrichmentInput {
            chain_env: ChainEnvInput {
                base_fee_gwei: Some(30.0),
                gas_used: Some(15_000_000),
                gas_limit: Some(30_000_000),
                priority_gas_p50: Some(1.5),
                priority_gas_p95: Some(5.0),
                sequencer_lag: None,
            },
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment {
                    fee_bps: Some(30),
                    tvl_usd: Some(1_000_000.0),
                    reserve_a: Some(500_000.0),
                    reserve_b: Some(500_000.0),
                },
            )],
            token_pair: TokenPairInput {
                log_mcap_ratio: Some(2.5),
                min_mcap: Some(1e8),
                max_mcap: Some(1e10),
            },
            fynd_context: FyndContextInput {
                n_alternatives: Some(5),
                gap_to_second_best_bps: Some(12.5),
                score_dispersion: Some(0.03),
                requested_slippage_tolerance: Some(0.005),
            },
            fynd_algorithm: FyndAlgorithmInput {
                algorithm_type: Some("branch_and_bound".to_owned()),
                max_hops: Some(4),
                max_splits: Some(3),
            },
            label_decay_bps: Some([
                Some(0.5),
                Some(1.2),
                Some(2.1),
                Some(3.0),
                Some(4.5),
                Some(5.8),
                Some(7.2),
                Some(8.9),
                Some(10.1),
                Some(12.0),
            ]),
        }
    }

    // ── Deterministic output ───────────────────────────────────────

    #[test]
    fn assembly_is_deterministic_for_fixed_inputs() {
        let record = eth_record();
        let enrichment = full_enrichment();

        let v1 = assemble(&record, &enrichment);
        let v2 = assemble(&record, &enrichment);

        assert_eq!(v1, v2, "identical inputs must produce identical vectors");
    }

    #[test]
    fn assembly_is_deterministic_across_serialization_roundtrip() {
        let record = eth_record();
        let enrichment = full_enrichment();

        let vector = assemble(&record, &enrichment);
        let json1 = serde_json::to_string(&vector).expect("serialize");
        let rt1: FeatureVector = serde_json::from_str(&json1).expect("deserialize");

        // After one roundtrip f64 values stabilize (Ryu shortest-form
        // idempotency). A second roundtrip must be bit-identical.
        let json2 = serde_json::to_string(&rt1).expect("re-serialize");
        let rt2: FeatureVector = serde_json::from_str(&json2).expect("re-deserialize");

        assert_eq!(json2, serde_json::to_string(&rt2).expect("third"));
        assert_eq!(rt1, rt2, "deserialized structs must be identical after stabilization");
    }

    #[test]
    fn deterministic_with_empty_enrichment() {
        let record = eth_record();
        let enrichment = EnrichmentInput::default();

        let v1 = assemble(&record, &enrichment);
        let v2 = assemble(&record, &enrichment);

        assert_eq!(v1, v2);
    }

    #[test]
    fn deterministic_multi_hop_record() {
        let record = base_multi_hop_record();
        let enrichment = EnrichmentInput {
            pools: vec![
                (
                    "0xpoolA".to_owned(),
                    PoolEnrichment {
                        fee_bps: Some(5),
                        tvl_usd: Some(50_000.0),
                        reserve_a: Some(25_000.0),
                        reserve_b: Some(25_000.0),
                    },
                ),
                (
                    "0xpoolB".to_owned(),
                    PoolEnrichment {
                        fee_bps: Some(30),
                        tvl_usd: Some(200_000.0),
                        reserve_a: Some(120_000.0),
                        reserve_b: Some(80_000.0),
                    },
                ),
            ],
            ..EnrichmentInput::default()
        };

        let v1 = assemble(&record, &enrichment);
        let v2 = assemble(&record, &enrichment);

        assert_eq!(v1, v2);
    }

    // ── Correct field ordering ─────────────────────────────────────

    #[test]
    fn serialized_json_preserves_field_order() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        let json_value: serde_json::Value = serde_json::to_value(&vector).expect("to_value");
        let map = json_value
            .as_object()
            .expect("top-level object");
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();

        assert_eq!(
            keys.as_slice(),
            FIELD_ORDER,
            "serialized field order must match FIELD_ORDER constant"
        );
    }

    #[test]
    fn field_order_constant_has_14_entries() {
        assert_eq!(FIELD_ORDER.len(), 14, "FIELD_ORDER should list all 14 top-level fields");
    }

    #[test]
    fn identity_fields_come_first() {
        assert_eq!(FIELD_ORDER[0], "quote_id");
        assert_eq!(FIELD_ORDER[1], "block_number");
        assert_eq!(FIELD_ORDER[2], "chain_id");
    }

    #[test]
    fn v2_extension_fields_are_after_v1_families() {
        let cex_idx = FIELD_ORDER
            .iter()
            .position(|f| *f == "cex_dynamics")
            .expect("cex_dynamics in order");
        let fynd_algo_idx = FIELD_ORDER
            .iter()
            .position(|f| *f == "fynd_algorithm")
            .expect("fynd_algorithm in order");

        assert!(cex_idx > fynd_algo_idx, "v2 extensions must follow v1 families");
    }

    #[test]
    fn labels_are_last() {
        assert_eq!(FIELD_ORDER.last(), Some(&"label_decay"), "label_decay must be the last field");
    }

    // ── Correct v1 feature values ──────────────────────────────────

    #[test]
    fn identity_fields_match_record() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert_eq!(vector.quote_id, "test-quote-001");
        assert_eq!(vector.block_number, 21_000_000);
        assert_eq!(vector.chain_id, 1);
    }

    #[test]
    fn route_topology_single_hop() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert_eq!(vector.route_topology.hop_count, 1);
        assert_eq!(vector.route_topology.split_count, 0);
        assert_eq!(vector.route_topology.gas_estimate, Some(150_000.0));
        assert_eq!(
            vector
                .route_topology
                .pool_type_diversity,
            Some(1.0)
        );
    }

    #[test]
    fn route_topology_multi_hop_with_splits() {
        let record = base_multi_hop_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert_eq!(vector.route_topology.hop_count, 2);
        assert_eq!(vector.route_topology.split_count, 2);
        assert_eq!(vector.route_topology.gas_estimate, Some(150_000.0));
        assert_eq!(
            vector
                .route_topology
                .pool_type_diversity,
            Some(1.0)
        );
    }

    #[test]
    fn temporal_features_from_known_timestamp() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        // timestamp = 1_730_000_000
        // secs_in_day = 1730000000 % 86400 = 12800 → hour = 3
        assert_eq!(vector.temporal.hour_of_day, Some(3));
        assert!(vector.temporal.day_of_week.is_some());
        assert!(vector
            .temporal
            .minutes_since_hour
            .is_some());
    }

    #[test]
    fn chain_env_ethereum_is_not_l2() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert_eq!(vector.chain_env.chain_id, 1);
        assert!(!vector.chain_env.is_l2);
    }

    #[test]
    fn chain_env_base_is_l2() {
        let record = base_multi_hop_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert_eq!(vector.chain_env.chain_id, 8453);
        assert!(vector.chain_env.is_l2);
    }

    #[test]
    fn chain_env_with_enrichment() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        assert!(vector.chain_env.base_fee_log.is_some());
        assert_eq!(vector.chain_env.block_utilization, Some(0.5));
        assert_eq!(vector.chain_env.priority_gas_p50, Some(1.5));
        assert_eq!(vector.chain_env.priority_gas_p95, Some(5.0));
    }

    #[test]
    fn pool_state_matches_enrichment() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        assert_eq!(vector.pool_state.pools.len(), 1);
        let pool = &vector.pool_state.pools[0];
        assert_eq!(pool.component_id, "0xpool1");
        assert_eq!(pool.protocol, "uniswap_v2");
        assert_eq!(pool.fee_tier, Some(0.003));
        assert!(pool.tvl_usd_log.is_some());
        assert_eq!(pool.reserve_imbalance, Some(0.0));
    }

    #[test]
    fn pool_state_multi_hop_matches_swap_order() {
        let record = base_multi_hop_record();
        let enrichment = EnrichmentInput {
            pools: vec![
                (
                    "0xpoolA".to_owned(),
                    PoolEnrichment { fee_bps: Some(5), ..PoolEnrichment::default() },
                ),
                (
                    "0xpoolB".to_owned(),
                    PoolEnrichment { fee_bps: Some(30), ..PoolEnrichment::default() },
                ),
            ],
            ..EnrichmentInput::default()
        };
        let vector = assemble(&record, &enrichment);

        assert_eq!(vector.pool_state.pools.len(), 2);
        assert_eq!(vector.pool_state.pools[0].component_id, "0xpoolA");
        assert_eq!(vector.pool_state.pools[0].fee_tier, Some(0.0005));
        assert_eq!(vector.pool_state.pools[1].component_id, "0xpoolB");
        assert_eq!(vector.pool_state.pools[1].fee_tier, Some(0.003));
    }

    #[test]
    fn token_pair_computed_from_record() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert!(
            vector
                .token_pair
                .log_amount_ratio
                .is_some(),
            "log_amount_ratio should be computable from record amounts"
        );
        assert!(
            vector
                .token_pair
                .gas_share_of_trade
                .is_some(),
            "gas_share_of_trade should be computable"
        );
    }

    #[test]
    fn fynd_context_carries_amounts_from_record() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        assert_eq!(vector.fynd_context.quoted_amount_in, "1000000000000000000");
        assert_eq!(vector.fynd_context.quoted_amount_out, "3500000000");
        assert_eq!(vector.fynd_context.n_alternatives, Some(5));
        assert_eq!(
            vector
                .fynd_context
                .requested_slippage_tolerance,
            Some(0.005)
        );
    }

    #[test]
    fn fynd_algorithm_from_enrichment() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        assert_eq!(vector.fynd_algorithm.algorithm_type, Some("branch_and_bound".to_owned()));
        assert_eq!(vector.fynd_algorithm.max_hops, Some(4));
        assert_eq!(vector.fynd_algorithm.max_splits, Some(3));
    }

    #[test]
    fn label_decay_from_enrichment() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        assert!(vector.label_decay.is_some());
        let decay = vector
            .label_decay
            .as_ref()
            .expect("has labels");
        assert_eq!(decay.decay_bps[0], Some(0.5));
        assert_eq!(decay.decay_bps[9], Some(12.0));
    }

    // ── Partial feature availability ───────────────────────────────

    #[test]
    fn no_enrichment_yields_none_for_optional_fields() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert!(vector.chain_env.base_fee_log.is_none());
        assert!(vector
            .chain_env
            .block_utilization
            .is_none());
        assert!(vector
            .chain_env
            .priority_gas_p50
            .is_none());
        assert!(vector
            .chain_env
            .priority_gas_p95
            .is_none());
        assert!(vector.chain_env.sequencer_lag.is_none());
        assert!(vector
            .token_pair
            .log_mcap_ratio
            .is_none());
        assert!(vector.token_pair.min_mcap.is_none());
        assert!(vector.token_pair.max_mcap.is_none());
        assert!(vector
            .fynd_context
            .n_alternatives
            .is_none());
        assert!(vector
            .fynd_context
            .gap_to_second_best_bps
            .is_none());
        assert!(vector
            .fynd_context
            .score_dispersion
            .is_none());
        assert!(vector
            .fynd_context
            .requested_slippage_tolerance
            .is_none());
        assert!(vector
            .fynd_algorithm
            .algorithm_type
            .is_none());
        assert!(vector.label_decay.is_none());
    }

    #[test]
    fn missing_pool_enrichment_yields_none_per_pool() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        assert_eq!(vector.pool_state.pools.len(), 1);
        let pool = &vector.pool_state.pools[0];
        assert!(pool.fee_tier.is_none());
        assert!(pool.tvl_usd_log.is_none());
        assert!(pool.reserve_imbalance.is_none());
    }

    #[test]
    fn partial_pool_enrichment_applies_selectively() {
        let record = base_multi_hop_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xpoolA".to_owned(),
                PoolEnrichment {
                    fee_bps: Some(5),
                    tvl_usd: Some(50_000.0),
                    reserve_a: None,
                    reserve_b: None,
                },
            )],
            ..EnrichmentInput::default()
        };
        let vector = assemble(&record, &enrichment);

        // Pool A has enrichment
        let pool_a = &vector.pool_state.pools[0];
        assert_eq!(pool_a.fee_tier, Some(0.0005));
        assert!(pool_a.tvl_usd_log.is_some());
        assert!(pool_a.reserve_imbalance.is_none());

        // Pool B has no enrichment
        let pool_b = &vector.pool_state.pools[1];
        assert!(pool_b.fee_tier.is_none());
        assert!(pool_b.tvl_usd_log.is_none());
        assert!(pool_b.reserve_imbalance.is_none());
    }

    #[test]
    fn partial_chain_env_enrichment() {
        let record = eth_record();
        let enrichment = EnrichmentInput {
            chain_env: ChainEnvInput {
                base_fee_gwei: Some(30.0),
                gas_used: None,
                gas_limit: None,
                priority_gas_p50: None,
                priority_gas_p95: None,
                sequencer_lag: None,
            },
            ..EnrichmentInput::default()
        };
        let vector = assemble(&record, &enrichment);

        assert!(vector.chain_env.base_fee_log.is_some());
        assert!(vector
            .chain_env
            .block_utilization
            .is_none());
    }

    #[test]
    fn partial_label_decay_with_some_none_entries() {
        let record = eth_record();
        let mut decay = [None; 10];
        decay[0] = Some(0.5);
        decay[1] = Some(1.2);
        // blocks 3..10 not yet replayed
        let enrichment =
            EnrichmentInput { label_decay_bps: Some(decay), ..EnrichmentInput::default() };
        let vector = assemble(&record, &enrichment);

        let labels = vector.label_decay.expect("has labels");
        assert_eq!(labels.decay_bps[0], Some(0.5));
        assert_eq!(labels.decay_bps[1], Some(1.2));
        assert!(labels.decay_bps[2].is_none());
        assert!(labels.decay_bps[9].is_none());
    }

    // ── v2 extensions always None ──────────────────────────────────

    #[test]
    fn v2_cex_dynamics_always_none() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);
        assert!(vector.cex_dynamics.is_none());
    }

    #[test]
    fn v2_onchain_flow_always_none() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);
        assert!(vector.onchain_flow.is_none());
    }

    #[test]
    fn v2_priors_always_none() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);
        assert!(vector.priors.is_none());
    }

    // ── Cross-chain coverage ───────────────────────────────────────

    #[test]
    fn assembly_works_for_ethereum_and_base() {
        let eth = assemble(&eth_record(), &EnrichmentInput::default());
        let base = assemble(&base_multi_hop_record(), &EnrichmentInput::default());

        assert_eq!(eth.chain_id, 1);
        assert!(!eth.chain_env.is_l2);
        assert_eq!(base.chain_id, 8453);
        assert!(base.chain_env.is_l2);
    }

    // ── Serialization correctness ──────────────────────────────────

    #[test]
    fn feature_vector_serializes_to_valid_json() {
        let record = eth_record();
        let enrichment = full_enrichment();
        let vector = assemble(&record, &enrichment);

        let json = serde_json::to_string_pretty(&vector);
        assert!(json.is_ok(), "FeatureVector must serialize cleanly");
    }

    #[test]
    fn null_v2_fields_serialize_as_json_null() {
        let record = eth_record();
        let vector = assemble(&record, &EnrichmentInput::default());

        let json_value: serde_json::Value = serde_json::to_value(&vector).expect("to_value");

        assert!(json_value["cex_dynamics"].is_null());
        assert!(json_value["onchain_flow"].is_null());
        assert!(json_value["priors"].is_null());
        assert!(json_value["label_decay"].is_null());
    }
}
