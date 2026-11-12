//! The maker wallets that pAMM venues pay from, and the limit they put on a route.
//!
//! A pAMM venue streams price levels per pair, and Fynd routes each pair as its own pool. Some
//! venues, such as Fermiswap, pay every pair from one maker wallet. Two of their pools in one route
//! that pay the same token draw on the same balance, and the second pays only what the first left.
//!
//! Each pool's simulation knows nothing of the others, so the route simulation keeps a
//! [`WalletSpend`](crate::maker_wallet::WalletSpend): what the route has taken from each wallet so
//! far. [`charge_hop`](crate::maker_wallet::charge_hop) adds a swap to it and fails the swap when
//! the wallet does not hold that much. The search then treats the swap like any other pool that
//! refuses an amount, and looks for another path.
//!
//! `path_frank_wolfe` charges its line search, its step re-simulation and its split route build.
//! Its paths come from `bellman_ford`, which charges nothing, so it prefers a split that fits over
//! a single path that overdraws. `water_fill` charges its single-path baseline and its split route
//! builds. `most_liquid` and `bellman_ford` charge nothing. The worker checks every algorithm's
//! route and rejects one that overdraws a wallet.
//!
//! A background task reads the balances from the node. A failed read keeps the last balance. A
//! balance it has not read, or one older than
//! [`MAX_BALANCE_AGE`](crate::maker_wallet::MAX_BALANCE_AGE), puts no limit on a route.

mod fetcher;

use std::time::{Duration, Instant};

use num_bigint::BigUint;
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::Address;

pub(crate) use self::fetcher::MakerWalletFetcher;
use crate::{algorithm::AlgorithmError, feed::market_data::MarketState, types::Route};

/// The oldest balance a route is checked against. An older one means its reads have failed for
/// that long, and it then puts no limit on a route.
pub const MAX_BALANCE_AGE: Duration = Duration::from_secs(30);

/// A maker wallet and the protocol systems whose pools pay from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakerWalletConfig {
    /// The wallet address.
    pub wallet: Address,
    /// The protocol systems that pay from `wallet`, e.g. `fallback:fermiswap`.
    pub protocol_systems: Vec<String>,
}

/// A maker wallet and one token it pays out.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WalletToken {
    pub(crate) wallet: Address,
    pub(crate) token: Address,
}

/// The maker wallets of the deployment, and their balances as the fetcher last read them.
#[derive(Debug, Clone, Default)]
pub struct MakerWallets {
    /// The maker wallet of each protocol system.
    wallet_of: FxHashMap<String, Address>,
    /// The last balance read of each wallet token, with the time of that read.
    balances: FxHashMap<WalletToken, (BigUint, Instant)>,
}

impl MakerWallets {
    /// The wallets of `configs`, with no balances read yet.
    ///
    /// # Errors
    ///
    /// Names the protocol system when two configs list it, since its pools pay from one wallet.
    pub(crate) fn new(configs: &[MakerWalletConfig]) -> Result<Self, String> {
        let mut wallet_of = FxHashMap::default();
        for config in configs {
            for protocol_system in &config.protocol_systems {
                if wallet_of
                    .insert(protocol_system.clone(), config.wallet.clone())
                    .is_some()
                {
                    return Err(format!("protocol system {protocol_system} lists two wallets"));
                }
            }
        }
        Ok(Self { wallet_of, balances: FxHashMap::default() })
    }

    /// Whether the deployment lists no maker wallet.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.wallet_of.is_empty()
    }

    /// Each protocol system with its maker wallet.
    pub(crate) fn wallets(&self) -> impl Iterator<Item = (&String, &Address)> {
        self.wallet_of.iter()
    }

    /// The same wallets, with `balances` read at `read_at`. A wallet token missing from
    /// `balances`, because its read failed, keeps its last balance until that is
    /// [`MAX_BALANCE_AGE`] old.
    pub(crate) fn with_balances(
        &self,
        balances: FxHashMap<WalletToken, BigUint>,
        read_at: Instant,
    ) -> Self {
        let mut merged = self.balances.clone();
        merged.retain(|_, (_, last_read)| last_read.elapsed() <= MAX_BALANCE_AGE);
        for (wallet_token, balance) in balances {
            merged.insert(wallet_token, (balance, read_at));
        }
        Self { wallet_of: self.wallet_of.clone(), balances: merged }
    }

    /// Wallets where `wallet`, the wallet of `protocol_system`, holds `balance` of `token`, read
    /// now.
    #[cfg(test)]
    pub(crate) fn holding(
        protocol_system: &str,
        wallet: &Address,
        token: &Address,
        balance: u64,
    ) -> Self {
        let wallets = Self::new(&[MakerWalletConfig {
            wallet: wallet.clone(),
            protocol_systems: vec![protocol_system.to_string()],
        }])
        .expect("one protocol system");
        let mut balances = FxHashMap::default();
        balances
            .insert(WalletToken { wallet: wallet.clone(), token: token.clone() }, balance.into());
        wallets.with_balances(balances, Instant::now())
    }

    /// What `wallet` holds of `token`, or `None` when that balance is unread or stale.
    fn balance(&self, wallet: &Address, token: &Address) -> Option<&BigUint> {
        let (balance, read_at) = self
            .balances
            .get(&WalletToken { wallet: wallet.clone(), token: token.clone() })?;
        if read_at.elapsed() > MAX_BALANCE_AGE {
            return None;
        }
        Some(balance)
    }

    /// Adds `amount_out` of `token_out`, which a `protocol_system` pool pays out, to `spend`.
    ///
    /// # Errors
    ///
    /// [`WalletOverdrawn`] when the wallet holds less than `spend` now asks of it. `spend` still
    /// includes `amount_out` after the error.
    pub(crate) fn charge(
        &self,
        spend: &mut WalletSpend,
        protocol_system: &str,
        token_out: &Address,
        amount_out: &BigUint,
    ) -> Result<(), WalletOverdrawn> {
        let Some(wallet) = self.wallet_of.get(protocol_system) else {
            return Ok(());
        };
        let wallet_token = WalletToken { wallet: wallet.clone(), token: token_out.clone() };
        let spent = spend.0.entry(wallet_token).or_default();
        *spent += amount_out;
        let Some(balance) = self.balance(wallet, token_out) else {
            return Ok(());
        };
        if &*spent > balance {
            return Err(WalletOverdrawn {
                wallet: wallet.clone(),
                token: token_out.clone(),
                balance: balance.clone(),
                asked: spent.clone(),
            });
        }
        Ok(())
    }

    /// Checks that `route` asks no maker wallet for more than it holds.
    ///
    /// # Errors
    ///
    /// [`WalletOverdrawn`] for the first wallet and token the route overdraws.
    pub fn check_route(&self, route: &Route) -> Result<(), WalletOverdrawn> {
        let mut spend = WalletSpend::default();
        for swap in route.swaps() {
            self.charge(&mut spend, swap.protocol(), swap.token_out(), swap.amount_out())?;
        }
        Ok(())
    }
}

/// What a route has taken from each maker wallet so far.
#[derive(Debug, Clone, Default)]
pub struct WalletSpend(FxHashMap<WalletToken, BigUint>);

/// A route asks a maker wallet for more of a token than the wallet holds.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("maker wallet {wallet} holds {balance} of {token}, the route asks for {asked}")]
pub struct WalletOverdrawn {
    /// The maker wallet.
    pub wallet: Address,
    /// The token the route asks for.
    pub token: Address,
    /// What the wallet holds.
    pub balance: BigUint,
    /// What the route asks for, all its swaps through the wallet together.
    pub asked: BigUint,
}

/// Adds `amount_out` of `token_out`, which the pool `component_id` pays out, to `spend`.
///
/// Call it once per simulated swap, with the same `spend` for every swap of one route, so that the
/// route's pools that share a maker wallet share its balance.
///
/// # Errors
///
/// [`AlgorithmError::SimulationFailed`] when the pool's maker wallet holds less than `spend` now
/// asks of it, as for a pool that refuses the amount.
pub fn charge_hop(
    market: &MarketState,
    spend: &mut WalletSpend,
    component_id: &str,
    token_out: &Address,
    amount_out: &BigUint,
) -> Result<(), AlgorithmError> {
    let wallets = market.maker_wallets();
    if wallets.is_empty() {
        return Ok(());
    }
    let Some(component) = market.get_component(component_id) else {
        return Ok(());
    };
    wallets
        .charge(spend, &component.protocol_system, token_out, amount_out)
        .map_err(|overdrawn| AlgorithmError::SimulationFailed {
            component_id: component_id.to_string(),
            error: overdrawn.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use tycho_simulation::tycho_common::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{component_with_protocol, token, MockProtocolSim},
        types::Swap,
    };

    const FERMISWAP: &str = "fallback:fermiswap";

    fn address(byte: u8) -> Address {
        Address::from(vec![byte; 20])
    }

    fn wallets_holding(token: &Address, balance: u64) -> MakerWallets {
        MakerWallets::holding(FERMISWAP, &address(0xAA), token, balance)
    }

    #[test]
    fn test_charge_two_swaps_of_one_wallet() {
        let usdc = address(0x01);
        let wallets = wallets_holding(&usdc, 100);
        let mut spend = WalletSpend::default();

        wallets
            .charge(&mut spend, FERMISWAP, &usdc, &BigUint::from(60u32))
            .expect("the wallet holds 100");
        let overdrawn = wallets
            .charge(&mut spend, FERMISWAP, &usdc, &BigUint::from(60u32))
            .expect_err("the two swaps ask for 120");

        assert_eq!(overdrawn.balance, BigUint::from(100u32));
        assert_eq!(overdrawn.asked, BigUint::from(120u32));
    }

    /// A swap through a Fermiswap pool that pays `amount_out` of `token_out`.
    fn fermiswap_swap(pool_id: &str, token_in: &Token, token_out: &Token, amount_out: u32) -> Swap {
        Swap::new(
            pool_id.to_string(),
            FERMISWAP.to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(500u32),
            BigUint::from(amount_out),
            BigUint::ZERO,
            component_with_protocol(pool_id, FERMISWAP, &[token_in.clone(), token_out.clone()]),
            Box::new(MockProtocolSim::new(2.0)),
        )
    }

    // Two Fermiswap pools in one split route pay 60 + 50 of B from a wallet that holds 100.
    #[test]
    fn test_check_route_two_pools_of_one_wallet() {
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let wallets = wallets_holding(&token_b.address, 100);
        let swaps = vec![
            fermiswap_swap("pamm_1", &token_a, &token_b, 60).with_split(0.5),
            fermiswap_swap("pamm_2", &token_a, &token_b, 50),
        ];
        let route = Route::new(swaps, FxHashMap::default()).expect("the route has swaps");

        let overdrawn = wallets
            .check_route(&route)
            .expect_err("the two swaps ask for 110");

        assert_eq!(overdrawn.asked, BigUint::from(110u32));
    }

    #[test]
    fn test_charge_exact_balance() {
        let usdc = address(0x01);
        let wallets = wallets_holding(&usdc, 100);
        let mut spend = WalletSpend::default();

        wallets
            .charge(&mut spend, FERMISWAP, &usdc, &BigUint::from(40u32))
            .unwrap();
        assert!(wallets
            .charge(&mut spend, FERMISWAP, &usdc, &BigUint::from(60u32))
            .is_ok());
    }

    #[test]
    fn test_charge_other_token_and_protocol() {
        let usdc = address(0x01);
        let wallets = wallets_holding(&usdc, 100);
        let mut spend = WalletSpend::default();

        assert!(wallets
            .charge(&mut spend, FERMISWAP, &address(0x02), &BigUint::from(1_000u32))
            .is_ok());
        assert!(wallets
            .charge(&mut spend, "uniswap_v3", &usdc, &BigUint::from(1_000u32))
            .is_ok());
    }

    #[test]
    fn test_charge_before_first_read() {
        let usdc = address(0x01);
        let wallets = MakerWallets::new(&[MakerWalletConfig {
            wallet: address(0xAA),
            protocol_systems: vec![FERMISWAP.to_string()],
        }])
        .expect("one protocol system");

        assert!(wallets
            .charge(&mut WalletSpend::default(), FERMISWAP, &usdc, &BigUint::from(1_000u32))
            .is_ok());
    }

    #[test]
    fn test_charge_stale_balances() {
        let usdc = address(0x01);
        let long_ago = Instant::now()
            .checked_sub(MAX_BALANCE_AGE + Duration::from_secs(1))
            .expect("the clock is more than 31 s past its origin");
        let mut balances = FxHashMap::default();
        balances.insert(
            WalletToken { wallet: address(0xAA), token: usdc.clone() },
            BigUint::from(100u32),
        );
        let wallets = MakerWallets::new(&[MakerWalletConfig {
            wallet: address(0xAA),
            protocol_systems: vec![FERMISWAP.to_string()],
        }])
        .expect("one protocol system")
        .with_balances(balances, long_ago);

        assert!(wallets
            .charge(&mut WalletSpend::default(), FERMISWAP, &usdc, &BigUint::from(1_000u32))
            .is_ok());
    }

    #[test]
    fn test_charge_after_failed_read() {
        let usdc = address(0x01);
        let wallets =
            wallets_holding(&usdc, 100).with_balances(FxHashMap::default(), Instant::now());

        assert!(wallets
            .charge(&mut WalletSpend::default(), FERMISWAP, &usdc, &BigUint::from(1_000u32))
            .is_err());
    }
}
