//! Reads the maker wallet balances from the node into the market state.

use std::time::{Duration, Instant};

use alloy::{
    network::Ethereum,
    primitives::U256,
    providers::{ProviderBuilder, RootProvider},
    sol,
    sol_types::SolCall,
};
use futures::future::join_all;
use metrics::counter;
use num_bigint::BigUint;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::time::{interval, timeout, MissedTickBehavior};
use tracing::warn;

use crate::{
    fallback::is_native_token, feed::market_data::MarketData, maker_wallet::WalletToken, rpc,
};

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Keeps the maker wallet balances of the market state up to date from the node.
///
/// It reads every token that a pool of a maker wallet pays, every refresh interval. A failed read
/// keeps the last balance of that token until it is too old to use.
pub(crate) struct MakerWalletFetcher {
    provider: RootProvider<Ethereum>,
    market_data: MarketData,
    refresh_interval: Duration,
}

impl MakerWalletFetcher {
    /// A fetcher that reads through the node at `rpc_url`.
    ///
    /// # Errors
    ///
    /// Returns a message when `rpc_url` is not a URL.
    pub(crate) fn new(
        rpc_url: &str,
        market_data: MarketData,
        refresh_interval: Duration,
    ) -> Result<Self, String> {
        let url = rpc_url
            .parse()
            .map_err(|e| format!("invalid RPC URL {rpc_url:?} for maker wallets: {e}"))?;
        Ok(Self {
            provider: ProviderBuilder::default().connect_http(url),
            market_data,
            refresh_interval,
        })
    }

    /// Reads the balances now, then on every refresh tick.
    pub(crate) async fn run(&self) {
        let mut ticker = interval(self.refresh_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let wallet_tokens = self.wallet_tokens().await;
            let read_at = Instant::now();
            let balances = self.read_balances(wallet_tokens).await;
            self.market_data
                .write()
                .await
                .update_maker_wallet_balances(balances, read_at);
        }
    }

    /// Every token that a pool of a maker wallet pays, with its wallet.
    async fn wallet_tokens(&self) -> FxHashSet<WalletToken> {
        let market = self.market_data.read().await;
        let market = market.base_market_state();
        let mut wallet_tokens = FxHashSet::default();
        for (protocol_system, wallet) in market.maker_wallets().wallets() {
            for component_id in market.components_by_protocol(protocol_system) {
                let Some(component) = market.get_component(component_id) else {
                    continue;
                };
                // The fallback router moves only ERC-20 tokens, so a pAMM leg never pays out ETH.
                for token in component
                    .tokens
                    .iter()
                    .filter(|token| !is_native_token(token))
                {
                    wallet_tokens
                        .insert(WalletToken { wallet: wallet.clone(), token: token.clone() });
                }
            }
        }
        wallet_tokens
    }

    /// Reads the balance of every wallet and token pair at once. Leaves out the ones the node could
    /// not read within the refresh interval.
    async fn read_balances(
        &self,
        wallet_tokens: FxHashSet<WalletToken>,
    ) -> FxHashMap<WalletToken, BigUint> {
        let reads = wallet_tokens
            .into_iter()
            .map(|wallet_token| async move {
                let balance =
                    match timeout(self.refresh_interval, self.read_balance(&wallet_token)).await {
                        Ok(balance) => balance,
                        Err(_) => Err(format!("no answer within {:?}", self.refresh_interval)),
                    };
                (wallet_token, balance)
            });
        let mut balances = FxHashMap::default();
        for (wallet_token, balance) in join_all(reads).await {
            match balance {
                Ok(balance) => {
                    balances.insert(wallet_token, balance);
                }
                Err(error) => {
                    counter!("maker_wallet_balance_fetch_failures_total").increment(1);
                    warn!(
                        wallet = %wallet_token.wallet,
                        token = %wallet_token.token,
                        %error,
                        "could not read a maker wallet balance; keeping the last one read"
                    );
                }
            }
        }
        balances
    }

    async fn read_balance(&self, wallet_token: &WalletToken) -> Result<BigUint, String> {
        let token = rpc::to_address(&wallet_token.token, "token")?;
        let wallet = rpc::to_address(&wallet_token.wallet, "maker wallet")?;
        let calldata = IERC20::balanceOfCall { account: wallet }.abi_encode();
        let balance: U256 =
            rpc::eth_call::<IERC20::balanceOfCall>(&self.provider, token, calldata).await?;
        Ok(BigUint::from_bytes_be(&balance.to_be_bytes::<32>()))
    }
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::Bytes as AlloyBytes, rpc::client::RpcClient, transports::mock::Asserter,
    };
    use tycho_simulation::tycho_common::models::Address;

    use super::*;

    fn fetcher_with(asserter: &Asserter) -> MakerWalletFetcher {
        MakerWalletFetcher {
            provider: RootProvider::new(RpcClient::mocked(asserter.clone())),
            market_data: MarketData::new_shared(),
            refresh_interval: Duration::from_secs(2),
        }
    }

    fn wallet_token() -> WalletToken {
        WalletToken { wallet: Address::from(vec![0xAA; 20]), token: Address::from(vec![0x01; 20]) }
    }

    #[tokio::test]
    async fn test_read_balances() {
        let asserter = Asserter::new();
        let encoded = IERC20::balanceOfCall::abi_encode_returns(&U256::from(1_234u32));
        asserter.push_success(&AlloyBytes::from(encoded));

        let balances = fetcher_with(&asserter)
            .read_balances(FxHashSet::from_iter([wallet_token()]))
            .await;

        assert_eq!(balances.get(&wallet_token()), Some(&BigUint::from(1_234u32)));
    }

    #[tokio::test]
    async fn test_read_balances_node_failure() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("node unavailable");

        let balances = fetcher_with(&asserter)
            .read_balances(FxHashSet::from_iter([wallet_token()]))
            .await;

        assert!(balances.is_empty());
    }
}
