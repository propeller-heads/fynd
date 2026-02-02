use crate::config::{BenchmarkResults, TimingStats};

impl TimingStats {
    /// Calculates comprehensive statistics from timing measurements.
    /// Returns min, max, mean, median, p95, p99, and standard deviation.
    pub fn from_measurements(times: &[u64]) -> Option<Self> {
        if times.is_empty() {
            return None;
        }

        if times.len() < 50 {
            tracing::warn!(
                "Calculating timing statistics from {} measurements, results may be unreliable",
                times.len()
            );
        }

        let mut sorted = times.to_vec();
        sorted.sort();

        let min = sorted[0];
        let max = sorted[sorted.len() - 1];
        let sum: u64 = sorted.iter().sum();
        let mean = sum / sorted.len() as u64;
        let median = sorted[sorted.len() / 2];
        let p95 = sorted[(sorted.len() as f64 * 0.95).min((sorted.len() - 1) as f64) as usize];
        let p99 = sorted[(sorted.len() as f64 * 0.99).min((sorted.len() - 1) as f64) as usize];

        // Calculate standard deviation
        let variance: f64 = sorted
            .iter()
            .map(|&x| {
                let diff = x as f64 - mean as f64;
                diff * diff
            })
            .sum::<f64>() /
            sorted.len() as f64;
        let std_dev = variance.sqrt();

        Some(Self { min, max, mean, median, p95, p99, std_dev })
    }

    /// Prints formatted statistics to the console with a given label.
    pub fn print(&self, label: &str) {
        println!("\n{}", label);
        println!("  Min:     {}ms", self.min);
        println!("  Mean:    {}ms", self.mean);
        println!("  Median:  {}ms", self.median);
        println!("  Std Dev: {:.2}ms", self.std_dev);
        println!("  P95:     {}ms", self.p95);
        println!("  P99:     {}ms", self.p99);
        println!("  Max:     {}ms", self.max);
    }
}

/// Prints formatted statistics to the console with a given label.
pub fn print_statistics(times: &[u64], label: &str) {
    if let Some(stats) = TimingStats::from_measurements(times) {
        stats.print(label);
    }
}

/// Prints an ASCII histogram with fixed-range buckets:
/// 0-100ms in 10ms steps, 100ms+ in 50ms steps.
/// Trims leading/trailing empty buckets but preserves interior gaps.
pub fn print_histogram(times: &[u64], label: &str, width: usize) {
    if times.is_empty() {
        return;
    }

    let max = *times.iter().max().unwrap();

    // Build fixed bucket boundaries
    let mut boundaries: Vec<u64> = Vec::new();
    // 0..100 in steps of 10
    let mut edge = 0u64;
    while edge < 100 && edge <= max {
        boundaries.push(edge);
        edge += 10;
    }
    // 100.. in steps of 50
    if edge <= max {
        while edge <= max {
            boundaries.push(edge);
            edge += 50;
        }
    }
    // Sentinel upper bound
    boundaries.push(edge);

    let num_buckets = boundaries.len() - 1;
    let mut buckets = vec![0usize; num_buckets];

    for &time in times {
        // Binary search for the right bucket
        let idx = match boundaries.binary_search(&time) {
            Ok(i) => i.min(num_buckets - 1),
            Err(i) => (i - 1).min(num_buckets - 1),
        };
        buckets[idx] += 1;
    }

    // Trim leading/trailing empty buckets
    let first_non_empty = buckets.iter().position(|&c| c > 0).unwrap_or(0);
    let last_non_empty = buckets.iter().rposition(|&c| c > 0).unwrap_or(0);

    println!("\n{} Distribution:", label);

    let visible = &buckets[first_non_empty..=last_non_empty];
    let max_count = *visible.iter().max().unwrap_or(&1);
    let scale = if max_count > width { max_count as f64 / width as f64 } else { 1.0 };

    for i in first_non_empty..=last_non_empty {
        let lo = boundaries[i];
        let hi = boundaries[i + 1] - 1;
        let count = buckets[i];
        let bar_length = (count as f64 / scale).round() as usize;
        let bar = "█".repeat(bar_length);
        println!("  {:>5}-{:<5}ms [{:>4}] {}", lo, hi, count, bar);
    }
}

/// Exports benchmark results to a JSON file with complete configuration and statistics.
/// The output includes worker pool configuration content and all timing measurements.
pub fn export_results(
    results: BenchmarkResults,
    output_file: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(&results)?;
    std::fs::write(&output_file, json)?;
    tracing::info!("Results exported to: {}", output_file);

    Ok(())
}
