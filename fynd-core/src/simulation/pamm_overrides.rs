//! The account state Titan priced a pAMM swap on, for simulating that swap.
//!
//! A pAMM quote prices the block Titan is building, on venue state that is not on chain yet, so a
//! plain simulation of the swap usually reverts. Tycho's price level stream attaches that state to
//! each `PriceLevelStreamState` as `PammSimulationOverrides`. This module converts it to alloy's
//! override types, merges the overrides of every pAMM swap of a route into one set, and counts
//! what it meets on the way.
//!
//! Nothing here acts on a quote: a missing, unconvertible or conflicting override only changes
//! what a simulation is given, never whether or how a quote is served.

use std::collections::{BTreeMap, BTreeSet};

use alloy::{
    primitives::{map::B256HashMap, Address, B256, U256},
    rpc::types::state::{AccountOverride, StateOverride},
};
use metrics::counter;
use tracing::{debug, warn};
use tycho_simulation::price_level_stream::{
    overrides::PammSimulationOverrides, state::PriceLevelStreamState,
};

use crate::{simulation::simulator::SimulationPurpose, Route, Swap};

/// pAMM swaps of served quotes, by whether their overrides reached the simulation set.
pub(crate) const LEGS_TOTAL: &str = "propamm_simulation_overrides_legs_total";
/// Attached overrides that name no block number or no block timestamp.
pub(crate) const MISSING_BLOCK_TOTAL: &str = "propamm_simulation_overrides_missing_block_total";
/// Values two pAMM swaps of one route set differently.
pub(crate) const CONFLICTS_TOTAL: &str = "propamm_simulation_overrides_conflicts_total";
/// Served quotes whose pAMM swaps name more than one block.
pub(crate) const BLOCK_MISMATCH_TOTAL: &str = "propamm_simulation_overrides_block_mismatch_total";
/// Values a pAMM override and the simulation's own funding both set differently.
pub(crate) const FUNDING_COLLISIONS_TOTAL: &str =
    "propamm_simulation_overrides_funding_collisions_total";
/// Simulated quotes that cross a pAMM swap, by whether they ran on the venues' overrides.
pub(crate) const SIMULATIONS_TOTAL: &str = "propamm_quote_simulations_total";
/// How far a simulated quote that crosses a pAMM swap landed from its quote, in bps.
pub(crate) const SIMULATION_DEVIATION_BPS: &str = "propamm_quote_simulation_deviation_bps";

/// The outcomes [`QuoteSimulator`](super::simulator::QuoteSimulator) counts a simulation under.
pub(crate) const SIMULATION_OUTCOMES: [&str; 4] =
    ["success", "reverted", "failed", "block_unavailable"];

/// The account state Titan priced one pAMM swap's venue on, in alloy's override types.
///
/// Every account sets only `balance`, `nonce` and `state_diff`, never a full `state`. An absent
/// field means Titan's simulation read the chain's value, never that the value is zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationOverrides {
    block_number: Option<u64>,
    block_timestamp: Option<u64>,
    accounts: BTreeMap<Address, AccountOverride>,
}

impl SimulationOverrides {
    /// The block Titan was building, the one the swap's price is for. `None` when Titan named
    /// none.
    pub fn block_number(&self) -> Option<u64> {
        self.block_number
    }

    /// The Unix second of [`Self::block_number`]. `None` when Titan named no beacon slot.
    pub fn block_timestamp(&self) -> Option<u64> {
        self.block_timestamp
    }

    /// The overridden accounts, keyed by address.
    pub fn accounts(&self) -> &BTreeMap<Address, AccountOverride> {
        &self.accounts
    }

    /// Converts the overrides tycho attached to a price level state.
    ///
    /// # Errors
    ///
    /// Names the first address, balance, storage slot or storage word that does not fit its alloy
    /// type. Tycho checks these widths when it receives an entry, so an error means its checks and
    /// this conversion disagree.
    fn from_pamm(overrides: &PammSimulationOverrides) -> Result<Self, String> {
        let mut accounts = BTreeMap::new();
        for (address, account) in &overrides.accounts {
            let address = Address::try_from(address.as_ref())
                .map_err(|_| format!("account {address} is {} bytes, not 20", address.len()))?;
            let balance = match &account.balance {
                Some(balance) => Some(U256::try_from_be_slice(balance).ok_or_else(|| {
                    format!("balance of {address} is {} bytes, more than 32", balance.len())
                })?),
                None => None,
            };
            let mut state_diff = B256HashMap::default();
            for (slot, word) in &account.state_diff {
                let slot_key = B256::try_from(slot.as_ref()).map_err(|_| {
                    format!("storage slot {slot} of {address} is {} bytes, not 32", slot.len())
                })?;
                let value = B256::try_from(word.as_ref()).map_err(|_| {
                    format!("storage word {word} of {address} is {} bytes, not 32", word.len())
                })?;
                state_diff.insert(slot_key, value);
            }
            accounts.insert(
                address,
                AccountOverride {
                    balance,
                    nonce: account.nonce,
                    state_diff: (!state_diff.is_empty()).then_some(state_diff),
                    ..Default::default()
                },
            );
        }
        Ok(Self {
            block_number: overrides.block_number,
            block_timestamp: overrides.block_timestamp,
            accounts,
        })
    }
}

/// What one swap of a route contributes to the route's simulation overrides.
#[derive(Debug)]
pub(crate) enum SwapOverrides {
    /// The swap is not a pAMM swap, so Titan priced nothing for it.
    NotPamm,
    /// A pAMM swap whose state carries no overrides.
    Missing,
    /// A pAMM swap whose overrides did not convert, with the reason.
    ConversionFailed(String),
    /// A pAMM swap with its overrides.
    Attached(SimulationOverrides),
}

impl SwapOverrides {
    /// Reads the overrides off the state `swap` was priced on.
    pub(crate) fn of(swap: &Swap) -> Self {
        let Some(state) = swap
            .protocol_state()
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
        else {
            return Self::NotPamm;
        };
        let Some(overrides) = state.simulation_overrides() else {
            return Self::Missing;
        };
        match SimulationOverrides::from_pamm(overrides) {
            Ok(converted) => Self::Attached(converted),
            Err(reason) => Self::ConversionFailed(reason),
        }
    }
}

/// The overrides of every pAMM swap of a route, merged into one set to simulate the route on.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RouteOverrides {
    /// The newest block any swap's overrides name.
    pub(crate) block_number: Option<u64>,
    /// The timestamp of [`Self::block_number`], when a swap naming that block named one.
    pub(crate) block_timestamp: Option<u64>,
    /// Every swap's accounts. Where two swaps set one value, the value of the later swap in the
    /// route.
    pub(crate) accounts: StateOverride,
}

impl RouteOverrides {
    /// Keeps `number` and `timestamp` when `number` is the newest block seen so far.
    fn take_block(&mut self, number: u64, timestamp: Option<u64>) {
        match self.block_number {
            Some(held) if held > number => {}
            Some(held) if held == number => {
                self.block_timestamp = self.block_timestamp.or(timestamp);
            }
            _ => {
                self.block_number = Some(number);
                self.block_timestamp = timestamp;
            }
        }
    }
}

/// What collecting a route's overrides met, for the metrics.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OverridesReport {
    /// pAMM swaps whose overrides were merged.
    pub(crate) attached: u64,
    /// pAMM swaps whose state carried none.
    pub(crate) missing: u64,
    /// The reason each pAMM swap's overrides did not convert.
    pub(crate) conversion_failures: Vec<String>,
    /// Merged overrides that name no block number.
    pub(crate) missing_block_number: u64,
    /// Merged overrides that name no block timestamp.
    pub(crate) missing_block_timestamp: u64,
    /// Storage slots a later swap set to a different value than an earlier one.
    pub(crate) storage_conflicts: u64,
    /// Balances a later swap set to a different value than an earlier one.
    pub(crate) balance_conflicts: u64,
    /// Nonces a later swap set to a different value than an earlier one.
    pub(crate) nonce_conflicts: u64,
    /// Whether the merged overrides name more than one block.
    pub(crate) block_mismatch: bool,
}

impl OverridesReport {
    /// Whether the route crosses a pAMM swap at all.
    pub(crate) fn has_pamm_swap(&self) -> bool {
        self.attached > 0 || self.missing > 0 || !self.conversion_failures.is_empty()
    }

    /// Counts what collecting one served quote's overrides met. A quote with no pAMM swap counts
    /// nothing.
    pub(crate) fn record(&self, order_id: &str) {
        if !self.has_pamm_swap() {
            return;
        }
        counter!(LEGS_TOTAL, "outcome" => "attached").increment(self.attached);
        counter!(LEGS_TOTAL, "outcome" => "missing").increment(self.missing);
        counter!(LEGS_TOTAL, "outcome" => "conversion_failed")
            .increment(self.conversion_failures.len() as u64);
        counter!(MISSING_BLOCK_TOTAL, "field" => "number").increment(self.missing_block_number);
        counter!(MISSING_BLOCK_TOTAL, "field" => "timestamp")
            .increment(self.missing_block_timestamp);
        counter!(CONFLICTS_TOTAL, "field" => "storage").increment(self.storage_conflicts);
        counter!(CONFLICTS_TOTAL, "field" => "balance").increment(self.balance_conflicts);
        counter!(CONFLICTS_TOTAL, "field" => "nonce").increment(self.nonce_conflicts);
        counter!(BLOCK_MISMATCH_TOTAL).increment(u64::from(self.block_mismatch));

        for reason in &self.conversion_failures {
            warn!(order_id, reason, "pAMM simulation overrides did not convert; leaving them out");
        }
        if self.missing > 0 ||
            self.storage_conflicts + self.balance_conflicts + self.nonce_conflicts > 0 ||
            self.block_mismatch
        {
            debug!(
                order_id,
                missing = self.missing,
                storage_conflicts = self.storage_conflicts,
                balance_conflicts = self.balance_conflicts,
                nonce_conflicts = self.nonce_conflicts,
                block_mismatch = self.block_mismatch,
                "pAMM simulation overrides incomplete or in conflict"
            );
        }
    }
}

/// Collects the overrides of `route`'s pAMM swaps and merges them into one set.
///
/// The accounts are a union, per account and per storage slot. Where two swaps set one balance,
/// nonce or slot, the later swap in the route wins. The block is the newest one any swap names.
/// Returns `None` when no pAMM swap carries overrides that convert.
pub(crate) fn collect(route: &Route) -> (Option<RouteOverrides>, OverridesReport) {
    let mut report = OverridesReport::default();
    let mut merged: Option<RouteOverrides> = None;
    let mut blocks = BTreeSet::new();
    for swap in route.swaps() {
        let overrides = match SwapOverrides::of(swap) {
            SwapOverrides::NotPamm => continue,
            SwapOverrides::Missing => {
                report.missing += 1;
                continue;
            }
            SwapOverrides::ConversionFailed(reason) => {
                report.conversion_failures.push(reason);
                continue;
            }
            SwapOverrides::Attached(overrides) => overrides,
        };
        report.attached += 1;
        let route_overrides = merged.get_or_insert_with(RouteOverrides::default);
        match overrides.block_number {
            Some(number) => {
                blocks.insert(number);
                route_overrides.take_block(number, overrides.block_timestamp);
            }
            None => report.missing_block_number += 1,
        }
        if overrides.block_timestamp.is_none() {
            report.missing_block_timestamp += 1;
        }
        merge_accounts(&mut route_overrides.accounts, overrides.accounts, &mut report);
    }
    report.block_mismatch = blocks.len() > 1;
    (merged, report)
}

/// Adds `accounts` to `merged`, `accounts` winning wherever both set a value, and counts the
/// values the two set differently.
fn merge_accounts(
    merged: &mut StateOverride,
    accounts: BTreeMap<Address, AccountOverride>,
    report: &mut OverridesReport,
) {
    for (address, account) in accounts {
        let held = merged.entry(address).or_default();
        if let Some(balance) = account.balance {
            if held
                .balance
                .is_some_and(|previous| previous != balance)
            {
                report.balance_conflicts += 1;
            }
            held.balance = Some(balance);
        }
        if let Some(nonce) = account.nonce {
            if held
                .nonce
                .is_some_and(|previous| previous != nonce)
            {
                report.nonce_conflicts += 1;
            }
            held.nonce = Some(nonce);
        }
        let Some(slots) = account.state_diff else { continue };
        let held_slots = held
            .state_diff
            .get_or_insert_with(B256HashMap::default);
        for (slot, word) in slots {
            if held_slots
                .insert(slot, word)
                .is_some_and(|previous| previous != word)
            {
                report.storage_conflicts += 1;
            }
        }
    }
}

/// Adds the venues' accounts to the overrides that fund the simulated sender, keeping the funding
/// wherever both set a value: the funding stands in for the sender's own balances, which no venue
/// state describes. Returns how many values the two set differently.
pub(crate) fn add_venue_accounts(funding: &mut StateOverride, venues: &StateOverride) -> u64 {
    let mut collisions = 0;
    for (address, venue) in venues {
        let held = funding.entry(*address).or_default();
        if let Some(balance) = venue.balance {
            match held.balance {
                None => held.balance = Some(balance),
                Some(own) if own != balance => collisions += 1,
                Some(_) => {}
            }
        }
        if let Some(nonce) = venue.nonce {
            match held.nonce {
                None => held.nonce = Some(nonce),
                Some(own) if own != nonce => collisions += 1,
                Some(_) => {}
            }
        }
        let Some(slots) = &venue.state_diff else { continue };
        let held_slots = held
            .state_diff
            .get_or_insert_with(B256HashMap::default);
        for (slot, word) in slots {
            match held_slots.get(slot) {
                None => {
                    held_slots.insert(*slot, *word);
                }
                Some(own) if own != word => collisions += 1,
                Some(_) => {}
            }
        }
    }
    collisions
}

/// Registers every series this module and the simulator count at zero, so an edge case that never
/// happened reads as a visible zero rather than as a missing series.
pub(crate) fn register_metrics() {
    for outcome in ["attached", "missing", "conversion_failed"] {
        counter!(LEGS_TOTAL, "outcome" => outcome).increment(0);
    }
    for field in ["number", "timestamp"] {
        counter!(MISSING_BLOCK_TOTAL, "field" => field).increment(0);
    }
    for field in ["storage", "balance", "nonce"] {
        counter!(CONFLICTS_TOTAL, "field" => field).increment(0);
    }
    counter!(BLOCK_MISMATCH_TOTAL).increment(0);
    let purpose = SimulationPurpose::QUOTE.label();
    counter!(FUNDING_COLLISIONS_TOTAL, "purpose" => purpose).increment(0);
    for overrides in ["applied", "none"] {
        for outcome in SIMULATION_OUTCOMES {
            counter!(
                SIMULATIONS_TOTAL,
                "overrides" => overrides,
                "outcome" => outcome,
                "purpose" => purpose
            )
            .increment(0);
        }
    }
}

/// pAMM state and swap fixtures shared by this module's tests, the simulator's and the encoder's.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::collections::BTreeMap;

    use num_bigint::BigUint;
    use tycho_simulation::{
        price_level_stream::{
            overrides::{AccountOverride as TychoAccountOverride, PammSimulationOverrides},
            state::{PriceLevelStreamQuote, PriceLevelStreamState},
        },
        tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes},
    };

    use crate::{
        algorithm::test_utils::{component_with_protocol, token, MockProtocolSim},
        Route, Swap,
    };

    /// A 20-byte address repeating `byte`.
    pub(crate) fn address(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }

    /// A 32-byte word whose last byte is `byte`.
    pub(crate) fn word(byte: u8) -> Bytes {
        let mut word = vec![0; 32];
        word[31] = byte;
        Bytes::from(word)
    }

    /// An account that overrides one storage slot per `(slot, value)` pair, and nothing else.
    pub(crate) fn storage(slots: &[(u8, u8)]) -> TychoAccountOverride {
        TychoAccountOverride {
            balance: None,
            nonce: None,
            state_diff: slots
                .iter()
                .map(|(slot, value)| (word(*slot), word(*value)))
                .collect(),
        }
    }

    /// Overrides for `block` (with its slot-derived timestamp) over `accounts`, each keyed by the
    /// byte its address repeats.
    pub(crate) fn overrides(
        block: Option<u64>,
        accounts: impl IntoIterator<Item = (u8, TychoAccountOverride)>,
    ) -> PammSimulationOverrides {
        PammSimulationOverrides {
            accounts: accounts
                .into_iter()
                .map(|(byte, account)| (address(byte), account))
                .collect::<BTreeMap<_, _>>(),
            block_number: block,
            block_timestamp: block.map(timestamp_of),
            built_at_ns: 1_792_115_920_127_487_452,
        }
    }

    /// The timestamp the fixtures give `block`: twelve seconds per block from a fixed origin.
    pub(crate) fn timestamp_of(block: u64) -> u64 {
        1_500_000_000 + 12 * block
    }

    /// A pAMM state quoting tokens `0x01` and `0x02`, carrying `overrides` when given.
    pub(crate) fn pamm_state(overrides: Option<PammSimulationOverrides>) -> PriceLevelStreamState {
        let quote = PriceLevelStreamQuote::new(BigUint::from(1_000_u64), BigUint::from(990_u64));
        let state = PriceLevelStreamState::new(
            address(0x01),
            address(0x02),
            vec![quote.clone()],
            vec![quote],
            BigUint::from(100_000_u64),
        );
        match overrides {
            Some(overrides) => state.with_simulation_overrides(overrides),
            None => state,
        }
    }

    /// A swap of token `0x01` for `0x02` on `id`, priced on `state`.
    pub(crate) fn swap_on(id: &str, protocol: &str, state: Box<dyn ProtocolSim>) -> Swap {
        let (token_in, token_out) = (token(0x01, "IN"), token(0x02, "OUT"));
        Swap::new(
            id.to_string(),
            protocol.to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(1_000_u64),
            BigUint::from(990_u64),
            BigUint::from(100_000_u64),
            component_with_protocol(id, protocol, &[token_in, token_out]),
            state,
        )
    }

    /// A pAMM swap whose state carries `overrides` when given.
    pub(crate) fn pamm_swap(id: &str, overrides: Option<PammSimulationOverrides>) -> Swap {
        swap_on(id, "fallback:fermiswap", Box::new(pamm_state(overrides)))
    }

    /// A swap on an ordinary pool.
    pub(crate) fn pool_swap(id: &str) -> Swap {
        swap_on(id, "uniswap_v2", Box::new(MockProtocolSim::new(1.0)))
    }

    /// A route of `swaps` over tokens `0x01` and `0x02`.
    pub(crate) fn route(swaps: Vec<Swap>) -> Route {
        let (token_in, token_out) = (token(0x01, "IN"), token(0x02, "OUT"));
        Route::new(
            swaps,
            [(token_in.address.clone(), token_in), (token_out.address.clone(), token_out)],
        )
        .expect("a route with swaps")
    }
}

#[cfg(test)]
#[path = "../tests/simulation/pamm_overrides.rs"]
mod tests;
