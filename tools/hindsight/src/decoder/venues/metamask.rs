//! `MetaMask` decoding.
//!
//! `MetaMask`'s Swap Router routes through a real solver and takes its fee (~87.5 bps, plus a gas
//! recoup on gasless "smart swaps") to a fee wallet — from the input token before swapping or
//! from the output after. The fee is charged whichever router `MetaMask` plugs in, so it is not
//! value better routing can recover; without backing it out, every comparison credits Fynd with
//! `MetaMask`'s own fee, and on dust trades — where the fee dominates — that fabricates extreme
//! "wins".

use alloy::{sol, sol_types::SolCall};
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    netting::venue_flow,
    registry::VenueAddresses,
};

sol! {
    /// The `MetaMask` Swap Router entry point (selector `0x5f575529`): `aggregatorId` names the
    /// solver API that produced the route.
    function swap(string aggregatorId, address tokenFrom, uint256 amount, bytes data);
}

/// `MetaMask`'s decoders, constructed with its address-book section (see `venues::DECODERS`).
pub(crate) fn decoders(addresses: &VenueAddresses) -> Vec<Box<dyn TradeDecoder>> {
    vec![Box::new(MetaMaskNetting { addresses: addresses.clone() })]
}

/// `MetaMask`'s netting decoder. Holds the full section: it uses both the fee collectors and the
/// solver-alias table (`normalize_solver`).
pub(crate) struct MetaMaskNetting {
    addresses: VenueAddresses,
}

#[async_trait]
impl TradeDecoder for MetaMaskNetting {
    fn name(&self) -> &'static str {
        "metamask-netting"
    }

    /// Net the sender's flow, back the venue fee out of it, and attribute the solver from the
    /// router calldata.
    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        let mut flow = venue_flow(
            ctx.transfer_ledger,
            ctx.receipt.from,
            ctx.entry_point,
            &self.addresses.fee_collectors,
        )?;
        flow.solver_override = solver_from_calldata(ctx.input, &self.addresses);
        Some(flow)
    }
}

/// The solver label declared in the router calldata's `aggregatorId`, normalized to the address
/// book's solver names via the `[venues.metamask.solver_aliases]` section.
///
/// `MetaMask` states which solver API it routed through (e.g. "oneInchV6FeeDynamic",
/// "uniswapPermit2FeeDynamic"). Trace attribution often cannot resolve these — a token→token
/// route moves no native value and enters through Permit2 — so the calldata declaration is the
/// authoritative source.
fn solver_from_calldata(input: &[u8], metamask: &VenueAddresses) -> Option<String> {
    let call = swapCall::abi_decode(input).ok()?;
    Some(metamask.normalize_solver(&call.aggregatorId))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, Bytes, U256};

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, make_transfer_log, swap, venue_addresses, CtxFixture},
        transfer_ledger::TransferLedger,
    };

    fn metamask_addresses(registry: &Registry) -> &VenueAddresses {
        registry.venue("metamask").unwrap()
    }

    fn fee_wallet(registry: &Registry) -> Address {
        *metamask_addresses(registry)
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    fn router(registry: &Registry) -> Address {
        *metamask_addresses(registry)
            .entry_points
            .iter()
            .next()
            .unwrap()
    }

    /// Decode a `MetaMask` transaction through the full `MetaMaskNetting` decoder.
    async fn decode(
        registry: &Registry,
        ledger: &TransferLedger,
        sender: Address,
        entry_point: Address,
        input: &[u8],
    ) -> Option<TraderFlow> {
        let decoder = MetaMaskNetting { addresses: venue_addresses(registry, "metamask") };
        let mut fixture = CtxFixture::new(sender, entry_point);
        let mut ctx = fixture.ctx(registry, ledger, input);
        decoder.decode(&mut ctx).await
    }

    #[test]
    fn test_swap_selector_against_deployed_router() {
        // The sol! declaration must match the on-chain function (verified against live calldata).
        assert_eq!(swapCall::SELECTOR, [0x5f, 0x57, 0x55, 0x29]);
    }

    #[test]
    fn test_solver_from_calldata_known_ids() {
        let registry = Registry::ethereum();
        let metamask = metamask_addresses(&registry);
        for (id, want) in [
            ("oneInchV6FeeDynamic", "1inch"),
            ("uniswapPermit2FeeDynamic", "uniswap"),
            ("okx6", "okx"),
            ("someFutureSolver", "someFutureSolver"),
        ] {
            let call = swapCall {
                aggregatorId: id.to_string(),
                tokenFrom: addr(10),
                amount: U256::from(1000),
                data: Bytes::default(),
            };
            assert_eq!(solver_from_calldata(&call.abi_encode(), metamask).as_deref(), Some(want));
        }
    }

    #[test]
    fn test_solver_from_calldata_other_selectors() {
        let registry = Registry::ethereum();
        let metamask = metamask_addresses(&registry);
        assert_eq!(solver_from_calldata(&[0xde, 0xad, 0xbe, 0xef, 0x00], metamask), None);
        assert_eq!(solver_from_calldata(&[], metamask), None);
    }

    #[tokio::test]
    async fn test_output_side_fee() {
        // Live tx 0x142de458… shape: token in, ETH out; the router takes the fee from the native
        // output before forwarding the rest to the trader. amount_out is grossed back up.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let router = router(&registry);
        let pool = addr(50);
        let token_in = addr(10);

        let logs = vec![make_transfer_log(token_in, user, pool, U256::from(15_000_000))];
        let native = vec![
            (pool, router, U256::from(8_408)),
            (router, collector, U256::from(883)),
            (router, user, U256::from(7_525)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &native);

        let flow = decode(&registry, &transfer_ledger, user, router, &[])
            .await
            .unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 15_000_000, Address::ZERO, 8_408));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, Some(U256::from(883)));
    }

    #[tokio::test]
    async fn test_input_side_fee() {
        // ETH in, token out: the router takes the fee from the native input before forwarding
        // the rest to the solver. amount_in shrinks to what actually entered the swap.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let router = router(&registry);
        let pool = addr(50);
        let token_out = addr(11);

        let native = vec![
            (user, router, U256::from(1_000)),
            (router, collector, U256::from(9)),
            (router, pool, U256::from(991)),
        ];
        let logs = vec![make_transfer_log(token_out, pool, user, U256::from(2_000))];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &native);

        let flow = decode(&registry, &transfer_ledger, user, router, &[])
            .await
            .unwrap();
        assert_eq!(flow.swap, swap(Address::ZERO, 991, token_out, 2_000));
        assert_eq!(flow.venue_fee_in, Some(U256::from(9)));
        assert_eq!(flow.venue_fee_out, None);
    }

    #[tokio::test]
    async fn test_solver_declaration() {
        // The declared aggregatorId lands on the flow as the solver override, so the orchestrator
        // needs no MetaMask-specific attribution branch.
        let registry = Registry::ethereum();
        let user = addr(1);
        let pool = addr(50);
        let logs = vec![
            make_transfer_log(addr(10), user, pool, U256::from(1_000)),
            make_transfer_log(addr(11), pool, user, U256::from(2_000)),
        ];
        let call = swapCall {
            aggregatorId: "oneInchV6FeeDynamic".to_string(),
            tokenFrom: addr(10),
            amount: U256::from(1_000),
            data: Bytes::default(),
        };
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&registry, &transfer_ledger, user, router(&registry), &call.abi_encode())
            .await
            .unwrap();
        assert_eq!(flow.solver_override.as_deref(), Some("1inch"));
    }

    #[tokio::test]
    async fn test_fee_free_trade() {
        let registry = Registry::ethereum();
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1_000)),
            make_transfer_log(token_out, pool, user, U256::from(2_000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&registry, &transfer_ledger, user, router(&registry), &[])
            .await
            .unwrap();
        assert_eq!(flow.swap, swap(token_in, 1_000, token_out, 2_000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
    }
}
