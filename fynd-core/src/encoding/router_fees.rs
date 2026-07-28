//! Router fee configuration mirrored from the on-chain `FeeCalculator` contract.
//!
//! [`RouterFees`] holds the default router fees, the per-client rates (already resolved
//! against the defaults), and the contract's fee-unit precision scale. The encoder reads a
//! [`SharedRouterFees`] snapshot on every encode; a background
//! [`RouterFeeFetcher`](crate::encoding::fee_fetcher::RouterFeeFetcher) refreshes it from
//! chain, so swapping in a FeeCalculator with a different precision is tracked automatically.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use tycho_simulation::tycho_common::Bytes;

/// Legacy basis-points denominator: client fees on Fynd's API use 10,000 = 100%.
///
/// The router takes `clientFeeBps` in the FeeCalculator's own fee units, so the encoder scales
/// the API value by `max_fee_units / LEGACY_BPS_DENOMINATOR` before putting it in calldata.
pub const LEGACY_BPS_DENOMINATOR: u64 = 10_000;

/// Fee-unit precision for the [`RouterFees::fallback`] configuration: 100% = 100,000,000 fee
/// units, matching the precision the on-chain FeeCalculator reports in production.
const FALLBACK_MAX_FEE_UNITS: u64 = 100_000_000;

/// Fallback router fee on swap output, used until the on-chain FeeCalculator has been read and
/// whenever a fetch fails: 0.1 bps (0.001%), i.e. 1000 fee units at 1e8 precision.
const FALLBACK_FEE_ON_OUTPUT: u32 = 1_000;

/// Effective router fee rates for one client, together with the precision scale they are
/// expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRates {
    on_output: u32,
    on_client_fee: u32,
    max_fee_units: u64,
}

impl FeeRates {
    /// Creates fee rates expressed in the given fee-unit scale (`max_fee_units` = 100%).
    pub fn new(on_output: u32, on_client_fee: u32, max_fee_units: u64) -> Self {
        Self { on_output, on_client_fee, max_fee_units }
    }

    /// Router fee charged on the swap output, in fee units.
    pub fn on_output(&self) -> u32 {
        self.on_output
    }

    /// Router share of the client fee, in fee units.
    pub fn on_client_fee(&self) -> u32 {
        self.on_client_fee
    }

    /// Fee units representing 100% (the contract's `MAX_BPS`).
    pub fn max_fee_units(&self) -> u64 {
        self.max_fee_units
    }

    /// Factor converting a legacy basis-point fee into fee units
    /// (`max_fee_units / LEGACY_BPS_DENOMINATOR`).
    pub fn fee_units_per_bps(&self) -> u64 {
        self.max_fee_units / LEGACY_BPS_DENOMINATOR
    }

    /// Converts a client fee given in legacy basis points into the fee units the router
    /// expects in `ClientFeeParams.clientFeeBps`.
    pub fn client_fee_units(&self, bps: u16) -> u64 {
        bps as u64 * self.fee_units_per_bps()
    }

    /// Combined denominator when two fee-unit rates are multiplied (`max_fee_units`²).
    pub fn max_fee_units_squared(&self) -> u128 {
        (self.max_fee_units as u128) * (self.max_fee_units as u128)
    }
}

/// Router fee configuration: precision scale, default rates, and per-client overrides.
///
/// Mirrors the on-chain FeeCalculator state. Rates are in fee units where
/// [`max_fee_units`](Self::max_fee_units) represents 100%.
#[derive(Debug, Clone)]
pub struct RouterFees {
    max_fee_units: u64,
    default_fee_on_output: u32,
    default_fee_on_client_fee: u32,
    /// Per-client resolved `(fee_on_output, fee_on_client_fee)` in fee units. The fetcher
    /// has already applied each client's overrides over the defaults, so a lookup miss simply
    /// falls back to the defaults.
    custom_fees: HashMap<Bytes, (u32, u32)>,
}

impl RouterFees {
    /// Creates a fee configuration from on-chain values. `custom_fees` maps a client to its
    /// resolved `(fee_on_output, fee_on_client_fee)` pair in fee units.
    pub fn new(
        max_fee_units: u64,
        default_fee_on_output: u32,
        default_fee_on_client_fee: u32,
        custom_fees: HashMap<Bytes, (u32, u32)>,
    ) -> Self {
        Self { max_fee_units, default_fee_on_output, default_fee_on_client_fee, custom_fees }
    }

    /// Conservative fallback used until the on-chain FeeCalculator has been read, and whenever
    /// a fetch fails: a 0.1 bps router fee on output, no fee on client fees, and no per-client
    /// overrides. Lets the encoder always produce a transaction rather than failing.
    pub fn fallback() -> Self {
        Self::new(FALLBACK_MAX_FEE_UNITS, FALLBACK_FEE_ON_OUTPUT, 0, HashMap::new())
    }

    /// Fee units representing 100% (the contract's `MAX_BPS`).
    pub fn max_fee_units(&self) -> u64 {
        self.max_fee_units
    }

    /// Resolves the effective fee rates for `client`: the per-client pair when present,
    /// otherwise the defaults. The fetcher has already applied `FeeCalculator._getFeeInfo`'s
    /// override-or-default logic per field, so this is a plain lookup. The contract's
    /// precision scale travels with the rates.
    pub fn fees_for(&self, client: &Bytes) -> FeeRates {
        let (on_output, on_client_fee) = self
            .custom_fees
            .get(client)
            .copied()
            .unwrap_or((self.default_fee_on_output, self.default_fee_on_client_fee));
        FeeRates::new(on_output, on_client_fee, self.max_fee_units)
    }

    /// Number of clients with at least one custom fee override.
    pub fn custom_client_count(&self) -> usize {
        self.custom_fees.len()
    }
}

/// Cloneable handle to the router fee configuration shared between the encoder (reader)
/// and the background fee fetcher (writer).
///
/// Initialised with [`RouterFees::fallback`] so the encoder always has a usable configuration;
/// the [`RouterFeeFetcher`](crate::encoding::fee_fetcher::RouterFeeFetcher) overwrites it with
/// on-chain values on each successful refresh.
#[derive(Debug, Clone)]
pub struct SharedRouterFees(Arc<RwLock<RouterFees>>);

impl Default for SharedRouterFees {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(RouterFees::fallback())))
    }
}

impl SharedRouterFees {
    /// Returns a copy of the current fee configuration.
    pub fn snapshot(&self) -> RouterFees {
        self.0
            .read()
            .expect("router fees lock poisoned")
            .clone()
    }

    /// Replaces the fee configuration with freshly fetched on-chain values.
    pub fn set(&self, fees: RouterFees) {
        *self
            .0
            .write()
            .expect("router fees lock poisoned") = fees;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u64 = 100_000_000;

    fn client(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }

    #[test]
    fn test_fees_for_unknown_client() {
        let fees = RouterFees::new(SCALE, 100_000, 20_000_000, HashMap::new());

        assert_eq!(fees.fees_for(&client(0xAA)), FeeRates::new(100_000, 20_000_000, SCALE));
    }

    #[test]
    fn test_fees_for_known_client() {
        let custom = HashMap::from([(client(0xAA), (50_000u32, 10_000_000u32))]);
        let fees = RouterFees::new(SCALE, 100_000, 20_000_000, custom);

        // Known client gets its stored pair; everyone else gets the defaults.
        assert_eq!(fees.fees_for(&client(0xAA)), FeeRates::new(50_000, 10_000_000, SCALE));
        assert_eq!(fees.fees_for(&client(0xBB)), FeeRates::new(100_000, 20_000_000, SCALE));
    }

    #[test]
    fn test_fallback_is_point_one_bps_on_output() {
        let rates = RouterFees::fallback().fees_for(&client(0xAA));
        // 1000 / 1e8 = 0.00001 = 0.1 bps, with no fee on client fees.
        assert_eq!(rates.on_output(), 1_000);
        assert_eq!(rates.on_client_fee(), 0);
        assert_eq!(rates.max_fee_units(), 100_000_000);
    }

    #[test]
    fn test_shared_router_fees_set_overrides() {
        let shared = SharedRouterFees::default();
        // Defaults to the fallback before any on-chain fetch lands.
        assert_eq!(
            shared
                .snapshot()
                .fees_for(&client(0xAA))
                .on_output(),
            1_000
        );

        shared.set(RouterFees::new(SCALE, 1, 2, HashMap::new()));

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.max_fee_units(), SCALE);
        assert_eq!(snapshot.fees_for(&client(0xAA)), FeeRates::new(1, 2, SCALE));
    }
}
