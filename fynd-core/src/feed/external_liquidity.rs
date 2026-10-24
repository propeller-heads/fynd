//! The types an external liquidity source speaks: what it is told when it starts and what it
//! publishes.

use std::{collections::HashMap, sync::Arc};

use tycho_simulation::{
    book::{BookSnapshot, ReceivedAt},
    protocol::models::ProtocolComponent,
    tycho_common::{
        models::{token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
    },
    tycho_core::Bytes,
};

/// What an external liquidity source needs to know to start: the chain, the tokens it may serve
/// and the TVL floor in USD. They are the same values the built-in RFQ venues get.
///
/// A source must not serve a component that has a token outside `tokens`.
#[derive(Clone)]
pub struct ExternalLiquidityConfig {
    /// The chain the deployment runs on.
    pub chain: Chain,
    /// The tokens a source may serve, by address.
    pub tokens: Arc<HashMap<Bytes, Token>>,
    /// The smallest TVL, in USD, a component should have to be worth serving.
    pub min_tvl_usd: f64,
}

impl std::fmt::Debug for ExternalLiquidityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalLiquidityConfig")
            .field("chain", &self.chain)
            .field("token_count", &self.tokens.len())
            .field("min_tvl_usd", &self.min_tvl_usd)
            .finish()
    }
}

/// One component a source serves and the state that prices it.
///
/// The state must quote on its own, without chain state.
#[derive(Clone, Debug)]
pub struct LiquidityComponent {
    /// The component, as the market and the encoder see it.
    pub component: ProtocolComponent,
    /// The state that prices swaps on the component.
    pub state: Arc<dyn ProtocolSim>,
}

/// Everything a source serves at one moment.
///
/// Each snapshot replaces the previous one entirely, so a component missing from a snapshot is
/// removed from the market.
#[derive(Clone, Debug, Default)]
pub struct LiquiditySnapshot {
    /// Keyed by component id, so each id appears once. Shared so that the copy each reader of
    /// the feed takes is cheap.
    components: Arc<HashMap<String, LiquidityComponent>>,
}

impl LiquiditySnapshot {
    /// A snapshot serving `components`. If two share a component id, the later one is kept.
    pub fn new(components: Vec<LiquidityComponent>) -> Self {
        let mut by_id = HashMap::with_capacity(components.len());
        for entry in components {
            by_id.insert(entry.component.id.to_string(), entry);
        }
        Self { components: Arc::new(by_id) }
    }

    /// The components the snapshot serves, in no particular order.
    pub fn components(&self) -> impl ExactSizeIterator<Item = &LiquidityComponent> {
        self.components.values()
    }
}

/// Reads a built-in venue's book set as a snapshot, taking each book's component and state.
pub(crate) fn liquidity_snapshot_from_books(
    snapshot: &BookSnapshot<ReceivedAt>,
) -> LiquiditySnapshot {
    let mut components = Vec::with_capacity(snapshot.books.len());
    for book in snapshot.books.values() {
        components.push(LiquidityComponent {
            component: book.component.clone(),
            state: Arc::clone(&book.state),
        });
    }
    LiquiditySnapshot::new(components)
}
