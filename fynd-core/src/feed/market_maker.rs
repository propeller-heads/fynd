//! The market maker behind an RFQ component, where the venue names one.
//!
//! A solution fills against a named market maker at most once. Two fills against one maker are
//! two firm quotes to one counterparty, and the second is not priced by the first. Every
//! algorithm checks this beside its check that one pool is not taken twice.
//!
//! Only Hashflow names its maker at graph time. Bebop names it in the firm quote only, Liquorice
//! picks one of several per component at quote time, Native names none, and a Metric component is
//! one pool of one maker, which the component id already keeps apart.

use tycho_simulation::{
    rfq::protocols::hashflow::state::HashflowState,
    tycho_common::simulation::protocol_sim::ProtocolSim,
};

/// The name a venue gives one market maker. Names are per venue: one firm on two venues carries
/// two names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarketMaker(String);

impl From<&str> for MarketMaker {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

/// The market maker quoting `state`, when its venue names one.
///
/// The name is not part of the `ProtocolSim` contract, so it is read off the concrete state.
pub fn market_maker_of(state: &dyn ProtocolSim) -> Option<MarketMaker> {
    state
        .as_any()
        .downcast_ref::<HashflowState>()
        .map(|hashflow| MarketMaker::from(hashflow.market_maker.as_str()))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env};

    use tycho_simulation::{
        protocol::models::{DecoderContext, TryFromWithBlock},
        rfq::models::TimestampHeader,
        tycho_client::feed::synchronizer::ComponentWithState,
        tycho_common::models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain,
        },
    };

    use super::*;
    use crate::algorithm::test_utils::{token, MockProtocolSim};

    #[tokio::test]
    async fn test_market_maker_of_hashflow_state() {
        // The Hashflow decoder builds a client from these; the test makes no request.
        env::set_var("HASHFLOW_USER", "test");
        env::set_var("HASHFLOW_KEY", "test");
        let base = token(0x01, "A");
        let quote = token(0x02, "B");
        let component = ProtocolComponent {
            id: "hashflow_ab".to_string(),
            protocol_system: "rfq:hashflow".to_string(),
            chain: Chain::Ethereum,
            tokens: vec![base.address.clone(), quote.address.clone()],
            ..Default::default()
        };
        let attributes = HashMap::from([("mm".to_string(), b"mm1".to_vec().into())]);
        let snapshot = ComponentWithState {
            state: ProtocolComponentState::new("hashflow_ab", attributes, HashMap::new()),
            component,
            component_tvl: None,
            entrypoints: vec![],
        };
        let all_tokens = HashMap::from([
            (base.address.clone(), base.clone()),
            (quote.address.clone(), quote.clone()),
        ]);

        let state = HashflowState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 0 },
            &HashMap::new(),
            &all_tokens,
            &DecoderContext::default(),
        )
        .await
        .unwrap();

        assert_eq!(market_maker_of(&state), Some(MarketMaker::from("mm1")));
    }

    #[test]
    fn test_market_maker_of_on_chain_state() {
        assert_eq!(market_maker_of(&MockProtocolSim::new(1.0)), None);
    }
}
