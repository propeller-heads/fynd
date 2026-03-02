use std::time::Duration;

use alloy::{
    consensus::{TxEip1559, TypedTransaction},
    eips::{eip2718::Encodable2718, eip2930::AccessList},
    network::Ethereum,
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
    mapping::wire_to_batch_quote,
    signing::{
        compute_settled_amount, ExecutionReceipt, FyndPayload, SettledOrder, SignablePayload,
        SignedOrder,
    },
    types::{BackendKind, HealthStatus, Quote, QuoteParams},
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
    /// - `base_url`: Base URL of the Fynd RPC server (e.g. `"https://rpc.fynd.exchange"`). Must use
    ///   `http` or `https` scheme.
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
///
/// The type parameter `P` is the alloy provider used for Ethereum RPC calls. In production code
/// this is `RootProvider<Ethereum>` (the default). In tests a mocked provider can be used.
pub struct FyndClient<P = RootProvider<Ethereum>>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    http: HttpClient,
    base_url: String,
    retry: RetryConfig,
    router_address: Address,
    chain_id: u64,
    default_sender: Option<Address>,
    provider: P,
    submit_provider: P,
}

impl<P> FyndClient<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    /// Construct a client directly from its individual fields.
    ///
    /// This is intended for testing; production code should use [`FyndClientBuilder`].
    /// Construct a client directly from its individual fields.
    ///
    /// Intended for testing only. Use [`FyndClientBuilder`] for production code.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_providers(
        http: HttpClient,
        base_url: String,
        retry: RetryConfig,
        router_address: Address,
        chain_id: u64,
        default_sender: Option<Address>,
        provider: P,
        submit_provider: P,
    ) -> Self {
        Self {
            http,
            base_url,
            retry,
            router_address,
            chain_id,
            default_sender,
            provider,
            submit_provider,
        }
    }

    /// Request a quote for one or more swap orders.
    ///
    /// The returned `Quote` has `token_out` and `receiver` populated on each
    /// `OrderSolution` from the corresponding input `Order` (matched by index).
    ///
    /// Retries automatically on transient failures according to the client's [`RetryConfig`].
    pub async fn quote(&self, params: QuoteParams) -> Result<Quote, FyndError> {
        let token_out = params.order.token_out().clone();
        let receiver = params
            .order
            .receiver()
            .unwrap_or_else(|| params.order.sender())
            .clone();
        let wire_request = mapping::quote_params_to_wire(params)?;

        let mut delay = self.retry.initial_backoff;
        for attempt in 0..self.retry.max_attempts {
            match self
                .do_quote(&wire_request, token_out.clone(), receiver.clone())
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
        token_out: Bytes,
        receiver: Bytes,
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
        let batch_quote = wire_to_batch_quote(wire_solution, token_out, receiver)?;

        batch_quote
            .quotes()
            .first()
            .cloned()
            .ok_or_else(|| FyndError::Protocol("Received empty solution".into()))
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
        quote: Quote,
        hints: &SigningHints,
    ) -> Result<SignablePayload, FyndError> {
        match quote.backend() {
            BackendKind::Fynd => {
                self.fynd_signable_payload(quote, hints)
                    .await
            }
            BackendKind::Turbine => {
                Err(FyndError::Protocol("Turbine signing not yet implemented".into()))
            }
        }
    }

    async fn fynd_signable_payload(
        &self,
        quote: Quote,
        hints: &SigningHints,
    ) -> Result<SignablePayload, FyndError> {
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
            None => quote
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
        Ok(SignablePayload::Fynd(Box::new(FyndPayload::new(quote, tx))))
    }

    /// Broadcast a signed order and return an [`ExecutionReceipt`] that resolves once the
    /// transaction is mined.
    ///
    /// This method returns **immediately** after submitting the transaction. The returned
    /// [`ExecutionReceipt`] is an unbounded future that polls for the receipt every 2 seconds.
    /// Wrap it with [`tokio::time::timeout`] to avoid waiting indefinitely.
    pub async fn execute(&self, order: SignedOrder) -> Result<ExecutionReceipt, FyndError> {
        let (payload, signature) = order.into_parts();
        let (solution, tx) = payload.into_fynd_parts()?;

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

        let token_out_addr = mapping::bytes_to_alloy_address(solution.token_out())?;
        let receiver_addr = mapping::bytes_to_alloy_address(solution.receiver())?;
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
                        let gas_cost = BigUint::from(receipt.gas_used) *
                            BigUint::from(receipt.effective_gas_price);
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
    use std::time::Duration;

    use super::*;

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

    // ========================================================================
    // Helpers shared by the HTTP-level tests below
    // ========================================================================

    /// Build a minimal valid [`FyndClient<RootProvider<Ethereum>>`] pointing at a mock HTTP
    /// server URL, using the alloy mock transport for the provider.
    ///
    /// Returns the client and the alloy asserter so tests can pre-load RPC responses.
    fn make_test_client(
        base_url: String,
        retry: RetryConfig,
        default_sender: Option<Address>,
    ) -> (FyndClient<alloy::providers::RootProvider<Ethereum>>, alloy::providers::mock::Asserter)
    {
        use alloy::providers::{mock::Asserter, ProviderBuilder};

        let asserter = Asserter::new();
        let provider = ProviderBuilder::default().connect_mocked_client(asserter.clone());
        let submit_provider = ProviderBuilder::default().connect_mocked_client(asserter.clone());

        let http = HttpClient::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");

        let client = FyndClient::new_with_providers(
            http,
            base_url,
            retry,
            Address::ZERO,
            1,
            default_sender,
            provider,
            submit_provider,
        );

        (client, asserter)
    }

    /// Build a minimal valid `OrderSolution` for use in tests.
    fn make_order_solution() -> crate::types::Quote {
        use num_bigint::BigUint;

        use crate::types::{BackendKind, BlockInfo, SolutionStatus};

        crate::types::Quote::new(
            "test-order-id".to_string(),
            SolutionStatus::Success,
            BackendKind::Fynd,
            None,
            BigUint::from(1_000_000u64),
            BigUint::from(990_000u64),
            BigUint::from(50_000u64),
            Some(10),
            BlockInfo::new(1_234_567, "0xabcdef".to_string(), 1_700_000_000),
            bytes::Bytes::copy_from_slice(&[0xbb; 20]),
            bytes::Bytes::copy_from_slice(&[0xcc; 20]),
        )
    }

    // ========================================================================
    // quote() tests
    // ========================================================================

    #[tokio::test]
    async fn quote_returns_parsed_quote_on_success() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        let body = serde_json::json!({
            "orders": [{
                "order_id": "abc-123",
                "status": "success",
                "amount_in": "1000000",
                "amount_out": "990000",
                "gas_estimate": "50000",
                "amount_out_net_gas": "940000",
                "price_impact_bps": 10,
                "block": {
                    "number": 1234567,
                    "hash": "0xabcdef",
                    "timestamp": 1700000000
                }
            }],
            "total_gas_estimate": "50000",
            "solve_time_ms": 42
        });

        Mock::given(method("POST"))
            .and(path("/v1/solve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let (client, _asserter) = make_test_client(server.uri(), RetryConfig::default(), None);

        let params = make_quote_params();
        let quote = client
            .quote(params)
            .await
            .expect("quote should succeed");

        assert_eq!(quote.order_id(), "abc-123");
        assert_eq!(quote.amount_out(), &num_bigint::BigUint::from(990_000u64));
    }

    #[tokio::test]
    async fn quote_returns_api_error_on_non_retryable_server_error() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        use crate::error::ErrorCode;

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/solve"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "bad input",
                "code": "BAD_REQUEST"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (client, _asserter) = make_test_client(server.uri(), RetryConfig::default(), None);

        let err = client
            .quote(make_quote_params())
            .await
            .unwrap_err();
        assert!(
            matches!(err, FyndError::Api { code: ErrorCode::BadRequest, .. }),
            "expected BadRequest, got {err:?}"
        );
    }

    #[tokio::test]
    async fn quote_retries_on_retryable_error_then_succeeds() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        // First attempt: service unavailable.
        Mock::given(method("POST"))
            .and(path("/v1/solve"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "queue full",
                "code": "QUEUE_FULL"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second attempt: success.
        let success_body = serde_json::json!({
            "orders": [{
                "order_id": "retry-order",
                "status": "success",
                "amount_in": "1000000",
                "amount_out": "990000",
                "gas_estimate": "50000",
                "amount_out_net_gas": "940000",
                "price_impact_bps": null,
                "block": {
                    "number": 1234568,
                    "hash": "0xabcdef01",
                    "timestamp": 1700000012
                }
            }],
            "total_gas_estimate": "50000",
            "solve_time_ms": 10
        });
        Mock::given(method("POST"))
            .and(path("/v1/solve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(success_body))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let retry = RetryConfig::new(3, Duration::from_millis(1), Duration::from_millis(10));
        let (client, _asserter) = make_test_client(server.uri(), retry, None);

        let quote = client
            .quote(make_quote_params())
            .await
            .expect("should succeed after retry");
        assert_eq!(quote.order_id(), "retry-order");
    }

    #[tokio::test]
    async fn quote_exhausts_retries_and_returns_last_error() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        use crate::error::ErrorCode;

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/solve"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "queue full",
                "code": "QUEUE_FULL"
            })))
            .mount(&server)
            .await;

        let retry = RetryConfig::new(2, Duration::from_millis(1), Duration::from_millis(10));
        let (client, _asserter) = make_test_client(server.uri(), retry, None);

        let err = client
            .quote(make_quote_params())
            .await
            .unwrap_err();
        assert!(
            matches!(err, FyndError::Api { code: ErrorCode::ServiceUnavailable, .. }),
            "expected ServiceUnavailable after retry exhaustion, got {err:?}"
        );
    }

    #[tokio::test]
    async fn quote_returns_error_on_malformed_response() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/solve"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"garbage": true})),
            )
            .mount(&server)
            .await;

        let (client, _asserter) = make_test_client(server.uri(), RetryConfig::default(), None);

        let err = client
            .quote(make_quote_params())
            .await
            .unwrap_err();
        // Deserialization failure is wrapped as FyndError::Http (from reqwest json decoding).
        assert!(
            matches!(err, FyndError::Http(_)),
            "expected Http deserialization error, got {err:?}"
        );
    }

    // ========================================================================
    // health() tests
    // ========================================================================

    #[tokio::test]
    async fn health_returns_status_on_success() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": true,
                "last_update_ms": 100,
                "num_solver_pools": 5
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (client, _asserter) = make_test_client(server.uri(), RetryConfig::default(), None);

        let status = client
            .health()
            .await
            .expect("health should succeed");
        assert!(status.healthy());
        assert_eq!(status.last_update_ms(), 100);
        assert_eq!(status.num_solver_pools(), 5);
    }

    #[tokio::test]
    async fn health_returns_error_on_server_failure() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "service unavailable",
                "code": "NOT_READY"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (client, _asserter) = make_test_client(server.uri(), RetryConfig::default(), None);

        let err = client.health().await.unwrap_err();
        assert!(matches!(err, FyndError::Api { .. }), "expected Api error, got {err:?}");
    }

    // ========================================================================
    // signable_payload() tests
    // ========================================================================

    #[tokio::test]
    async fn signable_payload_uses_hints_when_all_provided() {
        let sender = Address::with_last_byte(0xab);
        let (client, _asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let solution = make_order_solution();
        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(5),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: Some(100_000),
            simulate: false,
        };

        let payload = client
            .signable_payload(solution, &hints)
            .await
            .expect("signable_payload should succeed");

        let SignablePayload::Fynd(fynd) = payload else {
            panic!("expected Fynd payload");
        };
        let TypedTransaction::Eip1559(tx) = fynd.tx() else {
            panic!("expected EIP-1559 transaction");
        };
        assert_eq!(tx.nonce, 5);
        assert_eq!(tx.max_fee_per_gas, 1_000_000_000);
        assert_eq!(tx.max_priority_fee_per_gas, 1_000_000);
        assert_eq!(tx.gas_limit, 100_000);
    }

    #[tokio::test]
    async fn signable_payload_fetches_nonce_and_fees_when_hints_absent() {
        let sender = Address::with_last_byte(0xde);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), Some(sender));

        // eth_getTransactionCount → nonce 7
        asserter.push_success(&7u64);
        // estimate_eip1559_fees calls eth_feeHistory; push two values for the response
        // alloy's estimate_eip1559_fees uses eth_feeHistory; we push a plausible response.
        // The estimate_eip1559_fees method calls eth_feeHistory with 1 block, 25/75 percentiles.
        let fee_history = serde_json::json!({
            "oldestBlock": "0x1",
            "baseFeePerGas": ["0x3b9aca00", "0x3b9aca00"],
            "gasUsedRatio": [0.5],
            "reward": [["0xf4240", "0x1e8480"]]
        });
        asserter.push_success(&fee_history);

        let solution = make_order_solution();
        let hints = SigningHints::default();

        let payload = client
            .signable_payload(solution, &hints)
            .await
            .expect("signable_payload should succeed");

        let SignablePayload::Fynd(fynd) = payload else {
            panic!("expected Fynd payload");
        };
        let TypedTransaction::Eip1559(tx) = fynd.tx() else {
            panic!("expected EIP-1559 transaction");
        };
        assert_eq!(tx.nonce, 7, "nonce should come from mock");
    }

    #[tokio::test]
    async fn signable_payload_returns_config_error_when_no_sender() {
        // No sender on client, no sender in hints.
        let (client, _asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let solution = make_order_solution();
        let hints = SigningHints::default(); // no sender

        let err = client
            .signable_payload(solution, &hints)
            .await
            .unwrap_err();

        assert!(matches!(err, FyndError::Config(_)), "expected Config error, got {err:?}");
    }

    #[tokio::test]
    async fn signable_payload_with_simulate_true_calls_eth_call_successfully() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let solution = make_order_solution();
        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(1),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: Some(100_000),
            simulate: true,
        };

        // eth_call → success (empty bytes result)
        asserter.push_success(&alloy::primitives::Bytes::new());

        let payload = client
            .signable_payload(solution, &hints)
            .await
            .expect("signable_payload with simulate=true should succeed");

        assert!(matches!(payload, SignablePayload::Fynd(_)));
    }

    #[tokio::test]
    async fn signable_payload_with_simulate_true_returns_simulation_failed_on_revert() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let solution = make_order_solution();
        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(1),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: Some(100_000),
            simulate: true,
        };

        // eth_call → revert (RPC-level execution error)
        asserter.push_failure_msg("execution reverted");

        let err = client
            .signable_payload(solution, &hints)
            .await
            .unwrap_err();

        assert!(
            matches!(err, FyndError::SimulationFailed(_)),
            "expected SimulationFailed, got {err:?}"
        );
    }

    // ========================================================================
    // Helper to build minimal QuoteParams
    // ========================================================================

    fn make_quote_params() -> QuoteParams {
        use crate::types::{Order, OrderSide, QuoteOptions};

        let token_in = bytes::Bytes::copy_from_slice(&[0xaa; 20]);
        let token_out = bytes::Bytes::copy_from_slice(&[0xbb; 20]);
        let sender = bytes::Bytes::copy_from_slice(&[0xcc; 20]);

        let order = Order::new(
            token_in,
            token_out,
            num_bigint::BigUint::from(1_000_000u64),
            OrderSide::Sell,
            sender,
            None,
        );

        QuoteParams::new(order, QuoteOptions::default())
    }
}
