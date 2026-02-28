use std::time::Duration;

use alloy::{
    consensus::{TxEip1559, TypedTransaction},
    eips::eip2718::Encodable2718,
    eips::eip2930::AccessList,
    primitives::{Address, Bytes as AlloyBytes, TxKind, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
};
use bytes::Bytes;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use reqwest::Client as HttpClient;

use crate::{
    error::FyndError,
    mapping,
    signing::{
        compute_settled_amount, ExecutionReceipt, FyndPayload, SettledOrder, SignablePayload,
        SignedOrder,
    },
    types::{BackendKind, HealthStatus, OrderSolution, Quote, QuoteParams},
};

// ============================================================================
// RETRY CONFIG
// ============================================================================

/// Controls how [`FyndClient::quote`] retries transient failures.
///
/// Retries use exponential back-off: each attempt doubles the delay, capped at
/// [`max_backoff`](Self::max_backoff). Only errors where
/// [`FyndError::is_retryable`](crate::FyndError::is_retryable) returns `true` are retried.
pub struct RetryConfig {
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl RetryConfig {
    /// Create a custom retry configuration.
    ///
    /// - `max_attempts`: total attempts including the first try.
    /// - `initial_backoff`: sleep duration before the second attempt.
    /// - `max_backoff`: upper bound on any single sleep duration.
    pub fn new(max_attempts: u32, initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self { max_attempts, initial_backoff, max_backoff }
    }

    /// Maximum number of total attempts (default: 3).
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Sleep duration before the first retry (default: 100 ms).
    pub fn initial_backoff(&self) -> Duration {
        self.initial_backoff
    }

    /// Upper bound on any single sleep duration (default: 2 s).
    pub fn max_backoff(&self) -> Duration {
        self.max_backoff
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        }
    }
}

// ============================================================================
// SIGNING HINTS
// ============================================================================

/// Optional hints to override auto-resolved transaction parameters.
///
/// All fields default to `None` / `false`. Unset fields are resolved automatically from the
/// RPC node during [`FyndClient::signable_payload`].
#[derive(Default)]
pub struct SigningHints {
    /// Override the sender address. If `None`, falls back to the address set on the client via
    /// [`FyndClientBuilder::with_sender`].
    pub sender: Option<Address>,
    /// Override the transaction nonce. If `None`, fetched via `eth_getTransactionCount`.
    pub nonce: Option<u64>,
    /// Override `maxFeePerGas` (wei). If `None`, estimated via `eth_maxPriorityFeePerGas`.
    pub max_fee_per_gas: Option<u128>,
    /// Override `maxPriorityFeePerGas` (wei). If `None`, estimated alongside `max_fee_per_gas`.
    pub max_priority_fee_per_gas: Option<u128>,
    /// Override the gas limit. If `None`, taken from the solution's `gas_estimate`.
    pub gas_limit: Option<u64>,
    /// When `true`, simulate the transaction via `eth_call` before returning. A simulation
    /// failure results in [`FyndError::SimulationFailed`].
    pub simulate: bool,
}

// ============================================================================
// CLIENT BUILDER
// ============================================================================

/// Builder for [`FyndClient`].
///
/// Call [`FyndClientBuilder::new`] with the Fynd RPC URL and an Ethereum JSON-RPC URL, configure
/// optional settings, then call [`build`](Self::build) to connect and return a ready client.
///
/// `build` performs two network calls: one to validate the RPC URL (fetching `chain_id`) and one
/// to construct the HTTP provider. It does **not** connect to the Fynd API.
pub struct FyndClientBuilder {
    base_url: String,
    timeout: Duration,
    retry: RetryConfig,
    /// Optional router contract address. Defaults to `Address::ZERO` if not set.
    ///
    /// TODO: Replace with the actual RouterV3 contract address once deployed.
    router_address: Option<Address>,
    rpc_url: String,
    submit_url: Option<String>,
    sender: Option<Address>,
}

impl FyndClientBuilder {
    /// Create a new builder.
    ///
    /// - `base_url`: Base URL of the Fynd RPC server (e.g. `"https://rpc.fynd.exchange"`).
    ///   Must use `http` or `https` scheme.
    /// - `rpc_url`: Ethereum JSON-RPC endpoint for nonce/fee queries and receipt polling.
    pub fn new(base_url: impl Into<String>, rpc_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeout: Duration::from_secs(30),
            retry: RetryConfig::default(),
            router_address: None,
            rpc_url: rpc_url.into(),
            submit_url: None,
            sender: None,
        }
    }

    /// Set the HTTP request timeout for Fynd API calls (default: 30 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the retry configuration (default: 3 attempts, 100 ms / 2 s back-off).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Use a separate RPC URL for transaction submission and receipt polling.
    ///
    /// If not set, the `rpc_url` passed to [`new`](Self::new) is used for both.
    pub fn with_submit_url(mut self, url: impl Into<String>) -> Self {
        self.submit_url = Some(url.into());
        self
    }

    /// Set the default sender address used when [`SigningHints::sender`] is `None`.
    pub fn with_sender(mut self, sender: Address) -> Self {
        self.sender = Some(sender);
        self
    }

    /// Override the RouterV3 contract address (default: `Address::ZERO`).
    ///
    /// This address is used as the `to` field of every EIP-1559 transaction.
    pub fn with_router_address(mut self, addr: Address) -> Self {
        self.router_address = Some(addr);
        self
    }

    /// Connect to the Ethereum RPC node and build the [`FyndClient`].
    ///
    /// Validates the URLs and fetches the chain ID. Returns [`FyndError::Config`] if any URL is
    /// invalid or the chain ID cannot be fetched.
    pub async fn build(self) -> Result<FyndClient, FyndError> {
        // Validate base_url scheme.
        let parsed_base = self
            .base_url
            .parse::<reqwest::Url>()
            .map_err(|e| FyndError::Config(format!("invalid base URL: {e}")))?;
        let scheme = parsed_base.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(FyndError::Config(format!(
                "base URL must use http or https scheme, got '{scheme}'"
            )));
        }

        // Build HTTP providers.
        let rpc_url = self
            .rpc_url
            .parse::<reqwest::Url>()
            .map_err(|e| FyndError::Config(format!("invalid RPC URL: {e}")))?;
        let provider = ProviderBuilder::default().connect_http(rpc_url);

        let submit_url_str = self
            .submit_url
            .as_deref()
            .unwrap_or(&self.rpc_url);
        let submit_url = submit_url_str
            .parse::<reqwest::Url>()
            .map_err(|e| FyndError::Config(format!("invalid submit URL: {e}")))?;
        let submit_provider = ProviderBuilder::default().connect_http(submit_url);

        // Fetch chain_id from the RPC node.
        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| FyndError::Config(format!("failed to fetch chain_id from RPC: {e}")))?;

        // TODO: Replace with the actual RouterV3 contract address once deployed.
        let router_address = self
            .router_address
            .unwrap_or(Address::ZERO);

        // Build HTTP client.
        let http = HttpClient::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| FyndError::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(FyndClient {
            http,
            base_url: self.base_url,
            retry: self.retry,
            router_address,
            chain_id,
            default_sender: self.sender,
            provider,
            submit_provider,
        })
    }
}

// ============================================================================
// FYND CLIENT
// ============================================================================

/// The main entry point for interacting with the Fynd DEX router.
///
/// Construct via [`FyndClientBuilder`]. All methods are `async` and require a Tokio runtime.
pub struct FyndClient {
    http: HttpClient,
    base_url: String,
    retry: RetryConfig,
    router_address: Address,
    chain_id: u64,
    default_sender: Option<Address>,
    provider: RootProvider<alloy::network::Ethereum>,
    submit_provider: RootProvider<alloy::network::Ethereum>,
}

impl FyndClient {
    /// Request a quote for one or more swap orders.
    ///
    /// The returned `Quote` has `token_out` and `receiver` populated on each
    /// `OrderSolution` from the corresponding input `Order` (matched by index).
    ///
    /// Retries automatically on transient failures according to the client's [`RetryConfig`].
    pub async fn quote(&self, params: QuoteParams) -> Result<Quote, FyndError> {
        // Snapshot token_out and receiver before consuming params.
        // Orders and solutions are parallel arrays — matched by index.
        let order_token_outs: Vec<Bytes> = params
            .orders
            .iter()
            .map(|o| o.token_out().clone())
            .collect();
        let order_receivers: Vec<Bytes> = params
            .orders
            .iter()
            .map(|o| {
                o.receiver()
                    .cloned()
                    .unwrap_or_else(|| o.sender().clone())
            })
            .collect();

        let wire_request = mapping::quote_params_to_wire(params)?;

        let mut delay = self.retry.initial_backoff;
        for attempt in 0..self.retry.max_attempts {
            match self
                .do_quote(&wire_request, &order_token_outs, &order_receivers)
                .await
            {
                Ok(quote) => return Ok(quote),
                Err(e) if e.is_retryable() && attempt + 1 < self.retry.max_attempts => {
                    tracing::debug!(attempt, "quote request failed, retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(self.retry.max_backoff);
                }
                Err(e) => return Err(e),
            }
        }
        Err(FyndError::Protocol("retry loop exhausted without result".into()))
    }

    async fn do_quote(
        &self,
        wire_request: &fynd_rpc_types::SolutionRequest,
        order_token_outs: &[Bytes],
        order_receivers: &[Bytes],
    ) -> Result<Quote, FyndError> {
        let url = format!("{}/v1/solve", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(wire_request)
            .send()
            .await?;
        if !response.status().is_success() {
            let wire_err: fynd_rpc_types::ErrorResponse = response.json().await?;
            return Err(mapping::wire_error_to_fynd(wire_err));
        }
        let wire_solution: fynd_rpc_types::Solution = response.json().await?;
        let total_gas_estimate = wire_solution.total_gas_estimate.clone();
        let solve_time_ms = wire_solution.solve_time_ms;

        // Convert wire order solutions and populate token_out/receiver by index.
        let orders: Vec<OrderSolution> = wire_solution
            .orders
            .into_iter()
            .enumerate()
            .map(|(i, ws)| {
                let solution = OrderSolution::try_from(ws)?;
                let token_out = order_token_outs
                    .get(i)
                    .cloned()
                    .unwrap_or_default();
                let receiver = order_receivers
                    .get(i)
                    .cloned()
                    .unwrap_or_default();
                Ok(solution.with_token_out_and_receiver(token_out, receiver))
            })
            .collect::<Result<Vec<_>, FyndError>>()?;

        Ok(Quote::new(orders, total_gas_estimate, solve_time_ms))
    }

    /// Get the health status of the Fynd RPC server.
    pub async fn health(&self) -> Result<HealthStatus, FyndError> {
        let url = format!("{}/v1/health", self.base_url);
        let response = self.http.get(&url).send().await?;
        if !response.status().is_success() {
            let wire_err: fynd_rpc_types::ErrorResponse = response.json().await?;
            return Err(mapping::wire_error_to_fynd(wire_err));
        }
        let wh: fynd_rpc_types::HealthStatus = response.json().await?;
        Ok(HealthStatus::from(wh))
    }

    /// Build a signable payload for a given order solution.
    ///
    /// For [`BackendKind::Fynd`] solutions, this resolves the sender nonce and EIP-1559 fee
    /// parameters from the RPC node (unless overridden via `hints`), then constructs an
    /// unsigned EIP-1559 transaction targeting the RouterV3 contract.
    ///
    /// [`BackendKind::Turbine`] is not yet implemented and returns
    /// [`FyndError::Protocol`].
    ///
    /// `token_out` and `receiver` are read directly from the `solution` (populated during
    /// `quote()`). Pass `&SigningHints::default()` to auto-resolve all transaction parameters.
    pub async fn signable_payload(
        &self,
        solution: OrderSolution,
        hints: &SigningHints,
    ) -> Result<SignablePayload, FyndError> {
        match solution.backend() {
            BackendKind::Fynd => {
                self.fynd_signable_payload(solution, hints)
                    .await
            }
            BackendKind::Turbine => {
                Err(FyndError::Protocol("Turbine signing not yet implemented".into()))
            }
        }
    }

    async fn fynd_signable_payload(
        &self,
        solution: OrderSolution,
        hints: &SigningHints,
    ) -> Result<SignablePayload, FyndError> {
        // Read token_out and receiver from the solution (populated during quote()).
        let token_out = solution.token_out().clone();
        let receiver = solution.receiver().clone();

        // Resolve sender.
        let sender = hints
            .sender
            .or(self.default_sender)
            .ok_or_else(|| FyndError::Config("no sender configured".into()))?;

        // Resolve nonce.
        let nonce = match hints.nonce {
            Some(n) => n,
            None => self
                .provider
                .get_transaction_count(sender)
                .await
                .map_err(FyndError::Provider)?,
        };

        // Resolve EIP-1559 fees.
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            match (hints.max_fee_per_gas, hints.max_priority_fee_per_gas) {
                (Some(mf), Some(mp)) => (mf, mp),
                _ => {
                    let est = self
                        .provider
                        .estimate_eip1559_fees()
                        .await
                        .map_err(FyndError::Provider)?;
                    (est.max_fee_per_gas, est.max_priority_fee_per_gas)
                }
            };

        // Resolve gas limit.
        let gas_limit = match hints.gas_limit {
            Some(g) => g,
            None => solution
                .gas_estimate()
                .to_u64()
                .ok_or_else(|| FyndError::Protocol("gas estimate exceeds u64".into()))?,
        };

        let tx_eip1559 = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_limit,
            to: TxKind::Call(self.router_address),
            value: U256::ZERO,
            input: AlloyBytes::new(),
            access_list: AccessList::default(),
        };

        // Optionally simulate the transaction.
        if hints.simulate {
            let req: alloy::rpc::types::TransactionRequest = tx_eip1559.clone().into();
            self.provider
                .call(req)
                .await
                .map_err(|e| {
                    FyndError::SimulationFailed(format!("transaction simulation failed: {e}"))
                })?;
        }

        let tx = TypedTransaction::Eip1559(tx_eip1559);
        Ok(SignablePayload::Fynd(Box::new(FyndPayload::new(solution, tx, token_out, receiver))))
    }

    /// Broadcast a signed order and return an [`ExecutionReceipt`] that resolves once the
    /// transaction is mined.
    ///
    /// This method returns **immediately** after submitting the transaction. The returned
    /// [`ExecutionReceipt`] is an unbounded future that polls for the receipt every 2 seconds.
    /// Wrap it with [`tokio::time::timeout`] to avoid waiting indefinitely.
    pub async fn execute(&self, order: SignedOrder) -> Result<ExecutionReceipt, FyndError> {
        let (payload, signature) = order.into_parts();
        let (_solution, tx, token_out, receiver) = payload.into_fynd_parts()?;

        let TypedTransaction::Eip1559(tx_eip1559) = tx else {
            return Err(FyndError::Protocol(
                "only EIP-1559 transactions are supported for execution".into(),
            ));
        };

        let envelope = TypedTransaction::Eip1559(tx_eip1559).into_envelope(signature);
        let raw = envelope.encoded_2718();

        let pending = self
            .submit_provider
            .send_raw_transaction(&raw)
            .await
            .map_err(FyndError::Provider)?;
        let tx_hash = *pending.tx_hash();

        let token_out_addr = mapping::bytes_to_alloy_address(&token_out)?;
        let receiver_addr = mapping::bytes_to_alloy_address(&receiver)?;
        let provider = self.submit_provider.clone();

        Ok(ExecutionReceipt::Transaction(Box::pin(async move {
            loop {
                match provider
                    .get_transaction_receipt(tx_hash)
                    .await
                    .map_err(FyndError::Provider)?
                {
                    Some(receipt) => {
                        let settled_amount =
                            compute_settled_amount(&receipt, &token_out_addr, &receiver_addr);
                        let gas_cost = BigUint::from(receipt.gas_used)
                            * BigUint::from(receipt.effective_gas_price);
                        return Ok(SettledOrder::new(receipt, settled_amount, gas_cost));
                    }
                    None => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn retry_config_default_values() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts(), 3);
        assert_eq!(config.initial_backoff(), Duration::from_millis(100));
        assert_eq!(config.max_backoff(), Duration::from_secs(2));
    }

    #[test]
    fn signing_hints_default_all_none_and_no_simulate() {
        let hints = SigningHints::default();
        assert!(hints.sender.is_none());
        assert!(hints.nonce.is_none());
        assert!(!hints.simulate);
    }
}
