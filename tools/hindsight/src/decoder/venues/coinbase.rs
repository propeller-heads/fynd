//! Coinbase Wallet decoding.
//!
//! Coinbase Wallet's in-app swaps are 0x-powered ("aggregation is powered by 0x",
//! docs.cdp.coinbase.com): the app enters through its own proxy contracts and takes a fee in the
//! output token, sent to its fee wallet. Nets the sender's flow and backs that fee out through the
//! shared `venue_flow` — no venue-specific corrections.

use std::collections::HashSet;

use alloy::primitives::Address;
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    netting::venue_flow,
    registry::VenueAddresses,
};

/// Coinbase Wallet's decoders, constructed with the address-book fields they use (see
/// `venues::DECODERS`).
pub(crate) fn decoders(addresses: &VenueAddresses) -> Vec<Box<dyn TradeDecoder>> {
    vec![Box::new(CoinbaseNetting { fee_collectors: addresses.fee_collectors.clone() })]
}

/// Coinbase Wallet's netting decoder.
pub(crate) struct CoinbaseNetting {
    fee_collectors: HashSet<Address>,
}

#[async_trait]
impl TradeDecoder for CoinbaseNetting {
    fn name(&self) -> &'static str {
        "coinbase-netting"
    }

    /// Net the sender's flow and back the output-token fee out.
    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        venue_flow(ctx.transfer_ledger, ctx.receipt.from, ctx.entry_point, &self.fee_collectors)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, U256};

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, make_transfer_log, swap, venue_addresses, CtxFixture},
        transfer_ledger::TransferLedger,
    };

    fn fee_wallet(registry: &Registry) -> Address {
        *registry
            .venue("coinbase")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    async fn decode(
        registry: &Registry,
        ledger: &TransferLedger,
        sender: Address,
        entry_point: Address,
    ) -> Option<TraderFlow> {
        let decoder = CoinbaseNetting {
            fee_collectors: venue_addresses(registry, "coinbase").fee_collectors,
        };
        let mut fixture = CtxFixture::new(sender, entry_point);
        let mut ctx = fixture.ctx(registry, ledger, &[]);
        decoder.decode(&mut ctx).await
    }

    #[tokio::test]
    async fn test_output_token_fee_backed_out() {
        // The fee reaches the collector in the output token, so the shared back-out grosses it into
        // amount_out — else the settled output is under-reported and every comparison overcredits
        // Fynd.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let proxy = addr(2);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(1990)),
            make_transfer_log(token_out, pool, collector, U256::from(10)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&registry, &transfer_ledger, user, proxy)
            .await
            .unwrap();
        assert_eq!(flow.swap, swap(token_in, 1000, token_out, 2000));
        assert_eq!(flow.venue_fee_out, Some(U256::from(10)));
    }

    #[tokio::test]
    async fn test_fee_free_trade() {
        let registry = Registry::ethereum();
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&registry, &transfer_ledger, user, pool)
            .await
            .unwrap();
        assert_eq!(flow.swap, swap(token_in, 1000, token_out, 2000));
        assert_eq!(flow.venue_fee_out, None);
    }
}
