use std::{collections::HashMap, time::Duration};

use alloy::{
    consensus::{TxEip1559, TypedTransaction},
    eips::eip2930::AccessList,
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind, B256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionRequest,
    },
};
use bytes::Bytes;
use num_bigint::BigUint;
use reqwest::Client as HttpClient;

use crate::{
    error::FyndError,
    mapping,
    mapping::dto_to_batch_quote,
    signing::{
        compute_settled_amount, ApprovalPayload, ExecutionReceipt, FyndPayload, MinedTx,
        SettledOrder, SignedApproval, SignedSwap, SwapPayload, TxReceipt,
    },
    types::{BackendKind, HealthStatus, InstanceInfo, Quote, QuoteParams, UserTransferType},
};
// ============================================================================
// RETRY CONFIG
// ============================================================================

/// Controls how [`FyndClient::quote`] retries transient failures.
///
/// Retries use exponential back-off: each attempt doubles the delay, capped at
/// [`max_backoff`](Self::max_backoff). Only errors where
/// [`FyndError::is_retryable`](crate::FyndError::is_retryable) returns `true` are retried.
#[derive(Clone)]
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
/// RPC node during [`FyndClient::swap_payload`].
///
/// Build via the setter methods; all options are unset by default.
#[derive(Default)]
pub struct SigningHints {
    sender: Option<Address>,
    nonce: Option<u64>,
    max_fee_per_gas: Option<u128>,
    max_priority_fee_per_gas: Option<u128>,
    gas_limit: Option<u64>,
    simulate: bool,
}

impl SigningHints {
    /// Override the sender address. If not set, falls back to the address configured on the
    /// client via [`FyndClientBuilder::with_sender`].
    pub fn with_sender(mut self, sender: Address) -> Self {
        self.sender = Some(sender);
        self
    }

    /// Override the transaction nonce. If not set, fetched via `eth_getTransactionCount`.
    pub fn with_nonce(mut self, nonce: u64) -> Self {
        self.nonce = Some(nonce);
        self
    }

    /// Override `maxFeePerGas` (wei). If not set, estimated via `eth_feeHistory`.
    pub fn with_max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.max_fee_per_gas = Some(max_fee_per_gas);
        self
    }

    /// Override `maxPriorityFeePerGas` (wei). If not set, estimated alongside `max_fee_per_gas`.
    pub fn with_max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.max_priority_fee_per_gas = Some(max_priority_fee_per_gas);
        self
    }

    /// Override the gas limit. If not set, estimated via `eth_estimateGas` against the
    /// current chain state. Set explicitly to opt out (e.g. use `quote.gas_estimate()`
    /// as a pre-buffered fallback).
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    /// When `true`, simulate the transaction via `eth_call` before returning. A simulation
    /// failure results in [`FyndError::SimulationFailed`].
    pub fn with_simulate(mut self, simulate: bool) -> Self {
        self.simulate = simulate;
        self
    }

    /// The configured sender override, or `None` to fall back to the client default.
    pub fn sender(&self) -> Option<Address> {
        self.sender
    }

    /// The configured nonce override, or `None` to fetch from the RPC node.
    pub fn nonce(&self) -> Option<u64> {
        self.nonce
    }

    /// The configured `maxFeePerGas` override (wei), or `None` to estimate.
    pub fn max_fee_per_gas(&self) -> Option<u128> {
        self.max_fee_per_gas
    }

    /// The configured `maxPriorityFeePerGas` override (wei), or `None` to estimate.
    pub fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.max_priority_fee_per_gas
    }

    /// The configured gas limit override, or `None` to use the quote's estimate.
    pub fn gas_limit(&self) -> Option<u64> {
        self.gas_limit
    }

    /// Whether to simulate the transaction via `eth_call` before returning.
    pub fn simulate(&self) -> bool {
        self.simulate
    }
}

// ============================================================================
// STORAGE OVERRIDES
// ============================================================================

/// Per-account EVM storage slot overrides for dry-run simulations.
///
/// Maps 20-byte contract addresses to a set of 32-byte slot → value pairs. Passed via
/// [`ExecutionOptions::storage_overrides`] to override on-chain state during a
/// [`FyndClient::execute_swap`] dry run.
///
/// # Example
///
/// ```rust
/// use fynd_client::StorageOverrides;
/// use bytes::Bytes;
///
/// let mut overrides = StorageOverrides::default();
/// let contract = Bytes::copy_from_slice(&[0xAA; 20]);
/// let slot    = Bytes::copy_from_slice(&[0x00; 32]);
/// let value   = Bytes::copy_from_slice(&[0x01; 32]);
/// overrides.insert(contract, slot, value);
/// ```
#[derive(Clone, Default)]
pub struct StorageOverrides {
    /// address (20 bytes) → { slot (32 bytes) → value (32 bytes) }
    slots: HashMap<Bytes, HashMap<Bytes, Bytes>>,
}

impl StorageOverrides {
    /// Add a storage slot override for a contract.
    ///
    /// - `address`: 20-byte contract address.
    /// - `slot`: 32-byte storage slot key.
    /// - `value`: 32-byte replacement value.
    pub fn insert(&mut self, address: Bytes, slot: Bytes, value: Bytes) {
        self.slots
            .entry(address)
            .or_default()
            .insert(slot, value);
    }
}

fn storage_overrides_to_alloy(so: &StorageOverrides) -> Result<StateOverride, FyndError> {
    let mut result = StateOverride::default();
    for (addr_bytes, slot_map) in &so.slots {
        let addr = mapping::bytes_to_alloy_address(addr_bytes)?;
        let state_diff = slot_map
            .iter()
            .map(|(slot, val)| Ok((bytes_to_b256(slot)?, bytes_to_b256(val)?)))
            .collect::<Result<alloy::primitives::map::B256HashMap<B256>, FyndError>>()?;
        result.insert(addr, AccountOverride { state_diff: Some(state_diff), ..Default::default() });
    }
    Ok(result)
}

fn bytes_to_b256(b: &Bytes) -> Result<B256, FyndError> {
    if b.len() != 32 {
        return Err(FyndError::Protocol(format!("expected 32-byte slot, got {} bytes", b.len())));
    }
    let arr: [u8; 32] = b
        .as_ref()
        .try_into()
        .expect("length checked above");
    Ok(B256::from(arr))
}

// ============================================================================
// EXECUTION OPTIONS
// ============================================================================

/// Options controlling the behaviour of [`FyndClient::execute_swap`].
#[derive(Default)]
pub struct ExecutionOptions {
    /// When `true`, simulate the transaction via `eth_call` and `estimate_gas` instead of
    /// broadcasting it. The returned [`ExecutionReceipt`] resolves immediately with the
    /// simulated settled amount (decoded from the call return data) and the estimated gas cost.
    /// No transaction is submitted to the network.
    pub dry_run: bool,
    /// Storage slot overrides to apply during dry-run simulation. Ignored when `dry_run` is
    /// `false`.
    pub storage_overrides: Option<StorageOverrides>,
}

// ============================================================================
// APPROVAL PARAMS
// ============================================================================

/// Parameters for [`FyndClient::approval`].
pub struct ApprovalParams {
    token: bytes::Bytes,
    amount: num_bigint::BigUint,
    transfer_type: UserTransferType,
    check_allowance: bool,
}

impl ApprovalParams {
    /// Create approval parameters for the given token and amount.
    ///
    /// Defaults to a standard ERC-20 approval against the router contract.
    /// Use [`with_transfer_type`](Self::with_transfer_type) to approve the Permit2 contract
    /// instead.
    ///
    /// When `check_allowance` is `true`, [`FyndClient::approval`] checks the on-chain allowance
    /// first and returns `None` if it is already sufficient.
    pub fn new(token: bytes::Bytes, amount: num_bigint::BigUint, check_allowance: bool) -> Self {
        Self { token, amount, transfer_type: UserTransferType::TransferFrom, check_allowance }
    }

    /// Override the transfer type (and thus the spender contract).
    ///
    /// `UserTransferType::TransferFrom` → router (default).
    /// `UserTransferType::TransferFromPermit2` → Permit2.
    /// `UserTransferType::UseVaultsFunds` → [`FyndClient::approval`] returns `None` immediately.
    pub fn with_transfer_type(mut self, transfer_type: UserTransferType) -> Self {
        self.transfer_type = transfer_type;
        self
    }
}

// ============================================================================
// ERC-20 ABI
// ============================================================================

mod erc20 {
    use alloy::sol;

    sol! {
        function approve(address spender, uint256 amount) returns (bool);
        function allowance(address owner, address spender) returns (uint256);
    }
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

    /// Build a [`FyndClient`] without connecting to an Ethereum RPC node.
    ///
    /// Suitable for [`FyndClient::quote`] and [`FyndClient::health`] calls only.
    /// [`FyndClient::swap_payload`] and [`FyndClient::execute_swap`] require a live RPC URL and
    /// will fail if called on a client built this way.
    ///
    /// Returns [`FyndError::Config`] if `base_url` is invalid.
    pub fn build_quote_only(self) -> Result<FyndClient, FyndError> {
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

        // Use dummy providers pointing at the base URL.
        // These are never invoked for quote/health operations.
        let provider = ProviderBuilder::default().connect_http(parsed_base.clone());
        let submit_provider = ProviderBuilder::default().connect_http(parsed_base);

        let http = HttpClient::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| FyndError::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(FyndClient {
            http,
            base_url: self.base_url,
            retry: self.retry,
            chain_id: 1,
            default_sender: self.sender,
            provider,
            submit_provider,
            info_cache: std::sync::OnceLock::new(),
        })
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

        // Build HTTP client.
        let http = HttpClient::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| FyndError::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(FyndClient {
            http,
            base_url: self.base_url,
            retry: self.retry,
            chain_id,
            default_sender: self.sender,
            provider,
            submit_provider,
            info_cache: std::sync::OnceLock::new(),
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
    chain_id: u64,
    default_sender: Option<Address>,
    provider: P,
    submit_provider: P,
    info_cache: std::sync::OnceLock<InstanceInfo>,
}

impl<P> FyndClient<P>
where
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    /// Construct a client directly from its individual fields.
    ///
    /// Intended for testing only. Use [`FyndClientBuilder`] for production code.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_providers(
        http: HttpClient,
        base_url: String,
        retry: RetryConfig,
        chain_id: u64,
        default_sender: Option<Address>,
        provider: P,
        submit_provider: P,
    ) -> Self {
        Self {
            http,
            base_url,
            retry,
            chain_id,
            default_sender,
            provider,
            submit_provider,
            info_cache: std::sync::OnceLock::new(),
        }
    }

    /// Request a quote for one or more swap orders.
    ///
    /// The returned `Quote` has `token_out` and `receiver` populated on each
    /// `OrderQuote` from the corresponding input `Order` (matched by index).
    ///
    /// Retries automatically on transient failures according to the client's [`RetryConfig`].
    pub async fn quote(&self, params: QuoteParams) -> Result<Quote, FyndError> {
        let token_out = params.order.token_out().clone();
        let receiver = params
            .order
            .receiver()
            .unwrap_or_else(|| params.order.sender())
            .clone();
        let dto_request = mapping::quote_params_to_dto(params)?;

        let mut delay = self.retry.initial_backoff;
        for attempt in 0..self.retry.max_attempts {
            match self
                .request_quote(&dto_request, token_out.clone(), receiver.clone())
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

    async fn request_quote(
        &self,
        dto_request: &fynd_rpc_types::QuoteRequest,
        token_out: Bytes,
        receiver: Bytes,
    ) -> Result<Quote, FyndError> {
        let url = format!("{}/v1/quote", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(dto_request)
            .send()
            .await?;
        if !response.status().is_success() {
            let dto_err: fynd_rpc_types::ErrorResponse = response.json().await?;
            return Err(mapping::dto_error_to_fynd(dto_err));
        }
        let dto_quote: fynd_rpc_types::Quote = response.json().await?;
        let solve_time_ms = dto_quote.solve_time_ms();
        let batch_quote = dto_to_batch_quote(dto_quote, token_out, receiver)?;

        let mut quote = batch_quote
            .quotes()
            .first()
            .cloned()
            .ok_or_else(|| FyndError::Protocol("Received empty quote".into()))?;
        quote.solve_time_ms = solve_time_ms;
        Ok(quote)
    }

    /// Get the health status of the Fynd RPC server.
    pub async fn health(&self) -> Result<HealthStatus, FyndError> {
        let url = format!("{}/v1/health", self.base_url);
        let response = self.http.get(&url).send().await?;
        let status = response.status();
        let body = response.text().await?;
        // The server returns HealthStatus JSON for both 200 and 503 (not-ready).
        // Try parsing as HealthStatus first, then fall back to ErrorResponse.
        if let Ok(dh) = serde_json::from_str::<fynd_rpc_types::HealthStatus>(&body) {
            return Ok(HealthStatus::from(dh));
        }
        if let Ok(dto_err) = serde_json::from_str::<fynd_rpc_types::ErrorResponse>(&body) {
            return Err(mapping::dto_error_to_fynd(dto_err));
        }
        Err(FyndError::Protocol(format!("unexpected health response ({status}): {body}")))
    }

    /// Build a swap payload for a given order quote, ready for signing.
    ///
    /// For [`BackendKind::Fynd`] quotes, this resolves the sender nonce and EIP-1559 fee
    /// parameters from the RPC node (unless overridden via `hints`), then constructs an
    /// unsigned EIP-1559 transaction targeting the RouterV3 contract.
    ///
    /// [`BackendKind::Turbine`] is not yet implemented and returns
    /// [`FyndError::Protocol`].
    ///
    /// `token_out` and `receiver` are read directly from the `quote` (populated during
    /// `quote()`). Pass `&SigningHints::default()` to auto-resolve all transaction parameters.
    pub async fn swap_payload(
        &self,
        quote: Quote,
        hints: &SigningHints,
    ) -> Result<SwapPayload, FyndError> {
        match quote.backend() {
            BackendKind::Fynd => {
                self.fynd_swap_payload(quote, hints)
                    .await
            }
            BackendKind::Turbine => {
                Err(FyndError::Protocol("Turbine signing not yet implemented".into()))
            }
        }
    }

    async fn fynd_swap_payload(
        &self,
        quote: Quote,
        hints: &SigningHints,
    ) -> Result<SwapPayload, FyndError> {
        // Resolve sender.
        let sender = hints
            .sender()
            .or(self.default_sender)
            .ok_or_else(|| FyndError::Config("no sender configured".into()))?;

        // Resolve nonce.
        let nonce = match hints.nonce() {
            Some(n) => n,
            None => self
                .provider
                .get_transaction_count(sender)
                .await
                .map_err(FyndError::Provider)?,
        };

        // Resolve EIP-1559 fees.
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            match (hints.max_fee_per_gas(), hints.max_priority_fee_per_gas()) {
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

        let tx_data = quote.transaction().ok_or_else(|| {
            FyndError::Protocol(
                "quote has no calldata; set encoding_options in QuoteOptions".into(),
            )
        })?;
        let to_addr = mapping::bytes_to_alloy_address(tx_data.to())?;
        let value = mapping::biguint_to_u256(tx_data.value());
        let input = AlloyBytes::from(tx_data.data().to_vec());

        // Resolve gas limit. If not explicitly set, estimate via eth_estimateGas so the
        // limit reflects the actual chain state. Pass with_gas_limit() to use a fixed value
        // instead (e.g. quote.gas_estimate() as a pre-buffered fallback).
        let gas_limit = match hints.gas_limit() {
            Some(g) => g,
            None => {
                let req = alloy::rpc::types::TransactionRequest::default()
                    .from(sender)
                    .to(to_addr)
                    .value(value)
                    .input(input.clone().into());
                self.provider
                    .estimate_gas(req)
                    .await
                    .map_err(FyndError::Provider)?
            }
        };

        let tx_eip1559 = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_limit,
            to: TxKind::Call(to_addr),
            value,
            input,
            access_list: AccessList::default(),
        };

        // Optionally simulate the transaction.
        if hints.simulate() {
            let req = alloy::rpc::types::TransactionRequest::from_transaction_with_sender(
                tx_eip1559.clone(),
                sender,
            );
            self.provider
                .call(req)
                .await
                .map_err(|e| {
                    FyndError::SimulationFailed(format!("transaction simulation failed: {e}"))
                })?;
        }

        let tx = TypedTransaction::Eip1559(tx_eip1559);
        Ok(SwapPayload::Fynd(Box::new(FyndPayload::new(quote, tx))))
    }

    /// Broadcast a signed swap and return an [`ExecutionReceipt`] that resolves once the
    /// transaction is mined.
    ///
    /// Pass [`ExecutionOptions::default`] for standard on-chain submission. Set
    /// [`ExecutionOptions::dry_run`] to `true` to simulate only — the receipt resolves immediately
    /// with values derived from `eth_call` (settled amount) and `eth_estimateGas` (gas cost).
    ///
    /// For real submissions, this method returns **immediately** after broadcasting. The inner
    /// future polls every 2 seconds and has no built-in timeout; wrap with
    /// [`tokio::time::timeout`] to bound the wait.
    pub async fn execute_swap(
        &self,
        order: SignedSwap,
        options: &ExecutionOptions,
    ) -> Result<ExecutionReceipt, FyndError> {
        let (payload, signature) = order.into_parts();
        let (quote, tx) = payload.into_fynd_parts()?;

        let TypedTransaction::Eip1559(tx_eip1559) = tx else {
            return Err(FyndError::Protocol(
                "only EIP-1559 transactions are supported for execution".into(),
            ));
        };

        if options.dry_run {
            return self
                .dry_run_execute(tx_eip1559, options)
                .await;
        }

        let tx_hash = self
            .send_raw(tx_eip1559, signature)
            .await?;

        let token_out_addr = mapping::bytes_to_alloy_address(quote.token_out())?;
        let receiver_addr = mapping::bytes_to_alloy_address(quote.receiver())?;
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
                        return Ok(SettledOrder::new(settled_amount, gas_cost));
                    }
                    None => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
        })))
    }

    /// Fetch and cache static instance metadata from `GET /v1/info`.
    ///
    /// The result is fetched at most once per [`FyndClient`] instance; subsequent calls return the
    /// cached value without making a network request.
    pub async fn info(&self) -> Result<&InstanceInfo, FyndError> {
        if let Some(cached) = self.info_cache.get() {
            return Ok(cached);
        }
        let fetched = self.fetch_info().await?;
        let _ = self.info_cache.set(fetched);
        Ok(self.info_cache.get().expect("just set"))
    }

    async fn fetch_info(&self) -> Result<InstanceInfo, FyndError> {
        let url = format!("{}/v1/info", self.base_url);
        let response = self.http.get(&url).send().await?;
        if !response.status().is_success() {
            let dto_err: fynd_rpc_types::ErrorResponse = response.json().await?;
            return Err(mapping::dto_error_to_fynd(dto_err));
        }
        let dto_info: fynd_rpc_types::InstanceInfo = response.json().await?;
        mapping::dto_to_instance_info(dto_info)
    }

    /// Build an unsigned EIP-1559 `approve(spender, amount)` transaction for the given token,
    /// or `None` if the allowance is already sufficient.
    ///
    /// 1. Calls [`info()`](Self::info) to resolve the spender address from `params.spender`.
    /// 2. If `params.check_allowance` is `true`, checks the current ERC-20 allowance and returns
    ///    `None` immediately if it is already sufficient (skipping nonce and fee resolution).
    /// 3. Resolves nonce and EIP-1559 fees via `hints` (same semantics as
    ///    [`swap_payload`](Self::swap_payload)).
    /// 4. Encodes the `approve(spender, amount)` calldata using the ERC-20 ABI.
    ///
    /// Gas defaults to `hints.gas_limit().unwrap_or(65_000)`.
    pub async fn approval(
        &self,
        params: &ApprovalParams,
        hints: &SigningHints,
    ) -> Result<Option<ApprovalPayload>, FyndError> {
        use alloy::sol_types::SolCall;

        let info = self.info().await?;
        let spender_addr = match params.transfer_type {
            UserTransferType::TransferFrom => {
                mapping::bytes_to_alloy_address(info.router_address())?
            }
            UserTransferType::TransferFromPermit2 => {
                mapping::bytes_to_alloy_address(info.permit2_address())?
            }
            UserTransferType::UseVaultsFunds => return Ok(None),
        };

        let sender = hints
            .sender()
            .or(self.default_sender)
            .ok_or_else(|| FyndError::Config("no sender configured".into()))?;

        let token_addr = mapping::bytes_to_alloy_address(&params.token)?;
        let amount_u256 = mapping::biguint_to_u256(&params.amount);

        // Check allowance before any other RPC calls so we can return early.
        if params.check_allowance {
            let call_data =
                erc20::allowanceCall { owner: sender, spender: spender_addr }.abi_encode();
            let req = alloy::rpc::types::TransactionRequest {
                to: Some(alloy::primitives::TxKind::Call(token_addr)),
                input: alloy::rpc::types::TransactionInput::new(AlloyBytes::from(call_data)),
                ..Default::default()
            };
            let result = self
                .provider
                .call(req)
                .await
                .map_err(|e| FyndError::Protocol(format!("allowance call failed: {e}")))?;
            let current_allowance = if result.len() >= 32 {
                alloy::primitives::U256::from_be_slice(&result[0..32])
            } else {
                alloy::primitives::U256::ZERO
            };
            if current_allowance >= amount_u256 {
                return Ok(None);
            }
        }

        // Resolve nonce.
        let nonce = match hints.nonce() {
            Some(n) => n,
            None => self
                .provider
                .get_transaction_count(sender)
                .await
                .map_err(FyndError::Provider)?,
        };

        // Resolve EIP-1559 fees.
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            match (hints.max_fee_per_gas(), hints.max_priority_fee_per_gas()) {
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

        let calldata =
            erc20::approveCall { spender: spender_addr, amount: amount_u256 }.abi_encode();

        // Resolve gas limit via eth_estimateGas unless the caller provided an explicit value.
        let gas_limit = match hints.gas_limit() {
            Some(g) => g,
            None => {
                let req = alloy::rpc::types::TransactionRequest::default()
                    .from(sender)
                    .to(token_addr)
                    .input(AlloyBytes::from(calldata.clone()).into());
                self.provider
                    .estimate_gas(req)
                    .await
                    .map_err(FyndError::Provider)?
            }
        };

        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_limit,
            to: alloy::primitives::TxKind::Call(token_addr),
            value: alloy::primitives::U256::ZERO,
            input: AlloyBytes::from(calldata),
            access_list: alloy::eips::eip2930::AccessList::default(),
        };

        let spender = bytes::Bytes::copy_from_slice(spender_addr.as_slice());
        Ok(Some(ApprovalPayload {
            tx,
            token: params.token.clone(),
            spender,
            amount: params.amount.clone(),
        }))
    }

    /// Broadcast a signed approval transaction and return a [`TxReceipt`] that resolves once
    /// the transaction is mined.
    ///
    /// This method returns immediately after broadcasting. The inner future polls every 2 seconds
    /// and has no built-in timeout; wrap with [`tokio::time::timeout`] to bound the wait.
    pub async fn execute_approval(&self, approval: SignedApproval) -> Result<TxReceipt, FyndError> {
        let (payload, signature) = approval.into_parts();
        let tx_hash = self
            .send_raw(payload.tx, signature)
            .await?;
        let provider = self.submit_provider.clone();

        Ok(TxReceipt::Pending(Box::pin(async move {
            loop {
                match provider
                    .get_transaction_receipt(tx_hash)
                    .await
                    .map_err(FyndError::Provider)?
                {
                    Some(receipt) => {
                        let gas_cost = BigUint::from(receipt.gas_used) *
                            BigUint::from(receipt.effective_gas_price);
                        return Ok(MinedTx::new(tx_hash, gas_cost));
                    }
                    None => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
        })))
    }

    /// Encode, sign, and broadcast an EIP-1559 transaction, returning its hash.
    async fn send_raw(
        &self,
        tx: TxEip1559,
        signature: alloy::primitives::Signature,
    ) -> Result<B256, FyndError> {
        use alloy::eips::eip2718::Encodable2718;
        let envelope = TypedTransaction::Eip1559(tx).into_envelope(signature);
        let raw = envelope.encoded_2718();
        let pending = self
            .submit_provider
            .send_raw_transaction(&raw)
            .await
            .map_err(FyndError::Provider)?;
        Ok(*pending.tx_hash())
    }

    async fn dry_run_execute(
        &self,
        tx_eip1559: TxEip1559,
        options: &ExecutionOptions,
    ) -> Result<ExecutionReceipt, FyndError> {
        let mut req: TransactionRequest = tx_eip1559.clone().into();
        if let Some(sender) = self.default_sender {
            req.from = Some(sender);
        }
        let overrides = options
            .storage_overrides
            .as_ref()
            .map(storage_overrides_to_alloy)
            .transpose()?;

        let return_data = self
            .provider
            .call(req.clone())
            .overrides_opt(overrides.clone())
            .await
            .map_err(|e| FyndError::SimulationFailed(format!("dry run simulation failed: {e}")))?;

        let gas_used = self
            .provider
            .estimate_gas(req)
            .overrides_opt(overrides)
            .await
            .map_err(|e| {
                FyndError::SimulationFailed(format!("dry run gas estimation failed: {e}"))
            })?;

        let settled_amount = if return_data.len() >= 32 {
            Some(BigUint::from_bytes_be(&return_data[0..32]))
        } else {
            None
        };
        let gas_cost = BigUint::from(gas_used) * BigUint::from(tx_eip1559.max_fee_per_gas);
        let settled = SettledOrder::new(settled_amount, gas_cost);

        Ok(ExecutionReceipt::Transaction(Box::pin(async move { Ok(settled) })))
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
        assert!(hints.sender().is_none());
        assert!(hints.nonce().is_none());
        assert!(!hints.simulate());
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
            1,
            default_sender,
            provider,
            submit_provider,
        );

        (client, asserter)
    }

    /// Build a minimal valid `OrderQuote` for use in tests.
    fn make_order_quote() -> crate::types::Quote {
        use num_bigint::BigUint;

        use crate::types::{BackendKind, BlockInfo, QuoteStatus, Transaction};

        let tx = Transaction::new(
            bytes::Bytes::copy_from_slice(&[0x01; 20]),
            BigUint::ZERO,
            vec![0x12, 0x34],
        );

        crate::types::Quote::new(
            "test-order-id".to_string(),
            QuoteStatus::Success,
            BackendKind::Fynd,
            None,
            BigUint::from(1_000_000u64),
            BigUint::from(990_000u64),
            BigUint::from(50_000u64),
            BigUint::from(940_000u64),
            Some(10),
            BlockInfo::new(1_234_567, "0xabcdef".to_string(), 1_700_000_000),
            bytes::Bytes::copy_from_slice(&[0xbb; 20]),
            bytes::Bytes::copy_from_slice(&[0xcc; 20]),
            Some(tx),
            None,
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
            .and(path("/v1/quote"))
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
            .and(path("/v1/quote"))
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
            .and(path("/v1/quote"))
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
            .and(path("/v1/quote"))
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
            .and(path("/v1/quote"))
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
            .and(path("/v1/quote"))
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
    // swap_payload() tests
    // ========================================================================

    #[tokio::test]
    async fn swap_payload_uses_hints_when_all_provided() {
        let sender = Address::with_last_byte(0xab);
        let (client, _asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let quote = make_order_quote();
        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(5),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: Some(100_000),
            simulate: false,
        };

        let payload = client
            .swap_payload(quote, &hints)
            .await
            .expect("swap_payload should succeed");

        let SwapPayload::Fynd(fynd) = payload else {
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
    async fn swap_payload_fetches_nonce_and_fees_when_hints_absent() {
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
        // eth_estimateGas → 150_000
        asserter.push_success(&150_000u64);

        let quote = make_order_quote();
        let hints = SigningHints::default();

        let payload = client
            .swap_payload(quote, &hints)
            .await
            .expect("swap_payload should succeed");

        let SwapPayload::Fynd(fynd) = payload else {
            panic!("expected Fynd payload");
        };
        let TypedTransaction::Eip1559(tx) = fynd.tx() else {
            panic!("expected EIP-1559 transaction");
        };
        assert_eq!(tx.nonce, 7, "nonce should come from mock");
        assert_eq!(tx.gas_limit, 150_000, "gas limit should come from eth_estimateGas");
    }

    #[tokio::test]
    async fn swap_payload_returns_config_error_when_no_sender() {
        // No sender on client, no sender in hints.
        let (client, _asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let quote = make_order_quote();
        let hints = SigningHints::default(); // no sender

        let err = client
            .swap_payload(quote, &hints)
            .await
            .unwrap_err();

        assert!(matches!(err, FyndError::Config(_)), "expected Config error, got {err:?}");
    }

    #[tokio::test]
    async fn swap_payload_with_simulate_true_calls_eth_call_successfully() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let quote = make_order_quote();
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
            .swap_payload(quote, &hints)
            .await
            .expect("swap_payload with simulate=true should succeed");

        assert!(matches!(payload, SwapPayload::Fynd(_)));
    }

    #[tokio::test]
    async fn swap_payload_with_simulate_true_returns_simulation_failed_on_revert() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        let quote = make_order_quote();
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
            .swap_payload(quote, &hints)
            .await
            .unwrap_err();

        assert!(
            matches!(err, FyndError::SimulationFailed(_)),
            "expected SimulationFailed, got {err:?}"
        );
    }

    // ========================================================================
    // execute_swap() dry-run tests
    // ========================================================================

    /// Build a [`SignedSwap`] from a minimal [`Quote`] and a dummy transaction.
    ///
    /// Suitable for dry-run tests where neither the signature nor the transaction
    /// contents are validated on-chain.
    fn make_signed_swap() -> SignedSwap {
        use alloy::{
            eips::eip2930::AccessList,
            primitives::{Bytes as AlloyBytes, Signature, TxKind, U256},
        };

        use crate::signing::FyndPayload;

        let quote = make_order_quote();
        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 1,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            gas_limit: 100_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: AlloyBytes::new(),
            access_list: AccessList::default(),
        };
        let payload =
            SwapPayload::Fynd(Box::new(FyndPayload::new(quote, TypedTransaction::Eip1559(tx))));
        SignedSwap::assemble(payload, Signature::test_signature())
    }

    #[tokio::test]
    async fn execute_dry_run_returns_settled_order_without_broadcast() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), Some(sender));

        // Encode 990_000 as ABI uint256 (32-byte big-endian).
        let mut amount_bytes = vec![0u8; 32];
        amount_bytes[24..32].copy_from_slice(&990_000u64.to_be_bytes());
        asserter.push_success(&alloy::primitives::Bytes::copy_from_slice(&amount_bytes));
        asserter.push_success(&50_000u64); // estimate_gas response

        let order = make_signed_swap();
        let opts = ExecutionOptions { dry_run: true, storage_overrides: None };
        let receipt = client
            .execute_swap(order, &opts)
            .await
            .expect("execute should succeed");
        let settled = receipt
            .await
            .expect("should resolve immediately");

        assert_eq!(settled.settled_amount(), Some(&num_bigint::BigUint::from(990_000u64)),);
        let expected_gas_cost =
            num_bigint::BigUint::from(50_000u64) * num_bigint::BigUint::from(1_000_000_000u64);
        assert_eq!(settled.gas_cost(), &expected_gas_cost);
    }

    #[tokio::test]
    async fn execute_dry_run_with_storage_overrides_succeeds() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), Some(sender));

        let mut overrides = StorageOverrides::default();
        overrides.insert(
            bytes::Bytes::copy_from_slice(&[0u8; 20]),
            bytes::Bytes::copy_from_slice(&[0u8; 32]),
            bytes::Bytes::copy_from_slice(&[1u8; 32]),
        );

        let mut amount_bytes = vec![0u8; 32];
        amount_bytes[24..32].copy_from_slice(&100u64.to_be_bytes());
        asserter.push_success(&alloy::primitives::Bytes::copy_from_slice(&amount_bytes));
        asserter.push_success(&21_000u64);

        let order = make_signed_swap();
        let opts = ExecutionOptions { dry_run: true, storage_overrides: Some(overrides) };
        let receipt = client
            .execute_swap(order, &opts)
            .await
            .expect("execute with overrides should succeed");
        receipt.await.expect("should resolve");
    }

    #[tokio::test]
    async fn execute_dry_run_returns_simulation_failed_on_call_error() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), Some(sender));

        asserter.push_failure_msg("execution reverted");

        let order = make_signed_swap();
        let opts = ExecutionOptions { dry_run: true, storage_overrides: None };
        let result = client.execute_swap(order, &opts).await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected SimulationFailed error"),
        };

        assert!(
            matches!(err, FyndError::SimulationFailed(_)),
            "expected SimulationFailed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn execute_dry_run_with_empty_return_data_has_no_settled_amount() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), Some(sender));

        asserter.push_success(&alloy::primitives::Bytes::new());
        asserter.push_success(&21_000u64);

        let order = make_signed_swap();
        let opts = ExecutionOptions { dry_run: true, storage_overrides: None };
        let receipt = client
            .execute_swap(order, &opts)
            .await
            .expect("execute should succeed");
        let settled = receipt.await.expect("should resolve");

        assert!(
            settled.settled_amount().is_none(),
            "empty return data should yield None settled_amount"
        );
    }

    #[tokio::test]
    async fn swap_payload_returns_protocol_error_when_no_transaction() {
        use crate::types::{BackendKind, BlockInfo, QuoteStatus};

        let sender = Address::with_last_byte(0xab);
        let (client, _asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), None);

        // Build a quote with no transaction (encoding_options not set in request)
        let quote = crate::types::Quote::new(
            "no-tx".to_string(),
            QuoteStatus::Success,
            BackendKind::Fynd,
            None,
            num_bigint::BigUint::from(1_000u64),
            num_bigint::BigUint::from(990u64),
            num_bigint::BigUint::from(50_000u64),
            num_bigint::BigUint::from(940u64),
            None,
            BlockInfo::new(1, "0xabc".to_string(), 0),
            bytes::Bytes::copy_from_slice(&[0xbb; 20]),
            bytes::Bytes::copy_from_slice(&[0xcc; 20]),
            None,
            None,
        );
        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(1),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: Some(100_000),
            simulate: false,
        };

        let err = client
            .swap_payload(quote, &hints)
            .await
            .unwrap_err();

        assert!(
            matches!(err, FyndError::Protocol(_)),
            "expected Protocol error when quote has no transaction, got {err:?}"
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

    // ========================================================================
    // info() tests
    // ========================================================================

    fn make_info_body() -> serde_json::Value {
        serde_json::json!({
            "chain_id": 1,
            "router_address": "0x0101010101010101010101010101010101010101",
            "permit2_address": "0x0202020202020202020202020202020202020202"
        })
    }

    #[tokio::test]
    async fn info_fetches_and_caches() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_info_body()))
            .expect(1) // only one HTTP hit expected despite two calls
            .mount(&server)
            .await;

        let (client, _asserter) = make_test_client(server.uri(), RetryConfig::default(), None);

        let info1 = client
            .info()
            .await
            .expect("first info call should succeed");
        let info2 = client
            .info()
            .await
            .expect("second info call should use cache");

        assert_eq!(info1.chain_id(), 1);
        assert_eq!(info2.chain_id(), 1);
        assert_eq!(info1.router_address().as_ref(), &[0x01u8; 20]);
        assert_eq!(info1.permit2_address().as_ref(), &[0x02u8; 20]);
        // MockServer verifies expect(1) on drop.
    }

    // ========================================================================
    // approval() tests
    // ========================================================================

    #[tokio::test]
    async fn approval_builds_correct_calldata() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_info_body()))
            .expect(1)
            .mount(&server)
            .await;

        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client(server.uri(), RetryConfig::default(), Some(sender));

        // Hints provide nonce + fees so no RPC calls needed.
        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(3),
            max_fee_per_gas: Some(2_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: None, // should default to 65_000
            simulate: false,
        };
        // eth_estimateGas → 65_000
        asserter.push_success(&65_000u64);

        let params = ApprovalParams::new(
            bytes::Bytes::copy_from_slice(&[0xdd; 20]),
            num_bigint::BigUint::from(1_000_000u64),
            false,
        );

        let payload = client
            .approval(&params, &hints)
            .await
            .expect("approval should succeed")
            .expect("should build payload when check_allowance is false");

        // Verify function selector is approve(address,uint256) = 0x095ea7b3.
        let selector = &payload.tx().input[0..4];
        assert_eq!(selector, &[0x09, 0x5e, 0xa7, 0xb3]);
        assert_eq!(payload.tx().gas_limit, 65_000, "gas limit should come from eth_estimateGas");
        assert_eq!(payload.tx().nonce, 3);
    }

    #[tokio::test]
    async fn approval_with_insufficient_allowance_returns_some() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_info_body()))
            .expect(1)
            .mount(&server)
            .await;

        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client(server.uri(), RetryConfig::default(), Some(sender));

        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(0),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: None,
            simulate: false,
        };

        // Mock eth_call for allowance: return 0 (allowance insufficient).
        let zero_allowance = alloy::primitives::Bytes::copy_from_slice(&[0u8; 32]);
        asserter.push_success(&zero_allowance);
        // eth_estimateGas → 65_000
        asserter.push_success(&65_000u64);

        let params = ApprovalParams::new(
            bytes::Bytes::copy_from_slice(&[0xdd; 20]),
            num_bigint::BigUint::from(500_000u64),
            true,
        );

        let result = client
            .approval(&params, &hints)
            .await
            .expect("approval with allowance check should succeed");

        assert!(result.is_some(), "zero allowance should return a payload");
    }

    #[tokio::test]
    async fn approval_with_sufficient_allowance_returns_none() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_info_body()))
            .expect(1)
            .mount(&server)
            .await;

        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client(server.uri(), RetryConfig::default(), Some(sender));

        let hints = SigningHints {
            sender: Some(sender),
            nonce: Some(0),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas_limit: None,
            simulate: false,
        };

        // Mock eth_call for allowance: return amount > requested (allowance sufficient).
        let mut allowance_bytes = [0u8; 32];
        // Encode 1_000_000 as big-endian uint256 (same as the amount we will request).
        allowance_bytes[24..32].copy_from_slice(&1_000_000u64.to_be_bytes());
        asserter.push_success(&alloy::primitives::Bytes::copy_from_slice(&allowance_bytes));

        // Request 500_000, but allowance is 1_000_000 — sufficient.
        let params = ApprovalParams::new(
            bytes::Bytes::copy_from_slice(&[0xdd; 20]),
            num_bigint::BigUint::from(500_000u64),
            true,
        );

        let result = client
            .approval(&params, &hints)
            .await
            .expect("approval with sufficient allowance check should succeed");

        assert!(result.is_none(), "sufficient allowance should return None");
    }

    // ========================================================================
    // execute_approval() tests
    // ========================================================================

    fn make_signed_approval() -> crate::signing::SignedApproval {
        use alloy::primitives::{Signature, TxKind, U256};

        use crate::signing::ApprovalPayload;

        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            gas_limit: 65_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: AlloyBytes::from(vec![0x09, 0x5e, 0xa7, 0xb3]),
            access_list: AccessList::default(),
        };
        let payload = ApprovalPayload {
            tx,
            token: bytes::Bytes::copy_from_slice(&[0xdd; 20]),
            spender: bytes::Bytes::copy_from_slice(&[0x01; 20]),
            amount: num_bigint::BigUint::from(1_000_000u64),
        };
        SignedApproval::assemble(payload, Signature::test_signature())
    }

    #[tokio::test]
    async fn execute_approval_broadcasts_and_polls() {
        let sender = Address::with_last_byte(0xab);
        let (client, asserter) =
            make_test_client("http://localhost".to_string(), RetryConfig::default(), Some(sender));

        // send_raw_transaction response: tx hash
        let tx_hash = alloy::primitives::B256::repeat_byte(0xef);
        asserter.push_success(&tx_hash);

        // get_transaction_receipt: first call returns null (pending), second returns receipt.
        asserter.push_success::<Option<()>>(&None);
        let receipt = alloy::rpc::types::TransactionReceipt {
            inner: alloy::consensus::ReceiptEnvelope::Eip1559(alloy::consensus::ReceiptWithBloom {
                receipt: alloy::consensus::Receipt::<alloy::primitives::Log> {
                    status: alloy::consensus::Eip658Value::Eip658(true),
                    cumulative_gas_used: 50_000,
                    logs: vec![],
                },
                logs_bloom: alloy::primitives::Bloom::default(),
            }),
            transaction_hash: tx_hash,
            transaction_index: None,
            block_hash: None,
            block_number: None,
            gas_used: 45_000,
            effective_gas_price: 1_500_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        };
        asserter.push_success(&receipt);

        let approval = make_signed_approval();
        let tx_receipt = client
            .execute_approval(approval)
            .await
            .expect("execute_approval should succeed");

        let mined = tx_receipt
            .await
            .expect("receipt should resolve");

        assert_eq!(mined.tx_hash(), tx_hash);
        let expected_cost =
            num_bigint::BigUint::from(45_000u64) * num_bigint::BigUint::from(1_500_000_000u64);
        assert_eq!(mined.gas_cost(), &expected_cost);
    }
}
