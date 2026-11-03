//! The JSON file that keeps the health records across restarts.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _};
use fynd_core::ComponentId;
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

use crate::monitor::{component_health::ComponentHealth, health_store::HealthStore};

/// The format version of the file. A file of another version is refused.
const FORMAT_VERSION: u32 = 1;

/// The longest the file lags behind the store while changes are pending.
const SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// What the file holds: the version and the record of each tracked component.
#[derive(Serialize, Deserialize)]
struct FileContents {
    version: u32,
    components: BTreeMap<ComponentId, ComponentHealth>,
}

/// Reads the health records from the file at `path`. A missing file is a fresh start.
///
/// # Errors
///
/// Fails, naming the path, when the file cannot be read, is not the JSON of a health file, or is
/// of another format version.
pub fn load(path: &Path) -> anyhow::Result<BTreeMap<ComponentId, ComponentHealth>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read the health file {path:?}"))
        }
    };
    let contents: FileContents = serde_json::from_slice(&bytes)
        .with_context(|| format!("the health file {path:?} is malformed; fix or remove it"))?;
    if contents.version != FORMAT_VERSION {
        bail!(
            "the health file {path:?} has format version {}, this service reads version \
             {FORMAT_VERSION}; remove it to start afresh",
            contents.version
        );
    }
    Ok(contents.components)
}

/// Writes `components` to the file at `path`, through a temporary file in the same directory
/// that is renamed over it, so the file is never half written.
///
/// # Errors
///
/// Fails, naming the path, when the file cannot be written.
pub fn save(
    path: &Path,
    components: &BTreeMap<ComponentId, ComponentHealth>,
) -> anyhow::Result<()> {
    let contents = serde_json::to_vec(&FileContents {
        version: FORMAT_VERSION,
        components: components.clone(),
    })
    .context("cannot encode the health records")?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut file = File::create(&temporary)
        .with_context(|| format!("cannot create the temporary health file {temporary:?}"))?;
    file.write_all(&contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write the temporary health file {temporary:?}"))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("cannot move {temporary:?} over the health file {path:?}"))
}

/// Writes the store to the file at `path` every few seconds while it has unsaved changes.
///
/// A failed write is logged and tried again on the next round.
pub async fn keep_saved(store: Arc<HealthStore>, path: PathBuf) {
    let mut ticker = tokio::time::interval(SAVE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(components) = store.take_unsaved() else {
            continue;
        };
        let target = path.clone();
        let written = tokio::task::spawn_blocking(move || save(&target, &components)).await;
        match written {
            Ok(Ok(())) => debug!(?path, "saved the health records"),
            Ok(Err(error)) => {
                error!("{error:#}");
                store.mark_unsaved();
            }
            Err(error) => {
                error!(?path, "the task that saves the health records failed: {error}");
                store.mark_unsaved();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::monitor::component_health::{HealthStatus, HealthThresholds, Observation};

    fn broken_records() -> BTreeMap<ComponentId, ComponentHealth> {
        let thresholds =
            HealthThresholds { broken_after: 1, inaccurate_after: 3, inaccuracy_bps: 100.0 };
        let now = Utc::now();
        let mut broken = ComponentHealth::new("uniswap_v3".to_string(), now);
        broken.record(
            Observation::PoolReverted { block: 9, reason: "PoolEmpty".to_string() },
            &thresholds,
            now,
        );
        let fresh = ComponentHealth::new("uniswap_v2".to_string(), now);
        BTreeMap::from([("0xa".to_string(), broken), ("0xb".to_string(), fresh)])
    }

    #[test]
    fn test_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health.json");
        let records = broken_records();

        save(&path, &records).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded, records);
        assert_eq!(loaded["0xa"].status, HealthStatus::Broken);
    }

    #[test]
    fn test_missing_file_is_a_fresh_start() {
        let directory = tempfile::tempdir().unwrap();

        let loaded = load(&directory.path().join("absent.json")).unwrap();

        assert!(loaded.is_empty());
    }

    #[test]
    fn test_malformed_file_names_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health.json");
        fs::write(&path, b"{\"version\": 1, \"components\": [").unwrap();

        let error = format!("{:#}", load(&path).unwrap_err());

        assert!(error.contains("health.json"), "{error}");
        assert!(error.contains("malformed"), "{error}");
    }

    #[test]
    fn test_other_format_version_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health.json");
        fs::write(&path, br#"{"version": 2, "components": {}}"#).unwrap();

        let error = format!("{:#}", load(&path).unwrap_err());

        assert!(error.contains("format version 2"), "{error}");
    }

    #[test]
    fn test_save_replaces_the_file_and_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health.json");
        save(&path, &broken_records()).unwrap();

        save(&path, &BTreeMap::new()).unwrap();

        assert!(load(&path).unwrap().is_empty());
        let files: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, ["health.json"]);
    }

    #[test]
    fn test_save_into_a_missing_directory_names_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join("absent")
            .join("health.json");

        let error = format!("{:#}", save(&path, &BTreeMap::new()).unwrap_err());

        assert!(error.contains("health.json.tmp"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_keep_saved_writes_changes_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health.json");
        let store = Arc::new(HealthStore::new(
            HealthThresholds { broken_after: 2, inaccurate_after: 3, inaccuracy_bps: 100.0 },
            BTreeMap::new(),
        ));
        tokio::spawn(keep_saved(Arc::clone(&store), path.clone()));

        tokio::time::sleep(SAVE_INTERVAL * 2).await;
        assert!(!path.exists(), "nothing changed, so nothing is written");
        store.track("0xa", "uniswap_v3", Utc::now());
        tokio::time::sleep(SAVE_INTERVAL * 2).await;

        assert_eq!(load(&path).unwrap().len(), 1);
        assert!(store.take_unsaved().is_none());
    }
}
