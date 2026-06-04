use std::collections::{HashMap, HashSet, VecDeque};

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use tycho_simulation::tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::{
    algorithm::AlgorithmError,
    feed::market_data::MarketState,
    types::{ComponentId, Order, Route, Swap},
};

pub(crate) struct HopDescriptor {
    pub(crate) component_id: ComponentId,
    pub(crate) token_in: Token,
    pub(crate) token_out: Token,
    /// Per-hop output amount, populated by the solving algorithm.
    pub(crate) amount_out: Option<BigUint>,
    /// Per-hop gas estimate, populated by the solving algorithm.
    pub(crate) gas: Option<BigUint>,
}

impl HopDescriptor {
    pub(crate) fn new(component_id: ComponentId, token_in: Token, token_out: Token) -> Self {
        Self { component_id, token_in, token_out, amount_out: None, gas: None }
    }

    pub(crate) fn with_amounts(mut self, amount_out: BigUint, gas: BigUint) -> Self {
        self.amount_out = Some(amount_out);
        self.gas = Some(gas);
        self
    }
}

/// A fully-simulated path allocation.
///
/// One path in the current split solution, with the fraction of total `amount_in`
/// currently allocated to it. All fractions across allocations sum to 1.0.
pub(crate) struct PathAllocation {
    pub(crate) hops: Vec<HopDescriptor>,
    /// Fraction of total input on this path (0 < f <= 1).
    pub(crate) flow_fraction: f64,
    pub(crate) amount_in: BigUint,
    pub(crate) amount_out: BigUint,
    /// Product of marginal prices along all hops at the time this allocation was
    /// last simulated.
    pub(crate) marginal_price_product: f64,
}

impl PathAllocation {
    /// Validates that this path does not revisit any token.
    ///
    /// A token appearing more than once means `merge_shared_hops` would
    /// incorrectly collapse distinct hops into one. The only exception is
    /// a round-trip where the final output equals the first input.
    pub(crate) fn validate(&self) -> Result<(), AlgorithmError> {
        if self.hops.is_empty() {
            return Ok(());
        }
        let first_token = &self.hops[0].token_in.address;
        let mut seen = HashSet::new();
        seen.insert(first_token.clone());
        let last_idx = self.hops.len() - 1;
        for (i, hop) in self.hops.iter().enumerate() {
            if !seen.insert(hop.token_out.address.clone()) {
                let is_valid_round_trip = i == last_idx && &hop.token_out.address == first_token;
                if !is_valid_round_trip {
                    return Err(AlgorithmError::Other(format!(
                        "path revisits token {} at hop {i} \
                         (would corrupt merge_shared_hops)",
                        hop.token_out.address,
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Output of simulating one path at a given input amount.
pub(crate) struct SimResult {
    pub(crate) amount_out: BigUint,
    /// Raw per-hop sum; use only via `evaluate_total_output`.
    pub(crate) gas: u64,
    pub(crate) marginal_price_product: f64,
}

/// Pool state overrides for passing degraded states to `find_single_route`.
#[derive(Default)]
pub(crate) struct MarketOverrides(HashMap<ComponentId, Box<dyn ProtocolSim>>);

impl MarketOverrides {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Insert a degraded pool state as an override.
    pub(crate) fn with_override(mut self, id: ComponentId, sim: Box<dyn ProtocolSim>) -> Self {
        self.0.insert(id, sim);
        self
    }

    /// Insert a zero-gas wrapper around an existing sim. The underlying pool still
    /// produces correct amounts; only `get_amount_out().gas` is zeroed. Use for pools
    /// already present in `current_allocations` — their gas is paid once in the
    /// combined transaction.
    pub(crate) fn with_zero_gas(mut self, id: ComponentId, sim: Box<dyn ProtocolSim>) -> Self {
        self.0
            .insert(id, Box::new(ZeroGasSim(sim)));
        self
    }

    pub(crate) fn get(&self, id: &ComponentId) -> Option<&dyn ProtocolSim> {
        self.0.get(id).map(|b| b.as_ref())
    }
}

/// Wraps a sim so that `get_amount_out().gas` is always zero while amounts
/// remain correct. Use for pools whose gas is already accounted for elsewhere.
pub(crate) fn wrap_zero_gas(sim: Box<dyn ProtocolSim>) -> Box<dyn ProtocolSim> {
    Box::new(ZeroGasSim(sim))
}

/// Wrapper that delegates all [`ProtocolSim`] calls unchanged except
/// [`get_amount_out`](ProtocolSim::get_amount_out), where it zeroes the returned gas.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ZeroGasSim(Box<dyn ProtocolSim>);

#[typetag::serde]
impl ProtocolSim for ZeroGasSim {
    fn fee(&self) -> f64 {
        self.0.fee()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.0.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let mut result = self
            .0
            .get_amount_out(amount_in, token_in, token_out)?;
        result.gas = BigUint::ZERO;
        result.new_state = Box::new(ZeroGasSim(result.new_state));
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.0.get_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.0
            .delta_transition(delta, tokens, balances)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(ZeroGasSim(self.0.clone_box()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .map(|o| self.0.eq(&*o.0))
            .unwrap_or(false)
    }
}

/// Find the `x` in `[lo, hi]` that maximises `f(x)` using golden-section search.
///
/// Assumes `f` is roughly unimodal (has one maximum). `max_evals` controls the
/// number of function evaluations (higher = more precise but slower).
pub(crate) fn golden_section_search(
    f: impl Fn(f64) -> f64,
    mut lo: f64,
    mut hi: f64,
    max_evals: usize,
) -> f64 {
    let inv_phi = (5_f64.sqrt() - 1.0) / 2.0;

    let mut x1 = hi - inv_phi * (hi - lo);
    let mut x2 = lo + inv_phi * (hi - lo);
    let mut f1 = f(x1);
    let mut f2 = f(x2);
    // Two evaluations consumed so far.
    let remaining = max_evals.saturating_sub(2);

    for _ in 0..remaining {
        if f1 < f2 {
            lo = x1;
            x1 = x2;
            f1 = f2;
            x2 = lo + inv_phi * (hi - lo);
            f2 = f(x2);
        } else {
            hi = x2;
            x2 = x1;
            f2 = f1;
            x1 = hi - inv_phi * (hi - lo);
            f1 = f(x1);
        }
    }

    if f1 >= f2 {
        x1
    } else {
        x2
    }
}

/// Split `total` into `(part, remainder)` where `part ≈ total * fraction`.
///
/// Both values always sum exactly to `total` — no tokens lost to rounding.
/// `fraction` is clamped to `[0.0, 1.0]` before use.
pub(crate) fn split_amount(total: &BigUint, fraction: f64) -> (BigUint, BigUint) {
    let clamped = fraction.clamp(0.0, 1.0);
    // Scale fraction to fixed-point with 18 decimal digits of precision.
    let scale: u64 = 1_000_000_000_000_000_000;
    let numerator = (clamped * scale as f64) as u64;
    let part = (total * BigUint::from(numerator)) / BigUint::from(scale);
    let remainder = total - &part;
    (part, remainder)
}

/// Errors from split-routing math utilities.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum SplitMathError {
    #[error("fractions slice must not be empty")]
    EmptyFractions,
    #[error("all fractions are zero, cannot normalize")]
    AllZeroFractions,
    #[error("fractions must not be negative")]
    NegativeFraction,
}

/// Normalize a slice of fractions so they sum to 1.0.
///
/// # Errors
///
/// Returns [`SplitMathError::EmptyFractions`] if the slice is empty, or
/// [`SplitMathError::AllZeroFractions`] if every element is zero.
pub(crate) fn normalize_fractions(fractions: &mut [f64]) -> Result<(), SplitMathError> {
    if fractions.is_empty() {
        return Err(SplitMathError::EmptyFractions);
    }
    if fractions.iter().any(|&f| f < 0.0) {
        return Err(SplitMathError::NegativeFraction);
    }
    let sum: f64 = fractions.iter().sum();
    if sum == 0.0 {
        return Err(SplitMathError::AllZeroFractions);
    }
    for f in fractions.iter_mut() {
        *f /= sum;
    }
    Ok(())
}

/// Convert fractions (summing to 1.0) into `BigUint` amounts summing exactly
/// to `total`.
///
/// The last element absorbs any rounding remainder so the sum is exact.
///
/// # Errors
///
/// Returns [`SplitMathError::EmptyFractions`] if `fractions` is empty.
pub(crate) fn fractions_to_amounts(
    total: &BigUint,
    fractions: &[f64],
) -> Result<Vec<BigUint>, SplitMathError> {
    if fractions.is_empty() {
        return Err(SplitMathError::EmptyFractions);
    }
    let n = fractions.len();
    let mut amounts = Vec::with_capacity(n);
    let mut running_sum = BigUint::zero();

    for &frac in &fractions[..n - 1] {
        let (part, _) = split_amount(total, frac);
        running_sum += &part;
        amounts.push(part);
    }

    // Last element gets the remainder to guarantee exact sum.
    amounts.push(total - &running_sum);
    Ok(amounts)
}

/// Product of spot prices along a path — approximates the exchange rate at
/// near-zero input.
pub(crate) fn compute_marginal_price_product(
    hops: &[HopDescriptor],
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<f64, AlgorithmError> {
    let mut product = 1.0;
    for hop in hops {
        let sim = overrides
            .get(&hop.component_id)
            .or_else(|| market.get_simulation_state(&hop.component_id))
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "simulation state",
                id: Some(hop.component_id.clone()),
            })?;
        let price = sim
            .spot_price(&hop.token_in, &hop.token_out)
            .map_err(|e| AlgorithmError::SimulationFailed {
                component_id: hop.component_id.clone(),
                error: e.to_string(),
            })?;
        product *= price;
    }
    Ok(product)
}

/// Simulates a path hop-by-hop, threading output of each hop as input to the
/// next.
///
/// Checks `overrides` before falling back to the live market state for each
/// hop. Returns the final output amount, raw gas sum, and marginal price
/// product.
pub(crate) fn simulate_path(
    hops: &[HopDescriptor],
    amount_in: &BigUint,
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<SimResult, AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut total_gas: u64 = 0;

    for hop in hops {
        let sim = overrides
            .get(&hop.component_id)
            .or_else(|| market.get_simulation_state(&hop.component_id))
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "simulation state",
                id: Some(hop.component_id.clone()),
            })?;

        let result = sim
            .get_amount_out(current_amount, &hop.token_in, &hop.token_out)
            .map_err(|e| AlgorithmError::SimulationFailed {
                component_id: hop.component_id.clone(),
                error: e.to_string(),
            })?;

        // Cap at u64::MAX instead of panicking on overflow.
        total_gas = total_gas.saturating_add(result.gas.to_u64().unwrap_or(u64::MAX));
        current_amount = result.amount;
    }

    let marginal_price_product = compute_marginal_price_product(hops, market, overrides)?;

    Ok(SimResult { amount_out: current_amount, gas: total_gas, marginal_price_product })
}

/// Simulates all paths at their current fractions and returns
/// `(total_amount_out, total_gas)`. `paths[i]` corresponds to `fractions[i]`.
pub(crate) fn evaluate_total_output(
    paths: &[&[HopDescriptor]],
    fractions: &[f64],
    total_amount: &BigUint,
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<(BigUint, u64), AlgorithmError> {
    let amounts = fractions_to_amounts(total_amount, fractions)
        .map_err(|e| AlgorithmError::Other(e.to_string()))?;

    let mut total_out = BigUint::zero();
    let mut total_gas: u64 = 0;
    let mut seen_hops: HashSet<(ComponentId, Bytes, Bytes)> = HashSet::new();

    for (path, amount) in paths.iter().zip(amounts.iter()) {
        if amount.is_zero() {
            continue;
        }

        let mut current_amount = amount.clone();

        for hop in path.iter() {
            let sim = overrides
                .get(&hop.component_id)
                .or_else(|| market.get_simulation_state(&hop.component_id))
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: Some(hop.component_id.clone()),
                })?;

            let result = sim
                .get_amount_out(current_amount, &hop.token_in, &hop.token_out)
                .map_err(|e| AlgorithmError::SimulationFailed {
                    component_id: hop.component_id.clone(),
                    error: e.to_string(),
                })?;

            // Shared pre-split hops appear in multiple paths but are
            // executed once on-chain — count gas only once per unique
            // (pool, token_in, token_out).
            let hop_key = (
                hop.component_id.clone(),
                hop.token_in.address.clone(),
                hop.token_out.address.clone(),
            );
            if seen_hops.insert(hop_key) {
                total_gas = total_gas.saturating_add(result.gas.to_u64().unwrap_or(u64::MAX));
            }
            current_amount = result.amount;
        }

        total_out += current_amount;
    }

    Ok((total_out, total_gas))
}

/// Builds post-swap pool states after all paths in a split-route solution
/// have been executed.
///
/// For example, if the current solution splits 1000 USDC→ETH across:
///   - Path 1: USDC→WETH via Uniswap (600 USDC)
///   - Path 2: USDC→WBTC→WETH via Curve+Balancer (400 USDC)
///
/// this function simulates both swaps and returns overrides where Uniswap,
/// Curve, and Balancer all reflect their post-swap reserves. Pass the result
/// to `find_single_route` for the next iteration.
///
/// Paths are processed in order so shared pools accumulate the effects of
/// all prior paths.
pub(crate) fn build_post_swap_overrides(
    paths: &[&PathAllocation],
    market: &MarketState,
) -> MarketOverrides {
    let mut overrides = MarketOverrides::empty();

    for path in paths {
        let mut current_amount = path.amount_in.clone();

        for hop in &path.hops {
            let sim = overrides
                .get(&hop.component_id)
                .or_else(|| market.get_simulation_state(&hop.component_id));

            let Some(sim) = sim else { break };

            let Ok(result) = sim.get_amount_out(current_amount, &hop.token_in, &hop.token_out)
            else {
                break;
            };

            current_amount = result.amount;
            overrides = overrides.with_override(hop.component_id.clone(), result.new_state);
        }
    }

    overrides
}

struct SplitSwap {
    hop: HopDescriptor,
    split: f64,
    amount_in: BigUint,
    amount_out: BigUint,
    gas: BigUint,
}

/// Merge shared hops across paths, summing their flow fractions, and return
/// them collected by `token_in` (sorted by fraction descending within each
/// branch collection).
fn merge_shared_hops(
    paths: &[PathAllocation],
) -> Result<HashMap<Bytes, Vec<SplitSwap>>, AlgorithmError> {
    type HopKey = (ComponentId, Bytes, Bytes);
    let mut hops: HashMap<HopKey, SplitSwap> = HashMap::new();

    for path in paths {
        for hop in &path.hops {
            let key: HopKey = (
                hop.component_id.clone(),
                hop.token_in.address.clone(),
                hop.token_out.address.clone(),
            );
            let hop_amount_out =
                hop.amount_out
                    .clone()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "hop amount_out",
                        id: Some(hop.component_id.clone()),
                    })?;
            let hop_gas = hop
                .gas
                .clone()
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "hop gas",
                    id: Some(hop.component_id.clone()),
                })?;
            hops.entry(key)
                .and_modify(|h| {
                    h.split += path.flow_fraction;
                    h.amount_out += &hop_amount_out;
                    // Gas is not summed: swapping more on the same pool does not
                    // increase gas compared to swapping less.
                })
                .or_insert(SplitSwap {
                    hop: HopDescriptor::new(
                        hop.component_id.clone(),
                        hop.token_in.clone(),
                        hop.token_out.clone(),
                    ),
                    split: path.flow_fraction,
                    // Set later by assign_splits_and_amounts.
                    amount_in: BigUint::ZERO,
                    amount_out: hop_amount_out,
                    gas: hop_gas,
                });
        }
    }

    let mut branch_collections: HashMap<Bytes, Vec<SplitSwap>> = HashMap::new();
    for (_, swap) in hops {
        branch_collections
            .entry(swap.hop.token_in.address.clone())
            .or_default()
            .push(swap);
    }
    for branch_collection in branch_collections.values_mut() {
        branch_collection.sort_by(|a, b| {
            b.split
                .partial_cmp(&a.split)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    Ok(branch_collections)
}

/// Normalize fractions within a branch collection, convert them to input amounts, and
/// assign final split values using the tycho-execution remainder convention
/// (last hop gets `split = 0.0`).
fn assign_splits_and_amounts(
    mut hops: Vec<SplitSwap>,
    total_available: &BigUint,
) -> Vec<SplitSwap> {
    let len = hops.len();
    let fraction_total: f64 = hops.iter().map(|h| h.split).sum();

    let normalized: Vec<f64> = hops
        .iter()
        .map(|h| h.split / fraction_total)
        .collect();
    let amounts = fractions_to_amounts(total_available, &normalized)
        .unwrap_or_else(|_| vec![total_available.clone()]);

    for (i, (swap, amount)) in hops.iter_mut().zip(amounts).enumerate() {
        swap.amount_in = amount;
        swap.split = if i == len - 1 { 0.0 } else { normalized[i] };
    }
    hops
}

/// Assembles a [`Route`] from split-route path allocations with shared-hop
/// deduplication.
///
/// Paths may share pool hops (same `component_id`, `token_in`, `token_out`).
/// When they do, this function emits one combined swap rather than duplicates.
/// Within each branch collection of swaps sharing a `token_in`, the tycho-execution
/// remainder convention is applied: sorted by fraction descending, all but the
/// last receive their explicit split fraction, while the last gets
/// `split = 0.0` (meaning "use all remaining balance").
pub(crate) fn build_split_route(
    paths: &[PathAllocation],
    market: &MarketState,
    order: &Order,
) -> Result<Route, AlgorithmError> {
    for path in paths {
        path.validate()?;
    }
    let mut hops_by_token = merge_shared_hops(paths)?;

    let mut pending_tokens = VecDeque::new();
    pending_tokens.push_back(order.token_in().clone());

    let mut available: HashMap<Bytes, BigUint> = HashMap::new();
    available.insert(order.token_in().clone(), order.amount().clone());

    let mut swaps = Vec::new();
    let mut route_tokens: HashMap<Bytes, Token> = HashMap::new();
    let mut visited: HashSet<Bytes> = HashSet::new();

    // BFS from the source token: at each token, assemble Swap objects from
    // pre-computed hop amounts (populated by the solving algorithm).
    while let Some(token_addr) = pending_tokens.pop_front() {
        // Converging paths (e.g. B→D and C→D) add D to the queue twice — skip duplicates.
        if !visited.insert(token_addr.clone()) {
            continue;
        }
        // Terminal tokens (e.g. the final output) have no outgoing swaps.
        let Some(branch_collection) = hops_by_token.remove(&token_addr) else {
            continue;
        };
        let total = available
            .get(&token_addr)
            .cloned()
            .unwrap_or_default();

        for split_swap in assign_splits_and_amounts(branch_collection, &total) {
            let sim = market
                .get_simulation_state(&split_swap.hop.component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: Some(split_swap.hop.component_id.clone()),
                })?;

            let component = market
                .get_component(&split_swap.hop.component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "protocol component",
                    id: Some(split_swap.hop.component_id.clone()),
                })?;

            let in_addr = split_swap.hop.token_in.address.clone();
            let out_addr = split_swap.hop.token_out.address.clone();
            *available
                .entry(out_addr.clone())
                .or_default() += &split_swap.amount_out;
            swaps.push(
                Swap::new(
                    split_swap.hop.component_id,
                    component.protocol_system.clone(),
                    in_addr.clone(),
                    out_addr.clone(),
                    split_swap.amount_in,
                    split_swap.amount_out,
                    split_swap.gas,
                    component.clone(),
                    sim.clone_box(),
                )
                .with_split(split_swap.split),
            );
            route_tokens
                .entry(in_addr)
                .or_insert(split_swap.hop.token_in);
            route_tokens
                .entry(out_addr.clone())
                .or_insert(split_swap.hop.token_out);
            if !visited.contains(&out_addr) {
                pending_tokens.push_back(out_addr);
            }
        }
    }

    Ok(Route::new(swaps, route_tokens))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, order, token, ConstantProductSim, MockProtocolSim},
        types::OrderSide,
    };

    fn make_market(pools: Vec<(&str, Vec<Token>, Box<dyn ProtocolSim>)>) -> MarketState {
        let mut market = MarketState::new();
        for (pool_id, tokens, sim) in pools {
            market.upsert_components(std::iter::once(component(pool_id, &tokens)));
            market.update_states([(pool_id.to_string(), sim)]);
            market.upsert_tokens(tokens);
        }
        market
    }

    #[test]
    fn test_split_amount_exact_sum() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        for fraction in [0.1, 0.5, 0.9, 0.999] {
            let (part, remainder) = split_amount(&total, fraction);
            assert_eq!(
                &part + &remainder,
                total,
                "part + remainder must equal total for fraction={fraction}"
            );
        }
    }

    #[test]
    fn test_split_amount_edge_fraction_zero() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        let (part, remainder) = split_amount(&total, 0.0);
        assert!(part.is_zero());
        assert_eq!(remainder, total);
    }

    #[test]
    fn test_split_amount_clamps_above_one() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        let (part, remainder) = split_amount(&total, 1.5);
        assert_eq!(part, total);
        assert!(remainder.is_zero());
    }

    #[test]
    fn test_split_amount_clamps_negative() {
        let total = BigUint::from(1_000_000_000_000_000_000_u64);
        let (part, remainder) = split_amount(&total, -0.5);
        assert!(part.is_zero());
        assert_eq!(remainder, total);
    }

    #[test]
    fn test_fractions_to_amounts_exact_sum() {
        let total = BigUint::from(999_999_999_999_999_999_u64);
        let fractions = [0.3, 0.5, 0.2];
        let amounts = fractions_to_amounts(&total, &fractions).unwrap();
        assert_eq!(amounts.len(), 3);
        let sum: BigUint = amounts.iter().sum();
        assert_eq!(sum, total, "amounts must sum exactly to total");
    }

    #[test]
    fn test_fractions_to_amounts_empty() {
        let total = BigUint::from(1_000_u64);
        let err = fractions_to_amounts(&total, &[]).unwrap_err();
        assert_eq!(err, SplitMathError::EmptyFractions);
    }

    #[rstest]
    #[case::already_normalized(&[0.3, 0.5, 0.2])]
    #[case::drift(&[0.33, 0.33, 0.33])]
    fn test_normalize_fractions(#[case] input: &[f64]) {
        let mut fractions = input.to_vec();
        normalize_fractions(&mut fractions).unwrap();
        let sum: f64 = fractions.iter().sum();
        assert!((sum - 1.0).abs() < f64::EPSILON);
    }

    #[rstest]
    #[case::empty(&[], SplitMathError::EmptyFractions)]
    #[case::all_zeros(&[0.0, 0.0, 0.0], SplitMathError::AllZeroFractions)]
    #[case::negative(&[-0.5, 0.5], SplitMathError::NegativeFraction)]
    fn test_normalize_fractions_invalid(#[case] input: &[f64], #[case] expected: SplitMathError) {
        let mut fractions = input.to_vec();
        let err = normalize_fractions(&mut fractions).unwrap_err();
        assert_eq!(err, expected);
    }

    #[test]
    fn test_golden_section_finds_maximum() {
        // Maximize -(x - 0.3)^2; true maximum at x = 0.3.
        let f = |x: f64| -(x - 0.3) * (x - 0.3);
        let result = golden_section_search(f, 0.0, 1.0, 100);
        assert!((result - 0.3).abs() < 1e-4, "expected ~0.3, got {result}");
    }

    // ==================== PathAllocation::validate Tests ====================

    #[test]
    fn test_path_allocation_validate_valid_path() {
        let gas = BigUint::from(50_000u64);
        let path = PathAllocation {
            hops: vec![
                HopDescriptor::new("p1".to_string(), token(0x01, "A"), token(0x02, "B"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p2".to_string(), token(0x02, "B"), token(0x03, "C"))
                    .with_amounts(BigUint::from(100u64), gas),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(100u64),
            marginal_price_product: 1.0,
        };
        assert!(path.validate().is_ok());
    }

    #[test]
    fn test_path_allocation_validate_empty_hops() {
        let path = PathAllocation {
            hops: vec![],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(100u64),
            marginal_price_product: 1.0,
        };
        assert!(path.validate().is_ok());
    }

    #[test]
    fn test_path_allocation_validate_valid_round_trip() {
        // A → B → A is a valid round-trip (first == last).
        let gas = BigUint::from(50_000u64);
        let path = PathAllocation {
            hops: vec![
                HopDescriptor::new("p1".to_string(), token(0x01, "A"), token(0x02, "B"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p2".to_string(), token(0x02, "B"), token(0x01, "A"))
                    .with_amounts(BigUint::from(100u64), gas),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(100u64),
            marginal_price_product: 1.0,
        };
        assert!(path.validate().is_ok());
    }

    #[test]
    fn test_path_allocation_validate_rejects_mid_path_cycle() {
        // A → B → C → A → D: token A revisited mid-path (not a round-trip).
        // merge_shared_hops would incorrectly merge both A→? hops.
        let gas = BigUint::from(50_000u64);
        let path = PathAllocation {
            hops: vec![
                HopDescriptor::new("p1".to_string(), token(0x01, "A"), token(0x02, "B"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p2".to_string(), token(0x02, "B"), token(0x03, "C"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p3".to_string(), token(0x03, "C"), token(0x01, "A"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p4".to_string(), token(0x01, "A"), token(0x04, "D"))
                    .with_amounts(BigUint::from(100u64), gas),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(100u64),
            marginal_price_product: 1.0,
        };
        assert!(path.validate().is_err());
    }

    #[test]
    fn test_path_allocation_validate_rejects_intermediate_revisit() {
        // A → B → C → B → D: token B revisited.
        let gas = BigUint::from(50_000u64);
        let path = PathAllocation {
            hops: vec![
                HopDescriptor::new("p1".to_string(), token(0x01, "A"), token(0x02, "B"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p2".to_string(), token(0x02, "B"), token(0x03, "C"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p3".to_string(), token(0x03, "C"), token(0x02, "B"))
                    .with_amounts(BigUint::from(100u64), gas.clone()),
                HopDescriptor::new("p4".to_string(), token(0x02, "B"), token(0x04, "D"))
                    .with_amounts(BigUint::from(100u64), gas),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(100u64),
            marginal_price_product: 1.0,
        };
        assert!(path.validate().is_err());
    }

    // ==================== Simulation Utility Tests ====================

    #[test]
    fn test_compute_marginal_price_product_single_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(3.0)),
        )]);

        let hops = [HopDescriptor::new("pool_ab".to_string(), token_a, token_b)];

        let product =
            compute_marginal_price_product(&hops, &market, &MarketOverrides::empty()).unwrap();
        assert!((product - 3.0).abs() < f64::EPSILON, "expected 3.0, got {product}");
    }

    #[test]
    fn test_compute_marginal_price_product_multi_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "pool_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(4.0)),
            ),
        ]);

        let hops = [
            HopDescriptor::new("pool_ab".to_string(), token_a, token_b.clone()),
            HopDescriptor::new("pool_bc".to_string(), token_b, token_c),
        ];

        let product =
            compute_marginal_price_product(&hops, &market, &MarketOverrides::empty()).unwrap();
        // 2.0 * 4.0 = 8.0
        assert!((product - 8.0).abs() < f64::EPSILON, "expected 8.0, got {product}");
    }

    #[test]
    fn test_compute_marginal_price_product_uses_overrides() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(3.0)),
        )]);

        let hops = [HopDescriptor::new("pool_ab".to_string(), token_a, token_b)];

        // Override pool_ab with a different spot price.
        let overrides = MarketOverrides::empty()
            .with_override("pool_ab".to_string(), Box::new(MockProtocolSim::new(7.0)));

        let product = compute_marginal_price_product(&hops, &market, &overrides).unwrap();
        assert!((product - 7.0).abs() < f64::EPSILON, "expected 7.0, got {product}");
    }

    #[test]
    fn test_simulate_path_correct_output() {
        // 2-hop path A→B→C with spot prices 2.0 and 3.0.
        // Input 1000 should thread through: 1000*2=2000, 2000*3=6000.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "pool_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
        ]);

        let hops = [
            HopDescriptor::new("pool_ab".to_string(), token_a, token_b.clone()),
            HopDescriptor::new("pool_bc".to_string(), token_b, token_c),
        ];

        let amount_in = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();
        let result = simulate_path(&hops, &amount_in, &market, &overrides).unwrap();

        assert_eq!(result.amount_out, BigUint::from(6000u64));

        // spot_price(A→B) = 2.0, spot_price(B→C) = 3.0 → product = 6.0
        assert!(
            (result.marginal_price_product - 6.0).abs() < f64::EPSILON,
            "expected marginal_price_product 6.0, got {}",
            result.marginal_price_product
        );
    }

    #[test]
    fn test_market_overrides_with_zero_gas() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let sim_ab = MockProtocolSim::new(2.0).with_gas(100_000);
        let sim_bc = MockProtocolSim::new(3.0).with_gas(70_000);
        let market = make_market(vec![
            ("pool_ab", vec![token_a.clone(), token_b.clone()], Box::new(sim_ab.clone())),
            ("pool_bc", vec![token_b.clone(), token_c.clone()], Box::new(sim_bc.clone())),
        ]);

        // Zero gas on pool_ab, leave pool_bc as a normal override.
        let overrides = MarketOverrides::empty()
            .with_zero_gas("pool_ab".to_string(), Box::new(sim_ab))
            .with_override("pool_bc".to_string(), Box::new(sim_bc));

        let hops_ab = [HopDescriptor::new("pool_ab".to_string(), token_a.clone(), token_b.clone())];
        let hops_bc = [HopDescriptor::new("pool_bc".to_string(), token_b, token_c)];
        let amount_in = BigUint::from(1000u64);

        let normal_ab =
            simulate_path(&hops_ab, &amount_in, &market, &MarketOverrides::empty()).unwrap();
        let zero_gas_ab = simulate_path(&hops_ab, &amount_in, &market, &overrides).unwrap();

        assert_eq!(normal_ab.amount_out, zero_gas_ab.amount_out);
        assert!(normal_ab.gas > 0, "normal gas should be non-zero");
        assert_eq!(zero_gas_ab.gas, 0, "zero-gas override should report gas=0");

        // pool_bc is a normal override — its gas should be unaffected.
        let result_bc = simulate_path(&hops_bc, &amount_in, &market, &overrides).unwrap();
        assert_eq!(result_bc.gas, 70_000, "non-zero-gas override should keep its gas");
    }

    #[test]
    fn test_evaluate_total_output_two_paths() {
        // 50/50 split of 1000 across two parallel 1-hop paths:
        //
        //       500 -- pool_1 (price=2.0) --> 1000
        //      /                                   \
        //  1000                                     2500
        //      \                                   /
        //       500 -- pool_2 (price=3.0) --> 1500
        //
        // total_gas = 50k + 60k = 110k
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            (
                "pool_1",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(50_000)),
            ),
            (
                "pool_2",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(3.0).with_gas(60_000)),
            ),
        ]);

        let hops_1 = [HopDescriptor::new("pool_1".to_string(), token_a.clone(), token_b.clone())];
        let hops_2 = [HopDescriptor::new("pool_2".to_string(), token_a, token_b)];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (total_out, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        assert_eq!(total_out, BigUint::from(2500u64));
        assert_eq!(total_gas, 110_000);
    }

    #[test]
    fn test_evaluate_total_output_gas_deduplication() {
        // Two paths share pool P1 (pre-split hop). P1's gas should be
        // counted once, not twice.
        //
        //              P2 (50k gas) --> C
        //             /
        //  A -- P1 --+
        //             \
        //              P3 (70k gas) --> D
        //
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let token_d = token(0x0D, "D");
        let market = make_market(vec![
            (
                "P1",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(100_000)),
            ),
            (
                "P2",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(1.5).with_gas(50_000)),
            ),
            (
                "P3",
                vec![token_b.clone(), token_d.clone()],
                Box::new(MockProtocolSim::new(3.0).with_gas(70_000)),
            ),
        ]);

        // Path 1: A -> P1 -> B -> P2 -> C (uses P1 and P2)
        let hops_1 = [
            HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone()),
            HopDescriptor::new("P2".to_string(), token_b.clone(), token_c),
        ];
        // Path 2: A -> P1 -> B -> P3 -> D (uses P1 and P3)
        let hops_2 = [
            HopDescriptor::new("P1".to_string(), token_a, token_b.clone()),
            HopDescriptor::new("P3".to_string(), token_b, token_d),
        ];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (_, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        // P1 counted once: 100k + 50k + 70k = 220k
        assert_eq!(total_gas, 220_000);
    }

    #[test]
    fn test_gas_dedup_different_tokens() {
        // A single 3-token pool used for two different token pairs is two
        // distinct hops — gas must be counted for each.
        //
        //  A -- TRIPOOL (A→B) --> B    (path 1)
        //  B -- TRIPOOL (B→C) --> C    (path 2)
        //
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![(
            "tripool",
            vec![token_a.clone(), token_b.clone(), token_c.clone()],
            Box::new(MockProtocolSim::new(1.0).with_gas(80_000)),
        )]);

        let hops_1 = [HopDescriptor::new("tripool".to_string(), token_a, token_b.clone())];
        let hops_2 = [HopDescriptor::new("tripool".to_string(), token_b, token_c)];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (_, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        // Different token pairs on the same pool: 80k + 80k = 160k
        assert_eq!(total_gas, 160_000);
    }

    #[test]
    fn test_build_post_swap_overrides_degrades_used_pools() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "pool_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(10_000u64),
                reserve_1: BigUint::from(20_000u64),
                gas: 50_000,
            }),
        )]);

        let allocation = PathAllocation {
            hops: vec![HopDescriptor::new("pool_ab".to_string(), token_a.clone(), token_b.clone())],
            flow_fraction: 1.0,
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(1818u64),
            marginal_price_product: 2.0,
        };

        let degraded = build_post_swap_overrides(&[&allocation], &market);

        // xy=k: amount_out = amount_in * reserve_out / (reserve_in + amount_in)
        // Fresh pool (10000/20000): 100 * 20000 / (10000 + 100) = 198
        let probe = BigUint::from(100u64);
        let fresh_out = market
            .get_simulation_state("pool_ab")
            .unwrap()
            .get_amount_out(probe.clone(), &token_a, &token_b)
            .unwrap()
            .amount;
        assert_eq!(fresh_out, BigUint::from(198u64));

        // The 1000-in allocation produces 1000*20000/(10000+1000) = 1818 out,
        // shifting reserves to (10000+1000, 20000-1818) = (11000, 18182).
        // Degraded pool: 100 * 18182 / (11000 + 100) = 163
        let degraded_out = degraded
            .get(&"pool_ab".to_string())
            .unwrap()
            .get_amount_out(probe, &token_a, &token_b)
            .unwrap()
            .amount;
        assert_eq!(degraded_out, BigUint::from(163u64));
    }

    // ==================== merge / allocate Tests ====================

    #[test]
    fn test_merge_shared_hops_combines_fractions() {
        // Two paths share the first hop A→B via P1; second hops diverge.
        //
        //                P2
        //               /    \
        //  A -- P1 --> B      C
        //               \    /
        //                P3
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        let gas = BigUint::from(50_000u64);
        let paths = vec![
            PathAllocation {
                hops: vec![
                    HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                        .with_amounts(BigUint::from(1200u64), gas.clone()),
                    HopDescriptor::new("P2".to_string(), token_b.clone(), token_c.clone())
                        .with_amounts(BigUint::from(3600u64), gas.clone()),
                ],
                flow_fraction: 0.6,
                amount_in: BigUint::from(600u64),
                amount_out: BigUint::from(3600u64),
                marginal_price_product: 6.0,
            },
            PathAllocation {
                hops: vec![
                    HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                        .with_amounts(BigUint::from(800u64), gas.clone()),
                    HopDescriptor::new("P3".to_string(), token_b.clone(), token_c.clone())
                        .with_amounts(BigUint::from(1600u64), gas),
                ],
                flow_fraction: 0.4,
                amount_in: BigUint::from(400u64),
                amount_out: BigUint::from(1600u64),
                marginal_price_product: 4.0,
            },
        ];

        let hops_by_token = merge_shared_hops(&paths).unwrap();

        // Branch collection at A: one merged hop (P1, fraction = 0.6 + 0.4 = 1.0).
        let branch_collection_a = &hops_by_token[&token_a.address];
        assert_eq!(branch_collection_a.len(), 1);
        assert_eq!(branch_collection_a[0].hop.component_id, "P1");
        assert!((branch_collection_a[0].split - 1.0).abs() < f64::EPSILON);

        // Branch collection at B: two hops (P2 and P3), sorted descending by fraction.
        let branch_collection_b = &hops_by_token[&token_b.address];
        assert_eq!(branch_collection_b.len(), 2);
        assert_eq!(branch_collection_b[0].hop.component_id, "P2");
        assert!((branch_collection_b[0].split - 0.6).abs() < f64::EPSILON);
        assert_eq!(branch_collection_b[1].hop.component_id, "P3");
        assert!((branch_collection_b[1].split - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn test_assign_splits_and_amounts_splits_and_amounts() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let branch_collection = vec![
            SplitSwap {
                hop: HopDescriptor::new("pool1".to_string(), token_a.clone(), token_b.clone()),
                split: 0.7,
                amount_in: BigUint::ZERO,
                amount_out: BigUint::ZERO,
                gas: BigUint::ZERO,
            },
            SplitSwap {
                hop: HopDescriptor::new("pool2".to_string(), token_a.clone(), token_b.clone()),
                split: 0.3,
                amount_in: BigUint::ZERO,
                amount_out: BigUint::ZERO,
                gas: BigUint::ZERO,
            },
        ];

        let result = assign_splits_and_amounts(branch_collection, &BigUint::from(1000u64));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].split, 0.7);
        assert_eq!(result[0].amount_in, BigUint::from(700u64));
        assert_eq!(result[1].split, 0.0);
        assert_eq!(result[1].amount_in, BigUint::from(300u64));
    }

    #[test]
    fn test_assign_splits_and_amounts_single_hop() {
        // A single hop receives the entire amount with split = 0.0.
        //
        //  1000 -- pool1 (split=0.0) --> B
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let branch_collection = vec![SplitSwap {
            hop: HopDescriptor::new("pool1".to_string(), token_a, token_b),
            split: 1.0,
            amount_in: BigUint::ZERO,
            amount_out: BigUint::ZERO,
            gas: BigUint::ZERO,
        }];

        let total = BigUint::from(1000u64);
        let result = assign_splits_and_amounts(branch_collection, &total);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].split, 0.0);
        assert_eq!(result[0].amount_in, total);
    }

    // ==================== build_split_route Tests ====================

    #[test]
    fn test_build_split_route_remainder_convention() {
        // 3 paths splitting at source: last swap at the split point must
        // have split=0.0.
        //
        //       500 -- pool1 (price=2) --> 1000
        //      /
        //  1000---- 300 -- pool2 (price=3) -->  900
        //      \
        //       200 -- pool3 (price=4) -->  800
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            ("pool1", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("pool2", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(3.0))),
            ("pool3", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(4.0))),
        ]);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![
            PathAllocation {
                hops: vec![HopDescriptor::new(
                    "pool1".to_string(),
                    token_a.clone(),
                    token_b.clone(),
                )
                .with_amounts(BigUint::from(1000u64), gas.clone())],
                flow_fraction: 0.5,
                amount_in: BigUint::from(500u64),
                amount_out: BigUint::from(1000u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: vec![HopDescriptor::new(
                    "pool2".to_string(),
                    token_a.clone(),
                    token_b.clone(),
                )
                .with_amounts(BigUint::from(900u64), gas.clone())],
                flow_fraction: 0.3,
                amount_in: BigUint::from(300u64),
                amount_out: BigUint::from(900u64),
                marginal_price_product: 3.0,
            },
            PathAllocation {
                hops: vec![HopDescriptor::new(
                    "pool3".to_string(),
                    token_a.clone(),
                    token_b.clone(),
                )
                .with_amounts(BigUint::from(800u64), gas)],
                flow_fraction: 0.2,
                amount_in: BigUint::from(200u64),
                amount_out: BigUint::from(800u64),
                marginal_price_product: 4.0,
            },
        ];

        let route = build_split_route(&paths, &market, &ord).unwrap();
        let swaps = route.swaps();

        assert_eq!(swaps.len(), 3);

        // Sorted descending: pool1 (0.5), pool2 (0.3), pool3 (0.2).
        assert_eq!(swaps[0].component_id(), "pool1");
        assert_eq!(*swaps[0].split(), 0.5);
        assert_eq!(swaps[1].component_id(), "pool2");
        assert_eq!(*swaps[1].split(), 0.3);
        assert_eq!(swaps[2].component_id(), "pool3");
        assert_eq!(*swaps[2].split(), 0.0);
    }

    #[test]
    fn test_build_split_route_single_path() {
        // Single path A→B→C: all splits must be 0.0.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "pool_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
        ]);
        let ord = order(&token_a, &token_c, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![PathAllocation {
            hops: vec![
                HopDescriptor::new("pool_ab".to_string(), token_a.clone(), token_b.clone())
                    .with_amounts(BigUint::from(2000u64), gas.clone()),
                HopDescriptor::new("pool_bc".to_string(), token_b, token_c)
                    .with_amounts(BigUint::from(6000u64), gas),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(6000u64),
            marginal_price_product: 6.0,
        }];

        let route = build_split_route(&paths, &market, &ord).unwrap();
        let swaps = route.swaps();

        assert_eq!(swaps.len(), 2);
        for swap in swaps {
            assert_eq!(*swap.split(), 0.0, "single path should produce all-zero splits");
        }
    }

    #[test]
    fn test_build_split_route_shared_first_pool() {
        // Two paths sharing pool P1 at A→B, diverging at B→C (P2 vs P3).
        //
        //                  P2 (price=3) --> C
        //                 /
        //  A -- P1 (2) --B
        //                 \
        //                  P3 (price=4) --> C
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![
            ("P1", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("P2", vec![token_b.clone(), token_c.clone()], Box::new(MockProtocolSim::new(3.0))),
            ("P3", vec![token_b.clone(), token_c.clone()], Box::new(MockProtocolSim::new(4.0))),
        ]);
        let ord = order(&token_a, &token_c, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![
            PathAllocation {
                hops: vec![
                    HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                        .with_amounts(BigUint::from(1400u64), gas.clone()),
                    HopDescriptor::new("P2".to_string(), token_b.clone(), token_c.clone())
                        .with_amounts(BigUint::from(4200u64), gas.clone()),
                ],
                flow_fraction: 0.7,
                amount_in: BigUint::from(700u64),
                amount_out: BigUint::from(4200u64),
                marginal_price_product: 6.0,
            },
            PathAllocation {
                hops: vec![
                    HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                        .with_amounts(BigUint::from(600u64), gas.clone()),
                    HopDescriptor::new("P3".to_string(), token_b.clone(), token_c.clone())
                        .with_amounts(BigUint::from(2400u64), gas),
                ],
                flow_fraction: 0.3,
                amount_in: BigUint::from(300u64),
                amount_out: BigUint::from(1200u64),
                marginal_price_product: 8.0,
            },
        ];

        let route = build_split_route(&paths, &market, &ord).unwrap();
        let swaps = route.swaps();

        // Exactly 3 swaps: one combined A→B, two divergent B→C.
        assert_eq!(swaps.len(), 3, "expected 3 swaps, got {}", swaps.len());

        // First swap: combined A→B via P1 — amount_out is sum of per-path outputs.
        let ab_swap = &swaps[0];
        assert_eq!(ab_swap.component_id(), "P1");
        assert_eq!(
            *ab_swap.amount_in(),
            BigUint::from(1000u64),
            "A→B swap amount_in should equal sum of both paths"
        );
        assert_eq!(
            *ab_swap.amount_out(),
            BigUint::from(2000u64),
            "A→B amount_out should be sum of per-path outputs (1400+600)"
        );
        assert_eq!(
            *ab_swap.split(),
            0.0,
            "A→B is the sole swap in its branch collection, so it gets the remainder convention (split = 0.0)"
        );

        // B→C swaps: P2 (0.7) first, P3 (0.3) last.
        assert_eq!(swaps[1].component_id(), "P2");
        assert_eq!(*swaps[1].split(), 0.7);
        assert_eq!(swaps[2].component_id(), "P3");
        assert_eq!(*swaps[2].split(), 0.0);
    }

    #[test]
    fn test_build_split_route_source_level_split_different_intermediates() {
        // Paths A→B→Z and A→C→Z: source-level split with different
        // intermediate tokens.
        //
        //       pool_ab --> B -- pool_bz
        //      /                         \
        //  A --                           Z
        //      \                         /
        //       pool_ac --> C -- pool_cz
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let token_z = token(0x1A, "Z");
        let market = make_market(vec![
            (
                "pool_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "pool_ac",
                vec![token_a.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
            (
                "pool_bz",
                vec![token_b.clone(), token_z.clone()],
                Box::new(MockProtocolSim::new(4.0)),
            ),
            (
                "pool_cz",
                vec![token_c.clone(), token_z.clone()],
                Box::new(MockProtocolSim::new(5.0)),
            ),
        ]);
        let ord = order(&token_a, &token_z, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![
            PathAllocation {
                hops: vec![
                    HopDescriptor::new("pool_ab".to_string(), token_a.clone(), token_b.clone())
                        .with_amounts(BigUint::from(1200u64), gas.clone()),
                    HopDescriptor::new("pool_bz".to_string(), token_b, token_z.clone())
                        .with_amounts(BigUint::from(4800u64), gas.clone()),
                ],
                flow_fraction: 0.6,
                amount_in: BigUint::from(600u64),
                amount_out: BigUint::from(4800u64),
                marginal_price_product: 8.0,
            },
            PathAllocation {
                hops: vec![
                    HopDescriptor::new("pool_ac".to_string(), token_a.clone(), token_c.clone())
                        .with_amounts(BigUint::from(1200u64), gas.clone()),
                    HopDescriptor::new("pool_cz".to_string(), token_c, token_z)
                        .with_amounts(BigUint::from(6000u64), gas),
                ],
                flow_fraction: 0.4,
                amount_in: BigUint::from(400u64),
                amount_out: BigUint::from(6000u64),
                marginal_price_product: 15.0,
            },
        ];

        let route = build_split_route(&paths, &market, &ord).unwrap();
        let swaps = route.swaps();

        assert_eq!(swaps.len(), 4, "expected 4 swaps (2 source + 2 intermediate)");

        // Source-level split: pool_ab (0.6) first, pool_ac (0.4) last.
        assert_eq!(swaps[0].component_id(), "pool_ab");
        assert_eq!(*swaps[0].split(), 0.6);
        assert_eq!(*swaps[0].amount_in(), BigUint::from(600u64));
        assert_eq!(*swaps[0].amount_out(), BigUint::from(1200u64));

        assert_eq!(swaps[1].component_id(), "pool_ac");
        assert_eq!(*swaps[1].split(), 0.0);
        assert_eq!(*swaps[1].amount_in(), BigUint::from(400u64));
        assert_eq!(*swaps[1].amount_out(), BigUint::from(1200u64));

        // Intermediate swaps: single hops from B and C, all split=0.0.
        assert_eq!(swaps[2].component_id(), "pool_bz");
        assert_eq!(*swaps[2].split(), 0.0);
        assert_eq!(*swaps[2].amount_in(), BigUint::from(1200u64));
        assert_eq!(*swaps[2].amount_out(), BigUint::from(4800u64));

        assert_eq!(swaps[3].component_id(), "pool_cz");
        assert_eq!(*swaps[3].split(), 0.0);
        assert_eq!(*swaps[3].amount_in(), BigUint::from(1200u64));
        assert_eq!(*swaps[3].amount_out(), BigUint::from(6000u64));
    }
}
