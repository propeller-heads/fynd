//! Test helpers for split-routing algorithm split_scenarios.

use std::sync::Arc;

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use rustc_hash::FxHashMap;
use tokio::sync::RwLock;
use tycho_simulation::{
    tycho_core::{
        models::{token::Token, Address},
        simulation::protocol_sim::{Price, ProtocolSim},
    },
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

use crate::{
    algorithm::{
        most_liquid::DepthAndPrice,
        test_utils::{component, setup_market_unweighted, token_with_decimals, ConstantProductSim},
        Algorithm,
    },
    derived::{DerivedData, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketState},
    graph::{petgraph::PetgraphStableDiGraphManager, GraphManager, TopologyGraphManager},
    types::{
        quote::{Order, OrderSide, Route},
        BlockInfo, RouteResult,
    },
};

/// Returns `(fraction_for_component_1, total_output)` — the theoretically optimal output when
/// splitting `trade_amount` between two constant-product components with no fees.
///
/// Finds the split where both components offer the same marginal rate on the last unit traded.
/// Negative allocations are clamped to `0` — a clamped value means the full trade routes through
/// the other component.
pub(crate) fn optimal_two_component_output(
    reserve_in_1: f64,
    reserve_out_1: f64,
    reserve_in_2: f64,
    reserve_out_2: f64,
    trade_amount: f64,
) -> (f64, f64) {
    let d = ((reserve_in_1 * reserve_out_1) / (reserve_in_2 * reserve_out_2)).sqrt();
    let a2 =
        ((trade_amount + reserve_in_1 - d * reserve_in_2) / (d + 1.0)).clamp(0.0, trade_amount);
    let a1 = trade_amount - a2;

    let fraction_1 = a1 / trade_amount;
    let out_1 = a1 * reserve_out_1 / (reserve_in_1 + a1);
    let out_2 = a2 * reserve_out_2 / (reserve_in_2 + a2);

    (fraction_1, out_1 + out_2)
}

// ==================== Weighted two-component market for split-algorithm tests ====================

/// A WETH/USDC market for the `DepthAndPrice` split algorithm (`split`). The two
/// components are fee-free constant-product components, so their optimal split matches
/// [`optimal_two_component_output`]. One `MarketState` backs the whole market.
pub(crate) struct WeightedSplitMarket {
    pub market: MarketData,
    pub weighted: TopologyGraphManager<DepthAndPrice>,
    pub derived: SharedDerivedDataRef,
    pub weth: Address,
    pub usdc: Address,
}

/// Per-component reserves for [`two_equal_weth_usdc`], in whole tokens (WETH has 18 decimals, USDC
/// 6).
pub(crate) const TWO_EQUAL_WETH_RESERVE: u128 = 1000;
pub(crate) const TWO_EQUAL_USDC_RESERVE: u128 = 3_000_000;

fn weth_usdc_component() -> ConstantProductSim {
    ConstantProductSim {
        // reserve_0 is the lower-address token (WETH, 0x01); reserve_1 is USDC (0x02).
        reserve_0: BigUint::from(TWO_EQUAL_WETH_RESERVE) * BigUint::from(10u64).pow(18),
        reserve_1: BigUint::from(TWO_EQUAL_USDC_RESERVE) * BigUint::from(10u64).pow(6),
        gas: 50_000,
    }
}

/// Builds two equally-deep, fee-free WETH/USDC constant-product components (1000 WETH / 3,000,000
/// USDC each) at `gas_price` wei/gas. Derived data carries unit token gas prices so the split
/// algorithms deduct gas in output-token terms.
pub(crate) fn two_equal_weth_usdc(gas_price: u64) -> WeightedSplitMarket {
    let weth = token_with_decimals(0x01, "WETH", 18);
    let usdc = token_with_decimals(0x02, "USDC", 6);

    let mut market = MarketState::new();
    market.update_gas_price(BlockGasPrice {
        block_number: 1,
        block_hash: Default::default(),
        block_timestamp: 0,
        pricing: GasPrice::Legacy { gas_price: BigUint::from(gas_price) },
    });
    market.update_last_updated(BlockInfo::new(1, "0x01".to_string(), 0));

    let tokens = [weth.clone(), usdc.clone()];
    let mut edge_weights = Vec::new();
    for component_id in ["component_a", "component_b"] {
        let sim = weth_usdc_component();
        let weight_to = DepthAndPrice::from_protocol_sim(&sim, &weth, &usdc).unwrap();
        let weight_from = DepthAndPrice::from_protocol_sim(&sim, &usdc, &weth).unwrap();
        market.upsert_components(std::iter::once(component(component_id, &tokens)));
        market.update_states([(component_id.to_string(), Box::new(sim) as Box<dyn ProtocolSim>)]);
        market.upsert_tokens(tokens.clone());
        edge_weights.push((component_id, weight_to, weight_from));
    }

    let mut weighted = TopologyGraphManager::<DepthAndPrice>::default();
    weighted.initialize_graph(&market.component_topology());
    for (component_id, weight_to, weight_from) in edge_weights {
        weighted
            .set_pool_weight(
                &component_id.to_string(),
                &weth.address,
                &usdc.address,
                weight_to,
                false,
            )
            .unwrap();
        weighted
            .set_pool_weight(
                &component_id.to_string(),
                &usdc.address,
                &weth.address,
                weight_from,
                false,
            )
            .unwrap();
    }

    let unit_price = Price::new(BigUint::from(1u64), BigUint::from(1u64));
    let mut token_prices = FxHashMap::default();
    token_prices.insert(weth.address.clone(), unit_price.clone());
    token_prices.insert(usdc.address.clone(), unit_price);
    let mut derived = DerivedData::new();
    derived.set_token_prices(token_prices, vec![], 1, true);

    WeightedSplitMarket {
        weth: weth.address.clone(),
        usdc: usdc.address.clone(),
        market: MarketData::new(Arc::new(RwLock::new(market))),
        weighted,
        derived: Arc::new(RwLock::new(derived)),
    }
}

/// Extracts `(net_amount_out, path_count, gross_output)` from a split result. `path_count` counts
/// swaps consuming `token_in` (one per parallel leg); `gross` sums swaps producing `token_out`.
pub(crate) fn split_metrics(
    result: &RouteResult,
    token_in: &Address,
    token_out: &Address,
) -> (BigInt, usize, BigUint) {
    let swaps = result.route().swaps();
    let path_count = swaps
        .iter()
        .filter(|s| s.token_in() == token_in)
        .count();
    let gross = swaps
        .iter()
        .filter(|s| s.token_out() == token_out)
        .fold(BigUint::zero(), |acc, s| acc + s.amount_out());
    (result.net_amount_out().clone(), path_count, gross)
}

// ==================== Scenario harness ====================

/// A component entry in a `TestScenario`.
pub(crate) struct ScenarioComponent {
    pub id: &'static str,
    pub token_1: Token,
    pub token_2: Token,
    pub sim: Box<dyn ProtocolSim>,
}

/// A self-contained algorithm test case with pre-computed bounds.
///
/// Both bounds are net output amounts (gross minus gas cost), hardcoded from the scenario's fixed
/// reserves. Gas cost uses the test market's fixed assumptions: 100 wei/gas, 1 output-token = 1
/// ETH.
pub(crate) struct TestScenario {
    pub name: &'static str,
    pub description: &'static str,
    pub components: Vec<ScenarioComponent>,
    pub token_in: Token,
    pub token_out: Token,
    pub trade_amount: BigUint,
    /// Floor: the algorithm must produce at least this much net output.
    pub lower_bound: BigInt,
    /// Target: the best net output achievable under the scenario's simplified component model. A
    /// quality ceiling to measure against, not a hard constraint.
    pub analytical_optimum: BigInt,
}

impl TestScenario {
    /// Builds an unweighted `MarketData` + graph manager from this scenario's component
    /// definitions.
    pub(crate) fn build_market(&self) -> (MarketData, PetgraphStableDiGraphManager<()>) {
        let components = self
            .components
            .iter()
            .map(|p| (p.id, &p.token_1, &p.token_2, p.sim.clone_box()))
            .collect();
        setup_market_unweighted(components)
    }

    /// Builds a `DepthAndPrice`-weighted `MarketData` + graph manager from this scenario's
    /// component definitions, for algorithms that route on the weighted graph (`water_fill`,
    /// Most Liquid). Uses the same fixed gas assumptions as
    /// [`build_market`](Self::build_market) (100 wei/gas).
    pub(crate) fn build_market_weighted(
        &self,
    ) -> (MarketData, TopologyGraphManager<DepthAndPrice>) {
        let mut market = MarketState::new();
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(100u64) },
        });
        market.update_last_updated(BlockInfo::new(1, "0x00".to_string(), 0));

        let mut edge_weights = Vec::new();
        for scenario in &self.components {
            let tokens = [scenario.token_1.clone(), scenario.token_2.clone()];
            let weight_to = DepthAndPrice::from_protocol_sim(
                scenario.sim.as_ref(),
                &scenario.token_1,
                &scenario.token_2,
            )
            .unwrap();
            let weight_from = DepthAndPrice::from_protocol_sim(
                scenario.sim.as_ref(),
                &scenario.token_2,
                &scenario.token_1,
            )
            .unwrap();
            market.upsert_components(std::iter::once(component(scenario.id, &tokens)));
            market.update_states([(scenario.id.to_string(), scenario.sim.clone_box())]);
            market.upsert_tokens(tokens.to_vec());
            edge_weights.push((
                scenario.id,
                scenario.token_1.address.clone(),
                scenario.token_2.address.clone(),
                weight_to,
                weight_from,
            ));
        }

        let mut weighted = TopologyGraphManager::<DepthAndPrice>::default();
        weighted.initialize_graph(&market.component_topology());
        for (component_id, addr_1, addr_2, weight_to, weight_from) in edge_weights {
            weighted
                .set_pool_weight(&component_id.to_string(), &addr_1, &addr_2, weight_to, false)
                .unwrap();
            weighted
                .set_pool_weight(&component_id.to_string(), &addr_2, &addr_1, weight_from, false)
                .unwrap();
        }

        (MarketData::new(Arc::new(RwLock::new(market))), weighted)
    }

    /// Builds a `SharedDerivedDataRef` with unit token-gas-prices for every token in this
    /// scenario, matching the `TestScenario` gas assumption (1 output-token = 1 ETH). This lets
    /// BF's `compute_net_amount_out` deduct gas costs.
    pub(crate) fn build_derived_data(&self) -> SharedDerivedDataRef {
        let unit_price = Price::new(BigUint::from(1u64), BigUint::from(1u64));
        let mut token_prices = FxHashMap::default();

        token_prices.insert(self.token_in.address.clone(), unit_price.clone());
        token_prices.insert(self.token_out.address.clone(), unit_price.clone());
        for component in &self.components {
            token_prices
                .entry(component.token_1.address.clone())
                .or_insert_with(|| unit_price.clone());
            token_prices
                .entry(component.token_2.address.clone())
                .or_insert_with(|| unit_price.clone());
        }

        let mut derived = DerivedData::new();
        derived.set_token_prices(token_prices, vec![], 1, true);
        Arc::new(RwLock::new(derived))
    }
}

// ==================== Evaluation harness ====================

/// Results from running one algorithm against one scenario.
pub(crate) struct ScenarioResult {
    pub scenario_name: &'static str,
    pub algorithm_name: String,
    /// The route returned by the algorithm. `None` when routing failed.
    pub route: Option<Route>,
    /// Gross output minus gas costs. Can be negative when gas exceeds proceeds.
    pub net_output: BigInt,
    /// Best single-route net output. `net_output` must be >= this.
    pub lower_bound: BigInt,
    /// Net analytical optimum for the scenario's simplified component model. A reference value for
    /// measuring quality — the algorithm is not required to reach it.
    pub analytical_optimum: BigInt,
    /// Number of swaps consuming `token_in`. 1 for single-route, N for a split.
    pub path_count: usize,
}

impl ScenarioResult {
    pub fn assert_meets_lower_bound(&self) {
        self.assert_bound(self.net_output >= self.lower_bound, ">=");
    }

    pub fn assert_beats_lower_bound(&self) {
        self.assert_bound(self.net_output > self.lower_bound, ">");
    }

    /// Asserts `>= lower_bound` when optimum equals it, `> lower_bound` otherwise.
    pub fn assert_passes_lower_bound(&self) {
        if self.analytical_optimum == self.lower_bound {
            self.assert_meets_lower_bound();
        } else {
            self.assert_beats_lower_bound();
        }
    }

    /// Returns true if `net_output >= (100 - threshold_pct)% of analytical_optimum`.
    /// `threshold_pct` is a whole-number percentage (e.g. `5` means within 5% of optimum).
    pub fn within_pct_of_optimum(&self, threshold_pct: u32) -> bool {
        &self.net_output * BigInt::from(100u32) >=
            &self.analytical_optimum * BigInt::from(100 - threshold_pct)
    }

    fn assert_bound(&self, passes: bool, op: &str) {
        assert!(
            passes,
            "scenario '{}' algorithm '{}': net_output {} {op} lower_bound {} failed",
            self.scenario_name, self.algorithm_name, self.net_output, self.lower_bound,
        );
    }
}

/// Run an algorithm against a single scenario and return structured results.
///
/// Builds derived data with unit token-gas-prices so the algorithm deducts gas costs from output,
/// matching the scenario's net bounds. Calls `find_best_route` and returns a [`ScenarioResult`].
///
/// ```rust,ignore
/// let scenario = split_scenarios::symmetric_split();
/// let (market, gm) = scenario.build_market();
/// let result = evaluate_scenario(&algo, &scenario, market, gm).await;
/// result.assert_beats_lower_bound();
/// ```
pub(crate) async fn evaluate_scenario<A>(
    algo: &A,
    scenario: &TestScenario,
    market: MarketData,
    graph_manager: A::GraphManager,
) -> ScenarioResult
where
    A: Algorithm,
{
    let order = Order::new(
        scenario.token_in.address.clone(),
        scenario.token_out.address.clone(),
        scenario.trade_amount.clone(),
        OrderSide::Sell,
        Address::default(),
    );

    let derived = scenario.build_derived_data();
    let lower_bound = scenario.lower_bound.clone();
    let analytical_optimum = scenario.analytical_optimum.clone();

    let Ok(route_result) = algo
        .find_best_route(graph_manager.graph(), market, None, Some(derived), &order)
        .await
    else {
        return ScenarioResult {
            scenario_name: scenario.name,
            algorithm_name: algo.name().to_string(),
            route: None,
            net_output: BigInt::zero(),
            lower_bound,
            analytical_optimum,
            path_count: 0,
        };
    };

    let token_in_addr = &scenario.token_in.address;

    // Split fractions are set on swaps sharing the same token_in.
    let input_swaps: Vec<_> = route_result
        .route()
        .swaps()
        .iter()
        .filter(|s| s.token_in() == token_in_addr)
        .collect();

    let net_output = route_result.net_amount_out().clone();
    let path_count = input_swaps.len();
    let route = route_result.route().clone();

    ScenarioResult {
        scenario_name: scenario.name,
        algorithm_name: algo.name().to_string(),
        route: Some(route),
        net_output,
        lower_bound,
        analytical_optimum,
        path_count,
    }
}

// ==================== Named split_scenarios ====================

pub(crate) mod split_scenarios {
    use num_bigint::{BigInt, BigUint};

    use super::{ScenarioComponent, TestScenario};
    use crate::algorithm::test_utils::{token, ConstantProductSim, ONE_ETH};

    /// Valid Ethereum addresses the swap encoder registry recognizes
    const COMPONENT_ADDR_1: &str = "0xaB00000000000000000000000000000000000001";
    const COMPONENT_ADDR_2: &str = "0xaB00000000000000000000000000000000000002";
    const COMPONENT_ADDR_3: &str = "0xaB00000000000000000000000000000000000003";
    const COMPONENT_ADDR_4: &str = "0xaB00000000000000000000000000000000000004";

    /// S1: two identical A→B components; 50/50 split is optimal.
    ///
    /// `analytical_optimum`: `optimal_two_component_output` with equal reserves — exact
    /// mathematical optimum (50/50).
    pub(crate) fn symmetric_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let r = BigUint::from(1_000_000u64) * BigUint::from(ONE_ETH);

        TestScenario {
            name: "SYMMETRIC_SPLIT",
            description: "Two identical A→B components. 50/50 split is optimal.",
            components: vec![
                ScenarioComponent {
                    id: COMPONENT_ADDR_1,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_2,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(100_000u64) * BigUint::from(ONE_ETH),
            // gross 90_909_090_909_090_909_090_909 − 1 component × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(90_909_090_909_090_904_090_909u128),
            // gross 95_238_095_238_095_236_709_344 − 2 components × 50_000 gas × 100 wei/gas
            analytical_optimum: BigInt::from(95_238_095_238_095_226_709_344u128),
        }
    }

    /// S2: two A→B components with reserves 1_000_000 and 500_000; optimal split favours the larger
    /// component.
    ///
    /// `analytical_optimum`: `optimal_two_component_output` with the asymmetric reserves — exact
    /// mathematical optimum.
    pub(crate) fn asymmetric_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let one_eth = BigUint::from(ONE_ETH);
        let r1 = BigUint::from(1_000_000u64) * &one_eth;
        let r2 = BigUint::from(500_000u64) * &one_eth;

        TestScenario {
            name: "ASYMMETRIC_SPLIT",
            description:
                "Two A→B components of unequal size. Optimal split favours the larger component.",
            components: vec![
                ScenarioComponent {
                    id: COMPONENT_ADDR_1,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r1.clone(),
                        reserve_1: r1.clone(),
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_2,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r2.clone(),
                        reserve_1: r2.clone(),
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(200_000u64) * &one_eth,
            // gross 166_666_666_666_666_666_666_666 − 1 component × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(166_666_666_666_666_661_666_666u128),
            // gross 176_470_588_235_294_097_103_232 − 2 components × 50_000 gas × 100 wei/gas
            analytical_optimum: BigInt::from(176_470_588_235_294_087_103_232u128),
        }
    }

    /// S3: split has a real gross benefit, but the extra-hop gas cost exceeds it.
    ///
    /// `analytical_optimum`: equals `lower_bound`. Gas overhead makes splitting strictly worse than
    /// single-route — the best achievable output is the BF single-route result.
    pub(crate) fn gas_kills_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let r = BigUint::from(20_000_000u64);

        TestScenario {
            name: "GAS_KILLS_SPLIT",
            description: "Split has a real gross benefit but the extra-hop gas exceeds it, making the split net-negative.",
            components: vec![
                ScenarioComponent {
                    id: COMPONENT_ADDR_1,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_2,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r.clone(),
                        reserve_1: r.clone(),
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(10_000_000u64),
            // gross 6_666_666 − 1 component × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(1_666_666i64),
            // optimal net strategy is single route; gross split output (8M) loses on net
            analytical_optimum: BigInt::from(1_666_666i64),
        }
    }

    /// S4: single A→B component only; no alternative path to split into.
    ///
    /// `analytical_optimum`: equals `lower_bound`. With only one component there is nothing to
    /// split across; the analytical optimum is simply the single-route output.
    pub(crate) fn no_alternative_path() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let r = BigUint::from(1_000_000u64) * BigUint::from(ONE_ETH);

        TestScenario {
            name: "NO_ALTERNATIVE_PATH",
            description:
                "Single A→B component. No component to split into; algorithm must return single-route result.",
            components: vec![ScenarioComponent {
                id: COMPONENT_ADDR_1,
                token_1: token_a.clone(),
                token_2: token_b.clone(),
                sim: Box::new(ConstantProductSim {
                    reserve_0: r.clone(),
                    reserve_1: r.clone(),
                    gas: 50_000,
                }),
            }],
            token_in: token_a,
            token_out: token_b,
            trade_amount: BigUint::from(100_000u64) * BigUint::from(ONE_ETH),
            // gross 90_909_090_909_090_909_090_909 − 1 component × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(90_909_090_909_090_904_090_909u128),
            // single component only — no split possible; net optimum equals lower_bound
            analytical_optimum: BigInt::from(90_909_090_909_090_904_090_909u128),
        }
    }

    /// S5: A→B (one component) → C (two parallel components); bottleneck is at the B→C hop.
    ///
    /// PathFrankWolfe discovers two complete paths [P_AB, P_BC1] and [P_AB, P_BC2]. Because both
    /// share P_AB, `build_split_route` emits one combined A→B swap followed by split B→C swaps,
    /// with P_AB's gas counted once.
    ///
    /// `lower_bound`: best single 2-hop route A→B→C through the larger B→C component.
    /// `analytical_optimum`: only one A→B component (P_AB) exists so the B amount is fixed;
    /// `optimal_two_component_output` gives the exact optimum for splitting that B across the two
    /// B→C components.
    pub(crate) fn multi_hop_bottleneck() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let one_eth = BigUint::from(ONE_ETH);
        let r_ab = BigUint::from(10_000_000u64) * &one_eth;
        let r_bc_main = BigUint::from(1_000_000u64) * &one_eth;
        let r_bc_par = BigUint::from(500_000u64) * &one_eth;

        TestScenario {
            name: "MULTI_HOP_BOTTLENECK",
            description: "A→B→C with two parallel B→C components. PathFrankWolfe discovers both paths sharing P_AB.",
            components: vec![
                ScenarioComponent {
                    id: COMPONENT_ADDR_1,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_ab.clone(),
                        reserve_1: r_ab,
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_2,
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc_main.clone(),
                        reserve_1: r_bc_main,
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_3,
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc_par.clone(),
                        reserve_1: r_bc_par,
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_c,
            trade_amount: BigUint::from(200_000u64) * &one_eth,
            // gross 163_934_426_229_508_196_721_311 − 2 components × 50_000 gas × 100 wei/gas
            lower_bound: BigInt::from(163_934_426_229_508_186_721_311u128),
            // gross 173_410_404_624_277_463_881_280 − 3 components × 50_000 gas × 100 wei/gas
            analytical_optimum: BigInt::from(173_410_404_624_277_448_881_280u128),
        }
    }

    /// S6: two A→B components then two B→C components; component sizes mismatched between hops so
    /// the optimal A→B and B→C splits differ. An algorithm that routes independent per-path
    /// hops without pooling B first will use the wrong cross-allocations and miss the optimum.
    ///
    /// `lower_bound`: best single 2-hop path (larger component at each hop).
    /// `analytical_optimum`: chained `optimal_two_component_output` across both hops.
    pub(crate) fn double_split() -> TestScenario {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let one_eth = BigUint::from(ONE_ETH);
        let r_ab1 = BigUint::from(1_000_000u64) * &one_eth;
        let r_ab2 = BigUint::from(500_000u64) * &one_eth;
        let r_bc1 = BigUint::from(500_000u64) * &one_eth;
        let r_bc2 = BigUint::from(1_500_000u64) * &one_eth;

        TestScenario {
            name: "DOUBLE_SPLIT",
            description:
                "Two A→B components (1M and 500k ETH) then two B→C components (500k and 1.5M \
                          ETH). Optimal splits differ at each hop, forcing B to be pooled before \
                          re-splitting.",
            components: vec![
                ScenarioComponent {
                    id: COMPONENT_ADDR_1,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_ab1.clone(),
                        reserve_1: r_ab1,
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_2,
                    token_1: token_a.clone(),
                    token_2: token_b.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_ab2.clone(),
                        reserve_1: r_ab2,
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_3,
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc1.clone(),
                        reserve_1: r_bc1,
                        gas: 50_000,
                    }),
                },
                ScenarioComponent {
                    id: COMPONENT_ADDR_4,
                    token_1: token_b.clone(),
                    token_2: token_c.clone(),
                    sim: Box::new(ConstantProductSim {
                        reserve_0: r_bc2.clone(),
                        reserve_1: r_bc2,
                        gas: 50_000,
                    }),
                },
            ],
            token_in: token_a,
            token_out: token_c,
            trade_amount: BigUint::from(500_000u64) * &one_eth,
            // gross floor(3×10²⁴/11) = 272_727_272_727_272_727_272_727 − 2 components × 50_000 gas
            // × 100 wei/gas
            lower_bound: BigInt::from(272_727_272_727_272_717_272_727u128),
            // gross floor(6×10²⁴/19) = 315_789_473_684_210_526_315_789 − 4 components × 50_000 gas
            // × 100 wei/gas
            analytical_optimum: BigInt::from(315_789_473_684_210_526_295_789u128),
        }
    }

    /// Returns all 6 named split_scenarios.
    pub(crate) fn all() -> Vec<TestScenario> {
        vec![
            symmetric_split(),
            asymmetric_split(),
            gas_kills_split(),
            no_alternative_path(),
            multi_hop_bottleneck(),
            double_split(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tycho_execution::encoding::models::Solution;
    use tycho_simulation::tycho_core::Bytes;

    use super::*;
    use crate::{
        algorithm::{
            bellman_ford::BellmanFordAlgorithm,
            path_frank_wolfe::{PathFrankWolfeAlgorithm, PathFrankWolfeConfig},
            AlgorithmConfig,
        },
        types::{BlockInfo, OrderQuote, QuoteStatus},
    };

    fn f64_eq(x: f64, y: f64) -> bool {
        (x - y).abs() < 1e-9
    }

    fn bf_default() -> BellmanFordAlgorithm {
        BellmanFordAlgorithm::with_config(AlgorithmConfig::default())
    }

    fn pfw_default() -> PathFrankWolfeAlgorithm {
        PathFrankWolfeAlgorithm::new(
            AlgorithmConfig::new(1, 4, Duration::from_millis(5000), None).unwrap(),
            PathFrankWolfeConfig {
                max_paths: 6,
                max_probe: 0.5,
                min_split: 0.01,
                line_search_evals: 16,
            },
        )
    }

    // ==================== evaluate_scenario tests ====================

    #[tokio::test]
    async fn evaluate_returns_path_count_1_for_single_route_algorithm() {
        let algo = bf_default();
        // BellmanFord finds a single best route, so path_count is always 1.
        for scenario in split_scenarios::all() {
            let name = scenario.name;
            let (market, gm) = scenario.build_market();
            let result = evaluate_scenario(&algo, &scenario, market, gm).await;
            assert_eq!(result.path_count, 1, "scenario '{name}': expected path_count 1");
        }
    }

    #[tokio::test]
    async fn evaluate_output_is_within_5pct_of_analytical_optimum() {
        // symmetric_split: lower_bound ~90.9k ETH, optimum ~95.2k ETH — gap ~4.55%, within 5%.
        let scenario = split_scenarios::symmetric_split();
        let (market, gm) = scenario.build_market();
        let result = evaluate_scenario(&bf_default(), &scenario, market, gm).await;
        assert!(
            result.within_pct_of_optimum(5),
            "symmetric_split: single-route is within 5% of optimum"
        );

        // asymmetric_split: lower_bound ~166.7k ETH, optimum ~176.5k ETH — gap ~5.56%, outside 5%.
        let scenario = split_scenarios::asymmetric_split();
        let (market, gm) = scenario.build_market();
        let result = evaluate_scenario(&bf_default(), &scenario, market, gm).await;
        assert!(
            !result.within_pct_of_optimum(5),
            "asymmetric_split: single-route is >5% below optimum"
        );
    }

    #[test]
    fn test_optimal_two_component_output_symmetric() {
        // Identical components → 50/50 split is always optimal
        let (fraction, _) =
            optimal_two_component_output(10_000.0, 10_000.0, 10_000.0, 10_000.0, 1_000.0);
        assert!(
            f64_eq(fraction, 0.5),
            "symmetric components: expected fraction 0.5, got {fraction}"
        );
    }

    #[test]
    fn test_optimal_two_component_output_asymmetric() {
        // Component 1: reserve_in=100, reserve_out=400
        // Component 2: reserve_in=100, reserve_out=100
        // swap amount: 400
        let (fraction, split_out) = optimal_two_component_output(100.0, 400.0, 100.0, 100.0, 400.0);

        // Verify the split is correct
        assert!(f64_eq(fraction, 0.75), "expected fraction 0.75, got {fraction}");
        assert!(f64_eq(split_out, 350.0), "expected split output 350.0, got {split_out}");

        // Verify marginal prices are equal at the optimal split.
        let component_1_amount = fraction * 400.0;
        let component_2_amount = 400.0 - component_1_amount;
        let marginal_1 = (100.0 * 400.0) / (100.0 + component_1_amount).powi(2);
        let marginal_2 = (100.0 * 100.0) / (100.0 + component_2_amount).powi(2);
        assert!(
            f64_eq(marginal_1, marginal_2),
            "marginal prices should equalise at the optimum: {marginal_1} vs {marginal_2}"
        );
    }

    #[test]
    fn test_scenario_market_builds_without_panic() {
        for scenario in split_scenarios::all() {
            assert!(!scenario.name.is_empty());
            assert!(!scenario.description.is_empty());
            assert!(scenario.analytical_optimum >= scenario.lower_bound);
            let _ = scenario.build_market();
        }
    }

    #[tokio::test]
    async fn test_bf_lower_bounds() {
        let bf = BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, 4, Duration::from_millis(100), None).unwrap(),
        );
        for scenario in split_scenarios::all() {
            let name = scenario.name;
            let (market, gm) = scenario.build_market();
            let result = evaluate_scenario(&bf, &scenario, market, gm).await;
            assert_eq!(
                result.net_output, result.lower_bound,
                "BF output doesn't match claimed lower bound for scenario '{name}'",
            );
        }
    }

    #[tokio::test]
    async fn test_shared_hop_split_lower_bound() {
        // BF on double_split finds the best single 2-hop route via the largest component at each
        // hop (component_ab_1 → component_bc_2). Confirms the lower bound is set correctly
        // before any split-routing algorithm is evaluated against it.
        let scenario = split_scenarios::double_split();
        let bf = BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, 4, Duration::from_millis(100), None).unwrap(),
        );
        let (market, gm) = scenario.build_market();
        let result = evaluate_scenario(&bf, &scenario, market, gm).await;

        assert_eq!(result.path_count, 1, "BF should return a single route");
        assert_eq!(
            result.net_output, scenario.lower_bound,
            "BF on double_split should match the precomputed lower bound",
        );
    }

    // ==================== PFW scenario tests ====================

    /// Generic checks that must hold for every scenario: lower bound, optimum proximity, route
    /// validity, and encoding correctness. When `analytical_optimum == lower_bound` no split can
    /// help, so we only require meeting the bound; otherwise the algorithm must strictly beat it.
    #[tokio::test]
    async fn pfw_all_scenarios() {
        let pfw = pfw_default();
        for scenario in split_scenarios::all() {
            let name = scenario.name;
            let (market, gm) = scenario.build_market();
            let result = evaluate_scenario(&pfw, &scenario, market, gm).await;

            result.assert_passes_lower_bound();
            assert!(result.within_pct_of_optimum(5), "'{name}': not within 5% of optimum",);
            let route = result
                .route
                .as_ref()
                .unwrap_or_else(|| panic!("'{name}': expected a route"));
            assert!(route.validate().is_ok(), "'{name}': route validation failed");
            assert_route_converts_to_solution(route, &scenario, &result.net_output).await;
        }
    }

    #[tokio::test]
    async fn pfw_gas_kills_split() {
        let scenario = split_scenarios::gas_kills_split();
        let (market, gm) = scenario.build_market();
        let result = evaluate_scenario(&pfw_default(), &scenario, market, gm).await;
        assert_eq!(result.path_count, 1, "high gas should prevent splitting");
    }

    // ==================== E2e test helpers ====================

    fn make_order_quote(
        route: crate::types::quote::Route,
        amount_in: &BigUint,
        amount_out: &BigUint,
    ) -> OrderQuote {
        let sender = Bytes::from([0xAAu8; 20].as_ref());
        let receiver = sender.clone();
        OrderQuote::new(
            "e2e-test".to_string(),
            QuoteStatus::Success,
            amount_in.clone(),
            amount_out.clone(),
            route.total_gas(),
            amount_out.clone(),
            BlockInfo::new(1, "0x00".to_string(), 0),
            "test".to_string(),
            sender,
            receiver,
            "1".to_string(),
        )
        .with_route(route)
    }

    /// Converts a route to a Solution and verifies token_in, token_out, and amount_in.
    async fn assert_route_converts_to_solution(
        route: &crate::types::quote::Route,
        scenario: &TestScenario,
        net_output: &BigInt,
    ) {
        // When net_output is negative (gas exceeds output), clamp to zero for the quote.
        let amount_out = net_output
            .to_biguint()
            .unwrap_or_default();
        let quote = make_order_quote(route.clone(), &scenario.trade_amount, &amount_out);

        let solution = Solution::try_from(&quote)
            .unwrap_or_else(|e| panic!("'{}': Solution::try_from failed: {e}", scenario.name));
        assert!(!solution.swaps().is_empty(), "solution must have swaps");
        assert_eq!(
            *solution.token_in(),
            Bytes::from(scenario.token_in.address.as_ref()),
            "'{}': solution token_in mismatch",
            scenario.name,
        );
        assert_eq!(
            *solution.token_out(),
            Bytes::from(scenario.token_out.address.as_ref()),
            "'{}': solution token_out mismatch",
            scenario.name,
        );
        assert_eq!(
            *solution.amount_in(),
            scenario.trade_amount,
            "'{}': solution amount_in mismatch",
            scenario.name,
        );
    }
}
