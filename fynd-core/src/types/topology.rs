//! A component's tokens and the swap directions it offers.

use std::sync::Arc;

use itertools::Itertools;
use tracing::warn;
use tycho_simulation::{
    rfq::protocols::component::{decode_swap_directions, SWAP_DIRECTIONS_ATTRIBUTE},
    tycho_common::models::{protocol::ProtocolComponent, Address},
};

/// One swap a component offers: token in, then token out.
pub type SwapDirection = (Address, Address);

/// A component's tokens and swap directions, as the graph and the derived data read them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComponentTopology {
    /// Every token the component holds.
    pub tokens: Vec<Address>,
    /// The swap directions the component offers. A clone shares the list and does not copy it.
    pub swap_directions: Arc<[SwapDirection]>,
}

/// A component with no `swap_directions` attribute: every direction between its tokens.
#[cfg(any(test, feature = "test-utils"))]
impl From<Vec<Address>> for ComponentTopology {
    fn from(tokens: Vec<Address>) -> Self {
        let swap_directions = every_swap_direction(&tokens);
        Self { tokens, swap_directions }
    }
}

/// The swap directions of a component without the attribute, in sorted order. Sorted tokens give
/// the same order in every process.
fn every_swap_direction(tokens: &[Address]) -> Arc<[SwapDirection]> {
    let mut directions = Vec::with_capacity(tokens.len() * tokens.len().saturating_sub(1));
    for pair in tokens
        .iter()
        .sorted()
        .dedup()
        .tuple_combinations()
    {
        let (first, second): (&Address, &Address) = pair;
        directions.push((first.clone(), second.clone()));
        directions.push((second.clone(), first.clone()));
    }
    directions.into()
}

/// The swap directions `component` lists in its attribute, less those naming a token it does not
/// hold. Without the attribute, every direction between its tokens. A bad attribute gives none.
pub(crate) fn swap_directions(component: &ProtocolComponent) -> Arc<[SwapDirection]> {
    let Some(attribute) = component
        .static_attributes
        .get(SWAP_DIRECTIONS_ATTRIBUTE)
    else {
        return every_swap_direction(&component.tokens);
    };
    match decode_swap_directions(attribute) {
        Ok(mut directions) => {
            directions.retain(|(token_in, token_out)| {
                component.tokens.contains(token_in) && component.tokens.contains(token_out)
            });
            directions.into()
        }
        Err(error) => {
            warn!(
                component_id = %component.id,
                %error,
                "malformed swap directions, the component gets no edges"
            );
            Arc::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::{addr, component, component_with_directions, token};

    #[test]
    fn test_swap_directions_reads_the_attribute() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let component = component_with_directions("venue", &[a.clone(), b.clone(), c], &[(&a, &b)]);

        assert_eq!(swap_directions(&component).to_vec(), vec![(addr(0x01), addr(0x02))]);
    }

    #[test]
    fn test_swap_directions_without_attribute_is_every_pair() {
        let component = component("pool", &[token(0x01, "A"), token(0x02, "B")]);

        assert_eq!(
            swap_directions(&component).to_vec(),
            vec![(addr(0x01), addr(0x02)), (addr(0x02), addr(0x01))]
        );
    }

    #[test]
    fn test_swap_directions_keeps_known_tokens_only() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let mut component = component_with_directions(
            "venue",
            &[a.clone(), b.clone(), c.clone()],
            &[(&a, &b), (&b, &c)],
        );
        component
            .tokens
            .retain(|token| token != &c.address);

        assert_eq!(swap_directions(&component).to_vec(), vec![(addr(0x01), addr(0x02))]);
    }
}
