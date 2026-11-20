//! The rules a route with an exclusive leg must follow, and the route output an exclusive leg
//! produces.

use std::sync::LazyLock;

use metrics::{counter, gauge};
use num_bigint::BigUint;
use num_traits::{CheckedSub, ToPrimitive};
use tracing::warn;
use tycho_simulation::tycho_common::models::Address;

use crate::{
    bps,
    derived::TokenGasPrices,
    exclusive_protocol::protocol_of,
    types::{Commitment, OrderQuote, SurplusInfo, Swap},
};

/// How the exclusive legs of a route fit the rules for an exclusive route.
///
/// A route has one or more exclusive legs, at any hop, on any branch of a split route. The output
/// of each leg must reach the route's output token, and the swaps after each leg must not loop back
/// to a token already on their path. `pin_commitment` splits the surplus between the legs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExclusiveRouteShape {
    /// Every exclusive leg reaches the route's output token.
    Valid,
    /// The route has no exclusive leg.
    NoExclusiveLeg,
    /// For some exclusive leg, no swap carries its output to the output token, or the swaps after
    /// it loop back to a token already on their path.
    DoesNotReachOutput,
}

/// Whether `swap` is an exclusive leg: it goes through an exclusive component, or through a dual
/// pool priced with a grant.
pub(crate) fn is_exclusive_leg(swap: &Swap) -> bool {
    protocol_of(swap).is_some()
}

/// Whether any swap of `swaps` is an exclusive leg.
pub(crate) fn routes_through_exclusive(swaps: &[Swap]) -> bool {
    swaps.iter().any(is_exclusive_leg)
}

/// The first exclusive leg of `swaps`. `None` when no swap is one.
pub(crate) fn first_exclusive_leg(swaps: &[Swap]) -> Option<&Swap> {
    swaps
        .iter()
        .find(|swap| is_exclusive_leg(swap))
}

/// Returns the shape of the exclusive legs of `swaps`, without incrementing metrics.
pub(crate) fn exclusive_route_shape(swaps: &[Swap], output_token: &Address) -> ExclusiveRouteShape {
    let mut has_exclusive_leg = false;
    for leg in swaps
        .iter()
        .filter(|swap| is_exclusive_leg(swap))
    {
        has_exclusive_leg = true;
        let reaches_output = leg_route_output(swaps, leg, output_token)
            .is_some_and(|(numerator, _)| numerator > BigUint::ZERO);
        if !reaches_output {
            return ExclusiveRouteShape::DoesNotReachOutput;
        }
    }
    if has_exclusive_leg {
        ExclusiveRouteShape::Valid
    } else {
        ExclusiveRouteShape::NoExclusiveLeg
    }
}

/// Returns the route output that `leg`'s output produces, as the fraction
/// `(numerator, denominator)`.
///
/// A leg that outputs `output_token` produces its own output. Otherwise the leg gets the share of
/// its token's route output that its output makes up of all that the later swaps take of the
/// token. This is the average price of the pools after the leg. Returns `None` when the swaps after
/// the leg loop back to a token already on their path.
pub(crate) fn leg_route_output(
    swaps: &[Swap],
    leg: &Swap,
    output_token: &Address,
) -> Option<(BigUint, BigUint)> {
    if leg.token_out() == output_token {
        return Some((leg.amount_out().clone(), BigUint::from(1u8)));
    }
    let consumed = total_amount_in_for_token(swaps, leg.token_out());
    if consumed == BigUint::ZERO {
        return Some((BigUint::ZERO, BigUint::from(1u8)));
    }
    let (numerator, denominator) =
        token_route_output(swaps, leg.token_out(), output_token, &mut Vec::new())?;
    Some((leg.amount_out() * numerator, denominator * consumed))
}

/// Returns the route output that the swaps taking `token` produce from it, as the fraction
/// `(numerator, denominator)`, or `None` if they loop back to a token in `path`.
fn token_route_output(
    swaps: &[Swap],
    token: &Address,
    output_token: &Address,
    path: &mut Vec<Address>,
) -> Option<(BigUint, BigUint)> {
    if path.contains(token) {
        return None;
    }
    path.push(token.clone());

    let mut numerator = BigUint::ZERO;
    let mut denominator = BigUint::from(1u8);
    for swap in swaps
        .iter()
        .filter(|swap| swap.token_in() == token)
    {
        let (swap_numerator, swap_denominator) = if swap.token_out() == output_token {
            (swap.amount_out().clone(), BigUint::from(1u8))
        } else {
            let consumed = total_amount_in_for_token(swaps, swap.token_out());
            let (next_numerator, next_denominator) =
                token_route_output(swaps, swap.token_out(), output_token, path)?;
            (swap.amount_out() * next_numerator, next_denominator * consumed)
        };
        if swap_numerator == BigUint::ZERO {
            continue;
        }
        numerator = numerator * &swap_denominator + swap_numerator * &denominator;
        denominator *= swap_denominator;
    }

    path.pop();
    Some((numerator, denominator))
}

/// Returns how much of `token` the route's swaps take in.
fn total_amount_in_for_token(swaps: &[Swap], token: &Address) -> BigUint {
    swaps
        .iter()
        .filter(|swap| swap.token_in() == token)
        .map(|swap| swap.amount_in())
        .sum()
}

/// Environment variable overriding [`DEFAULT_USER_IMPROVEMENT_SHARE_BPS`].
const ENV_USER_IMPROVEMENT_SHARE_BPS: &str = "EXCLUSIVE_ROUTE_USER_SHARE_BPS";

/// Share of an exclusive route's improvement over the public market that is handed to the user,
/// in basis points of that improvement: `1_000` gives the user a tenth of it.
const DEFAULT_USER_IMPROVEMENT_SHARE_BPS: u32 = 1_000;

/// Wei per whole gas token. The exclusive metrics report whole gas tokens, not wei.
pub(crate) const WEI_PER_GAS_TOKEN: f64 = 1e18;

/// The configured user share, parsed from [`ENV_USER_IMPROVEMENT_SHARE_BPS`].
pub(crate) static USER_IMPROVEMENT_SHARE_BPS: LazyLock<u32> =
    LazyLock::new(user_improvement_share_bps_env);

/// Reads the user share from the environment, falling back to
/// [`DEFAULT_USER_IMPROVEMENT_SHARE_BPS`] when the variable is unset or unusable.
fn user_improvement_share_bps_env() -> u32 {
    let Ok(raw) = std::env::var(ENV_USER_IMPROVEMENT_SHARE_BPS) else {
        return DEFAULT_USER_IMPROVEMENT_SHARE_BPS;
    };
    match parse_user_improvement_share_bps(&raw) {
        Some(bps) => bps,
        None => {
            let denominator = bps::DENOMINATOR;
            warn!(
                value = %raw,
                default_bps = DEFAULT_USER_IMPROVEMENT_SHARE_BPS,
                "{ENV_USER_IMPROVEMENT_SHARE_BPS} must be an integer from 0 to \
                 {denominator} basis points; using the default",
            );
            DEFAULT_USER_IMPROVEMENT_SHARE_BPS
        }
    }
}

/// Parses a user share in basis points, rejecting anything above the whole improvement — a share
/// over `10_000` bps would commit more output than the route produces.
fn parse_user_improvement_share_bps(raw: &str) -> Option<u32> {
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|share| *share <= bps::DENOMINATOR)
}

/// Returns the part of `improvement` handed to the user, `user_share_bps` of it.
///
/// Rounded up, so a share that would otherwise round away still moves the price in the user's
/// favour. Never exceeds `improvement`, so the commitment it feeds stays within what the route
/// produces.
fn user_improvement_amount(improvement: &BigUint, user_share_bps: u32) -> BigUint {
    let denominator = BigUint::from(bps::DENOMINATOR);
    (improvement * BigUint::from(user_share_bps) + (&denominator - 1u32)) / denominator
}

/// What a route pays: the amount out, and the amount out net of gas.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteOutput<'a> {
    pub(crate) amount_out: &'a BigUint,
    pub(crate) amount_out_net_gas: &'a BigUint,
}

impl<'a> RouteOutput<'a> {
    pub(crate) fn from_quote(quote: &'a OrderQuote) -> Self {
        Self { amount_out: quote.amount_out(), amount_out_net_gas: quote.amount_out_net_gas() }
    }
}

/// Returns the amount to commit against a successful public route:
/// `max(public_amount_out, required_net + gas)`.
///
/// Returns `None` if the exclusive route fails a gate. It must match the public output net of
/// gas, and it must produce at least the public output. `combine_with_surplus` gives the reason for
/// each bound.
pub(crate) fn matched_commitment(
    public_reference: RouteOutput<'_>,
    exclusive_candidate: RouteOutput<'_>,
    user_share_bps: u32,
) -> Option<BigUint> {
    // The candidate route must match the public reference net-of-gas; anything it produces on
    // top is the improvement, of which the user keeps a share (the user improvement amount).
    let public_net_amount_out = public_reference.amount_out_net_gas;
    if exclusive_candidate.amount_out_net_gas < public_net_amount_out {
        return None;
    }
    let improvement = exclusive_candidate.amount_out_net_gas - public_net_amount_out;
    let required_net_amount_out =
        public_net_amount_out + user_improvement_amount(&improvement, user_share_bps);

    // Exact-in assumption: everything below compares and commits output amounts. For exact-out
    // orders this comparison would have to run on amount_in instead (see function docs).
    // We promise the user at least the public route's output. A private route that produces
    // less can't keep that promise, so we skip it — even when its gas savings make it better net.
    let public_amount_out = public_reference.amount_out;
    if exclusive_candidate.amount_out < public_amount_out {
        return None;
    }

    // Gas the user pays to execute the exclusive route, in output-token terms.
    let gas_cost = exclusive_candidate.amount_out - exclusive_candidate.amount_out_net_gas;
    Some((required_net_amount_out + gas_cost).max(public_amount_out.clone()))
}

/// Sets `exclusive_candidate` to `committed_amount_out`. This sets the committed amount on each
/// leg, sets `amount_out` and `amount_out_net_gas`, and attaches the `SurplusInfo`.
///
/// The exclusive components capture all output above the commitment. Each exclusive leg captures
/// a part of the surplus in proportion to the route output it produces, capped at that output.
/// Two legs in sequence both produce the whole route output, so each captures half. Rounding and
/// the cap leave any remainder with the user.
pub(crate) fn pin_commitment(
    exclusive_candidate: &OrderQuote,
    committed_amount_out: BigUint,
    token_prices: Option<&TokenGasPrices>,
) -> OrderQuote {
    let exclusive_route_amount_out = exclusive_candidate.amount_out();
    let exclusive_gas_cost = exclusive_route_amount_out - exclusive_candidate.amount_out_net_gas();
    let surplus_amount = exclusive_route_amount_out - &committed_amount_out;

    record_gas_token_amount(
        "exclusive_fee_amount",
        exclusive_candidate,
        &surplus_amount,
        token_prices,
    );

    let mut surplus_quote = exclusive_candidate.clone();

    // Route output each exclusive leg produces, read before the swaps are borrowed mutably.
    let leg_route_outputs: Vec<Option<(BigUint, BigUint)>> = surplus_quote
        .route()
        .map(|route| {
            let swaps = route.swaps();
            let output_token = route.output_token();
            swaps
                .iter()
                .map(|swap| {
                    let output_token = output_token.as_ref()?;
                    if !is_exclusive_leg(swap) {
                        return None;
                    }
                    leg_route_output(swaps, swap, output_token)
                })
                .collect()
        })
        .unwrap_or_default();

    // Route output each exclusive leg produces, rounded down, and their sum: the weights of the
    // split.
    let mut leg_outputs: Vec<BigUint> = Vec::with_capacity(leg_route_outputs.len());
    for fraction in &leg_route_outputs {
        let output = match fraction {
            Some((numerator, denominator)) => numerator / denominator,
            None => BigUint::ZERO,
        };
        leg_outputs.push(output);
    }
    let total_leg_output: BigUint = leg_outputs.iter().sum();

    if let Some(route) = surplus_quote.route_mut() {
        let surplus = exclusive_route_amount_out - &committed_amount_out;
        for (i, swap) in route.swaps_mut().iter_mut().enumerate() {
            if is_exclusive_leg(swap) {
                let Some(Some((numerator, denominator))) = leg_route_outputs.get(i) else {
                    continue;
                };
                if *numerator == BigUint::ZERO {
                    continue;
                }
                let leg_output = &leg_outputs[i];
                if *leg_output == BigUint::ZERO {
                    continue;
                }
                let captured = (&surplus * leg_output / &total_leg_output).min(leg_output.clone());
                // Convert into the leg's own token at the average price of the pools after the
                // leg, rounding down so any error is taken from the protocol, not the user. The
                // user gets at least the committed amount when those pools give less per unit
                // as their input grows. A leg that outputs the route's output token converts at
                // a price of exactly 1.
                let captured_swap_amount_out =
                    &captured * swap.amount_out() * denominator / numerator;
                debug_assert!(
                    captured_swap_amount_out <= *swap.amount_out(),
                    "captured amount ({captured_swap_amount_out}) must not exceed the leg's output ({})",
                    swap.amount_out(),
                );
                let committed_swap_amount_out = swap.amount_out() - &captured_swap_amount_out;
                swap.set_committed_amount_out(committed_swap_amount_out);
            }
        }
    }

    surplus_quote.set_amount_out(committed_amount_out.clone());

    // The user nets the committed amount minus the exclusive route's gas; in matched mode the
    // commitment is built so this is >= the public head's net, keeping the candidate list ranked
    // descending.
    surplus_quote.set_amount_out_net_gas(&committed_amount_out - &exclusive_gas_cost);

    let surplus_info = SurplusInfo::new(surplus_amount, committed_amount_out);
    surplus_quote.with_surplus(surplus_info)
}

/// Pins the commitment of a post-processed route on `quote`, and records how much the user gains
/// over the route the post-processor started from. See [`pin_commitment`].
pub(crate) fn pin_route_commitment(
    quote: &OrderQuote,
    commitment: &Commitment,
    token_prices: Option<&TokenGasPrices>,
) -> OrderQuote {
    let user_savings = commitment
        .committed_amount_out()
        .checked_sub(commitment.reference_amount_out())
        .unwrap_or_default();
    record_gas_token_amount("exclusive_user_savings_amount", quote, &user_savings, token_prices);
    pin_commitment(
        quote,
        commitment
            .committed_amount_out()
            .clone(),
        token_prices,
    )
}

/// Restates an output-token `amount` of `quote` in wei.
///
/// The rate is the derived gas price of the quote's output token, so it does not depend on how the
/// algorithm netted gas off the quote. `None` when there are no prices, the output token has none,
/// or either side of its price is zero.
pub(crate) fn to_gas_token_wei(
    quote: &OrderQuote,
    amount: &BigUint,
    token_prices: Option<&TokenGasPrices>,
) -> Option<BigUint> {
    let output_token = quote.route()?.output_token()?;
    let price = token_prices?.get(&output_token)?;
    if price.numerator == BigUint::ZERO || price.denominator == BigUint::ZERO {
        return None;
    }
    Some(amount * &price.denominator / &price.numerator)
}

/// Records an output-token `amount` under `metric`, in whole gas tokens.
///
/// Output-token base units are not summable across tokens, so a total over them means nothing. The
/// rate is the derived gas price of the quote's output token. An amount with no rate increments
/// `exclusive_unpriced_output_total`; a zero would read as "captured nothing".
pub(crate) fn record_gas_token_amount(
    metric: &'static str,
    quote: &OrderQuote,
    amount: &BigUint,
    token_prices: Option<&TokenGasPrices>,
) {
    let Some(gas_token_wei) = to_gas_token_wei(quote, amount, token_prices) else {
        counter!("exclusive_unpriced_output_total").increment(1);
        return;
    };
    gauge!(metric).increment(gas_token_wei.to_f64().unwrap_or(0.0) / WEI_PER_GAS_TOKEN);
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::zero("0", Some(0))]
    #[case::whole_improvement("10000", Some(10_000))]
    #[case::padded(" 2500 ", Some(2_500))]
    #[case::above_whole_improvement("10001", None)]
    #[case::negative("-1", None)]
    #[case::fractional("0.5", None)]
    #[case::empty("", None)]
    fn test_parse_user_improvement_share_bps(#[case] raw: &str, #[case] expected: Option<u32>) {
        assert_eq!(parse_user_improvement_share_bps(raw), expected);
    }
}
