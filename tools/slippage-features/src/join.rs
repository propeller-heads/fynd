//! Dataset join: merges quote records, feature vectors, and replay decay
//! arrays into the final per-quote tuple schema for offline analysis.
//!
//! The [`join_dataset`] function is the entry point. It takes three
//! collections keyed by `quote_id` and produces [`JoinedRecord`] instances
//! for every quote that has at least a matching enrichment input. Missing
//! decay arrays are tolerated (the decay field is set to all-`None`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    feature_vector::{self, EnrichmentInput, FeatureVector},
    quote_record::{QuoteRecord, RouteRecord},
    ReplayDecayArray,
};

/// A fully joined dataset record for one quote, combining identity fields,
/// the original route, the assembled feature vector, and replay decay labels.
///
/// This is the output schema for the offline feature-collection dataset:
/// `(quote_id, block_number, route, feature_vector, replay_decay[k=1..10])`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinedRecord {
    pub quote_id: String,
    pub block_number: u64,
    pub chain_id: u64,
    pub route: RouteRecord,
    pub feature_vector: FeatureVector,
    pub replay_decay: [Option<f64>; 10],
}

/// Diagnostic information about keys that could not be matched during join.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoinDiagnostics {
    /// Quote IDs present in `quotes` but missing from `enrichments`.
    pub quotes_missing_enrichment: Vec<String>,
    /// Quote IDs present in `enrichments` but not in `quotes`.
    pub orphan_enrichments: Vec<String>,
    /// Quote IDs present in `decay_arrays` but not in `quotes`.
    pub orphan_decays: Vec<String>,
}

/// Result of a batch join operation.
#[derive(Debug, Clone)]
pub struct JoinResult {
    /// Successfully joined records.
    pub joined: Vec<JoinedRecord>,
    /// Keys that could not be matched.
    pub diagnostics: JoinDiagnostics,
}

impl JoinResult {
    /// Number of successfully joined records.
    pub fn joined_count(&self) -> usize {
        self.joined.len()
    }

    /// Whether every input quote was successfully joined.
    pub fn is_complete(&self) -> bool {
        self.diagnostics.quotes_missing_enrichment.is_empty()
    }
}

/// Join quote records, enrichment inputs, and replay decay arrays by
/// `quote_id` into assembled [`JoinedRecord`] instances.
///
/// For each quote record whose `quote_id` appears in `enrichments`, the
/// function:
/// 1. Looks up the matching [`EnrichmentInput`].
/// 2. Looks up an optional [`ReplayDecayArray`] from `decay_arrays`.
/// 3. Merges the decay array into the enrichment's `label_decay_bps` field.
/// 4. Calls [`feature_vector::assemble`] to produce the [`FeatureVector`].
/// 5. Wraps the result in a [`JoinedRecord`].
///
/// Quotes without a matching enrichment entry are skipped and reported in
/// [`JoinResult::diagnostics`]. Missing decay arrays are tolerated: the
/// decay field is set to all-`None`.
///
/// The output order matches the input `quotes` slice order (stable join).
pub fn join_dataset(
    quotes: &[QuoteRecord],
    enrichments: &HashMap<String, EnrichmentInput>,
    decay_arrays: &HashMap<String, ReplayDecayArray>,
) -> JoinResult {
    let quote_ids: std::collections::HashSet<&str> =
        quotes.iter().map(|q| q.quote_id.as_str()).collect();

    let orphan_enrichments = enrichments
        .keys()
        .filter(|k| !quote_ids.contains(k.as_str()))
        .cloned()
        .collect();

    let orphan_decays = decay_arrays
        .keys()
        .filter(|k| !quote_ids.contains(k.as_str()))
        .cloned()
        .collect();

    let mut joined = Vec::new();
    let mut quotes_missing_enrichment = Vec::new();

    for quote in quotes {
        let Some(enrichment) = enrichments.get(&quote.quote_id) else {
            quotes_missing_enrichment.push(quote.quote_id.clone());
            continue;
        };

        let decay_bps = decay_arrays
            .get(&quote.quote_id)
            .map(|da| da.decay_bps)
            .unwrap_or([None; 10]);

        let merged_enrichment = EnrichmentInput {
            label_decay_bps: Some(decay_bps),
            chain_env: enrichment.chain_env.clone(),
            pools: enrichment.pools.clone(),
            token_pair: enrichment.token_pair.clone(),
            fynd_context: enrichment.fynd_context.clone(),
            fynd_algorithm: enrichment.fynd_algorithm.clone(),
        };

        let feature_vector = feature_vector::assemble(quote, &merged_enrichment);

        joined.push(JoinedRecord {
            quote_id: quote.quote_id.clone(),
            block_number: quote.block.number,
            chain_id: quote.chain_id,
            route: quote.route.clone(),
            feature_vector,
            replay_decay: decay_bps,
        });
    }

    JoinResult {
        joined,
        diagnostics: JoinDiagnostics {
            quotes_missing_enrichment,
            orphan_enrichments,
            orphan_decays,
        },
    }
}

/// Join a single quote record with its enrichment and optional decay array.
///
/// Convenience wrapper for joining one record at a time without building
/// hash maps. Returns `None` when `enrichment` is `None`.
pub fn join_single(
    quote: &QuoteRecord,
    enrichment: Option<&EnrichmentInput>,
    decay_array: Option<&ReplayDecayArray>,
) -> Option<JoinedRecord> {
    let enrichment = enrichment?;

    let decay_bps = decay_array
        .map(|da| da.decay_bps)
        .unwrap_or([None; 10]);

    let merged_enrichment = EnrichmentInput {
        label_decay_bps: Some(decay_bps),
        chain_env: enrichment.chain_env.clone(),
        pools: enrichment.pools.clone(),
        token_pair: enrichment.token_pair.clone(),
        fynd_context: enrichment.fynd_context.clone(),
        fynd_algorithm: enrichment.fynd_algorithm.clone(),
    };

    let feature_vector = feature_vector::assemble(quote, &merged_enrichment);

    Some(JoinedRecord {
        quote_id: quote.quote_id.clone(),
        block_number: quote.block.number,
        chain_id: quote.chain_id,
        route: quote.route.clone(),
        feature_vector,
        replay_decay: decay_bps,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{
        decay::{ReplayDecayArray, MAX_BLOCK_OFFSET},
        feature_vector::{
            ChainEnvInput, EnrichmentInput, FyndAlgorithmInput, FyndContextInput,
            PoolEnrichment, TokenPairInput, FIELD_ORDER,
        },
        quote_record::{BlockRecord, QuoteRecord, RouteRecord, SwapRecord},
    };

    use super::*;

    // ═══════════════════════════════════════════════════════════════════
    // Helpers
    // ═══════════════════════════════════════════════════════════════════

    fn eth_quote(id: &str) -> QuoteRecord {
        QuoteRecord {
            quote_id: id.to_owned(),
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

    fn base_quote(id: &str) -> QuoteRecord {
        QuoteRecord {
            quote_id: id.to_owned(),
            chain_id: 8453,
            block: BlockRecord {
                number: 5_000_000,
                hash: "0x1111111111111111111111111111111111\
                       111111111111111111111111111111"
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

    fn sample_enrichment() -> EnrichmentInput {
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
            label_decay_bps: None,
        }
    }

    fn sample_decay() -> ReplayDecayArray {
        ReplayDecayArray {
            decay_bps: [
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
            ],
            results: vec![],
        }
    }

    fn partial_decay() -> ReplayDecayArray {
        let mut decay_bps = [None; 10];
        decay_bps[0] = Some(0.5);
        decay_bps[4] = Some(4.5);
        decay_bps[9] = Some(12.0);
        ReplayDecayArray { decay_bps, results: vec![] }
    }

    // ═══════════════════════════════════════════════════════════════════
    // Correct field mapping
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn join_maps_identity_fields_from_quote() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        assert_eq!(result.joined_count(), 1);

        let record = &result.joined[0];
        assert_eq!(record.quote_id, "q1");
        assert_eq!(record.block_number, 21_000_000);
        assert_eq!(record.chain_id, 1);
    }

    #[test]
    fn join_maps_route_from_quote() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        assert_eq!(record.route.swaps.len(), 1);
        assert_eq!(record.route.swaps[0].protocol, "uniswap_v2");
        assert_eq!(record.route.swaps[0].component_id, "0xpool1");
    }

    #[test]
    fn join_maps_multi_hop_route_from_base_quote() {
        let quotes = vec![base_quote("b1")];
        let enrichments = HashMap::from([("b1".to_owned(), EnrichmentInput::default())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        assert_eq!(record.chain_id, 8453);
        assert_eq!(record.route.swaps.len(), 2);
        assert_eq!(record.route.swaps[0].protocol, "uniswap_v3");
        assert_eq!(record.route.swaps[1].protocol, "uniswap_v2");
    }

    #[test]
    fn join_maps_feature_vector_from_enrichment() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        let fv = &result.joined[0].feature_vector;

        assert_eq!(fv.quote_id, "q1");
        assert_eq!(fv.block_number, 21_000_000);
        assert_eq!(fv.chain_id, 1);
        assert_eq!(fv.fynd_context.n_alternatives, Some(5));
        assert_eq!(
            fv.fynd_algorithm.algorithm_type,
            Some("branch_and_bound".to_owned())
        );
        assert!(fv.chain_env.base_fee_log.is_some());
        assert_eq!(fv.route_topology.hop_count, 1);
    }

    #[test]
    fn join_maps_decay_array_into_both_fields() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        // Top-level replay_decay field
        assert_eq!(record.replay_decay[0], Some(0.5));
        assert_eq!(record.replay_decay[9], Some(12.0));

        // Feature vector's label_decay should also be populated
        let labels = record
            .feature_vector
            .label_decay
            .as_ref()
            .expect("label_decay should be populated");
        assert_eq!(labels.decay_bps[0], Some(0.5));
        assert_eq!(labels.decay_bps[9], Some(12.0));
    }

    #[test]
    fn join_maps_partial_decay_preserving_none_slots() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), partial_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        assert_eq!(record.replay_decay[0], Some(0.5));
        assert!(record.replay_decay[1].is_none());
        assert!(record.replay_decay[2].is_none());
        assert!(record.replay_decay[3].is_none());
        assert_eq!(record.replay_decay[4], Some(4.5));
        for idx in 5..9 {
            assert!(record.replay_decay[idx].is_none(), "idx {idx} should be None");
        }
        assert_eq!(record.replay_decay[9], Some(12.0));
    }

    #[test]
    fn feature_vector_identity_matches_joined_record() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        assert_eq!(record.quote_id, record.feature_vector.quote_id);
        assert_eq!(record.block_number, record.feature_vector.block_number);
        assert_eq!(record.chain_id, record.feature_vector.chain_id);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Missing / mismatched keys
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn quote_without_enrichment_is_skipped() {
        let quotes = vec![eth_quote("q1"), eth_quote("q2")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 1);
        assert_eq!(result.joined[0].quote_id, "q1");
        assert_eq!(result.diagnostics.quotes_missing_enrichment, vec!["q2"]);
    }

    #[test]
    fn quote_without_decay_gets_all_none_decay() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        assert!(
            record.replay_decay.iter().all(|v| v.is_none()),
            "missing decay should produce all-None array"
        );
        // label_decay in feature vector should still be Some (with all-None entries)
        let labels = record
            .feature_vector
            .label_decay
            .as_ref()
            .expect("label_decay should be Some even with all-None entries");
        assert!(labels.decay_bps.iter().all(|v| v.is_none()));
    }

    #[test]
    fn orphan_enrichment_reported_in_diagnostics() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([
            ("q1".to_owned(), sample_enrichment()),
            ("q_orphan".to_owned(), sample_enrichment()),
        ]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 1);
        assert_eq!(result.diagnostics.orphan_enrichments, vec!["q_orphan"]);
    }

    #[test]
    fn orphan_decay_reported_in_diagnostics() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([
            ("q1".to_owned(), sample_decay()),
            ("d_orphan".to_owned(), sample_decay()),
        ]);

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 1);
        assert_eq!(result.diagnostics.orphan_decays, vec!["d_orphan"]);
    }

    #[test]
    fn all_quotes_missing_enrichment_yields_empty_joined() {
        let quotes = vec![eth_quote("q1"), eth_quote("q2")];
        let enrichments = HashMap::new();
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 0);
        assert!(!result.is_complete());
        assert_eq!(result.diagnostics.quotes_missing_enrichment.len(), 2);
    }

    #[test]
    fn empty_inputs_yield_empty_result() {
        let result = join_dataset(&[], &HashMap::new(), &HashMap::new());

        assert_eq!(result.joined_count(), 0);
        assert!(result.is_complete());
        assert!(result.diagnostics.quotes_missing_enrichment.is_empty());
        assert!(result.diagnostics.orphan_enrichments.is_empty());
        assert!(result.diagnostics.orphan_decays.is_empty());
    }

    #[test]
    fn is_complete_true_when_all_quotes_matched() {
        let quotes = vec![eth_quote("q1"), base_quote("q2")];
        let enrichments = HashMap::from([
            ("q1".to_owned(), sample_enrichment()),
            ("q2".to_owned(), EnrichmentInput::default()),
        ]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        assert!(result.is_complete());
        assert_eq!(result.joined_count(), 2);
    }

    #[test]
    fn is_complete_false_when_some_quotes_unmatched() {
        let quotes = vec![eth_quote("q1"), eth_quote("q2")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        assert!(!result.is_complete());
    }

    // ═══════════════════════════════════════════════════════════════════
    // Output ordering
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn output_order_matches_input_quote_order() {
        let quotes = vec![
            eth_quote("q3"),
            base_quote("q1"),
            eth_quote("q2"),
        ];
        let enrichments = HashMap::from([
            ("q1".to_owned(), EnrichmentInput::default()),
            ("q2".to_owned(), sample_enrichment()),
            ("q3".to_owned(), sample_enrichment()),
        ]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 3);
        assert_eq!(result.joined[0].quote_id, "q3");
        assert_eq!(result.joined[1].quote_id, "q1");
        assert_eq!(result.joined[2].quote_id, "q2");
    }

    #[test]
    fn output_order_skips_unmatched_preserving_order() {
        let quotes = vec![
            eth_quote("q1"),
            eth_quote("q_missing"),
            base_quote("q2"),
        ];
        let enrichments = HashMap::from([
            ("q1".to_owned(), sample_enrichment()),
            ("q2".to_owned(), EnrichmentInput::default()),
        ]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 2);
        assert_eq!(result.joined[0].quote_id, "q1");
        assert_eq!(result.joined[1].quote_id, "q2");
    }

    // ═══════════════════════════════════════════════════════════════════
    // Output schema correctness
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn joined_record_serializes_to_valid_json() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        let json = serde_json::to_string_pretty(record);
        assert!(json.is_ok(), "JoinedRecord must serialize cleanly");

        let json_val: serde_json::Value =
            serde_json::to_value(record).expect("to_value");
        assert!(json_val["quote_id"].is_string());
        assert!(json_val["block_number"].is_number());
        assert!(json_val["chain_id"].is_number());
        assert!(json_val["route"].is_object());
        assert!(json_val["feature_vector"].is_object());
        assert!(json_val["replay_decay"].is_array());
    }

    #[test]
    fn replay_decay_array_always_has_10_elements() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        assert_eq!(
            result.joined[0].replay_decay.len(),
            MAX_BLOCK_OFFSET as usize
        );
    }

    #[test]
    fn feature_vector_field_order_preserved_in_serialization() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let fv = &result.joined[0].feature_vector;

        let json_val: serde_json::Value =
            serde_json::to_value(fv).expect("to_value");
        let map = json_val.as_object().expect("top-level object");
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(keys.as_slice(), FIELD_ORDER);
    }

    #[test]
    fn v2_extension_fields_are_null_in_output() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let fv = &result.joined[0].feature_vector;

        assert!(fv.cex_dynamics.is_none());
        assert!(fv.onchain_flow.is_none());
        assert!(fv.priors.is_none());
    }

    #[test]
    fn joined_record_roundtrips_through_json() {
        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        // First roundtrip stabilizes f64 Ryu shortest-form values.
        let json1 = serde_json::to_string(record).expect("serialize");
        let rt1: JoinedRecord = serde_json::from_str(&json1).expect("deserialize");

        // Second roundtrip must be bit-identical.
        let json2 = serde_json::to_string(&rt1).expect("re-serialize");
        let rt2: JoinedRecord = serde_json::from_str(&json2).expect("re-deserialize");

        assert_eq!(json2, serde_json::to_string(&rt2).expect("third"));
        assert_eq!(rt1, rt2, "deserialized structs must match after stabilization");

        // Key fields preserved through roundtrip
        assert_eq!(rt1.quote_id, "q1");
        assert_eq!(rt1.block_number, 21_000_000);
        assert_eq!(rt1.chain_id, 1);
        assert_eq!(rt1.replay_decay[0], Some(0.5));
        assert_eq!(rt1.replay_decay[9], Some(12.0));
    }

    // ═══════════════════════════════════════════════════════════════════
    // Cross-chain coverage
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn join_handles_mixed_ethereum_and_base_quotes() {
        let quotes = vec![eth_quote("eth1"), base_quote("base1")];
        let enrichments = HashMap::from([
            ("eth1".to_owned(), sample_enrichment()),
            ("base1".to_owned(), EnrichmentInput::default()),
        ]);
        let decays = HashMap::from([
            ("eth1".to_owned(), sample_decay()),
            ("base1".to_owned(), partial_decay()),
        ]);

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 2);
        assert!(result.is_complete());

        let eth = &result.joined[0];
        assert_eq!(eth.chain_id, 1);
        assert!(!eth.feature_vector.chain_env.is_l2);
        assert_eq!(eth.replay_decay[0], Some(0.5));

        let base = &result.joined[1];
        assert_eq!(base.chain_id, 8453);
        assert!(base.feature_vector.chain_env.is_l2);
        assert_eq!(base.replay_decay[0], Some(0.5));
        assert!(base.replay_decay[1].is_none());
    }

    // ═══════════════════════════════════════════════════════════════════
    // Default/empty enrichment
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn join_with_empty_enrichment_produces_valid_record() {
        let quotes = vec![eth_quote("q1")];
        let enrichments =
            HashMap::from([("q1".to_owned(), EnrichmentInput::default())]);
        let decays = HashMap::new();

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        assert_eq!(record.quote_id, "q1");
        assert_eq!(record.feature_vector.route_topology.hop_count, 1);
        assert!(record.feature_vector.chain_env.base_fee_log.is_none());
        assert!(record.feature_vector.label_decay.is_some());
    }

    // ═══════════════════════════════════════════════════════════════════
    // Enrichment does not override existing label_decay_bps
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn enrichment_label_decay_overridden_by_decay_array() {
        let mut enrichment = sample_enrichment();
        enrichment.label_decay_bps = Some([Some(999.0); 10]);

        let quotes = vec![eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), enrichment)]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);
        let record = &result.joined[0];

        // The decay array from decays map should win
        assert_eq!(record.replay_decay[0], Some(0.5));
        assert_eq!(
            record
                .feature_vector
                .label_decay
                .as_ref()
                .expect("has labels")
                .decay_bps[0],
            Some(0.5)
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // join_single convenience function
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn join_single_with_all_inputs() {
        let quote = eth_quote("q1");
        let enrichment = sample_enrichment();
        let decay = sample_decay();

        let record =
            join_single(&quote, Some(&enrichment), Some(&decay))
                .expect("should join");

        assert_eq!(record.quote_id, "q1");
        assert_eq!(record.block_number, 21_000_000);
        assert_eq!(record.replay_decay[0], Some(0.5));
        assert_eq!(record.feature_vector.quote_id, "q1");
    }

    #[test]
    fn join_single_without_enrichment_returns_none() {
        let quote = eth_quote("q1");
        let decay = sample_decay();

        let result = join_single(&quote, None, Some(&decay));
        assert!(result.is_none());
    }

    #[test]
    fn join_single_without_decay_yields_all_none_decay() {
        let quote = eth_quote("q1");
        let enrichment = sample_enrichment();

        let record = join_single(&quote, Some(&enrichment), None)
            .expect("should join");

        assert!(record.replay_decay.iter().all(|v| v.is_none()));
    }

    #[test]
    fn join_single_matches_batch_join_output() {
        let quote = eth_quote("q1");
        let enrichment = sample_enrichment();
        let decay = sample_decay();

        let single = join_single(&quote, Some(&enrichment), Some(&decay))
            .expect("should join");

        let batch = join_dataset(
            &[quote],
            &HashMap::from([("q1".to_owned(), enrichment)]),
            &HashMap::from([("q1".to_owned(), decay)]),
        );

        assert_eq!(batch.joined[0], single);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Determinism
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn join_is_deterministic_for_fixed_inputs() {
        let quotes = vec![eth_quote("q1"), base_quote("q2")];
        let enrichments = HashMap::from([
            ("q1".to_owned(), sample_enrichment()),
            ("q2".to_owned(), EnrichmentInput::default()),
        ]);
        let decays = HashMap::from([
            ("q1".to_owned(), sample_decay()),
            ("q2".to_owned(), partial_decay()),
        ]);

        let r1 = join_dataset(&quotes, &enrichments, &decays);
        let r2 = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(r1.joined, r2.joined);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Duplicate quote_ids
    // ═══════════════════════════════════════════════════════════════════

    #[test]
    fn duplicate_quote_ids_each_produce_joined_record() {
        let quotes = vec![eth_quote("q1"), eth_quote("q1")];
        let enrichments = HashMap::from([("q1".to_owned(), sample_enrichment())]);
        let decays = HashMap::from([("q1".to_owned(), sample_decay())]);

        let result = join_dataset(&quotes, &enrichments, &decays);

        assert_eq!(result.joined_count(), 2);
        assert_eq!(result.joined[0].quote_id, "q1");
        assert_eq!(result.joined[1].quote_id, "q1");
    }
}
