//! Dynamic library loader for participant submissions.
// Stubs — all items will be used once runner.rs is implemented.
#![allow(dead_code)]

use std::path::Path;

use fynd_algo_sdk::SpawnerHandle;
use libloading::Library;

/// Loads a participant's `.so` and returns its [`SpawnerHandle`].
///
/// # Safety
///
/// The `.so` must be compiled with the same `fynd-core` version and Rust toolchain.
/// Mixing versions produces undefined behaviour.
///
/// # Errors
///
/// Returns an error if the library cannot be opened or the `fynd_create_spawner` symbol
/// is not found.
pub unsafe fn load_submission(_path: &Path) -> anyhow::Result<(Library, SpawnerHandle)> {
    todo!("load .so, call fynd_create_spawner, return (Library, SpawnerHandle)")
}
