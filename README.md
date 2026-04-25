# metrics-exporter-sentry-v2

A [`metrics`](https://crates.io/crates/metrics) `Recorder` that ships metrics to
**Sentry's new trace-connected metrics product** via `trace_metric` envelope
items.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## Motivation

Sentry deprecated their original metrics product on 2024-10-07 and re-launched
a new metrics product in open beta in 2026:

- Deprecation notice: <https://sentry.zendesk.com/hc/en-us/articles/26369339769883>
- New product docs: <https://docs.sentry.io/product/explore/metrics/>
- Wire-format spec: <https://develop.sentry.dev/sdk/telemetry/metrics/>

The new product transports metrics as `trace_metric` envelope items where every
metric carries a `trace_id` and `span_id` for trace correlation. The official
Rust SDK has explicitly closed metrics support as not-planned
([getsentry/sentry-rust#938](https://github.com/getsentry/sentry-rust/issues/938)),
so this crate fills the gap by piggy-backing on `sentry::Client`'s existing
transport.

## Install

```toml
[dependencies]
metrics = "0.24"
metrics-exporter-sentry-v2 = "0.1"
sentry = "0.47"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## Basic usage

```rust
use metrics_exporter_sentry_v2::SentryRecorder;

#[tokio::main]
async fn main() {
    let _sentry = sentry::init(("https://your-dsn@sentry.io/123", sentry::ClientOptions {
        traces_sample_rate: 1.0,
        ..Default::default()
    }));

    metrics::set_global_recorder(SentryRecorder::new()).expect("set recorder");

    metrics::counter!("api.requests", "route" => "/health").increment(1);
    metrics::gauge!("queue.depth").set(42.0);
    metrics::histogram!("request.duration_ms").record(120.0);
}
```

## Runtime requirement

With the default `tokio` feature, install the recorder from inside a Tokio
runtime context (any flavor — current-thread, multi-thread, etc.). The
recorder spawns a background `tokio::task` that performs time-based flushes.

If no runtime is active, the recorder still works — size-based flushes still
trigger and `Drop` performs a final synchronous flush — but periodic
time-based flushing is disabled and the recorder logs a one-shot warning.

To use a dedicated OS thread instead of a tokio task, disable defaults and
enable the `blocking` feature:

```toml
metrics-exporter-sentry-v2 = { version = "0.1", default-features = false, features = ["blocking"] }
```

## Configuration options

```rust
use std::time::Duration;
use metrics_exporter_sentry_v2::SentryRecorder;

let recorder = SentryRecorder::builder()
    .max_buffer_size(500)                   // size-based flush trigger; default 100
    .flush_interval(Duration::from_secs(5)) // time-based flush trigger; default 10s
    .default_unit("none")                   // unit when none declared; default "none"
    .build();
```

| Option            | Default | Behavior                                                                      |
| ----------------- | ------- | ----------------------------------------------------------------------------- |
| `max_buffer_size` | 100     | Flush as soon as the buffer reaches this many queued items.                   |
| `flush_interval`  | 10s     | Force-flush every interval (whether or not the buffer is full).               |
| `default_unit`    | `none`  | Unit string applied to metrics that don't declare a `metrics::Unit`.          |

## Type mapping

| `metrics-rs` operation        | Sentry `trace_metric` shape                                  |
| ----------------------------- | ------------------------------------------------------------ |
| `Counter::increment(n)`       | `{"type": "counter", "value": n}` — value is a delta          |
| `Counter::absolute(n)`        | dropped (one-shot warn)  — Sentry counters don't take cumulative |
| `Gauge::set(v)`               | `{"type": "gauge", "value": v}` — absolute                    |
| `Gauge::increment / decrement`| dropped (one-shot warn) — Sentry gauges don't take deltas     |
| `Histogram::record(v)`        | `{"type": "distribution", "value": v}`                        |

`metrics-rs` labels become typed Sentry attributes
(`{"value": ..., "type": "string"|"integer"|"double"|"boolean"}`). Labels
arrive from `metrics-rs` as strings and are sent as `string`-typed attributes.

## Trace correlation

Every emitted metric reads the active Sentry span via
`sentry::configure_scope(|s| s.get_span())`. If a span is active, the metric
carries its `trace_id` (32 hex chars) and `span_id` (16 hex chars). If not,
both fields are omitted as the spec allows.

## Comparison to `metrics-exporter-sentry`

[`metrics-exporter-sentry`](https://crates.io/crates/metrics-exporter-sentry)
(v0.1.0, abandoned) targets the **deprecated** Sentry metrics endpoint and is
pinned to `metrics 0.22` / `sentry 0.32`. **Don't use it for new work.** This
crate targets the **new** `trace_metric` endpoint with current dependencies
(`metrics 0.24`, `sentry 0.47`).

## FAQ

**Q: Why the `-v2` suffix?**
The existing [`metrics-exporter-sentry`](https://crates.io/crates/metrics-exporter-sentry)
is at `0.1.0` on crates.io and abandoned — pinned to `metrics 0.22` / `sentry 0.32`,
targeting the **deprecated** Sentry metrics endpoint. The `-v2` suffix marks this
crate as the spiritual successor: same metrics-rs ecosystem position, but pointing
at the **new** `trace_metric` endpoint with current dependencies.

**Q: How do I get `trace_id`/`span_id` populated on my metrics?**
Make sure a Sentry transaction or span is active when the metric is emitted.
Either start one explicitly with `sentry::start_transaction(...)` and
`scope.set_span(...)`, or let the `sentry-tracing` integration push spans for
you.

**Q: Does this crate buffer or pre-aggregate?**
It buffers (size + time triggered) but does **not** pre-aggregate. Sentry
computes histogram quantiles server-side from the distribution stream.

**Q: What happens if Sentry transport fails?**
`sentry::Client::send_envelope` is infallible at the API level — the transport
handles retries / errors internally. Envelope-assembly failures inside this
crate (none are currently expected) would be logged via `tracing::warn!` and
the batch dropped. No panics.

**Q: Does it work with `sentry-tracing`?**
Yes. `sentry-tracing` pushes spans onto the scope; this recorder reads from the
same scope, so metrics emitted inside an instrumented function get correlated
automatically.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual-licensed as above, without any additional terms or conditions.
