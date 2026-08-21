//! Trade shapes drawn from a `monitor` run's comparison JSONL.
//!
//! Only the **shape** of a settled trade is taken — its token pair and input amount. Nothing about
//! how it settled is read: not the settled amounts, not the `top`/`back` outcomes, and above all
//! not the existing `slippage` field, which measures a route replayed into the very block its own
//! trade settled in and is therefore contaminated by that trade's own price impact.
//!
//! Sampled shapes are re-quoted at live blocks unrelated to any settlement, so nothing the sampled
//! trade did on-chain can move the pools we measure against.

use std::{
    io::{BufRead, BufReader},
    path::Path,
};

use alloy::primitives::{Address, U256};
use anyhow::Context;
use serde::Deserialize;
use tracing::info;

/// The venue whose flow is sampled. Compared case-insensitively against a record's `venue`.
pub(crate) const RELAY_VENUE: &str = "relay";

/// A trade's routing-essential shape: what to ask the solver for, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TradeShape {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
}

/// The fields of a comparison record this module reads. Everything else in the record is ignored by
/// omission, so no settled-outcome field can leak into the sample.
///
/// All three trade fields are optional so that a row with no shape to extract — whichever
/// producer wrote it, and whatever the reason — still parses cleanly. Such a row is well-formed
/// and expected, not corrupt, so it must parse and then be dropped for having no shape rather than
/// counted as malformed.
#[derive(Deserialize)]
struct ComparisonRow {
    venue: Option<String>,
    token_in: Option<String>,
    token_out: Option<String>,
    amount_in: Option<String>,
}

impl ComparisonRow {
    /// The row's trade shape, or `None` when it is not a usable sample: a different venue, a
    /// reverted trade with no amounts to read, an unparseable address or amount, or a zero input
    /// (nothing to quote).
    fn shape(&self, venue: &str) -> Option<TradeShape> {
        if !self
            .venue
            .as_deref()?
            .eq_ignore_ascii_case(venue)
        {
            return None;
        }
        let amount_in = U256::from_str_radix(self.amount_in.as_deref()?, 10).ok()?;
        if amount_in.is_zero() {
            return None;
        }
        Some(TradeShape {
            token_in: self.token_in.as_deref()?.parse().ok()?,
            token_out: self
                .token_out
                .as_deref()?
                .parse()
                .ok()?,
            amount_in,
        })
    }
}

/// Whether `name` is one of the daily comparison files `monitor` writes.
fn is_comparisons_file(name: &str) -> bool {
    let prefixed = name
        .strip_prefix(crate::resolve::jsonl::COMPARISONS_PREFIX)
        .is_some_and(|rest| rest.starts_with('-'));
    prefixed &&
        Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Load every `venue`-matching trade shape from the `comparisons-*.jsonl` files in `dir`.
///
/// Shapes are kept **per record, with duplicates**, so a pair that trades often is proportionally
/// represented and the sample reflects real demand rather than a flat list of distinct pairs.
/// Malformed lines are counted and skipped: these files are appended by a long-running process, so
/// the last line of a live file is routinely a partial write.
pub(crate) fn load_shapes(dir: &Path, venue: &str) -> anyhow::Result<Vec<TradeShape>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read comparisons directory {}", dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("failed to list {}", dir.display()))?
            .path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_comparisons_file)
        {
            paths.push(path);
        }
    }
    // Deterministic order, so a given directory always yields the same pool and `--seed` alone
    // decides the draw.
    paths.sort();
    anyhow::ensure!(
        !paths.is_empty(),
        "no comparisons-*.jsonl files in {} — point --comparisons-dir at a monitor run's output",
        dir.display()
    );

    let mut shapes = Vec::new();
    let mut skipped = 0_u64;
    for path in &paths {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                skipped += 1;
                continue;
            };
            // A row that parses but does not match the venue is the common case, not a defect, so
            // only a parse failure counts as skipped.
            match serde_json::from_str::<ComparisonRow>(&line) {
                Ok(row) => shapes.extend(row.shape(venue)),
                Err(_) => skipped += 1,
            }
        }
    }
    info!(
        files = paths.len(),
        shapes = shapes.len(),
        skipped_lines = skipped,
        venue,
        "loaded trade shapes"
    );
    anyhow::ensure!(
        !shapes.is_empty(),
        "no usable {venue} trade shapes found in {} ({skipped} unparseable lines)",
        dir.display()
    );
    Ok(shapes)
}

/// A seeded sampler over a fixed pool of trade shapes.
///
/// Deliberately does not use the `rand` crate: `rand` does not guarantee that a given seed produces
/// the same values across releases, which would silently invalidate `--seed` reproducibility on a
/// dependency bump. `SplitMix64` is pinned here instead, so a seed reproduces its draw for as long
/// as this file does — and it costs no dependency.
pub(crate) struct Sampler {
    state: u64,
    /// A permutation buffer over the pool's indices, partially shuffled on each draw.
    indices: Vec<usize>,
}

impl Sampler {
    pub(crate) fn new(seed: u64, pool_len: usize) -> Self {
        Self { state: seed, indices: (0..pool_len).collect() }
    }

    /// `SplitMix64`, the reference generator: one multiply-xor-shift finalizer over a
    /// golden-ratio-incremented counter. Good enough for choosing which trades to quote, and small
    /// enough to be obviously stable.
    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform index below `bound`, via Lemire's multiply-shift. Biased by at most 2^-64 per
    /// draw, which no sample of trades can notice.
    fn index_below(&mut self, bound: usize) -> usize {
        let product = u128::from(self.next_u64()) * bound as u128;
        // The high 64 bits are strictly below `bound`, so they always fit a usize; the fallback
        // only exists because `try_from` must return something.
        usize::try_from(product >> 64).unwrap_or(0)
    }

    /// Draw `count` distinct pool indices, uniformly and without replacement within the draw.
    ///
    /// Partial Fisher-Yates over the persistent permutation buffer: each call reshuffles only the
    /// first `count` slots, so a draw costs `count` swaps regardless of pool size. Successive calls
    /// keep permuting the same buffer, so each round gets a fresh sample. `count` above the pool
    /// size yields the whole pool.
    pub(crate) fn draw(&mut self, count: usize) -> Vec<usize> {
        let len = self.indices.len();
        let count = count.min(len);
        for slot in 0..count {
            let pick = slot + self.index_below(len - slot);
            self.indices.swap(slot, pick);
        }
        self.indices[..count].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(venue: &str, amount: &str) -> String {
        format!(
            r#"{{"venue":"{venue}","token_in":"0x{a}","token_out":"0x{b}","amount_in":"{amount}","settled_amount_out":"5","slippage":{{"bps":-3.0,"usd":0.0}}}}"#,
            a = "11".repeat(20),
            b = "22".repeat(20)
        )
    }

    fn write_dir(lines: &[String]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path()
                .join("comparisons-2026-08-03.jsonl"),
            lines.join("\n"),
        )
        .expect("write");
        dir
    }

    #[test]
    fn keeps_only_the_requested_venue() {
        let dir = write_dir(&[
            row("relay", "1000"),
            row("uniswap", "2000"),
            row("Relay", "3000"),
            row("kyberswap", "4000"),
        ]);
        let shapes = load_shapes(dir.path(), RELAY_VENUE).expect("load");
        // Venue match is case-insensitive, so "Relay" counts too.
        assert_eq!(shapes.len(), 2);
        let amounts: Vec<U256> = shapes
            .iter()
            .map(|s| s.amount_in)
            .collect();
        assert_eq!(amounts, vec![U256::from(1000), U256::from(3000)]);
    }

    #[test]
    fn keeps_duplicates_so_frequent_pairs_stay_weighted() {
        let dir = write_dir(&[row("relay", "1000"), row("relay", "1000"), row("relay", "1000")]);
        assert_eq!(
            load_shapes(dir.path(), RELAY_VENUE)
                .expect("load")
                .len(),
            3
        );
    }

    #[test]
    fn null_trade_fields_are_dropped_without_counting_as_malformed() {
        // A record with no shape to extract is well-formed, not corrupt: it must parse and then
        // be dropped for lacking a shape, rather than counted among the skipped/malformed lines.
        let no_shape = r#"{"venue":"relay","token_in":null,"token_out":null,"amount_in":null}"#;
        let dir = write_dir(&[no_shape.to_string(), row("relay", "1000")]);
        let shapes = load_shapes(dir.path(), RELAY_VENUE).expect("load");
        assert_eq!(shapes.len(), 1, "only the row with a shape survives");
        assert_eq!(shapes[0].amount_in, U256::from(1000));
    }

    #[test]
    fn skips_zero_amounts_and_malformed_lines() {
        let dir = write_dir(&[
            row("relay", "1000"),
            row("relay", "0"),
            "not json at all".to_string(),
            // A partial write, which a live file's last line routinely is.
            r#"{"venue":"relay","token_in":"0x11"#.to_string(),
        ]);
        let shapes = load_shapes(dir.path(), RELAY_VENUE).expect("load");
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].amount_in, U256::from(1000));
    }

    #[test]
    fn native_leg_shapes_survive_the_round_trip() {
        // ~55% of Base Relay records carry the zero address as a native-ETH leg; it must reach the
        // solver unchanged rather than being filtered as a bad address.
        let dir = write_dir(&[format!(
            r#"{{"venue":"relay","token_in":"0x{}","token_out":"0x{}","amount_in":"7"}}"#,
            "33".repeat(20),
            "0".repeat(40)
        )]);
        let shapes = load_shapes(dir.path(), RELAY_VENUE).expect("load");
        assert_eq!(shapes[0].token_out, Address::ZERO);
        assert_eq!(shapes[0].amount_in, U256::from(7));
    }

    #[test]
    fn errors_when_the_directory_has_no_comparison_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("report.html"), "<html/>").expect("write");
        assert!(load_shapes(dir.path(), RELAY_VENUE).is_err());
    }

    #[test]
    fn errors_when_no_record_matches_the_venue() {
        let dir = write_dir(&[row("uniswap", "1000")]);
        assert!(load_shapes(dir.path(), RELAY_VENUE).is_err());
    }

    #[test]
    fn draw_is_reproducible_for_a_seed_and_differs_across_seeds() {
        let first = Sampler::new(42, 100).draw(10);
        assert_eq!(first, Sampler::new(42, 100).draw(10), "same seed must replay the same draw");
        assert_ne!(first, Sampler::new(43, 100).draw(10), "a different seed must differ");
    }

    #[test]
    fn draw_yields_distinct_in_range_indices() {
        let mut sampler = Sampler::new(7, 50);
        let drawn = sampler.draw(20);
        assert_eq!(drawn.len(), 20);
        assert!(drawn.iter().all(|&i| i < 50));
        let unique: std::collections::HashSet<usize> = drawn.iter().copied().collect();
        assert_eq!(unique.len(), 20, "a draw must not repeat an index");
    }

    #[test]
    fn successive_draws_differ() {
        let mut sampler = Sampler::new(7, 500);
        // Each round re-quotes a fresh sample; identical consecutive draws would mean the
        // permutation buffer is not advancing.
        assert_ne!(sampler.draw(20), sampler.draw(20));
    }

    #[test]
    fn draw_larger_than_the_pool_yields_the_whole_pool() {
        let mut sampler = Sampler::new(1, 3);
        let drawn = sampler.draw(10);
        assert_eq!(drawn.len(), 3);
        let unique: std::collections::HashSet<usize> = drawn.iter().copied().collect();
        assert_eq!(unique, [0, 1, 2].into_iter().collect());
    }

    #[test]
    fn draw_covers_the_pool_roughly_uniformly() {
        // A biased index_below would concentrate draws; over many rounds every index should appear.
        let mut sampler = Sampler::new(99, 20);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.extend(sampler.draw(5));
        }
        assert_eq!(seen.len(), 20, "every pool index should be drawn eventually");
    }
}
