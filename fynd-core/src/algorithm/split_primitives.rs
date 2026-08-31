use std::collections::VecDeque;

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use rustc_hash::{FxHashMap, FxHashSet};
use tycho_simulation::tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::{sim_meter, sim_meter::MeteredProtocolSim};
use crate::{
    algorithm::AlgorithmError,
    feed::market_data::MarketState,
    types::{ComponentId, Order, Route, Swap},
};

/// The stage every swap [`simulate_path`] makes is booked under.
const SIMULATE_PATH_STAGE: sim_meter::StageLabel = "simulate_path";

/// The stage every swap [`execute_split_plan`] makes is booked under.
const SPLIT_PLAN_STAGE: sim_meter::StageLabel = "execute_split_plan";

#[derive(Clone)]
pub(crate) struct HopDescriptor {
    pub(crate) component_id: ComponentId,
    pub(crate) token_in: Token,
    pub(crate) token_out: Token,
}

impl HopDescriptor {
    pub(crate) fn new(component_id: ComponentId, token_in: Token, token_out: Token) -> Self {
        Self { component_id, token_in, token_out }
    }

    #[cfg(test)]
    pub(crate) fn with_amounts(self, amount_out: BigUint, gas: BigUint) -> SimulatedHop {
        SimulatedHop { descriptor: self, amount_out, gas }
    }
}

/// A [`HopDescriptor`] paired with its simulation result. Used in
/// [`PathAllocation::hops`] where the solving algorithm has already
/// computed per-hop outputs and gas.
#[derive(Clone)]
pub(crate) struct SimulatedHop {
    pub(crate) descriptor: HopDescriptor,
    pub(crate) amount_out: BigUint,
    pub(crate) gas: BigUint,
}

/// A fully-simulated path allocation.
///
/// One path in the current split solution, with the fraction of total `amount_in`
/// currently allocated to it. All fractions across allocations sum to 1.0.
#[derive(Clone)]
pub(crate) struct PathAllocation {
    pub(crate) hops: Vec<SimulatedHop>,
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
    pub(crate) fn validate_token_cycles(&self) -> Result<(), AlgorithmError> {
        if self.hops.is_empty() {
            return Err(AlgorithmError::Other("path has no hops".to_string()));
        }
        let first_token = &self.hops[0].descriptor.token_in.address;
        let mut seen = FxHashSet::default();
        seen.insert(first_token.clone());
        let last_idx = self.hops.len() - 1;
        for (i, hop) in self.hops.iter().enumerate() {
            let out_addr = &hop.descriptor.token_out.address;
            if !seen.insert(out_addr.clone()) {
                let is_valid_round_trip = i == last_idx && out_addr == first_token;
                if !is_valid_round_trip {
                    return Err(AlgorithmError::Other(format!(
                        "path revisits token {out_addr} at hop {i} \
                         (would corrupt merge_shared_hops)",
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
    pub(crate) marginal_price_product: f64,
    /// Per-hop `(amount_out, gas)` in path order.
    pub(crate) hop_results: Vec<(BigUint, BigUint)>,
    /// Per-hop post-swap component states in path order. Apply these as overrides
    /// before simulating another path so shared components see depleted reserves.
    pub(crate) post_swap_states: Vec<(ComponentId, Box<dyn ProtocolSim>)>,
}

/// Component state overrides for passing degraded states to `find_single_route`.
#[derive(Default)]
pub(crate) struct MarketOverrides(FxHashMap<ComponentId, Box<dyn ProtocolSim>>);

impl MarketOverrides {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Insert a degraded component state as an override.
    pub(crate) fn with_override(mut self, id: ComponentId, sim: Box<dyn ProtocolSim>) -> Self {
        self.0.insert(id, sim);
        self
    }

    /// Wraps an existing override entry so that `get_amount_out().gas` is zero for
    /// the specified `(token_in, token_out)` pair, but unchanged for other pairs
    /// through the same component.
    ///
    /// Different token pairs through the same component are separate on-chain swaps with
    /// independent gas costs, so only committed pairs should be zeroed. Call this
    /// once per committed `(component_id, token_in, token_out)` triple.
    ///
    /// Multiple calls for the same component accumulate pairs. If the ID has no
    /// override entry, this is a no-op.
    pub(crate) fn with_zero_gas(
        mut self,
        id: ComponentId,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Self {
        if let Some(sim) = self.0.remove(&id) {
            // If already wrapped, add the new pair to the existing set.
            let wrapped = if let Some(selective) = sim
                .as_any()
                .downcast_ref::<SelectiveZeroGasSim>()
            {
                let mut pairs = selective.zero_gas_pairs.clone();
                pairs.insert((token_in, token_out));
                Box::new(SelectiveZeroGasSim {
                    inner: selective.inner.clone_box(),
                    zero_gas_pairs: pairs,
                }) as Box<dyn ProtocolSim>
            } else {
                let mut pairs = FxHashSet::default();
                pairs.insert((token_in, token_out));
                Box::new(SelectiveZeroGasSim { inner: sim, zero_gas_pairs: pairs })
            };
            self.0.insert(id, wrapped);
        }
        self
    }

    pub(crate) fn get(&self, id: &ComponentId) -> Option<&dyn ProtocolSim> {
        self.0.get(id).map(|b| b.as_ref())
    }

    /// Commits a post-swap component state, replacing whatever was there.
    ///
    /// The building counterpart to [`MarketOverrides::with_override`], for the passes that fill an
    /// overlay chunk by chunk rather than declaring one up front.
    pub(crate) fn insert(&mut self, id: ComponentId, sim: Box<dyn ProtocolSim>) {
        self.0.insert(id, sim);
    }
}

/// Wrapper that delegates all [`ProtocolSim`] calls unchanged except
/// [`get_amount_out`](ProtocolSim::get_amount_out), where it zeroes the returned gas
/// only for token pairs in `zero_gas_pairs`. Other pairs pass through unchanged.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SelectiveZeroGasSim {
    inner: Box<dyn ProtocolSim>,
    zero_gas_pairs: FxHashSet<(Bytes, Bytes)>,
}

#[typetag::serde]
impl ProtocolSim for SelectiveZeroGasSim {
    fn fee(&self) -> f64 {
        self.inner.fee()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.inner.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let mut result = self
            .inner
            .get_amount_out(amount_in, token_in, token_out)?;
        if self
            .zero_gas_pairs
            .contains(&(token_in.address.clone(), token_out.address.clone()))
        {
            result.gas = BigUint::ZERO;
        }
        result.new_state = Box::new(SelectiveZeroGasSim {
            inner: result.new_state,
            zero_gas_pairs: self.zero_gas_pairs.clone(),
        });
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.inner
            .get_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &std::collections::HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.inner
            .delta_transition(delta, tokens, balances)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(SelectiveZeroGasSim {
            inner: self.inner.clone_box(),
            zero_gas_pairs: self.zero_gas_pairs.clone(),
        })
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
            .map(|o| self.inner.eq(&*o.inner) && self.zero_gas_pairs == o.zero_gas_pairs)
            .unwrap_or(false)
    }
}

/// Find the `x` in `[lo, hi]` that maximises `f(x)` using golden-section search.
///
/// Assumes `f` is roughly unimodal (has one maximum). `max_evals` controls the
/// number of function evaluations (higher = more precise but slower).
pub(crate) fn golden_section_search(
    mut f: impl FnMut(f64) -> f64,
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
/// For each hop, the path's own post-swap states are checked first, then
/// `overrides`, then the live market state. Returns the final output amount,
/// per-hop results, the post-swap component states, and the marginal price product, the spot
/// prices at the state each hop executed against.
pub(crate) fn simulate_path(
    hops: &[HopDescriptor],
    amount_in: &BigUint,
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<SimResult, AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut hop_results = Vec::with_capacity(hops.len());
    let mut post_swap_states: Vec<(ComponentId, Box<dyn ProtocolSim>)> =
        Vec::with_capacity(hops.len());
    let mut marginal_price_product = 1.0;

    for hop in hops {
        // Prefer this path's own post-swap state so a component reused by an
        // earlier hop is simulated on depleted reserves, not fresh ones.
        let sim = post_swap_states
            .iter()
            .rev()
            .find(|(id, _)| id == &hop.component_id)
            .map(|(_, state)| state.as_ref())
            .or_else(|| overrides.get(&hop.component_id))
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
        marginal_price_product *= price;

        let result = sim
            .get_amount_out_metered(
                &hop.component_id,
                SIMULATE_PATH_STAGE,
                current_amount,
                &hop.token_in,
                &hop.token_out,
            )
            .map_err(|e| AlgorithmError::SimulationFailed {
                component_id: hop.component_id.clone(),
                error: e.to_string(),
            })?;

        hop_results.push((result.amount.clone(), result.gas));
        current_amount = result.amount;
        post_swap_states.push((hop.component_id.clone(), result.new_state));
    }

    Ok(SimResult {
        amount_out: current_amount,
        marginal_price_product,
        hop_results,
        post_swap_states,
    })
}

/// Builds post-swap component states after all paths in a split-route solution
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
/// Swaps are simulated in the same topological order as the final route, so
/// candidate discovery sees the exact state the executable split leaves behind.
pub(crate) fn build_post_swap_overrides(
    paths: &[PathAllocation],
    market: &MarketState,
) -> Result<MarketOverrides, AlgorithmError> {
    let Some(root_hop) = paths
        .first()
        .and_then(|path| path.hops.first())
    else {
        return Ok(MarketOverrides::empty());
    };
    let total_amount = paths
        .iter()
        .map(|path| path.amount_in.clone())
        .sum();
    Ok(execute_split_plan(
        paths,
        market,
        &root_hop.descriptor.token_in.address,
        &total_amount,
        &MarketOverrides::empty(),
    )?
    .post_swap)
}

/// What makes two paths' hops the same on-chain swap: one pool, taken in one direction.
type HopKey = (ComponentId, Bytes, Bytes);

fn hop_key(hop: &HopDescriptor) -> HopKey {
    (hop.component_id.clone(), hop.token_in.address.clone(), hop.token_out.address.clone())
}

struct SplitSwap {
    hop: HopDescriptor,
    split: f64,
    amount_in: BigUint,
}

/// One component swap in a split route, after it has been simulated.
struct SimulatedSplitSwap {
    hop: HopDescriptor,
    split: f64,
    amount_in: BigUint,
    amount_out: BigUint,
    gas: BigUint,
    pre_swap_state: Box<dyn ProtocolSim>,
}

/// The outcome of simulating a whole split route.
struct SplitExecution {
    swaps: Vec<SimulatedSplitSwap>,
    available: FxHashMap<Bytes, BigUint>,
    post_swap: MarketOverrides,
    total_gas: u64,
}

/// Merge shared hops across paths, summing their flow fractions, and return
/// them collected by `token_in` (sorted by amount descending within each
/// branch collection).
fn merge_shared_hops(
    paths: &[PathAllocation],
) -> Result<FxHashMap<Bytes, Vec<SplitSwap>>, AlgorithmError> {
    let mut hops: FxHashMap<HopKey, SplitSwap> = FxHashMap::default();

    for path in paths {
        for hop in &path.hops {
            let desc = &hop.descriptor;
            let key: HopKey = (
                desc.component_id.clone(),
                desc.token_in.address.clone(),
                desc.token_out.address.clone(),
            );
            hops.entry(key).or_insert(SplitSwap {
                hop: HopDescriptor::new(
                    desc.component_id.clone(),
                    desc.token_in.clone(),
                    desc.token_out.clone(),
                ),
                // Both set by `splits_from_amounts`, from the amounts the paths standing at this
                // token actually carry.
                split: 0.0,
                amount_in: BigUint::ZERO,
            });
        }
    }

    let mut branch_collections: FxHashMap<Bytes, Vec<SplitSwap>> = FxHashMap::default();
    for (_, swap) in hops {
        branch_collections
            .entry(swap.hop.token_in.address.clone())
            .or_default()
            .push(swap);
    }
    // Only for determinism: `splits_from_amounts` re-sorts each collection by the amount its swap
    // carries, and this decides the order of swaps carrying equal amounts.
    for branch_collection in branch_collections.values_mut() {
        branch_collection.sort_by(|a, b| {
            a.hop
                .component_id
                .cmp(&b.hop.component_id)
                .then_with(|| {
                    a.hop
                        .token_in
                        .address
                        .cmp(&b.hop.token_in.address)
                })
                .then_with(|| {
                    a.hop
                        .token_out
                        .address
                        .cmp(&b.hop.token_out.address)
                })
        });
    }
    Ok(branch_collections)
}

/// Turns the amounts the execution wants into the fractions the encoder carries — and then back
/// into the amounts the encoder will actually produce.
///
/// On chain a split swap does not carry an amount. It carries the share of the balance held in its
/// input token that it should take, and the last swap of a group takes whatever is left, which is
/// what `split = 0.0` means. The fractions therefore come from the amounts the execution
/// attributed to each path.
///
/// The round trip back through [`fractions_to_amounts`] is not redundant. A fraction is an `f64`
/// and the amounts are integers, so the amount a fraction produces is not exactly the amount it
/// came from. `replay_route` derives its amounts from the fractions, so the execution has to as
/// well, or the two disagree on what the same route pays.
///
/// It does not make the quote exact against the chain. `tycho-execution` encodes the share as a
/// `uint24`, so what the router divides by is the fraction rounded to one part in 2^24 — about
/// `6e-8` of the branch, which the `split = 0.0` swap absorbs for the whole group.
fn splits_from_amounts(mut hops: Vec<SplitSwap>, total_available: &BigUint) -> Vec<SplitSwap> {
    // Largest first, so the remainder convention lands on the smallest share, as it did when the
    // order came from the paths' summed flow fractions. The sort also fixes the order the swaps
    // execute in, and swaps sharing a pool deplete it for each other, so reversing it would move
    // the quoted output rather than only moving the rounding remainder.
    hops.sort_by(|left, right| right.amount_in.cmp(&left.amount_in));

    let last = hops.len().saturating_sub(1);
    let fractions: Vec<f64> = hops
        .iter()
        .enumerate()
        .map(|(ix, swap)| if ix == last { 0.0 } else { share_of(&swap.amount_in, total_available) })
        .collect();

    let amounts = fractions_to_amounts(total_available, &fractions)
        .unwrap_or_else(|_| vec![total_available.clone()]);
    for ((swap, split), amount) in hops
        .iter_mut()
        .zip(fractions)
        .zip(amounts)
    {
        swap.split = split;
        swap.amount_in = amount;
    }
    hops
}

/// What share of `total` the `part` is, or zero when there is nothing to divide by.
fn share_of(part: &BigUint, total: &BigUint) -> f64 {
    match (part.to_f64(), total.to_f64()) {
        (Some(part), Some(total)) if total > 0.0 => part / total,
        _ => 0.0,
    }
}

/// Divides `output` between the paths that fed a swap, in proportion to what each put in.
///
/// One pool swapped once pays one amount, and each path's share of it is its share of the input —
/// that is what the on-chain swap does, and there is nothing else it could mean. The last path
/// takes the rounding remainder so the shares add back to `output` exactly.
fn share_output(output: &BigUint, fed_amounts: &[BigUint]) -> Vec<BigUint> {
    let total: BigUint = fed_amounts.iter().sum();
    if total.is_zero() {
        return vec![BigUint::ZERO; fed_amounts.len()];
    }
    let mut shares: Vec<BigUint> = fed_amounts
        .iter()
        .map(|fed| output * fed / &total)
        .collect();
    let assigned: BigUint = shares.iter().sum();
    if let Some(last) = shares.last_mut() {
        *last += output - assigned;
    }
    shares
}

/// Counts, per token, how many swaps produce it, so the traversal only swaps a
/// token once all its inflows have arrived.
fn build_in_degree(hops_by_token: &FxHashMap<Bytes, Vec<SplitSwap>>) -> FxHashMap<Bytes, usize> {
    let mut in_degree: FxHashMap<Bytes, usize> = FxHashMap::default();
    for (token_in_addr, branch_collection) in hops_by_token {
        in_degree
            .entry(token_in_addr.clone())
            .or_insert(0);
        for swap in branch_collection {
            *in_degree
                .entry(swap.hop.token_out.address.clone())
                .or_insert(0) += 1;
        }
    }
    in_degree
}

/// Each path's own money, and how far along its hops it has travelled.
///
/// The two vectors are indexed together by path, which is why they live behind one type: a path's
/// amount and its position are meaningless apart. Tracking them is what stops an intermediate token
/// being pooled and re-divided by the paths' shares of the *order* — see [`execute_split_plan`].
struct PathLedger {
    amount_in: Vec<BigUint>,
    next_hop_ix: Vec<usize>,
}

impl PathLedger {
    /// Starts each path on the amount its allocation carries.
    ///
    /// The amounts come from the allocations rather than being re-derived from their fractions:
    /// the caller has already decided what each path carries, and a fraction is an `f64`. They set
    /// the proportions only — every swap amount is divided out of the balance actually standing at
    /// its token — so they are not required to sum to the order exactly.
    ///
    /// # Errors
    ///
    /// [`AlgorithmError::Other`] when every path carries nothing. That describes no split at all,
    /// and guessing an allocation for it would misprice the quote it produces.
    fn new(paths: &[PathAllocation]) -> Result<Self, AlgorithmError> {
        let amount_in: Vec<BigUint> = paths
            .iter()
            .map(|path| path.amount_in.clone())
            .collect();
        if amount_in.iter().all(BigUint::is_zero) {
            return Err(AlgorithmError::Other(
                "cannot divide the order across these paths: every path carries a zero amount"
                    .to_string(),
            ));
        }
        Ok(Self { next_hop_ix: vec![0; paths.len()], amount_in })
    }

    /// Which paths are standing at `token`, grouped by the swap each is about to make.
    ///
    /// Every path that passes through the token has arrived: the token is only released once every
    /// hop producing it has run.
    fn standing_at(
        &self,
        paths: &[PathAllocation],
        token: &Bytes,
    ) -> FxHashMap<HopKey, Vec<usize>> {
        let mut by_hop: FxHashMap<HopKey, Vec<usize>> = FxHashMap::default();
        for (path_ix, path) in paths.iter().enumerate() {
            let Some(hop) = path.hops.get(self.next_hop_ix[path_ix]) else {
                continue;
            };
            if hop.descriptor.token_in.address == *token {
                by_hop
                    .entry(hop_key(&hop.descriptor))
                    .or_default()
                    .push(path_ix);
            }
        }
        by_hop
    }

    /// What the paths feeding one swap carry between them.
    fn fed_amounts(&self, fed: &[usize]) -> Vec<BigUint> {
        fed.iter()
            .map(|&path_ix| self.amount_in[path_ix].clone())
            .collect()
    }

    /// Divides `total` between the paths that fed a swap, in proportion to what each put in.
    fn rescale(&mut self, fed: &[usize], total: &BigUint) {
        let fed_amounts = self.fed_amounts(fed);
        for (&path_ix, share) in fed
            .iter()
            .zip(share_output(total, &fed_amounts))
        {
            self.amount_in[path_ix] = share;
        }
    }

    /// Moves the paths that fed a swap on to their next hop.
    fn advance(&mut self, fed: &[usize]) {
        for &path_ix in fed {
            self.next_hop_ix[path_ix] += 1;
        }
    }
}

/// What stands at each token as the plan executes.
struct TokenBalances(FxHashMap<Bytes, BigUint>);

impl TokenBalances {
    fn starting(token: &Bytes, amount: &BigUint) -> Self {
        Self(FxHashMap::from_iter([(token.clone(), amount.clone())]))
    }

    /// What stands at `token`, which is nothing for a token nothing has produced.
    fn at(&self, token: &Bytes) -> BigUint {
        self.0
            .get(token)
            .cloned()
            .unwrap_or_default()
    }

    /// Takes what a swap spends out of its input token.
    ///
    /// Without this a path ending back on the token it started from would count the order's own
    /// input as output, because [`evaluate_total_output`] reads the balance standing at each
    /// terminal token.
    ///
    /// # Errors
    ///
    /// [`AlgorithmError::Other`] when the swap spends more than stands at the token. The traversal
    /// releases a token only once every hop producing it has run, so that means the plan disagrees
    /// with itself.
    fn spend(
        &mut self,
        token: &Token,
        component_id: &ComponentId,
        amount: &BigUint,
    ) -> Result<(), AlgorithmError> {
        let standing = self
            .0
            .entry(token.address.clone())
            .or_default();
        if *standing < *amount {
            return Err(AlgorithmError::Other(format!(
                "the swap through {component_id} spends {amount} {}, more than the {standing} \
                 standing at it",
                token.symbol,
            )));
        }
        *standing -= amount;
        Ok(())
    }

    fn credit(&mut self, token: &Bytes, amount: &BigUint) {
        *self.0.entry(token.clone()).or_default() += amount;
    }

    fn into_inner(self) -> FxHashMap<Bytes, BigUint> {
        self.0
    }
}

/// Sizes one token's merged swaps from the amounts the paths standing at it carry, and pairs each
/// with the paths that feed it.
///
/// # Errors
///
/// [`AlgorithmError::Other`] when a merged swap has no path standing at it. Every merged swap was
/// built from the hops of these paths, and a token is only released once every hop producing it has
/// run, so one of them is standing here. No feeder means the traversal is broken, and a zero-amount
/// swap in the route would hide that.
fn amounts_for_branch(
    branch: Vec<SplitSwap>,
    standing: &FxHashMap<HopKey, Vec<usize>>,
    ledger: &PathLedger,
    total: &BigUint,
) -> Result<Vec<(SplitSwap, Vec<usize>)>, AlgorithmError> {
    let sized: Vec<SplitSwap> = branch
        .into_iter()
        .map(|mut split_swap| {
            let fed = standing
                .get(&hop_key(&split_swap.hop))
                .ok_or_else(|| {
                    AlgorithmError::Other(format!(
                        "no path feeds the swap through {} at this point in the plan",
                        split_swap.hop.component_id,
                    ))
                })?;
            split_swap.amount_in = ledger
                .fed_amounts(fed)
                .into_iter()
                .sum();
            Ok(split_swap)
        })
        .collect::<Result<_, AlgorithmError>>()?;

    // Paired after the sort, so each swap keeps the feeders it was sized from.
    Ok(splits_from_amounts(sized, total)
        .into_iter()
        .map(|split_swap| {
            let fed = standing
                .get(&hop_key(&split_swap.hop))
                .cloned()
                .unwrap_or_default();
            (split_swap, fed)
        })
        .collect())
}

/// Simulates one merged swap against the freshest state the plan holds for its component.
///
/// Returns the executed swap and the component state it left behind.
///
/// # Errors
///
/// [`AlgorithmError::DataNotFound`] when no state can be found for the component, and
/// [`AlgorithmError::SimulationFailed`] when the component refuses the swap.
fn run_merged_swap(
    swap: SplitSwap,
    market: &MarketState,
    base_overrides: &MarketOverrides,
    post_swap: &MarketOverrides,
) -> Result<(SimulatedSplitSwap, Box<dyn ProtocolSim>), AlgorithmError> {
    let sim = post_swap
        .get(&swap.hop.component_id)
        .or_else(|| base_overrides.get(&swap.hop.component_id))
        .or_else(|| market.get_simulation_state(&swap.hop.component_id))
        .ok_or_else(|| AlgorithmError::DataNotFound {
            kind: "simulation state",
            id: Some(swap.hop.component_id.clone()),
        })?;

    let result = sim
        .get_amount_out_metered(
            &swap.hop.component_id,
            SPLIT_PLAN_STAGE,
            swap.amount_in.clone(),
            &swap.hop.token_in,
            &swap.hop.token_out,
        )
        .map_err(|e| AlgorithmError::SimulationFailed {
            component_id: swap.hop.component_id.clone(),
            error: e.to_string(),
        })?;

    let executed = SimulatedSplitSwap {
        hop: swap.hop,
        split: swap.split,
        amount_in: swap.amount_in,
        amount_out: result.amount,
        gas: result.gas,
        pre_swap_state: sim.clone_box(),
    };
    Ok((executed, result.new_state))
}

/// Simulates a split route from start token to outputs and returns the outcome.
///
/// Swaps run in dependency order, so a token is only traded once every hop producing it has run,
/// and every swap sees the component state left by the swaps before it — so paths sharing a
/// component no longer each assume fresh liquidity.
///
/// A token is *not* pooled and re-divided. Each path keeps its own amount through its own hops, and
/// a swap several paths share divides its output between them in proportion to what each put in,
/// which is what the single on-chain swap does. This one pass backs scoring, candidate discovery,
/// and route assembly, so all three agree on the same executable route. Round-trips that
/// end on the input token are supported.
///
/// Errors if a path revisits an intermediate token, a component cannot be simulated, the merged
/// swaps cannot be ordered (a genuine dependency cycle), every path carries a zero amount, or a
/// merged swap has no path standing at it when its token is released.
fn execute_split_plan(
    paths: &[PathAllocation],
    market: &MarketState,
    start_token: &Bytes,
    start_amount: &BigUint,
    base_overrides: &MarketOverrides,
) -> Result<SplitExecution, AlgorithmError> {
    for path in paths {
        path.validate_token_cycles()?;
    }

    let mut hops_by_token = merge_shared_hops(paths)?;
    let mut in_degree = build_in_degree(&hops_by_token);
    let mut ready = VecDeque::from([start_token.clone()]);

    let mut ledger = PathLedger::new(paths)?;
    let mut balances = TokenBalances::starting(start_token, start_amount);
    let mut swaps = Vec::new();
    let mut post_swap = MarketOverrides::empty();
    let mut total_gas: u64 = 0;

    while let Some(token_addr) = ready.pop_front() {
        let Some(branch) = hops_by_token.remove(&token_addr) else {
            continue;
        };
        let standing = ledger.standing_at(paths, &token_addr);
        let sized = amounts_for_branch(branch, &standing, &ledger, &balances.at(&token_addr))?;

        // The executed amount can differ by a wei from what the paths standing here asked for,
        // because it came back through the encoder's own fraction arithmetic. Re-attribute it so
        // their amounts add up to what is really being swapped.
        for (split_swap, fed) in &sized {
            ledger.rescale(fed, &split_swap.amount_in);
        }

        for (split_swap, fed) in sized {
            let token_in = split_swap.hop.token_in.clone();
            let token_out = split_swap.hop.token_out.address.clone();
            let component_id = split_swap.hop.component_id.clone();

            balances.spend(&token_in, &component_id, &split_swap.amount_in)?;
            let (executed, new_state) =
                run_merged_swap(split_swap, market, base_overrides, &post_swap)?;
            balances.credit(&token_out, &executed.amount_out);

            // Hand the output back to the paths that fed this swap, in proportion to what each put
            // in, and move them to their next hop. A path's amount stays its own rather than being
            // pooled at the token and re-divided by shares that describe the order.
            ledger.rescale(&fed, &executed.amount_out);
            ledger.advance(&fed);

            total_gas = total_gas.saturating_add(
                executed
                    .gas
                    .to_u64()
                    .unwrap_or(u64::MAX),
            );
            swaps.push(executed);
            post_swap = post_swap.with_override(component_id, new_state);

            // Decrement in-degree; enqueue when all inflows are ready.
            if let Some(deg) = in_degree.get_mut(&token_out) {
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    ready.push_back(token_out);
                }
            }
        }
    }

    if !hops_by_token.is_empty() {
        let stuck: Vec<_> = hops_by_token
            .keys()
            .map(|k| format!("{k}"))
            .collect();
        return Err(AlgorithmError::Other(format!(
            "dependency cycle — unprocessed tokens: [{}]",
            stuck.join(", "),
        )));
    }

    Ok(SplitExecution { swaps, available: balances.into_inner(), post_swap, total_gas })
}

/// Turns the parallel paths/fractions the line search works in into the
/// allocations `execute_split_plan` consumes, dividing the input amount across
/// paths by fraction. Simulation-derived fields are left empty for the plan to
/// fill in.
///
/// Errors if the two slices differ in length or any path is empty.
fn allocations_from_descriptors(
    paths: &[&[HopDescriptor]],
    fractions: &[f64],
    total_amount: &BigUint,
) -> Result<Vec<PathAllocation>, AlgorithmError> {
    if paths.len() != fractions.len() {
        return Err(AlgorithmError::Other(format!(
            "paths/fractions length mismatch: {} paths, {} fractions",
            paths.len(),
            fractions.len(),
        )));
    }
    let amounts = fractions_to_amounts(total_amount, fractions)
        .map_err(|e| AlgorithmError::Other(e.to_string()))?;

    paths
        .iter()
        .zip(fractions.iter())
        .zip(amounts)
        .map(|((path, &flow_fraction), amount_in)| {
            if path.is_empty() {
                return Err(AlgorithmError::Other("path has no hops".to_string()));
            }
            Ok(PathAllocation {
                hops: path
                    .iter()
                    .cloned()
                    .map(|descriptor| SimulatedHop {
                        descriptor,
                        amount_out: BigUint::ZERO,
                        gas: BigUint::ZERO,
                    })
                    .collect(),
                flow_fraction,
                amount_in,
                amount_out: BigUint::ZERO,
                marginal_price_product: 0.0,
            })
        })
        .collect()
}

/// Simulates all paths at their current fractions and returns
/// `(total_amount_out, total_gas)`. `paths[i]` corresponds to `fractions[i]`.
///
/// Uses the same merged topological execution plan as `build_split_route`, so
/// the optimiser scores the route that will actually be emitted.
pub(crate) fn evaluate_total_output(
    paths: &[&[HopDescriptor]],
    fractions: &[f64],
    total_amount: &BigUint,
    market: &MarketState,
    overrides: &MarketOverrides,
) -> Result<(BigUint, u64), AlgorithmError> {
    let first_hop = paths
        .first()
        .and_then(|path| path.first())
        .ok_or_else(|| AlgorithmError::Other("paths must not be empty".to_string()))?;
    let terminal_tokens: FxHashSet<Bytes> = paths
        .iter()
        .map(|path| {
            path.last()
                .map(|hop| hop.token_out.address.clone())
                .ok_or_else(|| AlgorithmError::Other("path has no hops".to_string()))
        })
        .collect::<Result<_, AlgorithmError>>()?;
    let allocations = allocations_from_descriptors(paths, fractions, total_amount)?;
    let execution = execute_split_plan(
        &allocations,
        market,
        &first_hop.token_in.address,
        total_amount,
        overrides,
    )?;
    let total_out = terminal_tokens
        .iter()
        .map(|token| {
            execution
                .available
                .get(token)
                .cloned()
                .unwrap_or_default()
        })
        .sum();
    Ok((total_out, execution.total_gas))
}

/// Assembles a [`Route`] from split-route path allocations with shared-hop
/// deduplication.
///
/// Paths may share component hops (same `component_id`, `token_in`, `token_out`).
/// When they do, this function emits one combined swap rather than duplicates.
/// Within each branch collection of swaps sharing a `token_in`, the tycho-execution
/// remainder convention is applied: sorted by amount descending, all but the
/// last receive their explicit split fraction, while the last gets
/// `split = 0.0` (meaning "use all remaining balance").
///
/// # Swap ordering
///
/// Swaps are emitted in topological order (Kahn's algorithm): a token's
/// outgoing swaps are only emitted once every upstream swap producing it
/// has been emitted.
///
/// Why this matters:
/// - `merge_shared_hops` collapses a shared component hop into one swap (not one per path), saving
///   gas by calling the component once with combined input.
/// - That single swap's split fraction is computed against the full token balance, so all inflows
///   must be complete before it is emitted.
/// - The in-degree of each token tracks how many upstream swaps produce it; the token is processed
///   once all of them are done.
///
/// Note: the TychoRouter contract *could* support interleaved splits
/// (partial consume, more inflows, consume rest), but that would require
/// an extra swap on the same component, spending more gas.
///
/// For example, given paths of different lengths that converge on the same
/// intermediate token:
///
/// ```text
/// Path 1 (2 hops): WETH -> USDC -(component A)-> DAI
/// Path 2 (3 hops): WETH -> USDT -> USDC -(component A)-> DAI
/// ```
///
/// Component A (USDC→DAI) is merged into one swap. If USDC were visited before
/// the USDT→USDC hop completes, Component A would see only Path 1's USDC. The
/// topological sort prevents this by waiting for all inflows to USDC
/// before emitting Component A's swap. This extends to downstream splits too:
///
/// ```text
/// Path 1: WETH -> USDC -> DAI (Component A) -> PEPE (Component B)  (0.5)
/// Path 2: WETH -> USDC -> DAI (Component A) -> PEPE (Component C)  (0.5)
/// Path 3: WETH -> USDT -> USDC -> DAI (Component A) -> PEPE (Component B or C)
/// ```
///
/// The DAI→PEPE split between Component B and Component C must wait until all DAI
/// has been produced (from both paths through the merged Component A swap).
pub(crate) fn build_split_route(
    paths: &[PathAllocation],
    market: &MarketState,
    order: &Order,
) -> Result<Route, AlgorithmError> {
    let execution = execute_split_plan(
        paths,
        market,
        order.token_in(),
        order.amount(),
        &MarketOverrides::empty(),
    )?;
    let mut swaps = Vec::new();
    let mut route_tokens: FxHashMap<Bytes, Token> = FxHashMap::default();

    for executed in execution.swaps {
        let component = market
            .get_component(&executed.hop.component_id)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "protocol component",
                id: Some(executed.hop.component_id.clone()),
            })?;

        let in_addr = executed.hop.token_in.address.clone();
        let out_addr = executed.hop.token_out.address.clone();
        swaps.push(
            Swap::new(
                executed.hop.component_id,
                component.protocol_system.clone(),
                in_addr.clone(),
                out_addr.clone(),
                executed.amount_in,
                executed.amount_out,
                executed.gas,
                component.clone(),
                executed.pre_swap_state,
            )
            .with_split(executed.split),
        );
        route_tokens
            .entry(in_addr)
            .or_insert(executed.hop.token_in);
        route_tokens
            .entry(out_addr.clone())
            .or_insert(executed.hop.token_out);
    }

    Ok(Route::new(swaps, route_tokens)?)
}

#[cfg(test)]
mod tests {
    use num_bigint::BigInt;
    use rstest::rstest;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            component, order, token, ConstantProductSim, DivByZeroSim, MockProtocolSim,
        },
        types::OrderSide,
    };

    fn make_market(components: Vec<(&str, Vec<Token>, Box<dyn ProtocolSim>)>) -> MarketState {
        let mut market = MarketState::new();
        for (component_id, tokens, sim) in components {
            market.upsert_components(std::iter::once(component(component_id, &tokens)));
            market.update_states([(component_id.to_string(), sim)]);
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

    // ==================== PathAllocation::validate_token_cycles Tests ====================

    #[test]
    fn test_validate_token_cycles_valid_path() {
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
        assert!(path.validate_token_cycles().is_ok());
    }

    #[test]
    fn test_validate_token_cycles_empty_hops() {
        let path = PathAllocation {
            hops: vec![],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(100u64),
            marginal_price_product: 1.0,
        };
        assert!(path.validate_token_cycles().is_err());
    }

    #[test]
    fn test_validate_token_cycles_valid_round_trip() {
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
        assert!(path.validate_token_cycles().is_ok());
    }

    #[test]
    fn test_validate_token_cycles_rejects_mid_path_cycle() {
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
        assert!(path.validate_token_cycles().is_err());
    }

    #[test]
    fn test_validate_token_cycles_rejects_intermediate_revisit() {
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
        assert!(path.validate_token_cycles().is_err());
    }

    // ==================== Simulation Utility Tests ====================

    #[test]
    fn test_compute_marginal_price_product_single_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "component_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(3.0)),
        )]);

        let hops = [HopDescriptor::new("component_ab".to_string(), token_a, token_b)];

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
                "component_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "component_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(4.0)),
            ),
        ]);

        let hops = [
            HopDescriptor::new("component_ab".to_string(), token_a, token_b.clone()),
            HopDescriptor::new("component_bc".to_string(), token_b, token_c),
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
            "component_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(3.0)),
        )]);

        let hops = [HopDescriptor::new("component_ab".to_string(), token_a, token_b)];

        // Override component_ab with a different spot price.
        let overrides = MarketOverrides::empty()
            .with_override("component_ab".to_string(), Box::new(MockProtocolSim::new(7.0)));

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
                "component_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "component_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
        ]);

        let hops = [
            HopDescriptor::new("component_ab".to_string(), token_a, token_b.clone()),
            HopDescriptor::new("component_bc".to_string(), token_b, token_c),
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
    fn test_simulate_path_contains_simulation_panic() {
        // Component math that panics (e.g. U256 division by zero on degenerate amounts) must
        // surface as a SimulationFailed error, not unwind through the solver thread.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "component_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(DivByZeroSim::default()),
        )]);

        let hops = [HopDescriptor::new("component_ab".to_string(), token_a, token_b)];
        let result =
            simulate_path(&hops, &BigUint::from(1000u64), &market, &MarketOverrides::empty());

        match result {
            Err(AlgorithmError::SimulationFailed { component_id, error }) => {
                assert_eq!(component_id, "component_ab");
                assert!(error.contains("panic"), "error should mention the panic: {error}");
            }
            Err(other) => panic!("expected SimulationFailed, got {other:?}"),
            Ok(_) => panic!("expected SimulationFailed, got Ok"),
        }
    }

    #[test]
    fn test_market_overrides_with_zero_gas() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let sim_ab = MockProtocolSim::new(2.0).with_gas(100_000);
        let sim_bc = MockProtocolSim::new(3.0).with_gas(70_000);
        let market = make_market(vec![
            ("component_ab", vec![token_a.clone(), token_b.clone()], Box::new(sim_ab.clone())),
            ("component_bc", vec![token_b.clone(), token_c.clone()], Box::new(sim_bc.clone())),
        ]);

        // Zero gas on component_ab, leave component_bc as a normal override.
        let overrides = MarketOverrides::empty()
            .with_override("component_ab".to_string(), Box::new(sim_ab))
            .with_zero_gas(
                "component_ab".to_string(),
                token_a.address.clone(),
                token_b.address.clone(),
            )
            .with_override("component_bc".to_string(), Box::new(sim_bc));

        let hops_ab =
            [HopDescriptor::new("component_ab".to_string(), token_a.clone(), token_b.clone())];
        let hops_bc = [HopDescriptor::new("component_bc".to_string(), token_b, token_c)];
        let amount_in = BigUint::from(1000u64);

        let hop_gas_sum = |sim: &SimResult| -> BigUint {
            sim.hop_results
                .iter()
                .map(|(_, gas)| gas)
                .sum()
        };

        let normal_ab =
            simulate_path(&hops_ab, &amount_in, &market, &MarketOverrides::empty()).unwrap();
        let zero_gas_ab = simulate_path(&hops_ab, &amount_in, &market, &overrides).unwrap();

        assert_eq!(normal_ab.amount_out, zero_gas_ab.amount_out);
        assert!(hop_gas_sum(&normal_ab) > BigUint::ZERO, "normal gas should be non-zero");
        assert_eq!(
            hop_gas_sum(&zero_gas_ab),
            BigUint::ZERO,
            "zero-gas override should report gas=0"
        );

        // component_bc is a normal override — its gas should be unaffected.
        let result_bc = simulate_path(&hops_bc, &amount_in, &market, &overrides).unwrap();
        assert_eq!(
            hop_gas_sum(&result_bc),
            BigUint::from(70_000u64),
            "non-zero-gas override should keep its gas"
        );
    }

    #[test]
    fn test_evaluate_total_output_two_paths() {
        // 50/50 split of 1000 across two parallel 1-hop paths:
        //
        //       500 -- component_1 (price=2.0) --> 1000
        //      /                                   \
        //  1000                                     2500
        //      \                                   /
        //       500 -- component_2 (price=3.0) --> 1500
        //
        // total_gas = 50k + 60k = 110k
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            (
                "component_1",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(50_000)),
            ),
            (
                "component_2",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(3.0).with_gas(60_000)),
            ),
        ]);

        let hops_1 =
            [HopDescriptor::new("component_1".to_string(), token_a.clone(), token_b.clone())];
        let hops_2 = [HopDescriptor::new("component_2".to_string(), token_a, token_b)];

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
    fn test_evaluate_total_output_shared_component_depletes() {
        // Two "paths" through the SAME constant-product component. Sequential
        // simulation must thread the post-swap state, so the combined output
        // matches one full-amount swap instead of double-counting the fresh
        // reserves for each half.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let cp = ConstantProductSim {
            reserve_0: BigUint::from(10_000u64),
            reserve_1: BigUint::from(10_000u64),
            gas: 50_000,
        };
        let market = make_market(vec![(
            "component",
            vec![token_a.clone(), token_b.clone()],
            Box::new(cp.clone()),
        )]);

        let hops_1 =
            [HopDescriptor::new("component".to_string(), token_a.clone(), token_b.clone())];
        let hops_2 =
            [HopDescriptor::new("component".to_string(), token_a.clone(), token_b.clone())];
        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let total_amount = BigUint::from(1000u64);

        let (total_out, _) = evaluate_total_output(
            &paths,
            &[0.5, 0.5],
            &total_amount,
            &market,
            &MarketOverrides::empty(),
        )
        .unwrap();

        let full_swap_out = cp
            .get_amount_out(total_amount, &token_a, &token_b)
            .unwrap()
            .amount;
        let half_fresh_out = cp
            .get_amount_out(BigUint::from(500u64), &token_a, &token_b)
            .unwrap()
            .amount;

        // Double-counting would report ~2 × half_fresh_out; the honest value
        // equals the full-amount swap up to per-chunk rounding.
        assert!(
            total_out < &half_fresh_out * 2u32,
            "shared component must deplete between paths: {total_out} >= {}",
            &half_fresh_out * 2u32
        );
        let diff = BigInt::from(total_out) - BigInt::from(full_swap_out);
        assert!(
            diff.magnitude() <= &BigUint::from(2u32),
            "sequential split should match one full swap (±rounding), diff {diff}"
        );
    }

    #[test]
    fn test_evaluate_total_output_gas_deduplication() {
        // Two paths share component P1 (pre-split hop). P1's gas should be
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
    fn test_evaluate_total_output_matches_route_order_for_same_component_branches() {
        // Both source branches use the same component with different token
        // pairs. The input path order is B then C, but the route emits C then
        // B because the C branch has the larger split. Since MockProtocolSim
        // increments its spot price after each swap, path-order simulation
        // would produce a different total than route-order simulation.
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let token_d = token(0x0D, "D");
        let market = make_market(vec![
            (
                "tricomponent",
                vec![token_a.clone(), token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(2.0).with_gas(80_000)),
            ),
            (
                "component_bd",
                vec![token_b.clone(), token_d.clone()],
                Box::new(MockProtocolSim::new(1.0)),
            ),
            (
                "component_cd",
                vec![token_c.clone(), token_d.clone()],
                Box::new(MockProtocolSim::new(1.0)),
            ),
        ]);

        let hops_b = [
            HopDescriptor::new("tricomponent".to_string(), token_a.clone(), token_b.clone()),
            HopDescriptor::new("component_bd".to_string(), token_b.clone(), token_d.clone()),
        ];
        let hops_c = [
            HopDescriptor::new("tricomponent".to_string(), token_a.clone(), token_c.clone()),
            HopDescriptor::new("component_cd".to_string(), token_c.clone(), token_d.clone()),
        ];

        let total_amount = BigUint::from(1000u64);
        let paths: Vec<&[HopDescriptor]> = vec![&hops_b, &hops_c];
        let fractions = [0.4, 0.6];
        let (total_out, total_gas) = evaluate_total_output(
            &paths,
            &fractions,
            &total_amount,
            &market,
            &MarketOverrides::empty(),
        )
        .unwrap();

        let zero = BigUint::ZERO;
        let allocations = vec![
            PathAllocation {
                hops: hops_b
                    .iter()
                    .cloned()
                    .map(|hop| hop.with_amounts(zero.clone(), zero.clone()))
                    .collect(),
                flow_fraction: 0.4,
                amount_in: BigUint::from(400u64),
                amount_out: zero.clone(),
                marginal_price_product: 0.0,
            },
            PathAllocation {
                hops: hops_c
                    .iter()
                    .cloned()
                    .map(|hop| hop.with_amounts(zero.clone(), zero.clone()))
                    .collect(),
                flow_fraction: 0.6,
                amount_in: BigUint::from(600u64),
                amount_out: zero,
                marginal_price_product: 0.0,
            },
        ];
        let ord = order(&token_a, &token_d, 1000, OrderSide::Sell);
        let route = build_split_route(&allocations, &market, &ord).unwrap();
        let route_out: BigUint = route
            .swaps()
            .iter()
            .filter(|swap| swap.token_out() == &token_d.address)
            .map(|swap| swap.amount_out().clone())
            .sum();

        assert_eq!(
            route.swaps()[0].token_out(),
            &token_c.address,
            "larger C branch should execute first"
        );
        assert_eq!(total_out, BigUint::from(2400u64));
        assert_eq!(route_out, total_out);
        assert_eq!(route.total_gas().to_u64().unwrap(), total_gas);
    }

    #[test]
    fn test_gas_dedup_different_tokens() {
        // A single 3-token component used for two different token pairs is two
        // distinct hops — gas must be counted for each.
        //
        //  A -- TRICOMPONENT (A→B) --> B    (path 1)
        //  B -- TRICOMPONENT (B→C) --> C    (path 2)
        //
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let market = make_market(vec![(
            "tricomponent",
            vec![token_a.clone(), token_b.clone(), token_c.clone()],
            Box::new(MockProtocolSim::new(1.0).with_gas(80_000)),
        )]);

        let hops_1 = [HopDescriptor::new("tricomponent".to_string(), token_a, token_b.clone())];
        let hops_2 = [HopDescriptor::new("tricomponent".to_string(), token_b, token_c)];

        let paths: Vec<&[HopDescriptor]> = vec![&hops_1, &hops_2];
        let fractions = [0.5, 0.5];
        let total_amount = BigUint::from(1000u64);
        let overrides = MarketOverrides::empty();

        let (_, total_gas) =
            evaluate_total_output(&paths, &fractions, &total_amount, &market, &overrides).unwrap();

        // Different token pairs on the same component: 80k + 80k = 160k
        assert_eq!(total_gas, 160_000);
    }

    #[test]
    fn test_build_post_swap_overrides_degrades_used_components() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "component_ab",
            vec![token_a.clone(), token_b.clone()],
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(10_000u64),
                reserve_1: BigUint::from(20_000u64),
                gas: 50_000,
            }),
        )]);

        let allocation = PathAllocation {
            hops: vec![SimulatedHop {
                descriptor: HopDescriptor::new(
                    "component_ab".to_string(),
                    token_a.clone(),
                    token_b.clone(),
                ),
                amount_out: BigUint::from(1818u64),
                gas: BigUint::from(50_000u64),
            }],
            flow_fraction: 1.0,
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(1818u64),
            marginal_price_product: 2.0,
        };

        let degraded = build_post_swap_overrides(&[allocation], &market).unwrap();

        // xy=k: amount_out = amount_in * reserve_out / (reserve_in + amount_in)
        // Fresh component (10000/20000): 100 * 20000 / (10000 + 100) = 198
        let probe = BigUint::from(100u64);
        let fresh_out = market
            .get_simulation_state("component_ab")
            .unwrap()
            .get_amount_out(probe.clone(), &token_a, &token_b)
            .unwrap()
            .amount;
        assert_eq!(fresh_out, BigUint::from(198u64));

        // The 1000-in allocation produces 1000*20000/(10000+1000) = 1818 out,
        // shifting reserves to (10000+1000, 20000-1818) = (11000, 18182).
        // Degraded component: 100 * 18182 / (11000 + 100) = 163
        let degraded_out = degraded
            .get(&"component_ab".to_string())
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
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P1".to_string(),
                            token_a.clone(),
                            token_b.clone(),
                        ),
                        amount_out: BigUint::from(1200u64),
                        gas: gas.clone(),
                    },
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P2".to_string(),
                            token_b.clone(),
                            token_c.clone(),
                        ),
                        amount_out: BigUint::from(3600u64),
                        gas: gas.clone(),
                    },
                ],
                flow_fraction: 0.6,
                amount_in: BigUint::from(600u64),
                amount_out: BigUint::from(3600u64),
                marginal_price_product: 6.0,
            },
            PathAllocation {
                hops: vec![
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P1".to_string(),
                            token_a.clone(),
                            token_b.clone(),
                        ),
                        amount_out: BigUint::from(800u64),
                        gas: gas.clone(),
                    },
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P3".to_string(),
                            token_b.clone(),
                            token_c.clone(),
                        ),
                        amount_out: BigUint::from(1600u64),
                        gas,
                    },
                ],
                flow_fraction: 0.4,
                amount_in: BigUint::from(400u64),
                amount_out: BigUint::from(1600u64),
                marginal_price_product: 4.0,
            },
        ];

        let hops_by_token = merge_shared_hops(&paths).unwrap();

        // Branch collection at A: both paths cross P1 on the same pair, so it merges to one swap.
        let branch_collection_a = &hops_by_token[&token_a.address];
        assert_eq!(branch_collection_a.len(), 1);
        assert_eq!(branch_collection_a[0].hop.component_id, "P1");

        // Branch collection at B: two swaps, ordered by component id so equal amounts do not
        // reorder between runs. The split each carries is `splits_from_amounts`' to set.
        let branch_collection_b = &hops_by_token[&token_b.address];
        let at_b: Vec<&str> = branch_collection_b
            .iter()
            .map(|swap| swap.hop.component_id.as_str())
            .collect();
        assert_eq!(at_b, vec!["P2", "P3"]);
        assert!(
            branch_collection_b
                .iter()
                .chain(branch_collection_a)
                .all(|swap| swap.split == 0.0),
            "merging assigns no split; the amounts standing at the token decide it"
        );
    }

    #[test]
    fn test_splits_from_amounts_derives_fractions_from_the_amounts() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let branch_collection = vec![
            SplitSwap {
                hop: HopDescriptor::new("component1".to_string(), token_a.clone(), token_b.clone()),
                // Stale, and deliberately inconsistent with the amounts: the fractions are derived
                // from what the execution assigned, not from what a caller once guessed.
                split: 0.1,
                amount_in: BigUint::from(700u64),
            },
            SplitSwap {
                hop: HopDescriptor::new("component2".to_string(), token_a.clone(), token_b.clone()),
                split: 0.9,
                amount_in: BigUint::from(300u64),
            },
        ];

        let result = splits_from_amounts(branch_collection, &BigUint::from(1000u64));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].amount_in, BigUint::from(700u64));
        assert!((result[0].split - 0.7).abs() < 1e-9);
        // The last swap takes whatever is left, which on chain is what `split = 0.0` means.
        assert_eq!(result[1].amount_in, BigUint::from(300u64));
        assert_eq!(result[1].split, 0.0);
    }

    /// The sort is what decides which swap carries the remainder, so it has to be exercised by
    /// input that is not already in the order it produces.
    #[test]
    fn test_splits_from_amounts_sorts_by_amount_before_assigning_the_remainder() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let branch_collection = vec![
            SplitSwap {
                hop: HopDescriptor::new("small".to_string(), token_a.clone(), token_b.clone()),
                split: 0.0,
                amount_in: BigUint::from(300u64),
            },
            SplitSwap {
                hop: HopDescriptor::new("large".to_string(), token_a.clone(), token_b.clone()),
                split: 0.0,
                amount_in: BigUint::from(700u64),
            },
        ];

        let result = splits_from_amounts(branch_collection, &BigUint::from(1000u64));

        let order: Vec<&str> = result
            .iter()
            .map(|swap| swap.hop.component_id.as_str())
            .collect();
        assert_eq!(order, vec!["large", "small"], "the ascending input is sorted largest first");
        assert!((result[0].split - 0.7).abs() < 1e-9);
        // The smallest runs last and takes what is left, which on chain is what `split = 0.0` asks
        // the router for.
        assert_eq!(result[1].split, 0.0);
        assert_eq!(result[1].amount_in, BigUint::from(300u64));
    }

    /// A path that ends on the token it started from must be scored on what it produced, not on
    /// the order's own input still standing at that token.
    #[test]
    fn test_execute_split_plan_does_not_count_the_input_as_output() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            ("there", vec![token_a.clone(), token_b.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("back", vec![token_b.clone(), token_a.clone()], Box::new(MockProtocolSim::new(1.0))),
        ]);

        let hops: Vec<HopDescriptor> = vec![
            HopDescriptor::new("there".to_string(), token_a.clone(), token_b.clone()),
            HopDescriptor::new("back".to_string(), token_b, token_a.clone()),
        ];
        let (total_out, _) = evaluate_total_output(
            &[&hops],
            &[1.0],
            &BigUint::from(1000u64),
            &market,
            &MarketOverrides::empty(),
        )
        .expect("the round trip simulates");

        // 1000 A buys 2000 B, which buys 2000 A back. The 1000 that was spent is gone.
        assert_eq!(total_out, BigUint::from(2000u64), "the order's own input is not output");
    }

    /// A path set carrying nothing describes no split, and guessing an allocation for it would
    /// misprice the quote.
    #[test]
    fn test_execute_split_plan_rejects_paths_that_carry_nothing() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![(
            "component1",
            vec![token_a.clone(), token_b.clone()],
            Box::new(MockProtocolSim::new(2.0)),
        )]);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let paths = vec![PathAllocation {
            hops: vec![SimulatedHop {
                descriptor: HopDescriptor::new("component1".to_string(), token_a, token_b),
                amount_out: BigUint::ZERO,
                gas: BigUint::from(50_000u64),
            }],
            flow_fraction: 0.0,
            amount_in: BigUint::ZERO,
            amount_out: BigUint::ZERO,
            marginal_price_product: 0.0,
        }];

        let error = build_split_route(&paths, &market, &ord)
            .expect_err("a path carrying nothing cannot be divided");

        assert!(
            error
                .to_string()
                .contains("every path carries a zero amount"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_splits_from_amounts_single_hop() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");

        let total = BigUint::from(1000u64);
        let branch_collection = vec![SplitSwap {
            hop: HopDescriptor::new("component1".to_string(), token_a, token_b),
            split: 1.0,
            amount_in: total.clone(),
        }];

        let result = splits_from_amounts(branch_collection, &total);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].split, 0.0);
        assert_eq!(result[0].amount_in, total);
    }

    #[test]
    fn test_share_output_divides_by_what_each_path_put_in() {
        let shares =
            share_output(&BigUint::from(1000u64), &[BigUint::from(300u64), BigUint::from(100u64)]);

        assert_eq!(shares, vec![BigUint::from(750u64), BigUint::from(250u64)]);
    }

    /// The shares add back to the output exactly, whatever the division leaves over.
    #[test]
    fn test_share_output_gives_the_remainder_to_the_last_path() {
        let output = BigUint::from(1000u64);
        let shares =
            share_output(&output, &[BigUint::from(1u64), BigUint::from(1u64), BigUint::from(1u64)]);

        assert_eq!(shares.iter().sum::<BigUint>(), output);
        assert_eq!(shares[2], BigUint::from(334u64));
    }

    // ==================== build_split_route Tests ====================

    #[test]
    fn test_build_split_route_remainder_convention() {
        // 3 paths splitting at source: last swap at the split point must
        // have split=0.0.
        //
        //       500 -- component1 (price=2) --> 1000
        //      /
        //  1000---- 300 -- component2 (price=3) -->  900
        //      \
        //       200 -- component3 (price=4) -->  800
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let market = make_market(vec![
            (
                "component1",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "component2",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
            (
                "component3",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(4.0)),
            ),
        ]);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![
            PathAllocation {
                hops: vec![SimulatedHop {
                    descriptor: HopDescriptor::new(
                        "component1".to_string(),
                        token_a.clone(),
                        token_b.clone(),
                    ),
                    amount_out: BigUint::from(1000u64),
                    gas: gas.clone(),
                }],
                flow_fraction: 0.5,
                amount_in: BigUint::from(500u64),
                amount_out: BigUint::from(1000u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: vec![SimulatedHop {
                    descriptor: HopDescriptor::new(
                        "component2".to_string(),
                        token_a.clone(),
                        token_b.clone(),
                    ),
                    amount_out: BigUint::from(900u64),
                    gas: gas.clone(),
                }],
                flow_fraction: 0.3,
                amount_in: BigUint::from(300u64),
                amount_out: BigUint::from(900u64),
                marginal_price_product: 3.0,
            },
            PathAllocation {
                hops: vec![SimulatedHop {
                    descriptor: HopDescriptor::new(
                        "component3".to_string(),
                        token_a.clone(),
                        token_b.clone(),
                    ),
                    amount_out: BigUint::from(800u64),
                    gas,
                }],
                flow_fraction: 0.2,
                amount_in: BigUint::from(200u64),
                amount_out: BigUint::from(800u64),
                marginal_price_product: 4.0,
            },
        ];

        let route = build_split_route(&paths, &market, &ord).unwrap();
        let swaps = route.swaps();

        assert_eq!(swaps.len(), 3);

        // Sorted descending: component1 (0.5), component2 (0.3), component3 (0.2).
        assert_eq!(swaps[0].component_id(), "component1");
        assert_eq!(*swaps[0].split(), 0.5);
        assert_eq!(swaps[1].component_id(), "component2");
        assert_eq!(*swaps[1].split(), 0.3);
        assert_eq!(swaps[2].component_id(), "component3");
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
                "component_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "component_bc",
                vec![token_b.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
        ]);
        let ord = order(&token_a, &token_c, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![PathAllocation {
            hops: vec![
                SimulatedHop {
                    descriptor: HopDescriptor::new(
                        "component_ab".to_string(),
                        token_a.clone(),
                        token_b.clone(),
                    ),
                    amount_out: BigUint::from(2000u64),
                    gas: gas.clone(),
                },
                SimulatedHop {
                    descriptor: HopDescriptor::new("component_bc".to_string(), token_b, token_c),
                    amount_out: BigUint::from(6000u64),
                    gas,
                },
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
    fn test_build_split_route_shared_first_component() {
        // Two paths sharing component P1 at A→B, diverging at B→C (P2 vs P3).
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
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P1".to_string(),
                            token_a.clone(),
                            token_b.clone(),
                        ),
                        amount_out: BigUint::from(1400u64),
                        gas: gas.clone(),
                    },
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P2".to_string(),
                            token_b.clone(),
                            token_c.clone(),
                        ),
                        amount_out: BigUint::from(4200u64),
                        gas: gas.clone(),
                    },
                ],
                flow_fraction: 0.7,
                amount_in: BigUint::from(700u64),
                amount_out: BigUint::from(4200u64),
                marginal_price_product: 6.0,
            },
            PathAllocation {
                hops: vec![
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P1".to_string(),
                            token_a.clone(),
                            token_b.clone(),
                        ),
                        amount_out: BigUint::from(600u64),
                        gas: gas.clone(),
                    },
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "P3".to_string(),
                            token_b.clone(),
                            token_c.clone(),
                        ),
                        amount_out: BigUint::from(2400u64),
                        gas,
                    },
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
        //       component_ab --> B -- component_bz
        //      /                         \
        //  A --                           Z
        //      \                         /
        //       component_ac --> C -- component_cz
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        let token_z = token(0x1A, "Z");
        let market = make_market(vec![
            (
                "component_ab",
                vec![token_a.clone(), token_b.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "component_ac",
                vec![token_a.clone(), token_c.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
            (
                "component_bz",
                vec![token_b.clone(), token_z.clone()],
                Box::new(MockProtocolSim::new(4.0)),
            ),
            (
                "component_cz",
                vec![token_c.clone(), token_z.clone()],
                Box::new(MockProtocolSim::new(5.0)),
            ),
        ]);
        let ord = order(&token_a, &token_z, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);
        let paths = vec![
            PathAllocation {
                hops: vec![
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "component_ab".to_string(),
                            token_a.clone(),
                            token_b.clone(),
                        ),
                        amount_out: BigUint::from(1200u64),
                        gas: gas.clone(),
                    },
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "component_bz".to_string(),
                            token_b,
                            token_z.clone(),
                        ),
                        amount_out: BigUint::from(4800u64),
                        gas: gas.clone(),
                    },
                ],
                flow_fraction: 0.6,
                amount_in: BigUint::from(600u64),
                amount_out: BigUint::from(4800u64),
                marginal_price_product: 8.0,
            },
            PathAllocation {
                hops: vec![
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "component_ac".to_string(),
                            token_a.clone(),
                            token_c.clone(),
                        ),
                        amount_out: BigUint::from(1200u64),
                        gas: gas.clone(),
                    },
                    SimulatedHop {
                        descriptor: HopDescriptor::new(
                            "component_cz".to_string(),
                            token_c,
                            token_z,
                        ),
                        amount_out: BigUint::from(6000u64),
                        gas,
                    },
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

        // Source-level split: component_ab (0.6) first, component_ac (0.4) last.
        assert_eq!(swaps[0].component_id(), "component_ab");
        assert_eq!(*swaps[0].split(), 0.6);
        assert_eq!(*swaps[0].amount_in(), BigUint::from(600u64));
        assert_eq!(*swaps[0].amount_out(), BigUint::from(1200u64));

        assert_eq!(swaps[1].component_id(), "component_ac");
        assert_eq!(*swaps[1].split(), 0.0);
        assert_eq!(*swaps[1].amount_in(), BigUint::from(400u64));
        assert_eq!(*swaps[1].amount_out(), BigUint::from(1200u64));

        // Intermediate swaps: single hops from B and C, all split=0.0.
        assert_eq!(swaps[2].component_id(), "component_bz");
        assert_eq!(*swaps[2].split(), 0.0);
        assert_eq!(*swaps[2].amount_in(), BigUint::from(1200u64));
        assert_eq!(*swaps[2].amount_out(), BigUint::from(4800u64));

        assert_eq!(swaps[3].component_id(), "component_cz");
        assert_eq!(*swaps[3].split(), 0.0);
        assert_eq!(*swaps[3].amount_in(), BigUint::from(1200u64));
        assert_eq!(*swaps[3].amount_out(), BigUint::from(6000u64));
    }

    #[test]
    fn test_build_split_route_cross_depth_shared_component() {
        // Two paths of different lengths share Component A (USDC→DAI).
        // The BFS must process all USDC inflows before visiting USDC's
        // outgoing swaps.
        //
        //  WETH ──┬────────────────────▶ USDC ─── component_a ──▶ DAI
        //         │                      ▲
        //         └──────────▶ USDT ─────┘
        //
        // Path 1 (2 hops): WETH → USDC → DAI      (0.6 fraction)
        // Path 2 (3 hops): WETH → USDT → USDC → DAI (0.4 fraction)
        //
        // Component A appears in both paths with (USDC, DAI). After merging,
        // Component A's amount_in must reflect USDC from *both* paths.
        let weth = token(0x01, "WETH");
        let usdc = token(0x02, "USDC");
        let usdt = token(0x03, "USDT");
        let dai = token(0x04, "DAI");
        let market = make_market(vec![
            (
                "component_weth_usdc",
                vec![weth.clone(), usdc.clone()],
                Box::new(MockProtocolSim::new(2.0)),
            ),
            (
                "component_weth_usdt",
                vec![weth.clone(), usdt.clone()],
                Box::new(MockProtocolSim::new(3.0)),
            ),
            (
                "component_usdt_usdc",
                vec![usdt.clone(), usdc.clone()],
                Box::new(MockProtocolSim::new(1.0)),
            ),
            ("component_a", vec![usdc.clone(), dai.clone()], Box::new(MockProtocolSim::new(1.0))),
        ]);
        let ord = order(&weth, &dai, 1000, OrderSide::Sell);

        let gas = BigUint::from(50_000u64);

        // Path 1: WETH --(component_weth_usdc)--> USDC --(component_a)--> DAI
        // 600 WETH in, 1200 USDC out from first hop, 1200 DAI out from component_a
        let path1 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_weth_usdc".to_string(), weth.clone(), usdc.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
            ],
            flow_fraction: 0.6,
            amount_in: BigUint::from(600u64),
            amount_out: BigUint::from(1200u64),
            marginal_price_product: 2.0,
        };

        // Path 2: WETH --(component_weth_usdt)--> USDT --(component_usdt_usdc)--> USDC
        //         --(component_a)--> DAI
        // 400 WETH in, 1200 USDT out, 1200 USDC out, 1200 DAI out from component_a
        let path2 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_weth_usdt".to_string(), weth.clone(), usdt.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_usdt_usdc".to_string(), usdt.clone(), usdc.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(1200u64), gas),
            ],
            flow_fraction: 0.4,
            amount_in: BigUint::from(400u64),
            amount_out: BigUint::from(1200u64),
            marginal_price_product: 3.0,
        };

        let route = build_split_route(&[path1, path2], &market, &ord).unwrap();
        let swaps = route.swaps();

        // Component A is shared and merged: it should receive the total USDC
        // from both paths (1200 + 1200 = 2400).
        let component_a_swap = swaps
            .iter()
            .find(|s| s.component_id() == "component_a")
            .expect("component_a swap must exist");
        assert_eq!(
            *component_a_swap.amount_in(),
            BigUint::from(2400u64),
            "component_a must receive USDC from both paths (1200 + 1200)"
        );
        assert_eq!(
            *component_a_swap.amount_out(),
            BigUint::from(2400u64),
            "component_a amount_out should be the merged total"
        );

        // Component A is merged into one swap, so its gas is counted once.
        // Total = 4 distinct components × 50k gas = 200k (not 5 × 50k).
        assert_eq!(swaps.len(), 4, "component_a must appear once, not once per path");
        assert_eq!(
            route.total_gas(),
            BigUint::from(200_000u64),
            "gas must be counted once per component, not once per path"
        );
    }

    #[test]
    fn test_build_split_route_cross_depth_convergence_with_downstream_split() {
        // Cross-depth convergence on Component A (USDC→DAI) followed by a
        // downstream split at DAI (Component B and Component C → PEPE).
        //
        //  WETH ──┬──────────────▶ USDC ── component_a ──▶ DAI ──┬── component_b ──▶ PEPE
        //         │                  ▲                      │
        //         └──────▶ USDT ─────┘                      └── component_c ──▶ PEPE
        //
        // Path 1: WETH → USDC → DAI → PEPE (Component B)    fraction 0.3
        // Path 2: WETH → USDC → DAI → PEPE (Component C)    fraction 0.3
        // Path 3: WETH → USDT → USDC → DAI → PEPE (Component B) fraction 0.4
        //
        // Component A is shared across all 3 paths. The DAI split between Component B
        // and Component C must wait until all DAI has been produced (from both
        // the direct and USDT-detour paths through the merged Component A swap).
        let weth = token(0x01, "WETH");
        let usdc = token(0x02, "USDC");
        let usdt = token(0x03, "USDT");
        let dai = token(0x04, "DAI");
        let pepe = token(0x05, "PEPE");
        let market = make_market(vec![
            ("component_wu", vec![weth.clone(), usdc.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("component_wt", vec![weth.clone(), usdt.clone()], Box::new(MockProtocolSim::new(3.0))),
            ("component_tu", vec![usdt.clone(), usdc.clone()], Box::new(MockProtocolSim::new(1.0))),
            ("component_a", vec![usdc.clone(), dai.clone()], Box::new(MockProtocolSim::new(1.0))),
            ("component_b", vec![dai.clone(), pepe.clone()], Box::new(MockProtocolSim::new(5.0))),
            ("component_c", vec![dai.clone(), pepe.clone()], Box::new(MockProtocolSim::new(4.0))),
        ]);
        let ord = order(&weth, &pepe, 1000, OrderSide::Sell);
        let gas = BigUint::from(50_000u64);

        // Path 1: WETH → USDC → DAI → PEPE (Component B)
        let path1 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_wu".to_string(), weth.clone(), usdc.clone())
                    .with_amounts(BigUint::from(600u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(600u64), gas.clone()),
                HopDescriptor::new("component_b".to_string(), dai.clone(), pepe.clone())
                    .with_amounts(BigUint::from(3000u64), gas.clone()),
            ],
            flow_fraction: 0.3,
            amount_in: BigUint::from(300u64),
            amount_out: BigUint::from(3000u64),
            marginal_price_product: 10.0,
        };

        // Path 2: WETH → USDC → DAI → PEPE (Component C)
        let path2 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_wu".to_string(), weth.clone(), usdc.clone())
                    .with_amounts(BigUint::from(600u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(600u64), gas.clone()),
                HopDescriptor::new("component_c".to_string(), dai.clone(), pepe.clone())
                    .with_amounts(BigUint::from(2400u64), gas.clone()),
            ],
            flow_fraction: 0.3,
            amount_in: BigUint::from(300u64),
            amount_out: BigUint::from(2400u64),
            marginal_price_product: 8.0,
        };

        // Path 3: WETH → USDT → USDC → DAI → PEPE (Component B)
        let path3 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_wt".to_string(), weth.clone(), usdt.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_tu".to_string(), usdt.clone(), usdc.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_b".to_string(), dai.clone(), pepe.clone())
                    .with_amounts(BigUint::from(6000u64), gas),
            ],
            flow_fraction: 0.4,
            amount_in: BigUint::from(400u64),
            amount_out: BigUint::from(6000u64),
            marginal_price_product: 15.0,
        };

        let route = build_split_route(&[path1, path2, path3], &market, &ord).unwrap();
        let swaps = route.swaps();

        // Component A is merged: total USDC in = 600+600+1200 = 2400,
        // total DAI out = 600+600+1200 = 2400.
        let component_a_swap = swaps
            .iter()
            .find(|s| s.component_id() == "component_a")
            .expect("component_a swap must exist");
        assert_eq!(
            *component_a_swap.amount_in(),
            BigUint::from(2400u64),
            "component_a must receive all USDC from both direct and USDT-detour paths"
        );

        // The DAI split follows what each path *brought to DAI*, not what share of the order it
        // started with. Paths 1 and 3 feed component_b and path 2 feeds component_c, and the DAI
        // they arrive with is 600 / 600 / 1200 — because path 3 reached USDC through a better pair
        // of hops (WETH→USDT at 3.0 then USDT→USDC at 1.0) than paths 1 and 2 (WETH→USDC at 2.0).
        // So component_b takes 1800 of the 2400 DAI and component_c takes 600.
        let component_b_swap = swaps
            .iter()
            .find(|s| s.component_id() == "component_b")
            .expect("component_b swap must exist");
        let component_c_swap = swaps
            .iter()
            .find(|s| s.component_id() == "component_c")
            .expect("component_c swap must exist");

        assert_eq!(*component_b_swap.amount_in(), BigUint::from(1800u64));
        assert_eq!(
            *component_b_swap.amount_out(),
            BigUint::from(9000u64),
            "component_b amount_out should be simulated from emitted amount_in"
        );
        assert_eq!(*component_c_swap.amount_in(), BigUint::from(600u64));
        assert_eq!(
            *component_c_swap.amount_out(),
            BigUint::from(2400u64),
            "component_c amount_out should be simulated from remainder amount_in"
        );

        // Verify ordering: component_a must appear before component_b and component_c
        // (DAI must be fully produced before splitting).
        let component_a_idx = swaps
            .iter()
            .position(|s| s.component_id() == "component_a")
            .unwrap();
        let component_b_idx = swaps
            .iter()
            .position(|s| s.component_id() == "component_b")
            .unwrap();
        let component_c_idx = swaps
            .iter()
            .position(|s| s.component_id() == "component_c")
            .unwrap();
        assert!(
            component_a_idx < component_b_idx && component_a_idx < component_c_idx,
            "component_a (idx {component_a_idx}) must appear before component_b (idx {component_b_idx}) \
             and component_c (idx {component_c_idx})"
        );

        // Also verify USDT→USDC appears before component_a (USDC→DAI).
        let component_tu_idx = swaps
            .iter()
            .position(|s| s.component_id() == "component_tu")
            .unwrap();
        assert!(
            component_tu_idx < component_a_idx,
            "component_tu (idx {component_tu_idx}) must appear before component_a (idx {component_a_idx})"
        );

        // Component A is merged into one swap, so its gas is counted once.
        // Total = 6 distinct components × 50k gas = 300k (not 8 × 50k).
        assert_eq!(swaps.len(), 6, "component_a must appear once, not once per path");
        assert_eq!(
            route.total_gas(),
            BigUint::from(300_000u64),
            "gas must be counted once per component, not once per path"
        );
    }

    #[test]
    fn test_build_split_route_rejects_reverse_order_shared_components() {
        // Two paths use Component A and Component B in opposite order:
        //
        //         ┌── USDC ── component_a ──▶ DAI ── PEPE ── component_b ──▶ UNI ── WBTC
        //  WETH ──┤
        //         └── PEPE ── component_b ──▶ UNI ── USDC ── component_a ──▶ DAI ── WBTC
        //
        // merge_shared_hops collapses Component A and Component B into single swaps,
        // creating the cycle: USDC → DAI → PEPE → UNI → USDC.
        let weth = token(0x01, "WETH");
        let usdc = token(0x02, "USDC");
        let dai = token(0x03, "DAI");
        let pepe = token(0x04, "PEPE");
        let uni = token(0x05, "UNI");
        let wbtc = token(0x06, "WBTC");
        let market = make_market(vec![
            ("component_wu", vec![weth.clone(), usdc.clone()], Box::new(MockProtocolSim::new(2.0))),
            ("component_a", vec![usdc.clone(), dai.clone()], Box::new(MockProtocolSim::new(1.0))),
            ("component_dp", vec![dai.clone(), pepe.clone()], Box::new(MockProtocolSim::new(5.0))),
            ("component_b", vec![pepe.clone(), uni.clone()], Box::new(MockProtocolSim::new(1.0))),
            ("component_uw", vec![uni.clone(), wbtc.clone()], Box::new(MockProtocolSim::new(3.0))),
            ("component_wp", vec![weth.clone(), pepe.clone()], Box::new(MockProtocolSim::new(4.0))),
            ("component_us", vec![uni.clone(), usdc.clone()], Box::new(MockProtocolSim::new(1.0))),
            ("component_dw", vec![dai.clone(), wbtc.clone()], Box::new(MockProtocolSim::new(2.0))),
        ]);
        let ord = order(&weth, &wbtc, 1000, OrderSide::Sell);
        let gas = BigUint::from(50_000u64);

        let path1 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_wu".to_string(), weth.clone(), usdc.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(1200u64), gas.clone()),
                HopDescriptor::new("component_dp".to_string(), dai.clone(), pepe.clone())
                    .with_amounts(BigUint::from(6000u64), gas.clone()),
                HopDescriptor::new("component_b".to_string(), pepe.clone(), uni.clone())
                    .with_amounts(BigUint::from(6000u64), gas.clone()),
                HopDescriptor::new("component_uw".to_string(), uni.clone(), wbtc.clone())
                    .with_amounts(BigUint::from(18000u64), gas.clone()),
            ],
            flow_fraction: 0.6,
            amount_in: BigUint::from(600u64),
            amount_out: BigUint::from(18000u64),
            marginal_price_product: 30.0,
        };

        let path2 = PathAllocation {
            hops: vec![
                HopDescriptor::new("component_wp".to_string(), weth.clone(), pepe.clone())
                    .with_amounts(BigUint::from(1600u64), gas.clone()),
                HopDescriptor::new("component_b".to_string(), pepe.clone(), uni.clone())
                    .with_amounts(BigUint::from(1600u64), gas.clone()),
                HopDescriptor::new("component_us".to_string(), uni.clone(), usdc.clone())
                    .with_amounts(BigUint::from(1600u64), gas.clone()),
                HopDescriptor::new("component_a".to_string(), usdc.clone(), dai.clone())
                    .with_amounts(BigUint::from(1600u64), gas.clone()),
                HopDescriptor::new("component_dw".to_string(), dai.clone(), wbtc.clone())
                    .with_amounts(BigUint::from(3200u64), gas),
            ],
            flow_fraction: 0.4,
            amount_in: BigUint::from(400u64),
            amount_out: BigUint::from(3200u64),
            marginal_price_product: 8.0,
        };

        // merge_shared_hops collapses Component A and Component B into single entries.
        let merged = merge_shared_hops(&[path1.clone(), path2.clone()]).unwrap();
        assert_eq!(
            merged[&usdc.address]
                .iter()
                .filter(|s| s.hop.component_id == "component_a")
                .count(),
            1,
            "merge_shared_hops merges component_a into one"
        );
        assert_eq!(
            merged[&pepe.address]
                .iter()
                .filter(|s| s.hop.component_id == "component_b")
                .count(),
            1,
            "merge_shared_hops merges component_b into one"
        );

        // build_split_route rejects the combination.
        let err = build_split_route(&[path1, path2], &market, &ord)
            .expect_err("must reject cyclic path combination");
        assert!(
            matches!(&err, AlgorithmError::Other(msg) if msg.contains("dependency cycle")),
            "expected AlgorithmError::Other with dependency cycle, got: {err}"
        );
    }
}
