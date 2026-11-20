//! Curve pools whose `FyndPolicy` grants Fynd a lower fee.
//!
//! The exclusive view comes from `CurveState::exclusive_view` in tycho-simulation, which prices
//! the pool at the policy's minimum fee.

use std::sync::Arc;

use tycho_simulation::{
    evm::protocol::curve::CurveState,
    tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes},
};

use crate::{
    encoding::exclusive_swap::ExclusiveSwapSigner,
    exclusive_venue::{ExclusiveVenue, Grantor},
    SolveError, Swap,
};

/// A Curve pool is dual while its policy names Fynd's [`Grantor`] and is not killed.
pub(crate) struct CurveVenue;

impl ExclusiveVenue for CurveVenue {
    fn build_exclusive_view(&self, state: &dyn ProtocolSim) -> Option<Arc<dyn ProtocolSim>> {
        let curve = state
            .as_any()
            .downcast_ref::<CurveState>()?;
        let view = curve.exclusive_view()?;
        Some(Arc::new(view))
    }

    fn is_exclusive_view(&self, state: &dyn ProtocolSim) -> bool {
        find_exclusive_view(state).is_some()
    }

    fn can_grant(&self, state: &dyn ProtocolSim, grantor: &Grantor) -> bool {
        let Some(curve) = state
            .as_any()
            .downcast_ref::<CurveState>()
        else {
            return false;
        };
        let policy = curve.policy();
        policy.controller() == Some(grantor.controller()) &&
            policy.router() == Some(grantor.router()) &&
            !policy.is_killed()
    }

    fn sign(
        &self,
        signer: &ExclusiveSwapSigner,
        swap: &Swap,
        taker: &Bytes,
        slippage: f64,
    ) -> Result<Bytes, SolveError> {
        let curve = find_exclusive_view(swap.protocol_state()).ok_or_else(|| {
            SolveError::FailedEncoding(format!(
                "swap through {} is not priced with a Curve grant",
                swap.component_id()
            ))
        })?;
        signer.build_curve_user_data(swap, curve.policy(), taker, slippage)
    }
}

/// The Curve state of an exclusive view. `None` for any other state.
fn find_exclusive_view(state: &dyn ProtocolSim) -> Option<&CurveState> {
    state
        .as_any()
        .downcast_ref::<CurveState>()
        .filter(|curve| curve.is_exclusive_view())
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

    use crate::exclusive_venue::Grantor;

    /// The policy of a dual pool.
    pub(crate) const POLICY: [u8; 20] = [0xfd; 20];
    /// The key that signs grants in tests.
    pub(crate) const CONTROLLER: [u8; 20] = [0xc0; 20];
    /// The router that submits grants in tests.
    pub(crate) const ROUTER: [u8; 20] = [0xd0; 20];

    pub(crate) fn grantor() -> Grantor {
        Grantor::new(Bytes::from(CONTROLLER), Bytes::from(ROUTER))
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
    use crate::{algorithm::test_utils::MockProtocolSim, exclusive_venue::find_venue};

    #[rstest::rstest]
    #[case::dual(grant_policy(CONTROLLER, false), true)]
    #[case::other_controller(grant_policy([0xee; 20], false), false)]
    #[case::killed(grant_policy(CONTROLLER, true), false)]
    #[case::no_policy(CurvePolicy::default(), false)]
    fn test_can_grant(#[case] policy: CurvePolicy, #[case] expected: bool) {
        assert_eq!(CurveVenue.can_grant(&curve_state(policy), &grantor()), expected);
        assert!(!CurveVenue.can_grant(&MockProtocolSim::default(), &grantor()));
    }

    #[test]
    fn test_build_exclusive_view() {
        let view = CurveVenue
            .build_exclusive_view(&dual_curve_state())
            .expect("supported policy");

        assert!(find_venue(view.as_ref()).is_some());
        assert!(find_venue(&dual_curve_state()).is_none());
        assert!(CurveVenue
            .build_exclusive_view(&curve_state(CurvePolicy::default()))
            .is_none());
    }
}
