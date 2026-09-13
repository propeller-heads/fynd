//! Captures what a subscriber formats, so a test can assert on the bytes a log pipeline reads.
//!
//! The router's logs put their payload in one preformatted string. Only the formatter's own output
//! shows whether that survived, so these tests read the rendered line rather than tracing fields.

use std::sync::{Arc, Mutex};

use tracing::Level;

/// A writer that keeps every byte a subscriber formats.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

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

/// Runs `emit` under a capturing subscriber and returns what follows `prefix` on each line
/// that carries it.
///
/// Colour is on, so an ANSI escape that reached the payload lands in what this returns and the
/// test that looks for one can see it.
pub(super) fn capture_payloads(prefix: &str, emit: impl FnOnce()) -> Vec<String> {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .compact()
        .with_ansi(true)
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, emit);
    let rendered = String::from_utf8(std::mem::take(
        &mut *logs
            .0
            .lock()
            .expect("log buffer poisoned"),
    ))
    .expect("utf-8");
    rendered
        .lines()
        .filter_map(|line| line.split_once(prefix))
        .map(|(_, payload)| payload.to_string())
        .collect()
}
