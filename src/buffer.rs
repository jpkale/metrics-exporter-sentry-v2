//! In-memory buffer for metric items pending flush, plus the canonical
//! JSON-payload builder used for both wire-format emission and tests.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// One of the three Sentry trace-metric type values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// `"counter"` — value is a **delta** (the increment amount).
    Counter,
    /// `"gauge"` — value is an **absolute** reading.
    Gauge,
    /// `"distribution"` — a single observation in a distribution stream.
    Distribution,
}

impl MetricKind {
    /// The exact wire string Sentry expects in the `type` field.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Distribution => "distribution",
        }
    }
}

/// The numeric `value` field. The spec accepts 64-bit signed int *or* 64-bit
/// float; we always serialize as a JSON number and let serde_json choose the
/// shortest representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricValue {
    /// Counter increments arrive as `u64`; promoted into a JSON number.
    Counter(u64),
    /// Gauges and distributions arrive as `f64`.
    Float(f64),
}

impl MetricValue {
    fn to_json(self) -> Value {
        match self {
            MetricValue::Counter(n) => Value::from(n),
            MetricValue::Float(f) => {
                // serde_json::Number::from_f64 returns None for NaN / inf; we
                // fall back to 0.0 in that case to avoid emitting an invalid
                // payload. NaN/inf in a metric stream is almost always a bug;
                // we drop quietly and let upstream notice the missing data.
                Value::from(serde_json::Number::from_f64(f).unwrap_or_else(|| {
                    serde_json::Number::from_f64(0.0).expect("0.0 is a valid finite f64")
                }))
            }
        }
    }
}

/// One typed attribute, matching Sentry's
/// `{"value": ..., "type": "string"|"integer"|"double"|"boolean"}` shape.
#[derive(Debug, Clone)]
pub struct Attribute {
    /// Attribute name (the key in the outer `attributes` object).
    pub key: String,
    /// JSON-encoded value.
    pub value: Value,
    /// Sentry's typed-attribute discriminator.
    pub ty: AttributeType,
}

/// Sentry's typed-attribute discriminator values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeType {
    /// `"string"`
    String,
    /// `"integer"`
    Integer,
    /// `"double"`
    Double,
    /// `"boolean"`
    Boolean,
}

impl AttributeType {
    fn as_wire_str(self) -> &'static str {
        match self {
            AttributeType::String => "string",
            AttributeType::Integer => "integer",
            AttributeType::Double => "double",
            AttributeType::Boolean => "boolean",
        }
    }
}

/// One queued metric ready for flushing.
#[derive(Debug, Clone)]
pub struct MetricItem {
    /// Seconds since epoch, fractional seconds preserved.
    pub timestamp: f64,
    /// Optional 32-char hex trace id from the active Sentry span/scope.
    pub trace_id: Option<String>,
    /// Optional 16-char hex span id from the active Sentry span/scope.
    pub span_id: Option<String>,
    /// Hierarchical metric name (e.g. `"api.requests"`).
    pub name: String,
    /// Metric value (delta for counter, absolute for gauge/distribution).
    pub value: MetricValue,
    /// Sentry unit string (e.g. `"millisecond"`, `"byte"`, `"none"`).
    pub unit: String,
    /// Counter / gauge / distribution.
    pub kind: MetricKind,
    /// Typed attributes (metrics-rs labels).
    pub attributes: Vec<Attribute>,
}

impl MetricItem {
    /// Convert into the per-item JSON object the Sentry spec mandates.
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("timestamp".to_string(), Value::from(self.timestamp));
        if let Some(tid) = &self.trace_id {
            obj.insert("trace_id".to_string(), Value::from(tid.clone()));
        }
        if let Some(sid) = &self.span_id {
            obj.insert("span_id".to_string(), Value::from(sid.clone()));
        }
        obj.insert("name".to_string(), Value::from(self.name.clone()));
        obj.insert("value".to_string(), self.value.to_json());
        obj.insert(
            "type".to_string(),
            Value::from(self.kind.as_wire_str().to_string()),
        );
        obj.insert("unit".to_string(), Value::from(self.unit.clone()));

        if !self.attributes.is_empty() {
            let mut attrs = Map::new();
            for a in &self.attributes {
                attrs.insert(
                    a.key.clone(),
                    json!({ "value": a.value, "type": a.ty.as_wire_str() }),
                );
            }
            obj.insert("attributes".to_string(), Value::Object(attrs));
        }

        Value::Object(obj)
    }
}

/// Build the full top-level `{"version": 2, "items": [...]}` JSON payload
/// shipped inside the `trace_metric` envelope item.
pub fn build_payload_json(items: &[MetricItem]) -> Vec<u8> {
    let payload = json!({
        "version": 2,
        "items": items.iter().map(MetricItem::to_json).collect::<Vec<_>>(),
    });
    serde_json::to_vec(&payload).expect("valid JSON")
}

/// Helper for tests / the recorder itself: current epoch seconds as f64.
pub fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A [`Mutex`]-guarded `Vec<MetricItem>`. Recorder operations push, flush
/// drains.
#[derive(Debug, Default)]
pub struct MetricBuffer {
    inner: Mutex<Vec<MetricItem>>,
}

impl MetricBuffer {
    /// Create a new empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one item. Returns the new length.
    pub fn push(&self, item: MetricItem) -> usize {
        let mut g = self.inner.lock().expect("metric buffer mutex poisoned");
        g.push(item);
        g.len()
    }

    /// Drain everything currently queued.
    pub fn drain(&self) -> Vec<MetricItem> {
        let mut g = self.inner.lock().expect("metric buffer mutex poisoned");
        std::mem::take(&mut *g)
    }

    /// Current queued length (for tests).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("metric buffer mutex poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, kind: MetricKind, value: MetricValue) -> MetricItem {
        MetricItem {
            timestamp: 1_700_000_000.0,
            trace_id: None,
            span_id: None,
            name: name.to_string(),
            value,
            unit: "none".to_string(),
            kind,
            attributes: vec![],
        }
    }

    #[test]
    fn counter_serializes_with_delta_value() {
        // The Sentry spec says counter `value` is a delta. We assert it in
        // the wire-format here so a future refactor can't silently flip it.
        let it = item("api.requests", MetricKind::Counter, MetricValue::Counter(7));
        let v = it.to_json();
        assert_eq!(v["type"], "counter");
        assert_eq!(v["value"], 7);
        assert_eq!(v["name"], "api.requests");
        assert_eq!(v["unit"], "none");
    }

    #[test]
    fn gauge_serializes_as_absolute_float() {
        let it = item("queue.depth", MetricKind::Gauge, MetricValue::Float(42.5));
        let v = it.to_json();
        assert_eq!(v["type"], "gauge");
        assert_eq!(v["value"], 42.5);
    }

    #[test]
    fn distribution_serializes_as_float() {
        let it = item(
            "request.duration",
            MetricKind::Distribution,
            MetricValue::Float(123.0),
        );
        let v = it.to_json();
        assert_eq!(v["type"], "distribution");
        assert_eq!(v["value"], 123.0);
    }

    #[test]
    fn typed_attributes_use_value_type_wrapper() {
        let it = MetricItem {
            attributes: vec![
                Attribute {
                    key: "route".to_string(),
                    value: Value::from("/health"),
                    ty: AttributeType::String,
                },
                Attribute {
                    key: "status_code".to_string(),
                    value: Value::from(201_i64),
                    ty: AttributeType::Integer,
                },
                Attribute {
                    key: "cache_hit_rate".to_string(),
                    value: Value::from(0.95),
                    ty: AttributeType::Double,
                },
                Attribute {
                    key: "enabled".to_string(),
                    value: Value::from(true),
                    ty: AttributeType::Boolean,
                },
            ],
            ..item("x", MetricKind::Counter, MetricValue::Counter(1))
        };
        let v = it.to_json();
        let attrs = &v["attributes"];
        assert_eq!(attrs["route"]["value"], "/health");
        assert_eq!(attrs["route"]["type"], "string");
        assert_eq!(attrs["status_code"]["value"], 201);
        assert_eq!(attrs["status_code"]["type"], "integer");
        assert_eq!(attrs["cache_hit_rate"]["value"], 0.95);
        assert_eq!(attrs["cache_hit_rate"]["type"], "double");
        assert_eq!(attrs["enabled"]["value"], true);
        assert_eq!(attrs["enabled"]["type"], "boolean");
    }

    #[test]
    fn payload_envelope_shape_is_version_2_and_items_array() {
        let items = vec![item("n", MetricKind::Counter, MetricValue::Counter(1))];
        let bytes = build_payload_json(&items);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["version"], 2);
        assert!(v["items"].is_array());
        assert_eq!(v["items"][0]["name"], "n");
    }

    #[test]
    fn trace_and_span_ids_round_trip() {
        let mut it = item("n", MetricKind::Counter, MetricValue::Counter(1));
        it.trace_id = Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
        it.span_id = Some("bbbbbbbbbbbbbbbb".into());
        let v = it.to_json();
        assert_eq!(v["trace_id"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(v["span_id"], "bbbbbbbbbbbbbbbb");
    }

    #[test]
    fn trace_and_span_ids_omitted_when_absent() {
        let it = item("n", MetricKind::Counter, MetricValue::Counter(1));
        let v = it.to_json();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("trace_id"));
        assert!(!obj.contains_key("span_id"));
    }

    #[test]
    fn buffer_push_and_drain() {
        let b = MetricBuffer::new();
        assert_eq!(b.len(), 0);
        b.push(item("a", MetricKind::Counter, MetricValue::Counter(1)));
        b.push(item("b", MetricKind::Counter, MetricValue::Counter(2)));
        assert_eq!(b.len(), 2);
        let drained = b.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(b.len(), 0);
    }
}
