//! CLI for the offline APEX batch-clearing surplus analysis (see the `apex_batch` library for
//! the analysis itself).
//!
//! Two subcommands, in the order they are meant to be run:
//!
//! - **`shadow`** first: timing only. APEX's runtime at Ethereum scale is the study's biggest
//!   unknown, and the "in-block-ish" budget in the matrix is whatever the shadow run says is
//!   achievable. Running the matrix before this is running it against a guess.
//! - **`run`**: the full matrix — {parity, full-native} × {top, biased bottom} × two budgets.

use std::path::PathBuf;

use apex_batch::{
    adapter::LimitPolicy,
    runner::{Position, ProtocolSet, RunConfig},
};
use clap::{Args, Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "apex-batch", about = "Offline APEX batch-clearing surplus analysis")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the full configuration matrix over every captured block.
    Run(RunArgs),
    /// Timing only: solve a sample of blocks and report APEX's per-block wall-clock percentiles.
    Shadow(ShadowArgs),
}

/// The two recordings every subcommand joins, plus how hard to work the machine.
#[derive(Args)]
struct InputArgs {
    /// Directory of `batches-YYYY-MM-DD.jsonl` files written by `hindsight monitor
    /// --capture-dir`
    #[arg(long)]
    snapshots_dir: PathBuf,

    /// zstd-compressed `MarketRecording` covering the same blocks, from `record-market`
    #[arg(long)]
    recording: PathBuf,

    /// Blocks solved in parallel. Defaults to the machine's core count; on a laptop this is the
    /// knob that keeps the run from starving APEX's own inner parallelism
    #[arg(long, default_value_t = num_cpus_default())]
    jobs: usize,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    inputs: InputArgs,

    /// APEX wall-clock budget per block, in milliseconds. Repeat for each budget in the matrix;
    /// the defaults are the "in-block-ish" and exploratory budgets from the plan
    #[arg(long = "budget-ms", default_values_t = [10_000u64, 20_000])]
    budget_ms: Vec<u64>,

    /// Where in the block to place the batch. Repeat to run both; `top` clears against state N-1
    /// and `bottom` against state N (biased against the batch — the pools were already moved by
    /// the trades it is clearing)
    #[arg(long = "position", value_enum, default_values_t = [Position::Top, Position::BiasedBottom])]
    position: Vec<Position>,

    /// Which pools the batch may use. Repeat to run both; `parity` matches turbine's
    /// staging/prod protocol set, `full-native` takes every natively-simulated protocol
    #[arg(long = "protocol-set", value_enum, default_values_t = [ProtocolSet::Parity, ProtocolSet::FullNative])]
    protocol_set: Vec<ProtocolSet>,

    /// Override every order's limit with the executed output less this many basis points. Omit
    /// for the core matrix, which uses the captured limits; this is the sensitivity band that
    /// bounds how much the synthetic-limit assumption moves the answer
    #[arg(long)]
    limit_bps: Option<u32>,

    /// Write the per-order results and aggregates here as JSON
    #[arg(long, short = 'o')]
    out: PathBuf,
}

#[derive(Args)]
struct ShadowArgs {
    #[command(flatten)]
    inputs: InputArgs,

    /// How many captured blocks to sample. Sampling, not the first N: a contiguous run of blocks
    /// shares its market conditions, and the tail is what the budget has to survive
    #[arg(long, default_value_t = 50)]
    blocks: usize,

    /// Which pools the batch may use. Timing tracks pool count, so the shadow run measures the
    /// set the matrix will actually use
    #[arg(long, value_enum, default_value_t = ProtocolSet::Parity)]
    protocol_set: ProtocolSet,

    /// APEX wall-clock budget per block, in milliseconds. Generous by default: a shadow run
    /// measures how long APEX *wants*, so a tight budget would only measure the budget
    #[arg(long, default_value_t = 60_000)]
    budget_ms: u64,

    /// Write the timing summary here as JSON
    #[arg(long, short = 'o')]
    out: PathBuf,
}

/// Default `--jobs`: every core. Blocks are independent, so oversubscribing costs only APEX's
/// own worker pool.
fn num_cpus_default() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("apex_batch=info".parse()?))
        .init();

    match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::Shadow(args) => shadow(args),
    }
}

/// Run the full matrix and write per-order results plus aggregates.
fn run(args: RunArgs) -> anyhow::Result<()> {
    let configs = matrix_configs(&args.protocol_set, &args.position, &args.budget_ms);
    let policy = limit_policy(args.limit_bps);
    info!(
        cells = configs.len(),
        ?policy,
        out = %args.out.display(),
        "expanded the configuration matrix"
    );
    execute_matrix(&args.inputs, &configs, policy, &args.out)
}

/// Should: load the snapshots and the recording, hand `configs` to
/// [`apex_batch::runner::run_matrix`], aggregate with [`apex_batch::analysis::aggregate`], and
/// write the per-order results and aggregates to `out`.
fn execute_matrix(
    _inputs: &InputArgs,
    _configs: &[RunConfig],
    _limit_policy: LimitPolicy,
    _out: &std::path::Path,
) -> anyhow::Result<()> {
    todo!("load both recordings, run the config matrix, aggregate, and write the report")
}

/// Measure APEX's per-block wall-clock over a sample of blocks.
fn shadow(args: ShadowArgs) -> anyhow::Result<()> {
    // One cell only: a shadow run measures how long APEX wants, so the position and limit policy
    // are held fixed and the budget is deliberately generous.
    let config = RunConfig {
        protocol_set: args.protocol_set,
        position: Position::Top,
        budget_ms: args.budget_ms,
        label: RunConfig::derive_label(args.protocol_set, Position::Top, args.budget_ms),
    };
    info!(
        label = config.label,
        blocks = args.blocks,
        out = %args.out.display(),
        "measuring APEX wall-clock over a block sample"
    );
    measure_timings(&args.inputs, &config, args.blocks, &args.out)
}

/// Should: load both recordings, sample `blocks` captured blocks spread across the capture rather
/// than taken from its head (a contiguous run shares its market conditions, and it is the tail
/// the budget has to survive), solve each under `config`, and write
/// [`apex_batch::analysis::timings`]' percentiles to `out`. This is what fixes the matrix's
/// in-block budget, so it reports pool counts alongside the times — runtime is expected to track
/// pool count, and pool count is the knob (min TVL, top-K pools per pair) available if the tail
/// is too slow.
fn measure_timings(
    _inputs: &InputArgs,
    _config: &RunConfig,
    _blocks: usize,
    _out: &std::path::Path,
) -> anyhow::Result<()> {
    todo!("sample blocks, time one APEX solve each, and write the wall-clock percentiles")
}

/// Expand the matrix axes into one [`RunConfig`] per cell.
fn matrix_configs(
    protocol_sets: &[ProtocolSet],
    positions: &[Position],
    budgets_ms: &[u64],
) -> Vec<RunConfig> {
    let mut configs = Vec::with_capacity(protocol_sets.len() * positions.len() * budgets_ms.len());
    for &protocol_set in protocol_sets {
        for &position in positions {
            for &budget_ms in budgets_ms {
                configs.push(RunConfig {
                    protocol_set,
                    position,
                    budget_ms,
                    label: RunConfig::derive_label(protocol_set, position, budget_ms),
                });
            }
        }
    }
    configs
}

/// The limit policy the run's flags select: the captured limits unless `--limit-bps` overrides
/// them for the sensitivity band.
fn limit_policy(limit_bps: Option<u32>) -> LimitPolicy {
    match limit_bps {
        Some(bps) => LimitPolicy::ExecutedLessBps(bps),
        None => LimitPolicy::AsCaptured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses_both_subcommands() {
        // The arguments are the contract with whoever runs the study; a rename that silently
        // stops parsing would only surface hours into a run.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_matrix_expands_to_one_config_per_cell() {
        let configs = matrix_configs(
            &[ProtocolSet::Parity, ProtocolSet::FullNative],
            &[Position::Top, Position::BiasedBottom],
            &[10_000, 20_000],
        );
        assert_eq!(configs.len(), 8, "the core matrix is 8 runs per block");

        let mut labels: Vec<&str> = configs
            .iter()
            .map(|config| config.label.as_str())
            .collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), configs.len(), "every cell needs its own label");
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_limit_policy_defaults_to_the_captured_limits() {
        // The core matrix must use the real extracted floors; the bps override is opt-in, so a
        // forgotten flag cannot silently turn the headline into a sensitivity run.
        assert_eq!(limit_policy(None), LimitPolicy::AsCaptured);
        assert_eq!(limit_policy(Some(200)), LimitPolicy::ExecutedLessBps(200));
    }
}
