//! The benchmark over the algorithms built into `fynd-core`.
//!
//! Everything it does lives in [`fynd_bench_harness::bench`]; this target exists so
//! `cargo bench` builds it optimised. A crate with its own algorithm declares a target like
//! this one and passes its own registry.

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match fynd_bench_harness::bench::run(&fynd_core::AlgorithmRegistry::new()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("error: {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}
