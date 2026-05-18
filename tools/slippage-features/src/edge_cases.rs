//! Pipeline-level edge-case tests for the extraction pipeline.
//!
//! Exercises three categories of edge cases end-to-end:
//! 1. Empty routes — parse rejection, schema rejection, defensive assembly
//! 2. Missing on-chain data — partial/absent enrichment degrades gracefully
//! 3. Malformed quote inputs — error propagation for bad JSON/types/values

#[cfg(test)]
mod tests {
    use crate::{
        feature_vector::{assemble, ChainEnvInput, EnrichmentInput, PoolEnrichment},
        quote_loader::{load_records_from_string, LoadError, RecordOutcome},
        quote_record::{
            parse_quote_record, BlockRecord, ParseError, QuoteRecord, RouteRecord, SwapRecord,
        },
        schema::{validate_quote_record, ViolationKind},
    };

    // ═══════════════════════════════════════════════════════════════════
    // Helpers
    // ═══════════════════════════════════════════════════════════════════

    fn valid_eth_json() -> String {
        serde_json::json!({
            "quote_id": "edge-test-001",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890\
                         abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "route": {
                "swaps": [{
                    "component_id": "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc",
                    "protocol": "uniswap_v2",
                    "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                    "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                    "amount_in": "1000000000000000000",
                    "amount_out": "3500000000",
                    "gas_estimate": "150000",
                    "split": 1.0
                }]
            },
            "amount_in": "1000000000000000000",
            "amount_out": "3500000000",
            "gas_estimate": "150000"
        })
        .to_string()
    }

    fn minimal_record() -> QuoteRecord {
        QuoteRecord {
            quote_id: "edge-test-min".to_owned(),
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

    // ═══════════════════════════════════════════════════════════════════
    // 1. Empty routes
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn parse_rejects_empty_swaps_array() {
        let json = serde_json::json!({
            "quote_id": "edge-empty-route",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890\
                         abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": [] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let err = parse_quote_record(&json).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidField { .. }),
            "empty swaps should produce InvalidField, got: {err}"
        );
        assert!(
            err.to_string()
                .contains("at least one swap"),
            "error should mention 'at least one swap': {err}"
        );
    }

    #[test]
    fn parse_rejects_null_swaps_field() {
        let json = serde_json::json!({
            "quote_id": "edge-null-swaps",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890\
                         abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": null },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let err = parse_quote_record(&json).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingField { .. }),
            "null swaps should produce MissingField, got: {err}"
        );
    }

    #[test]
    fn parse_rejects_missing_route_entirely() {
        let json = serde_json::json!({
            "quote_id": "edge-no-route",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890\
                         abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let err = parse_quote_record(&json).unwrap_err();
        assert!(err.to_string().contains("route"), "error should mention 'route': {err}");
    }

    #[test]
    fn schema_rejects_record_with_empty_swaps() {
        let record = QuoteRecord {
            quote_id: "edge-schema-empty".to_owned(),
            chain_id: 1,
            block: BlockRecord {
                number: 21_000_000,
                hash: "0xabc123".to_owned(),
                timestamp: 1_730_000_000,
            },
            route: RouteRecord { swaps: vec![] },
            amount_in: "1000".to_owned(),
            amount_out: "900".to_owned(),
            gas_estimate: "100000".to_owned(),
        };

        let errs = validate_quote_record(&record).unwrap_err();
        assert!(
            errs.iter()
                .any(|v| v.field == "route.swaps"),
            "should flag empty swaps: {errs:?}"
        );
    }

    #[test]
    fn assembly_handles_empty_swaps_defensively() {
        let record = QuoteRecord {
            quote_id: "edge-assembly-empty".to_owned(),
            chain_id: 1,
            block: BlockRecord {
                number: 21_000_000,
                hash: "0xabc123".to_owned(),
                timestamp: 1_730_000_000,
            },
            route: RouteRecord { swaps: vec![] },
            amount_in: "1000".to_owned(),
            amount_out: "900".to_owned(),
            gas_estimate: "100000".to_owned(),
        };
        let enrichment = EnrichmentInput::default();
        let fv = assemble(&record, &enrichment);

        assert_eq!(fv.route_topology.hop_count, 0);
        assert_eq!(fv.route_topology.split_count, 0);
        assert!(
            fv.route_topology
                .pool_type_diversity
                .is_none(),
            "pool_type_diversity should be None for empty swaps"
        );
        assert!(
            fv.pool_state.pools.is_empty(),
            "pool_state should have no entries for empty swaps"
        );
    }

    #[test]
    fn loader_reports_empty_route_as_parse_error() {
        let json = serde_json::json!({
            "quote_id": "edge-loader-empty",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890\
                         abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": [] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let batch = load_records_from_string(&json).expect("non-empty input");
        assert_eq!(batch.total_count(), 1);
        assert_eq!(batch.valid_count(), 0);
        assert!(
            matches!(&batch.outcomes[0], RecordOutcome::ParseFailed { .. }),
            "empty-route record should be a ParseFailed outcome"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // 2. Missing on-chain data
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn assembly_no_enrichment_degrades_to_none_optional_fields() {
        let record = minimal_record();
        let fv = assemble(&record, &EnrichmentInput::default());

        // Chain env: all enriched fields should be None
        assert!(fv.chain_env.base_fee_log.is_none());
        assert!(fv.chain_env.block_utilization.is_none());
        assert!(fv.chain_env.priority_gas_p50.is_none());
        assert!(fv.chain_env.priority_gas_p95.is_none());
        assert!(fv.chain_env.sequencer_lag.is_none());

        // Pool state: no enrichment → all per-pool features None
        assert_eq!(fv.pool_state.pools.len(), 1);
        let pool = &fv.pool_state.pools[0];
        assert!(pool.fee_tier.is_none());
        assert!(pool.tvl_usd_log.is_none());
        assert!(pool.reserve_imbalance.is_none());

        // Token pair: CoinGecko fields None
        assert!(fv.token_pair.log_mcap_ratio.is_none());
        assert!(fv.token_pair.min_mcap.is_none());
        assert!(fv.token_pair.max_mcap.is_none());

        // Fynd context: solver fields None
        assert!(fv.fynd_context.n_alternatives.is_none());
        assert!(fv
            .fynd_context
            .gap_to_second_best_bps
            .is_none());
        assert!(fv
            .fynd_context
            .score_dispersion
            .is_none());
        assert!(fv
            .fynd_context
            .requested_slippage_tolerance
            .is_none());

        // Fynd algorithm: all None
        assert!(fv
            .fynd_algorithm
            .algorithm_type
            .is_none());
        assert!(fv.fynd_algorithm.max_hops.is_none());
        assert!(fv.fynd_algorithm.max_splits.is_none());

        // v2 extensions: always None
        assert!(fv.cex_dynamics.is_none());
        assert!(fv.onchain_flow.is_none());
        assert!(fv.priors.is_none());

        // Labels: None without enrichment
        assert!(fv.label_decay.is_none());

        // Core record-derived features should still be populated
        assert!(fv.route_topology.hop_count > 0);
        assert!(fv.route_topology.gas_estimate.is_some());
        assert!(fv.temporal.hour_of_day.is_some());
        assert!(fv.token_pair.log_amount_ratio.is_some());
    }

    #[test]
    fn assembly_with_nan_base_fee_yields_none() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            chain_env: ChainEnvInput {
                base_fee_gwei: Some(f64::NAN),
                gas_used: Some(15_000_000),
                gas_limit: Some(30_000_000),
                ..ChainEnvInput::default()
            },
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.chain_env.base_fee_log.is_none(),
            "NaN base_fee should produce None base_fee_log"
        );
        // Non-NaN fields should still work
        assert_eq!(fv.chain_env.block_utilization, Some(0.5));
    }

    #[test]
    fn assembly_with_infinity_base_fee_yields_none() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            chain_env: ChainEnvInput {
                base_fee_gwei: Some(f64::INFINITY),
                ..ChainEnvInput::default()
            },
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.chain_env.base_fee_log.is_none(),
            "Infinity base_fee should produce None base_fee_log"
        );
    }

    #[test]
    fn assembly_with_negative_base_fee_yields_none() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            chain_env: ChainEnvInput { base_fee_gwei: Some(-10.0), ..ChainEnvInput::default() },
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.chain_env.base_fee_log.is_none(),
            "negative base_fee should produce None base_fee_log"
        );
    }

    #[test]
    fn assembly_with_zero_gas_limit_yields_none_utilization() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            chain_env: ChainEnvInput {
                gas_used: Some(15_000_000),
                gas_limit: Some(0),
                ..ChainEnvInput::default()
            },
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.chain_env.block_utilization.is_none(),
            "zero gas_limit should produce None block_utilization"
        );
    }

    #[test]
    fn assembly_with_nan_tvl_yields_none_pool_liquidity() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment {
                    fee_bps: Some(30),
                    tvl_usd: Some(f64::NAN),
                    reserve_a: Some(500_000.0),
                    reserve_b: Some(500_000.0),
                },
            )],
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        let pool = &fv.pool_state.pools[0];
        assert!(pool.tvl_usd_log.is_none(), "NaN TVL should produce None tvl_usd_log");
        // Other pool fields should still work
        assert_eq!(pool.fee_tier, Some(0.003));
        assert_eq!(pool.reserve_imbalance, Some(0.0));
    }

    #[test]
    fn assembly_with_negative_reserves_yields_none_imbalance() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment {
                    fee_bps: Some(30),
                    tvl_usd: Some(1_000_000.0),
                    reserve_a: Some(-100.0),
                    reserve_b: Some(500_000.0),
                },
            )],
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        let pool = &fv.pool_state.pools[0];
        assert!(
            pool.reserve_imbalance.is_none(),
            "negative reserve should produce None reserve_imbalance"
        );
        // Non-affected fields still computed
        assert!(pool.tvl_usd_log.is_some());
        assert!(pool.fee_tier.is_some());
    }

    #[test]
    fn assembly_with_both_reserves_zero_yields_none_imbalance() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment {
                    reserve_a: Some(0.0),
                    reserve_b: Some(0.0),
                    ..PoolEnrichment::default()
                },
            )],
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.pool_state.pools[0]
                .reserve_imbalance
                .is_none(),
            "both-zero reserves should produce None (division by zero)"
        );
    }

    #[test]
    fn assembly_with_unmatched_pool_enrichment_ignores_extra() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xNOT_IN_ROUTE".to_owned(),
                PoolEnrichment {
                    fee_bps: Some(30),
                    tvl_usd: Some(1_000_000.0),
                    reserve_a: Some(500_000.0),
                    reserve_b: Some(500_000.0),
                },
            )],
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        let pool = &fv.pool_state.pools[0];
        assert!(pool.fee_tier.is_none(), "enrichment for non-matching pool ID should not apply");
        assert!(pool.tvl_usd_log.is_none());
        assert!(pool.reserve_imbalance.is_none());
    }

    #[test]
    fn assembly_with_infinity_reserves_yields_none() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment {
                    reserve_a: Some(f64::INFINITY),
                    reserve_b: Some(500_000.0),
                    ..PoolEnrichment::default()
                },
            )],
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.pool_state.pools[0]
                .reserve_imbalance
                .is_none(),
            "infinite reserve should produce None reserve_imbalance"
        );
    }

    #[test]
    fn assembly_with_negative_tvl_yields_none() {
        let record = minimal_record();
        let enrichment = EnrichmentInput {
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment { tvl_usd: Some(-1_000.0), ..PoolEnrichment::default() },
            )],
            ..EnrichmentInput::default()
        };
        let fv = assemble(&record, &enrichment);

        assert!(
            fv.pool_state.pools[0]
                .tvl_usd_log
                .is_none(),
            "negative TVL should produce None tvl_usd_log"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // 3. Malformed quote inputs
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn parse_rejects_plain_string_input() {
        let err = parse_quote_record("not json at all").unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson(_)),
            "non-JSON string should produce InvalidJson"
        );
    }

    #[test]
    fn parse_rejects_json_array_instead_of_object() {
        let err = parse_quote_record("[1, 2, 3]").unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson(_)), "JSON array should produce InvalidJson");
    }

    #[test]
    fn parse_rejects_empty_string() {
        let err = parse_quote_record("").unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson(_)),
            "empty string should produce InvalidJson"
        );
    }

    #[test]
    fn parse_rejects_null_literal() {
        let err = parse_quote_record("null").unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson(_)),
            "null literal should produce InvalidJson"
        );
    }

    #[test]
    fn parse_rejects_chain_id_as_string() {
        let json = serde_json::json!({
            "quote_id": "edge-bad-chain",
            "chain_id": "ethereum",
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": [{
                "component_id": "0xpool",
                "protocol": "uniswap_v2",
                "token_in": "0xaaa",
                "token_out": "0xbbb",
                "amount_in": "1000",
                "amount_out": "900",
                "gas_estimate": "100000",
                "split": 1.0
            }] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let err = parse_quote_record(&json).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson(_)),
            "string chain_id should produce InvalidJson"
        );
    }

    #[test]
    fn parse_rejects_block_number_as_string() {
        let json = serde_json::json!({
            "quote_id": "edge-bad-block",
            "chain_id": 1,
            "block": {
                "number": "not_a_number",
                "hash": "0xabcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": [{
                "component_id": "0xpool",
                "protocol": "uniswap_v2",
                "token_in": "0xaaa",
                "token_out": "0xbbb",
                "amount_in": "1000",
                "amount_out": "900",
                "gas_estimate": "100000",
                "split": 1.0
            }] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let err = parse_quote_record(&json).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson(_)),
            "string block.number should produce InvalidJson"
        );
    }

    #[test]
    fn parse_rejects_split_as_string() {
        let json = serde_json::json!({
            "quote_id": "edge-bad-split",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": [{
                "component_id": "0xpool",
                "protocol": "uniswap_v2",
                "token_in": "0xaaa",
                "token_out": "0xbbb",
                "amount_in": "1000",
                "amount_out": "900",
                "gas_estimate": "100000",
                "split": "half"
            }] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let err = parse_quote_record(&json).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson(_)),
            "string split should produce InvalidJson"
        );
    }

    #[test]
    fn parse_accepts_extra_unknown_fields() {
        let json = serde_json::json!({
            "quote_id": "edge-extra-fields",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890\
                         abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "route": { "swaps": [{
                "component_id": "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc",
                "protocol": "uniswap_v2",
                "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "amount_in": "1000",
                "amount_out": "900",
                "gas_estimate": "100000",
                "split": 1.0,
                "unknown_field": "should_be_ignored"
            }] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000",
            "extra_top_level": { "nested": true },
            "another_extra": 42
        })
        .to_string();

        let record = parse_quote_record(&json);
        assert!(record.is_ok(), "extra fields should be silently ignored: {:?}", record.err());
    }

    #[test]
    fn schema_rejects_non_numeric_amount_strings() {
        let mut record = minimal_record();
        record.amount_in = "not_a_number".to_owned();
        record.amount_out = "also_bad".to_owned();
        record.gas_estimate = "nope".to_owned();

        let errs = validate_quote_record(&record).unwrap_err();
        let fields: Vec<&str> = errs
            .iter()
            .map(|v| v.field.as_str())
            .collect();
        assert!(fields.contains(&"amount_in"));
        assert!(fields.contains(&"amount_out"));
        assert!(fields.contains(&"gas_estimate"));
        for err in &errs {
            assert!(
                matches!(err.kind, ViolationKind::InvalidNumericString { .. }),
                "non-numeric amounts should produce InvalidNumericString"
            );
        }
    }

    #[test]
    fn schema_rejects_negative_amount_strings() {
        let mut record = minimal_record();
        record.amount_in = "-1000".to_owned();

        let errs = validate_quote_record(&record).unwrap_err();
        assert!(
            errs.iter()
                .any(|v| v.field == "amount_in"),
            "negative amount_in should be rejected"
        );
    }

    #[test]
    fn schema_rejects_empty_amount_strings() {
        let mut record = minimal_record();
        record.amount_in = String::new();
        record.amount_out = String::new();

        let errs = validate_quote_record(&record).unwrap_err();
        assert!(
            errs.iter()
                .any(|v| v.field == "amount_in"),
            "empty amount_in should be rejected"
        );
        assert!(
            errs.iter()
                .any(|v| v.field == "amount_out"),
            "empty amount_out should be rejected"
        );
    }

    #[test]
    fn assembly_with_zero_amount_strings_degrades_features() {
        let mut record = minimal_record();
        record.amount_in = "0".to_owned();
        record.amount_out = "0".to_owned();

        let fv = assemble(&record, &EnrichmentInput::default());

        assert!(
            fv.token_pair.log_amount_ratio.is_none(),
            "zero amounts should produce None log_amount_ratio"
        );
        assert!(
            fv.token_pair
                .gas_share_of_trade
                .is_none(),
            "zero amount_in should produce None gas_share_of_trade"
        );

        // Route topology and other families still computed
        assert_eq!(fv.route_topology.hop_count, 1);
        assert!(fv.temporal.hour_of_day.is_some());
    }

    #[test]
    fn assembly_with_non_numeric_gas_estimate_degrades() {
        let mut record = minimal_record();
        record.gas_estimate = "not_a_number".to_owned();

        let fv = assemble(&record, &EnrichmentInput::default());

        assert!(
            fv.route_topology.gas_estimate.is_none(),
            "non-numeric gas_estimate should produce None"
        );
        // Other features unaffected
        assert_eq!(fv.route_topology.hop_count, 1);
    }

    #[test]
    fn assembly_with_empty_gas_estimate_degrades() {
        let mut record = minimal_record();
        record.gas_estimate = String::new();

        let fv = assemble(&record, &EnrichmentInput::default());

        assert!(fv.route_topology.gas_estimate.is_none(), "empty gas_estimate should produce None");
    }

    // ═══════════════════════════════════════════════════════════════════
    // Pipeline integration: loader → parse → validate → assemble
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn loader_empty_source_returns_error() {
        let result = load_records_from_string("");
        assert!(matches!(result, Err(LoadError::EmptySource)));
    }

    #[test]
    fn loader_whitespace_only_returns_error() {
        let result = load_records_from_string("   \n\n   \n");
        assert!(matches!(result, Err(LoadError::EmptySource)));
    }

    #[test]
    fn loader_mixed_valid_and_edge_cases() {
        let valid = valid_eth_json();
        let empty_route = serde_json::json!({
            "quote_id": "edge-batch-empty",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabc123",
                "timestamp": 1730000000
            },
            "route": { "swaps": [] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();
        let malformed = "{not valid json";
        let bad_chain = serde_json::json!({
            "quote_id": "edge-batch-badchain",
            "chain_id": 42161,
            "block": {
                "number": 100000,
                "hash": "0xdeadbeefdeadbeefdeadbeefdeadbeef\
                         deadbeefdeadbeefdeadbeefdeadbeef",
                "timestamp": 1700000000
            },
            "route": { "swaps": [{
                "component_id": "0x1234567890abcdef1234567890abcdef12345678",
                "protocol": "uniswap_v3",
                "token_in": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "token_out": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "amount_in": "1000",
                "amount_out": "900",
                "gas_estimate": "100000",
                "split": 1.0
            }] },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string();

        let data = format!("{valid}\n{empty_route}\n{malformed}\n{bad_chain}");
        let batch = load_records_from_string(&data).expect("non-empty");

        assert_eq!(batch.total_count(), 4);
        assert_eq!(batch.valid_count(), 1, "only the first record is valid");
        assert_eq!(batch.error_count(), 3);

        // Verify error types
        assert!(
            matches!(&batch.outcomes[0], RecordOutcome::Valid(_)),
            "first record should be valid"
        );
        assert!(
            matches!(&batch.outcomes[1], RecordOutcome::ParseFailed { .. }),
            "empty-route record should be ParseFailed"
        );
        assert!(
            matches!(&batch.outcomes[2], RecordOutcome::ParseFailed { .. }),
            "malformed JSON should be ParseFailed"
        );
        assert!(
            matches!(&batch.outcomes[3], RecordOutcome::InvalidRecord { .. }),
            "unsupported chain should be InvalidRecord"
        );
    }

    #[test]
    fn full_pipeline_valid_record_through_assembly() {
        let data = valid_eth_json();
        let batch = load_records_from_string(&data).expect("non-empty");
        let records = batch.into_valid_records();
        assert_eq!(records.len(), 1);

        let fv = assemble(&records[0], &EnrichmentInput::default());
        assert_eq!(fv.quote_id, "edge-test-001");
        assert_eq!(fv.chain_id, 1);
        assert_eq!(fv.block_number, 21_000_000);
        assert_eq!(fv.route_topology.hop_count, 1);
        assert!(fv.temporal.hour_of_day.is_some());
        assert!(!fv.chain_env.is_l2);
    }

    #[test]
    fn full_pipeline_all_invalid_batch_yields_zero_valid() {
        let lines = ["{broken json", r#"{"quote_id": null}"#, r#"{"not": "a quote record"}"#];
        let data = lines.join("\n");
        let batch = load_records_from_string(&data).expect("non-empty");

        assert_eq!(batch.valid_count(), 0);
        assert_eq!(batch.error_count(), 3);
        assert!(batch.into_valid_records().is_empty(), "no valid records should survive");
    }

    #[test]
    fn assembly_serialization_stable_with_edge_case_values() {
        let mut record = minimal_record();
        record.gas_estimate = String::new();
        record.amount_in = "0".to_owned();

        let enrichment = EnrichmentInput {
            chain_env: ChainEnvInput {
                base_fee_gwei: Some(f64::NAN),
                gas_used: Some(0),
                gas_limit: Some(0),
                ..ChainEnvInput::default()
            },
            pools: vec![(
                "0xpool1".to_owned(),
                PoolEnrichment {
                    tvl_usd: Some(f64::INFINITY),
                    reserve_a: Some(-1.0),
                    reserve_b: Some(f64::NAN),
                    ..PoolEnrichment::default()
                },
            )],
            ..EnrichmentInput::default()
        };

        let fv = assemble(&record, &enrichment);

        // All edge cases should result in None, not NaN in JSON
        let json = serde_json::to_string(&fv).expect("should serialize even with edge-case inputs");
        assert!(!json.contains("NaN"), "serialized JSON must not contain NaN: {json}");
        assert!(!json.contains("Infinity"), "serialized JSON must not contain Infinity: {json}");

        // Roundtrip should be stable
        let roundtrip: crate::FeatureVector =
            serde_json::from_str(&json).expect("should deserialize");
        let json2 = serde_json::to_string(&roundtrip).expect("re-serialize");
        assert_eq!(json, json2, "roundtrip should be stable");
    }
}
