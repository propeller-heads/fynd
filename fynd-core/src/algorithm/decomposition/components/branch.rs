//! One outer split: a shared hop plus the sequences that feed it or hang off it.

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{One, Zero};
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::components::*;

// ===================== Branch =====================

/// One parallel branch of a [`DecompositionGraph`]: a shared first [`Hop`] feeding parallel tails.
///
/// Port of the shape `_group_by_neighbour_token` builds (`order_solver.py:517-554`):
/// `Sequential[first_hop, Parallel[tails]]`, one per distinct token following the sell token. Its
/// docstring gives the reason — *"This allows for solving each group as a subgraph of independent
/// paths (i.e. no shared pools)."*
///
/// Without it, every token path starting `A -> B` owns a private [`PoolRef`] copy of the `A -> B`
/// pools, and the outer optimizer allocates that liquidity once per path as though each had
/// exclusive access. Grouping makes the shared first hop a single object that is sold exactly once
/// for the whole group, so it can only be allocated once.
///
/// # A strict generalisation of a token path
///
/// Every plain token path is a branch with one tail, and [`Branch::from_route`] performs that
/// conversion:
///
/// | token path | branch |
/// | --- | --- |
/// | `SequentialRoute[h0]` | `head: h0, tails: []` |
/// | `SequentialRoute[h0, h1]` | `head: h0, tails: [SequentialRoute[h1]]` |
/// | `SequentialRoute[h0, h1, h2]` | `head: h0, tails: [SequentialRoute[h1, h2]]` |
///
/// Every composition rule below collapses to [`SequentialRoute`]'s own when there is exactly one
/// tail: prices multiply, fees compose in series, inertia takes the minimum, gas sums. That is what
/// makes the level free of cost on branches that do not share a first hop.
///
/// # Splits
///
/// [`Branch::tail_splits`] divides the *head's output* across the tails — it is the inner parallel
/// split, one level below the graph's outer split over branches. An empty vector means unsolved,
/// the same encoding [`Hop::splits`] and [`DecompositionGraph::outer_splits`] use. A branch with no
/// tails has nothing to split and is solved as soon as its head is.
/// Which end of a [`Branch`] its [`Hop`] sits at.
///
/// A branch is one hop that carries the branch's whole amount plus several sequences that each
/// carry a fraction. Which end the hop sits at decides the order of operations, and nothing else:
/// the two shapes hold the same parts.
///
/// Grouping picks the side. Grouping candidate paths by the token *after* the sell token gives
/// [`BranchSide::Head`]; grouping by the token *before* the buy token gives [`BranchSide::Tail`].
/// Whichever produces fewer branches leaves fewer pools duplicated across branches, which is the
/// error the grouping exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchSide {
    /// Hop first: sold once, then its output is split across the sequences.
    Head,
    /// Hop last: the amount is split across the sequences, and their combined output is sold
    /// through the hop once.
    Tail,
}

pub(crate) struct Branch {
    hop: Hop,
    side: BranchSide,
    /// See [`SequentialRoute::prices`].
    prices: Option<Arc<TokenGasPrices>>,
    sequences: Vec<SequentialRoute>,

    // CZ: Can we have something like a enum state for whether the objects are solved or not? Like
    // Unsolved;Solved{splits, sell, buy}
    splits: Vec<Fraction>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    limit_cache: Option<(BigUint, Vec<ComponentId>)>,
}

impl Branch {
    /// Builds a branch from the hop every sequence passes through and the sequences themselves.
    ///
    /// Pass an empty `splits` to build it unsolved. `sequences` must be empty (a one-hop branch) or
    /// all start at the hop's output token and end at the same buy token.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when a sequence does not start where the hop ends,
    /// when the sequences disagree on the buy token, or when a non-empty `splits` does not match
    /// the sequence count.
    pub(crate) fn head(
        hop: Hop,
        sequences: Vec<SequentialRoute>,
        splits: Vec<Fraction>,
    ) -> Result<Self, DecompositionError> {
        if !splits.is_empty() && splits.len() != sequences.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "branch has {} sequences but got {} splits",
                    sequences.len(),
                    splits.len()
                ),
            });
        }
        if let Some(first) = sequences.first() {
            let buy_token = first.buy_token().address.clone();
            for tail in &sequences {
                if tail.sell_token().address != hop.token_out().address {
                    return Err(DecompositionError::InvalidStructure {
                        reason: format!(
                            "branch sequence starts at {} but the hop ends at {}",
                            tail.sell_token().address,
                            hop.token_out().address
                        ),
                    });
                }
                if tail.buy_token().address != buy_token {
                    return Err(DecompositionError::InvalidStructure {
                        reason: format!(
                            "branch tail ends at {} but a sibling ends at {buy_token}",
                            tail.buy_token().address
                        ),
                    });
                }
            }
        }
        Ok(Self {
            prices: None,
            hop,
            side: BranchSide::Head,
            sequences,
            splits,
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
        })
    }

    /// Builds a tail-grouped branch: sequences that all end where `hop` begins.
    ///
    /// The mirror of [`Branch::head`]. `sequences` must all start at the same sell token and all
    /// end at `hop`'s input token.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when a sequence does not end where the hop starts,
    /// when the sequences disagree on the sell token, when `sequences` is empty, or when a
    /// non-empty `splits` does not match the sequence count.
    pub(crate) fn tail(
        hop: Hop,
        sequences: Vec<SequentialRoute>,
        splits: Vec<Fraction>,
    ) -> Result<Self, DecompositionError> {
        if !splits.is_empty() && splits.len() != sequences.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "branch has {} sequences but got {} splits",
                    sequences.len(),
                    splits.len()
                ),
            });
        }

        let Some(first) = sequences.first() else {
            // A hop with nothing feeding it is a head-grouped branch with no sequences, not a
            // tail-grouped one; the caller has the wrong shape.
            return Err(DecompositionError::InvalidStructure {
                reason: "tail-grouped branch needs at least one sequence".to_string(),
            });
        };
        let sell_token = first.sell_token().address.clone();
        for sequence in &sequences {
            if sequence.buy_token().address != hop.token_in().address {
                return Err(DecompositionError::InvalidStructure {
                    reason: format!(
                        "branch sequence ends at {} but the hop starts at {}",
                        sequence.buy_token().address,
                        hop.token_in().address
                    ),
                });
            }
            if sequence.sell_token().address != sell_token {
                return Err(DecompositionError::InvalidStructure {
                    reason: "branch sequences disagree on the sell token".to_string(),
                });
            }
        }
        Ok(Self {
            prices: None,
            hop,
            side: BranchSide::Tail,
            sequences,
            splits,
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
        })
    }

    /// Supplies the derived mid-prices to this branch and everything under it.
    ///
    /// See [`SequentialRoute::set_prices`].
    pub(crate) fn set_prices(&mut self, prices: Arc<TokenGasPrices>) {
        for sequence in &mut self.sequences {
            sequence.set_prices(Arc::clone(&prices));
        }
        self.prices = Some(prices);
        self.limit_cache = None;
    }

    /// Which end this branch's hop sits at.
    pub(crate) fn side(&self) -> BranchSide {
        self.side
    }

    /// Splits a token path into its first hop and the remainder — the one-tail case of the table in
    /// [`Branch`].
    ///
    /// The solved state carries across exactly: a route is solved when every hop is, and the branch
    /// this produces is solved under the same condition, because `tail_splits` is set precisely
    /// when the tail is already solved.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when the remaining hops do not form a valid route.
    pub(crate) fn from_route(route: SequentialRoute) -> Result<Self, DecompositionError> {
        let (tokens, mut hops) = route.into_parts();
        let head = hops.remove(0);
        if hops.is_empty() {
            return Self::head(head, Vec::new(), Vec::new());
        }

        let tail = SequentialRoute::new(tokens[1..].to_vec(), hops)?;
        let tail_splits = if tail.solved() { vec![Fraction::one()] } else { Vec::new() };
        Self::head(head, vec![tail], tail_splits)
    }

    /// The hop every sequence of this branch passes through.
    pub(crate) fn hop(&self) -> &Hop {
        &self.hop
    }

    /// The branch's own hop, for solvers assigning its pool splits.
    pub(crate) fn hop_mut(&mut self) -> &mut Hop {
        &mut self.hop
    }

    /// Consumes a tail-less branch into its only hop.
    ///
    /// Reference-route assembly stitches two independently built one-hop subgraphs into a two-hop
    /// branch (`order_solver.py:366-368`), which needs the hop itself rather than a view of it.
    pub(crate) fn into_hop(self) -> Hop {
        self.hop
    }

    /// The parallel sequences hanging off the hop, empty for a one-hop branch.
    pub(crate) fn sequences(&self) -> &[SequentialRoute] {
        &self.sequences
    }

    /// The sequences, for solvers splitting the hop's output across them.
    pub(crate) fn sequences_mut(&mut self) -> &mut [SequentialRoute] {
        &mut self.sequences
    }

    /// Share of the hop's output routed to each sequence, empty while unsolved.
    pub(crate) fn splits(&self) -> &[Fraction] {
        &self.splits
    }

    /// Assigns one split per tail, or an empty vector to mark the tails unsolved again.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when a non-empty split vector does not match the
    /// tail count.
    pub(crate) fn set_splits(
        &mut self,
        tail_splits: Vec<Fraction>,
    ) -> Result<(), DecompositionError> {
        if !tail_splits.is_empty() && tail_splits.len() != self.sequences.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "branch has {} tails but got {} tail splits",
                    self.sequences.len(),
                    tail_splits.len()
                ),
            });
        }
        self.splits = tail_splits;
        Ok(())
    }

    /// Every hop of the branch in flow order: for [`BranchSide::Head`] the branch's own hop and
    /// then each sequence's hops, for [`BranchSide::Tail`] the sequences first and the hop last.
    pub(crate) fn hops(&self) -> Box<dyn Iterator<Item = &Hop> + '_> {
        let sequences = self
            .sequences
            .iter()
            .flat_map(SequentialRoute::hops);
        match self.side {
            BranchSide::Head => Box::new(std::iter::once(&self.hop).chain(sequences)),
            BranchSide::Tail => Box::new(sequences.chain(std::iter::once(&self.hop))),
        }
    }

    /// Every hop of the branch, mutably, in the same order as [`Branch::hops`].
    pub(crate) fn hops_mut(&mut self) -> Box<dyn Iterator<Item = &mut Hop> + '_> {
        match self.side {
            BranchSide::Head => Box::new(
                std::iter::once(&mut self.hop).chain(
                    self.sequences
                        .iter_mut()
                        .flat_map(SequentialRoute::hops_mut),
                ),
            ),
            BranchSide::Tail => Box::new(
                self.sequences
                    .iter_mut()
                    .flat_map(SequentialRoute::hops_mut)
                    .chain(std::iter::once(&mut self.hop)),
            ),
        }
    }

    /// The hop at `index` of [`Branch::hops`].
    ///
    /// Assertion sugar: the production code walks the iterator, but a test that knows the branch's
    /// shape wants to name one leg of it.
    #[cfg(test)]
    pub(crate) fn hop_at(&self, index: usize) -> &Hop {
        self.hops()
            .nth(index)
            .expect("branch has a hop at this index")
    }

    /// The hop at `index` of [`Branch::hops`], mutably. See [`Branch::hop_at`].
    #[cfg(test)]
    pub(crate) fn hop_at_mut(&mut self, index: usize) -> &mut Hop {
        self.hops_mut()
            .nth(index)
            .expect("branch has a hop at this index")
    }

    /// The branch's token path rendered from symbols, as `A->C->B`.
    ///
    /// A branch with several tails has several paths and renders them as `A->C->[D->B | B]`, which
    /// is what tells a grouped branch from an ungrouped one in a log line or an assertion.
    pub(crate) fn token_path_label(&self) -> String {
        let hop = format!("{}->{}", self.hop.token_in().symbol, self.hop.token_out().symbol);
        match self.side {
            BranchSide::Head => {
                let tails: Vec<String> = self
                    .sequences
                    .iter()
                    .map(|sequence| {
                        sequence
                            .hops()
                            .iter()
                            .map(|hop| hop.token_out().symbol.clone())
                            .collect::<Vec<_>>()
                            .join("->")
                    })
                    .collect();
                match tails.len() {
                    0 => hop,
                    1 => format!("{hop}->{}", tails[0]),
                    _ => format!("{hop}->[{}]", tails.join(" | ")),
                }
            }
            BranchSide::Tail => {
                let heads: Vec<String> = self
                    .sequences
                    .iter()
                    .map(|sequence| {
                        std::iter::once(sequence.sell_token().symbol.clone())
                            .chain(
                                sequence
                                    .hops()
                                    .iter()
                                    .take(sequence.hops().len().saturating_sub(1))
                                    .map(|hop| hop.token_out().symbol.clone()),
                            )
                            .collect::<Vec<_>>()
                            .join("->")
                    })
                    .collect();
                match heads.len() {
                    1 => format!("{}->{}", heads[0], hop),
                    _ => format!("[{}]->{}", heads.join(" | "), hop),
                }
            }
        }
    }

    /// Token the branch consumes: the hop's input when the hop leads, the sequences' shared sell
    /// token when it trails.
    pub(crate) fn sell_token(&self) -> &Token {
        match self.side {
            BranchSide::Head => self.hop.token_in(),
            BranchSide::Tail => match self.sequences.first() {
                Some(sequence) => sequence.sell_token(),
                None => self.hop.token_in(),
            },
        }
    }

    /// Token the branch produces: the sequences' shared buy token when the hop leads, the hop's
    /// output when it trails. A branch with no sequences produces its hop's output either way.
    pub(crate) fn buy_token(&self) -> &Token {
        match self.side {
            BranchSide::Head => match self.sequences.first() {
                Some(sequence) => sequence.buy_token(),
                None => self.hop.token_out(),
            },
            BranchSide::Tail => self.hop.token_out(),
        }
    }

    /// Amount of [`Branch::sell_token`] the last sell consumed.
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount of [`Branch::buy_token`] the last sell produced.
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// A branch is solved once its head is, its tails are, and the head's output has been split
    /// across them.
    ///
    /// A branch with no tails has nothing to split, so its head's state is the whole answer — which
    /// is what makes [`Branch::from_route`] preserve [`SequentialRoute::solved`] for a one-hop
    /// path.
    pub(crate) fn solved(&self) -> bool {
        if !self.hop.solved() {
            return false;
        }
        if self.sequences.is_empty() {
            return true;
        }
        !self.splits.is_empty() &&
            self.sequences
                .iter()
                .all(SequentialRoute::solved)
    }

    /// Whether the tail set falls back to unsolved estimates (`routes/parallel.py:78`).
    fn sequences_use_estimate(&self) -> bool {
        self.splits.is_empty() || splits_sum(&self.splits) < BigRational::one()
    }

    /// Mean over the tails while unsolved, split-weighted sum once solved.
    ///
    /// Only called with a non-empty tail set.
    fn combine_sequences<F>(&self, quantity: F) -> Result<f64, DecompositionError>
    where
        F: Fn(&SequentialRoute) -> Result<f64, DecompositionError>,
    {
        if self.sequences_use_estimate() {
            let mut total = 0.0;
            for tail in &self.sequences {
                total += quantity(tail)?;
            }
            return Ok(total / self.sequences.len() as f64);
        }
        let mut total = 0.0;
        for (tail, split) in self.sequences.iter().zip(&self.splits) {
            total += quantity(tail)? * split.to_f64();
        }
        Ok(total)
    }

    /// [`Branch::combine_tails`] for quantities that cannot fail.
    #[cfg(test)]
    fn combine_legs_infallible<F>(&self, quantity: F) -> f64
    where
        F: Fn(&SequentialRoute) -> f64,
    {
        if self.sequences_use_estimate() {
            let total: f64 = self
                .sequences
                .iter()
                .map(&quantity)
                .sum();
            return total / self.sequences.len() as f64;
        }
        let mut total = 0.0;
        for (tail, split) in self.sequences.iter().zip(&self.splits) {
            total += quantity(tail) * split.to_f64();
        }
        total
    }

    /// Head price times the tails' price — the product over all hops when there is one tail
    /// (`routes/sequential.py:61-65`).
    ///
    /// Parity-only, like [`Branch::fee`], [`Branch::inertia`] and [`Branch::weight`]. Candidate
    /// ranking happens on the *token paths*, before they are grouped into branches
    /// (`order_solver.py:503-514`), so nothing in the library reads these three today. They are
    /// ported and tested against the composition they must satisfy so that a future caller
    /// inherits them correct, and are not compiled into it.
    #[cfg(test)]
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        let head = self.hop.route_price()?;
        if self.sequences.is_empty() {
            return Ok(head);
        }
        Ok(head * self.combine_sequences(SequentialRoute::route_price)?)
    }

    /// The same product net of fees (`routes/sequential.py:76-81`).
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        let head = self.hop.marginal_price()?;
        if self.sequences.is_empty() {
            return Ok(head);
        }
        Ok(head * self.combine_sequences(SequentialRoute::marginal_price)?)
    }

    /// Post-trade marginal price, `None` propagating from either side
    /// (`routes/sequential.py:84-90` over `routes/parallel.py:120-134`).
    ///
    /// The head contributes a `None` when it was not sold on; the tail set contributes one when it
    /// is unsolved or when any tail carrying a non-zero split has none of its own.
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        let Some(head) = self.hop.new_marginal_price() else {
            return None;
        };
        if self.sequences.is_empty() {
            return Some(head);
        }
        if self.splits.is_empty() {
            return None;
        }

        let mut tails = 0.0;
        for (tail, split) in self.sequences.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            let Some(price) = tail.new_marginal_price() else {
                return None;
            };
            tails += price * split.to_f64();
        }
        Some(head * tails)
    }

    /// Series composition of the head's fee with the tails': `1 - (1 - head)(1 - tails)`
    /// (`routes/sequential.py:98`). Parity-only; see [`Branch::route_price`].
    #[cfg(test)]
    pub(crate) fn fee(&self) -> f64 {
        let head = self.hop.fee();
        if self.sequences.is_empty() {
            return head;
        }
        let tails = self.combine_legs_infallible(SequentialRoute::fee);
        1.0 - (1.0 - head) * (1.0 - tails)
    }

    /// Gas of every pool in the branch (`routes/sequential.py:101-102`).
    pub(crate) fn gas(&self) -> BigUint {
        let mut total = self.hop.gas();
        for tail in &self.sequences {
            total += tail.gas();
        }
        total
    }

    /// Gas of only the pools the branch's splits activate (`routes/sequential.py:93-94`).
    ///
    /// The head is always activated — a branch that carries anything pays for its head — while a
    /// tail on a zero split is not. See [`Hop::minimum_gas`].
    pub(crate) fn minimum_gas(&self) -> BigUint {
        let mut total = self.hop.minimum_gas();
        for (tail, split) in self.sequences.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            total += tail.minimum_gas();
        }
        total
    }

    /// Depth of the shallower of the head and the tail set (`routes/sequential.py:105-106`).
    ///
    /// The tail set reports the deepest tail while unsolved and the split-weighted mean once
    /// solved, exactly as [`Hop::inertia`] does over its pools. Parity-only; see
    /// [`Branch::route_price`].
    #[cfg(test)]
    pub(crate) fn inertia(&self) -> f64 {
        let head = self.hop.inertia();
        if self.sequences.is_empty() {
            return head;
        }
        let tails = if self.splits.is_empty() {
            self.sequences
                .iter()
                .map(SequentialRoute::inertia)
                .fold(f64::NEG_INFINITY, f64::max)
        } else {
            let mut total = 0.0;
            for (tail, split) in self.sequences.iter().zip(&self.splits) {
                total += tail.inertia() * split.to_f64();
            }
            total
        };
        head.min(tails)
    }

    /// Ranking score: `inertia * (1 - fee) * route_price` (`routes/sequential.py:109-111`).
    ///
    /// A branch with no tails delegates to its head instead, for the reason recorded on
    /// [`SequentialRoute::weight`]: defibot never wraps a single-hop sequence in a `SequentialRoute`
    /// but appends the bare `ParallelRoute` (`order_solver.py:450-456`), whose unsolved weight is
    /// the *maximum* over its pools (`routes/parallel.py:136-146`). The composed formula would pair
    /// the mean pool price with the maximum pool inertia and can score above every individual pool,
    /// inflating single-hop branches against multi-hop ones.
    ///
    /// Parity-only; see [`Branch::route_price`]. The delegation ranking actually relies on lives on
    /// [`SequentialRoute::weight`], which scores the token paths before they are grouped.
    #[cfg(test)]
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        if self.sequences.is_empty() {
            return self.hop.weight();
        }
        Ok(self.route_price()? * (1.0 - self.fee()) * self.inertia())
    }

    /// Price actually achieved by the last sell, in human units. Gas is not accounted for.
    pub(crate) fn executed_price(&self) -> f64 {
        executed_price(&self.sell_amount, self.sell_token(), &self.buy_amount, self.buy_token())
    }

    /// Sells `amount` through the head and splits its output across the tails.
    ///
    /// This is what makes the shared first hop safe: it is sold **once**, for the whole branch, so
    /// no two tails can each assume they get all of its liquidity.
    ///
    /// Returns the bought amount and the total gas.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::Unsolved`] when the tails were never split,
    /// [`DecompositionError::SellAmountLimit`] with the limit cast back into
    /// [`Branch::sell_token`] units, and whatever the head or the tails raise.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        let (limit, pools) = self.sell_amount_limit()?;
        if amount > &limit {
            return Err(DecompositionError::SellAmountLimit {
                limit,
                token: self.sell_token().address.clone(),
                pools,
            });
        }
        if !self.sequences.is_empty() && self.splits.is_empty() {
            return Err(DecompositionError::Unsolved {
                token_in: self.hop.token_out().address.clone(),
                token_out: self.buy_token().address.clone(),
            });
        }

        match self.side {
            BranchSide::Head => self.sell_hop_first(amount),
            BranchSide::Tail => self.sell_sequences_first(amount),
        }
    }

    /// [`BranchSide::Head`]: sell the hop once, then split its output across the sequences.
    fn sell_hop_first(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        let (hop_out, mut total_gas) = self.hop.sell(amount)?;
        if self.sequences.is_empty() {
            self.sell_amount = amount.clone();
            self.buy_amount = hop_out.clone();
            return Ok((hop_out, total_gas));
        }

        let mut total_bought = BigUint::zero();
        for index in 0..self.sequences.len() {
            let sequence_amount = self.splits[index].apply(&hop_out);
            match self.sequences[index].sell(&sequence_amount) {
                Ok((bought, gas)) => {
                    total_bought += bought;
                    total_gas += gas;
                }
                Err(DecompositionError::SellAmountLimit { limit, pools, .. }) => {
                    return Err(DecompositionError::SellAmountLimit {
                        limit: self.cast_from_hop_out(&limit)?,
                        token: self.sell_token().address.clone(),
                        pools,
                    });
                }
                Err(other) => return Err(other),
            }
        }

        self.sell_amount = amount.clone();
        self.buy_amount = total_bought.clone();
        Ok((total_bought, total_gas))
    }

    /// [`BranchSide::Tail`]: split the amount across the sequences, then sell their combined
    /// output through the hop once.
    ///
    /// Selling the hop once is the whole point of grouping this way. Every sequence converges on
    /// it, so if each held its own copy the split search would hand every one of them that pool's
    /// full liquidity and the plan would over-subscribe it — which is what a run of paths sharing
    /// one shallow final pool does under head grouping.
    fn sell_sequences_first(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        let mut into_hop = BigUint::zero();
        let mut total_gas = BigUint::zero();
        for index in 0..self.sequences.len() {
            let sequence_amount = self.splits[index].apply(amount);
            let (bought, gas) = self.sequences[index].sell(&sequence_amount)?;
            into_hop += bought;
            total_gas += gas;
        }

        let (bought, gas) = match self.hop.sell(&into_hop) {
            Ok(result) => result,
            Err(DecompositionError::SellAmountLimit { limit, pools, .. }) => {
                // The hop refuses in its own input token, which is the sequences' buy token. Cast
                // it back the way the head side casts its tails' limit, so the caller's back-off
                // is denominated in what it actually passed in.
                return Err(DecompositionError::SellAmountLimit {
                    limit: self.cast_to_sequence_in(&limit)?,
                    token: self.sell_token().address.clone(),
                    pools,
                });
            }
            Err(other) => return Err(other),
        };
        total_gas += gas;

        self.sell_amount = amount.clone();
        self.buy_amount = bought.clone();
        Ok((bought, total_gas))
    }

    /// Largest amount of [`Branch::sell_token`] the branch can absorb.
    ///
    /// The head's own limit against the tails' summed limit cast back through the head — the
    /// parallel sum of `routes/parallel.py:216-222` inside the sequential minimum of
    /// `routes/sequential.py:176-185`. A tail set that can absorb nothing takes the whole branch to
    /// zero, and the head wins an exact tie the way the lower hop index does in a plain route.
    ///
    /// Cached until [`Branch::invalidate`].
    pub(crate) fn sell_amount_limit(
        &mut self,
    ) -> Result<(BigUint, Vec<ComponentId>), DecompositionError> {
        if let Some(cached) = self.limit_cache.as_ref() {
            return Ok(cached.clone());
        }

        let (hop_limit, hop_pools) = self.hop.sell_amount_limit()?;
        if self.sequences.is_empty() {
            debug!(
                branch = %self.token_path_label(),
                hop_limit = %hop_limit,
                bound_by = "hop (no sequences)",
                "branch sell limit: how the branch is bounded"
            );
            let limit = (hop_limit, hop_pools);
            self.limit_cache = Some(limit.clone());
            return Ok(limit);
        }

        // Parallel alternatives sum; a sequence takes the tighter of its two parts. Both terms
        // have to be in sell-token units before they can be compared, which is what the cast does.
        let mut sequences_limit = BigUint::zero();
        let mut sequence_pools = Vec::new();
        for sequence in &mut self.sequences {
            let (limit, pools) = sequence.sell_amount_limit()?;
            sequences_limit += limit;
            sequence_pools.extend(pools);
        }

        if sequences_limit.is_zero() {
            let limit = (BigUint::zero(), sequence_pools);
            self.limit_cache = Some(limit.clone());
            return Ok(limit);
        }

        let limit = match self.side {
            // The sequences run from the hop's output to the buy token, so their summed limit is
            // in the hop's output token and casts back through the hop's own price.
            BranchSide::Head => {
                let cast = self.cast_from_hop_out(&sequences_limit)?;
                debug!(
                    branch = %self.token_path_label(),
                    side = "head",
                    hop_limit = %hop_limit,
                    sequences_limit = %sequences_limit,
                    sequences_cast_to_sell_token = %cast,
                    bound_by = if cast < hop_limit { "sequences" } else { "hop" },
                    "branch sell limit: how the branch is bounded"
                );
                if cast < hop_limit {
                    (cast, sequence_pools)
                } else {
                    (hop_limit, hop_pools)
                }
            }
            // The sequences already run from the sell token, so their summed limit needs no cast.
            // The hop's does: it is denominated in the token the sequences deliver.
            BranchSide::Tail => {
                let cast = self.cast_to_sequence_in(&hop_limit)?;
                debug!(
                    branch = %self.token_path_label(),
                    side = "tail",
                    hop_limit = %hop_limit,
                    hop_cast_to_sell_token = %cast,
                    sequences_limit = %sequences_limit,
                    bound_by = if cast < sequences_limit { "hop" } else { "sequences" },
                    "branch sell limit: how the branch is bounded"
                );
                if cast < sequences_limit {
                    (cast, hop_pools)
                } else {
                    (sequences_limit, sequence_pools)
                }
            }
        };
        self.limit_cache = Some(limit.clone());
        Ok(limit)
    }

    /// Converts an amount denominated in the hop's *input* token into sell-token units.
    ///
    /// The [`BranchSide::Tail`] mirror of [`Branch::cast_from_hop_out`]. The sequences all end at
    /// the hop's input token, so their combined route price converts back to what the branch was
    /// asked for. Like its mirror it is a linear approximation: it uses the sequences' spot prices
    /// and so ignores the impact they would suffer pushing that amount through.
    ///
    /// The sequences may disagree on price, so this uses the same combination the branch's own
    /// pricing does — the mean while unsolved, the split-weighted sum once solved.
    pub(crate) fn cast_to_sequence_in(
        &self,
        amount: &BigUint,
    ) -> Result<BigUint, DecompositionError> {
        if let Some(prices) = self.prices.as_ref() {
            if let Some(converted) =
                convert_through_numeraire(prices, amount, self.hop.token_in(), self.sell_token())
            {
                return Ok(converted);
            }
        }
        // Fallback only. Averaging route prices across unrelated sequences is not a price: one
        // exotic member drags the mean by orders of magnitude and the cast collapses to zero,
        // which zeroes the branch's limit and drops it from the solve entirely.
        let price = self.combine_sequences(SequentialRoute::route_price)?;
        if price == 0.0 {
            return Ok(BigUint::zero());
        }
        let conversion = 1.0 / price;
        let Some(conversion) = BigRational::from_float(conversion) else {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "sequence price produced a non-finite conversion factor: {conversion}"
                ),
            });
        };
        let scaled = BigRational::from(BigInt::from(amount.clone())) *
            conversion *
            decimal_scale(self.sell_token().decimals, self.hop.token_in().decimals);
        (scaled.numer() / scaled.denom())
            .to_biguint()
            .ok_or_else(|| DecompositionError::InvalidStructure {
                reason: "cast to sell token produced a negative amount".to_string(),
            })
    }

    /// Converts an amount denominated in the head's *output* token into sell-token units.
    ///
    /// The one-hop case of [`SequentialRoute::cast_to_sell_token`], and a linear approximation for
    /// the same reason: it divides by the head's spot price and so ignores the impact the head
    /// would suffer while actually pushing that amount through.
    pub(crate) fn cast_from_hop_out(
        &self,
        amount: &BigUint,
    ) -> Result<BigUint, DecompositionError> {
        if let Some(prices) = self.prices.as_ref() {
            if let Some(converted) =
                convert_through_numeraire(prices, amount, self.hop.token_out(), self.sell_token())
            {
                return Ok(converted);
            }
        }
        let price = self.hop.route_price()?;
        if price == 0.0 {
            return Ok(BigUint::zero());
        }
        let conversion = 1.0 / price;
        let Some(conversion) = BigRational::from_float(conversion) else {
            return Err(DecompositionError::InvalidStructure {
                reason: format!("head price produced a non-finite conversion factor: {conversion}"),
            });
        };
        let scaled = BigRational::from(BigInt::from(amount.clone())) *
            conversion *
            decimal_scale(self.hop.token_in().decimals, self.hop.token_out().decimals);
        (scaled.numer() / scaled.denom())
            .to_biguint()
            .ok_or_else(|| DecompositionError::InvalidStructure {
                reason: "cast to sell token produced a negative amount".to_string(),
            })
    }

    /// Drops this branch's cached limit and every cache below it.
    pub(crate) fn invalidate(&mut self) {
        self.limit_cache = None;
        self.hop.invalidate();
        for tail in &mut self.sequences {
            tail.invalidate();
        }
    }
}
