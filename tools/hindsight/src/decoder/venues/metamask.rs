//! MetaMask-specific decoding.
//!
//! MetaMask's Swap Router routes through a real venue and skims its fee (~87.5 bps, plus a gas
//! recoup on gasless "smart swaps") to a fee wallet — from the input token before swapping or
//! from the output after. Without backing that skim out, every comparison credits Fynd with
//! MetaMask's own fee: the skim is charged whichever router MetaMask plugs in, so it is not
//! value better routing can recover. On dust trades the skim dominates and fabricated
//! extreme "wins".

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
};

use crate::decoder::{
    registry::Registry,
    venues::{back_out_client_fees, sender_flow, Flow},
};

/// Decode a MetaMask-entered transaction: net the sender's flow, then back the
/// client fee out of it.
pub(crate) fn decode(
    logs: &[Log],
    native: &[(Address, Address, U256)],
    sender: Address,
    entry_point: Address,
    registry: &Registry,
) -> Option<Flow> {
    let flow = sender_flow(logs, native, sender, entry_point)?;
    // A fee wallet trading through the router is moving its own money; its receipts are its
    // output, not a skim to back out.
    if registry
        .metamask()
        .fee_collectors
        .contains(&flow.tracked)
    {
        return Some(flow);
    }
    Some(back_out_client_fees(flow, logs, native, &registry.metamask().fee_collectors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    /// One of the real MetaMask fee wallets, so tests exercise the registry entries.
    fn fee_wallet(registry: &Registry) -> Address {
        *registry
            .metamask()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    #[test]
    fn decode_backs_out_output_side_skim() {
        // Live tx 0x142de458… shape: token in, ETH out; the router skims the fee from the native
        // output before forwarding the rest to the trader. amount_out is grossed back up.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let router = registry.metamask().router;
        let pool = addr(50);
        let token_in = addr(10);

        let logs = vec![make_transfer_log(token_in, user, pool, U256::from(15_000_000))];
        let native = vec![
            (pool, router, U256::from(8_408)),
            (router, collector, U256::from(883)),
            (router, user, U256::from(7_525)),
        ];

        let flow = decode(&logs, &native, user, router, &registry).unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 15_000_000, Address::ZERO, 8_408));
        assert_eq!(flow.client_fee, None);
        assert_eq!(flow.client_fee_out, Some(U256::from(883)));
    }

    #[test]
    fn decode_backs_out_input_side_skim() {
        // ETH in, token out: the router skims the fee from the native input before forwarding
        // the rest to the venue. amount_in shrinks to what actually entered the swap.
        let registry = Registry::ethereum();
        let collector = fee_wallet(&registry);
        let user = addr(1);
        let router = registry.metamask().router;
        let pool = addr(50);
        let token_out = addr(11);

        let native = vec![
            (user, router, U256::from(1_000)),
            (router, collector, U256::from(9)),
            (router, pool, U256::from(991)),
        ];
        let logs = vec![make_transfer_log(token_out, pool, user, U256::from(2_000))];

        let flow = decode(&logs, &native, user, router, &registry).unwrap();
        assert_eq!(flow.swap, swap(Address::ZERO, 991, token_out, 2_000));
        assert_eq!(flow.client_fee, Some(U256::from(9)));
        assert_eq!(flow.client_fee_out, None);
    }

    #[test]
    fn decode_keeps_fee_free_trade_unchanged() {
        // Some pairs are genuinely fee-free; nothing reached a fee wallet, nothing is backed out.
        let registry = Registry::ethereum();
        let user = addr(1);
        let router = registry.metamask().router;
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1_000)),
            make_transfer_log(token_out, pool, user, U256::from(2_000)),
        ];

        let flow = decode(&logs, &[], user, router, &registry).unwrap();
        assert_eq!(flow.swap, swap(token_in, 1_000, token_out, 2_000));
        assert_eq!(flow.client_fee, None);
        assert_eq!(flow.client_fee_out, None);
    }
}
