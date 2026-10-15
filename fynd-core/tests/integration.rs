#![cfg(feature = "test-utils")]

#[path = "integration/harness.rs"]
mod harness;

#[path = "integration/derived_data_tests.rs"]
mod derived_data_tests;
#[path = "integration/fixture_counts.rs"]
mod fixture_counts;
#[path = "integration/solution_tests.rs"]
mod solution_tests;
#[path = "integration/timing_tests.rs"]
mod timing_tests;
