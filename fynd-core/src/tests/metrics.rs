//! Reading back what a run recorded, for tests that assert on metrics.

use metrics_util::debugging::{DebugValue, Snapshotter};

/// One recorded metric: its name, its labels as `key=value`, and its value.
pub(crate) type Recorded = (String, Vec<String>, DebugValue);

/// Names every metric a run recorded, with its labels and value.
pub(crate) fn recorded_metrics(snapshotter: &Snapshotter) -> Vec<Recorded> {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(key, _, _, value)| {
            (
                key.key().name().to_string(),
                key.key()
                    .labels()
                    .map(|label| format!("{}={}", label.key(), label.value()))
                    .collect(),
                value,
            )
        })
        .collect()
}
