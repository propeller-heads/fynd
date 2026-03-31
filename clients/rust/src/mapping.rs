//! Conversions between client types and the Fynd RPC server's DTO types.
//!
//! Uses `fynd-rpc-types` directly for the DTO format, providing compile-time
//! compatibility guarantees with the server.

use fynd_rpc_types as dto;
use fynd_rpc_types::OrderQuote;

use crate::{
    error::{ErrorCode, FyndError},
    types::{
        BackendKind, BatchQuote, BlockInfo, EncodingOptions, FeeBreakdown, HealthStatus, Order,
        OrderSide, PermitDetails, PermitSingle, Quote, QuoteOptions, QuoteParams, QuoteStatus,
        Route, Swap, Transaction, UserTransferType,
    },
};
// ============================================================================
// ADDRESS CONVERSION HELPERS
// ============================================================================

pub(crate) fn bytes_to_alloy_address(
    b: &bytes::Bytes,
) -> Result<alloy::primitives::Address, FyndError> {
    let arr: [u8; 20] = b.as_ref().try_into().map_err(|_| {
        FyndError::Protocol(format!("expected 20-byte address, got {} bytes", b.len()))
    })?;

    Ok(alloy::primitives::Address::from(arr))
}

/// Wrap a client `bytes::Bytes` address as a DTO address, validating the 20-byte length.
fn bytes_to_dto_addr(b: &bytes::Bytes) -> Result<dto::Bytes, FyndError> {
    if b.len() != 20 {
        return Err(FyndError::Protocol(format!("expected 20-byte address, got {} bytes", b.len())));
    }
    Ok(dto::Bytes::from(b.as_ref()))
}

/// Unwrap a DTO address back to a client `bytes::Bytes`.
fn dto_addr_to_bytes(b: dto::Bytes) -> bytes::Bytes {
    b.0
}

// ============================================================================
// PRIMITIVE CONVERSIONS
// ============================================================================

/// Convert a [`num_bigint::BigUint`] to an [`alloy::primitives::U256`].
pub(crate) fn biguint_to_u256(n: &num_bigint::BigUint) -> alloy::primitives::U256 {
    alloy::primitives::U256::from_be_slice(&n.to_bytes_be())
}

// ============================================================================
// CLIENT TYPES → DTO FORMAT
// ============================================================================

pub(crate) fn quote_params_to_dto(params: QuoteParams) -> Result<dto::QuoteRequest, FyndError> {
    let order = dto::Order::try_from(params.order)?;
    let options = dto::QuoteOptions::try_from(params.options)?;
    Ok(dto::QuoteRequest::new(vec![order]).with_options(options))
}

impl TryFrom<Order> for dto::Order {
    type Error = FyndError;

    fn try_from(order: Order) -> Result<Self, Self::Error> {
        let token_in = bytes_to_dto_addr(order.token_in())?;
        let token_out = bytes_to_dto_addr(order.token_out())?;
        let sender = bytes_to_dto_addr(order.sender())?;
        let receiver = order
            .receiver()
            .map(bytes_to_dto_addr)
            .transpose()?;
        let mut dto_order = dto::Order::new(
            token_in,
            token_out,
            order.amount().clone(),
            order.side().into(),
            sender,
        );
        if let Some(r) = receiver {
            dto_order = dto_order.with_receiver(r);
        }
        Ok(dto_order)
    }
}

impl From<OrderSide> for dto::OrderSide {
    fn from(side: OrderSide) -> Self {
        match side {
            OrderSide::Sell => dto::OrderSide::Sell,
        }
    }
}

impl TryFrom<QuoteOptions> for dto::QuoteOptions {
    type Error = FyndError;

    fn try_from(opts: QuoteOptions) -> Result<Self, Self::Error> {
        let mut dto_opts = dto::QuoteOptions::default();
        if let Some(ms) = opts.timeout_ms {
            dto_opts = dto_opts.with_timeout_ms(ms);
        }
        if let Some(n) = opts.min_responses {
            dto_opts = dto_opts.with_min_responses(n);
        }
        if let Some(gas) = opts.max_gas {
            dto_opts = dto_opts.with_max_gas(gas);
        }
        if let Some(enc) = opts.encoding_options {
            dto_opts = dto_opts.with_encoding_options(dto::EncodingOptions::try_from(enc)?);
        }
        if let Some(pg) = opts.price_guard {
            dto_opts = dto_opts.with_price_guard(pg);
        }
        Ok(dto_opts)
    }
}

impl TryFrom<EncodingOptions> for dto::EncodingOptions {
    type Error = FyndError;

    fn try_from(opts: EncodingOptions) -> Result<Self, Self::Error> {
        let mut dto_opts =
            dto::EncodingOptions::new(opts.slippage).with_transfer_type(opts.transfer_type.into());
        if let (Some(permit), Some(sig)) = (
            opts.permit
                .map(dto::PermitSingle::try_from)
                .transpose()?,
            opts.permit2_signature
                .map(|b| dto::Bytes::from(b.as_ref())),
        ) {
            dto_opts = dto_opts.with_permit2(permit, sig);
        }
        if let Some(fee) = opts.client_fee_params {
            dto_opts = dto_opts.with_client_fee_params(dto::ClientFeeParams::new(
                fee.bps,
                dto::Bytes::from(fee.receiver.as_ref()),
                fee.max_contribution,
                fee.deadline,
                dto::Bytes::from(
                    fee.signature
                        .unwrap_or_default()
                        .as_ref(),
                ),
            ));
        }
        Ok(dto_opts)
    }
}

impl TryFrom<PermitSingle> for dto::PermitSingle {
    type Error = FyndError;

    fn try_from(p: PermitSingle) -> Result<Self, Self::Error> {
        let details = dto::PermitDetails::try_from(p.details)?;
        let spender = bytes_to_dto_addr(&p.spender)?;
        Ok(dto::PermitSingle::new(details, spender, p.sig_deadline))
    }
}

impl TryFrom<PermitDetails> for dto::PermitDetails {
    type Error = FyndError;

    fn try_from(d: PermitDetails) -> Result<Self, Self::Error> {
        let token = bytes_to_dto_addr(&d.token)?;
        Ok(dto::PermitDetails::new(token, d.amount, d.expiration, d.nonce))
    }
}

impl From<UserTransferType> for dto::UserTransferType {
    fn from(t: UserTransferType) -> Self {
        match t {
            UserTransferType::TransferFrom => dto::UserTransferType::TransferFrom,
            UserTransferType::TransferFromPermit2 => dto::UserTransferType::TransferFromPermit2,
            UserTransferType::UseVaultsFunds => dto::UserTransferType::UseVaultsFunds,
        }
    }
}

// ============================================================================
// DTO FORMAT → CLIENT TYPES
// ============================================================================

pub(crate) fn dto_to_quote(
    ds: OrderQuote,
    token_out: bytes::Bytes,
    receiver: bytes::Bytes,
) -> Result<Quote, FyndError> {
    let status = QuoteStatus::from(ds.status());
    let route = ds
        .route()
        .cloned()
        .map(Route::try_from)
        .transpose()?;
    let block = BlockInfo::from(ds.block().clone());
    let transaction = ds
        .transaction()
        .cloned()
        .map(Transaction::from);
    let fee_breakdown = ds.fee_breakdown().map(|fb| {
        FeeBreakdown::new(
            fb.router_fee().clone(),
            fb.client_fee().clone(),
            fb.max_slippage().clone(),
            fb.min_amount_received().clone(),
        )
    });
    Ok(Quote::new(
        ds.order_id().to_string(),
        status,
        BackendKind::Fynd,
        route,
        ds.amount_in().clone(),
        ds.amount_out().clone(),
        ds.gas_estimate().clone(),
        ds.amount_out_net_gas().clone(),
        ds.price_impact_bps(),
        block,
        token_out,
        receiver,
        transaction,
        fee_breakdown,
    ))
}

impl From<dto::Transaction> for Transaction {
    fn from(dt: dto::Transaction) -> Self {
        Transaction::new(
            bytes::Bytes::copy_from_slice(dt.to().as_ref()),
            dt.value().clone(),
            dt.data().to_vec(),
        )
    }
}

pub(crate) fn dto_to_batch_quote(
    ds: dto::Quote,
    token_out: bytes::Bytes,
    receiver: bytes::Bytes,
) -> Result<BatchQuote, FyndError> {
    let quotes = ds
        .into_orders()
        .into_iter()
        .map(|os| dto_to_quote(os, token_out.clone(), receiver.clone()))
        .collect::<Result<Vec<Quote>, _>>()?;
    Ok(BatchQuote::new(quotes))
}

impl From<dto::QuoteStatus> for QuoteStatus {
    fn from(ds: dto::QuoteStatus) -> Self {
        match ds {
            dto::QuoteStatus::Success => Self::Success,
            dto::QuoteStatus::NoRouteFound => Self::NoRouteFound,
            dto::QuoteStatus::InsufficientLiquidity => Self::InsufficientLiquidity,
            dto::QuoteStatus::Timeout => Self::Timeout,
            dto::QuoteStatus::NotReady => Self::NotReady,
            dto::QuoteStatus::PriceCheckFailed => Self::PriceCheckFailed,
            _ => Self::NotReady,
        }
    }
}

impl TryFrom<dto::Route> for Route {
    type Error = FyndError;

    fn try_from(dr: dto::Route) -> Result<Self, Self::Error> {
        let swaps = dr
            .into_swaps()
            .into_iter()
            .map(Swap::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Route::new(swaps))
    }
}

impl TryFrom<dto::Swap> for Swap {
    type Error = FyndError;

    fn try_from(ds: dto::Swap) -> Result<Self, Self::Error> {
        let token_in = dto_addr_to_bytes(ds.token_in().clone());
        let token_out = dto_addr_to_bytes(ds.token_out().clone());
        Ok(Swap::new(
            ds.component_id().to_string(),
            ds.protocol().to_string(),
            token_in,
            token_out,
            ds.amount_in().clone(),
            ds.amount_out().clone(),
            ds.gas_estimate().clone(),
            ds.split(),
        ))
    }
}

impl From<dto::BlockInfo> for BlockInfo {
    fn from(db: dto::BlockInfo) -> Self {
        BlockInfo::new(db.number(), db.hash().to_string(), db.timestamp())
    }
}

impl TryFrom<fynd_rpc_types::InstanceInfo> for crate::types::InstanceInfo {
    type Error = FyndError;

    fn try_from(dto: fynd_rpc_types::InstanceInfo) -> Result<Self, Self::Error> {
        let router = bytes::Bytes::copy_from_slice(dto.router_address().as_ref());
        let permit2 = bytes::Bytes::copy_from_slice(dto.permit2_address().as_ref());
        if router.len() != 20 {
            return Err(FyndError::Protocol(format!(
                "router_address must be 20 bytes, got {}",
                router.len()
            )));
        }
        if permit2.len() != 20 {
            return Err(FyndError::Protocol(format!(
                "permit2_address must be 20 bytes, got {}",
                permit2.len()
            )));
        }
        Ok(crate::types::InstanceInfo::new(router, permit2, dto.chain_id()))
    }
}

impl From<dto::HealthStatus> for HealthStatus {
    fn from(dh: dto::HealthStatus) -> Self {
        HealthStatus::new(
            dh.healthy(),
            dh.last_update_ms(),
            dh.num_solver_pools(),
            dh.derived_data_ready(),
            dh.gas_price_age_ms(),
        )
    }
}

// ============================================================================
// ERROR CONVERSION
// ============================================================================

pub(crate) fn dto_error_to_fynd(de: dto::ErrorResponse) -> FyndError {
    let code = ErrorCode::from_server_code(de.code());
    FyndError::Api { code, message: de.error().to_string() }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use num_bigint::BigUint;

    use super::*;

    fn sample_dto_swap() -> dto::Swap {
        serde_json::from_str(
            r#"{
            "component_id": "pool-1",
            "protocol": "uniswap-v3",
            "token_in": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "token_out": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "amount_in": "100",
            "amount_out": "99",
            "gas_estimate": "50000",
            "split": "0"
        }"#,
        )
        .expect("valid swap JSON")
    }

    fn sample_dto_block() -> dto::BlockInfo {
        serde_json::from_str(
            r#"{
            "number": 21000000,
            "hash": "0xdeadbeef",
            "timestamp": 1730000000
        }"#,
        )
        .expect("valid block JSON")
    }

    fn sample_dto_order_quote() -> dto::OrderQuote {
        serde_json::from_str(
            r#"{
            "order_id": "test-order-id",
            "status": "success",
            "amount_in": "1000",
            "amount_out": "999",
            "gas_estimate": "100000",
            "price_impact_bps": 5,
            "amount_out_net_gas": "998",
            "block": {"number": 21000000, "hash": "0xdeadbeef", "timestamp": 1730000000}
        }"#,
        )
        .expect("valid order quote JSON")
    }

    // -----------------------------------------------------------------------
    // biguint_to_u256
    // -----------------------------------------------------------------------

    #[test]
    fn biguint_to_u256_zero() {
        let result = biguint_to_u256(&BigUint::ZERO);
        assert_eq!(result, alloy::primitives::U256::ZERO);
    }

    #[test]
    fn biguint_to_u256_known_value() {
        let n = BigUint::from(0x1234_5678u64);
        let result = biguint_to_u256(&n);
        assert_eq!(result, alloy::primitives::U256::from(0x1234_5678u64));
    }

    // -----------------------------------------------------------------------
    // Transaction conversion
    // -----------------------------------------------------------------------

    #[test]
    fn transaction_from_dto() {
        let router_bytes = vec![0x01u8; 20];
        let dto_tx = dto::Transaction::new(
            dto::Bytes::from(router_bytes.as_slice()),
            BigUint::ZERO,
            vec![0x12, 0x34],
        );
        let tx = Transaction::from(dto_tx);
        assert_eq!(tx.to().as_ref(), router_bytes.as_slice());
        assert_eq!(tx.value(), &BigUint::ZERO);
        assert_eq!(tx.data(), &[0x12, 0x34]);
    }

    // -----------------------------------------------------------------------
    // bytes_to_alloy_address
    // -----------------------------------------------------------------------

    #[test]
    fn bytes_to_alloy_address_happy_path() {
        let b = Bytes::copy_from_slice(&[0xab; 20]);
        let addr = bytes_to_alloy_address(&b).unwrap();
        assert_eq!(addr.as_slice(), &[0xab; 20]);
    }

    #[test]
    fn bytes_to_alloy_address_wrong_length() {
        let b = Bytes::copy_from_slice(&[0xab; 4]);
        assert!(matches!(bytes_to_alloy_address(&b), Err(FyndError::Protocol(_))));
    }

    // -----------------------------------------------------------------------
    // Swap conversion
    // -----------------------------------------------------------------------

    #[test]
    fn swap_try_from_dto_happy_path() {
        let client_swap = Swap::try_from(sample_dto_swap()).unwrap();
        assert_eq!(client_swap.component_id(), "pool-1");
        assert_eq!(client_swap.protocol(), "uniswap-v3");
        assert_eq!(client_swap.token_in(), &Bytes::copy_from_slice(&[0xaa; 20]));
        assert_eq!(client_swap.token_out(), &Bytes::copy_from_slice(&[0xbb; 20]));
        assert_eq!(client_swap.amount_in(), &BigUint::from(100u32));
        assert_eq!(client_swap.amount_out(), &BigUint::from(99u32));
        assert_eq!(client_swap.gas_estimate(), &BigUint::from(50_000u32));
    }

    // -----------------------------------------------------------------------
    // QuoteStatus conversion
    // -----------------------------------------------------------------------

    #[test]
    fn quote_status_all_variants() {
        use dto::QuoteStatus as Dto;
        assert!(matches!(QuoteStatus::from(Dto::Success), QuoteStatus::Success));
        assert!(matches!(QuoteStatus::from(Dto::NoRouteFound), QuoteStatus::NoRouteFound));
        assert!(matches!(
            QuoteStatus::from(Dto::InsufficientLiquidity),
            QuoteStatus::InsufficientLiquidity
        ));
        assert!(matches!(QuoteStatus::from(Dto::Timeout), QuoteStatus::Timeout));
        assert!(matches!(QuoteStatus::from(Dto::NotReady), QuoteStatus::NotReady));
        assert!(matches!(QuoteStatus::from(Dto::PriceCheckFailed), QuoteStatus::PriceCheckFailed));
    }

    // -----------------------------------------------------------------------
    // BlockInfo conversion
    // -----------------------------------------------------------------------

    #[test]
    fn block_info_from_dto() {
        let dto_block = sample_dto_block();
        let block = BlockInfo::from(dto_block);
        assert_eq!(block.number(), 21_000_000);
        assert_eq!(block.hash(), "0xdeadbeef");
        assert_eq!(block.timestamp(), 1_730_000_000);
    }

    // -----------------------------------------------------------------------
    // OrderQuote conversion
    // -----------------------------------------------------------------------

    #[test]
    fn quote_from_dto() {
        let ds = sample_dto_order_quote();
        let quote = dto_to_quote(ds, Bytes::new(), Bytes::new()).unwrap();
        assert_eq!(quote.order_id(), "test-order-id");
        assert!(matches!(quote.status(), QuoteStatus::Success));
        assert!(matches!(quote.backend(), BackendKind::Fynd));
        assert_eq!(quote.amount_in(), &BigUint::from(1_000u32));
        assert_eq!(quote.amount_out(), &BigUint::from(999u32));
        assert_eq!(quote.gas_estimate(), &BigUint::from(100_000u32));
        assert_eq!(quote.amount_out_net_gas(), &BigUint::from(998u32));
        assert_eq!(quote.price_impact_bps(), Some(5));
        // token_out and receiver are left empty until populated by quote()
        assert!(quote.token_out().is_empty());
        assert!(quote.receiver().is_empty());
    }

    // -----------------------------------------------------------------------
    // Order → dto::Order conversion
    // -----------------------------------------------------------------------

    #[test]
    fn order_try_from_client_encodes_addresses_as_tycho() {
        let order = Order::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(1_000u32),
            OrderSide::Sell,
            Bytes::copy_from_slice(&[0xcc; 20]),
            None,
        );

        let dto_order = dto::Order::try_from(order).unwrap();
        assert_eq!(dto_order.token_in().as_ref(), &[0xaa; 20]);
        assert_eq!(dto_order.token_out().as_ref(), &[0xbb; 20]);
        assert_eq!(dto_order.sender().as_ref(), &[0xcc; 20]);
        assert!(dto_order.receiver().is_none());
        assert_eq!(dto_order.amount(), &BigUint::from(1_000u32));
    }

    #[test]
    fn order_try_from_client_with_receiver() {
        let order = Order::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(1u32),
            OrderSide::Sell,
            Bytes::copy_from_slice(&[0xcc; 20]),
            Some(Bytes::copy_from_slice(&[0xdd; 20])),
        );

        let dto_order = dto::Order::try_from(order).unwrap();
        let receiver = dto_order.receiver().unwrap();
        assert_eq!(receiver.as_ref(), &[0xdd; 20]);
    }

    #[test]
    fn order_try_from_client_invalid_address_length() {
        let order = Order::new(
            Bytes::copy_from_slice(&[0xaa; 4]), // wrong length
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(1u32),
            OrderSide::Sell,
            Bytes::copy_from_slice(&[0xcc; 20]),
            None,
        );
        assert!(matches!(dto::Order::try_from(order), Err(FyndError::Protocol(_))));
    }

    // -----------------------------------------------------------------------
    // UserTransferType mapping
    // -----------------------------------------------------------------------

    #[test]
    fn user_transfer_type_permit2_maps_correctly() {
        let result = dto::UserTransferType::from(UserTransferType::TransferFromPermit2);
        assert!(matches!(result, dto::UserTransferType::TransferFromPermit2));
    }

    #[test]
    fn user_transfer_type_vault_funds_maps_correctly() {
        let result = dto::UserTransferType::from(UserTransferType::UseVaultsFunds);
        assert!(matches!(result, dto::UserTransferType::UseVaultsFunds));
    }

    // -----------------------------------------------------------------------
    // PermitDetails TryFrom
    // -----------------------------------------------------------------------

    #[test]
    fn permit_details_try_from_happy_path() {
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            BigUint::from(1_000u32),
            BigUint::from(9_999_999u32),
            BigUint::from(0u32),
        );
        let dto_details = dto::PermitDetails::try_from(details).unwrap();
        assert_eq!(dto_details.token().as_ref(), &[0xaa; 20]);
        assert_eq!(dto_details.amount(), &BigUint::from(1_000u32));
        assert_eq!(dto_details.expiration(), &BigUint::from(9_999_999u32));
        assert_eq!(dto_details.nonce(), &BigUint::from(0u32));
    }

    #[test]
    fn permit_details_try_from_invalid_token() {
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 4]), // wrong length
            BigUint::from(1u32),
            BigUint::from(1u32),
            BigUint::from(0u32),
        );
        assert!(matches!(dto::PermitDetails::try_from(details), Err(FyndError::Protocol(_))));
    }

    // -----------------------------------------------------------------------
    // PermitSingle TryFrom
    // -----------------------------------------------------------------------

    #[test]
    fn permit_single_try_from_happy_path() {
        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            BigUint::from(500u32),
            BigUint::from(1_000_000u32),
            BigUint::from(1u32),
        );
        let permit = PermitSingle::new(
            details,
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(2_000_000u32),
        );
        let dto_permit = dto::PermitSingle::try_from(permit).unwrap();
        assert_eq!(dto_permit.spender().as_ref(), &[0xbb; 20]);
        assert_eq!(dto_permit.sig_deadline(), &BigUint::from(2_000_000u32));
        assert_eq!(dto_permit.details().amount(), &BigUint::from(500u32));
    }

    // -----------------------------------------------------------------------
    // EncodingOptions TryFrom with permit2
    // -----------------------------------------------------------------------

    #[test]
    fn encoding_options_try_from_with_permit2() {
        use crate::types::{EncodingOptions, PermitDetails, PermitSingle};

        let details = PermitDetails::new(
            Bytes::copy_from_slice(&[0xaa; 20]),
            BigUint::from(1_000u32),
            BigUint::from(9_999_999u32),
            BigUint::from(0u32),
        );
        let permit = PermitSingle::new(
            details,
            Bytes::copy_from_slice(&[0xbb; 20]),
            BigUint::from(9_999_999u32),
        );
        let sig = Bytes::copy_from_slice(&[0xcc; 65]);
        let opts = EncodingOptions::new(0.005)
            .with_permit2(permit, sig.clone())
            .unwrap();

        let dto_opts = dto::EncodingOptions::try_from(opts).unwrap();
        assert!(matches!(dto_opts.transfer_type(), dto::UserTransferType::TransferFromPermit2));
        assert!(dto_opts.permit().is_some());
        assert_eq!(
            dto_opts
                .permit2_signature()
                .unwrap()
                .as_ref(),
            sig.as_ref()
        );
    }

    // -----------------------------------------------------------------------
    // EncodingOptions TryFrom with client fee
    // -----------------------------------------------------------------------

    #[test]
    fn encoding_options_try_from_with_client_fee() {
        use crate::types::{ClientFeeParams, EncodingOptions};

        let fee = ClientFeeParams::new(
            100,
            Bytes::copy_from_slice(&[0x44; 20]),
            BigUint::from(500_000u64),
            1_893_456_000u64,
        )
        .with_signature(Bytes::copy_from_slice(&[0xAB; 65]));
        let opts = EncodingOptions::new(0.01).with_client_fee(fee);

        let dto_opts = dto::EncodingOptions::try_from(opts).unwrap();
        assert!(dto_opts.client_fee_params().is_some());
        let dto_fee = dto_opts.client_fee_params().unwrap();
        assert_eq!(dto_fee.bps(), 100);
        assert_eq!(*dto_fee.max_contribution(), BigUint::from(500_000u64));
        assert_eq!(dto_fee.deadline(), 1_893_456_000u64);
        assert_eq!(dto_fee.signature().len(), 65);
    }

    #[test]
    fn encoding_options_try_from_without_client_fee() {
        use crate::types::EncodingOptions;

        let opts = EncodingOptions::new(0.005);
        let dto_opts = dto::EncodingOptions::try_from(opts).unwrap();
        assert!(dto_opts.client_fee_params().is_none());
    }
}
