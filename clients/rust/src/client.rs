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

pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
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
#[derive(Default)]
pub struct SigningHints {
    pub sender: Option<Address>,
    pub nonce: Option<u64>,
    pub max_fee_per_gas: Option<u128>,
    pub max_priority_fee_per_gas: Option<u128>,
    pub gas_limit: Option<u64>,
    pub simulate: bool,
}

// ============================================================================
// CLIENT BUILDER
// ============================================================================

pub struct FyndClientBuilder {
    base_url: String,
    timeout: Duration,
    retry: RetryConfig,
    router_address: String,
    chain_id: u64,
    rpc_url: String,
    submit_url: Option<String>,
    sender: Option<Address>,
}

impl FyndClientBuilder {
    pub fn new(
        base_url: impl Into<String>,
        router_address: impl Into<String>,
        chain_id: u64,
        rpc_url: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            timeout: Duration::from_secs(30),
            retry: RetryConfig::default(),
            router_address: router_address.into(),
            chain_id,
            rpc_url: rpc_url.into(),
            submit_url: None,
            sender: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_submit_url(mut self, url: impl Into<String>) -> Self {
        self.submit_url = Some(url.into());
        self
    }

    pub fn with_sender(mut self, sender: Address) -> Self {
        self.sender = Some(sender);
        self
    }

    pub fn build(self) -> Result<FyndClient, FyndError> {
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

        // Parse router address.
        let router_address: Address = self
            .router_address
            .parse()
            .map_err(|e| FyndError::Config(format!("invalid router address: {e}")))?;

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
            chain_id: self.chain_id,
            default_sender: self.sender,
            provider,
            submit_provider,
        })
    }
}

// ============================================================================
// FYND CLIENT
// ============================================================================

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
    pub async fn quote(&self, params: QuoteParams) -> Result<Quote, FyndError> {
        let wire_request = mapping::quote_params_to_wire(params)?;
        let mut delay = self.retry.initial_backoff;
        for attempt in 0..self.retry.max_attempts {
            match self.do_quote(&wire_request).await {
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
        wire_request: &mapping::WireSolutionRequest,
    ) -> Result<Quote, FyndError> {
        let url = format!("{}/v1/solve", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(wire_request)
            .send()
            .await?;
        if !response.status().is_success() {
            let wire_err: mapping::WireErrorResponse = response.json().await?;
            return Err(mapping::wire_error_to_fynd(wire_err));
        }
        let wire_solution: mapping::WireSolution = response.json().await?;
        mapping::wire_to_quote(wire_solution)
    }

    /// Get the health status of the Fynd RPC server.
    pub async fn health(&self) -> Result<HealthStatus, FyndError> {
        let url = format!("{}/v1/health", self.base_url);
        let response = self.http.get(&url).send().await?;
        if !response.status().is_success() {
            let wire_err: mapping::WireErrorResponse = response.json().await?;
            return Err(mapping::wire_error_to_fynd(wire_err));
        }
        let wh: mapping::WireHealthStatus = response.json().await?;
        Ok(mapping::wire_to_health(wh))
    }

    /// Build a signable payload for a given order solution.
    ///
    /// `token_out` and `receiver` are the raw 20-byte addresses from the original order.
    /// They are stored in the payload and used later by `execute()` to decode settlement logs.
    pub async fn signable_payload(
        &self,
        solution: OrderSolution,
        token_out: Bytes,
        receiver: Bytes,
        hints: SigningHints,
    ) -> Result<SignablePayload, FyndError> {
        match solution.backend() {
            BackendKind::Fynd => {
                self.fynd_signable_payload(solution, token_out, receiver, hints)
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
        token_out: Bytes,
        receiver: Bytes,
        hints: SigningHints,
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

    /// Broadcast a signed order and return an `ExecutionReceipt` that resolves once confirmed.
    pub async fn execute(&self, order: SignedOrder) -> Result<ExecutionReceipt, FyndError> {
        let SignedOrder { payload, signature } = order;
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
                        return Ok(SettledOrder { tx_receipt: receipt, settled_amount, gas_cost });
                    }
                    None => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
        })))
    }
}
