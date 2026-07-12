//! Deterministic fixed-grid quote planning.

use std::collections::HashMap;

use crate::{
    config::{CollectorConfig, PairConfig, TokenConfig},
    record::{point_id, Direction, PointIdentity, QuoteRole, TokenRecord},
};

/// One quote to submit to Fynd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPoint {
    /// Deterministic observation ID.
    pub point_id: String,
    /// Parent source point for matched reverses.
    pub parent_point_id: Option<String>,
    /// Pair ID.
    pub pair_id: String,
    /// Configured pair direction.
    pub direction: Direction,
    /// Ladder index.
    pub depth_index: usize,
    /// Source or matched role.
    pub quote_role: QuoteRole,
    /// Input token.
    pub token_in: TokenRecord,
    /// Output token.
    pub token_out: TokenRecord,
    /// Exact input base units.
    pub amount_in: String,
}

/// Build all fixed-grid source points for one block. Every configured pair is
/// planned on every block; capacity shortfalls surface as explicit skipped rows,
/// never as silent sub-sampling.
pub fn plan_forward_points(
    config: &CollectorConfig,
    run_id: &str,
    block_hash: &str,
) -> Vec<PlannedPoint> {
    let tokens: HashMap<&str, &TokenConfig> = config
        .tokens
        .iter()
        .map(|token| (token.id.as_str(), token))
        .collect();
    let mut points = Vec::new();
    for pair in &config.pairs {
        let token_a = tokens[pair.token_a.as_str()];
        let token_b = tokens[pair.token_b.as_str()];
        append_direction(
            &mut points,
            DirectionInput {
                pair,
                token_in: token_a,
                token_out: token_b,
                amounts: &pair.amounts_a,
                direction: Direction::AToB,
                run_id,
                block_hash,
            },
        );
        append_direction(
            &mut points,
            DirectionInput {
                pair,
                token_in: token_b,
                token_out: token_a,
                amounts: &pair.amounts_b,
                direction: Direction::BToA,
                run_id,
                block_hash,
            },
        );
    }
    points
}

/// Build the exact reverse point for a successful source quote.
pub fn plan_matched_reverse(
    source: &PlannedPoint,
    run_id: &str,
    block_hash: &str,
    forward_gross_output: String,
) -> PlannedPoint {
    PlannedPoint {
        point_id: point_id(PointIdentity {
            run_id,
            block_hash,
            pair_id: &source.pair_id,
            direction: source.direction,
            depth_index: source.depth_index,
            role: QuoteRole::MatchedReverse,
        }),
        parent_point_id: Some(source.point_id.clone()),
        pair_id: source.pair_id.clone(),
        direction: source.direction,
        depth_index: source.depth_index,
        quote_role: QuoteRole::MatchedReverse,
        token_in: source.token_out.clone(),
        token_out: source.token_in.clone(),
        amount_in: forward_gross_output,
    }
}

struct DirectionInput<'a> {
    pair: &'a PairConfig,
    token_in: &'a TokenConfig,
    token_out: &'a TokenConfig,
    amounts: &'a [String],
    direction: Direction,
    run_id: &'a str,
    block_hash: &'a str,
}

fn append_direction(points: &mut Vec<PlannedPoint>, input: DirectionInput<'_>) {
    for (depth_index, amount_in) in input.amounts.iter().enumerate() {
        points.push(PlannedPoint {
            point_id: point_id(PointIdentity {
                run_id: input.run_id,
                block_hash: input.block_hash,
                pair_id: &input.pair.id,
                direction: input.direction,
                depth_index,
                role: QuoteRole::LadderForward,
            }),
            parent_point_id: None,
            pair_id: input.pair.id.clone(),
            direction: input.direction,
            depth_index,
            quote_role: QuoteRole::LadderForward,
            token_in: token_record(input.token_in),
            token_out: token_record(input.token_out),
            amount_in: amount_in.clone(),
        });
    }
}

fn token_record(token: &TokenConfig) -> TokenRecord {
    TokenRecord {
        address: token.address.to_ascii_lowercase(),
        symbol: token.symbol.clone(),
        decimals: token.decimals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CollectionConfig, FyndConfig};

    fn config() -> CollectorConfig {
        CollectorConfig {
            run_name: "test".into(),
            fynd: FyndConfig {
                tycho_url: "localhost".into(),
                tycho_api_key_env: "KEY".into(),
                rpc_http_url_env: "HTTP".into(),
                rpc_ws_url_env: "WS".into(),
                protocols: vec!["uniswap_v3".into()],
                min_tvl: 1.0,
                algorithm: "bellman_ford".into(),
                num_workers: 1,
                task_queue_capacity: 32,
                max_hops: 3,
                algorithm_timeout_ms: 100,
            },
            collection: CollectionConfig {
                sender: "0x0000000000000000000000000000000000000001".into(),
                request_chunk_size: 8,
                state_wait_timeout_ms: 100,
                quote_timeout_ms: 100,
                collection_budget_ms: 200,
                confirmation_depth: 2,
            },
            tokens: vec![
                TokenConfig {
                    id: "a".into(),
                    address: "0x000000000000000000000000000000000000000a".into(),
                    symbol: "A".into(),
                    decimals: 18,
                },
                TokenConfig {
                    id: "b".into(),
                    address: "0x000000000000000000000000000000000000000b".into(),
                    symbol: "B".into(),
                    decimals: 6,
                },
            ],
            pairs: vec![PairConfig {
                id: "a-b".into(),
                token_a: "a".into(),
                token_b: "b".into(),
                amounts_a: vec!["10".into(), "20".into()],
                amounts_b: vec!["30".into()],
            }],
        }
    }

    #[test]
    fn plans_both_directions_with_fixed_integer_amounts() {
        let points = plan_forward_points(&config(), "run", "0xabc");

        assert_eq!(points.len(), 3);
        assert_eq!(points[0].amount_in, "10");
        assert_eq!(points[1].amount_in, "20");
        assert_eq!(points[2].amount_in, "30");
        assert_eq!(points[2].direction, Direction::BToA);
    }

    #[test]
    fn matched_reverse_swaps_tokens_and_uses_gross_output() {
        let source = plan_forward_points(&config(), "run", "0xabc").remove(0);

        let reverse = plan_matched_reverse(&source, "run", "0xabc", "777".into());

        assert_eq!(reverse.parent_point_id.as_deref(), Some(source.point_id.as_str()));
        assert_eq!(reverse.token_in, source.token_out);
        assert_eq!(reverse.token_out, source.token_in);
        assert_eq!(reverse.amount_in, "777");
        assert_eq!(reverse.quote_role, QuoteRole::MatchedReverse);
    }

    #[test]
    fn plans_every_configured_pair_on_every_block() {
        let mut config = config();
        config.pairs.push(PairConfig {
            id: "a-c".into(),
            token_a: "a".into(),
            token_b: "c".into(),
            amounts_a: vec!["40".into()],
            amounts_b: vec!["50".into()],
        });
        config.tokens.push(TokenConfig {
            id: "c".into(),
            address: "0x000000000000000000000000000000000000000c".into(),
            symbol: "C".into(),
            decimals: 18,
        });

        let first = plan_forward_points(&config, "run", "0x1");
        let second = plan_forward_points(&config, "run", "0x2");

        assert_eq!(first.len(), 5);
        assert_eq!(second.len(), 5);
    }
}
