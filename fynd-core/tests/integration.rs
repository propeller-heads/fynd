#![cfg(feature = "test-utils")]

// The `fynd` binary sets jemalloc as its global allocator, but `#[global_allocator]` binds to one
// binary crate and this test binary is a separate one. Benchmarks replay the market through here,
// so without this they would measure the system allocator and report any allocator change as a
// null. Kept in the test crate root rather than the library: a library must not impose an
// allocator on its consumers.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[path = "integration/harness.rs"]
mod harness;

#[path = "integration/derived_data_tests.rs"]
mod derived_data_tests;
#[path = "integration/solution_tests.rs"]
mod solution_tests;
#[path = "integration/timing_tests.rs"]
mod timing_tests;
