//! End-to-end integration tests using `sentry::test::with_captured_envelopes`.
//!
//! These tests exercise the full path: a `SentryRecorder` is bound as the
//! global metrics recorder, metrics are emitted, the recorder builds raw
//! `trace_metric` envelopes and ships them via the test transport, and we
//! parse the captured wire bytes back to assert the JSON shape.
//!
//! ## Why we re-parse wire bytes
//!
//! The current `sentry-rust` `EnvelopeItem` enum has no public variant for
//! arbitrary content-types, so we ship envelopes via `Envelope::from_bytes_raw`.
//! Captured envelopes therefore present as `Items::Raw(bytes)` and are not
//! decomposable through `envelope.items()`. We use `to_writer` to recover the
//! original bytes and parse the wire format directly.

use std::sync::Once;
use std::time::Duration;

use metrics::with_local_recorder;
use metrics_exporter_sentry_v2::SentryRecorder;
use sentry::ClientOptions;
use sentry::test::with_captured_envelopes_options;
use serde_json::Value;

/// Splits a captured envelope's raw wire bytes into (envelope_header_json,
/// item_header_json, payload_json) — the spec's three-line layout.
fn parse_trace_metric_envelope(envelope: &sentry::Envelope) -> Option<(Value, Value, Value)> {
    let mut buf = Vec::new();
    envelope.to_writer(&mut buf).ok()?;

    // Strip a single trailing newline if present so we don't see an empty
    // 4th line.
    let trimmed: &[u8] = buf.strip_suffix(b"\n").unwrap_or(&buf);
    let mut iter = trimmed.split(|&b| b == b'\n');
    let env_h = iter.next()?;
    let item_h = iter.next()?;
    let payload = iter.next()?;

    let env_h: Value = serde_json::from_slice(env_h).ok()?;
    let item_h: Value = serde_json::from_slice(item_h).ok()?;
    // Only treat envelopes whose item is a trace_metric as "ours".
    if item_h.get("type")? != "trace_metric" {
        return None;
    }
    let payload: Value = serde_json::from_slice(payload).ok()?;
    Some((env_h, item_h, payload))
}

/// Filter captured envelopes down to just the ones we care about (trace_metric).
fn trace_metric_envelopes(envs: Vec<sentry::Envelope>) -> Vec<(Value, Value, Value)> {
    envs.iter()
        .filter_map(parse_trace_metric_envelope)
        .collect()
}

fn test_options() -> ClientOptions {
    ClientOptions {
        dsn: "https://public@example.com/1".parse().ok(),
        ..Default::default()
    }
}

fn ensure_tracing_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Best-effort tracing init for visibility on test failure.
        let _ =
            tracing::subscriber::set_global_default(tracing_subscriber_compat::FallbackSubscriber);
    });
}

mod tracing_subscriber_compat {
    //! A no-op tracing subscriber so we don't pull in tracing-subscriber as a
    //! dev-dep just for tests. Tracing emissions inside the recorder are
    //! invisible but harmless.
    use tracing::Subscriber;
    use tracing::span::{Attributes, Id, Record};
    pub struct FallbackSubscriber;
    impl Subscriber for FallbackSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }
}

#[test]
fn counter_increment_emits_counter_with_delta_value() {
    ensure_tracing_init();
    let envs = with_captured_envelopes_options(
        || {
            // Use a current-thread runtime so the time-based flush task can
            // attach (the recorder still works without it; this test triggers
            // a flush manually anyway).
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _g = rt.enter();
            let recorder = SentryRecorder::builder()
                .max_buffer_size(100)
                .flush_interval(Duration::from_secs(60))
                .build();

            with_local_recorder(&recorder, || {
                metrics::counter!("api.requests", "route" => "/health").increment(3);
                metrics::counter!("api.requests", "route" => "/health").increment(4);
            });

            recorder.flush();
        },
        test_options(),
    );

    let parsed = trace_metric_envelopes(envs);
    assert_eq!(
        parsed.len(),
        1,
        "expected exactly one trace_metric envelope"
    );
    let (_env_h, item_h, payload) = &parsed[0];

    assert_eq!(item_h["type"], "trace_metric");
    assert_eq!(
        item_h["content_type"],
        "application/vnd.sentry.items.trace-metric+json"
    );
    assert_eq!(payload["version"], 2);
    let items = payload["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "counter");
    // The Sentry spec defines counter `value` as a delta (the increment).
    assert_eq!(items[0]["value"], 3);
    assert_eq!(items[1]["value"], 4);
    assert_eq!(items[0]["name"], "api.requests");
    // Typed-attribute wrapper shape.
    assert_eq!(items[0]["attributes"]["route"]["value"], "/health");
    assert_eq!(items[0]["attributes"]["route"]["type"], "string");
}

#[test]
fn gauge_set_emits_gauge_with_absolute_value() {
    ensure_tracing_init();
    let envs = with_captured_envelopes_options(
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _g = rt.enter();
            let recorder = SentryRecorder::builder()
                .flush_interval(Duration::from_secs(60))
                .build();
            with_local_recorder(&recorder, || {
                metrics::gauge!("queue.depth").set(42.5);
            });
            recorder.flush();
        },
        test_options(),
    );
    let parsed = trace_metric_envelopes(envs);
    assert_eq!(parsed.len(), 1);
    let items = parsed[0].2["items"].as_array().unwrap();
    assert_eq!(items[0]["type"], "gauge");
    assert_eq!(items[0]["value"], 42.5);
}

#[test]
fn histogram_record_emits_distribution() {
    ensure_tracing_init();
    let envs = with_captured_envelopes_options(
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _g = rt.enter();
            let recorder = SentryRecorder::builder()
                .flush_interval(Duration::from_secs(60))
                .build();
            with_local_recorder(&recorder, || {
                metrics::histogram!("request.duration").record(120.0);
                metrics::histogram!("request.duration").record(45.0);
            });
            recorder.flush();
        },
        test_options(),
    );
    let parsed = trace_metric_envelopes(envs);
    assert_eq!(parsed.len(), 1);
    let items = parsed[0].2["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "distribution");
    assert_eq!(items[0]["value"], 120.0);
    assert_eq!(items[1]["value"], 45.0);
}

#[test]
fn flush_by_size_triggers_at_max_buffer() {
    ensure_tracing_init();
    let envs = with_captured_envelopes_options(
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _g = rt.enter();
            // Tiny buffer so we can trigger size flush deterministically.
            let recorder = SentryRecorder::builder()
                .max_buffer_size(2)
                .flush_interval(Duration::from_secs(3600))
                .build();
            with_local_recorder(&recorder, || {
                metrics::counter!("a").increment(1);
                metrics::counter!("a").increment(1);
                // At this point the buffer hit 2; a size-flush already ran.
                metrics::counter!("a").increment(1);
                metrics::counter!("a").increment(1);
                // Buffer hit 2 again → second size-flush.
            });
            // Don't call recorder.flush() — we want to prove size triggered
            // both flushes on its own. (Drop will flush any leftover.)
        },
        test_options(),
    );
    let parsed = trace_metric_envelopes(envs);
    assert!(
        parsed.len() >= 2,
        "expected at least two envelopes from size-triggered flushes, got {}",
        parsed.len()
    );
    // Total items across all envelopes must equal 4 emissions.
    let total: usize = parsed
        .iter()
        .map(|(_, _, p)| p["items"].as_array().map(|a| a.len()).unwrap_or(0))
        .sum();
    assert_eq!(total, 4);
}

#[test]
fn flush_by_time_triggers_after_interval() {
    ensure_tracing_init();
    let envs = with_captured_envelopes_options(
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Hold the runtime active for the duration of this block so the
            // background task gets to tick.
            rt.block_on(async {
                let recorder = SentryRecorder::builder()
                    .max_buffer_size(10_000) // never trigger size flush
                    .flush_interval(Duration::from_millis(50))
                    .build();
                with_local_recorder(&recorder, || {
                    metrics::counter!("a").increment(1);
                });
                // Wait for the background task to fire at least once.
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        },
        test_options(),
    );
    let parsed = trace_metric_envelopes(envs);
    assert!(
        !parsed.is_empty(),
        "expected at least one envelope from time-triggered flush"
    );
    let total: usize = parsed
        .iter()
        .map(|(_, _, p)| p["items"].as_array().map(|a| a.len()).unwrap_or(0))
        .sum();
    assert!(total >= 1);
}

#[test]
fn no_active_span_omits_trace_and_span_ids() {
    ensure_tracing_init();
    let envs = with_captured_envelopes_options(
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _g = rt.enter();
            let recorder = SentryRecorder::builder()
                .flush_interval(Duration::from_secs(60))
                .build();
            with_local_recorder(&recorder, || {
                metrics::counter!("no.span").increment(1);
            });
            recorder.flush();
        },
        test_options(),
    );
    let parsed = trace_metric_envelopes(envs);
    assert_eq!(parsed.len(), 1);
    let item = &parsed[0].2["items"][0];
    let obj = item.as_object().unwrap();
    assert!(
        !obj.contains_key("trace_id"),
        "trace_id should be omitted when no active span"
    );
    assert!(
        !obj.contains_key("span_id"),
        "span_id should be omitted when no active span"
    );
}

#[test]
fn active_span_attaches_trace_and_span_ids() {
    ensure_tracing_init();
    // For start_transaction to actually sample, the test client needs
    // traces_sample_rate > 0. The test transport accepts the envelopes
    // either way, but our recorder reads the trace_id/span_id from
    // configure_scope/get_span — set_span() makes it visible regardless of
    // sampling.
    let mut opts = test_options();
    opts.traces_sample_rate = 1.0;

    let envs = with_captured_envelopes_options(
        || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _g = rt.enter();

            let recorder = SentryRecorder::builder()
                .flush_interval(Duration::from_secs(60))
                .build();

            let txn =
                sentry::start_transaction(sentry::TransactionContext::new("test-op", "test-tx"));
            sentry::configure_scope(|scope| {
                scope.set_span(Some(txn.clone().into()));
            });

            with_local_recorder(&recorder, || {
                metrics::counter!("with.span").increment(1);
            });
            recorder.flush();

            // Tear down the active span before leaving so other tests in the
            // same process don't see it.
            sentry::configure_scope(|scope| scope.set_span(None));
            txn.finish();
        },
        opts,
    );

    let parsed = trace_metric_envelopes(envs);
    // We may also see a Transaction envelope in `envs`; trace_metric_envelopes
    // filters those out. Find the trace_metric envelope with our metric.
    let metric_envs: Vec<_> = parsed
        .iter()
        .filter(|(_, _, p)| {
            p["items"]
                .as_array()
                .map(|a| a.iter().any(|i| i["name"] == "with.span"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        metric_envs.len(),
        1,
        "expected one matching trace_metric envelope"
    );
    let item = metric_envs[0].2["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "with.span")
        .unwrap();

    let trace_id = item["trace_id"].as_str().expect("trace_id present");
    let span_id = item["span_id"].as_str().expect("span_id present");
    assert_eq!(trace_id.len(), 32, "trace_id should be 32 hex chars");
    assert_eq!(span_id.len(), 16, "span_id should be 16 hex chars");
    assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn graceful_when_sentry_hub_uninitialized() {
    // No sentry::init / no test transport. Build a recorder, emit metrics,
    // flush, and assert nothing panics. The recorder simply drops the
    // envelopes when no client is bound.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _g = rt.enter();
    let recorder = SentryRecorder::new();
    with_local_recorder(&recorder, || {
        metrics::counter!("nope").increment(1);
        metrics::gauge!("nope").set(1.0);
        metrics::histogram!("nope").record(1.0);
    });
    recorder.flush();
}
