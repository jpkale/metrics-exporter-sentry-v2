//! # metrics-exporter-sentry-v2
//!
//! A [`metrics`]-crate [`Recorder`] that ships metrics to Sentry's new
//! **trace-connected metrics** product via `trace_metric` envelope items.
//!
//! ## Why this crate exists
//!
//! Sentry deprecated their original metrics product on 2024-10-07 and re-launched
//! a new metrics product in open beta in 2026 that uses the [`trace_metric`
//! envelope spec]. Each metric is correlated to a Sentry trace via `trace_id`
//! and `span_id` fields read from the active Sentry hub.
//!
//! The official Rust SDK ([`getsentry/sentry-rust#938`]) has explicitly closed
//! native metrics support as not-planned. This crate fills the gap by piggy-backing
//! on the existing `sentry::Client` transport.
//!
//! [`trace_metric` envelope spec]: https://develop.sentry.dev/sdk/telemetry/metrics/
//! [`getsentry/sentry-rust#938`]: https://github.com/getsentry/sentry-rust/issues/938
//! [`Recorder`]: metrics::Recorder
//!
//! ## Quick start
//!
//! ```no_run
//! use metrics_exporter_sentry_v2::SentryRecorder;
//!
//! let _sentry = sentry::init(("__DSN__", sentry::ClientOptions::default()));
//! metrics::set_global_recorder(SentryRecorder::new()).unwrap();
//!
//! metrics::counter!("api.requests", "route" => "/health").increment(1);
//! ```
//!
//! ## Runtime requirement
//!
//! With the default `tokio` feature, calling
//! [`set_global_recorder`](metrics::set_global_recorder) MUST happen from
//! within a Tokio runtime context (multi- or current-thread); the time-based
//! flush task is spawned via `tokio::spawn`. If no runtime is active the
//! recorder will still work — size-based flushes still happen — but
//! time-based flushing degrades to "next push or `Drop`".
//!
//! Enable the `blocking` feature (and disable default features) to use a
//! dedicated OS thread instead.
//!
//! ## Wire-format choices and rationale
//!
//! Per the [Sentry spec][`trace_metric` envelope spec]:
//!
//! * `counter` `value` is sent as a **delta** (the increment), not a cumulative
//!   total. [`metrics::Counter::absolute`] cannot be expressed and is dropped
//!   with a one-shot warning.
//! * `gauge` `value` is sent as an **absolute value**. Wire-format gauge deltas
//!   are not supported; [`metrics::Gauge::increment`] / [`metrics::Gauge::decrement`]
//!   are dropped with a one-shot warning.
//! * Attributes are sent as the typed wrapper shape
//!   `{"value": ..., "type": "string"|"integer"|"double"|"boolean"}`.
//!
//! ## Comparison to `metrics-exporter-sentry`
//!
//! The crate `metrics-exporter-sentry` (v0.1.0, abandoned) targets the
//! **deprecated** Sentry metrics endpoint and is pinned to `metrics 0.22` /
//! `sentry 0.32`. This crate targets the **new** `trace_metric` endpoint with
//! current dependencies.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

mod buffer;
mod envelope;
mod recorder;
mod runtime;

pub use recorder::{SentryRecorder, SentryRecorderBuilder};

// Internal items exposed for integration tests only.
#[doc(hidden)]
pub mod __test_internals {
    //! Test-only re-exports. Not part of the public API.
    pub use crate::buffer::{MetricItem, MetricKind, MetricValue, build_payload_json};
    pub use crate::envelope::build_envelope_bytes;
}
