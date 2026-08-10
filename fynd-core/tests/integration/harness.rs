use std::{collections::HashMap, time::Duration};

use fynd_core::{
    derived::{ComponentDepths, SpotPrices},
    Quote, QuoteOptions, QuoteRequest, SolveError, Solver,
};
use fynd_test_fixtures::{read_recording, TestScenario};
use tycho_simulation::tycho_common::models::Chain;

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
        let (chain, chain_name, updates, gas_price) = load_fixture();

        let solver = Solver::from_recording(chain, updates, load_pools(), gas_price)
            .await
            .expect("failed to build solver from recording");

        Self::wait_ready(solver, chain_name).await
    }

    /// Build the same pipeline, but seed spot prices and component depths instead of
    /// computing them from the recording.
    pub async fn from_fixture_hydrated(
        spot_prices: SpotPrices,
        component_depths: ComponentDepths,
    ) -> Self {
        let (chain, chain_name, updates, gas_price) = load_fixture();

        let solver = Solver::from_recording_hydrated(
            chain,
            updates,
            load_pools(),
            gas_price,
            spot_prices,
            component_depths,
        )
        .await
        .expect("failed to build hydrated solver from recording");

        Self::wait_ready(solver, chain_name).await
    }

    async fn wait_ready(solver: Solver, chain_name: String) -> Self {
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

/// Reads the recorded market and returns what every harness variant needs to build a solver.
fn load_fixture(
) -> (Chain, String, Vec<tycho_simulation::protocol::models::Update>, Option<num_bigint::BigUint>) {
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

    (chain, chain_name, recording.updates, gas_price)
}

fn load_pools() -> HashMap<String, fynd_core::PoolConfig> {
    let toml_content = include_str!("../../../worker_pools.toml");
    fynd_test_fixtures::parse_pools_toml(toml_content).expect("failed to parse worker_pools.toml")
}
