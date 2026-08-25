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

use tycho_simulation::tycho_common::models::protocol::ProtocolComponent;

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
