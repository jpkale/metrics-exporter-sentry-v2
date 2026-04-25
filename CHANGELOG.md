# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-04-25

### Added
- Initial release: a `metrics`-crate `Recorder` that ships counters, gauges,
  and histograms to Sentry's new trace-connected metrics product via
  `trace_metric` envelope items.
- `SentryRecorder` and `SentryRecorderBuilder` public API.
- Buffered emission with size-based and time-based flush triggers.
- Trace correlation: every emitted metric carries `trace_id` and `span_id`
  from the active Sentry hub's currently active span when one exists.
- Cargo features: `tokio` (default) for the async runtime variant of the
  flush task, `blocking` for a dedicated OS-thread variant.
- Dual MIT / Apache-2.0 license.

[0.1.0]: https://github.com/jpkale/metrics-exporter-sentry-v2/releases/tag/v0.1.0
