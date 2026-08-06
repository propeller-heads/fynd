//! Background task that mirrors the PropAMMRouter's fee tiers into `SharedFallbackFees`.
//!
//! The router picks the Uniswap V3 pool it falls back to with
//! `resolvedFee(tokenIn, tokenOut)`: the per-pair override if one is set, else the global
//! `fallbackFee`. Governance can change both without a contract upgrade, so the values are read
//! from chain rather than hardcoded.
//!
//! Reads are rare and small. Only the pairs the pAMM venues quote can reach the fallback, so each
//! refresh costs one `fallbackFee` call, one `getPairs` call per venue, and one `getPairFee` call
//! per pair. A failed refresh keeps the previous tiers.

use std::{collections::HashSet, time::Duration};

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};
use tycho_simulation::tycho_common::{models::Address as TychoAddress, Bytes};

use crate::propamm_fallback::{FallbackFees, SharedFallbackFees};

sol! {
    interface IPropAMMRouter {
        function fallbackFee() external view returns (uint24);
        function getPairFee(address tokenA, address tokenB) external view returns (uint24);
    }

    interface IPropAMM {
        struct TokenPair {
            address token0;
            address token1;
        }
        function getPairs() external view returns (TokenPair[] memory pairs);
    }
}

/// Error reading the fee tiers from chain.
#[derive(Debug, thiserror::Error)]
pub enum FallbackFeeFetchError {
    /// The fetcher could not be constructed from the given configuration.
    #[error("invalid fallback fee fetcher configuration: {0}")]
    Config(String),
    /// An `eth_call` failed or returned undecodable data.
    #[error("{method} call to {contract} failed: {reason}")]
    Call {
        /// Contract method that failed.
        method: &'static str,
        /// Contract the call was sent to.
        contract: Address,
        /// Underlying transport or ABI decoding error.
        reason: String,
    },
}

/// Periodically refreshes `SharedFallbackFees` from the PropAMMRouter.
pub struct FallbackFeeFetcher {
    provider: RootProvider<Ethereum>,
    router_address: Address,
    venues: Vec<Address>,
    shared_fees: SharedFallbackFees,
    refresh_interval: Duration,
}

impl FallbackFeeFetcher {
    /// Creates a fetcher reading `router_address` and the pairs of `venues` via the node at
    /// `rpc_url`.
    ///
    /// # Errors
    ///
    /// Returns `FallbackFeeFetchError::Config` if `rpc_url` is not a valid URL, or if any
    /// address is not 20 bytes.
    pub fn new(
        rpc_url: &str,
        router_address: &Bytes,
        venues: &[Bytes],
        shared_fees: SharedFallbackFees,
        refresh_interval: Duration,
    ) -> Result<Self, FallbackFeeFetchError> {
        let url = rpc_url.parse().map_err(|e| {
            FallbackFeeFetchError::Config(format!("invalid RPC URL {rpc_url:?}: {e}"))
        })?;
        Ok(Self {
            provider: ProviderBuilder::default().connect_http(url),
            router_address: to_address(router_address, "router address")?,
            venues: venues
                .iter()
                .map(|venue| to_address(venue, "venue address"))
                .collect::<Result<_, _>>()?,
            shared_fees,
            refresh_interval,
        })
    }

    /// Runs the refresh loop: fetches immediately, then on every `refresh_interval` tick.
    ///
    /// Fetch failures are logged. The previously stored tiers stay in effect until a fetch
    /// succeeds, so a node outage cannot leave the encoder without fee tiers.
    pub async fn run(&self) {
        let mut ticker = interval(self.refresh_interval);
        // Skip missed ticks rather than catching up — fetches are best-effort.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            match self.fetch_fees().await {
                Ok(fees) => {
                    info!(
                        pair_overrides = fees.per_pair.len(),
                        "PropAMMRouter fee tiers refreshed"
                    );
                    self.shared_fees.set(fees);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "failed to refresh PropAMMRouter fee tiers; keeping previous values"
                    );
                }
            }
        }
    }

    /// Reads the global `fallbackFee` and the per-pair override for every pair the venues quote.
    ///
    /// A venue whose `getPairs` call fails is skipped, so one unreachable venue does not discard
    /// the tiers of the others. A failed `fallbackFee` read aborts the whole refresh, because
    /// every pair without an override resolves through it.
    async fn fetch_fees(&self) -> Result<FallbackFees, FallbackFeeFetchError> {
        let default_fee = self
            .eth_call::<IPropAMMRouter::fallbackFeeCall>(
                self.router_address,
                "fallbackFee",
                IPropAMMRouter::fallbackFeeCall {}.abi_encode(),
            )
            .await?;

        // A Uniswap V3 fee tier fits in 24 bits, so this cannot lose information.
        let mut fees = FallbackFees::default();
        fees.set_default_fee(default_fee.to::<u32>());

        let mut seen = HashSet::new();
        for venue in &self.venues {
            let pairs = match self
                .eth_call::<IPropAMM::getPairsCall>(
                    *venue,
                    "getPairs",
                    IPropAMM::getPairsCall {}.abi_encode(),
                )
                .await
            {
                Ok(pairs) => pairs,
                Err(e) => {
                    warn!(%venue, error = %e, "skipping venue while refreshing fee tiers");
                    continue;
                }
            };

            for pair in pairs {
                if !seen.insert((pair.token0, pair.token1)) {
                    continue;
                }
                let fee = self
                    .eth_call::<IPropAMMRouter::getPairFeeCall>(
                        self.router_address,
                        "getPairFee",
                        IPropAMMRouter::getPairFeeCall { tokenA: pair.token0, tokenB: pair.token1 }
                            .abi_encode(),
                    )
                    .await?;
                let token0 = TychoAddress::from(pair.token0.as_slice().to_vec());
                let token1 = TychoAddress::from(pair.token1.as_slice().to_vec());
                // A zero fee means no override, which `FallbackFees::set_pair_fee` already
                // treats as "resolve through the default".
                fees.set_pair_fee(&token0, &token1, fee.to::<u32>());
            }
        }

        Ok(fees)
    }

    /// Performs an `eth_call` of `calldata` against `contract` and decodes the return value.
    async fn eth_call<C: SolCall>(
        &self,
        contract: Address,
        method: &'static str,
        calldata: Vec<u8>,
    ) -> Result<C::Return, FallbackFeeFetchError> {
        let response = self
            .provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(contract)),
                input: AlloyBytes::from(calldata).into(),
                ..Default::default()
            })
            .await
            .map_err(|e| FallbackFeeFetchError::Call { method, contract, reason: e.to_string() })?;
        C::abi_decode_returns(&response).map_err(|e| FallbackFeeFetchError::Call {
            method,
            contract,
            reason: format!("failed to decode response: {e}"),
        })
    }
}

fn to_address(raw: &Bytes, what: &str) -> Result<Address, FallbackFeeFetchError> {
    if raw.len() != 20 {
        return Err(FallbackFeeFetchError::Config(format!("{what} {raw:?} is not 20 bytes")));
    }
    Ok(Address::from_slice(raw.as_ref()))
}

#[cfg(test)]
mod tests {
    use alloy::{
        rpc::client::RpcClient,
        sol_types::{SolCall, SolValue},
        transports::mock::Asserter,
    };
    use serde_json::json;

    use super::*;

    const ROUTER: Address = Address::repeat_byte(0x11);
    const VENUE: Address = Address::repeat_byte(0x22);
    const WETH: Address = Address::repeat_byte(0x33);
    const USDC: Address = Address::repeat_byte(0x44);

    fn fetcher_with(asserter: &Asserter) -> FallbackFeeFetcher {
        FallbackFeeFetcher {
            provider: RootProvider::new(RpcClient::mocked(asserter.clone())),
            router_address: ROUTER,
            venues: vec![VENUE],
            shared_fees: SharedFallbackFees::default(),
            refresh_interval: Duration::from_secs(300),
        }
    }

    /// Encodes a return value the way the mocked transport expects it.
    /// The tycho-side form of an alloy address, as the fetcher stores it.
    fn tycho(address: Address) -> TychoAddress {
        TychoAddress::from(address.as_slice().to_vec())
    }

    fn ret(bytes: Vec<u8>) -> serde_json::Value {
        json!(format!("0x{}", alloy::hex::encode(bytes)))
    }

    fn pairs_response(pairs: &[(Address, Address)]) -> serde_json::Value {
        let tuples: Vec<(Address, Address)> = pairs.to_vec();
        ret(tuples.abi_encode())
    }

    #[tokio::test]
    async fn test_fetch_fees_reads_default_and_pair_overrides() {
        let asserter = Asserter::new();
        asserter.push_success(&ret(IPropAMMRouter::fallbackFeeCall::abi_encode_returns(
            &alloy::primitives::Uint::<24, 1>::from(3000),
        )));
        asserter.push_success(&pairs_response(&[(WETH, USDC)]));
        asserter.push_success(&ret(IPropAMMRouter::getPairFeeCall::abi_encode_returns(
            &alloy::primitives::Uint::<24, 1>::from(500),
        )));

        let fees = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .expect("fetch should succeed");

        assert_eq!(fees.resolved_fee(&tycho(WETH), &tycho(USDC)), 500);
        // Any other pair resolves through the global default.
        assert_eq!(fees.resolved_fee(&tycho(WETH), &tycho(ROUTER)), 3000);
    }

    /// A pair with no override must resolve through the default, not be stored as fee 0.
    #[tokio::test]
    async fn test_fetch_fees_treats_zero_override_as_default() {
        let asserter = Asserter::new();
        asserter.push_success(&ret(IPropAMMRouter::fallbackFeeCall::abi_encode_returns(
            &alloy::primitives::Uint::<24, 1>::from(3000),
        )));
        asserter.push_success(&pairs_response(&[(WETH, USDC)]));
        asserter.push_success(&ret(IPropAMMRouter::getPairFeeCall::abi_encode_returns(
            &alloy::primitives::Uint::<24, 1>::from(0),
        )));

        let fees = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .expect("fetch should succeed");

        assert_eq!(fees.resolved_fee(&tycho(WETH), &tycho(USDC)), 3000);
        assert_eq!(fees.per_pair.len(), 0);
    }

    /// One unreachable venue must not discard the tiers already read.
    #[tokio::test]
    async fn test_fetch_fees_skips_a_failing_venue() {
        let asserter = Asserter::new();
        asserter.push_success(&ret(IPropAMMRouter::fallbackFeeCall::abi_encode_returns(
            &alloy::primitives::Uint::<24, 1>::from(100),
        )));
        asserter.push_failure_msg("venue unreachable");

        let fees = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .expect("fetch should succeed without the venue");

        assert_eq!(fees.per_pair.len(), 0);
        assert_eq!(fees.resolved_fee(&tycho(WETH), &tycho(USDC)), 100);
    }

    /// Without the global fee no pair can be resolved, so the refresh must fail and keep the
    /// previous tiers.
    #[tokio::test]
    async fn test_fetch_fees_fails_without_the_default_fee() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("router unreachable");

        let error = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .expect_err("fetch should fail");

        assert!(matches!(error, FallbackFeeFetchError::Call { method: "fallbackFee", .. }));
    }

    #[test]
    fn test_new_rejects_a_short_address() {
        let result = FallbackFeeFetcher::new(
            "http://localhost:8545",
            &Bytes::from(vec![0u8; 19]),
            &[],
            SharedFallbackFees::default(),
            Duration::from_secs(300),
        );

        assert!(matches!(result, Err(FallbackFeeFetchError::Config(_))));
    }
}
