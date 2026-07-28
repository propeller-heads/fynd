//! Mock `PropAMM` pool for measuring what dynamic underbidding would win, before the pool exists.
//!
//! # What this measures
//!
//! The live `PropAMM` will be an Ekubo V3 pool at base fee 0 whose per-swap fee Fynd signs just low
//! enough to underbid the best competing route. Two independent quantities decide whether that
//! works, so the harness keeps them separate:
//!
//! 1. **The pool's fee-free price**, set by `--propamm-price-pct` as a percentage of the best real
//!    pool's price for the pair. `100` means "the `PropAMM` holds exactly the best price we can
//!    see"; `100.05` means it holds a price 5 bps better. This is the input — the assumption about
//!    how the pool is positioned.
//! 2. **The fee it could charge and still win**, which the harness *measures*. The mock quotes at
//!    the configured price with no fee at all, so whatever the router finds above the public
//!    commitment is exactly the headroom the signed extension could take. That is reported per
//!    trade and averaged over the run as `fee_headroom_bps`.
//!
//! So a run answers: "with the pool at N% of the market's best price, it would have won this share
//! of flow, and could have charged this much fee on top and still won it."
//!
//! Mechanically the harness inserts a synthetic component into the running solver's market state,
//! mirrors the best real pool for the pair onto it at the configured price (see
//! [`mirror::MirrorPool`]), and lets Fynd's existing exclusive-access routing do the rest: public
//! worker pools never see the mock, their exclusive-access twins do, and the router reports the
//! surplus whenever the mock route beats the public reference.
//!
//! # Why the market state, not the Tycho stream
//!
//! Rewriting each `Update` before it reaches `TychoFeed` would need a mirrored state cached across
//! blocks, because a pool only appears in `Update::states` on blocks where it changed. Writing
//! straight into [`MarketData`] avoids that entirely: `MarketState` holds the latest state for
//! every component regardless of which block last touched it, so every block has a source to
//! mirror.
//!
//! Two things the injection still has to get right:
//!
//! - **Announce the component, once.** Workers build their graph from `MarketEvent::MarketUpdated`,
//!   so a component written into `MarketState` without an event is invisible to routing. The first
//!   injection announces it as added; later ones as updated, which is also what drives incremental
//!   recomputation of its spot price and depth.
//! - **Wait for derived data.** Edge weights come from spot prices. Solving in the gap between the
//!   injected event and the recomputation it triggers would rank the mock on a stale weight, so
//!   [`Injector::inject`] blocks until the mock's spot price is recomputed at the target block.
//!
//! # Not for production
//!
//! Scaffolding for ENG-6157. The mock prices a pool that does not exist on chain, so any calldata
//! it produces is unexecutable.

mod mirror;
pub(crate) mod report;

use std::{
    collections::HashMap,
    str::FromStr,
    time::{Duration, Instant},
};

use chrono::NaiveDateTime;
use fynd_core::{feed::market_data::MarketData, MarketEvent, Solver};
use mirror::MirrorPool;
use num_bigint::BigUint;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use tycho_simulation::{
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Address, Chain, ChangeType},
        Bytes,
    },
    tycho_core::simulation::protocol_sim::ProtocolSim,
};

/// Component id of the mock pool. Address-shaped so anything that treats a component id as a pool
/// address stays well-formed, and recognisable in logs and JSONL.
pub(crate) const MOCK_COMPONENT_ID: &str = "0x9797979797979797979797979797979797979797";

/// Protocol system the mock reports. `ekubo_v3` is what the live pool will be, and it selects the
/// Ekubo swap encoder — the one path that knows how to carry a signed exclusive swap.
const MOCK_PROTOCOL_SYSTEM: &str = "ekubo_v3";

/// Placeholder for Ekubo's `SignedExclusiveSwap` extension, which is not deployed yet. Only the
/// encoder and the EIP-712 domain read it, so a placeholder produces a well-formed — but
/// deliberately unexecutable — payload.
const SIGNED_EXCLUSIVE_SWAP_ADDRESS: &str = "0x5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";

/// How long to wait for the mock's spot price to be recomputed at the target block before solving
/// anyway. Generous: it competes with a whole block's incremental computation, not just the mock's.
const DERIVED_DATA_WAIT: Duration = Duration::from_secs(30);
/// Poll interval while waiting for that recomputation.
const DERIVED_DATA_POLL: Duration = Duration::from_millis(25);

/// Which pool to mirror, and at what fee-free price.
#[derive(Debug, Clone)]
pub(crate) struct MirrorConfig {
    /// First token of the mirrored pair.
    pub token_a: Address,
    /// Second token of the mirrored pair.
    pub token_b: Address,
    /// The mock's fee-free price as a percentage of the best real pool's price for the pair.
    /// `100.0` mirrors that pool exactly and must never win — the harness's control case.
    pub price_pct: f64,
    /// `token_a` amount, in whole units, used to rank candidate source pools each block. Picking
    /// by realized output at a representative size, rather than by TVL, keeps the mirror on
    /// the pool that actually prices best for the sizes being re-solved.
    pub probe_units: f64,
    /// Chain the mock component reports.
    pub chain: Chain,
}

impl MirrorConfig {
    /// Parses a `--propamm-pair` value of two comma-separated token addresses.
    pub(crate) fn parse_pair(pair: &[String]) -> anyhow::Result<(Address, Address)> {
        let [token_a, token_b] = pair else {
            anyhow::bail!("--propamm-pair needs exactly two token addresses, got {}", pair.len());
        };
        Ok((
            Bytes::from_str(token_a)
                .map_err(|e| anyhow::anyhow!("invalid token address {token_a}: {e}"))?,
            Bytes::from_str(token_b)
                .map_err(|e| anyhow::anyhow!("invalid token address {token_b}: {e}"))?,
        ))
    }
}

/// What one injection did, for logging.
#[derive(Debug)]
pub(crate) struct Injected {
    /// Component id of the pool that was mirrored this block.
    pub source_component: String,
    /// The best real pool's price: `token_b` per whole unit of `token_a`, at the probe size.
    /// Watching this across blocks is how you tell a live mirror from one stuck on a stale state.
    pub source_price: f64,
    /// The mock's fee-free price, i.e. `source_price` scaled by `--propamm-price-pct`.
    pub mock_price: f64,
    /// Whether the mock's spot price was recomputed before the wait expired.
    pub derived_data_ready: bool,
    /// The mirrored pair as token symbols, e.g. `WETH/USDC`.
    pub pair_label: String,
}

/// Writes the mock pool into a running solver's market state, once per block.
pub(crate) struct Injector {
    config: MirrorConfig,
    events: broadcast::Sender<MarketEvent>,
    /// Candidate source pools for the pair, resolved on first use. Pools for a pair change far
    /// more slowly than blocks do, so this is refreshed only while still empty.
    candidates: Vec<String>,
    /// Whether the mock has been announced as an added component. Announcing twice would re-add an
    /// existing graph edge.
    announced: bool,
}

impl Injector {
    /// Creates an injector publishing on the solver's market-event channel.
    pub(crate) fn new(solver: &Solver, config: MirrorConfig) -> Self {
        Self {
            config,
            events: solver.market_event_sender(),
            candidates: Vec::new(),
            announced: false,
        }
    }

    /// Mirrors the best source pool onto the mock component for `block`.
    ///
    /// Returns `Ok(None)` when no pool for the configured pair carries state yet — early blocks of
    /// a fresh feed, or a pair that simply is not indexed.
    pub(crate) async fn inject(
        &mut self,
        solver: &Solver,
        block: u64,
    ) -> anyhow::Result<Option<Injected>> {
        let market = solver.market_data();

        if self.candidates.is_empty() {
            self.candidates = find_candidates(&market, &self.config).await;
            if self.candidates.is_empty() {
                return Ok(None);
            }
            info!(
                candidates = self.candidates.len(),
                "resolved candidate source pools for the mirrored pair"
            );
        }

        let Some(source) = best_source(&market, &self.candidates, &self.config).await else {
            return Ok(None);
        };

        let mock = MirrorPool::from_price_pct(source.state, self.config.price_pct);
        let price_factor = mock.price_factor();
        let mirrored: Box<dyn ProtocolSim> = Box::new(mock);
        let first_injection = !self.announced;
        {
            let mut state = market.write().await;
            if first_injection {
                state.upsert_components([mock_component(&self.config)]);
            }
            state.update_states([(MOCK_COMPONENT_ID.to_string(), mirrored)]);
        }

        let event = if first_injection {
            self.announced = true;
            MarketEvent::MarketUpdated {
                added_components: HashMap::from([(
                    MOCK_COMPONENT_ID.to_string(),
                    vec![self.config.token_a.clone(), self.config.token_b.clone()],
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        } else {
            MarketEvent::MarketUpdated {
                added_components: HashMap::new(),
                removed_components: Vec::new(),
                updated_components: vec![MOCK_COMPONENT_ID.to_string()],
            }
        };
        self.events
            .send(event)
            .map_err(|e| anyhow::anyhow!("no market-event receivers left: {e}"))?;

        let derived_data_ready = wait_for_derived_data(solver, block).await;
        if !derived_data_ready {
            warn!(
                block,
                "mock pool's spot price was not recomputed within {}s; its edge weight may be \
                 stale for this block",
                DERIVED_DATA_WAIT.as_secs()
            );
        }

        let mock_price = source.price * price_factor;
        debug!(
            block,
            source_component = source.component_id,
            source_price = source.price,
            mock_price,
            price_pct = self.config.price_pct,
            "mirrored source pool onto the mock PropAMM component"
        );
        Ok(Some(Injected {
            source_component: source.component_id,
            source_price: source.price,
            mock_price,
            derived_data_ready,
            pair_label: source.pair_label,
        }))
    }
}

/// Component ids holding both tokens of the configured pair, excluding the mock itself.
///
/// Multi-token pools (Curve, Balancer) qualify: the mirror only ever quotes the configured pair, so
/// what matters is that the source can price it.
async fn find_candidates(market: &MarketData, config: &MirrorConfig) -> Vec<String> {
    let state = market.read().await;
    state
        .component_topology()
        .into_iter()
        .filter(|(id, tokens)| {
            id != MOCK_COMPONENT_ID &&
                tokens.contains(&config.token_a) &&
                tokens.contains(&config.token_b)
        })
        .map(|(id, _)| id)
        .collect()
}

/// The real pool the mock mirrors for one block.
///
/// Chosen by realized output at the probe size rather than by TVL, so the mirror tracks the pool
/// that actually prices the sizes being re-solved best. Candidates that cannot price the pair — no
/// state yet, or a simulation error — are skipped rather than failing the block: a single broken
/// pool must not stop the run.
struct BestSource {
    /// Component id of the mirrored pool.
    component_id: String,
    /// A clone of its live state.
    state: Box<dyn ProtocolSim>,
    /// The price it quoted: `token_b` per whole unit of `token_a`.
    price: f64,
    /// The pair as token symbols, e.g. `WETH/USDC`.
    pair_label: String,
}

/// The candidate pool the mock mirrors this block.
async fn best_source(
    market: &MarketData,
    candidates: &[String],
    config: &MirrorConfig,
) -> Option<BestSource> {
    let state = market.read().await;
    let token_a = state
        .get_token(&config.token_a)?
        .clone();
    let token_b = state
        .get_token(&config.token_b)?
        .clone();

    let probe_amount = to_atomic(config.probe_units, &token_a);
    let mut best: Option<(String, Box<dyn ProtocolSim>, BigUint)> = None;
    for id in candidates {
        let Some(sim) = state.get_simulation_state(id) else {
            continue;
        };
        let Ok(quoted) = sim.get_amount_out(probe_amount.clone(), &token_a, &token_b) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(_, _, best_out)| quoted.amount > *best_out)
        {
            best = Some((id.clone(), sim.clone_box(), quoted.amount));
        }
    }

    let (component_id, state, probe_out) = best?;
    let price = if config.probe_units > 0.0 {
        to_units(&probe_out, &token_b) / config.probe_units
    } else {
        0.0
    };
    Some(BestSource {
        component_id,
        state,
        price,
        pair_label: format!("{}/{}", token_a.symbol, token_b.symbol),
    })
}

/// Blocks until the mock's spot price has been recomputed at `block`, or the wait expires.
///
/// Returns whether it landed in time. Both conditions matter: the entry proves the mock entered the
/// computation at all, and the block proves the entry is this block's, not the previous one's.
async fn wait_for_derived_data(solver: &Solver, block: u64) -> bool {
    let started = Instant::now();
    let derived = solver.derived_data();
    loop {
        {
            let guard = derived.read().await;
            let computed_at_block = guard.spot_prices_block() == Some(block);
            let includes_mock = guard
                .spot_prices()
                .is_some_and(|prices| {
                    prices
                        .keys()
                        .any(|(component_id, _, _)| component_id == MOCK_COMPONENT_ID)
                });
            if computed_at_block && includes_mock {
                return true;
            }
        }
        if started.elapsed() >= DERIVED_DATA_WAIT {
            return false;
        }
        tokio::time::sleep(DERIVED_DATA_POLL).await;
    }
}

/// The mock's `ProtocolComponent`.
///
/// The three static attributes are what `EkuboV3SwapEncoder` and the exclusive-swap signer read:
/// `extension` (20 bytes), `fee` (8-byte big-endian `u64`, zero because the `PropAMM`'s base fee is
/// zero and the per-swap fee arrives in the signature), and `pool_type_config` (4 bytes).
fn mock_component(config: &MirrorConfig) -> ProtocolComponent {
    let extension = Bytes::from_str(SIGNED_EXCLUSIVE_SWAP_ADDRESS)
        .expect("the extension placeholder is a valid address literal");
    ProtocolComponent {
        id: MOCK_COMPONENT_ID.to_string(),
        protocol_system: MOCK_PROTOCOL_SYSTEM.to_string(),
        protocol_type_name: "swap".to_string(),
        chain: config.chain,
        tokens: vec![config.token_a.clone(), config.token_b.clone()],
        static_attributes: HashMap::from([
            ("extension".to_string(), extension),
            ("fee".to_string(), Bytes::from(0u64)),
            ("pool_type_config".to_string(), Bytes::from(0u32)),
        ]),
        change: ChangeType::default(),
        creation_tx: Bytes::default(),
        created_at: NaiveDateTime::default(),
        contract_addresses: Vec::new(),
    }
}

/// Whether a component is the mock `PropAMM` pool — the predicate the solver's
/// [`ExclusivityPolicy`](fynd_core) is built from.
pub(crate) fn is_mock_component(component: &ProtocolComponent) -> bool {
    component.id == MOCK_COMPONENT_ID
}

/// Formats a token amount in whole units, for logging.
pub(crate) fn to_units(amount: &BigUint, token: &Token) -> f64 {
    report::biguint_to_f64(amount) / 10f64.powi(decimals_exponent(token))
}

/// Converts whole units into the token's atomic units.
///
/// Returns zero for anything that cannot be represented — non-finite, sub-atomic, or beyond `u64`.
/// A zero probe makes every candidate quote zero, so the caller picks arbitrarily rather than
/// silently mirroring a pool chosen by a garbage number.
// Truncation and sign loss are impossible after the guard: the value is finite and in [1, 2^63).
// The bound is a power of two, so it is exactly representable and the comparison is not
// approximate.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_atomic(units: f64, token: &Token) -> BigUint {
    let atomic = units * 10f64.powi(decimals_exponent(token));
    if !atomic.is_finite() || atomic < 1.0 || atomic >= 2f64.powi(63) {
        return BigUint::ZERO;
    }
    BigUint::from(atomic as u64)
}

/// A token's decimals as the exponent both conversions use, defaulting to 18 on an absurd value.
fn decimals_exponent(token: &Token) -> i32 {
    i32::try_from(token.decimals).unwrap_or(18)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_component_carries_the_attributes_the_encoder_reads() {
        let config = MirrorConfig {
            token_a: Bytes::from(vec![0x11; 20]),
            token_b: Bytes::from(vec![0x22; 20]),
            price_pct: 100.05,
            probe_units: 1.0,
            chain: Chain::Ethereum,
        };
        let component = mock_component(&config);

        assert_eq!(component.protocol_system, MOCK_PROTOCOL_SYSTEM);
        assert_eq!(component.static_attributes["extension"].len(), 20);
        assert_eq!(component.static_attributes["fee"].len(), 8);
        assert_eq!(component.static_attributes["pool_type_config"].len(), 4);
        assert_eq!(component.tokens, vec![config.token_a, config.token_b]);
    }

    #[test]
    fn test_mock_component_declares_zero_base_fee() {
        // The live pool's base fee must be 0 (Ekubo reverts with PoolFeeMustBeZero otherwise); the
        // whole fee is carried in the signed payload.
        let config = MirrorConfig {
            token_a: Bytes::from(vec![0x11; 20]),
            token_b: Bytes::from(vec![0x22; 20]),
            price_pct: 100.0,
            probe_units: 1.0,
            chain: Chain::Ethereum,
        };
        assert_eq!(mock_component(&config).static_attributes["fee"], Bytes::from(0u64));
    }

    #[test]
    fn test_is_mock_component_matches_only_the_mock() {
        let config = MirrorConfig {
            token_a: Bytes::from(vec![0x11; 20]),
            token_b: Bytes::from(vec![0x22; 20]),
            price_pct: 100.0,
            probe_units: 1.0,
            chain: Chain::Ethereum,
        };
        let mut real = mock_component(&config);
        real.id = "0xdeadbeef".to_string();

        assert!(is_mock_component(&mock_component(&config)));
        assert!(!is_mock_component(&real));
    }

    #[test]
    fn test_parse_pair_accepts_two_addresses() {
        let pair = vec![
            "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string(),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
        ];
        let (token_a, token_b) = MirrorConfig::parse_pair(&pair).expect("valid pair");
        assert_eq!(token_a.len(), 20);
        assert_eq!(token_b.len(), 20);
    }

    #[test]
    fn test_parse_pair_rejects_wrong_arity() {
        assert!(MirrorConfig::parse_pair(&["0x11".to_string()]).is_err());
        assert!(MirrorConfig::parse_pair(&[]).is_err());
    }

    #[test]
    fn test_to_units_scales_by_decimals() {
        let token = Token {
            address: Bytes::from(vec![0x11; 20]),
            symbol: "USDC".to_string(),
            decimals: 6,
            tax: 0,
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };
        assert!((to_units(&BigUint::from(2_500_000u64), &token) - 2.5).abs() < 1e-9);
    }
}
