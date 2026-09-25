//! Times the derived computations on a recorded market.
//!
//! Everything it does lives in [`fynd_bench_harness::derived`]; this target exists so
//! `cargo bench` builds it optimised.

#[global_allocator]
static GLOBAL: fynd_bench_harness::heap::CountingAllocator =
    fynd_bench_harness::heap::CountingAllocator;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match fynd_bench_harness::derived::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("error: {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}
