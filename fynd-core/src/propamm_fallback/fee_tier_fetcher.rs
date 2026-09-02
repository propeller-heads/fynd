//! Background task that mirrors the fee tiers of Titan's PropAMMRouter into `SharedFeeTiers`.
//!
//! The router picks the Uniswap V3 pool it falls back to with
//! `resolvedFee(tokenIn, tokenOut)`: the per-pair override if one is set, else the global
//! `fallbackFee`. Governance can change both without a contract upgrade, so the values are read
//! from chain rather than hardcoded.
//!
//! A refresh costs two `eth_call`s however many venues and pairs there are: one Multicall3 batch
//! for `fallbackFee` plus each venue's `getPairs`, and one for the `getPairFee` of every pair those
//! venues quote. A failed refresh keeps the previous tiers.

use std::time::Duration;

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes},
    providers::{
        bindings::IMulticall3::{aggregate3Call, Call3, Result as Multicall3Result},
        ProviderBuilder, RootProvider, MULTICALL3_ADDRESS,
    },
    sol,
    sol_types::SolCall,
};
use rustc_hash::FxHashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};
use tycho_simulation::tycho_common::{models::Address as TychoAddress, Bytes};

use crate::{
    propamm_fallback::{FeeTiers, SharedFeeTiers},
    rpc,
};

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
pub enum FeeTierFetchError {
    /// The fetcher could not be constructed from the given configuration.
    #[error("invalid fallback fee tier fetcher configuration: {0}")]
    Config(String),
    /// The batched `eth_call` failed, or a call the batch carried reverted.
    #[error("{method} call to {contract} failed: {reason}")]
    Call {
        /// Contract method that failed.
        method: &'static str,
        /// Contract the call was sent to.
        contract: Address,
        /// Underlying transport, revert or ABI decoding error.
        reason: String,
    },
}

/// Periodically refreshes `SharedFeeTiers` from the PropAMMRouter.
pub struct FeeTierFetcher {
    provider: RootProvider<Ethereum>,
    router_address: Address,
    venues: Vec<Address>,
    shared_fee_tiers: SharedFeeTiers,
    refresh_interval: Duration,
}

impl FeeTierFetcher {
    /// Creates a fetcher reading `router_address` and the pairs of `venues` via the node at
    /// `rpc_url`.
    ///
    /// # Errors
    ///
    /// Returns `FeeTierFetchError::Config` if `rpc_url` is not a valid URL, or if any
    /// address is not 20 bytes.
    pub fn new(
        rpc_url: &str,
        router_address: &Bytes,
        venues: &[Bytes],
        shared_fee_tiers: SharedFeeTiers,
        refresh_interval: Duration,
    ) -> Result<Self, FeeTierFetchError> {
        let url = rpc_url
            .parse()
            .map_err(|e| FeeTierFetchError::Config(format!("invalid RPC URL {rpc_url:?}: {e}")))?;
        Ok(Self {
            provider: ProviderBuilder::default().connect_http(url),
            router_address: to_address(router_address, "router address")?,
            venues: venues
                .iter()
                .map(|venue| to_address(venue, "venue address"))
                .collect::<Result<_, _>>()?,
            shared_fee_tiers,
            refresh_interval,
        })
    }

    /// Runs the refresh loop: fetches immediately, then on every `refresh_interval` tick.
    ///
    /// Fetch failures are logged. The previously read tiers stay in effect until a fetch succeeds.
    /// Before the first success the tiers stay empty, which drops pAMM routes rather than pricing
    /// them against a guessed tier.
    pub async fn run(&self) {
        let mut ticker = interval(self.refresh_interval);
        // Skip missed ticks rather than catching up — fetches are best-effort.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            if let Err(e) = self.refresh_once().await {
                warn!(
                    error = %e,
                    "failed to refresh PropAMMRouter fee tiers; keeping previous values"
                );
            }
        }
    }

    /// Reads the tiers once and stores them.
    ///
    /// [`run`](Self::run) is this on a loop. A caller with no task to run it in takes this instead
    /// — a replayed market is solved against one block and never refreshes — and gets the failure
    /// back rather than a warning it has to notice in the log.
    ///
    /// # Errors
    ///
    /// Returns whatever the read failed with. The stored tiers are left as they were.
    pub async fn refresh_once(&self) -> Result<(), FeeTierFetchError> {
        let fee_tiers = self.fetch_fee_tiers().await?;
        info!(
            pair_overrides = fee_tiers.pair_override_count(),
            "PropAMMRouter fee tiers refreshed"
        );
        self.shared_fee_tiers.set(fee_tiers);
        Ok(())
    }

    /// Reads the global `fallbackFee` and the per-pair override for every pair the venues quote.
    ///
    /// A venue whose `getPairs` call reverts is skipped, so one broken venue does not discard the
    /// tiers of the others. A failed `fallbackFee` read aborts the whole refresh, because every
    /// pair without an override resolves through it.
    async fn fetch_fee_tiers(&self) -> Result<FeeTiers, FeeTierFetchError> {
        let (default_tier, pairs) = self
            .fetch_default_tier_and_pairs()
            .await?;
        let mut fee_tiers = FeeTiers::new(default_tier);

        if pairs.is_empty() {
            return Ok(fee_tiers);
        }

        for ((token_a, token_b), tier) in pairs
            .iter()
            .zip(self.fetch_pair_tiers(&pairs).await?)
        {
            let token_a = TychoAddress::from(token_a.as_slice().to_vec());
            let token_b = TychoAddress::from(token_b.as_slice().to_vec());
            // A zero tier means no override, which `FeeTiers::set_pair_tier` already treats as
            // "resolve through the default".
            fee_tiers.set_pair_tier(&token_a, &token_b, tier);
        }
        Ok(fee_tiers)
    }

    /// Batches the router's `fallbackFee` with each venue's `getPairs` into one `eth_call`.
    ///
    /// The returned pairs are deduplicated on their sorted addresses, so two venues quoting the
    /// same pair — in either token order — cost one `getPairFee` rather than two.
    async fn fetch_default_tier_and_pairs(
        &self,
    ) -> Result<(u32, Vec<(Address, Address)>), FeeTierFetchError> {
        let mut calls = Vec::with_capacity(self.venues.len() + 1);
        calls.push(call3(self.router_address, IPropAMMRouter::fallbackFeeCall {}.abi_encode()));
        for venue in &self.venues {
            calls.push(call3(*venue, IPropAMM::getPairsCall {}.abi_encode()));
        }
        let results = self
            .aggregate3(calls, "fallbackFee")
            .await?;

        // The batch put `fallbackFee` first, so its result leads and one result per venue follows.
        let Some((fallback_fee, venue_results)) = results.split_first() else {
            return Err(self.call_error("fallbackFee", "empty multicall response".to_string()));
        };
        let default_tier = decode::<IPropAMMRouter::fallbackFeeCall>(fallback_fee)
            .map_err(|e| self.call_error("fallbackFee", e))?;

        let mut seen = FxHashSet::default();
        let mut pairs = Vec::new();
        for (venue, result) in self.venues.iter().zip(venue_results) {
            match decode::<IPropAMM::getPairsCall>(result) {
                Ok(venue_pairs) => {
                    for pair in venue_pairs {
                        let key = sorted(pair.token0, pair.token1);
                        if seen.insert(key) {
                            pairs.push(key);
                        }
                    }
                }
                Err(e) => warn!(%venue, error = %e, "skipping venue while refreshing fee tiers"),
            }
        }
        Ok((default_tier.to::<u32>(), pairs))
    }

    /// Batches the `getPairFee` of every pair into one `eth_call`, answering in the order of
    /// `pairs`.
    async fn fetch_pair_tiers(
        &self,
        pairs: &[(Address, Address)],
    ) -> Result<Vec<u32>, FeeTierFetchError> {
        let calls = pairs
            .iter()
            .map(|(token_a, token_b)| {
                call3(
                    self.router_address,
                    IPropAMMRouter::getPairFeeCall { tokenA: *token_a, tokenB: *token_b }
                        .abi_encode(),
                )
            })
            .collect();
        let results = self
            .aggregate3(calls, "getPairFee")
            .await?;
        if results.len() != pairs.len() {
            return Err(self.call_error(
                "getPairFee",
                format!("multicall answered {} of {} pairs", results.len(), pairs.len()),
            ));
        }

        let mut tiers = Vec::with_capacity(results.len());
        for (result, (token_a, token_b)) in results.iter().zip(pairs) {
            let tier = decode::<IPropAMMRouter::getPairFeeCall>(result).map_err(|e| {
                self.call_error("getPairFee", format!("pair ({token_a}, {token_b}): {e}"))
            })?;
            tiers.push(tier.to::<u32>());
        }
        Ok(tiers)
    }

    /// Sends `calls` as one Multicall3 `aggregate3`, attributing a transport failure to `method`.
    async fn aggregate3(
        &self,
        calls: Vec<Call3>,
        method: &'static str,
    ) -> Result<Vec<Multicall3Result>, FeeTierFetchError> {
        rpc::eth_call::<aggregate3Call>(
            &self.provider,
            MULTICALL3_ADDRESS,
            aggregate3Call { calls }.abi_encode(),
        )
        .await
        .map_err(|reason| FeeTierFetchError::Call { method, contract: MULTICALL3_ADDRESS, reason })
    }

    fn call_error(&self, method: &'static str, reason: String) -> FeeTierFetchError {
        FeeTierFetchError::Call { method, contract: self.router_address, reason }
    }
}

/// A Multicall3 entry that tolerates a revert, so one failing call does not fail the batch.
fn call3(target: Address, calldata: Vec<u8>) -> Call3 {
    Call3 { target, allowFailure: true, callData: AlloyBytes::from(calldata) }
}

/// Decodes one entry of an `aggregate3` response as `C`'s return value.
fn decode<C: SolCall>(result: &Multicall3Result) -> Result<C::Return, String> {
    if !result.success {
        return Err("call reverted".to_string());
    }
    C::abi_decode_returns(&result.returnData).map_err(|e| format!("failed to decode response: {e}"))
}

/// Order-independent pair key, matching the router's `_pairKey`.
fn sorted(token_a: Address, token_b: Address) -> (Address, Address) {
    if token_a <= token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

fn to_address(raw: &Bytes, what: &str) -> Result<Address, FeeTierFetchError> {
    rpc::to_address(raw, what).map_err(FeeTierFetchError::Config)
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::Uint,
        rpc::client::RpcClient,
        sol_types::{SolCall, SolValue},
        transports::mock::Asserter,
    };
    use serde_json::json;

    use super::*;

    const ROUTER: Address = Address::repeat_byte(0x11);
    const VENUE: Address = Address::repeat_byte(0x22);
    const OTHER_VENUE: Address = Address::repeat_byte(0x55);
    const WETH: Address = Address::repeat_byte(0x33);
    const USDC: Address = Address::repeat_byte(0x44);

    fn fetcher_with(asserter: &Asserter) -> FeeTierFetcher {
        FeeTierFetcher {
            provider: RootProvider::new(RpcClient::mocked(asserter.clone())),
            router_address: ROUTER,
            venues: vec![VENUE],
            shared_fee_tiers: SharedFeeTiers::default(),
            refresh_interval: Duration::from_secs(300),
        }
    }

    /// The tycho-side form of an alloy address, as the fetcher stores it.
    fn tycho(address: Address) -> TychoAddress {
        TychoAddress::from(address.as_slice().to_vec())
    }

    fn tier(fee_tier: u32) -> Vec<u8> {
        IPropAMMRouter::fallbackFeeCall::abi_encode_returns(&Uint::<24, 1>::from(fee_tier))
    }

    fn pairs(pairs: &[(Address, Address)]) -> Vec<u8> {
        pairs.to_vec().abi_encode()
    }

    /// Queues one Multicall3 `aggregate3` response: `Ok` entries carry return data, `Err` entries
    /// mark that call as reverted.
    fn push_batch(asserter: &Asserter, results: &[Result<Vec<u8>, ()>]) {
        let results: Vec<Multicall3Result> = results
            .iter()
            .map(|result| match result {
                Ok(data) => {
                    Multicall3Result { success: true, returnData: AlloyBytes::from(data.clone()) }
                }
                Err(()) => Multicall3Result { success: false, returnData: AlloyBytes::new() },
            })
            .collect();
        let encoded = aggregate3Call::abi_encode_returns(&results);
        asserter.push_success(&json!(format!("0x{}", alloy::hex::encode(encoded))));
    }

    #[tokio::test]
    async fn test_fetch_fee_tiers_default_and_pair_overrides() {
        let asserter = Asserter::new();
        // Round one: the router's global tier, then the venue's pairs.
        push_batch(&asserter, &[Ok(tier(3000)), Ok(pairs(&[(WETH, USDC)]))]);
        // Round two: the per-pair override.
        push_batch(&asserter, &[Ok(tier(500))]);

        let fee_tiers = fetcher_with(&asserter)
            .fetch_fee_tiers()
            .await
            .expect("fetch should succeed");

        assert_eq!(fee_tiers.resolved_tier(&tycho(WETH), &tycho(USDC)), 500);
        // Any other pair resolves through the global default.
        assert_eq!(fee_tiers.resolved_tier(&tycho(WETH), &tycho(ROUTER)), 3000);
        // A refresh is two batched calls, whatever the venue and pair count.
        assert!(asserter.read_q().is_empty(), "both queued batches should be consumed");
    }

    /// A pair with no override must resolve through the default, not be stored as tier 0.
    #[tokio::test]
    async fn test_fetch_fee_tiers_zero_override() {
        let asserter = Asserter::new();
        push_batch(&asserter, &[Ok(tier(3000)), Ok(pairs(&[(WETH, USDC)]))]);
        push_batch(&asserter, &[Ok(tier(0))]);

        let fee_tiers = fetcher_with(&asserter)
            .fetch_fee_tiers()
            .await
            .expect("fetch should succeed");

        assert_eq!(fee_tiers.resolved_tier(&tycho(WETH), &tycho(USDC)), 3000);
        assert_eq!(fee_tiers.pair_override_count(), 0);
    }

    /// One reverting venue must not discard the tiers already read.
    #[tokio::test]
    async fn test_fetch_fee_tiers_failing_venue() {
        let asserter = Asserter::new();
        push_batch(&asserter, &[Ok(tier(100)), Err(())]);

        let fee_tiers = fetcher_with(&asserter)
            .fetch_fee_tiers()
            .await
            .expect("fetch should succeed without the venue");

        assert_eq!(fee_tiers.pair_override_count(), 0);
        assert_eq!(fee_tiers.resolved_tier(&tycho(WETH), &tycho(USDC)), 100);
    }

    /// Without the global tier no pair can be resolved, so the refresh must fail and keep the
    /// previous tiers.
    #[tokio::test]
    async fn test_fetch_fee_tiers_without_default_tier() {
        let asserter = Asserter::new();
        push_batch(&asserter, &[Err(()), Ok(pairs(&[(WETH, USDC)]))]);

        let error = fetcher_with(&asserter)
            .fetch_fee_tiers()
            .await
            .expect_err("fetch should fail");

        assert!(matches!(error, FeeTierFetchError::Call { method: "fallbackFee", .. }));
    }

    /// Two venues quoting the same pair in opposite token order cost one `getPairFee`.
    #[tokio::test]
    async fn test_fetch_fee_tiers_deduplicates_pairs_across_venues() {
        let asserter = Asserter::new();
        push_batch(
            &asserter,
            &[Ok(tier(3000)), Ok(pairs(&[(WETH, USDC)])), Ok(pairs(&[(USDC, WETH)]))],
        );
        // One override, for the one deduplicated pair.
        push_batch(&asserter, &[Ok(tier(500))]);

        let mut fetcher = fetcher_with(&asserter);
        fetcher.venues = vec![VENUE, OTHER_VENUE];
        let fee_tiers = fetcher
            .fetch_fee_tiers()
            .await
            .expect("fetch should succeed");

        assert_eq!(fee_tiers.pair_override_count(), 1);
        assert_eq!(fee_tiers.resolved_tier(&tycho(WETH), &tycho(USDC)), 500);
    }

    /// A reverting `getPairFee` leaves that pair without a tier, so the refresh fails rather than
    /// resolve the pair through the wrong default.
    #[tokio::test]
    async fn test_fetch_fee_tiers_failing_pair_tier() {
        let asserter = Asserter::new();
        push_batch(&asserter, &[Ok(tier(3000)), Ok(pairs(&[(WETH, USDC)]))]);
        push_batch(&asserter, &[Err(())]);

        let error = fetcher_with(&asserter)
            .fetch_fee_tiers()
            .await
            .expect_err("fetch should fail");

        assert!(matches!(error, FeeTierFetchError::Call { method: "getPairFee", .. }));
    }

    #[test]
    fn test_new_short_address() {
        let result = FeeTierFetcher::new(
            "http://localhost:8545",
            &Bytes::from(vec![0u8; 19]),
            &[],
            SharedFeeTiers::default(),
            Duration::from_secs(300),
        );

        assert!(matches!(result, Err(FeeTierFetchError::Config(_))));
    }
}
