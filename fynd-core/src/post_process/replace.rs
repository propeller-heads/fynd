//! The search that the replacing post-processors share.
//!
//! A search takes a route and a set of swaps with zero amounts, one for each market component it
//! may use. It tries each swap in place of a target of the route and keeps the replacement that
//! raises the net amount out most.

use std::{collections::BTreeSet, time::Instant};

use num_bigint::{BigInt, BigUint};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, trace, warn};
use tycho_simulation::tycho_common::models::{token::Token, Address};

use crate::{
    feed::market_data::MarketDataView,
    post_process::PostProcessRequest,
    replay::{simulate_swaps, ReplayError, SimulatedRoute, SimulationEnds},
    types::{ComponentId, Route, RouteResult, Swap},
};

/// The RFQ components of the market, by token.
#[derive(Default)]
pub(super) struct RfqIndex {
    by_token: FxHashMap<Address, BTreeSet<ComponentId>>,
    tokens_of: FxHashMap<ComponentId, Vec<Address>>,
}

impl RfqIndex {
    pub(super) fn is_empty(&self) -> bool {
        self.tokens_of.is_empty()
    }

    pub(super) fn insert(&mut self, id: ComponentId, tokens: Vec<Address>) {
        for token in &tokens {
            self.by_token
                .entry(token.clone())
                .or_default()
                .insert(id.clone());
        }
        self.tokens_of.insert(id, tokens);
    }

    pub(super) fn remove(&mut self, id: &str) {
        let Some(tokens) = self.tokens_of.remove(id) else {
            return;
        };
        for token in tokens {
            if let Some(ids) = self.by_token.get_mut(&token) {
                ids.remove(id);
                if ids.is_empty() {
                    self.by_token.remove(&token);
                }
            }
        }
    }

    /// The components that hold at least two of `tokens`, in id order.
    fn find_touching(&self, tokens: &FxHashSet<&Address>) -> BTreeSet<&ComponentId> {
        let mut counts: FxHashMap<&ComponentId, usize> = FxHashMap::default();
        for token in tokens {
            for id in self
                .by_token
                .get(*token)
                .into_iter()
                .flatten()
            {
                *counts.entry(id).or_default() += 1;
            }
        }
        let mut touching = BTreeSet::new();
        for (id, count) in counts {
            if count >= 2 {
                touching.insert(id);
            }
        }
        touching
    }
}

/// What a search reads from the market, copied under the read lock so the search holds no lock.
pub(super) struct Snapshot {
    /// One swap with zero amounts for each RFQ component the search may use, in id order. Its
    /// token pair is a placeholder: `Search` sets the pair and the split before it simulates.
    rfq: Vec<Swap>,
    /// The tokens of the route.
    tokens: FxHashMap<Address, Token>,
}

impl Snapshot {
    /// Copies the tokens of `route` and the RFQ components containing at least two of those
    /// tokens. Omits components the request excludes or the pool drops.
    pub(super) fn take(
        index: &RfqIndex,
        view: &MarketDataView<'_>,
        request: &PostProcessRequest<'_>,
        route: &Route,
    ) -> Self {
        let mut tokens = route.tokens().clone();
        let mut route_tokens = FxHashSet::default();
        for swap in route.swaps() {
            for address in [swap.token_in(), swap.token_out()] {
                route_tokens.insert(address);
                if let Some(token) = view.get_token(address) {
                    tokens
                        .entry(address.clone())
                        .or_insert_with(|| token.clone());
                }
            }
        }
        let mut rfq = Vec::new();
        for id in index.find_touching(&route_tokens) {
            if request.exclusions().excludes_pool(id) {
                continue;
            }
            let (Some(component), Some(state)) =
                (view.get_component(id), view.get_simulation_state(id))
            else {
                continue;
            };
            let [token_in, token_out, ..] = component.tokens.as_slice() else {
                continue;
            };
            if request.drops_component(component) {
                continue;
            }
            rfq.push(Swap::new(
                id.clone(),
                component.protocol_system.clone(),
                token_in.clone(),
                token_out.clone(),
                BigUint::ZERO,
                BigUint::ZERO,
                BigUint::ZERO,
                component.clone(),
                state.clone_box(),
            ));
        }
        Self { rfq, tokens }
    }
}

/// The gas cost of the original route in the output token, and the gas it used.
pub(super) struct GasPricing {
    gas_cost_out: BigInt,
    gas: BigInt,
}

impl GasPricing {
    /// Prices the gas of `result` in `output_token`. `None` when the route uses no gas, so the
    /// cost has no price per unit.
    fn of(result: &RouteResult, output_token: &Address) -> Option<Self> {
        let gas = BigInt::from(result.route().total_gas());
        if gas == BigInt::ZERO {
            return None;
        }
        let amount_out = BigInt::from(result.route().amount_out(output_token));
        let gas_cost_out = (amount_out - result.net_amount_out()).max(BigInt::ZERO);
        Some(Self { gas_cost_out, gas })
    }

    /// Returns the net amount out of `simulated`, scaling the original gas cost by the ratio of
    /// simulated gas to original gas.
    fn net_amount_out(&self, simulated: &SimulatedRoute) -> BigInt {
        BigInt::from(simulated.amount_out.clone()) -
            &self.gas_cost_out * BigInt::from(simulated.gas.clone()) / &self.gas
    }
}

/// The swaps to replace, and the pair of tokens the replacement swap connects.
#[derive(Clone)]
pub(super) struct Target {
    /// Positions of the swaps to replace, in ascending order.
    pub(super) replaced: Vec<usize>,
    pub(super) token_in: Address,
    pub(super) token_out: Address,
    /// The split of the RFQ swap; `0.0` takes what remains of `token_in`.
    split: f64,
}

/// A target with the RFQ swap that replaces it.
pub(super) struct Candidate {
    target: Target,
    rfq: Swap,
    simulated: SimulatedRoute,
    net_amount_out: BigInt,
}

/// Searches one route for RFQ swaps that raise the net amount out.
pub(super) struct Search<'a> {
    original: &'a RouteResult,
    rfq: Vec<Swap>,
    deadline: Instant,
    pricing: GasPricing,
    tokens: FxHashMap<Address, Token>,
    input_token: Address,
    output_token: Address,
    total_in: BigUint,
}

impl<'a> Search<'a> {
    /// Prepares a search of `original` on `snapshot`. `None` when the route cannot be searched:
    /// it has no swaps, it starts and ends on one token, or it uses no gas.
    pub(super) fn new(
        snapshot: Snapshot,
        request: &PostProcessRequest<'_>,
        original: &'a RouteResult,
    ) -> Option<Self> {
        let route = original.route();
        let (input_token, output_token) = (route.input_token()?, route.output_token()?);
        if input_token == output_token {
            return None;
        }
        let pricing = GasPricing::of(original, &output_token)?;
        Some(Self {
            original,
            rfq: snapshot.rfq,
            deadline: request.deadline(),
            pricing,
            tokens: snapshot.tokens,
            input_token,
            output_token,
            total_in: request.order().amount().clone(),
        })
    }

    /// Applies the best replacement until none raises the net amount out, the search has run one
    /// round per original swap, or the deadline passes. A candidate must beat the net amount out
    /// of the current route; the first round simulates the original route again. `None` when the
    /// route did not change, or when the final net amount out is not above the original one.
    pub(super) fn run(mut self) -> Option<RouteResult> {
        let mut swaps = self.original.route().swaps().to_vec();
        let rounds = swaps.len();
        let mut net_amount_out = self.simulate_baseline(&swaps)?;
        let mut changed = false;
        for _ in 0..rounds {
            if self.deadline_passed() {
                break;
            }
            let Some(candidate) = self.best_candidate(&swaps, &net_amount_out) else {
                break;
            };
            net_amount_out = candidate.net_amount_out.clone();
            swaps = self.apply(&swaps, candidate);
            changed = true;
        }
        if !changed || net_amount_out <= *self.original.net_amount_out() {
            return None;
        }
        let route = Route::new(swaps, self.tokens)
            .inspect_err(|error| {
                warn!(%error, "cannot build the replacement route; keeping the original route");
            })
            .ok()?;
        Some(RouteResult::new(route, net_amount_out, self.original.gas_price().clone()))
    }

    /// The net amount out of `swaps`, simulated again with the pricing of the original route.
    /// `None` when a swap simulation fails.
    fn simulate_baseline(&self, swaps: &[Swap]) -> Option<BigInt> {
        let ordered: Vec<&Swap> = swaps.iter().collect();
        let simulated = self
            .simulate(&ordered)
            .inspect_err(|error| debug!(%error, "cannot simulate the original route"))
            .ok()?;
        Some(self.pricing.net_amount_out(&simulated))
    }

    fn deadline_passed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// The candidate with the highest net amount out, if it beats `current_net`. Stops at the
    /// deadline with the best so far.
    fn best_candidate(&mut self, swaps: &[Swap], current_net: &BigInt) -> Option<Candidate> {
        let mut best: Option<Candidate> = None;
        let mut best_net = current_net.clone();
        for target in build_targets(swaps) {
            for position in 0..self.rfq.len() {
                if !super::rfq::fits(&self.rfq[position], &target, swaps, &self.input_token) {
                    continue;
                }
                if self.deadline_passed() {
                    return best;
                }
                self.rfq[position].retarget(
                    target.token_in.clone(),
                    target.token_out.clone(),
                    target.split,
                );
                let rfq = &self.rfq[position];
                let Some((simulated, net_amount_out)) = self.evaluate(swaps, &target, rfq) else {
                    continue;
                };
                if net_amount_out > best_net {
                    best_net = net_amount_out.clone();
                    best = Some(Candidate {
                        target: target.clone(),
                        rfq: rfq.clone(),
                        simulated,
                        net_amount_out,
                    });
                }
            }
        }
        best
    }

    /// Simulates the route with `rfq` in place of the target swaps.
    /// `None` when a swap simulation fails.
    fn evaluate(
        &self,
        swaps: &[Swap],
        target: &Target,
        rfq: &Swap,
    ) -> Option<(SimulatedRoute, BigInt)> {
        let ordered = splice(swaps, &target.replaced, rfq);
        let simulated = self
            .simulate(&ordered)
            .inspect_err(|error| {
                trace!(
                    component = rfq.component_id(),
                    %error,
                    "dropped RFQ candidate: route simulation failed"
                );
            })
            .ok()?;
        let net_amount_out = self.pricing.net_amount_out(&simulated);
        Some((simulated, net_amount_out))
    }

    /// Simulates the swaps in `ordered` in topological order, using the snapshot's tokens and each
    /// swap's state.
    fn simulate<'s>(&'s self, ordered: &[&'s Swap]) -> Result<SimulatedRoute, ReplayError> {
        let ends = SimulationEnds {
            input_token: &self.input_token,
            output_token: &self.output_token,
            total_in: &self.total_in,
        };
        simulate_swaps(
            ordered,
            &ends,
            |address| self.tokens.get(address),
            |swap| Some(swap.protocol_state()),
        )
    }

    /// The swaps after the replacement, with the amounts and gas of the simulation.
    fn apply(&self, swaps: &[Swap], candidate: Candidate) -> Vec<Swap> {
        let Candidate { target, rfq, simulated, .. } = candidate;
        let ordered = splice(swaps, &target.replaced, &rfq);
        let mut replaced = Vec::with_capacity(ordered.len());
        for (swap, simulated) in ordered.into_iter().zip(simulated.swaps) {
            replaced.push(
                Swap::new(
                    swap.component_id().to_string(),
                    swap.protocol().to_string(),
                    swap.token_in().clone(),
                    swap.token_out().clone(),
                    simulated.amount_in,
                    simulated.amount_out,
                    simulated.gas,
                    swap.protocol_component().clone(),
                    swap.protocol_state().clone_box(),
                )
                .with_split(*swap.split()),
            );
        }
        replaced
    }
}

/// Returns `swaps` with the positions in `replaced` removed and `rfq` inserted at the first
/// removed position. The result keeps a topological order: `rfq` consumes only tokens that the
/// swaps before it make.
pub(super) fn splice<'a>(swaps: &'a [Swap], replaced: &[usize], rfq: &'a Swap) -> Vec<&'a Swap> {
    let mut ordered = Vec::with_capacity(swaps.len());
    for (position, swap) in swaps.iter().enumerate() {
        if replaced.first() == Some(&position) {
            ordered.push(rfq);
        }
        if !replaced.contains(&position) {
            ordered.push(swap);
        }
    }
    ordered
}

/// Returns replacement targets for individual swaps, followed by targets for segments.
pub(super) fn build_targets(swaps: &[Swap]) -> Vec<Target> {
    let mut targets = Vec::new();
    for (position, swap) in swaps.iter().enumerate() {
        targets.push(Target {
            replaced: vec![position],
            token_in: swap.token_in().clone(),
            token_out: swap.token_out().clone(),
            split: *swap.split(),
        });
    }
    targets.extend(build_segment_targets(swaps));
    targets
}

/// One target for each pair of cut tokens with more than one swap in the segment between them.
/// The pair of the order's input and output token covers the whole route.
pub(super) fn build_segment_targets(swaps: &[Swap]) -> Vec<Target> {
    let cuts = find_cut_tokens(swaps);
    let forward: Vec<FxHashSet<Address>> = cuts
        .iter()
        .map(|cut| find_reachable(swaps, cut, Direction::Forward))
        .collect();
    let backward: Vec<FxHashSet<Address>> = cuts
        .iter()
        .map(|cut| find_reachable(swaps, cut, Direction::Backward))
        .collect();
    let mut targets = Vec::new();
    for (i, token_in) in cuts.iter().enumerate() {
        for (j, token_out) in cuts.iter().enumerate().skip(i + 1) {
            let replaced: Vec<usize> = swaps
                .iter()
                .enumerate()
                .filter(|(_, swap)| {
                    forward[i].contains(swap.token_in()) && backward[j].contains(swap.token_out())
                })
                .map(|(position, _)| position)
                .collect();
            if replaced.len() > 1 {
                targets.push(Target {
                    replaced,
                    token_in: token_in.clone(),
                    token_out: token_out.clone(),
                    split: 0.0,
                });
            }
        }
    }
    targets
}

/// The tokens that every path from the first input token to the last output token passes
/// through, in route order. The input and output token come first and last.
pub(super) fn find_cut_tokens(swaps: &[Swap]) -> Vec<Address> {
    let (Some(first), Some(last)) = (swaps.first(), swaps.last()) else {
        return Vec::new();
    };
    let (input, output) = (first.token_in(), last.token_out());
    let mut seen = FxHashSet::default();
    let mut cuts = vec![input.clone()];
    for swap in swaps {
        let token = swap.token_out();
        let is_end = token == output;
        if !is_end && seen.insert(token) && !connects(swaps, input, output, token) {
            cuts.push(token.clone());
        }
    }
    cuts.push(output.clone());
    cuts
}

/// Whether swaps lead from `from` to `to` without a swap that ends on `skip`.
fn connects(swaps: &[Swap], from: &Address, to: &Address, skip: &Address) -> bool {
    let mut reached = FxHashSet::default();
    reached.insert(from);
    for swap in swaps {
        let (token_in, token_out) = (swap.token_in(), swap.token_out());
        if token_out != skip && reached.contains(token_in) {
            reached.insert(token_out);
        }
    }
    reached.contains(to)
}

enum Direction {
    Forward,
    Backward,
}

/// The tokens that swaps connect to `start`, and `start` itself. `Forward` follows the swaps
/// from input to output, `Backward` the other way.
fn find_reachable(swaps: &[Swap], start: &Address, direction: Direction) -> FxHashSet<Address> {
    let mut reached = FxHashSet::default();
    reached.insert(start.clone());
    match direction {
        Direction::Forward => {
            for swap in swaps {
                if reached.contains(swap.token_in()) {
                    reached.insert(swap.token_out().clone());
                }
            }
        }
        Direction::Backward => {
            for swap in swaps.iter().rev() {
                if reached.contains(swap.token_out()) {
                    reached.insert(swap.token_in().clone());
                }
            }
        }
    }
    reached
}
