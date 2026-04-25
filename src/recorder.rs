//! [`metrics::Recorder`] implementation that buffers and ships
//! `trace_metric` envelopes to Sentry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use serde_json::Value as JsonValue;

use crate::buffer::{
    Attribute, AttributeType, MetricBuffer, MetricItem, MetricKind, MetricValue, now_epoch_secs,
};
use crate::envelope::build_envelope;
use crate::runtime::{FlushTaskHandle, spawn_flush_task};

/// Default maximum buffered items before a size-triggered flush.
pub const DEFAULT_MAX_BUFFER_SIZE: usize = 100;
/// Default time between forced flushes.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// Default unit applied to metrics that don't declare one.
pub const DEFAULT_UNIT: &str = "none";

/// A [`Recorder`] that ships metrics to Sentry's `trace_metric` endpoint.
///
/// See the [crate root](crate) for an overview, runtime requirements, and
/// type-mapping decisions.
#[derive(Clone)]
pub struct SentryRecorder {
    inner: Arc<RecorderInner>,
}

struct RecorderInner {
    buffer: Arc<MetricBuffer>,
    max_buffer_size: usize,
    default_unit: &'static str,
    /// Per-metric description map (KeyName -> Unit). Populated by
    /// `describe_*` calls so we can attach the right wire-unit string at
    /// emit time. We use `Mutex` over `RwLock` because describes are rare.
    descriptions: Arc<Mutex<HashMap<String, Unit>>>,
    /// Set of metric names for which we've already emitted a one-shot
    /// "unsupported op" warning. Keeps the log from getting flooded.
    warned_keys: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Background flush task handle. Dropped on `Drop` of the last clone of
    /// the recorder. Wrapped in `Option` so `Drop` can take ownership.
    flush_task: Mutex<Option<FlushTaskHandle>>,
    /// Set to true on `Drop` so handles created earlier short-circuit.
    shutdown: AtomicBool,
}

impl std::fmt::Debug for SentryRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SentryRecorder")
            .field("max_buffer_size", &self.inner.max_buffer_size)
            .field("default_unit", &self.inner.default_unit)
            .finish_non_exhaustive()
    }
}

impl Default for SentryRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl SentryRecorder {
    /// Build a recorder with all defaults
    /// (`max_buffer_size = 100`, `flush_interval = 10s`, `default_unit = "none"`).
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Start a builder for configuring buffer size, flush cadence, and unit
    /// fallback.
    pub fn builder() -> SentryRecorderBuilder {
        SentryRecorderBuilder::default()
    }

    /// Manually flush the buffer. Public for tests; production code can rely
    /// on the size and time triggers.
    pub fn flush(&self) {
        flush_buffer(&self.inner);
    }

    fn record(&self, kind: MetricKind, key: &Key, value: MetricValue) {
        if self.inner.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let (trace_id, span_id) = current_trace_and_span_ids();
        let unit = unit_for(&self.inner, key.name());
        let attributes = labels_to_attributes(key);
        let item = MetricItem {
            timestamp: now_epoch_secs(),
            trace_id,
            span_id,
            name: key.name().to_string(),
            value,
            unit,
            kind,
            attributes,
        };
        let new_len = self.inner.buffer.push(item);
        if new_len >= self.inner.max_buffer_size {
            flush_buffer(&self.inner);
        }
    }

    fn warn_once(&self, name: &str, msg: &str) {
        let mut g = self
            .inner
            .warned_keys
            .lock()
            .expect("warned_keys mutex poisoned");
        if g.insert(name.to_string()) {
            tracing::warn!(metric = %name, "{}", msg);
        }
    }
}

impl Drop for RecorderInner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Best-effort synchronous final flush.
        let drained = self.buffer.drain();
        if !drained.is_empty()
            && let Some(env) = build_envelope(&drained)
            && let Some(client) = sentry::Hub::current().client()
        {
            client.send_envelope(env);
        }
        // Stop the background task. Its Drop impl signals stop and (on
        // blocking) joins.
        if let Ok(mut g) = self.flush_task.lock() {
            g.take();
        }
    }
}

/// Builder for [`SentryRecorder`].
pub struct SentryRecorderBuilder {
    max_buffer_size: usize,
    flush_interval: Duration,
    default_unit: &'static str,
}

impl Default for SentryRecorderBuilder {
    fn default() -> Self {
        Self {
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            default_unit: DEFAULT_UNIT,
        }
    }
}

impl SentryRecorderBuilder {
    /// Maximum buffered items before a size-triggered flush. Default `100`.
    pub fn max_buffer_size(mut self, n: usize) -> Self {
        // 0 would mean "flush on every push", which defeats batching but is
        // valid; we accept it.
        self.max_buffer_size = n;
        self
    }

    /// Time between forced flushes. Default `10s`.
    pub fn flush_interval(mut self, d: Duration) -> Self {
        self.flush_interval = d;
        self
    }

    /// Unit string applied to metrics with no declared unit. Default `"none"`.
    pub fn default_unit(mut self, unit: &'static str) -> Self {
        self.default_unit = unit;
        self
    }

    /// Build the recorder and spawn its background flush task.
    pub fn build(self) -> SentryRecorder {
        let buffer = Arc::new(MetricBuffer::new());
        let inner = Arc::new(RecorderInner {
            buffer: buffer.clone(),
            max_buffer_size: self.max_buffer_size,
            default_unit: self.default_unit,
            descriptions: Arc::new(Mutex::new(HashMap::new())),
            warned_keys: Arc::new(Mutex::new(std::collections::HashSet::new())),
            flush_task: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        });

        let inner_for_task = Arc::downgrade(&inner);
        let task = spawn_flush_task(self.flush_interval, move || {
            if let Some(inner) = inner_for_task.upgrade() {
                flush_buffer(&inner);
            }
        });
        if let Ok(mut g) = inner.flush_task.lock() {
            *g = Some(task);
        }

        SentryRecorder { inner }
    }
}

// ---------------------------------------------------------------------------
// Recorder trait impl
// ---------------------------------------------------------------------------

impl Recorder for SentryRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, _description: SharedString) {
        if let Some(u) = unit {
            self.inner
                .descriptions
                .lock()
                .expect("descriptions mutex poisoned")
                .insert(key.as_str().to_string(), u);
        }
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, _description: SharedString) {
        if let Some(u) = unit {
            self.inner
                .descriptions
                .lock()
                .expect("descriptions mutex poisoned")
                .insert(key.as_str().to_string(), u);
        }
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, _description: SharedString) {
        if let Some(u) = unit {
            self.inner
                .descriptions
                .lock()
                .expect("descriptions mutex poisoned")
                .insert(key.as_str().to_string(), u);
        }
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(SentryCounter {
            recorder: self.clone(),
            key: key.clone(),
        }))
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(Arc::new(SentryGauge {
            recorder: self.clone(),
            key: key.clone(),
        }))
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(Arc::new(SentryHistogram {
            recorder: self.clone(),
            key: key.clone(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Per-handle structs implementing CounterFn / GaugeFn / HistogramFn
// ---------------------------------------------------------------------------

struct SentryCounter {
    recorder: SentryRecorder,
    key: Key,
}

impl CounterFn for SentryCounter {
    fn increment(&self, value: u64) {
        self.recorder
            .record(MetricKind::Counter, &self.key, MetricValue::Counter(value));
    }

    fn absolute(&self, _value: u64) {
        // Sentry's `counter` wire value is a delta (verified against the spec
        // and asserted in unit tests). Cumulative writes can't be expressed
        // without per-handle state we don't have, so we drop them and warn
        // once.
        self.recorder.warn_once(
            self.key.name(),
            "metrics-exporter-sentry-v2: Counter::absolute is not supported by Sentry's \
             trace_metric protocol (counter values are deltas); dropping",
        );
    }
}

struct SentryGauge {
    recorder: SentryRecorder,
    key: Key,
}

impl GaugeFn for SentryGauge {
    fn increment(&self, _value: f64) {
        // Sentry's `gauge` wire value is absolute; gauge-deltas are not on
        // the wire (verified against the spec). Drop with a one-shot warn.
        self.recorder.warn_once(
            self.key.name(),
            "metrics-exporter-sentry-v2: Gauge::increment is not supported by Sentry's \
             trace_metric protocol (gauge values are absolute); dropping",
        );
    }

    fn decrement(&self, _value: f64) {
        self.recorder.warn_once(
            self.key.name(),
            "metrics-exporter-sentry-v2: Gauge::decrement is not supported by Sentry's \
             trace_metric protocol (gauge values are absolute); dropping",
        );
    }

    fn set(&self, value: f64) {
        self.recorder
            .record(MetricKind::Gauge, &self.key, MetricValue::Float(value));
    }
}

struct SentryHistogram {
    recorder: SentryRecorder,
    key: Key,
}

impl HistogramFn for SentryHistogram {
    fn record(&self, value: f64) {
        self.recorder.record(
            MetricKind::Distribution,
            &self.key,
            MetricValue::Float(value),
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn flush_buffer(inner: &RecorderInner) {
    let drained = inner.buffer.drain();
    if drained.is_empty() {
        return;
    }
    let Some(env) = build_envelope(&drained) else {
        return;
    };
    let Some(client) = sentry::Hub::current().client() else {
        // No Sentry client bound; drop the batch silently. metrics-rs users
        // commonly install a recorder before sentry::init in tests; warning
        // here would be noisy.
        return;
    };
    // send_envelope is infallible (transport handles retries / errors
    // internally); no panic risk.
    client.send_envelope(env);
}

fn unit_for(inner: &RecorderInner, name: &str) -> String {
    if let Some(u) = inner
        .descriptions
        .lock()
        .expect("descriptions mutex poisoned")
        .get(name)
        .copied()
    {
        return map_unit(u).to_string();
    }
    inner.default_unit.to_string()
}

/// Map a `metrics::Unit` to Sentry's unit-string. Sentry's documented set
/// includes second/millisecond/microsecond/nanosecond, byte/kilobyte/etc.,
/// and ratio/percent — we use the singular forms most consistent with their
/// examples.
fn map_unit(u: Unit) -> &'static str {
    match u {
        Unit::Count => "none",
        Unit::Percent => "percent",
        Unit::Seconds => "second",
        Unit::Milliseconds => "millisecond",
        Unit::Microseconds => "microsecond",
        Unit::Nanoseconds => "nanosecond",
        Unit::Tebibytes => "tebibyte",
        Unit::Gibibytes => "gibibyte",
        Unit::Mebibytes => "mebibyte",
        Unit::Kibibytes => "kibibyte",
        Unit::Bytes => "byte",
        Unit::TerabitsPerSecond => "terabit_per_second",
        Unit::GigabitsPerSecond => "gigabit_per_second",
        Unit::MegabitsPerSecond => "megabit_per_second",
        Unit::KilobitsPerSecond => "kilobit_per_second",
        Unit::BitsPerSecond => "bit_per_second",
        Unit::CountPerSecond => "count_per_second",
    }
}

fn labels_to_attributes(key: &Key) -> Vec<Attribute> {
    key.labels()
        .map(|l| Attribute {
            key: l.key().to_string(),
            value: JsonValue::from(l.value().to_string()),
            ty: AttributeType::String,
        })
        .collect()
}

/// Read the active Sentry span's `(trace_id, span_id)` if one exists.
///
/// We use [`sentry::configure_scope`] (publicly exported by the `sentry`
/// crate) which takes `&mut Scope`, then call `Scope::get_span()` to read
/// the currently active `TransactionOrSpan`.
fn current_trace_and_span_ids() -> (Option<String>, Option<String>) {
    let mut tid: Option<String> = None;
    let mut sid: Option<String> = None;
    sentry::configure_scope(|scope| {
        if let Some(span) = scope.get_span() {
            // TransactionOrSpan has get_trace_context() which returns a
            // protocol::TraceContext { trace_id, span_id, .. }. Both ids'
            // Display impls produce lowercase hex strings of the right
            // lengths (32 / 16).
            let ctx = span.get_trace_context();
            tid = Some(ctx.trace_id.to_string());
            sid = Some(ctx.span_id.to_string());
        }
    });
    (tid, sid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_unit_canonical_strings() {
        assert_eq!(map_unit(Unit::Milliseconds), "millisecond");
        assert_eq!(map_unit(Unit::Bytes), "byte");
        assert_eq!(map_unit(Unit::Count), "none");
        assert_eq!(map_unit(Unit::Percent), "percent");
    }

    #[test]
    fn builder_round_trip_defaults() {
        // Build inside an explicit current-thread runtime so the tokio task
        // can attach. We don't actually need a Sentry client.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        let r = SentryRecorder::builder().build();
        assert_eq!(r.inner.max_buffer_size, DEFAULT_MAX_BUFFER_SIZE);
        assert_eq!(r.inner.default_unit, DEFAULT_UNIT);
    }

    #[test]
    fn no_panic_when_sentry_uninitialized() {
        // No sentry::init, no tokio runtime. Recording should be a no-op
        // and absolutely must not panic.
        let r = SentryRecorder::new();
        let key = Key::from_name("x");
        let md = metrics::Metadata::new(module_path!(), metrics::Level::INFO, None);
        let c = r.register_counter(&key, &md);
        c.increment(1);
        let g = r.register_gauge(&key, &md);
        g.set(2.0);
        let h = r.register_histogram(&key, &md);
        h.record(3.0);
        // Manual flush — no client bound, no-op.
        r.flush();
    }
}
