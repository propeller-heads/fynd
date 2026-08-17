//! Background task mirroring on-chain `FeeCalculator` fee configuration into
//! [`SharedRouterFees`].
//!
//! On start-up and on every refresh tick the fetcher resolves the FeeCalculator address
//! from the Tycho Router (`getFeeCalculator`), then reads its precision scale (`MAX_BPS`),
//! the default router fees, and all per-client overrides. Failed fetches keep the previously
//! stored values, so the encoder always has a usable fee configuration.

use std::time::Duration;

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};
use tycho_simulation::tycho_common::Bytes;

use crate::encoding::router_fees::{RouterFees, SharedRouterFees};

sol! {
    /// Mirror of the FeeCalculator's `CustomFees` storage struct.
    struct CustomFees {
        bool hasCustomFeeOnOutput;
        uint32 feeBpsOnOutput;
        bool hasCustomFeeOnClientFee;
        uint32 feeBpsOnClientFee;
    }

    interface ITychoRouter {
        function getFeeCalculator() external view returns (address);
    }

    interface IFeeCalculator {
        function MAX_BPS() external view returns (uint32);
        function getRouterFeeOnOutput() external view returns (uint32);
        function getRouterFeeOnClientFee() external view returns (uint32);
        function getAllClientFees(uint256 start, uint256 count)
            external view returns (address[] memory clients, CustomFees[] memory fees);
    }
}

/// Custom-fee entries requested per `getAllClientFees` call. Each entry is five 32-byte ABI
/// words (an address plus the four-field `CustomFees` tuple), so a full page is ~80 KB —
/// well within node response limits.
const CLIENT_FEE_PAGE_SIZE: usize = 500;

/// Error fetching router fees from chain.
#[derive(Debug, thiserror::Error)]
pub enum RouterFeeFetchError {
    /// The fetcher could not be constructed from the given configuration.
    #[error("invalid router fee fetcher configuration: {0}")]
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

/// Periodically refreshes [`SharedRouterFees`] from the on-chain FeeCalculator.
pub struct RouterFeeFetcher {
    provider: RootProvider<Ethereum>,
    router_address: Address,
    shared_fees: SharedRouterFees,
    refresh_interval: Duration,
}

impl RouterFeeFetcher {
    /// Creates a fetcher reading from `router_address` via the JSON-RPC node at `rpc_url`.
    ///
    /// # Errors
    ///
    /// Returns [`RouterFeeFetchError::Config`] if `rpc_url` is not a valid URL or
    /// `router_address` is not 20 bytes.
    pub fn new(
        rpc_url: &str,
        router_address: &Bytes,
        shared_fees: SharedRouterFees,
        refresh_interval: Duration,
    ) -> Result<Self, RouterFeeFetchError> {
        let url = rpc_url.parse().map_err(|e| {
            RouterFeeFetchError::Config(format!("invalid RPC URL {rpc_url:?}: {e}"))
        })?;
        if router_address.len() != 20 {
            return Err(RouterFeeFetchError::Config(format!(
                "router address {router_address:?} is not 20 bytes"
            )));
        }
        Ok(Self {
            provider: ProviderBuilder::default().connect_http(url),
            router_address: Address::from_slice(router_address.as_ref()),
            shared_fees,
            refresh_interval,
        })
    }

    /// Runs the refresh loop: fetches immediately, then on every `refresh_interval` tick.
    ///
    /// Fetch failures are logged; the previously stored fees stay in effect until a fetch
    /// succeeds.
    pub async fn run(&self) {
        let mut ticker = interval(self.refresh_interval);
        // Skip missed ticks rather than catching up — fetches are best-effort.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            match self.fetch_fees().await {
                Ok(fees) => {
                    info!(
                        custom_clients = fees.custom_client_count(),
                        "router fees refreshed from on-chain FeeCalculator"
                    );
                    self.shared_fees.set(fees);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "failed to refresh router fees from chain; keeping previous values"
                    );
                }
            }
        }
    }

    /// Reads the full fee configuration from chain: the precision scale, default fees, and
    /// all custom client fees.
    ///
    /// Resolves the FeeCalculator address from the router on every fetch, so calculator
    /// upgrades are picked up without reconfiguration.
    async fn fetch_fees(&self) -> Result<RouterFees, RouterFeeFetchError> {
        let fee_calculator = self
            .eth_call::<ITychoRouter::getFeeCalculatorCall>(
                self.router_address,
                "getFeeCalculator",
                ITychoRouter::getFeeCalculatorCall {}.abi_encode(),
            )
            .await?;

        let max_fee_units = self
            .eth_call::<IFeeCalculator::MAX_BPSCall>(
                fee_calculator,
                "MAX_BPS",
                IFeeCalculator::MAX_BPSCall {}.abi_encode(),
            )
            .await?;
        if max_fee_units == 0 {
            return Err(RouterFeeFetchError::Call {
                method: "MAX_BPS",
                contract: fee_calculator,
                reason: "fee precision scale is zero".to_string(),
            });
        }

        let default_fee_on_output = self
            .eth_call::<IFeeCalculator::getRouterFeeOnOutputCall>(
                fee_calculator,
                "getRouterFeeOnOutput",
                IFeeCalculator::getRouterFeeOnOutputCall {}.abi_encode(),
            )
            .await?;

        let default_fee_on_client_fee = self
            .eth_call::<IFeeCalculator::getRouterFeeOnClientFeeCall>(
                fee_calculator,
                "getRouterFeeOnClientFee",
                IFeeCalculator::getRouterFeeOnClientFeeCall {}.abi_encode(),
            )
            .await?;

        let mut custom_fees = rustc_hash::FxHashMap::default();
        let mut start = 0usize;
        loop {
            let page = self
                .eth_call::<IFeeCalculator::getAllClientFeesCall>(
                    fee_calculator,
                    "getAllClientFees",
                    IFeeCalculator::getAllClientFeesCall {
                        start: U256::from(start),
                        count: U256::from(CLIENT_FEE_PAGE_SIZE),
                    }
                    .abi_encode(),
                )
                .await?;

            let page_len = page.clients.len();
            for (client, fees) in page.clients.into_iter().zip(page.fees) {
                // Resolve each field against the defaults here, mirroring
                // FeeCalculator._getFeeInfo, so the stored pair is the effective rate.
                let on_output = if fees.hasCustomFeeOnOutput {
                    fees.feeBpsOnOutput
                } else {
                    default_fee_on_output
                };
                let on_client_fee = if fees.hasCustomFeeOnClientFee {
                    fees.feeBpsOnClientFee
                } else {
                    default_fee_on_client_fee
                };
                custom_fees
                    .insert(Bytes::from(client.as_slice().to_vec()), (on_output, on_client_fee));
            }

            if page_len < CLIENT_FEE_PAGE_SIZE {
                break;
            }
            start += CLIENT_FEE_PAGE_SIZE;
        }

        Ok(RouterFees::new(
            max_fee_units as u64,
            default_fee_on_output,
            default_fee_on_client_fee,
            custom_fees,
        ))
    }

    /// Performs an `eth_call` of `calldata` against `contract` and decodes the return value.
    async fn eth_call<C: SolCall>(
        &self,
        contract: Address,
        method: &'static str,
        calldata: Vec<u8>,
    ) -> Result<C::Return, RouterFeeFetchError> {
        let response = self
            .provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(contract)),
                input: AlloyBytes::from(calldata).into(),
                ..Default::default()
            })
            .await
            .map_err(|e| RouterFeeFetchError::Call { method, contract, reason: e.to_string() })?;
        C::abi_decode_returns(&response).map_err(|e| RouterFeeFetchError::Call {
            method,
            contract,
            reason: format!("failed to decode response: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::{rpc::client::RpcClient, transports::mock::Asserter};

    use super::*;

    const ROUTER: Address = Address::repeat_byte(0x11);
    const CALCULATOR: Address = Address::repeat_byte(0x22);
    /// FeeCalculator precision returned by the mock: 100% = 100,000,000 fee units.
    const MAX_FEE_UNITS: u32 = 100_000_000;

    fn fetcher_with(asserter: &Asserter) -> RouterFeeFetcher {
        RouterFeeFetcher {
            provider: RootProvider::new(RpcClient::mocked(asserter.clone())),
            router_address: ROUTER,
            shared_fees: SharedRouterFees::default(),
            refresh_interval: Duration::from_secs(300),
        }
    }

    fn push_return<C: SolCall>(asserter: &Asserter, ret: &C::Return) {
        asserter.push_success(&AlloyBytes::from(C::abi_encode_returns(ret)));
    }

    fn push_defaults(asserter: &Asserter, fee_on_output: u32, fee_on_client_fee: u32) {
        push_return::<ITychoRouter::getFeeCalculatorCall>(asserter, &CALCULATOR);
        push_return::<IFeeCalculator::MAX_BPSCall>(asserter, &MAX_FEE_UNITS);
        push_return::<IFeeCalculator::getRouterFeeOnOutputCall>(asserter, &fee_on_output);
        push_return::<IFeeCalculator::getRouterFeeOnClientFeeCall>(asserter, &fee_on_client_fee);
    }

    fn custom_fees(on_output: Option<u32>, on_client_fee: Option<u32>) -> CustomFees {
        CustomFees {
            hasCustomFeeOnOutput: on_output.is_some(),
            feeBpsOnOutput: on_output.unwrap_or(0),
            hasCustomFeeOnClientFee: on_client_fee.is_some(),
            feeBpsOnClientFee: on_client_fee.unwrap_or(0),
        }
    }

    #[tokio::test]
    async fn test_fetch_fees_defaults_and_custom_clients() {
        let asserter = Asserter::new();
        push_defaults(&asserter, 150_000, 25_000_000);
        let client_a = Address::repeat_byte(0xAA);
        let client_b = Address::repeat_byte(0xBB);
        push_return::<IFeeCalculator::getAllClientFeesCall>(
            &asserter,
            &IFeeCalculator::getAllClientFeesReturn {
                clients: vec![client_a, client_b],
                fees: vec![custom_fees(Some(50_000), None), custom_fees(None, Some(10_000_000))],
            },
        );

        let fees = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .unwrap();

        let rates_a = fees.fees_for(&Bytes::from(client_a.as_slice().to_vec()));
        assert_eq!(rates_a.on_output(), 50_000);
        assert_eq!(rates_a.on_client_fee(), 25_000_000);
        let rates_b = fees.fees_for(&Bytes::from(client_b.as_slice().to_vec()));
        assert_eq!(rates_b.on_output(), 150_000);
        assert_eq!(rates_b.on_client_fee(), 10_000_000);
        let rates_unknown = fees.fees_for(&Bytes::from(vec![0xCC; 20]));
        assert_eq!(rates_unknown.on_output(), 150_000);
        assert_eq!(rates_unknown.on_client_fee(), 25_000_000);
        assert_eq!(fees.max_fee_units(), MAX_FEE_UNITS as u64);
    }

    #[tokio::test]
    async fn test_fetch_fees_rejects_zero_precision_scale() {
        let asserter = Asserter::new();
        push_return::<ITychoRouter::getFeeCalculatorCall>(&asserter, &CALCULATOR);
        push_return::<IFeeCalculator::MAX_BPSCall>(&asserter, &0u32);

        let err = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .unwrap_err();

        assert!(err.to_string().contains("MAX_BPS"));
    }

    #[tokio::test]
    async fn test_fetch_fees_paginates_until_partial_page() {
        let asserter = Asserter::new();
        push_defaults(&asserter, 100_000, 20_000_000);

        // Full first page → fetcher must request a second page.
        let full_page: Vec<Address> = (0..CLIENT_FEE_PAGE_SIZE)
            .map(|i| {
                let mut bytes = [0u8; 20];
                bytes[..8].copy_from_slice(&(i as u64).to_be_bytes());
                bytes[19] = 1;
                Address::from(bytes)
            })
            .collect();
        push_return::<IFeeCalculator::getAllClientFeesCall>(
            &asserter,
            &IFeeCalculator::getAllClientFeesReturn {
                clients: full_page.clone(),
                fees: vec![custom_fees(Some(1), None); CLIENT_FEE_PAGE_SIZE],
            },
        );
        let last_client = Address::repeat_byte(0xEE);
        push_return::<IFeeCalculator::getAllClientFeesCall>(
            &asserter,
            &IFeeCalculator::getAllClientFeesReturn {
                clients: vec![last_client],
                fees: vec![custom_fees(Some(2), None)],
            },
        );

        let fees = fetcher_with(&asserter)
            .fetch_fees()
            .await
            .unwrap();

        assert_eq!(fees.custom_client_count(), CLIENT_FEE_PAGE_SIZE + 1);
        let last_rates = fees.fees_for(&Bytes::from(last_client.as_slice().to_vec()));
        assert_eq!(last_rates.on_output(), 2);
    }

    /// Live integration test against the deployed Tycho Router on Ethereum mainnet.
    ///
    /// Ignored by default because it hits a real RPC node. Run with:
    /// `RPC_URL=<mainnet-rpc> cargo test -p fynd-core fetch_fees_against_mainnet -- --ignored`
    /// (falls back to a public endpoint if `RPC_URL` is unset).
    #[tokio::test]
    #[ignore = "hits a live mainnet RPC node"]
    async fn test_fetch_fees_against_mainnet_router() {
        // Tycho Router on Ethereum mainnet.
        let router = Bytes::from(
            Address::from_str("0xdA892C989d07A18B5DD3F392d949f00dF15C5736")
                .unwrap()
                .as_slice(),
        );
        let rpc_url = std::env::var("RPC_URL")
            .unwrap_or_else(|_| "https://ethereum-rpc.publicnode.com".to_string());

        let fetcher =
            RouterFeeFetcher::new(&rpc_url, &router, SharedRouterFees::default(), Duration::ZERO)
                .unwrap();

        let fees = fetcher
            .fetch_fees()
            .await
            .expect("should read fees from the live mainnet FeeCalculator");

        // The deployed FeeCalculator must expose a non-zero precision scale, and default
        // rates must resolve for an arbitrary (unknown) client.
        assert!(fees.max_fee_units() > 0, "max_fee_units must be non-zero");
        let default_rates = fees.fees_for(&Bytes::from(vec![0u8; 20]));
        assert_eq!(default_rates.max_fee_units(), fees.max_fee_units());

        println!(
            "mainnet router fees: max_fee_units={}, default_on_output={}, \
             default_on_client_fee={}, custom_clients={}",
            fees.max_fee_units(),
            default_rates.on_output(),
            default_rates.on_client_fee(),
            fees.custom_client_count(),
        );
    }
}
