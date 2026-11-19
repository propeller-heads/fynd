//! Concrete computation implementations.
//!
//! Each computation implements the `DerivedComputation` trait from the parent module
//! to produce derived data from market data. The ComputationManager calls `compute()`
//! when relevant market events occur.

pub mod component_depth;
pub mod spot_price;
pub mod token_gas_price;

use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use tycho_simulation::tycho_common::models::Address;

use crate::{
    feed::market_data::MarketState,
    types::{ComponentId, ComponentTopology},
};

/// Removes the entries for swap directions that the components in `added` no longer offer.
/// Without this, a dropped direction keeps its last entry.
pub(crate) fn drop_unlisted_directions<V>(
    entries: &mut hashbrown::HashMap<(ComponentId, Address, Address), V, FxBuildHasher>,
    added: &FxHashMap<ComponentId, ComponentTopology>,
) {
    let listed: FxHashMap<&ComponentId, FxHashSet<(&Address, &Address)>> = added
        .iter()
        .map(|(id, topology)| {
            let directions = topology
                .swap_directions
                .iter()
                .map(|(token_in, token_out)| (token_in, token_out))
                .collect();
            (id, directions)
        })
        .collect();
    entries.retain(|(id, token_in, token_out), _| {
        listed
            .get(id)
            .is_none_or(|directions| directions.contains(&(token_in, token_out)))
    });
}

/// The routable components of `market` that none of the `listed` ids names.
pub(crate) fn components_without_entries<'a>(
    market: &MarketState,
    listed: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let listed: FxHashSet<&str> = listed.collect();
    market
        .routable_component_ids()
        .filter(|id| !listed.contains(id.as_str()))
        .cloned()
        .collect()
}

pub use component_depth::ComponentDepthComputation;
pub use spot_price::SpotPriceComputation;
pub use token_gas_price::TokenGasPriceComputation;
