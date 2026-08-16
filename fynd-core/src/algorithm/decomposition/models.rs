use std::sync::Arc;

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::Price};

use crate::{derived::TokenGasPrices, ComponentId};

/// Gas cost expressed in a token.
///
/// defibot passes `dict[symbol, Decimal]` holding, per token, the price of one gas unit denominated
/// in that token (`optimizers/interface.py:13`). Fynd splits the same quantity in two: the block's
/// gas price in wei, and [`TokenGasPrices`] mapping a token to its wei ratio.
#[derive(Clone)]
pub(crate) struct TokenPriceData {
    pub(crate) gas_price_wei: BigUint,
    pub(crate) token_prices: Option<Arc<TokenGasPrices>>,
}

impl TokenPriceData {
    /// Builds a gas model from a block gas price and the derived token prices.
    ///
    /// With `None` for `token_prices` every cost is zero and the optimizer ranks on gross output.
    /// defibot instead falls back to a `DEFAULT_GAS_PRICE` of `1e-6`
    /// (`defibot/solver/models.py:29`), a constant in human units of whatever the buy token
    /// happens to be, which means something different for every token.
    pub(crate) fn new(gas_price_wei: BigUint, token_prices: Option<Arc<TokenGasPrices>>) -> Self {
        Self { gas_price_wei, token_prices }
    }

    /// Cost of `gas` gas units in on-chain units of `token`, or zero when no price is known.
    ///
    /// `token` is the alternative's *own* buy token, which is the order's buy token only at the
    /// branch level: a hop's alternatives produce the hop's output token, and a tail-grouped
    /// branch's sequences produce the token feeding its shared hop.
    pub(crate) fn cost_in_token(&self, gas: &BigUint, token: &Address) -> BigUint {
        let Some(price) = self
            .token_prices
            .as_ref()
            .and_then(|prices| prices.get(token))
        else {
            return BigUint::zero();
        };
        let Price { numerator, denominator } = price;
        if denominator.is_zero() {
            return BigUint::zero();
        }
        gas * &self.gas_price_wei * numerator / denominator
    }
}

/// A token path through the routing graph together with the pool used at each leg.
///
/// Owns its ids so nothing downstream carries the graph's lifetime.
pub(crate) struct DirectPath {
    /// Token addresses visited; one longer than [`DirectPath::components`].
    pub(crate) tokens: Vec<Address>,
    /// Component traded at each leg.
    pub(crate) components: Vec<ComponentId>,
}
