//! MetaMask-specific decoding.
//!
//! MetaMask's Swap Router routes through a real venue and skims its fee (~87.5 bps, plus a gas
//! recoup on gasless "smart swaps") to a fee wallet — from the input token before swapping or
//! from the output after. Without backing that skim out, every comparison credits Fynd with
//! MetaMask's own fee: the skim is charged whichever router MetaMask plugs in, so it is not
//! value better routing can recover. On dust trades the skim dominates and fabricated
//! extreme "wins".

use alloy::{primitives::Address, sol, sol_types::SolCall};

use crate::decoder::{
    ledger::TransferLedger,
    registry::Registry,
    venues::{client_fee_flow, Flow},
};

sol! {
    /// The MetaMask Swap Router entry point (selector `0x5f575529`): `aggregatorId` names the
    /// aggregator API that produced the route.
    function swap(string aggregatorId, address tokenFrom, uint256 amount, bytes data);
}

/// The venue label declared in the router calldata's `aggregatorId`, normalized to the
/// registry's venue names.
///
/// MetaMask states which aggregator API it routed through (e.g. "oneInchV6FeeDynamic",
/// "uniswapPermit2FeeDynamic"). Trace attribution often cannot resolve these — a token→token
/// route moves no native value and enters through Permit2 — so the calldata declaration is the
/// authoritative source. Unrecognized ids pass through as-is: "airswapV4" is still more
/// informative than a raw executor address.
fn aggregator_from_calldata(input: &[u8]) -> Option<String> {
    let call = swapCall::abi_decode(input).ok()?;
    Some(normalize(&call.aggregatorId))
}

fn normalize(id: &str) -> String {
    let lower = id.to_lowercase();
    let names = [
        ("oneinch", "1inch"),
        ("zeroex", "0x"),
        ("uniswap", "uniswap"),
        ("okx", "okx"),
        ("kyber", "kyberswap"),
        ("paraswap", "paraswap"),
        ("airswap", "airswap"),
        ("openocean", "openocean"),
        ("hashflow", "hashflow"),
    ];
    for (needle, name) in names {
        if lower.contains(needle) {
            return name.to_string();
        }
    }
    id.to_string()
}

/// Decode a MetaMask-entered transaction: net the sender's flow, back the client fee out of it,
/// and attribute the venue from the router calldata (`input`).
pub(crate) fn decode(
    ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
    input: &[u8],
    registry: &Registry,
) -> Option<Flow> {
    let metamask = registry.client("metamask")?;
    let mut flow = client_fee_flow(ledger, sender, entry_point, &metamask.fee_collectors)?;
    flow.aggregator_override = aggregator_from_calldata(input);
    Some(flow)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    /// One of the real MetaMask fee wallets, so tests exercise the registry entries.
    fn fee_wallet(registry: &Registry) -> Address {
        *registry
            .client("metamask")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    /// The real MetaMask Swap Router entry point.
    fn router(registry: &Registry) -> Address {
        *registry
            .client("metamask")
            .unwrap()
            .entry_points
            .iter()
            .next()
            .unwrap()
    }

    #[test]
    fn swap_selector_matches_deployed_router() {
        // The sol! declaration must match the on-chain function (verified against live calldata).
        assert_eq!(swapCall::SELECTOR, [0x5f, 0x57, 0x55, 0x29]);
    }

    #[test]
    fn aggregator_from_calldata_normalizes_known_ids() {
        for (id, want) in [
            ("oneInchV6FeeDynamic", "1inch"),
            ("uniswapPermit2FeeDynamic", "uniswap"),
            ("okx6", "okx"),
            ("someFutureAggregator", "someFutureAggregator"),
        ] {
            let call = swapCall {
                aggregatorId: id.to_string(),
                tokenFrom: addr(10),
                amount: U256::from(1000),
                data: Default::default(),
            };
            assert_eq!(aggregator_from_calldata(&call.abi_encode()).as_deref(), Some(want));
        }
    }

    #[test]
    fn aggregator_from_calldata_declines_other_selectors() {
        assert_eq!(aggregator_from_calldata(&[0xde, 0xad, 0xbe, 0xef, 0x00]), None);
        assert_eq!(aggregator_from_calldata(&[]), None);
    }

    #[test]
    fn decode_backs_out_output_side_skim() {
        // Live tx 0x142de458… shape: token in, ETH out; the router skims the fee from the native
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
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let flow = decode(&ledger, user, router, &[], &registry).unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 15_000_000, Address::ZERO, 8_408));
        assert_eq!(flow.client_fee, None);
        assert_eq!(flow.client_fee_out, Some(U256::from(883)));
        assert!(flow.trader_paid_gas);
    }

    #[test]
    fn decode_backs_out_input_side_skim() {
        // ETH in, token out: the router skims the fee from the native input before forwarding
        // the rest to the venue. amount_in shrinks to what actually entered the swap.
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
        let ledger = TransferLedger::from_transaction(&logs, &native);

        let flow = decode(&ledger, user, router, &[], &registry).unwrap();
        assert_eq!(flow.swap, swap(Address::ZERO, 991, token_out, 2_000));
        assert_eq!(flow.client_fee, Some(U256::from(9)));
        assert_eq!(flow.client_fee_out, None);
    }

    #[test]
    fn decode_asserts_venue_from_calldata() {
        // The declared aggregatorId lands on the flow as the venue override, so the orchestrator
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
            data: Default::default(),
        };
        let ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&ledger, user, router(&registry), &call.abi_encode(), &registry).unwrap();
        assert_eq!(flow.aggregator_override.as_deref(), Some("1inch"));
    }

    #[test]
    fn decode_keeps_fee_free_trade_unchanged() {
        // Some pairs are genuinely fee-free; nothing reached a fee wallet, nothing is backed out.
        let registry = Registry::ethereum();
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1_000)),
            make_transfer_log(token_out, pool, user, U256::from(2_000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = decode(&ledger, user, router(&registry), &[], &registry).unwrap();
        assert_eq!(flow.swap, swap(token_in, 1_000, token_out, 2_000));
        assert_eq!(flow.client_fee, None);
        assert_eq!(flow.client_fee_out, None);
    }
}
