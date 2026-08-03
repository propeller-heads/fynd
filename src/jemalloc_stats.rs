//! jemalloc heap-statistics reporter.
//!
//! Compiled only with the `jemalloc` feature. Emits Prometheus gauges so the
//! resident-vs-allocated split is observable: a rising `resident` while
//! `allocated` stays flat means the allocator is holding freed pages
//! (fragmentation / arena retention) rather than the application leaking. The
//! gauge calls are no-ops when the `metrics` feature is off (no recorder installed).

use std::time::Duration;

use metrics::gauge;
use tikv_jemalloc_ctl::{epoch, stats};
use tracing::error;

const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Spawns a task that emits jemalloc heap statistics every [`REPORT_INTERVAL`].
pub fn spawn_stats_reporter() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(REPORT_INTERVAL);
        loop {
            ticker.tick().await;
            // A read only fails if jemalloc was built without `stats`, i.e. a
            // permanent build misconfiguration. Surface it once and stop rather
            // than spamming the log on every interval.
            if let Err(err) = report_once() {
                error!("jemalloc stats reporter stopping, read failed: {err}");
                break;
            }
        }
    })
}

fn report_once() -> Result<(), tikv_jemalloc_ctl::Error> {
    // jemalloc caches stats; advancing the epoch refreshes every reading below.
    epoch::advance()?;
    gauge!("jemalloc_allocated_bytes").set(stats::allocated::read()? as f64);
    gauge!("jemalloc_active_bytes").set(stats::active::read()? as f64);
    gauge!("jemalloc_resident_bytes").set(stats::resident::read()? as f64);
    gauge!("jemalloc_retained_bytes").set(stats::retained::read()? as f64);
    gauge!("jemalloc_mapped_bytes").set(stats::mapped::read()? as f64);
    Ok(())
}
