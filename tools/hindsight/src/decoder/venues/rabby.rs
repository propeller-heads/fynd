//! Rabby decoding.
//!
//! Rabby is a consumer wallet with its own meta-aggregator: it picks among many solvers and takes
//! a flat 0.25% of the output token as its fee. Only its Uniswap-routed swaps enter through
//! Rabby's own `SwapProxy` contract; the rest go straight to the chosen solver's router, where
//! `tx.to` is that solver and the sole Rabby fingerprint is the fee transfer. Matching keys on the
//! entry point, so only the `SwapProxy` swaps are recognized as Rabby here — the shared-router
//! swaps decode as the solver's own trades with the 0.25% fee still inside the amounts.
//!
//! On a swap whose output is native ETH, Rabby unwraps the proceeds to the trader but keeps its
//! cut in WETH beforehand, so the fee reaches the collector denominated in the wrapped token
//! while the trade's output token is native ETH. The shared fee back-out matches the exact output
//! token and would miss that, so the wrapped-native fee is recognized here and grossed back into
//! the ETH output.

use alloy::primitives::Address;
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    netting_decoders::venue_flow,
    registry::VenueAddresses,
};

/// Rabby's decoders, constructed with its address-book section (see `venues::DECODERS`).
pub(crate) fn decoders(addresses: &VenueAddresses) -> Vec<Box<dyn TradeDecoder>> {
    vec![Box::new(RabbyNetting { addresses: addresses.clone() })]
}

/// Rabby's netting decoder.
pub(crate) struct RabbyNetting {
    addresses: VenueAddresses,
}

#[async_trait]
impl TradeDecoder for RabbyNetting {
    fn name(&self) -> &'static str {
        "rabby-netting"
    }

    /// Net the sender's flow and back the 0.25% fee out. A fee in the output token is handled by
    /// the shared `venue_flow`; a WETH fee on an ETH-output swap is grossed back in here.
    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        let mut flow = venue_flow(
            ctx.transfer_ledger,
            ctx.receipt.from,
            ctx.entry_point,
            &self.addresses.fee_collectors,
        )?;

        if flow.swap.token_out == Address::ZERO {
            let wrapped_fee = ctx
                .transfer_ledger
                .received_by(&self.addresses.fee_collectors)
                .get(&ctx.registry.wrapped_native())
                .copied()
                .filter(|fee| !fee.is_zero());
            if let Some(fee) = wrapped_fee {
                flow.gross_output_fee(fee);
            }
        }
        Some(flow)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, make_transfer_log, swap, venue_addresses, CtxFixture},
        transfer_ledger::TransferLedger,
    };

    fn fee_wallet(registry: &Registry) -> Address {
        *registry
            .venue("rabby")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    /// Decode a Rabby transaction through the full `RabbyNetting` decoder.
    async fn decode(
        registry: &Registry,
        ledger: &TransferLedger,
        sender: Address,
        entry_point: Address,
    ) -> Option<TraderFlow> {
        let decoder = RabbyNetting { addresses: venue_addresses(registry, "rabby") };
        let mut fixture = CtxFixture::new(sender, entry_point);
        let mut ctx = fixture.ctx(registry, ledger, &[]);
        decoder.decode(&mut ctx).await
    }

    #[tokio::test]
    async fn test_eth_output_wraps_fee_back_in() {
        // Live tx 0x96c81d9b… shape: USDC in, ETH out through the SwapProxy. Rabby takes its
        // 0.25% cut in WETH before unwrapping the rest to the trader, so the fee reaches the
        // collector as WETH while the output token is native ETH. It must be grossed back into
        // amount_out, else every ETH-output Rabby swap under-reports the settled output by 25 bps.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let usdc = addr(10);
        let weth = registry.wrapped_native();

        let logs = vec![
            make_transfer_log(usdc, user, pool, U256::from(4000)),
            make_transfer_log(weth, pool, router, U256::from(8000)),
            make_transfer_log(weth, router, collector, U256::from(20)),
        ];
        let native = vec![(router, user, U256::from(7980))];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &native);

        let flow = decode(&registry, &transfer_ledger, user, router)
            .await
            .unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(usdc, 4000, Address::ZERO, 8000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, Some(U256::from(20)));
    }

    #[tokio::test]
    async fn test_token_output_fee() {
        // Token-to-token swap: the fee reaches the collector in the output token itself, so the
        // shared back-out grosses it in and the Rabby wrapped-native branch stays out of the way.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(1995)),
            make_transfer_log(token_out, pool, collector, U256::from(5)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&registry, &transfer_ledger, user, router)
            .await
            .unwrap();
        assert_eq!(flow.swap, swap(token_in, 1000, token_out, 2000));
        assert_eq!(flow.venue_fee_out, Some(U256::from(5)));
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
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
    }
}
