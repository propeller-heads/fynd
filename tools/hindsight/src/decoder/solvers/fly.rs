//! Fly (formerly Magpie) `DexAggregator` calldata extraction.
//!
//! Fly's entry functions all take one `bytes` argument that is not standard ABI-encoded but a
//! packed blob `LibRouter.getData()` reads via hardcoded absolute byte offsets (verified against
//! Fly's Sourcify-published source, chain 8453). Addresses and the input amount sit at fixed
//! offsets; `amountOutMin` and `expectedAmountOut` sit behind a 3-byte packed header (a right-shift
//! amount, then a 2-byte big-endian pointer to the 32-byte word holding the value) — Magpie's own
//! calldata-packing scheme, not a proxy indirection. `amountOutMin` is the floor the trade reverts
//! below (`InsufficientAmountOut()`, selector `0xe52970aa`); `expectedAmountOut` is Magpie's
//! off-chain quote, usable as this solver's declared quote.

use alloy::primitives::{Address, U256};

use crate::decoder::solvers::{SolverDecoder, SwapIntent};

/// Selectors sharing `LibRouter`'s packed layout (`swapWithBackendSignature`,
/// `swapWithMagpieSignature`, `swapWithUserSignature`, `swapWithoutSignature`, `swap`).
const SELECTORS: [[u8; 4]; 5] = [
    [0x46, 0xec, 0x27, 0x8a],
    [0x73, 0xfc, 0x44, 0x57],
    [0x25, 0xe6, 0x51, 0xed],
    [0x15, 0x8f, 0x68, 0x94],
    [0x62, 0x7d, 0xd5, 0x6a],
];

const TO_ADDRESS_OFFSET: usize = 72;
const FROM_ASSET_OFFSET: usize = 92;
const TO_ASSET_OFFSET: usize = 112;
const AMOUNT_IN_OFFSET: usize = 132;
const AMOUNT_OUT_MIN_HEADER: usize = 167;
const EXPECTED_AMOUNT_OUT_HEADER: usize = 170;
const ADDRESS_LEN: usize = 20;
const WORD_LEN: usize = 32;

/// The fields of `LibRouter.SwapData` this decoder needs, read from one frame's packed calldata.
struct SwapData {
    from_asset: Address,
    to_asset: Address,
    amount_in: U256,
    amount_out_min: U256,
    /// Magpie's off-chain quote. Can legitimately be absent (zero) in some calldata variants.
    expected_amount_out: U256,
}

/// Read a 3-byte packed header at `header_offset`: a right-shift byte, then a 2-byte big-endian
/// pointer to the 32-byte word holding the value. `None` on any out-of-bounds read — a
/// malformed or truncated blob, not a real Fly call.
fn read_packed(input: &[u8], header_offset: usize) -> Option<U256> {
    let shift = *input.get(header_offset)?;
    let ptr_bytes = input.get(header_offset + 1..header_offset + 3)?;
    let ptr = usize::from(u16::from_be_bytes([ptr_bytes[0], ptr_bytes[1]]));
    let word = input.get(ptr..ptr + WORD_LEN)?;
    Some(U256::from_be_slice(word) >> usize::from(shift))
}

/// Whether `input` opens with one of Fly's packed-layout selectors.
fn has_fly_selector(input: &[u8]) -> Option<()> {
    let selector: [u8; 4] = input.get(0..4)?.try_into().ok()?;
    SELECTORS
        .contains(&selector)
        .then_some(())
}

/// Parse a Fly/Magpie frame's packed calldata. `None` when the selector does not match, or any
/// field's bytes fall outside the input — bounds are checked, never assumed.
fn parse(input: &[u8]) -> Option<SwapData> {
    has_fly_selector(input)?;
    let from_asset =
        Address::from_slice(input.get(FROM_ASSET_OFFSET..FROM_ASSET_OFFSET + ADDRESS_LEN)?);
    let to_asset = Address::from_slice(input.get(TO_ASSET_OFFSET..TO_ASSET_OFFSET + ADDRESS_LEN)?);
    let amount_in = U256::from_be_slice(input.get(AMOUNT_IN_OFFSET..AMOUNT_IN_OFFSET + WORD_LEN)?);
    let amount_out_min = read_packed(input, AMOUNT_OUT_MIN_HEADER)?;
    let expected_amount_out = read_packed(input, EXPECTED_AMOUNT_OUT_HEADER)?;
    Some(SwapData { from_asset, to_asset, amount_in, amount_out_min, expected_amount_out })
}

/// The Fly (Magpie) `DexAggregator` solver.
pub(crate) struct Fly;

impl SolverDecoder for Fly {
    /// The trader's enforced swap terms: `amountOutMin` is the on-chain floor
    /// (`InsufficientAmountOut()` below it); `expectedAmountOut`, when present, is Magpie's
    /// declared quote and must not be stricter than the floor it is quoted against. `input` must
    /// carry Fly's packed layout (e.g. it is `None` when `input` is the outer Relay wrapper, not
    /// Fly's own frame); the hint is unused — Fly's fields sit at fixed offsets, not located by
    /// value.
    fn declared_swap(&self, input: &[u8], _amount_in_hint: Option<U256>) -> Option<SwapIntent> {
        let data = parse(input)?;
        if data.amount_in.is_zero() || data.amount_out_min.is_zero() {
            return None;
        }
        if !data.expected_amount_out.is_zero() && data.amount_out_min > data.expected_amount_out {
            return None;
        }
        let intent =
            SwapIntent::new(data.from_asset, data.to_asset, data.amount_in, data.amount_out_min);
        Some(if data.expected_amount_out.is_zero() {
            intent
        } else {
            intent.with_quote(data.expected_amount_out, None)
        })
    }

    /// The packed blob's `toAddress` field. In practice this is Relay's own router, not the
    /// trader — Relay receives the output and forwards it — so callers must read the settled
    /// output as what this address *received*, not treat it as the trader.
    fn output_recipient(&self, input: &[u8]) -> Option<Address> {
        has_fly_selector(input)?;
        Some(Address::from_slice(input.get(TO_ADDRESS_OFFSET..TO_ADDRESS_OFFSET + ADDRESS_LEN)?))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    /// Live-validated Fly calldata (tx `0x1ae5b3e2…`, Base): `fromAsset` USDT, `toAsset` native,
    /// `amountIn` matching the transfer log exactly, `amountOutMin` 55bps below the settled
    /// amount, `expectedAmountOut` 45bps above it.
    fn real_input() -> Vec<u8> {
        let text = include_str!("fixtures/fly_input.txt").trim();
        alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
    }

    #[test]
    fn test_real_fixture_declared_swap() {
        let intent = Fly
            .declared_swap(&real_input(), None)
            .unwrap();
        assert_eq!(intent.token_in, address!("0xfde4c96c8593536e31f229ea8f37b2ada2699bb2"));
        assert_eq!(intent.token_out, Address::ZERO);
        assert_eq!(intent.amount_in, U256::from(19_694_643u64));
        assert_eq!(intent.min_amount_out, U256::from(10_217_898_321_149_381u64));
        assert_eq!(intent.quoted_amount_out(), U256::from(10_321_109_415_302_405u64));
    }

    #[test]
    fn test_real_fixture_output_recipient() {
        // Relay's own router — the delivery address, not the trader (see the method's doc).
        let recipient = Fly
            .output_recipient(&real_input())
            .unwrap();
        assert_eq!(recipient, address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f"));
    }

    #[test]
    fn test_output_recipient_wrong_selector() {
        let mut input = real_input();
        input[0] = 0xff;
        assert!(Fly.output_recipient(&input).is_none());
    }

    #[test]
    fn test_output_recipient_truncated_input() {
        let full = real_input();
        assert!(Fly
            .output_recipient(&full[..80])
            .is_none());
    }

    #[test]
    fn test_wrong_selector() {
        let mut input = real_input();
        input[0] = 0xff;
        assert!(Fly
            .declared_swap(&input, None)
            .is_none());
    }

    #[test]
    fn test_truncated_input() {
        let full = real_input();
        // Cut before the fixed-offset fields are readable at all.
        assert!(Fly
            .declared_swap(&full[..100], None)
            .is_none());
        // Cut inside the packed-header pointer's target word.
        assert!(Fly
            .declared_swap(&full[..300], None)
            .is_none());
    }

    #[test]
    fn test_empty_input() {
        assert!(Fly.declared_swap(&[], None).is_none());
    }

    #[test]
    fn test_zero_amount_out_min_rejected() {
        let mut input = real_input();
        // Zero out the word the amountOutMin pointer resolves to (ptr 281 in this fixture).
        input[281..313].fill(0);
        assert!(Fly
            .declared_swap(&input, None)
            .is_none());
    }

    #[test]
    fn test_amount_out_min_above_expected_rejected() {
        let mut input = real_input();
        // Zero the shift in amountOutMin's packed header (offset 167): decoding then reads the
        // target word unshifted, a value far above expectedAmountOut — an inconsistent blob (the
        // floor cannot exceed the quote). Corrupting the shift rather than the word's bytes
        // matters here: amountOutMin's and expectedAmountOut's target words overlap in this
        // fixture (ptrs 281 and 289), so filling the word instead would corrupt both readings
        // identically and leave them equal, not violate the check.
        input[AMOUNT_OUT_MIN_HEADER] = 0;
        assert!(Fly
            .declared_swap(&input, None)
            .is_none());
    }
}
