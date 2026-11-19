//! Exclusive-component classification.
//!
//! "Exclusive" means swappable only with off-chain authorization: such components must never
//! enter the route a public worker pool returns, while an exclusive-access worker pool routes
//! through them to capture the surplus they offer above the best public-market rate. Both worker
//! pool kinds serve the same request; they differ only in which liquidity their workers may route
//! through. A `PublicOnly` worker feeds this classification to `feed::component_filter`, which
//! keeps such a component out of its graph; workers of `IncludeExclusive`-scoped worker pools
//! ingest everything.
//!
//! A component is classified from its own data via `is_exclusive`, applied generically to every
//! ingested component. A Curve pool is exclusive while its policy is one of `FYND_POLICIES`. Its
//! policy can change, so the feed copies the result into the component's static attributes.

use tycho_simulation::{
    evm::protocol::curve::CurveState,
    tycho_common::{models::protocol::ProtocolComponent, simulation::protocol_sim::ProtocolSim},
};

/// A policy contract that makes a Curve pool exclusive.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FyndPolicy {
    pub(crate) address: [u8; 20],
    /// The lowest fee the policy accepts in a grant, in units of 1e-10.
    pub(crate) min_fee: u64,
}

#[cfg(not(test))]
const FYND_POLICIES: &[FyndPolicy] = &[];
#[cfg(test)]
const FYND_POLICIES: &[FyndPolicy] = &[TEST_FYND_POLICY];

/// The Fynd policy that tests attach to a Curve pool.
#[cfg(test)]
pub(crate) const TEST_FYND_POLICY: FyndPolicy =
    FyndPolicy { address: [0xfd; 20], min_fee: 1_000_000 };

/// Name of the static attribute that holds an exclusive Curve pool's Fynd policy.
pub(crate) const FYND_POLICY_ATTRIBUTE: &str = "fynd_policy";

/// Name of the static attribute that marks a component exclusive.
const IS_EXCLUSIVE_ATTRIBUTE: &str = "is_exclusive";

/// Returns `true` when the component offers exclusive liquidity, i.e. is swappable only with
/// off-chain authorization. Signaled by the `is_exclusive` static attribute; absence means the
/// component is public.
pub(crate) fn is_exclusive(component: &ProtocolComponent) -> bool {
    component
        .static_attributes
        .contains_key(IS_EXCLUSIVE_ATTRIBUTE)
}

/// Stamps a Curve pool's Fynd policy, or removes it with the pool's exclusivity.
pub(crate) fn set_fynd_policy(component: &mut ProtocolComponent, fynd_policy: Option<&FyndPolicy>) {
    set_exclusive(component, fynd_policy.is_some());
    match fynd_policy {
        Some(fynd_policy) => {
            component
                .static_attributes
                .insert(FYND_POLICY_ATTRIBUTE.to_string(), fynd_policy.address.to_vec().into());
        }
        None => {
            component
                .static_attributes
                .remove(FYND_POLICY_ATTRIBUTE);
        }
    }
}

/// Adds or removes the `is_exclusive` static attribute.
pub(crate) fn set_exclusive(component: &mut ProtocolComponent, exclusive: bool) {
    if exclusive {
        component
            .static_attributes
            .insert(IS_EXCLUSIVE_ATTRIBUTE.to_string(), vec![1u8].into());
    } else {
        component
            .static_attributes
            .remove(IS_EXCLUSIVE_ATTRIBUTE);
    }
}

/// Reads a Curve state and the Fynd policy it names. `None` when the state is not a Curve pool.
pub(crate) fn find_fynd_policy(
    state: &dyn ProtocolSim,
) -> Option<(&CurveState, Option<&'static FyndPolicy>)> {
    let curve = state
        .as_any()
        .downcast_ref::<CurveState>()?;
    let fynd_policy = curve.policy().and_then(|policy| {
        FYND_POLICIES
            .iter()
            .find(|fynd_policy| policy.as_ref() == fynd_policy.address.as_slice())
    });
    Some((curve, fynd_policy))
}

/// Reads the Fynd policy stamped on an exclusive Curve pool.
pub(crate) fn read_fynd_policy(component: &ProtocolComponent) -> Option<&'static FyndPolicy> {
    let address = component
        .static_attributes
        .get(FYND_POLICY_ATTRIBUTE)?;
    FYND_POLICIES
        .iter()
        .find(|fynd_policy| address.as_ref() == fynd_policy.address.as_slice())
}

/// Stamps the `is_exclusive` attribute onto a component's static attributes, so tests can build
/// exclusive components without protocol-specific fixtures.
#[cfg(test)]
pub(crate) fn mark_exclusive(component: &mut ProtocolComponent) {
    set_exclusive(component, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::{component, token};

    fn exclusive_component(id: &str) -> ProtocolComponent {
        let mut c = component(id, &[token(0x01, "A"), token(0x02, "B")]);
        mark_exclusive(&mut c);
        c
    }

    fn public_component(id: &str) -> ProtocolComponent {
        component(id, &[token(0x01, "A"), token(0x02, "B")])
    }

    fn component_with_extension() -> ProtocolComponent {
        let mut c = public_component("pub-1");
        c.static_attributes
            .insert("extension".to_string(), vec![0x55, 0x19].into());
        c
    }

    #[rstest::rstest]
    #[case::tagged(exclusive_component("excl-1"), true)]
    #[case::missing_attribute(public_component("pub-1"), false)]
    #[case::untagged_extension(component_with_extension(), false)]
    fn test_is_exclusive(#[case] component: ProtocolComponent, #[case] expected: bool) {
        assert_eq!(is_exclusive(&component), expected);
    }
}
