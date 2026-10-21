//! The rules a route with an exclusive leg must follow, and the route output an exclusive leg
//! produces.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Address;

use crate::{feed::exclusivity::is_exclusive, types::Swap};

/// How the exclusive legs of a route fit the rules for an exclusive route.
///
/// A route has exactly one exclusive leg, at any hop, on any branch of a split route. The output
/// of the leg must reach the route's output token, and the swaps after the leg must not loop back
/// to a token already on their path. The route has a single leg so that the attribution of the
/// surplus to each leg stays unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExclusiveRouteShape {
    /// One exclusive leg that reaches the route's output token.
    Valid,
    /// The route has no exclusive leg.
    NoExclusiveLeg,
    /// The route has more than one exclusive leg.
    MultipleLegs,
    /// No swap carries the leg's output to the output token, or the swaps after the leg loop back
    /// to a token already on their path.
    DoesNotReachOutput,
}

/// Whether `swap` goes through an exclusive component.
pub(crate) fn is_exclusive_leg(swap: &Swap) -> bool {
    is_exclusive(swap.protocol_component())
}

/// Returns the shape of the exclusive legs of `swaps`, without incrementing metrics.
pub(crate) fn exclusive_route_shape(swaps: &[Swap], output_token: &Address) -> ExclusiveRouteShape {
    let mut exclusive_legs = swaps
        .iter()
        .filter(|swap| is_exclusive_leg(swap));
    let Some(leg) = exclusive_legs.next() else {
        return ExclusiveRouteShape::NoExclusiveLeg;
    };
    if exclusive_legs.next().is_some() {
        return ExclusiveRouteShape::MultipleLegs;
    }

    let reaches_output = leg_route_output(swaps, leg, output_token)
        .is_some_and(|(numerator, _)| numerator > BigUint::ZERO);
    if reaches_output {
        ExclusiveRouteShape::Valid
    } else {
        ExclusiveRouteShape::DoesNotReachOutput
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
    let consumed = consumed_amount(swaps, leg.token_out());
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
            let consumed = consumed_amount(swaps, swap.token_out());
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
fn consumed_amount(swaps: &[Swap], token: &Address) -> BigUint {
    swaps
        .iter()
        .filter(|swap| swap.token_in() == token)
        .map(|swap| swap.amount_in())
        .sum()
}
