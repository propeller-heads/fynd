//! The benchmark. Everything it does lives in [`fynd_bench_harness::bench`]; this target exists so
//! `cargo bench` builds it optimised.

#[tokio::main]
async fn main() {
    fynd_bench_harness::bench::run().await;
}
