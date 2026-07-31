use std::{collections::HashMap, time::Duration};

use fynd_core::{Quote, QuoteOptions, QuoteRequest, SolveError, Solver};
use fynd_test_fixtures::{read_recording, TestScenario};

/// The fully constructed test pipeline, ready to receive quote requests.
pub struct TestHarness {
    solver: Solver,
    chain_name: String,
}

impl TestHarness {
    /// Load recording from the fixtures directory and build the full pipeline.
    ///
    /// The chain comes from the recording's metadata, so the tests always run
    /// against the chain the fixture was recorded on.
    pub async fn from_fixture() -> Self {
        let recording_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/market_recording.json.zst");

        let recording =
            read_recording(&recording_path).expect("failed to load market recording fixture");

        let chain_name = recording.metadata.chain.clone();
        let chain = fynd_core::types::parse_chain(&chain_name)
            .expect("recording fixture has unsupported chain");
        let gas_price = recording
            .metadata
            .gas_price_as_biguint();
        let pools = load_pools();

        let solver = Solver::from_recording(chain, recording.updates, pools, gas_price)
            .await
            .expect("failed to build solver from recording");

        solver
            .wait_until_ready(Duration::from_secs(120))
            .await
            .expect("solver not ready after 120s");

        Self { solver, chain_name }
    }

    /// Load the test scenarios for the recording's chain from
    /// `tests/fixtures/pairs/<chain>.json`.
    pub fn scenarios(&self) -> Vec<TestScenario> {
        let pairs_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pairs")
            .join(format!("{}.json", self.chain_name));
        let pairs_json = std::fs::read_to_string(&pairs_path).unwrap_or_else(|e| {
            panic!(
                "failed to read scenario pairs file {}: {e} — add a pairs file for chain '{}'",
                pairs_path.display(),
                self.chain_name
            )
        });
        fynd_test_fixtures::load_test_scenarios(&pairs_json).expect("failed to load test scenarios")
    }

    /// Run a single quote request and return the result.
    pub async fn quote(&self, orders: Vec<fynd_core::Order>) -> Result<Quote, SolveError> {
        let request = QuoteRequest::new(orders, QuoteOptions::default());
        self.solver.quote(request).await
    }

    /// Access the solver for derived data inspection.
    pub fn solver(&self) -> &Solver {
        &self.solver
    }
}

fn load_pools() -> HashMap<String, fynd_core::PoolConfig> {
    let toml_content = include_str!("../../../worker_pools.toml");
    fynd_test_fixtures::parse_pools_toml(toml_content).expect("failed to parse worker_pools.toml")
}
