//! The derived data a `fynd-nexus` service serves: one snapshot per default computation, holding
//! the block it was computed at and a list of entries.
//!
//! Depths and token prices are `BigUint`s written as decimal strings, so they survive JSON without
//! loss. Spot prices are `f64`s, and a price that is not finite is left out.

use std::{fmt, str::FromStr};

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tycho_simulation::tycho_common::models::Address;

use crate::{
    derived::{
        computation::DerivedComputation,
        computations::{ComponentDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
        types::{ComponentDepths, SpotPrices, TokenGasPrices},
        ComputationId,
    },
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

/// The entries of one derived computation as computed at `block`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DerivedSnapshot<E> {
    /// The market block the entries were computed at.
    pub block: u64,
    /// The entries, in no particular order.
    pub entries: Vec<E>,
}

/// The spot prices of a market.
pub type SpotPriceSnapshot = DerivedSnapshot<SpotPriceEntry>;
/// The pool depths of a market.
pub type PoolDepthSnapshot = DerivedSnapshot<PoolDepthEntry>;
/// The token gas prices of a market.
pub type TokenGasPriceSnapshot = DerivedSnapshot<TokenGasPriceEntry>;

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

impl SpotPriceSnapshot {
    /// The snapshot of `prices` computed at `block`. Prices that are not finite are omitted
    /// because JSON cannot represent them.
    pub fn from_spot_prices(prices: &SpotPrices, block: u64) -> Self {
        let mut entries = Vec::with_capacity(prices.len());
        for ((component_id, token_in, token_out), price) in prices {
            if price.is_finite() {
                entries.push(SpotPriceEntry {
                    component_id: component_id.clone(),
                    token_in: token_in.clone(),
                    token_out: token_out.clone(),
                    price: *price,
                });
            }
        }
        Self { block, entries }
    }
}

impl PoolDepthSnapshot {
    /// The snapshot of `depths` computed at `block`.
    pub fn from_depths(depths: &ComponentDepths, block: u64) -> Self {
        let mut entries = Vec::with_capacity(depths.len());
        for ((component_id, token_in, token_out), depth) in depths {
            entries.push(PoolDepthEntry {
                component_id: component_id.clone(),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                depth: depth.clone(),
            });
        }
        Self { block, entries }
    }
}

impl TokenGasPriceSnapshot {
    /// The snapshot of `prices` computed at `block`.
    pub fn from_token_prices(prices: &TokenGasPrices, block: u64) -> Self {
        let mut entries = Vec::with_capacity(prices.len());
        for (token, price) in prices {
            entries.push(TokenGasPriceEntry {
                token: token.clone(),
                numerator: price.numerator.clone(),
                denominator: price.denominator.clone(),
            });
        }
        Self { block, entries }
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
