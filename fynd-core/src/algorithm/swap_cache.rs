use num_bigint::BigUint;
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::{models::Address, simulation::errors::SimulationError};

use crate::{algorithm::sim_meter, ComponentId};

/// How far apart the two amounts either side of a requested one may sit, as a percentage of the
/// amount requested, before ranking stops reading across them and asks the pool instead.
const INTERPOLATION_GAP_PERCENT: u32 = 10;

/// A pool and the direction taken through it. Both token addresses are part of it — a pool trading
/// three tokens answers `USDC -> DAI` and `USDT -> DAI` differently for the same amount.
#[derive(PartialEq, Eq, Hash)]
pub struct PoolDirection<'a> {
    pub(crate) component_id: &'a ComponentId,
    pub(crate) address_in: &'a Address,
    pub(crate) address_out: &'a Address,
}

/// Why a pool paid nothing for an amount.
///
/// A pool that turns an amount down for being more than it can serve turns down every larger
/// amount too, and that is worth remembering. A pool that simply failed says nothing about any
/// other amount — the panic guard turns a component that blew up on one input into an error like
/// any other, and reading a size limit out of that would drop a working pool for the whole solve.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The pool rejected the input itself, which is how it reports an amount beyond its limit.
    OverLimit,
    /// The pool failed for a reason that carries no information about size.
    Failed,
}

impl Refusal {
    /// Reads a failed simulation. Only the pool rejecting the input says anything about size;
    /// a fatal error is what a caught panic arrives as, and a recoverable one is transient.
    pub(crate) fn of(error: &SimulationError) -> Self {
        match error {
            SimulationError::InvalidInput(_, _) => Refusal::OverLimit,
            SimulationError::FatalError(_) | SimulationError::RecoverableError(_) => {
                Refusal::Failed
            }
        }
    }
}

/// What a swap paid: what came out, and the gas it cost.
#[derive(Clone)]
pub struct SwapResult {
    pub(crate) amount_out: BigUint,
    pub(crate) gas: BigUint,
}

/// What one pool paid at each amount it was asked, ascending by amount. A missing outcome is an
/// amount it refused.
///
/// Short by nature — a handful of amounts per direction over one solve — so a sorted `Vec` searched
/// by bisection beats a map, and inserting in place keeps the neighbours of any amount adjacent.
#[derive(Default)]
pub struct SwappedAmounts {
    /// Sorted ascending by amount!
    amounts_and_results: Vec<(BigUint, Option<SwapResult>)>,
    /// The amount from which this pool has been taken to refuse everything. See
    /// [`SwappedAmounts::record`] for what has to hold before an amount is recorded here.
    failed_at: Option<BigUint>,
}

impl SwappedAmounts {
    /// Whether `amount_in` is at or above the point this pool started refusing.
    fn refuses(&self, amount_in: &BigUint) -> bool {
        self.failed_at
            .as_ref()
            .is_some_and(|refused_from| amount_in >= refused_from)
    }

    /// Records what the pool paid for `amount_in`.
    ///
    /// A pool that turns an amount down for being over its limit turns down every larger amount
    /// too, so `refused_from` is set to this amount (or lowered to it) and larger amounts are
    /// refused without asking the pool.
    ///
    /// Three things must hold first, because a refusal is not always about size:
    ///
    /// * the pool rejected the input itself rather than failing ([`Refusal::OverLimit`]) — a pool
    ///   that blew up on one input still serves every other;
    /// * the pool served some smaller amount, so this really is a limit — a swap can also fail for
    ///   being too small to quote, and that fails at the opposite end;
    /// * the pool served no larger amount, which would prove it does not refuse by size at all.
    fn record(
        &mut self,
        insert_at: usize,
        amount_in: &BigUint,
        outcome: Result<SwapResult, Refusal>,
    ) {
        let refusal = outcome.as_ref().err().copied();
        self.amounts_and_results
            .insert(insert_at, (amount_in.clone(), outcome.ok()));
        if refusal != Some(Refusal::OverLimit) {
            return;
        }

        let was_served = |(_, outcome): &(BigUint, Option<SwapResult>)| outcome.is_some();
        let served_below = self.amounts_and_results[..insert_at]
            .iter()
            .any(was_served);
        let served_above = self.amounts_and_results[insert_at + 1..]
            .iter()
            .any(was_served);
        if !served_below || served_above {
            return;
        }

        let lowest_refused = match self.failed_at.take() {
            Some(already_refused) if already_refused <= *amount_in => already_refused,
            _ => amount_in.clone(),
        };
        self.failed_at = Some(lowest_refused);
    }
}

/// Swaps already made, so a pool asked the same question twice is only simulated once.
///
/// **Only for swaps that read untouched component state.** Every answer here is kept for the whole
/// solve, which is sound exactly while nothing commits a swap back into the state being read. The
/// chunked water-fills do commit — they ask one pool the same question repeatedly and depend on a
/// worse answer each time as it is drained — so they simulate against their own overlay and must
/// not come through here.
pub struct SwapCache<'a> {
    by_direction: FxHashMap<PoolDirection<'a>, SwappedAmounts>,
}

impl<'a> SwapCache<'a> {
    pub(crate) fn new() -> Self {
        Self { by_direction: FxHashMap::default() }
    }

    /// What `direction` pays for `amount_in`.
    ///
    /// Answers from the amounts already asked of that pool where it can, reading across two of them
    /// when the asking pass allows it, and otherwise calls `simulate` and keeps the result. Every
    /// route through here is booked against the component and the pass, so the report separates
    /// what was simulated from what was reused and from what was read across.
    pub(crate) fn swap(
        &mut self,
        direction: PoolDirection<'a>,
        amount_in: &BigUint,
        label: &'static str,
        simulate: impl FnOnce() -> Result<SwapResult, Refusal>,
        may_interpolate: bool,
    ) -> Option<SwapResult> {
        let component_id = direction.component_id;
        let amounts_swapped = self
            .by_direction
            .entry(direction)
            .or_default();

        let insert_at = match amounts_swapped
            .amounts_and_results
            .binary_search_by(|(amount, _)| amount.cmp(amount_in))
        {
            Ok(asked_before) => {
                sim_meter::record_cache_hit(component_id, label);
                return amounts_swapped.amounts_and_results[asked_before]
                    .1
                    .clone();
            }
            Err(insert_at) => insert_at,
        };

        // Asking a pool for more than it has already turned down buys the same refusal again, and
        // on a `vm:` pool that is as expensive as a swap it would have served.
        if amounts_swapped.refuses(amount_in) {
            sim_meter::record_refusal_without_calling(component_id, label);
            return None;
        }

        if may_interpolate {
            if let Some(read_across) = Self::interpolate(amounts_swapped, insert_at, amount_in) {
                sim_meter::record_interpolation(component_id, label);
                return Some(read_across);
            }
        }

        // Only a simulated amount is kept. Keeping one that was itself read across would let the
        // error compound, each reading drifting further from the pool's own curve.
        let outcome = simulate();
        amounts_swapped.record(insert_at, amount_in, outcome.clone());
        outcome.ok()
    }

    /// What the pool would pay for `amount_in`, read across the amounts either side of it.
    ///
    /// Output against input is concave for a pool — each further unit in buys less out — so the
    /// straight line between two amounts runs below the pool's own curve. Reading across it
    /// therefore comes out a little low, never high, and a path can only lose a ranking it
    /// deserved rather than win one it did not.
    ///
    /// That only holds between two amounts. Past the largest one asked, the same line runs above
    /// the curve, because it carries a price the pool no longer offers — so those are simulated.
    ///
    /// The gap between the two amounts must also be within [`INTERPOLATION_GAP_PERCENT`] of the
    /// amount asked for.
    /// A wider gap is where the line drifts furthest from the curve, and the gap narrows on its
    /// own: a request this turns away is simulated, and that amount lands between the two,
    /// leaving a closer pair behind for the next pass.
    ///
    /// Gas takes the larger amount's figure, not the nearer one's. Gas does not climb smoothly — a
    /// crossed tick costs what it costs — and a swap never needs less gas than a smaller one, so
    /// the larger amount's figure is never under the truth. Ranking subtracts gas from output, so
    /// understating it here would overstate a path's net and let it take a place it had not
    /// earned, which is the one thing this must not do.
    fn interpolate(
        amounts_swapped: &SwappedAmounts,
        insert_at: usize,
        amount_in: &BigUint,
    ) -> Option<SwapResult> {
        let (lower_amount, lower) = amounts_swapped
            .amounts_and_results
            .get(insert_at.checked_sub(1)?)?;
        let (upper_amount, upper) = amounts_swapped
            .amounts_and_results
            .get(insert_at)?;
        let (lower, upper) = (lower.as_ref()?, upper.as_ref()?);

        let amount_gap = upper_amount - lower_amount;
        if &amount_gap * 100u32 > amount_in * INTERPOLATION_GAP_PERCENT {
            return None;
        }
        // A pool paying less for more is not the concave curve this reads across.
        if upper.amount_out < lower.amount_out {
            return None;
        }

        let output_gap = &upper.amount_out - &lower.amount_out;
        let amount_past_lower = amount_in - lower_amount;
        let amount_out = &lower.amount_out + output_gap * amount_past_lower / amount_gap;
        Some(SwapResult { amount_out, gas: upper.gas.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::addr;

    /// Stage labels the cache is asked under. The cache only groups by the label and obeys the
    /// interpolation flag, so the tests name them here rather than reaching for a solve's stages.
    const RANKING: &str = "ranking";
    const COMMITTING: &str = "chunking";
    const EXCHANGE: &str = "exchange";
    const INTERPOLATES: bool = true;
    const NO_INTERPOLATION: bool = false;

    // ==================== Reading across known amounts ====================

    fn hop(amount_out: u64, gas: u64) -> SwapResult {
        SwapResult { amount_out: BigUint::from(amount_out), gas: BigUint::from(gas) }
    }

    /// A cache holding `amounts` for one pool direction, with no refusal point recorded.
    fn cache_holding(amounts: Vec<(u64, Option<SwapResult>)>) -> SwappedAmounts {
        SwappedAmounts {
            amounts_and_results: amounts
                .into_iter()
                .map(|(amount, outcome)| (BigUint::from(amount), outcome))
                .collect(),
            failed_at: None,
        }
    }

    /// Where `amount` would be inserted into a cache's ascending amounts.
    fn insert_at(swapped: &SwappedAmounts, amount: &BigUint) -> usize {
        swapped
            .amounts_and_results
            .binary_search_by(|(known, _)| known.cmp(amount))
            .expect_err("amount must not already be recorded")
    }

    fn read_across(swapped: &SwappedAmounts, amount: u64) -> Option<SwapResult> {
        let amount = BigUint::from(amount);
        SwapCache::interpolate(swapped, insert_at(swapped, &amount), &amount)
    }

    /// Halfway between two amounts reads back halfway between their outputs.
    #[test]
    fn test_interpolate_reads_across_two_amounts() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(2080, 90)))]);

        let across = read_across(&swapped, 1020).expect("bracketed and inside the gap");

        assert_eq!(across.amount_out, BigUint::from(2040u64));
    }

    /// Gas comes from the larger amount, never the nearer one: understating gas would overstate a
    /// path's output net of gas, which is the one direction reading across must not err in.
    #[test]
    fn test_interpolate_takes_gas_from_the_larger_amount() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(2080, 90)))]);

        let nearer_the_lower = read_across(&swapped, 1001).expect("bracketed and inside the gap");

        assert_eq!(nearer_the_lower.gas, BigUint::from(90u64));
    }

    /// Amounts further apart than `INTERPOLATION_GAP_PERCENT` are left to the pool.
    #[test]
    fn test_interpolate_declines_a_wide_gap() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1500, Some(hop(2600, 90)))]);

        assert!(read_across(&swapped, 1200).is_none());
    }

    /// Above every amount asked, the straight line carries a price the pool no longer offers, so
    /// there is nothing to read across.
    #[test]
    fn test_interpolate_declines_above_the_largest_amount() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(2080, 90)))]);

        assert!(read_across(&swapped, 1050).is_none());
    }

    /// A pool paying less for more is not the curve this reads across.
    #[test]
    fn test_interpolate_declines_when_output_falls() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(1900, 90)))]);

        assert!(read_across(&swapped, 1020).is_none());
    }

    /// A refused amount either side leaves nothing to read across.
    #[test]
    fn test_interpolate_declines_across_a_refusal() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, None)]);

        assert!(read_across(&swapped, 1020).is_none());
    }

    // ==================== Refusals reaching upwards ====================

    fn record(swapped: &mut SwappedAmounts, amount: u64, outcome: Result<SwapResult, Refusal>) {
        let amount = BigUint::from(amount);
        let at = insert_at(swapped, &amount);
        swapped.record(at, &amount, outcome);
    }

    /// A refusal above an amount the pool served is taken to refuse everything larger.
    #[test]
    fn test_refusal_above_a_served_amount_reaches_upwards() {
        let mut swapped = cache_holding(vec![(1000, Some(hop(2000, 50)))]);

        record(&mut swapped, 2000, Err(Refusal::OverLimit));

        assert!(swapped.refuses(&BigUint::from(2000u64)));
        assert!(swapped.refuses(&BigUint::from(5000u64)));
        assert!(!swapped.refuses(&BigUint::from(1500u64)));
    }

    /// A refusal with nothing served below it may be an amount too small to quote rather than a
    /// limit, so it stands only for itself.
    #[test]
    fn test_refusal_with_nothing_served_below_stands_alone() {
        let mut swapped = cache_holding(vec![]);

        record(&mut swapped, 1000, Err(Refusal::OverLimit));

        assert!(!swapped.refuses(&BigUint::from(5000u64)));
    }

    /// A larger amount the pool did serve says it does not refuse upwards at all.
    #[test]
    fn test_refusal_below_a_served_amount_does_not_reach_upwards() {
        let mut swapped =
            cache_holding(vec![(1000, Some(hop(2000, 50))), (3000, Some(hop(5000, 50)))]);

        record(&mut swapped, 2000, Err(Refusal::OverLimit));

        assert!(!swapped.refuses(&BigUint::from(4000u64)));
    }

    // ==================== The cache deciding when a pool is called ====================

    /// Drives `SwapCache::swap` with a closure that counts its calls, so a test can assert whether
    /// the pool was reached at all — which is the whole point of the cache.
    struct CountingPool {
        component_id: ComponentId,
        address_in: Address,
        address_out: Address,
        calls: std::cell::Cell<usize>,
        answer: Option<SwapResult>,
    }

    impl CountingPool {
        fn paying(amount_out: u64) -> Self {
            Self {
                component_id: ComponentId::from("pool"),
                address_in: addr(0x01),
                address_out: addr(0x02),
                calls: std::cell::Cell::new(0),
                answer: Some(hop(amount_out, 10)),
            }
        }

        fn refusing() -> Self {
            Self { answer: None, ..Self::paying(0) }
        }

        fn direction(&self) -> PoolDirection<'_> {
            PoolDirection {
                component_id: &self.component_id,
                address_in: &self.address_in,
                address_out: &self.address_out,
            }
        }

        fn ask<'a>(
            &'a self,
            cache: &mut SwapCache<'a>,
            amount: u64,
            label: &'static str,
            may_interpolate: bool,
        ) -> Option<SwapResult> {
            cache.swap(
                self.direction(),
                &BigUint::from(amount),
                label,
                || {
                    self.calls.set(self.calls.get() + 1);
                    self.answer
                        .clone()
                        .ok_or(Refusal::OverLimit)
                },
                may_interpolate,
            )
        }
    }

    /// The same amount asked twice reaches the pool once; the second answer is the first one.
    #[test]
    fn test_swap_answers_a_repeated_amount_without_calling() {
        let pool = CountingPool::paying(2000);
        let mut cache = SwapCache::new();

        let first = pool.ask(&mut cache, 1000, COMMITTING, NO_INTERPOLATION);
        let second = pool.ask(&mut cache, 1000, COMMITTING, NO_INTERPOLATION);

        assert_eq!(pool.calls.get(), 1);
        assert_eq!(first.map(|h| h.amount_out), Some(BigUint::from(2000u64)));
        assert_eq!(second.map(|h| h.amount_out), Some(BigUint::from(2000u64)));
    }

    /// Past an amount the pool refused for being over its limit, larger amounts are refused
    /// without asking it again.
    #[test]
    fn test_swap_short_circuits_above_a_refusal() {
        let pool = CountingPool::refusing();
        let mut cache = SwapCache::new();
        // Serve a smaller amount first: a refusal only reaches upwards once one below it worked.
        cache.swap(
            pool.direction(),
            &BigUint::from(500u64),
            COMMITTING,
            || Ok(hop(1000, 10)),
            NO_INTERPOLATION,
        );
        pool.ask(&mut cache, 1000, COMMITTING, NO_INTERPOLATION);
        let calls_after_refusal = pool.calls.get();

        let larger = pool.ask(&mut cache, 5000, COMMITTING, NO_INTERPOLATION);

        assert!(larger.is_none());
        assert_eq!(pool.calls.get(), calls_after_refusal, "the pool was asked again");
    }

    /// A pass that may read across two nearby amounts gets an answer without a call; one that may
    /// not is simulated for real. This is the gate that keeps approximated amounts out of the
    /// passes that commit and report.
    #[test]
    fn test_swap_interpolates_only_for_a_pass_that_allows_it() {
        let interpolating = CountingPool::paying(0);
        let mut cache = SwapCache::new();
        cache.swap(
            interpolating.direction(),
            &BigUint::from(1000u64),
            RANKING,
            || Ok(hop(1000, 10)),
            INTERPOLATES,
        );
        cache.swap(
            interpolating.direction(),
            &BigUint::from(1100u64),
            RANKING,
            || Ok(hop(1100, 10)),
            INTERPOLATES,
        );
        let calls_before = interpolating.calls.get();

        let read_across = interpolating.ask(&mut cache, 1050, RANKING, INTERPOLATES);
        assert_eq!(interpolating.calls.get(), calls_before, "ranking should not have called");
        assert_eq!(read_across.map(|h| h.amount_out), Some(BigUint::from(1050u64)));

        let simulated = interpolating.ask(&mut cache, 1060, EXCHANGE, NO_INTERPOLATION);
        assert_eq!(interpolating.calls.get(), calls_before + 1, "exchange must call the pool");
        assert_eq!(simulated.map(|h| h.amount_out), Some(BigUint::from(0u64)));
    }

    /// An interpolated answer is not kept, so it can never be read across again and compound. The
    /// same request a second time still reaches the pool.
    #[test]
    fn test_swap_does_not_store_an_interpolated_answer() {
        let pool = CountingPool::paying(7777);
        let mut cache = SwapCache::new();
        cache.swap(
            pool.direction(),
            &BigUint::from(1000u64),
            RANKING,
            || Ok(hop(1000, 10)),
            INTERPOLATES,
        );
        cache.swap(
            pool.direction(),
            &BigUint::from(1100u64),
            RANKING,
            || Ok(hop(1100, 10)),
            INTERPOLATES,
        );
        pool.ask(&mut cache, 1050, RANKING, INTERPOLATES);

        let asked_again = pool.ask(&mut cache, 1050, EXCHANGE, NO_INTERPOLATION);

        assert_eq!(pool.calls.get(), 1, "the interpolated answer should not have been stored");
        assert_eq!(asked_again.map(|h| h.amount_out), Some(BigUint::from(7777u64)));
    }

    /// A pool that failed rather than rejected the amount says nothing about larger amounts: the
    /// panic guard reports a component that blew up on one input as an error like any other, and
    /// reading a limit out of that would drop a working pool for the rest of the solve.
    #[test]
    fn test_failure_that_is_not_a_limit_does_not_reach_upwards() {
        let mut swapped = cache_holding(vec![(1000, Some(hop(2000, 50)))]);

        record(&mut swapped, 2000, Err(Refusal::Failed));

        assert!(!swapped.refuses(&BigUint::from(2000u64)));
        assert!(!swapped.refuses(&BigUint::from(5000u64)));
    }

    /// A second, lower refusal moves the point everything above is refused from down to it.
    #[test]
    fn test_lower_refusal_moves_the_refusal_point_down() {
        let mut swapped = cache_holding(vec![(1000, Some(hop(2000, 50)))]);

        record(&mut swapped, 3000, Err(Refusal::OverLimit));
        record(&mut swapped, 2000, Err(Refusal::OverLimit));

        assert!(swapped.refuses(&BigUint::from(2000u64)));
    }
}
