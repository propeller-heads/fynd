//! Rainbow decoding.
//!
//! Rainbow is a consumer wallet with its own router (`0x0000…10e2`, the same address on every
//! chain it supports). It wraps 0x and takes its fee on the input side, passed explicitly as the
//! call's `feeAmount` argument and kept by the router — there is no fee transfer to observe, so the
//! fee is read from the calldata.
//!
//! Only the ETH→token entry (`fillQuoteEthToToken`) is decoded: its `feeAmount` is an absolute
//! amount of the input ETH (verified on-chain, tx 0xe09cf895…). The token→ETH and token→token
//! entries encode their cut as a basis-point rate instead (see `rainbow-me/swaps`), so they are
//! declined until that is verified — a declined trade is a coverage gap, never a mis-priced record.

use std::collections::HashSet;

use alloy::{primitives::U256, sol, sol_types::SolCall};
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    netting_decoders::venue_flow,
    registry::VenueAddresses,
};

sol! {
    /// Rainbow's ETH→token entry; `feeAmount` is the input-side fee the router keeps.
    function fillQuoteEthToToken(address buyToken, address to, bytes data, uint256 feeAmount);
}

/// Rainbow's decoders (see `venues::DECODERS`). Rainbow keeps its fee in the router — there is
/// no fee-collector state to hold, so the constructor takes no addresses.
pub(crate) fn decoders(_addresses: &VenueAddresses) -> Vec<Box<dyn TradeDecoder>> {
    vec![Box::new(RainbowCalldata)]
}

/// Rainbow's calldata decoder.
pub(crate) struct RainbowCalldata;

#[async_trait]
impl TradeDecoder for RainbowCalldata {
    fn name(&self) -> &'static str {
        "rainbow-calldata"
    }

    /// Net the sender's flow, then subtract the input-side fee read from the calldata so the
    /// amount that entered the swap is comparable to a re-solve. Declines any non-ETH→token call.
    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        let fee = eth_to_token_fee(ctx.input)?;
        // The router keeps no fee transfer, so there is nothing for `venue_flow` to back out; it
        // just nets the sender. The input-side fee is applied here.
        let mut flow =
            venue_flow(ctx.transfer_ledger, ctx.receipt.from, ctx.entry_point, &HashSet::new())?;
        flow.venue_fee_in = Some(fee);
        flow.swap.amount_in = flow.swap.amount_in.saturating_sub(fee);
        Some(flow)
    }
}

/// The input-side fee of a `fillQuoteEthToToken` call, or `None` for any other selector or a
/// malformed input.
fn eth_to_token_fee(input: &[u8]) -> Option<U256> {
    fillQuoteEthToTokenCall::abi_decode(input)
        .ok()
        .map(|call| call.feeAmount)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, U256};

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, make_transfer_log, swap, CtxFixture},
        transfer_ledger::TransferLedger,
    };

    /// `fillQuoteEthToToken` calldata carrying `fee` as its `feeAmount` argument.
    fn eth_to_token_calldata(fee: u64) -> Vec<u8> {
        fillQuoteEthToTokenCall {
            buyToken: Address::ZERO,
            to: Address::ZERO,
            data: alloy::primitives::Bytes::default(),
            feeAmount: U256::from(fee),
        }
        .abi_encode()
    }

    async fn decode(
        input: &[u8],
        ledger: &TransferLedger,
        entry_point: Address,
    ) -> Option<TraderFlow> {
        let registry = Registry::ethereum();
        let mut fixture = CtxFixture::new(addr(1), entry_point);
        let mut ctx = fixture.ctx(&registry, ledger, input);
        RainbowCalldata.decode(&mut ctx).await
    }

    #[tokio::test]
    async fn test_eth_to_token_subtracts_input_fee() {
        // ETH in, token out through the Rainbow router: the fee is part of the ETH the user sent
        // but never entered the swap, so amount_in must drop by it (else Fynd is handed the fee).
        let user = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let token_out = addr(11);
        let native = vec![(user, router, U256::from(18_500))];
        let logs = vec![make_transfer_log(token_out, pool, user, U256::from(34_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let flow = decode(&eth_to_token_calldata(157), &ledger, router)
            .await
            .unwrap();
        assert_eq!(flow.swap, swap(Address::ZERO, 18_500 - 157, token_out, 34_000));
        assert_eq!(flow.venue_fee_in, Some(U256::from(157)));
        assert_eq!(flow.venue_fee_out, None);
    }

    #[tokio::test]
    async fn test_other_selector_declined() {
        // A non-ETH→token call (here an empty/unknown selector) is declined rather than decoded
        // without its fee.
        let user = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let token_out = addr(11);
        let native = vec![(user, router, U256::from(18_500))];
        let logs = vec![make_transfer_log(token_out, pool, user, U256::from(34_000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);

        assert!(decode(&[0xde, 0xad, 0xbe, 0xef], &ledger, router)
            .await
            .is_none());
    }
}
