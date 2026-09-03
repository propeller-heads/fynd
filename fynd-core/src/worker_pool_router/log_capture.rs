//! Collects formatted log output, so a test can inspect the bytes a log pipeline would see.
//!
//! The router writes its per-quote logs as one plain string rather than tracing fields, because
//! the formatter wraps field names and their `=` separators in ANSI escapes. Asserting on the
//! rendered bytes is what proves that, so the tests need the formatter's own output.

use std::sync::{Arc, Mutex};

use tracing::Level;

/// A writer that keeps every byte a subscriber formats.
#[derive(Clone, Default)]
pub(super) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `emit` under a capturing subscriber and returns the payload that follows `prefix` on each
/// line that carries it.
///
/// Colour is on, as it is in a deployment, so a payload that carried an escape sequence shows up
/// in what this returns.
pub(super) fn capture_payloads(prefix: &str, emit: impl FnOnce()) -> Vec<String> {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .compact()
        .with_ansi(true)
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, emit);
    let rendered = String::from_utf8(
        logs.0
            .lock()
            .expect("log buffer poisoned")
            .clone(),
    )
    .expect("utf-8");
    rendered
        .lines()
        .filter_map(|line| line.split_once(prefix))
        .map(|(_, payload)| payload.to_string())
        .collect()
}
