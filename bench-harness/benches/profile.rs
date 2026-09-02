//! The profiler. Everything it does lives in [`fynd_bench_harness::profile`]; this target exists so
//! `cargo bench` builds it optimised.

#[tokio::main]
async fn main() {
    fynd_bench_harness::profile::run().await;
}
