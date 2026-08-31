//! Turns decoded pool interactions into LP revenue and markout.
//!
//! # What the numbers mean
//!
//! `pnl` is profit and loss: what the pool's liquidity providers gained or lost, stated in token1.
//! A negative value means the LPs ended a swap worse off than holding and trading at the reference
//! price instead.
//!
//! For one swap, with `p` the reference price in token1 per token0:
//!
//! ```text
//! adverse_selection = delta0·p + delta1        (the raw curve trade, valued at p)
//! fee_revenue       = lp_fee0·p + lp_fee1      (what the pool kept for its LPs)
//! lp_pnl            = adverse_selection + fee_revenue   (the LPs' profit and loss)
//! ```
//!
//! `adverse_selection` is zero when the curve trades exactly at `p` and negative when the pool
//! traded at a worse price than the market. Classical LVR measures that loss against arbitrageurs
//! who trade *because* the pool is mispriced. No arbitrageur can trade this pool — every swap
//! needs a controller signature — so what this reports is markout on Fynd's routed flow, which is
//! the closest measurable analogue, not LVR itself.
//!
//! Fee amounts are measured from Ekubo's `FeesAccumulated` events rather than recomputed from the
//! signed rate. Ekubo credits an accrued fee on the *next* interaction with the pool, so
//! [`attribute_fees`] shifts each credit back onto the swap that earned it. A swap whose fee has
//! not been flushed yet is reported as pending, and a swap whose fee never reached the pool shows
//! zero LP revenue even though the taker was charged.

use crate::{
    chain::Interaction,
    pool::{token0_units, token1_units},
    prices::PriceSeries,
};

/// Fees credited to LPs for one swap, resolved across Ekubo's one-interaction lag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttributedFee {
    /// token0 fees credited to the pool for this swap.
    pub amount0: u128,
    /// token1 fees credited to the pool for this swap.
    pub amount1: u128,
    /// Set when no later interaction has flushed this swap's fee yet.
    pub pending: bool,
}

/// LP outcome for one swap, valued at one markout horizon.
#[derive(Debug, Clone, Copy)]
pub struct Markout {
    /// Seconds after the swap that the reference price was taken at.
    pub horizon_secs: u64,
    /// Reference price used, in token1 per token0.
    pub reference_price: f64,
    /// Value the curve trade gave up against the reference, in token1 units.
    pub adverse_selection: f64,
    /// Value the pool kept as fees, in token1 units.
    pub fee_revenue: f64,
    /// Net LP outcome, in token1 units.
    pub lp_pnl: f64,
}

/// One swap, with its fee attribution and markouts.
#[derive(Debug, Clone)]
pub struct SwapPnl {
    pub block: u64,
    pub timestamp: u64,
    pub tx: String,
    /// Pool-side token0 delta, in whole units.
    pub delta0: f64,
    /// Pool-side token1 delta, in whole units.
    pub delta1: f64,
    /// Price the curve traded at, in token1 per token0.
    pub pool_price: f64,
    /// Rate the controller signed, in basis points of the output token.
    pub signed_fee_bps: Option<f64>,
    pub fee: AttributedFee,
    pub markouts: Vec<Markout>,
}

impl SwapPnl {
    /// Trade size in token1 units, always positive.
    pub fn size(&self) -> f64 {
        self.delta1.abs()
    }

    /// Whether the pool bought token0 (the taker sold it).
    pub fn pool_bought_token0(&self) -> bool {
        self.delta0 > 0.0
    }
}

/// Assigns each `FeesAccumulated` credit back to the swap that earned it.
///
/// Walks interactions in chain order holding the most recent swap that has not been credited yet.
/// That swap is settled by the next interaction which either credits fees, or is itself a swap and
/// therefore takes over the accrual — settling at zero means the fee never reached the pool. An
/// interaction that does neither leaves the swap pending, so a position update that turns out not
/// to flush cannot misattribute a later credit. The returned vector is aligned with the swaps in
/// `interactions`, in the same order.
pub fn attribute_fees(interactions: &[Interaction]) -> Vec<AttributedFee> {
    let mut fees: Vec<AttributedFee> = Vec::new();
    let mut awaiting: Option<usize> = None;
    for interaction in interactions {
        let credited = interaction.fees_credited0 > 0 || interaction.fees_credited1 > 0;
        if credited || interaction.is_swap() {
            if let Some(index) = awaiting.take() {
                fees[index] = AttributedFee {
                    amount0: interaction.fees_credited0,
                    amount1: interaction.fees_credited1,
                    pending: false,
                };
            }
        }
        if interaction.is_swap() {
            awaiting = Some(fees.len());
            fees.push(AttributedFee { pending: true, ..AttributedFee::default() });
        }
    }
    fees
}

/// Builds the per-swap report from decoded interactions and a reference price series.
pub fn build(interactions: &[Interaction], prices: &PriceSeries, horizons: &[u64]) -> Vec<SwapPnl> {
    let fees = attribute_fees(interactions);
    interactions
        .iter()
        .filter(|interaction| interaction.is_swap())
        .zip(fees)
        .map(|(interaction, fee)| {
            let trade = interaction
                .trade
                .expect("filtered to swaps");
            let delta0 = token0_units(trade.delta0);
            let delta1 = token1_units(trade.delta1);
            let markouts = horizons
                .iter()
                .filter_map(|horizon| {
                    markout(*horizon, interaction.timestamp, delta0, delta1, &fee, prices)
                })
                .collect();
            SwapPnl {
                block: interaction.block,
                timestamp: interaction.timestamp,
                tx: interaction.tx.to_string(),
                delta0,
                delta1,
                pool_price: if delta0 == 0.0 { f64::NAN } else { -delta1 / delta0 },
                signed_fee_bps: interaction
                    .signed_fee
                    .map(|signed| f64::from(signed.fee_q32) / 2f64.powi(32) * 10_000.0),
                fee,
                markouts,
            }
        })
        .collect()
}

/// Values one swap against the reference price `horizon` seconds after it settled.
fn markout(
    horizon: u64,
    timestamp: u64,
    delta0: f64,
    delta1: f64,
    fee: &AttributedFee,
    prices: &PriceSeries,
) -> Option<Markout> {
    let reference_price = prices.at(timestamp + horizon)?;
    let adverse_selection = delta0 * reference_price + delta1;
    let fee_revenue =
        token0_units(fee.amount0 as i128) * reference_price + token1_units(fee.amount1 as i128);
    Some(Markout {
        horizon_secs: horizon,
        reference_price,
        adverse_selection,
        fee_revenue,
        lp_pnl: adverse_selection + fee_revenue,
    })
}

/// Totals across every swap, for one markout horizon.
#[derive(Debug, Clone, Copy, Default)]
pub struct Totals {
    pub horizon_secs: u64,
    pub volume: f64,
    pub adverse_selection: f64,
    pub fee_revenue: f64,
    pub lp_pnl: f64,
}

impl Totals {
    /// Net LP outcome as basis points of traded volume, or zero when nothing traded.
    pub fn lp_bps(&self) -> f64 {
        if self.volume == 0.0 {
            return 0.0;
        }
        self.lp_pnl / self.volume * 10_000.0
    }

    /// Fee revenue as basis points of traded volume, or zero when nothing traded.
    pub fn fee_bps(&self) -> f64 {
        if self.volume == 0.0 {
            return 0.0;
        }
        self.fee_revenue / self.volume * 10_000.0
    }
}

/// Sums per-swap results for one horizon. Swaps missing that horizon are skipped.
pub fn totals(swaps: &[SwapPnl], horizon: u64) -> Totals {
    let mut out = Totals { horizon_secs: horizon, ..Totals::default() };
    for swap in swaps {
        let Some(markout) = swap
            .markouts
            .iter()
            .find(|m| m.horizon_secs == horizon)
        else {
            continue;
        };
        out.volume += swap.size();
        out.adverse_selection += markout.adverse_selection;
        out.fee_revenue += markout.fee_revenue;
        out.lp_pnl += markout.lp_pnl;
    }
    out
}

#[cfg(test)]
mod tests {
    use alloy::primitives::TxHash;

    use super::*;
    use crate::chain::CurveTrade;

    fn interaction(block: u64, trade: Option<CurveTrade>, credited0: u128) -> Interaction {
        Interaction {
            block,
            timestamp: block * 12,
            tx: TxHash::with_last_byte(block as u8),
            trade,
            signed_fee: None,
            fees_credited0: credited0,
            fees_credited1: 0,
        }
    }

    fn swap(block: u64, credited0: u128) -> Interaction {
        interaction(block, Some(CurveTrade { delta0: -1, delta1: 1 }), credited0)
    }

    #[test]
    fn credits_a_fee_to_the_swap_one_interaction_earlier() {
        let fees = attribute_fees(&[swap(1, 0), swap(2, 500)]);
        assert_eq!(fees[0], AttributedFee { amount0: 500, amount1: 0, pending: false });
    }

    #[test]
    fn marks_the_last_swap_pending_until_it_is_flushed() {
        let fees = attribute_fees(&[swap(1, 0), swap(2, 500)]);
        assert!(fees[1].pending);
        assert_eq!(fees[1].amount0, 0);
    }

    #[test]
    fn lets_a_position_update_flush_a_swap_fee() {
        let fees = attribute_fees(&[swap(1, 0), interaction(2, None, 700)]);
        assert_eq!(fees.len(), 1);
        assert_eq!(fees[0], AttributedFee { amount0: 700, amount1: 0, pending: false });
    }

    #[test]
    fn reports_zero_when_a_swap_fee_never_reaches_the_pool() {
        // Three swaps, and only the third credits anything: swap 1's fee was never flushed.
        let fees = attribute_fees(&[swap(1, 0), swap(2, 0), swap(3, 900)]);
        assert_eq!(fees[0], AttributedFee::default());
        assert_eq!(fees[1].amount0, 900);
    }

    #[test]
    fn ignores_position_updates_when_listing_swaps() {
        let fees = attribute_fees(&[interaction(1, None, 0), swap(2, 0), interaction(3, None, 0)]);
        assert_eq!(fees.len(), 1);
    }

    #[test]
    fn markout_is_zero_when_the_curve_trades_at_the_reference() {
        let prices = PriceSeries::from_candles(vec![(60, 2_000.0)]);
        // Pool pays out 1 token0 and receives 2 000 token1 — exactly the reference price.
        let result = markout(0, 60, -1.0, 2_000.0, &AttributedFee::default(), &prices)
            .expect("price available");
        assert!(result.adverse_selection.abs() < 1e-9);
        assert!(result.lp_pnl.abs() < 1e-9);
    }

    #[test]
    fn markout_is_negative_when_the_pool_sells_below_the_reference() {
        let prices = PriceSeries::from_candles(vec![(60, 2_100.0)]);
        let result = markout(0, 60, -1.0, 2_000.0, &AttributedFee::default(), &prices)
            .expect("price available");
        assert!((result.adverse_selection - -100.0).abs() < 1e-9);
    }

    #[test]
    fn fee_revenue_offsets_the_curve_loss() {
        let prices = PriceSeries::from_candles(vec![(60, 2_100.0)]);
        let fee = AttributedFee { amount0: 10u128.pow(17), amount1: 0, pending: false };
        let result = markout(0, 60, -1.0, 2_000.0, &fee, &prices).expect("price available");
        assert!((result.fee_revenue - 210.0).abs() < 1e-9);
        assert!((result.lp_pnl - 110.0).abs() < 1e-9);
    }

    #[test]
    fn markout_is_skipped_when_the_horizon_predates_the_price_series() {
        let prices = PriceSeries::from_candles(vec![(1_000, 2_000.0)]);
        assert!(markout(0, 60, -1.0, 2_000.0, &AttributedFee::default(), &prices).is_none());
    }

    #[test]
    fn totals_are_zero_bps_without_volume() {
        assert_eq!(totals(&[], 0).lp_bps(), 0.0);
    }
}
