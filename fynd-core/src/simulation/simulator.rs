//! Simulation of encoded quotes using `eth_simulateV1` state overrides.

use std::sync::Mutex;

use alloy::{
    network::Ethereum,
    primitives::{map::B256HashMap, Address, Bytes, TxKind, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::{AccountOverride, StateOverride},
        TransactionRequest,
    },
    sol,
    sol_types::SolError,
};
use num_bigint::BigUint;
use rustc_hash::FxHashMap;
use tokio::time::timeout;
use tycho_simulation::tycho_common::models::Chain;

use crate::{
    encoding::encoder::PERMIT2_ADDRESS,
    simulation::erc20_slots::{find_slot_positions, Erc20SlotPositions},
    solver::defaults::SIMULATION_SLOT_DISCOVERY_TIMEOUT,
    EncodingOptions, OrderQuote, SimulationResult, UserTransferType,
};

sol! { error Error(string); }

/// Balance and allowance every simulated account is given.
///
/// Leaving the high bit clear avoids node-side arithmetic overflow while still funding any
/// practical quote.
const SIMULATION_FUNDING_VALUE: U256 = U256::MAX.wrapping_shr(1);

/// Simulates encoded quote transactions with temporary sender funding.
pub struct QuoteSimulator {
    provider: RootProvider<Ethereum>,
    slot_cache: Mutex<FxHashMap<Address, Erc20SlotPositions>>,
    native_token: Address,
    request_timeout: std::time::Duration,
}

pub(crate) enum SimulationAttempt {
    /// The simulated call completed.
    Success { amount_out: BigUint, gas_used: u64 },
    /// The simulated call reverted.
    Reverted { reason: String },
    /// Simulation could not be completed.
    Failure { reason: String },
}

impl QuoteSimulator {
    /// Creates a simulator that sends requests to `rpc_url` for `chain`.
    ///
    /// # Errors
    ///
    /// Returns an error when `rpc_url` is not a valid URL or the chain has no native token.
    pub fn new(
        rpc_url: &str,
        chain: Chain,
        request_timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let url = rpc_url
            .parse()
            .map_err(|error| format!("invalid RPC URL {rpc_url:?}: {error}"))?;
        let native_token = chain
            .try_native_token()
            .map_err(|error| format!("native token for {chain:?}: {error}"))?;
        Ok(Self::with_provider(
            ProviderBuilder::default().connect_http(url),
            Address::from_slice(native_token.address.as_ref()),
            request_timeout,
        ))
    }

    /// Simulates an encoded quote and reports its returned amount and gas used or a failure.
    pub async fn simulate(
        &self,
        quote: &OrderQuote,
        encoding_options: &EncodingOptions,
    ) -> SimulationResult {
        self.simulate_attempt(quote, encoding_options)
            .await
            .into_result()
    }

    pub(crate) async fn simulate_attempt(
        &self,
        quote: &OrderQuote,
        encoding_options: &EncodingOptions,
    ) -> SimulationAttempt {
        let transaction = match quote.transaction() {
            Some(value) => value,
            None => return failure("simulation setup failed: quote has no encoded transaction"),
        };
        let route = match quote.route() {
            Some(value) => value,
            None => return failure("simulation setup failed: quote has no route"),
        };
        let first_swap = match route.swaps().first() {
            Some(value) => value,
            None => return failure("simulation setup failed: route has no swaps"),
        };
        let sender = match crate::rpc::to_address(quote.sender(), "quote sender") {
            Ok(value) => value,
            Err(error) => return failure_with(format!("simulation setup failed: {error}")),
        };
        let router = match crate::rpc::to_address(transaction.to(), "transaction destination") {
            Ok(value) => value,
            Err(error) => return failure_with(format!("simulation setup failed: {error}")),
        };
        let token_in = match crate::rpc::to_address(first_swap.token_in(), "route token_in") {
            Ok(value) => value,
            Err(error) => return failure_with(format!("simulation setup failed: {error}")),
        };
        let overrides = match self
            .overrides(sender, token_in, router, encoding_options.transfer_type())
            .await
        {
            Ok(value) => value,
            Err(reason) => return failure_with(reason),
        };
        let value = U256::from_be_slice(
            transaction
                .value()
                .to_bytes_be()
                .as_slice(),
        );
        self.simulate_within_timeout(sender, router, value, transaction.data(), overrides)
            .await
    }

    /// Runs one simulated call, reporting a timeout as a failure rather than waiting forever.
    pub(crate) async fn simulate_within_timeout(
        &self,
        sender: Address,
        router: Address,
        value: U256,
        data: &[u8],
        overrides: StateOverride,
    ) -> SimulationAttempt {
        match timeout(
            self.request_timeout,
            simulate_with_overrides(&self.provider, sender, router, value, data, overrides),
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(_) => failure_with(format!(
                "simulation request timed out after {:?}",
                self.request_timeout
            )),
        }
    }

    pub(crate) fn with_provider(
        provider: RootProvider<Ethereum>,
        native_token: Address,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            provider,
            slot_cache: Mutex::new(FxHashMap::default()),
            native_token,
            request_timeout,
        }
    }

    async fn overrides(
        &self,
        sender: Address,
        token: Address,
        router: Address,
        transfer_type: &UserTransferType,
    ) -> Result<StateOverride, String> {
        if token == self.native_token {
            return Ok(native_balance_override(sender));
        }
        let (holder, spender) = match transfer_type {
            UserTransferType::UseVaultsFunds => (router, None),
            _ => (sender, Some(spender_for(router, transfer_type)?)),
        };
        let positions = self
            .cached_positions(token, holder, spender.unwrap_or(router))
            .await?;
        Ok(token_overrides(sender, token, holder, positions, spender))
    }

    async fn cached_positions(
        &self,
        token: Address,
        holder: Address,
        spender: Address,
    ) -> Result<Erc20SlotPositions, String> {
        if let Some(positions) = self
            .slot_cache
            .lock()
            .map_err(|_| "simulation slot cache lock poisoned".to_string())?
            .get(&token)
            .copied()
        {
            return Ok(positions);
        }
        let positions = timeout(SIMULATION_SLOT_DISCOVERY_TIMEOUT, find_slot_positions(&self.provider, token, holder, spender)).await
            .map_err(|_| format!("simulation ERC-20 slot discovery timed out after {SIMULATION_SLOT_DISCOVERY_TIMEOUT:?}"))?
            .map_err(|error| format!("simulation ERC-20 slot detection failed: {error}"))?;
        self.slot_cache
            .lock()
            .map_err(|_| "simulation slot cache lock poisoned".to_string())?
            .insert(token, positions);
        Ok(positions)
    }
}

impl SimulationAttempt {
    pub(crate) fn into_result(self) -> SimulationResult {
        match self {
            Self::Success { amount_out, gas_used } => {
                SimulationResult::Success { amount_out, gas_used }
            }
            Self::Reverted { reason } | Self::Failure { reason } => {
                SimulationResult::Failure { reason }
            }
        }
    }
}

fn failure(reason: &str) -> SimulationAttempt {
    failure_with(reason.to_string())
}
fn failure_with(reason: String) -> SimulationAttempt {
    SimulationAttempt::Failure { reason }
}

async fn simulate_with_overrides(
    provider: &RootProvider<Ethereum>,
    sender: Address,
    router: Address,
    value: U256,
    data: &[u8],
    overrides: StateOverride,
) -> SimulationAttempt {
    let call = TransactionRequest {
        from: Some(sender),
        to: Some(TxKind::Call(router)),
        value: Some(value),
        input: Bytes::copy_from_slice(data).into(),
        ..Default::default()
    };
    let payload = SimulatePayload::default().extend(
        SimBlock::default()
            .with_state_overrides(overrides)
            .call(call),
    );
    let response = match provider.simulate(&payload).await {
        Ok(value) => value,
        Err(error) => {
            return failure_with(format!(
                "simulation eth_simulateV1 failed: {}",
                rpc_error_reason(&error)
            ))
        }
    };
    let Some(block) = response.first() else {
        return failure("simulation eth_simulateV1 returned no blocks");
    };
    let Some(result) = block.calls.first() else {
        return failure("simulation eth_simulateV1 returned no call results");
    };
    if !result.status {
        let message = result
            .error
            .as_ref()
            .map_or("execution reverted", |error| error.message.as_str());
        return SimulationAttempt::Reverted {
            reason: format!(
                "simulation reverted: {}",
                decode_revert_reason(Some(result.return_data.as_ref()), message)
            ),
        };
    }
    if result.return_data.len() != 32 {
        return failure_with(format!(
            "simulation eth_simulateV1 returned {} bytes; expected exactly 32 bytes for uint256",
            result.return_data.len()
        ));
    }
    let amount = U256::from_be_slice(&result.return_data);
    SimulationAttempt::Success {
        amount_out: BigUint::from_bytes_be(&amount.to_be_bytes::<32>()),
        gas_used: result.gas_used,
    }
}

fn rpc_error_reason(
    error: &alloy::transports::RpcError<alloy::transports::TransportErrorKind>,
) -> String {
    let response = error.as_error_resp();
    let data = response.and_then(|response| response.as_revert_data());
    let message =
        response.map_or_else(|| error.to_string(), |response| response.message.to_string());
    decode_revert_reason(data.as_deref().map(AsRef::as_ref), &message)
}

fn spender_for(router: Address, transfer_type: &UserTransferType) -> Result<Address, String> {
    match transfer_type {
        UserTransferType::TransferFrom | UserTransferType::UseVaultsFunds => Ok(router),
        UserTransferType::TransferFromPermit2 => PERMIT2_ADDRESS
            .parse()
            .map_err(|error| format!("invalid Permit2 address: {error}")),
    }
}

fn native_balance_override(sender: Address) -> StateOverride {
    StateOverride::from_iter([(
        sender,
        AccountOverride::default().with_balance(SIMULATION_FUNDING_VALUE),
    )])
}

fn token_overrides(
    sender: Address,
    token: Address,
    holder: Address,
    positions: Erc20SlotPositions,
    spender: Option<Address>,
) -> StateOverride {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(positions.balance_slot(holder), B256::from(SIMULATION_FUNDING_VALUE));
    if let Some(spender) = spender {
        state_diff.insert(
            positions.allowance_slot(sender, spender),
            B256::from(SIMULATION_FUNDING_VALUE),
        );
    }
    StateOverride::from_iter([
        (sender, AccountOverride::default().with_balance(SIMULATION_FUNDING_VALUE)),
        (token, AccountOverride { state_diff: Some(state_diff), ..Default::default() }),
    ])
}

fn decode_revert_reason(revert_data: Option<&[u8]>, message: &str) -> String {
    let Some(data) = revert_data else {
        return message.to_string();
    };
    if data.starts_with(&Error::SELECTOR) {
        if let Ok(error) = Error::abi_decode_raw(&data[4..]) {
            return format!("reverted: {}", error.0);
        }
    }
    format!("reverted with data 0x{}", alloy::hex::encode(data))
}

#[cfg(test)]
#[path = "../tests/simulation/simulator.rs"]
mod tests;
