//! Tests for the solution components.

use super::*;

/// Unit tests for the component types.
mod unit {
    use num_traits::FromPrimitive;

    use super::*;
    use crate::algorithm::test_utils::{
        token, token_with_decimals, ConstantProductSim, MockProtocolSim,
    };

    fn cp_pool(id: &str, reserve_0: u64, reserve_1: u64) -> PoolRef {
        cp_pool_with_depth(id, reserve_0, reserve_1, None)
    }

    fn cp_pool_with_depth(
        id: &str,
        reserve_0: u64,
        reserve_1: u64,
        depth: Option<BigUint>,
    ) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve_0),
                reserve_1: BigUint::from(reserve_1),
                gas: 50_000,
            }),
            depth,
        )
    }

    fn mock_pool(id: &str, spot_price: f64, fee: f64, liquidity: u128) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(
                MockProtocolSim::new(spot_price)
                    .with_fee(fee)
                    .with_liquidity(liquidity),
            ),
            None,
        )
    }

    /// A depth of `whole` whole tokens, in the 18-decimal on-chain units the fixtures use.
    fn depth(whole: u64) -> Option<BigUint> {
        Some(BigUint::from(whole) * BigUint::from(10u8).pow(18))
    }

    /// One hop A -> B over the given pools, unsolved.
    fn hop_ab(pools: Vec<PoolRef>) -> Hop {
        Hop::new(token(0x0A, "A"), token(0x0B, "B"), pools).expect("hop has pools")
    }

    fn route_ab(hop: Hop) -> SequentialRoute {
        SequentialRoute::new(vec![token(0x0A, "A"), token(0x0B, "B")], vec![hop])
            .expect("route matches its token path")
    }

    // ---------- Fraction ----------

    #[test]
    fn test_fraction_from_f64_limits_denominator() {
        let tenth = Fraction::from_f64(0.1).expect("0.1 is finite");

        assert_eq!(tenth.as_ratio(), &BigRational::new(BigInt::from(1), BigInt::from(10)));
    }

    #[test]
    fn test_fraction_new_caps_denominator_at_split_precision() {
        let pi = BigRational::from_f64(std::f64::consts::PI).expect("pi is finite");

        let split = Fraction::new(pi);

        assert!(split.as_ratio().denom() <= &BigInt::from(SPLIT_PRECISION));
        assert!((split.to_f64() - std::f64::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn test_fraction_new_keeps_small_denominators_exact() {
        let third = Fraction::new(BigRational::new(BigInt::from(1), BigInt::from(3)));

        assert_eq!(third.as_ratio(), &BigRational::new(BigInt::from(1), BigInt::from(3)));
    }

    #[test]
    fn test_fraction_new_handles_negative_values() {
        let value = BigRational::from_f64(-0.1).expect("-0.1 is finite");

        let split = Fraction::new(value);

        assert_eq!(split.as_ratio(), &BigRational::new(BigInt::from(-1), BigInt::from(10)));
    }

    #[test]
    fn test_fraction_apply_is_exact_for_large_amounts() {
        // A third of 3 * 10^30 must be exactly 10^30; f64 splits lose the low digits.
        let amount = BigUint::from(3u8) * BigUint::from(10u8).pow(30);
        let third = Fraction::from_ratio(1, 3).expect("non-zero denominator");

        assert_eq!(third.apply(&amount), BigUint::from(10u8).pow(30));
    }

    #[test]
    fn test_fraction_apply_rounds_down() {
        let half = Fraction::from_ratio(1, 2).expect("non-zero denominator");

        assert_eq!(half.apply(&BigUint::from(7u8)), BigUint::from(3u8));
    }

    #[test]
    fn test_fraction_zero_and_one() {
        let amount = BigUint::from(1_000u32);

        assert_eq!(Fraction::zero().apply(&amount), BigUint::zero());
        assert_eq!(Fraction::one().apply(&amount), amount);
        assert!(Fraction::zero().is_zero());
    }

    #[test]
    fn test_fraction_from_ratio_rejects_zero_denominator() {
        assert!(Fraction::from_ratio(1, 0).is_none());
    }

    // ---------- Hop composition ----------

    #[test]
    fn test_hop_route_price_is_mean_when_unsolved() {
        // Prices 2.0 and 4.0 -> mean 3.0.
        let hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000), cp_pool("p2", 1_000, 4_000)]);

        assert!(!hop.solved());
        assert!(
            (hop.route_price()
                .expect("prices available") -
                3.0)
            .abs() <
                1e-9
        );
    }

    #[test]
    fn test_hop_route_price_is_split_weighted_when_solved() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000), cp_pool("p2", 1_000, 4_000)]);
        hop.set_splits(vec![
            Fraction::from_ratio(1, 4).expect("non-zero denominator"),
            Fraction::from_ratio(3, 4).expect("non-zero denominator"),
        ])
        .expect("one split per pool");

        // 2.0 * 0.25 + 4.0 * 0.75 = 3.5
        assert!(
            (hop.route_price()
                .expect("prices available") -
                3.5)
            .abs() <
                1e-9
        );
    }

    #[test]
    fn test_hop_route_price_falls_back_to_mean_when_splits_do_not_fill() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000), cp_pool("p2", 1_000, 4_000)]);
        hop.set_splits(vec![
            Fraction::from_ratio(1, 4).expect("non-zero denominator"),
            Fraction::from_ratio(1, 4).expect("non-zero denominator"),
        ])
        .expect("one split per pool");

        assert!(
            (hop.route_price()
                .expect("prices available") -
                3.0)
            .abs() <
                1e-9
        );
    }

    #[test]
    fn test_hop_set_splits_rejects_wrong_count() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000)]);

        let result = hop.set_splits(vec![Fraction::one(), Fraction::one()]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_hop_new_rejects_empty_pools() {
        let result = Hop::new(token(0x0A, "A"), token(0x0B, "B"), vec![]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_hop_marginal_price_applies_fee() {
        // Mock spot price is marked up by 1/(1-fee), so marginal price returns to 2.0.
        let hop = hop_ab(vec![mock_pool("p1", 2.0, 0.5, u128::MAX)]);

        assert!(
            (hop.marginal_price()
                .expect("mock prices") -
                2.0)
            .abs() <
                1e-9
        );
    }

    #[test]
    fn test_pool_inertia_uses_supplied_depth() {
        let pool = cp_pool_with_depth("p1", 1_000, 2_000, depth(7));

        assert!((pool.inertia(&token(0x0A, "A")) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn test_pool_inertia_falls_back_when_depth_missing() {
        // The pool has plenty of reserves; only the absent depth entry decides the value.
        let pool = cp_pool("p1", 1_000_000, 2_000_000);

        assert_eq!(pool.inertia(&token(0x0A, "A")), MISSING_DEPTH_INERTIA);
    }

    #[test]
    fn test_hop_inertia_is_max_over_unsolved_pools() {
        let hop = hop_ab(vec![
            cp_pool_with_depth("p1", 1_000, 2_000, depth(1)),
            cp_pool_with_depth("p2", 1_000, 2_000, depth(4)),
        ]);

        assert!(!hop.solved());
        assert!((hop.inertia() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn test_hop_inertia_is_split_weighted_when_solved() {
        let mut hop = hop_ab(vec![
            cp_pool_with_depth("p1", 1_000, 2_000, depth(1)),
            cp_pool_with_depth("p2", 1_000, 2_000, depth(4)),
        ]);
        hop.set_splits(vec![
            Fraction::from_ratio(1, 2).expect("non-zero denominator"),
            Fraction::from_ratio(1, 2).expect("non-zero denominator"),
        ])
        .expect("one split per pool");

        assert!((hop.inertia() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn test_hop_gas_sums_pools_after_sell() {
        let mut hop =
            hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000), cp_pool("p2", 1_000_000, 2_000_000)]);
        hop.set_splits(vec![
            Fraction::from_ratio(1, 2).expect("non-zero denominator"),
            Fraction::from_ratio(1, 2).expect("non-zero denominator"),
        ])
        .expect("one split per pool");

        hop.sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        assert_eq!(hop.gas(), BigUint::from(100_000u32));
    }

    // ---------- new_marginal_price None propagation ----------

    #[test]
    fn test_hop_new_marginal_price_none_when_unsolved() {
        let hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000)]);

        assert_eq!(hop.new_marginal_price(), None);
    }

    #[test]
    fn test_hop_new_marginal_price_none_before_sell() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000)]);
        hop.set_splits(vec![Fraction::one()])
            .expect("one split per pool");

        assert_eq!(hop.new_marginal_price(), None);
    }

    #[test]
    fn test_hop_new_marginal_price_ignores_zero_split_pools() {
        // Only the pool with a non-zero split is sold on; the untouched one must not veto.
        let mut hop =
            hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000), cp_pool("p2", 1_000_000, 2_000_000)]);
        hop.set_splits(vec![Fraction::one(), Fraction::zero()])
            .expect("one split per pool");

        hop.sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        let price = hop
            .new_marginal_price()
            .expect("the hop was sold on");
        assert!(price > 0.0);
    }

    #[test]
    fn test_route_new_marginal_price_none_when_any_hop_unsold() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let mut first =
            Hop::new(token_a.clone(), token_b.clone(), vec![cp_pool("ab", 1_000_000, 2_000_000)])
                .expect("hop has pools");
        first
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut second =
            Hop::new(token_b.clone(), token_c.clone(), vec![cp_pool("bc", 1_000_000, 2_000_000)])
                .expect("hop has pools");
        second
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        assert_eq!(route.new_marginal_price(), None);

        route
            .sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        assert!(route.new_marginal_price().is_some());
    }

    #[test]
    fn test_graph_new_marginal_price_none_when_a_branch_is_unsold() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000)]);
        hop.set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut graph = DecompositionGraph::from_routes(vec![route_ab(hop)], vec![Fraction::one()])
            .expect("branches share endpoints");

        assert_eq!(graph.new_marginal_price(), None);

        graph
            .sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        assert!(graph.new_marginal_price().is_some());
    }

    // ---------- Unsolved outer splits ----------

    /// Two solved single-hop branches: price 2 / inertia 1, and price 4 / inertia 3.
    fn two_branch_graph(outer_splits: Vec<Fraction>) -> DecompositionGraph {
        let mut cheap = hop_ab(vec![cp_pool_with_depth("p1", 1_000, 2_000, depth(1))]);
        cheap
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut rich = hop_ab(vec![cp_pool_with_depth("p2", 1_000, 4_000, depth(3))]);
        rich.set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        DecompositionGraph::from_routes(vec![route_ab(cheap), route_ab(rich)], outer_splits)
            .expect("branches share endpoints")
    }

    #[test]
    fn test_graph_without_outer_splits_is_unsolved() {
        let graph = two_branch_graph(Vec::new());

        assert!(graph
            .branches()
            .iter()
            .all(Branch::solved));
        assert!(!graph.solved());
    }

    #[test]
    fn test_graph_without_outer_splits_uses_estimates() {
        let graph = two_branch_graph(Vec::new());

        // Mean of 2 and 4; max of the branch weights 2*1 and 4*3; max of the inertias 1 and 3.
        assert!(
            (graph
                .route_price()
                .expect("prices available") -
                3.0)
            .abs() <
                1e-9
        );
        assert!(
            (graph
                .weight()
                .expect("prices available") -
                12.0)
                .abs() <
                1e-9
        );
        assert!((graph.inertia() - 3.0).abs() < 1e-9);
        assert_eq!(graph.new_marginal_price(), None);
    }

    #[test]
    fn test_graph_with_outer_splits_uses_weighted_composition() {
        let graph = two_branch_graph(vec![
            Fraction::from_ratio(1, 4).expect("non-zero denominator"),
            Fraction::from_ratio(3, 4).expect("non-zero denominator"),
        ]);

        assert!(graph.solved());
        // 2*0.25 + 4*0.75 = 3.5; 2*0.25 + 12*0.75 = 9.5; 1*0.25 + 3*0.75 = 2.5.
        assert!(
            (graph
                .route_price()
                .expect("prices available") -
                3.5)
            .abs() <
                1e-9
        );
        assert!(
            (graph
                .weight()
                .expect("prices available") -
                9.5)
            .abs() <
                1e-9
        );
        assert!((graph.inertia() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn test_graph_set_outer_splits_empty_marks_unsolved() {
        let mut graph = two_branch_graph(vec![Fraction::one(), Fraction::zero()]);
        assert!(graph.solved());

        graph
            .set_outer_splits(Vec::new())
            .expect("clearing splits is allowed");

        assert!(!graph.solved());
    }

    #[test]
    fn test_graph_set_outer_splits_rejects_wrong_count() {
        let mut graph = two_branch_graph(Vec::new());

        let result = graph.set_outer_splits(vec![Fraction::one()]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_graph_new_rejects_wrong_split_count() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000)]);
        hop.set_splits(vec![Fraction::one()])
            .expect("one split per pool");

        let result = DecompositionGraph::from_routes(
            vec![route_ab(hop)],
            vec![Fraction::one(), Fraction::one()],
        );

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_graph_sell_requires_outer_splits() {
        let mut graph = two_branch_graph(Vec::new());

        let error = graph
            .sell(&BigUint::from(1_000u32))
            .expect_err("graph has no outer splits");

        assert!(matches!(error, DecompositionError::Unsolved { .. }));
    }

    // ---------- SequentialRoute composition ----------

    #[test]
    fn test_route_fee_composes_in_series() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let first =
            Hop::new(token_a.clone(), token_b.clone(), vec![mock_pool("ab", 2.0, 0.1, u128::MAX)])
                .expect("hop has pools");
        let second =
            Hop::new(token_b.clone(), token_c.clone(), vec![mock_pool("bc", 2.0, 0.2, u128::MAX)])
                .expect("hop has pools");
        let route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        // 1 - (1 - 0.1)(1 - 0.2) = 0.28
        assert!((route.fee() - 0.28).abs() < 1e-9);
    }

    #[test]
    fn test_route_inertia_is_min_over_hops() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let first = Hop::new(
            token_a.clone(),
            token_b.clone(),
            vec![cp_pool_with_depth("ab", 4_000, 2_000, depth(4))],
        )
        .expect("hop has pools");
        let second = Hop::new(
            token_b.clone(),
            token_c.clone(),
            vec![cp_pool_with_depth("bc", 1_000, 2_000, depth(1))],
        )
        .expect("hop has pools");
        let route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        assert!((route.inertia() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_route_price_is_product_of_hops() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let first = Hop::new(token_a.clone(), token_b.clone(), vec![cp_pool("ab", 1_000, 2_000)])
            .expect("hop has pools");
        let second = Hop::new(token_b.clone(), token_c.clone(), vec![cp_pool("bc", 1_000, 3_000)])
            .expect("hop has pools");
        let route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        assert!(
            (route
                .route_price()
                .expect("prices available") -
                6.0)
            .abs() <
                1e-9
        );
    }

    #[test]
    fn test_route_new_rejects_disconnected_hops() {
        let hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000)]);

        let result = SequentialRoute::new(vec![token(0x0A, "A"), token(0x0C, "C")], vec![hop]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_route_solved_requires_every_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let mut first =
            Hop::new(token_a.clone(), token_b.clone(), vec![cp_pool("ab", 1_000, 2_000)])
                .expect("hop has pools");
        first
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let second = Hop::new(token_b.clone(), token_c.clone(), vec![cp_pool("bc", 1_000, 2_000)])
            .expect("hop has pools");
        let mut route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        assert!(!route.solved());

        route.hops_mut()[1]
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");

        assert!(route.solved());
    }

    // ---------- Selling ----------

    #[test]
    fn test_pool_sell_zero_clears_post_trade_state() {
        let mut pool = cp_pool("p1", 1_000_000, 2_000_000);
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        pool.sell(&BigUint::from(1_000u32), &token_a, &token_b)
            .expect("amount is under the limit");
        assert!(pool.new_state().is_some());

        pool.sell(&BigUint::zero(), &token_a, &token_b)
            .expect("selling zero always succeeds");

        assert!(pool.new_state().is_none());
        assert!(pool.buy_amount().is_zero());
        assert!(pool.gas().is_zero());
    }

    #[test]
    fn test_pool_sell_serves_repeats_from_cache() {
        let mut pool = cp_pool("p1", 1_000_000, 2_000_000);
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let amount = BigUint::from(1_000u32);

        let first = pool
            .sell(&amount, &token_a, &token_b)
            .expect("amount is under the limit");
        assert!(pool.has_cached_swap(&amount));

        let second = pool
            .sell(&amount, &token_a, &token_b)
            .expect("amount is under the limit");

        assert_eq!(first, second);
        assert_eq!(pool.cached_swaps(), 1);
        assert!(pool.new_state().is_some());
    }

    #[test]
    fn test_pool_sell_over_limit_carries_limit() {
        let mut pool = mock_pool("p1", 1.0, 0.0, 1_000);
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let error = pool
            .sell(&BigUint::from(2_000u32), &token_a, &token_b)
            .expect_err("amount is over the limit");

        match error {
            DecompositionError::SellAmountLimit { limit, token, pools } => {
                assert_eq!(limit, BigUint::from(1_000u32));
                assert_eq!(token, token_a.address);
                assert_eq!(pools, vec!["p1".to_string()]);
            }
            other => panic!("expected SellAmountLimit, got {other:?}"),
        }
    }

    #[test]
    fn test_hop_sell_requires_splits() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000)]);

        let error = hop
            .sell(&BigUint::from(1_000u32))
            .expect_err("hop has no splits");

        assert!(matches!(error, DecompositionError::Unsolved { .. }));
    }

    #[test]
    fn test_hop_sell_splits_amount_across_pools() {
        let mut hop =
            hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000), cp_pool("p2", 1_000_000, 2_000_000)]);
        hop.set_splits(vec![
            Fraction::from_ratio(1, 4).expect("non-zero denominator"),
            Fraction::from_ratio(3, 4).expect("non-zero denominator"),
        ])
        .expect("one split per pool");

        hop.sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        assert_eq!(hop.pools()[0].sell_amount(), &BigUint::from(250u32));
        assert_eq!(hop.pools()[1].sell_amount(), &BigUint::from(750u32));
    }

    #[test]
    fn test_route_sell_threads_output_into_next_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let mut first =
            Hop::new(token_a.clone(), token_b.clone(), vec![cp_pool("ab", 1_000_000, 2_000_000)])
                .expect("hop has pools");
        first
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut second =
            Hop::new(token_b.clone(), token_c.clone(), vec![cp_pool("bc", 1_000_000, 2_000_000)])
                .expect("hop has pools");
        second
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        let (bought, _) = route
            .sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        let intermediate = route.hops()[0].buy_amount().clone();
        assert_eq!(route.hops()[1].sell_amount(), &intermediate);
        assert_eq!(route.buy_amount(), &bought);
    }

    #[test]
    fn test_graph_sell_splits_amount_across_branches() {
        let mut first_hop = hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000)]);
        first_hop
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut second_hop = hop_ab(vec![cp_pool("p2", 1_000_000, 2_000_000)]);
        second_hop
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut graph = DecompositionGraph::from_routes(
            vec![route_ab(first_hop), route_ab(second_hop)],
            vec![
                Fraction::from_ratio(1, 4).expect("non-zero denominator"),
                Fraction::from_ratio(3, 4).expect("non-zero denominator"),
            ],
        )
        .expect("branches share endpoints");

        graph
            .sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        assert_eq!(graph.branches()[0].sell_amount(), &BigUint::from(250u32));
        assert_eq!(graph.branches()[1].sell_amount(), &BigUint::from(750u32));
    }

    // ---------- Limits ----------

    #[test]
    fn test_hop_sell_amount_limit_sums_pools() {
        let mut hop =
            hop_ab(vec![mock_pool("p1", 1.0, 0.0, 1_000), mock_pool("p2", 1.0, 0.0, 3_000)]);

        let (limit, pools) = hop
            .sell_amount_limit()
            .expect("limits available");

        assert_eq!(limit, BigUint::from(4_000u32));
        assert_eq!(pools, vec!["p1".to_string(), "p2".to_string()]);
    }

    #[test]
    fn test_route_sell_amount_limit_is_min_after_casting() {
        // Hop 1 sells A at price 2 into B; hop 2's B limit of 1_000 is worth 500 A.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let first =
            Hop::new(token_a.clone(), token_b.clone(), vec![mock_pool("ab", 2.0, 0.0, 10_000)])
                .expect("hop has pools");
        let second =
            Hop::new(token_b.clone(), token_c.clone(), vec![mock_pool("bc", 1.0, 0.0, 1_000)])
                .expect("hop has pools");
        let mut route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        let (limit, pools) = route
            .sell_amount_limit()
            .expect("limits available");

        assert_eq!(limit, BigUint::from(500u32));
        assert_eq!(pools, vec!["bc".to_string()]);
    }

    #[test]
    fn test_route_sell_amount_limit_short_circuits_on_empty_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let first =
            Hop::new(token_a.clone(), token_b.clone(), vec![mock_pool("ab", 2.0, 0.0, 10_000)])
                .expect("hop has pools");
        let second = Hop::new(token_b.clone(), token_c.clone(), vec![mock_pool("bc", 1.0, 0.0, 0)])
            .expect("hop has pools");
        let mut route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        let (limit, _) = route
            .sell_amount_limit()
            .expect("limits available");

        assert!(limit.is_zero());
    }

    #[test]
    fn test_route_sell_over_limit_reports_limit_in_sell_token() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let mut first =
            Hop::new(token_a.clone(), token_b.clone(), vec![mock_pool("ab", 2.0, 0.0, 10_000)])
                .expect("hop has pools");
        first
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut second =
            Hop::new(token_b.clone(), token_c.clone(), vec![mock_pool("bc", 1.0, 0.0, 1_000)])
                .expect("hop has pools");
        second
            .set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        let mut route =
            SequentialRoute::new(vec![token_a.clone(), token_b, token_c], vec![first, second])
                .expect("route matches its token path");

        let error = route
            .sell(&BigUint::from(9_000u32))
            .expect_err("amount is over the route limit");

        match error {
            DecompositionError::SellAmountLimit { limit, token, .. } => {
                assert_eq!(limit, BigUint::from(500u32));
                assert_eq!(token, token_a.address);
            }
            other => panic!("expected SellAmountLimit, got {other:?}"),
        }
    }

    #[test]
    fn test_route_cast_to_sell_token_scales_decimals() {
        // A has 18 decimals, B has 6; price is 1.0, so 10^6 B units are 10^18 A units.
        let token_a = token_with_decimals(0x0A, "A", 18);
        let token_b = token_with_decimals(0x0B, "B", 6);
        let token_c = token_with_decimals(0x0C, "C", 6);
        let first =
            Hop::new(token_a.clone(), token_b.clone(), vec![mock_pool("ab", 1.0, 0.0, u128::MAX)])
                .expect("hop has pools");
        let second =
            Hop::new(token_b.clone(), token_c.clone(), vec![mock_pool("bc", 1.0, 0.0, u128::MAX)])
                .expect("hop has pools");
        let route = SequentialRoute::new(vec![token_a, token_b, token_c], vec![first, second])
            .expect("route matches its token path");

        let cast = route
            .cast_to_sell_token(1, &BigUint::from(1_000_000u32))
            .expect("prices available");

        assert_eq!(cast, BigUint::from(10u8).pow(18));
    }

    #[test]
    fn test_graph_sell_amount_limit_sums_branches() {
        let first_hop = hop_ab(vec![mock_pool("p1", 1.0, 0.0, 1_000)]);
        let second_hop = hop_ab(vec![mock_pool("p2", 1.0, 0.0, 3_000)]);
        let mut graph = DecompositionGraph::from_routes(
            vec![route_ab(first_hop), route_ab(second_hop)],
            vec![Fraction::one(), Fraction::one()],
        )
        .expect("branches share endpoints");

        let (limit, pools) = graph
            .sell_amount_limit()
            .expect("limits available");

        assert_eq!(limit, BigUint::from(4_000u32));
        assert_eq!(pools, vec!["p1".to_string(), "p2".to_string()]);
    }

    #[test]
    fn test_invalidate_clears_caches() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000)]);
        hop.set_splits(vec![Fraction::one()])
            .expect("one split per pool");
        hop.sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");
        assert!(hop.has_cached_limit());
        assert!(hop.pools()[0].cached_swaps() > 0);

        hop.invalidate();

        assert!(!hop.has_cached_limit());
        assert_eq!(hop.pools()[0].cached_swaps(), 0);
        assert!(!hop.pools()[0].has_cached_limit());
    }

    // ---------- executed_price ----------

    #[test]
    fn test_executed_price_zero_on_zero_buy_amount() {
        let hop = hop_ab(vec![cp_pool("p1", 1_000, 2_000)]);

        assert_eq!(hop.executed_price(), 0.0);
    }

    #[test]
    fn test_executed_price_scales_decimals() {
        let sell_token = token_with_decimals(0x0A, "A", 18);
        let buy_token = token_with_decimals(0x0B, "B", 6);
        let sell_amount = BigUint::from(10u8).pow(18);
        let buy_amount = BigUint::from(2_000_000u32);

        let price = executed_price(&sell_amount, &sell_token, &buy_amount, &buy_token);

        assert!((price - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_executed_price_after_sell() {
        let mut hop = hop_ab(vec![cp_pool("p1", 1_000_000, 2_000_000)]);
        hop.set_splits(vec![Fraction::one()])
            .expect("one split per pool");

        hop.sell(&BigUint::from(1_000u32))
            .expect("amount is under the limit");

        // Constant product with equal decimals: slightly below the spot price of 2.
        assert!(hop.executed_price() > 1.9 && hop.executed_price() < 2.0);
    }
}

/// Behaviour ported from defibot's decomposition route tests.
///
/// Source: `defibot/solver/tests/algorithms/decomposition/test_routes.py`,
/// `test_bugfixing.py` and `test_utils.py`. Each test names the defibot test it carries over.
/// Cases that only exercised the recursive `FractalRoute` tree, the route builder or the solver
/// are not here; the port report lists them.
///
/// Numbers are defibot's wherever they follow from the fixture arithmetic rather than from a
/// recorded mainnet block.
mod defibot_routes {
    use num_bigint::{BigInt, BigUint};
    use num_rational::BigRational;
    use num_traits::{Signed, Zero};
    use proptest::prelude::*;
    use tycho_simulation::tycho_core::{
        models::token::Token, simulation::protocol_sim::ProtocolSim,
    };

    use crate::algorithm::{
        decomposition::{
            components::{
                Branch, BranchSide, DecompositionError, DecompositionGraph, Fraction, Hop, PoolRef,
                SellLimitKind, SequentialRoute, SPLIT_PRECISION,
            },
            solve::sell_with_coupled_paths,
            test_fixtures::{
                branch, diamond_graph, expect_sell_amount_limit, graph, hop, pool, route,
                single_pool_hop, solved_hop, split, split_f64, tenfold_pool, tenfold_route,
                token_a, token_b, token_c, token_d, usdc, wbtc, FixedRateSim,
            },
        },
        test_utils::{token, ConstantProductSim, MockProtocolSim},
    };

    /// A pool priced like defibot's `TestMarginalPrice.make_route` (`test_routes.py:443-476`): a
    /// spot price before the trade, an independent one after it, and a fee.
    fn priced_pool(id: &str, spot_price: f64, post_trade_spot_price: f64, fee: f64) -> PoolRef {
        pool(
            id,
            FixedRateSim::new(1)
                .with_spot_price(spot_price)
                .with_post_trade_spot_price(post_trade_spot_price)
                .with_fee(fee),
        )
    }

    /// A pool with a hard cap on what it will sell, defibot's `MockRouteLimit`
    /// (`utils.py:138-148`).
    fn limited_pool(id: &str, sell_limit: u64) -> PoolRef {
        pool(id, FixedRateSim::new(10).with_sell_limit(sell_limit))
    }

    /// A fee-free pool with an explicit spot price and depth, in whole 18-decimal tokens.
    ///
    /// [`pool`] supplies no depth, so every pool built through it reports the missing-depth inertia
    /// of `1.0` and differences between the max and mean inertia rules are invisible.
    fn pool_with_depth(id: &str, spot_price: f64, depth_tokens: u64) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(FixedRateSim::new(1).with_spot_price(spot_price)),
            Some(BigUint::from(depth_tokens) * BigUint::from(10u64).pow(18)),
        )
    }

    #[test]
    fn test_one_hop_route_weight_is_the_best_pool_not_the_composed_formula() {
        // defibot ranks a single-hop token sequence as a bare `ParallelRoute`
        // (`order_solver.py:450-456`), whose unsolved weight is the maximum over its pools
        // (`routes/parallel.py:136-146`). Both pools here score 1.0 * 10 and 5.0 * 2, so the answer
        // is 10 — while the composed sequential formula would pair the mean price (6) with
        // the max inertia (5) for 30, above either pool on its own.
        let route = route(
            vec![token_a(), token_b()],
            vec![hop(
                token_a(),
                token_b(),
                vec![pool_with_depth("shallow", 10.0, 1), pool_with_depth("deep", 2.0, 5)],
            )],
        );

        let weight = route
            .weight()
            .expect("pools are priced");
        assert!((weight - 10.0).abs() < 1e-9, "expected the best pool's weight, got {weight}");

        let composed = route
            .route_price()
            .expect("pools are priced") *
            (1.0 - route.fee()) *
            route.inertia();
        assert!(
            (composed - 30.0).abs() < 1e-9,
            "fixture no longer distinguishes the two rules: composed is {composed}"
        );
    }

    #[test]
    fn test_multi_hop_route_weight_uses_the_composed_formula() {
        // The delegation above is scoped to one hop. A two-hop route still composes
        // (`routes/sequential.py:109-111`): price 10*2 = 20, fee 0, inertia min(1, 5) = 1.
        let route = route(
            vec![token_a(), token_b(), token_c()],
            vec![
                hop(token_a(), token_b(), vec![pool_with_depth("first", 10.0, 1)]),
                hop(token_b(), token_c(), vec![pool_with_depth("second", 2.0, 5)]),
            ],
        );

        let weight = route
            .weight()
            .expect("pools are priced");
        assert!((weight - 20.0).abs() < 1e-9, "expected the composed weight, got {weight}");
    }

    // ===================== TestMarginalPrice (test_routes.py:442-576) =====================

    #[test]
    fn test_pool_marginal_price_applies_fee_to_both_states() {
        // `test_simple_route` (:479): spot 100 at a 1% fee is a marginal price of 99, and the
        // post-trade spot of 200 is a post-trade marginal price of 198.
        let mut pool = priced_pool("p", 100.0, 200.0, 0.01);
        let (token_in, token_out) = (token_a(), token_b());

        assert!(
            (pool
                .marginal_price(&token_in, &token_out)
                .unwrap() -
                99.0)
                .abs() <
                1e-9
        );

        pool.sell(&BigUint::from(1u8), &token_in, &token_out)
            .unwrap();

        let new_price = pool
            .new_marginal_price(&token_in, &token_out)
            .expect("the pool was sold on");
        assert!((new_price - 198.0).abs() < 1e-9);
    }

    #[test]
    fn test_pool_new_marginal_price_none_without_a_sell() {
        // `test_simple_route_no_new_state` (:521).
        let pool = priced_pool("p", 100.0, 200.0, 0.01);
        let (token_in, token_out) = (token_a(), token_b());

        assert!(
            (pool
                .marginal_price(&token_in, &token_out)
                .unwrap() -
                99.0)
                .abs() <
                1e-9
        );
        assert_eq!(pool.new_marginal_price(&token_in, &token_out), None);
    }

    #[test]
    fn test_route_marginal_price_multiplies_hops() {
        // `test_sequential_route` (:487): 100*(1-0.7) * 100*(1-0.5) = 30 * 50 = 1500, and after the
        // trade 200*0.3 * 200*0.5 = 60 * 100 = 6000.
        let mut route = route(
            vec![token_a(), token_b(), token_c()],
            vec![
                single_pool_hop(token_a(), token_b(), priced_pool("ab", 100.0, 200.0, 0.7)),
                single_pool_hop(token_b(), token_c(), priced_pool("bc", 100.0, 200.0, 0.5)),
            ],
        );

        assert!((route.marginal_price().unwrap() - 1500.0).abs() < 1e-9);

        route.sell(&BigUint::from(1u8)).unwrap();

        assert!(
            (route
                .new_marginal_price()
                .expect("sold on") -
                6000.0)
                .abs() <
                1e-9
        );
    }

    #[test]
    fn test_route_new_marginal_price_none_when_a_hop_was_not_sold() {
        // `test_sequential_route_no_new_state` (:529): the pre-trade price still composes, but one
        // hop without a post-trade state vetoes the whole product.
        let mut route = route(
            vec![token_a(), token_b(), token_c()],
            vec![
                single_pool_hop(token_a(), token_b(), priced_pool("ab", 100.0, 200.0, 0.7)),
                single_pool_hop(token_b(), token_c(), priced_pool("bc", 100.0, 200.0, 0.5)),
            ],
        );

        route
            .hop_mut(0)
            .sell(&BigUint::from(1u8))
            .unwrap();

        assert!((route.marginal_price().unwrap() - 1500.0).abs() < 1e-9);
        assert_eq!(route.new_marginal_price(), None);
    }

    #[test]
    fn test_hop_marginal_price_is_split_weighted() {
        // `test_parallel_route` (:503): 0.4*100*(1-0.7) + 0.6*100*(1-0.5) = 12 + 30 = 42, and after
        // the trade 0.4*60 + 0.6*100 = 84.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![priced_pool("p1", 100.0, 200.0, 0.7), priced_pool("p2", 100.0, 200.0, 0.5)],
            vec![split_f64(0.4), split_f64(0.6)],
        );

        assert!((hop.marginal_price().unwrap() - 42.0).abs() < 1e-9);

        hop.sell(&BigUint::from(10u8)).unwrap();

        assert!(
            (hop.new_marginal_price()
                .expect("sold on") -
                84.0)
                .abs() <
                1e-9
        );
    }

    #[test]
    fn test_hop_new_marginal_price_none_when_a_split_pool_was_not_sold() {
        // `test_parallel_route_no_new_state` (:544): the second pool carries 60% of the hop but has
        // no post-trade state, so the hop has no post-trade price either.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![priced_pool("p1", 100.0, 200.0, 0.7), priced_pool("p2", 100.0, 200.0, 0.5)],
            vec![Fraction::one(), Fraction::zero()],
        );
        hop.sell(&BigUint::from(10u8)).unwrap();

        hop.set_splits(vec![split_f64(0.4), split_f64(0.6)])
            .unwrap();

        assert!((hop.marginal_price().unwrap() - 42.0).abs() < 1e-9);
        assert_eq!(hop.new_marginal_price(), None);
    }

    #[test]
    fn test_hop_new_marginal_price_ignores_pools_with_a_zero_split() {
        // `test_parallel_route_no_new_state_with_zero_split` (:560): the middle pool has no
        // post-trade state, but it carries nothing, so the price is still 0.4*60 + 0.6*100
        // = 84.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![
                priced_pool("p1", 100.0, 200.0, 0.7),
                priced_pool("p2", 100.0, 200.0, 0.5),
                priced_pool("p3", 100.0, 200.0, 0.5),
            ],
            vec![Fraction::one(), Fraction::zero(), Fraction::one()],
        );
        hop.sell(&BigUint::from(10u8)).unwrap();

        hop.set_splits(vec![split_f64(0.4), Fraction::zero(), split_f64(0.6)])
            .unwrap();

        assert!((hop.marginal_price().unwrap() - 42.0).abs() < 1e-9);
        assert!(
            (hop.new_marginal_price()
                .expect("sold on") -
                84.0)
                .abs() <
                1e-9
        );
    }

    #[test]
    fn test_graph_new_marginal_price_none_when_a_split_branch_was_not_sold() {
        // The outer level of `test_parallel_route_no_new_state` (:544). The three-level structure
        // has no equivalent of a `ParallelRoute` directly over `SimpleRoute`s, so the same
        // rule is checked where it now lives: over branches.
        let mut graph = graph(
            vec![
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(
                        token_a(),
                        token_b(),
                        priced_pool("p1", 100.0, 200.0, 0.7),
                    )],
                ),
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(
                        token_a(),
                        token_b(),
                        priced_pool("p2", 100.0, 200.0, 0.5),
                    )],
                ),
            ],
            vec![Fraction::one(), Fraction::zero()],
        );
        graph
            .sell(&BigUint::from(10u8))
            .unwrap();

        graph
            .set_outer_splits(vec![split_f64(0.4), split_f64(0.6)])
            .unwrap();

        assert!((graph.marginal_price().unwrap() - 42.0).abs() < 1e-9);
        assert_eq!(graph.new_marginal_price(), None);
    }

    #[test]
    fn test_graph_new_marginal_price_ignores_branches_with_a_zero_split() {
        // The outer level of `test_parallel_route_no_new_state_with_zero_split` (:560).
        let mut graph = graph(
            vec![
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(
                        token_a(),
                        token_b(),
                        priced_pool("p1", 100.0, 200.0, 0.7),
                    )],
                ),
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(
                        token_a(),
                        token_b(),
                        priced_pool("p2", 100.0, 200.0, 0.5),
                    )],
                ),
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(
                        token_a(),
                        token_b(),
                        priced_pool("p3", 100.0, 200.0, 0.5),
                    )],
                ),
            ],
            vec![Fraction::one(), Fraction::zero(), Fraction::one()],
        );
        graph
            .sell(&BigUint::from(10u8))
            .unwrap();

        graph
            .set_outer_splits(vec![split_f64(0.4), Fraction::zero(), split_f64(0.6)])
            .unwrap();

        assert!((graph.marginal_price().unwrap() - 42.0).abs() < 1e-9);
        assert!(
            (graph
                .new_marginal_price()
                .expect("sold on") -
                84.0)
                .abs() <
                1e-9
        );
    }

    // ===================== Sell-amount limits =====================

    #[test]
    fn test_hop_sell_over_limit_names_every_responsible_pool() {
        // `TestParallelRoute.test_sell_over_limit` (:235): three parallel pools with limits 10, 20
        // and 30 absorb 60 between them, and the error over that carries the limit and all
        // three pools.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![limited_pool("p10", 10), limited_pool("p20", 20), limited_pool("p30", 30)],
            vec![split(1, 3); 3],
        );

        let (limit, pools) = hop.sell_amount_limit().unwrap();
        assert_eq!(limit, BigUint::from(60u8));
        assert_eq!(pools, vec!["p10".to_string(), "p20".to_string(), "p30".to_string()]);

        let error = hop
            .sell(&BigUint::from(70u8))
            .expect_err("70 is over the hop's limit of 60");

        let (limit, token_address, pools) = expect_sell_amount_limit(error);
        assert_eq!(limit, BigUint::from(60u8));
        assert_eq!(token_address, token_a().address);
        assert_eq!(pools, vec!["p10".to_string(), "p20".to_string(), "p30".to_string()]);
    }

    #[test]
    fn test_route_sell_amount_limit_is_the_tightest_hop_in_sell_token_units() {
        // `TestSequentialRoute.test_get_sell_amount_limit` (:370, `intermediate_0=False`): hop
        // limits 10, 10 and 5 at prices 1.0, 1.0 and 0.01 make the third hop the binding
        // one, and its limit of 5 is already in sell-token units because the hops before it
        // price at one.
        let mut route = route(
            vec![token_a(), token_b(), token_c(), token_d()],
            vec![
                single_pool_hop(
                    token_a(),
                    token_b(),
                    pool("p10", FixedRateSim::new(1).with_sell_limit(10)),
                ),
                single_pool_hop(
                    token_b(),
                    token_c(),
                    pool("p10b", FixedRateSim::new(1).with_sell_limit(10)),
                ),
                single_pool_hop(
                    token_c(),
                    token_d(),
                    pool(
                        "p5",
                        FixedRateSim::new(1)
                            .with_spot_price(0.01)
                            .with_sell_limit(5),
                    ),
                ),
            ],
        );

        let (limit, pools) = route.sell_amount_limit().unwrap();

        assert_eq!(limit, BigUint::from(5u8));
        assert_eq!(pools, vec!["p5".to_string()]);

        let error = route
            .sell(&BigUint::from(10u8))
            .expect_err("10 is over the route's limit of 5");
        let (limit, token_address, _) = expect_sell_amount_limit(error);
        assert_eq!(limit, BigUint::from(5u8));
        assert_eq!(token_address, token_a().address);

        route
            .sell(&BigUint::from(1u8))
            .expect("1 is under the route's limit");
    }

    #[test]
    fn test_route_sell_amount_limit_is_zero_when_an_intermediate_hop_is_empty() {
        // `TestSequentialRoute.test_get_sell_amount_limit` (:370, `intermediate_0=True`): a middle
        // hop that can absorb nothing takes the whole route to zero, and names itself as
        // the reason.
        let mut route = route(
            vec![token_a(), token_b(), token_c(), token_d()],
            vec![
                single_pool_hop(
                    token_a(),
                    token_b(),
                    pool("p10", FixedRateSim::new(1).with_sell_limit(10)),
                ),
                single_pool_hop(
                    token_b(),
                    token_c(),
                    pool("p0", FixedRateSim::new(1).with_sell_limit(0)),
                ),
                single_pool_hop(
                    token_c(),
                    token_d(),
                    pool(
                        "p5",
                        FixedRateSim::new(1)
                            .with_spot_price(0.01)
                            .with_sell_limit(5),
                    ),
                ),
            ],
        );

        let (limit, pools) = route.sell_amount_limit().unwrap();

        assert!(limit.is_zero());
        assert_eq!(pools, vec!["p0".to_string()]);

        let error = route
            .sell(&BigUint::from(10u8))
            .expect_err("nothing can be sold through an empty hop");
        let (limit, _, pools) = expect_sell_amount_limit(error);
        assert!(limit.is_zero());
        assert_eq!(pools, vec!["p0".to_string()]);
    }

    #[test]
    fn test_graph_sell_amount_limit_sums_branches_and_lists_their_pools() {
        // The graph level of `test_sell_over_limit` (:235): parallel capacity adds up.
        let mut graph = graph(
            vec![
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(token_a(), token_b(), limited_pool("p10", 10))],
                ),
                route(
                    vec![token_a(), token_b()],
                    vec![single_pool_hop(token_a(), token_b(), limited_pool("p50", 50))],
                ),
            ],
            vec![split(1, 2); 2],
        );

        let (limit, pools) = graph.sell_amount_limit().unwrap();
        assert_eq!(limit, BigUint::from(60u8));
        assert_eq!(pools, vec!["p10".to_string(), "p50".to_string()]);

        let error = graph
            .sell(&BigUint::from(70u8))
            .expect_err("70 is over the graph's limit of 60");

        let (limit, token_address, _) = expect_sell_amount_limit(error);
        assert_eq!(limit, BigUint::from(60u8));
        assert_eq!(token_address, token_a().address);
    }

    // ===================== Structural validation =====================

    #[test]
    fn test_graph_new_rejects_branches_with_different_endpoints() {
        // `TestParallelRoute.test_invalid_parallelism` (:265): branches that do not start and end
        // on the same tokens are not alternatives for the same order.
        let first = route(
            vec![token_a(), token_b()],
            vec![single_pool_hop(token_a(), token_b(), tenfold_pool("ab"))],
        );
        let second = route(
            vec![token_c(), token_b()],
            vec![single_pool_hop(token_c(), token_b(), tenfold_pool("cb"))],
        );

        let result = DecompositionGraph::from_routes(vec![first, second], vec![split(1, 2); 2]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_graph_new_rejects_branches_with_different_sell_tokens() {
        // The other half of `test_invalid_parallelism` (:265): sharing the buy token is not enough.
        let first = route(
            vec![token_a(), token_c()],
            vec![single_pool_hop(token_a(), token_c(), tenfold_pool("ac"))],
        );
        let second = route(
            vec![token_b(), token_c()],
            vec![single_pool_hop(token_b(), token_c(), tenfold_pool("bc"))],
        );

        let result = DecompositionGraph::from_routes(vec![first, second], vec![split(1, 2); 2]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    // ===================== Selling =====================

    #[test]
    fn test_hop_sell_splits_across_pools_and_charges_gas_only_where_it_traded() {
        // `TestParallelRoute.test_sell` (:167): splits of 0.4704 / 0.52 / 0.0096 / 0 over four
        // pools that each buy ten times what they are sold. Three of the four are touched,
        // so three units of gas are charged and the whole 10^10 is routed.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![tenfold_pool("p1"), tenfold_pool("p2"), tenfold_pool("p3"), tenfold_pool("p4")],
            vec![split_f64(0.4704), split_f64(0.52), split_f64(0.0096), Fraction::zero()],
        );

        let (bought, gas) = hop
            .sell(&BigUint::from(10u8).pow(10))
            .unwrap();

        assert_eq!(bought, BigUint::from(10u8).pow(11));
        assert_eq!(gas, BigUint::from(3u8));
        assert_eq!(hop.pools()[0].sell_amount(), &BigUint::from(4_704_000_000u64));
        assert_eq!(hop.pools()[3].sell_amount(), &BigUint::zero());
    }

    #[test]
    fn test_hop_sell_with_only_zero_splits_buys_nothing() {
        // `TestParallelRoute.test_sell_with_zero_splits` (:220): every pool exhausted. The hop
        // still records what it was asked to sell, but buys nothing and spends no gas.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![tenfold_pool("p1"), tenfold_pool("p2")],
            vec![Fraction::zero(), Fraction::zero()],
        );

        let (bought, gas) = hop.sell(&BigUint::from(100u8)).unwrap();

        assert!(bought.is_zero());
        assert!(gas.is_zero());
        assert_eq!(hop.sell_amount(), &BigUint::from(100u8));
        assert!(hop.buy_amount().is_zero());
    }

    #[test]
    fn test_route_sell_compounds_the_hop_multiples() {
        // `TestSequentialRoute.test_sell` (:294): the first hop's output is the second hop's input,
        // so two tenfold hops turn 10^9 into 10^11 and charge one gas each.
        let mut route = tenfold_route("hop", vec![token_a(), token_b(), token_c()]);

        let (bought, gas) = route
            .sell(&BigUint::from(10u8).pow(9))
            .unwrap();

        assert_eq!(bought, BigUint::from(10u8).pow(11));
        assert_eq!(gas, BigUint::from(2u8));
        assert_eq!(route.hop(1).sell_amount(), route.hop(0).buy_amount());
        assert_eq!(route.buy_amount(), &bought);
    }

    #[test]
    fn test_route_sell_stops_at_a_hop_that_buys_nothing() {
        // `TestSequentialRoute.test_inner_buy_0` (:346): the middle hop returns nothing, so the
        // last hop is sold zero and charges no gas while the two before it still do.
        let mut route = route(
            vec![token_a(), token_b(), token_c(), token_d()],
            vec![
                single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
                // Quotes a spot price like any other pool, but hands back nothing when actually
                // sold.
                single_pool_hop(
                    token_b(),
                    token_c(),
                    pool("bc", FixedRateSim::new(0).with_spot_price(1.0)),
                ),
                single_pool_hop(token_c(), token_d(), tenfold_pool("cd")),
            ],
        );

        let (bought, gas) = route
            .sell(&BigUint::from(10u8))
            .unwrap();

        assert!(bought.is_zero());
        assert_eq!(route.sell_amount(), &BigUint::from(10u8));
        assert_eq!(gas, BigUint::from(2u8));
        assert!(route.hop(2).gas().is_zero());
    }

    #[test]
    fn test_graph_sell_routes_a_quarter_down_each_diamond_branch() {
        // `conftest.py:87-135`'s four-branch WBTC -> USDC diamond at even splits. The branches are
        // one, two, three and two tenfold hops long, so 100 sold becomes
        // 25*(10 + 100 + 1000 + 100) = 30250 bought at 1 + 2 + 3 + 2 = 8 gas.
        let mut graph = diamond_graph();

        let (bought, gas) = graph
            .sell(&BigUint::from(100u8))
            .unwrap();

        assert_eq!(bought, BigUint::from(30_250u32));
        assert_eq!(gas, BigUint::from(8u8));
        assert_eq!(graph.sell_token().address, wbtc().address);
        assert_eq!(graph.buy_token().address, usdc().address);
        for branch in graph.branches() {
            assert_eq!(branch.sell_amount(), &BigUint::from(25u8));
        }
    }

    // ===================== Pool state is never mutated =====================

    #[test]
    fn test_pool_state_is_unchanged_after_selling() {
        // `test_bugfixing.py::test_curve_bug` (:18): solving must not write back into the pool
        // state it was handed. defibot compared a Curve pool's balances before and after
        // solving an order; the same check here is that the pre-trade state a pool exposes
        // still equals the state it was built from.
        let untouched = ConstantProductSim {
            reserve_0: BigUint::from(1_000_000u32),
            reserve_1: BigUint::from(2_000_000u32),
            gas: 50_000,
        };
        let mut hop = solved_hop(
            token(0x0A, "A"),
            token(0x0B, "B"),
            vec![PoolRef::new(
                "cp".to_string(),
                SellLimitKind::Enforced,
                Box::new(untouched.clone()),
                None,
            )],
            vec![Fraction::one()],
        );

        hop.sell(&BigUint::from(100_000u32))
            .unwrap();

        assert!(hop.pools()[0].new_state().is_some());
        assert!(ProtocolSim::eq(hop.pools()[0].state(), &untouched));
    }

    #[test]
    fn test_diamond_pool_states_are_unchanged_after_selling() {
        // The whole-solution form of `test_curve_bug` (:18): no branch writes back into any pool.
        let before: Vec<Box<dyn ProtocolSim>> = diamond_graph()
            .branches()
            .iter()
            .flat_map(|route| route.hops())
            .flat_map(|hop| hop.pools())
            .map(|pool| pool.state().clone_box())
            .collect();
        let mut graph = diamond_graph();

        graph
            .sell(&BigUint::from(100u8))
            .unwrap();

        let after: Vec<&dyn ProtocolSim> = graph
            .branches()
            .iter()
            .flat_map(|route| route.hops())
            .flat_map(|hop| hop.pools())
            .map(PoolRef::state)
            .collect();
        assert_eq!(before.len(), after.len());
        for (original, current) in before.iter().zip(after) {
            assert!(ProtocolSim::eq(current, original.as_ref()));
        }
    }

    // ===================== Unsolved estimates =====================

    #[test]
    fn test_hop_fee_is_the_mean_while_unsolved() {
        // `TestParallelRoute.test_attributes` (:253): without splits every pool weighs the same.
        let hop = hop(
            token_a(),
            token_b(),
            vec![
                pool("p1", FixedRateSim::new(1).with_fee(0.1)),
                pool("p2", FixedRateSim::new(1).with_fee(0.3)),
            ],
        );

        assert!((hop.fee() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn test_pool_weight_multiplies_inertia_fee_and_price() {
        // `TestSimpleRoute.test_attributes` (:137): `weight == inertia * (1 - fee) * route_price`.
        let depth = BigUint::from(4u8) * BigUint::from(10u8).pow(18);
        let pool = PoolRef::new(
            "p".to_string(),
            SellLimitKind::Enforced,
            Box::new(
                FixedRateSim::new(1)
                    .with_spot_price(3.0)
                    .with_fee(0.25),
            ),
            Some(depth),
        );
        let (token_in, token_out) = (token_a(), token_b());

        let weight = pool
            .weight(&token_in, &token_out)
            .unwrap();

        let expected = pool.inertia(&token_in) *
            (1.0 - pool.fee()) *
            pool.route_price(&token_in, &token_out)
                .unwrap();
        assert!((weight - 9.0).abs() < 1e-9);
        assert!((weight - expected).abs() < 1e-9);
    }

    // ===================== Blocked on later tasks =====================

    // defibot's `test_build_routes_graph_no_duplicate_pools_in_sequential_routes`
    // (`test_routes.py:631`) is a `build_routes_subgraph` test despite living in the route test
    // module: it solves USDT -> USDC and asserts each constructed `SequentialRoute` has no
    // repeated pool. defibot enforces that during construction via the `seen_pools` guard
    // (`order_solver.py:467-474`), not in the route type, so there is nothing to assert at this
    // level. The ported coverage is
    // `graph_build::tests::test_pool_reused_across_hops_is_skipped`.

    #[test]
    fn test_sell_with_coupled_paths_buys_less_than_independent_paths() {
        // `test_utils.py::test_sell_with_coupled_paths` (:16). Two branches over the *same* pools
        // cannot both trade against untouched liquidity: routing them as coupled must buy strictly
        // less than solving each in isolation and adding the results up.
        //
        // defibot builds the two branches by duplicating one WBTC -> WETH path from a recorded
        // market. The fixture pools here are constant-product rather than fixed-rate: a
        // pool with no price impact answers the same whether or not another branch traded
        // it first, so it could not tell the two routings apart.
        let coupled_graph = || {
            graph(
                vec![
                    route(
                        vec![token_a(), token_b(), token_c()],
                        vec![
                            single_pool_hop(token_a(), token_b(), cp_pool("ab", 1_000_000)),
                            single_pool_hop(token_b(), token_c(), cp_pool("bc", 1_000_000)),
                        ],
                    ),
                    route(
                        vec![token_a(), token_b(), token_c()],
                        vec![
                            single_pool_hop(token_a(), token_b(), cp_pool("ab", 1_000_000)),
                            single_pool_hop(token_b(), token_c(), cp_pool("bc", 1_000_000)),
                        ],
                    ),
                ],
                vec![split(1, 2); 2],
            )
        };
        let sell_amount = BigUint::from(1_000u32) * BigUint::from(10u8).pow(18);

        let (independent, _) = coupled_graph()
            .sell(&sell_amount)
            .unwrap();
        let mut coupled = coupled_graph();
        let (bought, _) = sell_with_coupled_paths(&mut coupled, &sell_amount).unwrap();

        assert!(bought < independent, "coupled {bought} should be below independent {independent}");
        assert_eq!(coupled.buy_amount(), &bought);
    }

    /// A constant-product pool with equal reserves of two 18-decimal tokens.
    fn cp_pool(id: &str, reserves: u64) -> PoolRef {
        let reserve = BigUint::from(reserves) * BigUint::from(10u8).pow(18);
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(ConstantProductSim {
                reserve_0: reserve.clone(),
                reserve_1: reserve,
                gas: 50_000,
            }),
            None,
        )
    }

    // ===================== Split rounding =====================

    #[test]
    fn test_hop_sell_loses_up_to_one_unit_per_pool_to_rounding() {
        // Each pool receives `floor(amount * split)`, so a hop over `P` pools can route up to `P -
        // 1` units less than it was handed even when the splits sum to exactly one. The
        // shortfall is not redistributed: it stays in the caller's balance.
        //
        // The encoder's convention is that a final leg with `split = 0.0` spends the entire
        // remaining balance, which reconciles the difference on-chain. Wiring the two
        // together is task 6's job; this test pins down exactly what this layer does so
        // that task has a number to work with.
        let mut hop = solved_hop(
            token_a(),
            token_b(),
            vec![tenfold_pool("p1"), tenfold_pool("p2"), tenfold_pool("p3")],
            vec![split(1, 3); 3],
        );

        let (bought, _) = hop.sell(&BigUint::from(100u8)).unwrap();

        // floor(100/3) = 33 into each pool: 99 routed, 1 unit left behind, 990 bought instead of
        // 1000.
        let routed: BigUint = hop
            .pools()
            .iter()
            .map(PoolRef::sell_amount)
            .sum();
        assert_eq!(routed, BigUint::from(99u8));
        assert_eq!(bought, BigUint::from(990u32));
        // The hop still reports the amount it was *asked* for, not the amount it routed.
        assert_eq!(hop.sell_amount(), &BigUint::from(100u8));
    }

    #[test]
    fn test_graph_sell_loses_rounding_at_every_level() {
        // The outer splits floor the same way, so a two-level solution can lose a unit per branch
        // and then another per pool inside each branch.
        let branch = |prefix: &str| {
            route(
                vec![token_a(), token_b()],
                vec![solved_hop(
                    token_a(),
                    token_b(),
                    vec![
                        tenfold_pool(&format!("{prefix}_1")),
                        tenfold_pool(&format!("{prefix}_2")),
                    ],
                    vec![split(1, 2); 2],
                )],
            )
        };
        let mut graph = graph(vec![branch("left"), branch("right")], vec![split(1, 3); 3 - 1]);
        graph
            .set_outer_splits(vec![split(1, 2); 2])
            .unwrap();

        let (bought, _) = graph
            .sell(&BigUint::from(101u8))
            .unwrap();

        // 101 -> 50 per branch (one unit lost) -> 25 per pool, so 100 of the 101 reaches a pool.
        let routed: BigUint = graph
            .branches()
            .iter()
            .flat_map(|route| route.hops())
            .flat_map(|hop| hop.pools())
            .map(PoolRef::sell_amount)
            .sum();
        assert_eq!(routed, BigUint::from(100u8));
        assert_eq!(bought, BigUint::from(1_000u32));
        assert_eq!(graph.sell_amount(), &BigUint::from(101u8));
    }

    /// Absolute difference between two exact rationals.
    fn distance(left: &BigRational, right: &BigRational) -> BigRational {
        (left - right).abs()
    }

    proptest! {
        /// [`Fraction::new`] returns the closest rational under the denominator bound.
        ///
        /// `limit_denominator` is a port of CPython's `Fraction.limit_denominator`, whose contract is
        /// exactly this: no other rational with a denominator of at most [`SPLIT_PRECISION`] is closer
        /// to the input. Split rounding decides how much of an order each pool gets, so a rounding
        /// that is merely close would silently misroute.
        #[test]
        fn test_fraction_new_is_the_closest_rational_under_the_bound(
            numerator in -1_000_000_000i64..1_000_000_000,
            denominator in 1i64..1_000_000_000,
            other_numerator in -2_000_000_000i64..2_000_000_000,
            other_denominator in 1i64..(SPLIT_PRECISION as i64),
        ) {
            let target = BigRational::new(BigInt::from(numerator), BigInt::from(denominator));
            let limited = Fraction::new(target.clone());
            let other = BigRational::new(BigInt::from(other_numerator), BigInt::from(other_denominator));

            prop_assert!(limited.as_ratio().denom() <= &BigInt::from(SPLIT_PRECISION));
            prop_assert!(
                distance(limited.as_ratio(), &target) <= distance(&other, &target),
                "{limited:?} is further from {target} than {other}",
            );
        }

        /// Rationals already under the bound are returned untouched.
        #[test]
        fn test_fraction_new_keeps_representable_values_exact(
            numerator in -1_000_000i64..1_000_000,
            denominator in 1i64..(SPLIT_PRECISION as i64),
        ) {
            let target = BigRational::new(BigInt::from(numerator), BigInt::from(denominator));

            let limited = Fraction::new(target.clone());

            prop_assert_eq!(limited.as_ratio(), &target);
        }
    }

    // ===================== Branch: the shared-prefix level =====================
    //
    // A `Branch` is `Sequential[head, Parallel[tails]]` (`order_solver.py:517-554`). Two things
    // about it are load-bearing and tested here: the level is **free** on a branch with one
    // tail — every composed quantity reproduces the plain token path it came from, to the bit —
    // and a branch with several tails holds its shared first hop once and sells it once.

    /// Asserts that a branch and the route it came from agree on a composed quantity.
    ///
    /// Not bit equality: the two group their `f64` multiplications differently — a route folds
    /// `p0 * p1 * p2` left to right while a branch computes `p0 * (p1 * p2)` — and `f64`
    /// multiplication is not associative, so the results can differ in the last unit in the
    /// last place. The collapse is exact in real arithmetic; this bounds the floating-point
    /// residue at a thousandth of a basis point, far below anything that could reorder two
    /// branches.
    fn assert_composes(branch: f64, route: f64, quantity: &str) {
        let tolerance = route.abs() * 1e-12;
        assert!(
            (branch - route).abs() <= tolerance,
            "branch {quantity} {branch} differs from the route's {route} by more than {tolerance}",
        );
    }

    /// A pool with an explicit depth, so `inertia` is something other than the missing-depth
    /// constant.
    ///
    /// The arithmetic fixtures' tokens all have 18 decimals, so the inertia is `whole` exactly.
    fn deep_pool(id: &str, sim: FixedRateSim, whole: u64) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(sim),
            Some(BigUint::from(whole) * BigUint::from(10u8).pow(18)),
        )
    }

    /// `A -> B -> C`: prices 2 and 5, fees 10% and 20%, depths 40 and 9.
    ///
    /// Every quantity the collapse has to preserve differs between the legs, so an implementation
    /// that silently used one leg's value for the pair would fail rather than coincide.
    fn asymmetric_two_hop() -> SequentialRoute {
        route(
            vec![token_a(), token_b(), token_c()],
            vec![
                single_pool_hop(
                    token_a(),
                    token_b(),
                    deep_pool("ab", FixedRateSim::new(2).with_fee(0.1), 40),
                ),
                single_pool_hop(
                    token_b(),
                    token_c(),
                    deep_pool("bc", FixedRateSim::new(5).with_fee(0.2), 9),
                ),
            ],
        )
    }

    /// `A -> B -> C -> D`, the deepest shape the algorithm builds.
    fn asymmetric_three_hop() -> SequentialRoute {
        route(
            vec![token_a(), token_b(), token_c(), token_d()],
            vec![
                single_pool_hop(
                    token_a(),
                    token_b(),
                    deep_pool("ab", FixedRateSim::new(2).with_fee(0.1), 40),
                ),
                single_pool_hop(
                    token_b(),
                    token_c(),
                    deep_pool("bc", FixedRateSim::new(5).with_fee(0.2), 9),
                ),
                single_pool_hop(
                    token_c(),
                    token_d(),
                    deep_pool("cd", FixedRateSim::new(3).with_fee(0.25), 21),
                ),
            ],
        )
    }

    /// A branch whose head feeds two tails: `A -> B` then either `B -> D` or `B -> C -> D`.
    ///
    /// The head is one object, so its pool can only be allocated once no matter how many tails read
    /// from it — which is the whole point of the level.
    fn two_tail_branch(tail_splits: Vec<Fraction>) -> Branch {
        branch(
            single_pool_hop(token_a(), token_b(), pool("ab", FixedRateSim::new(2))),
            vec![
                route(
                    vec![token_b(), token_d()],
                    vec![single_pool_hop(token_b(), token_d(), pool("bd", FixedRateSim::new(3)))],
                ),
                route(
                    vec![token_b(), token_c(), token_d()],
                    vec![
                        single_pool_hop(token_b(), token_c(), pool("bc", FixedRateSim::new(5))),
                        single_pool_hop(token_c(), token_d(), pool("cd", FixedRateSim::new(7))),
                    ],
                ),
            ],
            tail_splits,
        )
    }

    // ---------- The one-tail collapse ----------

    #[test]
    fn test_two_hop_branch_composes_like_the_route_it_came_from() {
        let route = asymmetric_two_hop();
        let branch = Branch::from_route(asymmetric_two_hop()).expect("a token path is a branch");

        assert_eq!(branch.sequences().len(), 1);
        assert_composes(branch.route_price().unwrap(), route.route_price().unwrap(), "route_price");
        assert_composes(
            branch.marginal_price().unwrap(),
            route.marginal_price().unwrap(),
            "marginal_price",
        );
        assert_composes(branch.fee(), route.fee(), "fee");
        assert_composes(branch.inertia(), route.inertia(), "inertia");
        assert_composes(branch.weight().unwrap(), route.weight().unwrap(), "weight");
        // Gas is exact: it sums integers.
        assert_eq!(branch.gas(), route.gas());
        assert_eq!(branch.minimum_gas(), route.minimum_gas());
    }

    #[test]
    fn test_three_hop_branch_composes_like_the_route_it_came_from() {
        let route = asymmetric_three_hop();
        let branch = Branch::from_route(asymmetric_three_hop()).expect("a token path is a branch");

        // The tail is itself two hops, so this exercises composition through both levels.
        assert_eq!(branch.sequences()[0].hops().len(), 2);
        assert_composes(branch.route_price().unwrap(), route.route_price().unwrap(), "route_price");
        assert_composes(
            branch.marginal_price().unwrap(),
            route.marginal_price().unwrap(),
            "marginal_price",
        );
        assert_composes(branch.fee(), route.fee(), "fee");
        assert_composes(branch.inertia(), route.inertia(), "inertia");
        assert_composes(branch.weight().unwrap(), route.weight().unwrap(), "weight");
    }

    #[test]
    fn test_two_hop_branch_composes_the_hop_quantities_defibot_composes() {
        // Spelled out against the fixture rather than against the route, so a matching bug in both
        // cannot hide: price 2 * 5, fee 1 - 0.9 * 0.8, inertia min(40, 9).
        let branch = Branch::from_route(asymmetric_two_hop()).expect("a token path is a branch");

        assert!((branch.route_price().unwrap() - 10.0).abs() < 1e-9);
        assert!((branch.fee() - 0.28).abs() < 1e-9);
        assert!((branch.inertia() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn test_one_hop_branch_delegates_its_weight_to_the_head() {
        // The delegation exists because the composed formula pairs the hop's *mean* price with its
        // *maximum* inertia and can score above every pool in it, inflating single-hop branches.
        // A deep pool at a poor price beside a shallow one at a good price. The maximum over the
        // pools is 100 * 2; the composed formula pairs the *mean* price of 11 with the
        // *maximum* inertia of 100 and lands at 1100, above either pool.
        let leg = hop(
            token_a(),
            token_b(),
            vec![
                deep_pool("deep", FixedRateSim::new(2).with_spot_price(2.0), 100),
                deep_pool("pricey", FixedRateSim::new(20).with_spot_price(20.0), 1),
            ],
        );
        let expected = leg.weight().unwrap();
        let branch = Branch::from_route(route(vec![token_a(), token_b()], vec![leg]))
            .expect("a token path is a branch");

        assert!(branch.sequences().is_empty());
        assert_eq!(branch.weight().unwrap(), expected);
        // The composed formula would score above the best pool; the delegation must not.
        let composed = branch.route_price().unwrap() * (1.0 - branch.fee()) * branch.inertia();
        assert!(composed > expected, "the fixture must make the two formulas disagree");
    }

    #[test]
    fn test_one_tail_branch_sells_exactly_like_the_route_it_came_from() {
        let mut route = asymmetric_two_hop();
        let mut branch =
            Branch::from_route(asymmetric_two_hop()).expect("a token path is a branch");
        let amount = BigUint::from(1_000u32);

        let from_route = route.sell(&amount).unwrap();
        let from_branch = branch.sell(&amount).unwrap();

        assert_eq!(from_branch, from_route);
        assert_eq!(branch.buy_amount(), route.buy_amount());
        assert_eq!(branch.sell_amount(), route.sell_amount());
        assert_eq!(
            branch
                .new_marginal_price()
                .expect("sold on"),
            route
                .new_marginal_price()
                .expect("sold on")
        );
    }

    #[test]
    fn test_one_tail_branch_reports_the_same_sell_limit_as_its_route() {
        // The limit is hit at the *second* leg, so it has to travel back through the head's price —
        // the composition most at risk of drifting from `cast_to_sell_token`.
        let build = || {
            route(
                vec![token_a(), token_b(), token_c()],
                vec![
                    single_pool_hop(
                        token_a(),
                        token_b(),
                        pool("ab", FixedRateSim::new(2).with_sell_limit(10_000_000)),
                    ),
                    single_pool_hop(
                        token_b(),
                        token_c(),
                        pool("bc", FixedRateSim::new(5).with_sell_limit(1_000)),
                    ),
                ],
            )
        };
        let mut route = build();
        let mut branch = Branch::from_route(build()).expect("a token path is a branch");

        let (route_limit, _) = route.sell_amount_limit().unwrap();
        let (branch_limit, _) = branch.sell_amount_limit().unwrap();

        // 1000 units of B cast back through the head's price of 2 is 500 units of A, which binds
        // well below the head's own limit.
        assert_eq!(branch_limit, route_limit);
        assert_eq!(branch_limit, BigUint::from(500u32));
    }

    #[test]
    fn test_one_tail_branch_is_solved_exactly_when_its_route_is() {
        let solved = Branch::from_route(asymmetric_two_hop()).expect("a token path is a branch");
        assert!(asymmetric_two_hop().solved() && solved.solved());

        let unsolved_route = route(
            vec![token_a(), token_b(), token_c()],
            vec![
                single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
                hop(token_b(), token_c(), vec![tenfold_pool("bc1"), tenfold_pool("bc2")]),
            ],
        );
        let unsolved = Branch::from_route(unsolved_route).expect("a token path is a branch");

        assert!(!unsolved.solved());
        assert!(unsolved.splits().is_empty());
    }

    // ---------- A branch with several tails ----------

    #[test]
    fn test_branch_holds_its_head_pool_once_for_every_tail() {
        let branch = two_tail_branch(vec![split(1, 4), split(3, 4)]);

        // One head pool, and it appears once in the branch's hops even though two tails read from
        // it.
        let head_pools: Vec<&str> = branch
            .hops()
            .flat_map(|hop| hop.pools())
            .map(|pool| pool.component_id().as_str())
            .filter(|id| *id == "ab")
            .collect();
        assert_eq!(head_pools, ["ab"]);
        assert_eq!(branch.hop().pools().len(), 1);
        assert_eq!(branch.sequences().len(), 2);
    }

    #[test]
    fn test_branch_sells_its_head_once_and_splits_its_output_across_the_tails() {
        // This is the allocation bug in miniature. The head is sold for the branch's whole amount,
        // and the tails divide what came *out* of it — not the input, and not once each.
        let mut branch = two_tail_branch(vec![split(1, 4), split(3, 4)]);

        let (bought, _) = branch
            .sell(&BigUint::from(1_000u32))
            .unwrap();

        // The head pool saw the full 1000, not 250 and 750 in two independent sells.
        assert_eq!(branch.hop().pools()[0].sell_amount(), &BigUint::from(1_000u32));
        assert_eq!(branch.hop().buy_amount(), &BigUint::from(2_000u32));
        // A quarter of the head's 2000 output down `B -> D` at x3, three quarters down `B -> C ->
        // D` at x5 then x7: 500 * 3 + 1500 * 35.
        assert_eq!(branch.sequences()[0].sell_amount(), &BigUint::from(500u32));
        assert_eq!(branch.sequences()[1].sell_amount(), &BigUint::from(1_500u32));
        assert_eq!(bought, BigUint::from(1_500u32 + 52_500u32));
    }

    #[test]
    fn test_branch_sell_limit_composes_the_head_against_the_summed_tails() {
        // The tails are parallel, so their limits *sum*; the head is in series with them, so the
        // branch takes the smaller of its own limit and the summed one cast back through it.
        let branch = |head_limit: u64| {
            branch(
                single_pool_hop(
                    token_a(),
                    token_b(),
                    pool("ab", FixedRateSim::new(2).with_sell_limit(head_limit)),
                ),
                vec![
                    route(
                        vec![token_b(), token_d()],
                        vec![single_pool_hop(
                            token_b(),
                            token_d(),
                            pool("bd", FixedRateSim::new(3).with_sell_limit(600)),
                        )],
                    ),
                    route(
                        vec![token_b(), token_d()],
                        vec![single_pool_hop(
                            token_b(),
                            token_d(),
                            pool("bd2", FixedRateSim::new(3).with_sell_limit(400)),
                        )],
                    ),
                ],
                vec![split(1, 2); 2],
            )
        };

        // Tails absorb 600 + 400 = 1000 units of B, which is 500 units of A through the head's
        // price of 2. With a roomy head that binds; with a tight head the head binds.
        let (tails_bind, _) = branch(100_000)
            .sell_amount_limit()
            .unwrap();
        let (head_binds, _) = branch(300).sell_amount_limit().unwrap();

        assert_eq!(tails_bind, BigUint::from(500u32));
        assert_eq!(head_binds, BigUint::from(300u32));
    }

    #[test]
    fn test_branch_sell_limit_is_zero_when_no_tail_can_absorb_anything() {
        let mut branch = branch(
            single_pool_hop(token_a(), token_b(), pool("ab", FixedRateSim::new(2))),
            vec![route(
                vec![token_b(), token_d()],
                vec![single_pool_hop(
                    token_b(),
                    token_d(),
                    pool("bd", FixedRateSim::new(3).with_sell_limit(0)),
                )],
            )],
            vec![Fraction::one()],
        );

        let (limit, _) = branch.sell_amount_limit().unwrap();

        // A dead tail takes the whole branch to zero however roomy the head is: nothing that goes
        // in can come out the far side.
        assert!(limit.is_zero());
    }

    #[test]
    fn test_branch_new_marginal_price_multiplies_the_head_by_the_split_weighted_tails() {
        let mut branch = two_tail_branch(vec![split(1, 4), split(3, 4)]);
        branch
            .sell(&BigUint::from(1_000u32))
            .unwrap();

        let price = branch
            .new_marginal_price()
            .expect("every leg was sold on");

        // Head 2, tails 3 and 5 * 7 = 35: 2 * (3 * 0.25 + 35 * 0.75).
        assert!((price - 54.0).abs() < 1e-9);
    }

    #[test]
    fn test_branch_new_marginal_price_is_none_when_the_head_was_not_sold() {
        // `None` has to propagate from the head as well as from a tail, and the head is the side a
        // sequential product is most likely to drop. Everything is sold first and then only the
        // head's post-trade state is cleared, so nothing but the head can be the reason for
        // the `None`.
        let mut branch = two_tail_branch(vec![split(1, 4), split(3, 4)]);
        branch
            .sell(&BigUint::from(1_000u32))
            .unwrap();
        assert!(branch.new_marginal_price().is_some());

        branch
            .hop_mut()
            .sell(&BigUint::zero())
            .unwrap();

        assert_eq!(branch.new_marginal_price(), None);
    }

    #[test]
    fn test_tail_less_branch_new_marginal_price_is_none_until_its_head_is_sold() {
        let mut branch = branch(
            single_pool_hop(token_a(), token_b(), pool("ab", FixedRateSim::new(2))),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(branch.new_marginal_price(), None);

        branch
            .sell(&BigUint::from(1_000u32))
            .unwrap();

        assert_eq!(branch.new_marginal_price(), Some(2.0));
    }

    #[test]
    fn test_branch_new_marginal_price_is_none_when_a_tail_carrying_flow_has_none() {
        let mut branch = two_tail_branch(vec![split(1, 4), split(3, 4)]);
        branch
            .sell(&BigUint::from(1_000u32))
            .unwrap();

        // Selling zero on a tail clears its post-trade state while its split stays non-zero.
        branch.sequences_mut()[1]
            .sell(&BigUint::zero())
            .unwrap();

        assert_eq!(branch.new_marginal_price(), None);
    }

    #[test]
    fn test_branch_new_marginal_price_skips_tails_on_a_zero_split() {
        // A tail the search settled on at zero carries no flow, so it must not be able to veto the
        // price with a missing post-trade state.
        let mut branch = two_tail_branch(vec![Fraction::zero(), Fraction::one()]);
        branch
            .sell(&BigUint::from(1_000u32))
            .unwrap();

        let price = branch
            .new_marginal_price()
            .expect("the tail carrying flow was sold on");

        assert!((price - 70.0).abs() < 1e-9);
    }

    #[test]
    fn test_branch_is_unsolved_until_its_tails_are_split() {
        let mut branch = two_tail_branch(Vec::new());
        assert!(!branch.solved());

        branch
            .set_splits(vec![split(1, 2); 2])
            .unwrap();

        assert!(branch.solved());
    }

    #[test]
    fn test_branch_selling_before_its_tails_are_split_is_an_error() {
        let mut branch = two_tail_branch(Vec::new());

        let result = branch.sell(&BigUint::from(1_000u32));

        assert!(matches!(result, Err(DecompositionError::Unsolved { .. })));
    }

    #[test]
    fn test_branch_rejects_tail_splits_that_do_not_match_its_tails() {
        let mut branch = two_tail_branch(Vec::new());

        let result = branch.set_splits(vec![Fraction::one()]);

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_branch_rejects_a_tail_that_does_not_start_where_the_head_ends() {
        let result = Branch::head(
            single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
            vec![route(
                vec![token_c(), token_d()],
                vec![single_pool_hop(token_c(), token_d(), tenfold_pool("cd"))],
            )],
            Vec::new(),
        );

        assert!(matches!(result, Err(DecompositionError::InvalidStructure { .. })));
    }

    #[test]
    fn test_hop_remove_pool_leaves_the_hop_unsolved_and_reports_what_is_left() {
        let mut leg = solved_hop(
            token_a(),
            token_b(),
            vec![tenfold_pool("first"), tenfold_pool("second")],
            vec![split(1, 2); 2],
        );

        assert!(leg.remove_pool(&"first".to_string()));

        assert_eq!(leg.pools().len(), 1);
        assert_eq!(leg.pools()[0].component_id(), "second");
        // The splits were sized for two pools; keeping them would reroute the removed pool's share.
        assert!(!leg.solved());
    }

    #[test]
    fn test_hop_remove_pool_reports_when_nothing_is_left() {
        let mut leg = single_pool_hop(token_a(), token_b(), tenfold_pool("only"));

        assert!(!leg.remove_pool(&"only".to_string()));
    }

    /// A pool trading at `spot_price` with no fee.
    fn no_fee_pool(id: &str, spot_price: f64) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(MockProtocolSim::new(spot_price).with_fee(0.0)),
            None,
        )
    }

    /// A one-pool hop between two tokens, already solved — its single pool takes everything.
    ///
    /// Selling on a branch requires every hop below it to be solved, so the fixtures build them
    /// that way rather than having each test set the same split.
    fn solved_leg(id: &str, token_in: &Token, token_out: &Token) -> Hop {
        let mut hop = Hop::new(token_in.clone(), token_out.clone(), vec![no_fee_pool(id, 2.0)])
            .expect("hop has pools");
        hop.set_splits(vec![Fraction::one()])
            .expect("one split for one pool");
        hop
    }

    /// A token path through `tokens`, one pool per leg, pool ids taken from `ids`.
    fn token_path(tokens: &[Token], ids: &[&str]) -> SequentialRoute {
        let hops = tokens
            .windows(2)
            .zip(ids)
            .map(|(pair, id)| solved_leg(id, &pair[0], &pair[1]))
            .collect();
        SequentialRoute::new(tokens.to_vec(), hops).expect("route matches its token path")
    }

    /// `A`, `B`, `C`, `D`, `X` — `A` sells, `X` buys.
    fn five_tokens() -> (Token, Token, Token, Token, Token) {
        (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"), token(0x0D, "D"), token(0x11, "X"))
    }

    // ==================== the tail-grouped branch ====================

    #[test]
    fn test_tail_grouped_branch_reports_the_outer_tokens() {
        let (a, b, c, _d, x) = five_tokens();
        let branch = Branch::tail(
            solved_leg("cx", &c, &x),
            vec![token_path(&[a.clone(), b, c], &["ab", "bc"])],
            Vec::new(),
        )
        .expect("sequences end where the hop starts");

        assert_eq!(branch.side(), BranchSide::Tail);
        assert_eq!(branch.sell_token().symbol, a.symbol);
        assert_eq!(branch.buy_token().symbol, x.symbol);
    }

    #[test]
    fn test_tail_grouped_branch_walks_its_hops_in_flow_order() {
        let (a, b, c, _d, x) = five_tokens();
        let branch = Branch::tail(
            solved_leg("cx", &c, &x),
            vec![token_path(&[a, b, c], &["ab", "bc"])],
            Vec::new(),
        )
        .expect("sequences end where the hop starts");

        // The shared hop is last, not first: flow order, so assembly and loop removal read it
        // right.
        let symbols: Vec<String> = branch
            .hops()
            .map(|hop| format!("{}->{}", hop.token_in().symbol, hop.token_out().symbol))
            .collect();
        assert_eq!(symbols, vec!["A->B", "B->C", "C->X"]);
    }

    #[test]
    fn test_tail_grouped_label_renders_the_sequences_before_the_hop() {
        let (a, b, c, d, x) = five_tokens();
        let branch = Branch::tail(
            solved_leg("dx", &d, &x),
            vec![
                token_path(&[a.clone(), b, d.clone()], &["ab", "bd"]),
                token_path(&[a, c, d], &["ac", "cd"]),
            ],
            Vec::new(),
        )
        .expect("sequences end where the hop starts");

        assert_eq!(branch.token_path_label(), "[A->B | A->C]->D->X");
    }

    #[test]
    fn test_tail_grouped_branch_rejects_a_sequence_that_ends_elsewhere() {
        let (a, b, c, d, x) = five_tokens();
        let result = Branch::tail(
            solved_leg("dx", &d, &x),
            vec![token_path(&[a, b, c], &["ab", "bc"])],
            Vec::new(),
        );

        assert!(
            matches!(result, Err(DecompositionError::InvalidStructure { .. })),
            "a sequence ending at C must not feed a hop starting at D"
        );
    }

    #[test]
    fn test_tail_grouped_branch_rejects_an_empty_sequence_set() {
        let (_a, _b, _c, d, x) = five_tokens();
        let result = Branch::tail(solved_leg("dx", &d, &x), Vec::new(), Vec::new());

        assert!(
            matches!(result, Err(DecompositionError::InvalidStructure { .. })),
            "a tail-grouped branch needs something feeding its hop"
        );
    }

    #[test]
    fn test_tail_grouped_branch_sells_its_hop_once_for_the_whole_flow() {
        let (a, b, c, d, x) = five_tokens();
        let mut branch = Branch::tail(
            solved_leg("dx", &d, &x),
            vec![
                token_path(&[a.clone(), b, d.clone()], &["ab", "bd"]),
                token_path(&[a, c, d], &["ac", "cd"]),
            ],
            vec![Fraction::one(), Fraction::zero()],
        )
        .expect("sequences end where the hop starts");

        let (bought, _gas) = branch
            .sell(&BigUint::from(100u32))
            .expect("the branch sells");

        // Both sequences feed one hop, so the hop's own sell amount is their combined output rather
        // than either sequence's share. That is what stops the two from each claiming its
        // liquidity.
        let into_hop: BigUint = branch
            .sequences()
            .iter()
            .map(SequentialRoute::buy_amount)
            .sum();
        assert_eq!(branch.hop().sell_amount(), &into_hop);
        assert_eq!(branch.buy_amount(), &bought);
        assert!(!bought.is_zero());
    }
}
