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
