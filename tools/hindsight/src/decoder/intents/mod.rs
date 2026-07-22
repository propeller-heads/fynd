//! Intent-role decoders: transactions a solver sends on the trader's behalf.
//!
//! Unlike a venue (entered by the trader, so the sender is the trader), an intent fill or batch
//! settlement is sent by a solver — the trader only signed an order. So these decoders find the
//! real trader inside the transaction rather than reading the sender's flow. This mirrors
//! `venues/`: one place lists the Intent role's decoders, tried in order.

pub(crate) mod cow;
pub(crate) mod netting;

use alloy::providers::Provider;

use crate::decoder::decode::TradeDecoder;

/// The decoders tried for the Intent role, first flow wins: a source with a rich signal (`CoW`'s
/// `Trade` event) is tried before the generic net-flow finder that works for any intent fill.
pub(crate) fn decoders_for<P: Provider>() -> Vec<Box<dyn TradeDecoder<P>>> {
    vec![Box::new(cow::CowSettlement), Box::new(netting::IntentNetting)]
}
