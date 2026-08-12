use std::collections::HashMap;

use alloy::{
    consensus::{Eip658Value, Receipt, ReceiptWithBloom},
    network::{AnyReceiptEnvelope, AnyTransactionReceipt},
    primitives::{address, Address, Bloom, Bytes, Log as PrimitiveLog, TxHash, B256, U256},
    providers::{DynProvider, Provider, RootProvider},
    rpc::{
        client::RpcClient,
        types::{trace::geth::CallFrame, Log, TransactionReceipt},
    },
    serde::WithOtherFields,
    sol_types::SolEvent,
    transports::mock::Asserter,
};

use crate::decoder::{
    decode::DecodeContext,
    registry::{Registry, VenueAddresses},
    transfer_ledger::{NetSwap, Transfer, TransferLedger},
};

/// The named venue's address-book section, for tests that construct a venue decoder directly.
/// Panics naming the venue when the book has no such section — a silent default (empty entry
/// points, no fee collectors) would let such a test pass without exercising the decoder.
pub(crate) fn venue_addresses(registry: &Registry, name: &str) -> VenueAddresses {
    let addresses = registry.venue(name);
    assert!(addresses.is_some(), "the address book has no venue '{name}'");
    addresses.unwrap().clone()
}

/// The canonical Permit2 deployment, for fixtures exercising the registry's infrastructure set.
pub(crate) const PERMIT2: Address = address!("0x000000000022d473030f116ddee9f6b43ac78ba3");

/// An address with `n` in its last byte, for readable test fixtures.
pub(crate) fn addr(n: u8) -> Address {
    let mut bytes = [0u8; 20];
    bytes[19] = n;
    Address::from(bytes)
}

/// A transaction hash with `n` in its last byte, for readable test fixtures.
pub(crate) fn tx_hash(n: u8) -> TxHash {
    let mut bytes = [0u8; 32];
    bytes[31] = n;
    TxHash::from(bytes)
}

/// A `NetSwap` literal, for concise assertions.
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

/// A log with an arbitrary non-`Transfer` topic0, standing in for a pool event (`Swap`, `Sync`,
/// …) whose payload sandwich detection never decodes — only the emitting address matters.
pub(crate) fn make_pool_log(pool: Address) -> Log {
    let primitive = PrimitiveLog::new_unchecked(pool, vec![B256::repeat_byte(0xAA)], Bytes::new());
    Log { inner: primitive, ..Default::default() }
}

/// Owns everything a `DecodeContext` borrows, so a decoder test builds one in two lines instead
/// of hand-assembling every field. The provider is a mocked `DynProvider` that answers nothing —
/// tests that need RPC answers push them via [`CtxFixture::with_asserter`].
pub(crate) struct CtxFixture {
    provider: DynProvider,
    code_cache: HashMap<Address, bool>,
    receipt: AnyTransactionReceipt,
    root: CallFrame,
    entry_point: Address,
}

impl CtxFixture {
    /// A fixture for a transaction `sender` sent into `entry_point`, with an empty receipt and a
    /// bare root frame. Replace [`CtxFixture::root`] for trace-walking decoders.
    pub(crate) fn new(sender: Address, entry_point: Address) -> Self {
        Self::with_asserter(sender, entry_point, &Asserter::new())
    }

    /// Like [`CtxFixture::new`], with a caller-held [`Asserter`] to feed the provider mocked RPC
    /// responses.
    pub(crate) fn with_asserter(
        sender: Address,
        entry_point: Address,
        asserter: &Asserter,
    ) -> Self {
        Self {
            provider: RootProvider::new(RpcClient::mocked(asserter.clone())).erased(),
            code_cache: HashMap::new(),
            receipt: receipt(tx_hash(1), sender, Some(entry_point), vec![]),
            root: frame("CALL", sender, entry_point, 0),
            entry_point,
        }
    }

    /// Replace the root trace frame, for decoders that walk the trace.
    pub(crate) fn set_root(&mut self, root: CallFrame) {
        self.root = root;
    }

    /// Replace the receipt's logs, for decoders that read events.
    pub(crate) fn set_logs(&mut self, logs: Vec<Log>) {
        self.receipt = receipt(tx_hash(1), self.receipt.from, self.receipt.to, logs);
    }

    /// The `DecodeContext` over this fixture's fields and the caller's registry, ledger, and
    /// calldata.
    pub(crate) fn ctx<'a>(
        &'a mut self,
        registry: &'a Registry,
        transfer_ledger: &'a TransferLedger,
        input: &'a [u8],
    ) -> DecodeContext<'a> {
        DecodeContext {
            provider: &self.provider,
            registry,
            code_cache: &mut self.code_cache,
            receipt: &self.receipt,
            entry_point: self.entry_point,
            transfer_ledger,
            input,
            root: &self.root,
        }
    }
}

/// A synthetic transaction receipt for decoder tests that need block-level context (sandwich
/// detection): a sender, an optional entry point (`to`), and its logs. Every other field is
/// irrelevant to the code under test.
pub(crate) fn receipt(
    hash: TxHash,
    from: Address,
    to: Option<Address>,
    logs: Vec<Log>,
) -> AnyTransactionReceipt {
    WithOtherFields::new(TransactionReceipt {
        inner: AnyReceiptEnvelope {
            inner: ReceiptWithBloom {
                receipt: Receipt {
                    status: Eip658Value::Eip658(true),
                    cumulative_gas_used: 0,
                    logs,
                },
                logs_bloom: Bloom::default(),
            },
            r#type: 0,
        },
        transaction_hash: hash,
        transaction_index: None,
        block_hash: None,
        block_number: None,
        gas_used: 0,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from,
        to,
        contract_address: None,
    })
}
