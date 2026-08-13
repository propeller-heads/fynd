//! Types and helpers for the GET /v1/tokens endpoint.

use std::{collections::HashMap, sync::Arc};

use fynd_core::{
    derived::{ComponentDepths, TokenGasPrices},
    types::ComponentId,
};
use serde::{Deserialize, Serialize};
use tycho_simulation::{
    tycho_common::models::{token::Token, Address},
    tycho_core::simulation::protocol_sim::Price,
};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for GET /v1/tokens.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct TokensQuery {
    /// Maximum number of tokens returned (default: 1000).
    #[param(example = 1000)]
    pub limit: Option<usize>,
    /// Number of tokens to skip from the start of the ranked list (default: 0).
    ///
    /// Pages are consistent as long as `block` is unchanged between requests;
    /// restart from offset 0 when it advances mid-pagination.
    #[param(example = 0)]
    pub offset: Option<usize>,
}

/// Top-level response for GET /v1/tokens.
#[derive(Debug, Serialize, ToSchema)]
pub struct TokensResponse {
    /// Graph tokens sorted by descending `liquidity`, then `component_count`, then address.
    pub tokens: Vec<GraphTokenEntry>,
    /// Total number of graph tokens before `limit` was applied.
    pub total: usize,
    /// Block at which token gas prices (the `liquidity` input) were computed.
    pub block: u64,
}

/// A token currently present in the routing graph, with ranking signals.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GraphTokenEntry {
    /// Token address.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub address: Address,
    /// Token symbol as indexed by Tycho.
    pub symbol: String,
    /// Token decimals.
    pub decimals: u32,
    /// Transfer tax in basis points (0 for ordinary tokens).
    pub tax: u64,
    /// Transfer gas cost estimates as indexed by Tycho (entries may be null).
    pub gas: Vec<Option<u64>>,
    /// Tycho token quality: 100 = normal; lower values indicate rebasing, fee-on-transfer,
    /// or analysis-failed tokens.
    pub quality: u32,
    /// Number of graph components (liquidity pools) containing this token.
    pub component_count: u32,
    /// Approximate routable liquidity in raw gas-token units: the sum of this token's
    /// directional component depths converted via its gas price. Approximate `f64`,
    /// intended for sorting and display only. Absent when the token has no computed
    /// gas price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidity: Option<f64>,
}

/// Cached, fully sorted token list for one derived-data state.
#[derive(Debug, Clone)]
pub(crate) struct TokensCache {
    /// Blocks of (token_prices, component_depths) the entries were computed from.
    pub key: (u64, Option<u64>),
    /// All graph tokens, pre-sorted; handlers truncate to the request limit.
    pub entries: Arc<Vec<GraphTokenEntry>>,
}

/// Approximates a `Price { numerator, denominator }` as an f64 ratio for scoring.
///
/// Returns `None` unless the result is finite and strictly positive. The score is
/// internal ranking input, never wire output — exact decimal serialization for the
/// wire lives in [`crate::api::prices::price_to_decimal_string`].
fn price_ratio_f64(price: &Price) -> Option<f64> {
    use num_traits::ToPrimitive;

    let ratio = price.numerator.to_f64()? / price.denominator.to_f64()?;
    (ratio.is_finite() && ratio > 0.0).then_some(ratio)
}

/// Builds ranked token entries from the market topology and derived data.
///
/// A token is included when it appears in at least one component's token list and has
/// metadata in the token registry. `liquidity` is the sum over all directional depths
/// where the token is the input, each converted to raw gas-token units via the token's
/// gas price; tokens without a computed gas price get `None` and sort after priced ones.
pub fn build_token_entries(
    topology: &HashMap<ComponentId, Vec<Address>>,
    token_registry: &HashMap<Address, Token>,
    depths: Option<&ComponentDepths>,
    token_prices: Option<&TokenGasPrices>,
) -> Vec<GraphTokenEntry> {
    use num_traits::ToPrimitive;

    let mut component_counts: HashMap<Address, u32> = HashMap::new();
    for tokens in topology.values() {
        for address in tokens {
            *component_counts
                .entry(address.clone())
                .or_default() += 1;
        }
    }

    let mut liquidity: HashMap<Address, f64> = HashMap::new();
    if let (Some(depths), Some(prices)) = (depths, token_prices) {
        for ((_, token_in, _), depth) in depths {
            if !component_counts.contains_key(token_in) {
                continue;
            }
            let Some(price) = prices.get(token_in) else { continue };
            let Some(price) = price_ratio_f64(price) else { continue };
            let Some(depth) = depth.to_f64() else { continue };
            let gas_units = depth * price;
            if gas_units.is_finite() {
                *liquidity
                    .entry(token_in.clone())
                    .or_default() += gas_units;
            }
        }
    }

    let mut entries = Vec::with_capacity(component_counts.len());
    for (address, component_count) in component_counts {
        let Some(token) = token_registry.get(&address) else { continue };
        entries.push(GraphTokenEntry {
            symbol: token.symbol.clone(),
            decimals: token.decimals,
            tax: token.tax,
            gas: token.gas.clone(),
            quality: token.quality,
            component_count,
            liquidity: liquidity.get(&address).copied(),
            address,
        });
    }

    entries.sort_by(|a, b| {
        b.liquidity
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&a.liquidity.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| {
                b.component_count
                    .cmp(&a.component_count)
            })
            .then_with(|| a.address.cmp(&b.address))
    });
    entries
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::{
        tycho_common::models::Chain, tycho_core::simulation::protocol_sim::Price,
    };

    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn test_token(byte: u8, symbol: &str, decimals: u32) -> Token {
        Token {
            address: addr(byte),
            symbol: symbol.to_string(),
            decimals,
            tax: 0,
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    /// Topology + registry for three tokens: A and B share two components, C sits in one.
    fn test_market() -> (HashMap<ComponentId, Vec<Address>>, HashMap<Address, Token>) {
        let topology = HashMap::from([
            ("c1".to_string(), vec![addr(0x0a), addr(0x0b)]),
            ("c2".to_string(), vec![addr(0x0a), addr(0x0b)]),
            ("c3".to_string(), vec![addr(0x0a), addr(0x0c)]),
        ]);
        let registry =
            [test_token(0x0a, "AAA", 18), test_token(0x0b, "BBB", 6), test_token(0x0c, "CCC", 8)]
                .into_iter()
                .map(|t| (t.address.clone(), t))
                .collect();
        (topology, registry)
    }

    #[test]
    fn test_build_token_entries_counts_components_without_derived_data() {
        let (topology, registry) = test_market();

        let entries = build_token_entries(&topology, &registry, None, None);

        assert_eq!(entries.len(), 3);
        // No liquidity anywhere: sorted by component count, then address.
        assert_eq!(entries[0].address, addr(0x0a));
        assert_eq!(entries[0].component_count, 3);
        assert_eq!(entries[0].liquidity, None);
        assert_eq!(entries[1].component_count, 2);
        assert_eq!(entries[2].component_count, 1);
    }

    #[test]
    fn test_build_token_entries_sums_depths_into_gas_units() {
        let (topology, registry) = test_market();
        // A depths: 100 in c1 + 300 in c3; price 2 gas units per raw A unit.
        let depths: ComponentDepths = [
            (("c1".to_string(), addr(0x0a), addr(0x0b)), BigUint::from(100u32)),
            (("c3".to_string(), addr(0x0a), addr(0x0c)), BigUint::from(300u32)),
            (("c1".to_string(), addr(0x0b), addr(0x0a)), BigUint::from(50u32)),
        ]
        .into_iter()
        .collect();
        let prices: TokenGasPrices = [
            (addr(0x0a), Price::new(BigUint::from(2u8), BigUint::from(1u8))),
            (addr(0x0b), Price::new(BigUint::from(1u8), BigUint::from(1u8))),
        ]
        .into_iter()
        .collect();

        let entries = build_token_entries(&topology, &registry, Some(&depths), Some(&prices));

        let entry_a = entries
            .iter()
            .find(|e| e.address == addr(0x0a))
            .unwrap();
        let entry_b = entries
            .iter()
            .find(|e| e.address == addr(0x0b))
            .unwrap();
        let entry_c = entries
            .iter()
            .find(|e| e.address == addr(0x0c))
            .unwrap();
        assert_eq!(entry_a.liquidity, Some(800.0));
        assert_eq!(entry_b.liquidity, Some(50.0));
        assert_eq!(entry_c.liquidity, None);
        // Priced tokens sort before the unpriced one regardless of degree.
        assert_eq!(entries[0].address, addr(0x0a));
        assert_eq!(entries[1].address, addr(0x0b));
        assert_eq!(entries[2].address, addr(0x0c));
    }

    #[test]
    fn test_build_token_entries_skips_tokens_missing_from_registry() {
        let (topology, mut registry) = test_market();
        registry.remove(&addr(0x0c));

        let entries = build_token_entries(&topology, &registry, None, None);

        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .all(|e| e.address != addr(0x0c)));
    }

    #[test]
    fn test_build_token_entries_ignores_depths_for_tokens_outside_topology() {
        let (topology, registry) = test_market();
        let depths: ComponentDepths =
            [(("cx".to_string(), addr(0xff), addr(0x0a)), BigUint::from(500u32))]
                .into_iter()
                .collect();
        let prices: TokenGasPrices =
            [(addr(0xff), Price::new(BigUint::from(1u8), BigUint::from(1u8)))]
                .into_iter()
                .collect();

        let entries = build_token_entries(&topology, &registry, Some(&depths), Some(&prices));

        assert_eq!(entries.len(), 3);
        assert!(entries
            .iter()
            .all(|e| e.liquidity.is_none()));
    }

    #[test]
    fn test_build_token_entries_empty_topology() {
        let (_, registry) = test_market();
        assert!(build_token_entries(&HashMap::new(), &registry, None, None).is_empty());
    }
}
