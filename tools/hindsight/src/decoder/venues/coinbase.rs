//! Coinbase Wallet decoding.
//!
//! Coinbase Wallet's in-app swaps are 0x-powered ("aggregation is powered by 0x",
//! docs.cdp.coinbase.com): the app enters through its own proxy contracts and takes a fee in the
//! output token, sent to its fee wallet. Nets the sender's flow and backs that fee out through the
//! shared `venue_flow` — no venue-specific corrections.

use alloy::providers::Provider;
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    netting_decoders::venue_flow,
};

/// Coinbase Wallet's netting decoder.
pub(crate) struct CoinbaseNetting;

#[async_trait]
impl<P: Provider> TradeDecoder<P> for CoinbaseNetting {
    fn name(&self) -> &'static str {
        "coinbase-netting"
    }

    /// Net the sender's flow and back the output-token fee out.
    async fn decode(&self, ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow> {
        let addresses = ctx.venue?;
        venue_flow(
            ctx.transfer_ledger,
            ctx.receipt.from,
            ctx.entry_point,
            &addresses.fee_collectors,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::{
        primitives::{Address, U256},
        providers::RootProvider,
        rpc::client::RpcClient,
        transports::mock::Asserter,
    };

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, make_transfer_log, receipt, swap, tx_hash},
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
        let provider = RootProvider::new(RpcClient::mocked(Asserter::new()));
        let mut code_cache = HashMap::new();
        let receipt = receipt(tx_hash(1), sender, Some(entry_point), vec![]);
        let mut ctx = DecodeContext {
            provider: &provider,
            registry,
            code_cache: &mut code_cache,
            receipt: &receipt,
            entry_point,
            transfer_ledger: ledger,
            input: &[],
            venue: registry.venue("coinbase"),
        };
        CoinbaseNetting.decode(&mut ctx).await
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
