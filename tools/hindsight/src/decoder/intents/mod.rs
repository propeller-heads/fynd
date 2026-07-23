//! Intent-role decoders: transactions a solver sends on the trader's behalf.
//!
//! Unlike a venue (entered by the trader, so the sender is the trader), an intent fill or batch
//! settlement is sent by a solver — the trader only signed an order. So these decoders find the
//! real trader inside the transaction rather than reading the sender's flow. This mirrors
//! `venues/`: one place lists the Intent role's decoders, tried in order.

pub(crate) mod cow;
pub(crate) mod netting;

use alloy::{
    primitives::{Address, B256},
    providers::Provider,
};

use crate::decoder::{decode::TradeDecoder, registry::Registry};

/// The decoders tried for the Intent role, first flow wins: a source with a rich signal (`CoW`'s
/// `Trade` event) is tried before the generic net-flow finder that works for any intent fill.
pub(crate) fn decoders_for<P: Provider>() -> Vec<Box<dyn TradeDecoder<P>>> {
    vec![Box::new(cow::CowSettlement), Box::new(netting::IntentNetting)]
}

/// The order-flow tag a batch settlement carries for client attribution: `CoW`'s per-order
/// `appData` hash, read from the settle calldata. `None` for entries that are not batch settlers
/// and for multi-order batches. Mirrors `solvers::integrator` — the orchestrator asks for a tag
/// without knowing which intent protocol produced it.
pub(crate) fn client_tag(registry: &Registry, entry_point: Address, input: &[u8]) -> Option<B256> {
    registry
        .is_batch_settler(entry_point)
        .then(|| cow::order_app_data(input))
        .flatten()
}
