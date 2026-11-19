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
//! ingested component.
//!
//! A Curve pool with a grant policy is dual: public workers route through it at its own fee, and
//! exclusive workers through its exclusive view, at the policy's minimum fee. The pool is dual
//! while its policy names Fynd's [`CurveGrantor`] and is not killed.

use std::sync::Arc;

use tycho_simulation::{
    evm::protocol::curve::CurveState,
    tycho_common::{
        models::protocol::ProtocolComponent, simulation::protocol_sim::ProtocolSim, Bytes,
    },
};

/// The key that signs Fynd's Curve grants and the router that submits them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurveGrantor {
    controller: Bytes,
    router: Bytes,
}

impl CurveGrantor {
    pub(crate) fn new(controller: Bytes, router: Bytes) -> Self {
        Self { controller, router }
    }

    /// Whether Fynd can sign a grant for the pool of `state`: its policy names this controller and
    /// router, and is not killed.
    pub(crate) fn can_grant(&self, state: &dyn ProtocolSim) -> bool {
        let Some(curve) = state
            .as_any()
            .downcast_ref::<CurveState>()
        else {
            return false;
        };
        let policy = curve.policy();
        policy.controller() == Some(&self.controller) &&
            policy.router() == Some(&self.router) &&
            !policy.is_killed()
    }
}

/// Builds the exclusive view of a Curve pool's state. `None` for other states, and for a policy
/// that the simulation cannot price.
pub(crate) fn build_exclusive_view(state: &dyn ProtocolSim) -> Option<Arc<dyn ProtocolSim>> {
    let curve = state
        .as_any()
        .downcast_ref::<CurveState>()?;
    let view = curve.exclusive_view()?;
    Some(Arc::new(view))
}

/// The Curve state of a swap priced with a grant. `None` for any other state.
pub(crate) fn find_exclusive_curve_view(state: &dyn ProtocolSim) -> Option<&CurveState> {
    state
        .as_any()
        .downcast_ref::<CurveState>()
        .filter(|curve| curve.is_exclusive_view())
}

/// Returns `true` when the component offers exclusive liquidity, i.e. is swappable only with
/// off-chain authorization. Signaled by the `is_exclusive` static attribute; absence means the
/// component is public.
pub(crate) fn is_exclusive(component: &ProtocolComponent) -> bool {
    component
        .static_attributes
        .contains_key("is_exclusive")
}

/// Stamps the `is_exclusive` attribute onto a component's static attributes, so tests can build
/// exclusive components without protocol-specific fixtures.
#[cfg(test)]
pub(crate) fn mark_exclusive(component: &mut ProtocolComponent) {
    component
        .static_attributes
        .insert("is_exclusive".to_string(), vec![1u8].into());
}

/// Curve states with a grant policy, for tests.
#[cfg(test)]
pub(crate) mod test_curve {
    use tycho_simulation::{
        evm::protocol::curve::{
            adapter::{build_pool, CurveVariant, RawPoolState},
            CurvePolicy, CurveState, POLICY_ATTRIBUTE, POLICY_CONTROLLER_ATTRIBUTE,
            POLICY_KILLED_ATTRIBUTE, POLICY_MIN_FEE_ATTRIBUTE, POLICY_ROUTER_ATTRIBUTE,
            POLICY_VERSION_ATTRIBUTE,
        },
        tycho_common::Bytes,
    };

    use super::CurveGrantor;

    /// The policy of a dual pool.
    pub(crate) const POLICY: [u8; 20] = [0xfd; 20];
    /// The key that signs grants in tests.
    pub(crate) const CONTROLLER: [u8; 20] = [0xc0; 20];
    /// The router that submits grants in tests.
    pub(crate) const ROUTER: [u8; 20] = [0xd0; 20];

    pub(crate) fn grantor() -> CurveGrantor {
        CurveGrantor::new(Bytes::from(CONTROLLER), Bytes::from(ROUTER))
    }

    /// A grant policy at version 1 with a 1 bp minimum fee, signed by `controller`.
    pub(crate) fn grant_policy(controller: [u8; 20], is_killed: bool) -> CurvePolicy {
        let attributes = [
            (POLICY_ATTRIBUTE, POLICY.to_vec()),
            (POLICY_VERSION_ATTRIBUTE, vec![1]),
            (POLICY_CONTROLLER_ATTRIBUTE, controller.to_vec()),
            (POLICY_ROUTER_ATTRIBUTE, ROUTER.to_vec()),
            (POLICY_KILLED_ATTRIBUTE, vec![u8::from(is_killed)]),
            (POLICY_MIN_FEE_ATTRIBUTE, 1_000_000u64.to_be_bytes().to_vec()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), Bytes::from(value)))
        .collect();
        CurvePolicy::from_attributes(&attributes).expect("policy")
    }

    /// A TriCrypto pool of the coins `0x01..`, `0x02..` and `0x03..`, with `policy`.
    pub(crate) fn curve_state(policy: CurvePolicy) -> CurveState {
        let parse = |value: &str| value.parse().expect("u256");
        let raw = RawPoolState {
            variant: CurveVariant::TriCryptoNG,
            balances: vec![
                parse("2466241139205"),
                parse("4200057336"),
                parse("1595469030050811720465"),
            ],
            token_decimals: vec![6, 8, 18],
            amp: parse("1707629"),
            mid_fee: Some(parse("3000000")),
            out_fee: Some(parse("30000000")),
            fee_gamma: Some(parse("500000000000000")),
            d: Some(parse("7457948167729606869978625")),
            gamma: Some(parse("11809167828997")),
            price_scale: Some(vec![
                parse("59372627314351316239076"),
                parse("1565715369034455123313"),
            ]),
            ..Default::default()
        };
        CurveState::new(
            Bytes::from([7u8; 20]),
            vec![Bytes::from([1u8; 20]), Bytes::from([2u8; 20]), Bytes::from([3u8; 20])],
            vec![6, 8, 18],
            CurveVariant::TriCryptoNG,
            build_pool(&raw).expect("build curve pool"),
            None,
        )
        .with_policy(policy)
    }

    /// A dual pool whose policy names [`grantor`].
    pub(crate) fn dual_curve_state() -> CurveState {
        curve_state(grant_policy(CONTROLLER, false))
    }
}

#[cfg(test)]
mod tests {
    use tycho_simulation::evm::protocol::curve::CurvePolicy;

    use super::{test_curve::*, *};
    use crate::algorithm::test_utils::{component, token, MockProtocolSim};

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

    #[rstest::rstest]
    #[case::dual(grant_policy(CONTROLLER, false), true)]
    #[case::other_controller(grant_policy([0xee; 20], false), false)]
    #[case::killed(grant_policy(CONTROLLER, true), false)]
    #[case::no_policy(CurvePolicy::default(), false)]
    fn test_can_grant(#[case] policy: CurvePolicy, #[case] expected: bool) {
        assert_eq!(grantor().can_grant(&curve_state(policy)), expected);
        assert!(!grantor().can_grant(&MockProtocolSim::default()));
    }

    #[test]
    fn test_build_exclusive_view() {
        let view = build_exclusive_view(&dual_curve_state()).expect("supported policy");

        assert!(find_exclusive_curve_view(view.as_ref()).is_some());
        assert!(find_exclusive_curve_view(&dual_curve_state()).is_none());
        assert!(build_exclusive_view(&curve_state(CurvePolicy::default())).is_none());
    }
}
