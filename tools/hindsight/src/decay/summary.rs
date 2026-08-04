//! Per-offset aggregation of a decay run's measurements.
//!
//! Accumulates every measurement in memory, which a run of any realistic length affords: at the
//! default sample size a day of Base produces ~1.1M measurements, or ~1.7 MB of `f64` per offset.

use serde::Serialize;

use crate::decay::record::DecayBps;

/// Degradation past this many bps is counted as tail risk — PR #297's threshold, kept so the Base
/// numbers line up with its Ethereum run.
const TAIL_BPS: f64 = 20.0;

/// Percentile denominator, so quantiles are expressed as exact integer percents and no float ever
/// has to be turned into a slice index.
const PERCENT: usize = 100;

/// Winsorizing percentile: the most extreme 1% at each end is clamped to the 1st/99th percentile
/// before the winsorized mean. Route decay is heavy-tailed enough that a handful of thousand-bps
/// observations otherwise decide the raw mean.
const WINSOR_PERCENT: usize = 1;

/// Every measurement taken at one offset.
#[derive(Default)]
struct OffsetSamples {
    /// The total move per measurement: the quoted route replayed this many blocks later.
    route: Vec<f64>,
    /// Unavoidable market drift per measurement.
    market: Vec<f64>,
    /// The part specific to holding a stale route.
    execution: Vec<f64>,
}

impl OffsetSamples {
    fn push(&mut self, bps: DecayBps) {
        self.route.push(bps.route_slippage_bps);
        self.market
            .push(bps.market_movement_bps);
        self.execution
            .push(bps.execution_slippage_bps);
    }
}

/// One offset's aggregated statistics.
///
/// Every bps figure keeps hindsight's sign convention: **positive means surplus**, so degradation
/// is negative. `degraded_share` and `tail_share` are therefore counted on the negative side.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OffsetStats {
    pub offset: u32,
    /// Measurements with a complete decomposition at this offset.
    pub count: usize,
    pub mean_bps: f64,
    /// Mean after clamping the outer 1% at each end — the number to read when comparing runs,
    /// since the raw mean is dominated by a few extreme observations.
    pub winsorized_mean_bps: f64,
    pub p50_bps: f64,
    /// The 5th percentile: the bad tail, since negative is degradation.
    pub p05_bps: f64,
    pub p01_bps: f64,
    /// Fraction of measurements where the route produced less than quoted.
    pub degraded_share: f64,
    /// Fraction that degraded by more than [`TAIL_BPS`].
    pub tail_share: f64,
    /// Mean absolute market drift, and mean absolute stale-route loss.
    pub mean_abs_market_bps: f64,
    pub mean_abs_execution_bps: f64,
    /// Market drift's share of the total absolute move, and the stale route's. These sum to 1 and
    /// are the split PR #297 reported as "41% market movement / 59% execution slippage".
    pub market_share: f64,
    pub execution_share: f64,
}

/// Accumulates measurements and renders the run's per-offset summary.
pub(crate) struct Summary {
    offsets: Vec<OffsetSamples>,
}

impl Summary {
    /// Room for offsets `1..=offsets`.
    pub(crate) fn new(offsets: u32) -> Self {
        Self {
            offsets: (0..offsets)
                .map(|_| OffsetSamples::default())
                .collect(),
        }
    }

    /// Record one complete measurement. Offsets outside `1..=offsets` are ignored rather than
    /// panicking: a summary is a report, and must never take a run down.
    pub(crate) fn record(&mut self, offset: u32, bps: DecayBps) {
        let Some(slot) = offset
            .checked_sub(1)
            .and_then(|i| self.offsets.get_mut(i as usize))
        else {
            return;
        };
        slot.push(bps);
    }

    /// Statistics for every offset that saw at least one measurement, in offset order.
    pub(crate) fn stats(&self) -> Vec<OffsetStats> {
        let mut stats = Vec::new();
        for (index, samples) in self.offsets.iter().enumerate() {
            if samples.route.is_empty() {
                continue;
            }
            let offset = u32::try_from(index + 1).unwrap_or(u32::MAX);
            stats.push(offset_stats(offset, samples));
        }
        stats
    }
}

fn offset_stats(offset: u32, samples: &OffsetSamples) -> OffsetStats {
    let mut sorted = samples.route.clone();
    // Total order over floats: measurements are finite by construction (a non-finite bps value
    // cannot come out of `raw_bps_diff`, which rejects non-positive amounts).
    sorted.sort_by(f64::total_cmp);
    let count = sorted.len();
    let mean_abs_market = mean_abs(&samples.market);
    let mean_abs_execution = mean_abs(&samples.execution);
    let abs_total = mean_abs_market + mean_abs_execution;
    // A run where nothing moved at all leaves the split undefined; report it as an even split
    // rather than a NaN that would poison the JSON summary.
    let (market_share, execution_share) = if abs_total > 0.0 {
        (mean_abs_market / abs_total, mean_abs_execution / abs_total)
    } else {
        (0.5, 0.5)
    };
    OffsetStats {
        offset,
        count,
        mean_bps: mean(&sorted),
        winsorized_mean_bps: winsorized_mean(&sorted),
        p50_bps: percentile(&sorted, 50),
        p05_bps: percentile(&sorted, 5),
        p01_bps: percentile(&sorted, 1),
        degraded_share: share(&sorted, |v| v < 0.0),
        tail_share: share(&sorted, |v| v < -TAIL_BPS),
        mean_abs_market_bps: mean_abs_market,
        mean_abs_execution_bps: mean_abs_execution,
        market_share,
        execution_share,
    }
}

/// A sample count as an `f64` divisor. Counts are bounded by how many measurements a run can take,
/// so they never reach the 2^53 where an `f64` stops representing integers exactly.
#[expect(
    clippy::cast_precision_loss,
    reason = "a measurement count cannot reach 2^53, where f64 stops being exact for integers"
)]
fn count_as_f64(count: usize) -> f64 {
    count as f64
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / count_as_f64(values.len())
}

fn mean_abs(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let total: f64 = values.iter().map(|v| v.abs()).sum();
    total / count_as_f64(values.len())
}

/// The `percent`th percentile of an ascending slice, by nearest rank. Empty input yields `0.0`.
///
/// The rank is integer arithmetic rather than `percent / 100 * len` in floating point, so no float
/// is ever cast to an index — the cast that would silently truncate or lose a sign.
fn percentile(sorted: &[f64], percent: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = sorted
        .len()
        .saturating_mul(percent.min(PERCENT)) /
        PERCENT;
    sorted[rank.min(sorted.len() - 1)]
}

/// Mean of an ascending slice with the outer [`WINSOR_PERCENT`] at each end clamped to that
/// percentile's value, rather than dropped — so the count stays the same and the mean stays
/// comparable across runs of different length.
fn winsorized_mean(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let low = percentile(sorted, WINSOR_PERCENT);
    let high = percentile(sorted, PERCENT - WINSOR_PERCENT);
    let clamped: f64 = sorted
        .iter()
        .map(|v| v.clamp(low, high))
        .sum();
    clamped / count_as_f64(sorted.len())
}

fn share(values: &[f64], predicate: impl Fn(f64) -> bool) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let matched = values
        .iter()
        .filter(|&&v| predicate(v))
        .count();
    count_as_f64(matched) / count_as_f64(values.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bps(route: f64, market: f64) -> DecayBps {
        DecayBps {
            route_slippage_bps: route,
            market_movement_bps: market,
            execution_slippage_bps: route - market,
        }
    }

    #[test]
    fn stats_are_reported_per_offset_in_order() {
        let mut summary = Summary::new(3);
        summary.record(3, bps(-3.0, -1.0));
        summary.record(1, bps(-1.0, -1.0));
        summary.record(1, bps(-1.0, -1.0));
        let stats = summary.stats();
        // Offset 2 saw nothing, so it is absent rather than reported as an empty row.
        assert_eq!(
            stats
                .iter()
                .map(|s| s.offset)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(stats[0].count, 2);
        assert_eq!(stats[1].count, 1);
    }

    #[test]
    fn out_of_range_offsets_are_ignored() {
        let mut summary = Summary::new(2);
        summary.record(0, bps(-1.0, 0.0));
        summary.record(9, bps(-1.0, 0.0));
        assert!(summary.stats().is_empty(), "no in-range offset saw a measurement");
    }

    #[test]
    fn degraded_and_tail_shares_count_the_negative_side() {
        let mut summary = Summary::new(1);
        // Two degrade (one past the 20 bps tail), one improves, one is flat.
        for route in [-30.0, -5.0, 4.0, 0.0] {
            summary.record(1, bps(route, 0.0));
        }
        let stats = &summary.stats()[0];
        assert!((stats.degraded_share - 0.5).abs() < 1e-9);
        assert!((stats.tail_share - 0.25).abs() < 1e-9);
    }

    #[test]
    fn winsorizing_pulls_in_an_extreme_outlier() {
        let mut summary = Summary::new(1);
        for _ in 0..99 {
            summary.record(1, bps(-1.0, 0.0));
        }
        // One 7405 bps observation, the magnitude seen in real Base Relay data.
        summary.record(1, bps(-7405.0, 0.0));
        let stats = &summary.stats()[0];
        assert!(stats.mean_bps < -70.0, "raw mean must feel the outlier: {}", stats.mean_bps);
        assert!(
            (stats.winsorized_mean_bps + 1.0).abs() < 0.5,
            "winsorized mean must not: {}",
            stats.winsorized_mean_bps
        );
    }

    #[test]
    fn split_shares_sum_to_one_and_attribute_correctly() {
        let mut summary = Summary::new(1);
        // Total -10, of which -4 was market drift: 40/60, the shape PR #297 reported.
        summary.record(1, bps(-10.0, -4.0));
        let stats = &summary.stats()[0];
        assert!((stats.market_share - 0.4).abs() < 1e-9, "got {}", stats.market_share);
        assert!((stats.execution_share - 0.6).abs() < 1e-9);
        assert!((stats.market_share + stats.execution_share - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_flat_market_reports_an_even_split_not_a_nan() {
        let mut summary = Summary::new(1);
        summary.record(1, bps(0.0, 0.0));
        let stats = &summary.stats()[0];
        assert!(stats.market_share.is_finite() && stats.execution_share.is_finite());
        assert!((stats.market_share - 0.5).abs() < 1e-9);
    }

    #[test]
    fn percentiles_track_the_distribution() {
        let mut summary = Summary::new(1);
        // -100..-1, so the bad tail is the most negative.
        for i in 1..=100 {
            summary.record(1, bps(-f64::from(i), 0.0));
        }
        let stats = &summary.stats()[0];
        assert!((stats.p50_bps + 50.0).abs() <= 1.0, "p50 {}", stats.p50_bps);
        assert!((stats.p05_bps + 95.0).abs() <= 1.0, "p05 {}", stats.p05_bps);
        assert!((stats.p01_bps + 99.0).abs() <= 1.0, "p01 {}", stats.p01_bps);
    }

    #[test]
    fn percentile_and_mean_helpers_tolerate_empty_input() {
        assert!(percentile(&[], 50).abs() < f64::EPSILON);
        assert!(winsorized_mean(&[]).abs() < f64::EPSILON);
        assert!(mean(&[]).abs() < f64::EPSILON);
        assert!(mean_abs(&[]).abs() < f64::EPSILON);
        assert!(share(&[], |_| true).abs() < f64::EPSILON);
    }
}
