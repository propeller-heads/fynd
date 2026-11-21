//! The derived data a `fynd-nexus` service serves: for each default computation, a full list of
//! entries or a delta of the entries that changed, each stamped with a [`DerivedCursor`].
//!
//! Depths and token prices are `BigUint`s written as decimal strings, so they survive JSON without
//! loss. Spot prices are `f64`s, and a price that is not finite is left out.

use std::{fmt, hash::Hash, str::FromStr};

use hashbrown::HashMap;
use num_bigint::BigUint;
use rustc_hash::FxBuildHasher;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    derived::{
        computation::DerivedComputation,
        computations::{
            components_without_entries, tokens_without_price, ComponentDepthComputation,
            SpotPriceComputation, TokenGasPriceComputation,
        },
        types::{ComponentDepths, ComponentDirectionRef, SpotPrices, TokenGasPrices},
        ComputationId,
    },
    feed::market_data::MarketState,
    types::ComponentId,
};

/// One of the default derived computations of a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DerivedKind {
    /// The spot price of every direction of every component.
    SpotPrices,
    /// The depth of every direction of every component.
    PoolDepths,
    /// The price of every token in raw token units per raw native gas unit.
    TokenPrices,
}

impl DerivedKind {
    /// Every kind the service can serve.
    pub const ALL: [DerivedKind; 3] = [Self::SpotPrices, Self::PoolDepths, Self::TokenPrices];

    /// The computation id of this kind: the name used in endpoint paths, such as `spot_prices` in
    /// `/v1/derived/spot_prices`, and as the `computation` metric label value.
    pub fn name(self) -> &'static str {
        self.computation_id()
    }

    /// The id of the computation that produces this kind.
    pub fn computation_id(self) -> ComputationId {
        match self {
            Self::SpotPrices => SpotPriceComputation::ID,
            Self::PoolDepths => ComponentDepthComputation::ID,
            Self::TokenPrices => TokenGasPriceComputation::ID,
        }
    }

    /// The kind produced by the computation `id`, or `None` for any other computation.
    pub fn from_computation_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.computation_id() == id)
    }
}

impl fmt::Display for DerivedKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A kind name not listed in [`DerivedKind::ALL`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown derived data kind '{0}'; expected one of {names}", names = kind_names())]
pub struct UnknownDerivedKind(String);

fn kind_names() -> String {
    DerivedKind::ALL
        .map(DerivedKind::name)
        .join(", ")
}

impl FromStr for DerivedKind {
    type Err = UnknownDerivedKind;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == name)
            .ok_or_else(|| UnknownDerivedKind(name.to_string()))
    }
}

/// The spot price of one direction of a component: 1 `token_in` buys `price` `token_out`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpotPriceEntry {
    /// The component.
    pub component_id: ComponentId,
    /// The token sold.
    pub token_in: Address,
    /// The token bought.
    pub token_out: Address,
    /// The amount of `token_out` bought by one `token_in`.
    pub price: f64,
}

/// The depth of one direction of a component: the input amount at which its net marginal price
/// has fallen by the configured drop.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolDepthEntry {
    /// The component.
    pub component_id: ComponentId,
    /// The token sold.
    pub token_in: Address,
    /// The token bought.
    pub token_out: Address,
    /// The depth, as a decimal string.
    #[serde_as(as = "DisplayFromStr")]
    pub depth: BigUint,
}

/// The price of a token in raw token units per raw native gas unit: `numerator / denominator`.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenGasPriceEntry {
    /// The token.
    pub token: Address,
    /// The numerator, as a decimal string.
    #[serde_as(as = "DisplayFromStr")]
    pub numerator: BigUint,
    /// The denominator, as a decimal string.
    #[serde_as(as = "DisplayFromStr")]
    pub denominator: BigUint,
}

/// Where a reader stands in the history of one derived kind: the run of the service and the
/// number of the last update the reader applied.
///
/// A service run starts at `run_id` and numbers each publish of a kind with a `seq` that grows by
/// one. It is written `{run_id}-{seq}`, which is the `since` query of the endpoint and, in quotes,
/// the `ETag` of the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedCursor {
    /// The start time of the service run in milliseconds.
    pub run_id: u64,
    /// The position of the update in its kind's history within the run.
    pub seq: u64,
}

impl fmt::Display for DerivedCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.run_id, self.seq)
    }
}

/// Text that is not a `{run_id}-{seq}` cursor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a derived data cursor; expected '<run_id>-<seq>'")]
pub struct InvalidDerivedCursor(String);

impl FromStr for DerivedCursor {
    type Err = InvalidDerivedCursor;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidDerivedCursor(text.to_string());
        let (run_id, seq) = text
            .split_once('-')
            .ok_or_else(invalid)?;
        Ok(Self {
            run_id: run_id.parse().map_err(|_| invalid())?,
            seq: seq.parse().map_err(|_| invalid())?,
        })
    }
}

/// The key of a spot price or depth entry: one direction of one component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DirectionEntryKey {
    /// The component.
    pub component_id: ComponentId,
    /// The token sold.
    pub token_in: Address,
    /// The token bought.
    pub token_out: Address,
}

impl DirectionEntryKey {
    fn of(component_id: &str, token_in: &Address, token_out: &Address) -> Self {
        Self {
            component_id: component_id.to_string(),
            token_in: token_in.clone(),
            token_out: token_out.clone(),
        }
    }
}

/// The key of a token price entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenEntryKey {
    /// The token.
    pub token: Address,
}

/// One update of a derived kind as served by `fynd-nexus`: the whole kind, or what changed since
/// an earlier update of the same run.
///
/// In JSON the variant is the field `type`. A full update also parses as a snapshot with `block`
/// and `entries`, so readers that know nothing of deltas still read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DerivedUpdate<Entry, Key> {
    /// Every entry of the kind.
    Full {
        /// The run of the service.
        run_id: u64,
        /// The position of this update in its kind's history.
        seq: u64,
        /// The market block the entries were computed at.
        block: u64,
        /// The entries, in no particular order.
        entries: Vec<Entry>,
    },
    /// The entries that differ between two updates of one run.
    Delta {
        /// The run of the service.
        run_id: u64,
        /// The update this delta applies to.
        from_seq: u64,
        /// The position of this update in its kind's history.
        seq: u64,
        /// The market block of the entries after the delta.
        block: u64,
        /// The entries that are new or changed.
        upserted: Vec<Entry>,
        /// The keys of the entries that are gone.
        removed: Vec<Key>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdateType {
    Full,
    Delta,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "Entry: Deserialize<'de>, Key: Deserialize<'de>"))]
struct UpdateFields<Entry, Key> {
    #[serde(rename = "type")]
    update_type: UpdateType,
    run_id: u64,
    seq: u64,
    block: u64,
    from_seq: Option<u64>,
    #[serde(default = "Vec::new")]
    entries: Vec<Entry>,
    #[serde(default = "Vec::new")]
    upserted: Vec<Entry>,
    #[serde(default = "Vec::new")]
    removed: Vec<Key>,
}

/// Reads the fields directly instead of through serde's buffering of an internally tagged enum,
/// which would copy a multi-megabyte full update first.
impl<'de, Entry: Deserialize<'de>, Key: Deserialize<'de>> Deserialize<'de>
    for DerivedUpdate<Entry, Key>
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = UpdateFields::<Entry, Key>::deserialize(deserializer)?;
        let UpdateFields { update_type, run_id, seq, block, from_seq, entries, upserted, removed } =
            fields;
        Ok(match update_type {
            UpdateType::Full => Self::Full { run_id, seq, block, entries },
            UpdateType::Delta => Self::Delta {
                run_id,
                from_seq: from_seq.ok_or_else(|| serde::de::Error::missing_field("from_seq"))?,
                seq,
                block,
                upserted,
                removed,
            },
        })
    }
}

/// What differs between two states of a derived output.
pub struct DerivedChanges<Output: ServedDerivedOutput> {
    /// The entries that are new or changed.
    pub upserted: Vec<Output::Entry>,
    /// The keys of the entries that are gone.
    pub removed: Vec<Output::Key>,
    /// How many spot prices were left out for not being finite. Always 0 for other outputs.
    pub non_finite_left_out: usize,
}

impl<Output: ServedDerivedOutput> Default for DerivedChanges<Output> {
    fn default() -> Self {
        Self { upserted: Vec::new(), removed: Vec::new(), non_finite_left_out: 0 }
    }
}

impl<Output: ServedDerivedOutput> DerivedChanges<Output> {
    /// Whether no entry is new, changed or gone.
    pub fn is_empty(&self) -> bool {
        self.upserted.is_empty() && self.removed.is_empty()
    }
}

/// The output of a default derived computation, as the entries `fynd-nexus` serves.
///
/// Implemented for [`SpotPrices`], [`ComponentDepths`] and [`TokenGasPrices`]. The service turns
/// the store's output into entries and deltas with it, and a pod turns them back.
pub trait ServedDerivedOutput: Clone + Default + Send + Sync + 'static {
    /// One entry of the output as it travels in JSON.
    type Entry: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;
    /// What identifies an entry.
    type Key: Clone + Eq + Hash + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// The key of `entry`.
    fn entry_key(entry: &Self::Entry) -> Self::Key;

    /// The entries of the output, in no particular order. Spot prices that are not finite are
    /// left out because JSON cannot represent them.
    fn entries(&self) -> Vec<Self::Entry>;

    /// The output of the entries. A key listed twice keeps its last entry.
    fn from_entries(entries: Vec<Self::Entry>) -> Self;

    /// What turns `previous` into `self`. A spot price differs when its bits do, and one that is
    /// not finite counts as absent.
    fn changes_since(&self, previous: &Self) -> DerivedChanges<Self>;

    /// Applies `changes` to the output and says whether that added or removed a key.
    fn apply(&mut self, changes: &DerivedChanges<Self>) -> bool;

    /// The ids of the market's components, or of its tokens for token prices, that the output has
    /// no entry for.
    fn ids_without_entry(&self, market: &MarketState) -> Vec<String>;
}

/// The new and changed entries of `current` against `previous`, and the keys `current` lacks.
/// A value `is_listed` rejects counts as absent.
fn direction_changes<Value: Clone>(
    current: &HashMap<(ComponentId, Address, Address), Value, FxBuildHasher>,
    previous: &HashMap<(ComponentId, Address, Address), Value, FxBuildHasher>,
    is_listed: fn(&Value) -> bool,
    is_same: fn(&Value, &Value) -> bool,
) -> (Vec<(DirectionEntryKey, Value)>, Vec<DirectionEntryKey>) {
    let mut upserted = Vec::new();
    for ((component_id, token_in, token_out), value) in current {
        if !is_listed(value) {
            continue;
        }
        let held = previous.get(&ComponentDirectionRef(component_id, token_in, token_out));
        if !held.is_some_and(|held| is_same(held, value)) {
            upserted
                .push((DirectionEntryKey::of(component_id, token_in, token_out), value.clone()));
        }
    }
    let mut removed = Vec::new();
    for (component_id, token_in, token_out) in previous.keys() {
        let kept = current.get(&ComponentDirectionRef(component_id, token_in, token_out));
        if !kept.is_some_and(is_listed) {
            removed.push(DirectionEntryKey::of(component_id, token_in, token_out));
        }
    }
    (upserted, removed)
}

/// Writes `upserted` into and removes `removed` from `output`, and says whether a key came or
/// went.
fn apply_direction_changes<Value>(
    output: &mut HashMap<(ComponentId, Address, Address), Value, FxBuildHasher>,
    upserted: impl Iterator<Item = (DirectionEntryKey, Value)>,
    removed: &[DirectionEntryKey],
) -> bool {
    let mut keys_changed = false;
    for key in removed {
        let DirectionEntryKey { component_id, token_in, token_out } = key;
        let gone = output.remove(&ComponentDirectionRef(component_id, token_in, token_out));
        keys_changed |= gone.is_some();
    }
    for (key, value) in upserted {
        let DirectionEntryKey { component_id, token_in, token_out } = key;
        keys_changed |= output
            .insert((component_id, token_in, token_out), value)
            .is_none();
    }
    keys_changed
}

impl ServedDerivedOutput for SpotPrices {
    type Entry = SpotPriceEntry;
    type Key = DirectionEntryKey;

    fn entry_key(entry: &SpotPriceEntry) -> DirectionEntryKey {
        DirectionEntryKey::of(&entry.component_id, &entry.token_in, &entry.token_out)
    }

    fn entries(&self) -> Vec<SpotPriceEntry> {
        let mut entries = Vec::with_capacity(self.len());
        for ((component_id, token_in, token_out), price) in self {
            if price.is_finite() {
                entries.push(SpotPriceEntry {
                    component_id: component_id.clone(),
                    token_in: token_in.clone(),
                    token_out: token_out.clone(),
                    price: *price,
                });
            }
        }
        entries
    }

    fn from_entries(entries: Vec<SpotPriceEntry>) -> Self {
        let mut prices = Self::with_capacity_and_hasher(entries.len(), FxBuildHasher);
        for entry in entries {
            prices.insert((entry.component_id, entry.token_in, entry.token_out), entry.price);
        }
        prices
    }

    fn changes_since(&self, previous: &Self) -> DerivedChanges<Self> {
        let (upserted, removed) = direction_changes(
            self,
            previous,
            |price| price.is_finite(),
            |held, price| held.to_bits() == price.to_bits(),
        );
        DerivedChanges {
            upserted: upserted
                .into_iter()
                .map(|(key, price)| SpotPriceEntry {
                    component_id: key.component_id,
                    token_in: key.token_in,
                    token_out: key.token_out,
                    price,
                })
                .collect(),
            removed,
            non_finite_left_out: self
                .values()
                .filter(|price| !price.is_finite())
                .count(),
        }
    }

    fn apply(&mut self, changes: &DerivedChanges<Self>) -> bool {
        let upserted = changes
            .upserted
            .iter()
            .map(|entry| (Self::entry_key(entry), entry.price));
        apply_direction_changes(self, upserted, &changes.removed)
    }

    fn ids_without_entry(&self, market: &MarketState) -> Vec<String> {
        components_without_entries(market, self.keys().map(|key| key.0.as_str()))
    }
}

impl ServedDerivedOutput for ComponentDepths {
    type Entry = PoolDepthEntry;
    type Key = DirectionEntryKey;

    fn entry_key(entry: &PoolDepthEntry) -> DirectionEntryKey {
        DirectionEntryKey::of(&entry.component_id, &entry.token_in, &entry.token_out)
    }

    fn entries(&self) -> Vec<PoolDepthEntry> {
        let mut entries = Vec::with_capacity(self.len());
        for ((component_id, token_in, token_out), depth) in self {
            entries.push(PoolDepthEntry {
                component_id: component_id.clone(),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                depth: depth.clone(),
            });
        }
        entries
    }

    fn from_entries(entries: Vec<PoolDepthEntry>) -> Self {
        let mut depths = Self::with_capacity_and_hasher(entries.len(), FxBuildHasher);
        for entry in entries {
            depths.insert((entry.component_id, entry.token_in, entry.token_out), entry.depth);
        }
        depths
    }

    fn changes_since(&self, previous: &Self) -> DerivedChanges<Self> {
        let (upserted, removed) =
            direction_changes(self, previous, |_| true, |held, depth| held == depth);
        DerivedChanges {
            upserted: upserted
                .into_iter()
                .map(|(key, depth)| PoolDepthEntry {
                    component_id: key.component_id,
                    token_in: key.token_in,
                    token_out: key.token_out,
                    depth,
                })
                .collect(),
            removed,
            non_finite_left_out: 0,
        }
    }

    fn apply(&mut self, changes: &DerivedChanges<Self>) -> bool {
        let upserted = changes
            .upserted
            .iter()
            .map(|entry| (Self::entry_key(entry), entry.depth.clone()));
        apply_direction_changes(self, upserted, &changes.removed)
    }

    fn ids_without_entry(&self, market: &MarketState) -> Vec<String> {
        components_without_entries(market, self.keys().map(|key| key.0.as_str()))
    }
}

impl ServedDerivedOutput for TokenGasPrices {
    type Entry = TokenGasPriceEntry;
    type Key = TokenEntryKey;

    fn entry_key(entry: &TokenGasPriceEntry) -> TokenEntryKey {
        TokenEntryKey { token: entry.token.clone() }
    }

    fn entries(&self) -> Vec<TokenGasPriceEntry> {
        let mut entries = Vec::with_capacity(self.len());
        for (token, price) in self {
            entries.push(TokenGasPriceEntry {
                token: token.clone(),
                numerator: price.numerator.clone(),
                denominator: price.denominator.clone(),
            });
        }
        entries
    }

    fn from_entries(entries: Vec<TokenGasPriceEntry>) -> Self {
        let mut prices = Self::default();
        for entry in entries {
            prices.insert(
                entry.token,
                Price { numerator: entry.numerator, denominator: entry.denominator },
            );
        }
        prices
    }

    fn changes_since(&self, previous: &Self) -> DerivedChanges<Self> {
        let mut changes = DerivedChanges::default();
        for (token, price) in self {
            let unchanged = previous.get(token).is_some_and(|held| {
                held.numerator == price.numerator && held.denominator == price.denominator
            });
            if !unchanged {
                changes
                    .upserted
                    .push(TokenGasPriceEntry {
                        token: token.clone(),
                        numerator: price.numerator.clone(),
                        denominator: price.denominator.clone(),
                    });
            }
        }
        for token in previous.keys() {
            if !self.contains_key(token) {
                changes
                    .removed
                    .push(TokenEntryKey { token: token.clone() });
            }
        }
        changes
    }

    fn apply(&mut self, changes: &DerivedChanges<Self>) -> bool {
        let mut keys_changed = false;
        for key in &changes.removed {
            keys_changed |= self.remove(&key.token).is_some();
        }
        for entry in &changes.upserted {
            let price = Price {
                numerator: entry.numerator.clone(),
                denominator: entry.denominator.clone(),
            };
            keys_changed |= self
                .insert(entry.token.clone(), price)
                .is_none();
        }
        keys_changed
    }

    fn ids_without_entry(&self, market: &MarketState) -> Vec<String> {
        tokens_without_price(market, self)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::{json, Value};
    use tycho_simulation::tycho_core::{simulation::protocol_sim::Price, Bytes};

    use super::*;

    fn address(byte: u8) -> Address {
        Bytes::from([byte; 20])
    }

    fn spot_prices() -> SpotPrices {
        let mut prices = SpotPrices::default();
        prices.insert(("pool".to_string(), address(1), address(2)), 1.5);
        prices.insert(("pool".to_string(), address(2), address(1)), 1.0 / 1.5);
        prices
    }

    fn depths() -> ComponentDepths {
        let mut depths = ComponentDepths::default();
        depths.insert(("pool".to_string(), address(1), address(2)), BigUint::from(u128::MAX) * 7u8);
        depths
    }

    fn token_prices() -> TokenGasPrices {
        let mut prices = TokenGasPrices::default();
        prices.insert(
            address(1),
            Price { numerator: BigUint::from(u128::MAX) * 3u8, denominator: BigUint::from(7u8) },
        );
        prices
    }

    #[test]
    fn test_kind_name_is_computation_id() {
        for kind in DerivedKind::ALL {
            assert_eq!(kind.name(), kind.computation_id());
            assert_eq!(kind.name().parse(), Ok(kind));
            assert_eq!(DerivedKind::from_computation_id(kind.computation_id()), Some(kind));
        }
        assert_eq!(DerivedKind::PoolDepths.name(), "pool_depths");
        assert_eq!(DerivedKind::from_computation_id("token_classification"), None);
    }

    #[test]
    fn test_unknown_kind_lists_every_kind() {
        let error = "spot-prices"
            .parse::<DerivedKind>()
            .unwrap_err();

        assert_eq!(error, UnknownDerivedKind("spot-prices".to_string()));
        assert_eq!(
            error.to_string(),
            "unknown derived data kind 'spot-prices'; expected one of spot_prices, pool_depths, \
             token_prices"
        );
    }

    #[test]
    fn test_snapshots_round_trip_through_json() {
        let spot = SpotPriceSnapshot::from_spot_prices(&spot_prices(), 9);
        let depth = PoolDepthSnapshot::from_depths(&depths(), 9);
        let token = TokenGasPriceSnapshot::from_token_prices(&token_prices(), 9);

        let spot_back: SpotPriceSnapshot =
            serde_json::from_str(&serde_json::to_string(&spot).unwrap()).unwrap();
        let depth_back: PoolDepthSnapshot =
            serde_json::from_str(&serde_json::to_string(&depth).unwrap()).unwrap();
        let token_back: TokenGasPriceSnapshot =
            serde_json::from_str(&serde_json::to_string(&token).unwrap()).unwrap();

        assert_eq!(spot_back, spot);
        assert_eq!(depth_back, depth);
        assert_eq!(token_back, token);
    }

    #[rstest]
    #[case::pool_depth(
        serde_json::to_value(PoolDepthSnapshot::from_depths(&depths(), 9)).unwrap(),
        json!({"block": 9, "entries": [{
            "component_id": "pool",
            "token_in": "0x0101010101010101010101010101010101010101",
            "token_out": "0x0202020202020202020202020202020202020202",
            "depth": (BigUint::from(u128::MAX) * 7u8).to_string(),
        }]})
    )]
    #[case::token_gas_price(
        serde_json::to_value(TokenGasPriceSnapshot::from_token_prices(&token_prices(), 9)).unwrap(),
        json!({"block": 9, "entries": [{
            "token": "0x0101010101010101010101010101010101010101",
            "numerator": (BigUint::from(u128::MAX) * 3u8).to_string(),
            "denominator": "7",
        }]})
    )]
    fn test_snapshot_json_large_integers(#[case] actual: Value, #[case] expected: Value) {
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_snapshots_convert_back_into_the_maps_they_came_from() {
        let spot = SpotPriceSnapshot::from_spot_prices(&spot_prices(), 9);
        let depth = PoolDepthSnapshot::from_depths(&depths(), 9);
        let token = TokenGasPriceSnapshot::from_token_prices(&token_prices(), 9);

        assert_eq!(spot.into_spot_prices(), spot_prices());
        assert_eq!(depth.into_depths(), depths());
        assert_eq!(token.into_token_prices(), token_prices());
    }

    #[test]
    fn test_duplicate_entries_keep_the_last() {
        let entry = |price| SpotPriceEntry {
            component_id: "pool".to_string(),
            token_in: address(1),
            token_out: address(2),
            price,
        };
        let snapshot = SpotPriceSnapshot { block: 1, entries: vec![entry(1.0), entry(2.0)] };

        let prices = snapshot.into_spot_prices();

        assert_eq!(prices.len(), 1);
        assert_eq!(prices[&("pool".to_string(), address(1), address(2))], 2.0);
    }

    #[rstest]
    #[case::none_skipped(vec![1.5, 2.0], 2)]
    #[case::non_finite(vec![1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY], 1)]
    #[case::empty(vec![], 0)]
    fn test_spot_prices_skip_non_finite(#[case] values: Vec<f64>, #[case] kept: usize) {
        let mut prices = SpotPrices::default();
        for (index, value) in values.into_iter().enumerate() {
            prices.insert((format!("pool{index}"), address(1), address(2)), value);
        }

        let snapshot = SpotPriceSnapshot::from_spot_prices(&prices, 1);

        assert_eq!(snapshot.entries.len(), kept);
        let read_back: SpotPriceSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(read_back, snapshot);
    }
}
