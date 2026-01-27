use tycho_solver::SolutionRequest;

use crate::config::{
    BenchmarkConfig, BenchmarkResults, BenchmarkStatistics, ParallelizationMode, TimingStats,
};

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

/// Prints an ASCII histogram showing the distribution of timing values across 10 buckets.
pub fn print_histogram(times: &[u64], label: &str, width: usize) {
    if times.is_empty() {
        return;
    }

    let mut sorted = times.to_vec();
    sorted.sort();

    let min = sorted[0];
    let max = sorted[sorted.len() - 1];

    if min == max {
        println!("\n{} - All values are {}ms", label, min);
        return;
    }

    println!("\n{} Distribution:", label);

    let num_buckets = 10;
    let bucket_size = (max - min).div_ceil(num_buckets);
    let mut buckets = vec![0usize; num_buckets as usize];

    for &time in times {
        let bucket_idx = ((time - min) / bucket_size).min(num_buckets - 1) as usize;
        buckets[bucket_idx] += 1;
    }

    let max_count = *buckets.iter().max().unwrap();
    let scale = if max_count > width { max_count as f64 / width as f64 } else { 1.0 };

    for (i, &count) in buckets.iter().enumerate() {
        let bucket_start = min + (i as u64 * bucket_size);
        let bucket_end = bucket_start + bucket_size - 1;
        let bar_length = (count as f64 / scale).round() as usize;
        let bar = "█".repeat(bar_length);

        println!("  {:>5}-{:<5}ms [{:>4}] {}", bucket_start, bucket_end, count, bar);
    }
}

/// Exports benchmark results to a JSON file with complete configuration and statistics.
/// The output includes worker pool configuration content and all timing measurements.
#[allow(clippy::too_many_arguments)]
pub fn export_results(
    chain_str: String,
    rpc_url: String,
    tycho_url: String,
    protocols: Vec<String>,
    http_port: u16,
    num_requests: usize,
    parallelization_mode: ParallelizationMode,
    worker_pools_config_path: String,
    worker_pools_config_content: String,
    output_file: String,
    requests_file: Option<String>,
    requests: Vec<SolutionRequest>,
    successful_requests: usize,
    failed_requests: usize,
    total_duration_ms: u64,
    throughput_rps: f64,
    round_trip_times: Vec<u64>,
    solve_times: Vec<u64>,
    overheads: Vec<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let benchmark_config = BenchmarkConfig {
        chain: chain_str,
        rpc_url,
        tycho_url,
        protocols,
        http_port,
        num_requests,
        parallelization_mode,
        worker_pools_config_path,
        worker_pools_config: worker_pools_config_content,
        requests_file,
        num_request_templates: requests.len(),
    };

    let round_trip_stats = TimingStats::from_measurements(&round_trip_times).unwrap();
    let solve_time_stats = TimingStats::from_measurements(&solve_times).unwrap();
    let overhead_stats = TimingStats::from_measurements(&overheads).unwrap();

    let results = BenchmarkResults {
        config: benchmark_config,
        request_templates: requests,
        successful_requests,
        failed_requests,
        total_duration_ms,
        throughput_rps,
        round_trip_times_ms: round_trip_times,
        solve_times_ms: solve_times,
        overhead_times_ms: overheads,
        statistics: BenchmarkStatistics {
            round_trip: round_trip_stats,
            solve_time: solve_time_stats,
            overhead: overhead_stats,
        },
    };

    let json = serde_json::to_string_pretty(&results)?;
    std::fs::write(&output_file, json)?;
    tracing::info!("Results exported to: {}", output_file);

    Ok(())
}
