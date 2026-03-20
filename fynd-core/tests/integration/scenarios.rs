
// Re-export shared golden types from fynd-core so tests can use them directly.
pub use fynd_core::recording::{
    golden::load_test_scenarios, GoldenFile, GoldenMetadata, GoldenOutput, GoldenScenario,
};

pub fn golden_file_path() -> std::path::PathBuf {
    fynd_core::recording::golden::golden_file_path()
}

pub fn load_golden_file() -> Option<GoldenFile> {
    fynd_core::recording::golden::load_golden_file()
}

pub fn should_bless() -> bool {
    std::env::var("BLESS_GOLDEN").is_ok()
}

pub fn write_golden_file(golden: &GoldenFile) {
    let path = golden_file_path();
    let json =
        serde_json::to_string_pretty(golden).expect("failed to serialize golden file");
    std::fs::write(&path, json).expect("failed to write golden file");
    eprintln!("Golden outputs written to {}", path.display());
}
