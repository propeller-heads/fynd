use fynd_core::recording::MarketRecording;
use serde::Serialize;

/// Generate golden outputs by replaying a recording through the full pipeline.
///
/// This uses the same replay approach as the test harness:
/// 1. Replay recorded Updates through TychoFeed::handle_tycho_message()
/// 2. Compute derived data via ComputationManager
/// 3. Build WorkerPoolRouter
/// 4. Run each test scenario and capture results
pub async fn generate_golden_outputs(recording: MarketRecording) -> anyhow::Result<GoldenFile> {
    // The implementation reuses the same pipeline construction as
    // TestHarness::from_recording() in the integration tests.
    let _ = recording;
    todo!("Implement once TestHarness pipeline is stable")
}

// Placeholder — real types will be imported from fynd-core
#[derive(Serialize)]
pub struct GoldenFile {
    pub scenarios: Vec<GoldenScenario>,
}

#[derive(Serialize)]
pub struct GoldenScenario;
