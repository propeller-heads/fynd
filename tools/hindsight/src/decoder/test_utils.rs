use alloy::{
    primitives::{Address, Bytes, Log as PrimitiveLog, B256, U256},
    rpc::types::{trace::geth::CallFrame, Log},
    sol_types::SolEvent,
};

use crate::decoder::net::{NetSwap, Transfer};

/// An address with `n` in its last byte, for readable test fixtures.
pub(crate) fn addr(n: u8) -> Address {
    let mut bytes = [0u8; 20];
    bytes[19] = n;
    Address::from(bytes)
}

/// A [`NetSwap`] literal, for concise assertions.
pub(crate) fn swap(
    token_in: Address,
    amount_in: u64,
    token_out: Address,
    amount_out: u64,
) -> NetSwap {
    NetSwap {
        token_in,
        amount_in: U256::from(amount_in),
        token_out,
        amount_out: U256::from(amount_out),
    }
}

/// An ERC-20 `Transfer` log for `token` moving `value` from `from` to `to`.
pub(crate) fn make_transfer_log(token: Address, from: Address, to: Address, value: U256) -> Log {
    let event = Transfer { from, to, value };
    let log_data = event.encode_log_data();
    let primitive =
        PrimitiveLog::new_unchecked(token, log_data.topics().to_vec(), log_data.data.clone());
    Log { inner: primitive, ..Default::default() }
}

/// An ERC-721 `Transfer` log: same event signature as ERC-20 but all three parameters indexed
/// (four topics, empty data).
pub(crate) fn make_nft_transfer_log(
    token: Address,
    from: Address,
    to: Address,
    token_id: u64,
) -> Log {
    let topics = vec![
        Transfer::SIGNATURE_HASH,
        from.into_word(),
        to.into_word(),
        B256::from(U256::from(token_id)),
    ];
    let primitive = PrimitiveLog::new_unchecked(token, topics, Bytes::new());
    Log { inner: primitive, ..Default::default() }
}

/// A call frame moving `value` wei from `from` to `to` via call type `typ`.
pub(crate) fn frame(typ: &str, from: Address, to: Address, value: u64) -> CallFrame {
    CallFrame {
        from,
        to: Some(to),
        value: Some(U256::from(value)),
        typ: typ.to_string(),
        ..Default::default()
    }
}
