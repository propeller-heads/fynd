use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, Layer};

use crate::harness::TestHarness;

/// Collects the `output_token` field of every `no gas price for output token`
/// debug event, counting how many quotes each token appeared on.
#[derive(Clone, Default)]
struct TokenSink(Arc<Mutex<BTreeMap<String, usize>>>);

struct FieldGrab(Option<String>);

impl Visit for FieldGrab {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "output_token" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

impl<S: Subscriber> Layer<S> for TokenSink {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut grab = FieldGrab(None);
        event.record(&mut grab);
        if let Some(token) = grab.0 {
            *self
                .0
                .lock()
                .expect("token sink poisoned")
                .entry(token)
                .or_default() += 1;
        }
    }
}

/// Replays the recorded market and quotes every fixture scenario, then prints
/// the set of output tokens whose gas price could not be resolved (the tokens
/// that trip the `bellman_ford` gross-amount fallback).
///
/// Ignored: it is an investigation aid, not an assertion. Run it with
/// `cargo nextest run -p fynd-core --features test-utils --test integration \
///   -E 'test(unpriced_output_tokens)' --run-ignored all --nocapture`.
#[tokio::test]
#[ignore = "diagnostic: prints unpriced output tokens, asserts nothing"]
async fn test_unpriced_output_tokens() {
    let sink = TokenSink::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(sink.clone()))
        .expect("a global subscriber was already installed");

    let harness = TestHarness::from_fixture().await;

    let scenarios = harness.scenarios();
    for scenario in &scenarios {
        let _ = harness
            .quote(vec![scenario.to_order()])
            .await;
    }

    let tokens = sink
        .0
        .lock()
        .expect("token sink poisoned");
    println!("\n=== output tokens with no resolvable gas price ===");
    println!("scenarios quoted: {}", scenarios.len());
    if tokens.is_empty() {
        println!("(none — every quoted route resolved a gas price for its output token)");
    } else {
        for (token, count) in tokens.iter() {
            println!("{token}  x{count}");
        }
    }
    println!("=================================================\n");
}
