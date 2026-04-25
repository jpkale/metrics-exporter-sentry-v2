//! Envelope assembly. Sentry's current Rust SDK does not expose a generic
//! "raw item with custom content-type" variant on `EnvelopeItem`, so we
//! hand-build the envelope wire bytes (header line + item header + payload)
//! and ship them via [`Envelope::from_bytes_raw`].

use sentry::Envelope;
use serde_json::json;

use crate::buffer::{MetricItem, build_payload_json};

/// Sentry's envelope-item type string for the new metrics product.
pub const TRACE_METRIC_ITEM_TYPE: &str = "trace_metric";

/// Sentry's envelope-item content-type for the new metrics product.
pub const TRACE_METRIC_CONTENT_TYPE: &str = "application/vnd.sentry.items.trace-metric+json";

/// Build the raw envelope bytes for a batch of metric items.
///
/// Layout (one line per JSON object, separated by `\n`):
///
/// ```text
/// {"event_id":"…"}\n                       <- envelope headers (empty object also valid)
/// {"type":"trace_metric","content_type":"…","length":N}\n   <- item headers
/// <N bytes of payload JSON>\n              <- item payload
/// ```
pub fn build_envelope_bytes(items: &[MetricItem]) -> Vec<u8> {
    let payload = build_payload_json(items);

    // Envelope-level headers. The spec allows an empty object; including an
    // event_id is optional and only makes sense for events. We omit it.
    let envelope_headers = serde_json::to_vec(&json!({})).expect("valid JSON");

    // Per-item headers. `length` MUST be byte-length of the payload that
    // follows. `content_type` carries the trace-metric MIME type.
    let item_headers = serde_json::to_vec(&json!({
        "type": TRACE_METRIC_ITEM_TYPE,
        "content_type": TRACE_METRIC_CONTENT_TYPE,
        "length": payload.len(),
    }))
    .expect("valid JSON");

    let mut out =
        Vec::with_capacity(envelope_headers.len() + item_headers.len() + payload.len() + 3);
    out.extend_from_slice(&envelope_headers);
    out.push(b'\n');
    out.extend_from_slice(&item_headers);
    out.push(b'\n');
    out.extend_from_slice(&payload);
    out.push(b'\n');
    out
}

/// Build an [`Envelope`] from a batch of metric items, suitable for
/// [`sentry::Client::send_envelope`].
pub fn build_envelope(items: &[MetricItem]) -> Option<Envelope> {
    if items.is_empty() {
        return None;
    }
    let bytes = build_envelope_bytes(items);
    match Envelope::from_bytes_raw(bytes) {
        Ok(env) => Some(env),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "metrics-exporter-sentry-v2: failed to assemble trace_metric envelope; dropping batch"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{MetricKind, MetricValue};
    use serde_json::Value;

    fn one_item() -> MetricItem {
        MetricItem {
            timestamp: 1_700_000_000.0,
            trace_id: None,
            span_id: None,
            name: "demo".to_string(),
            value: MetricValue::Counter(1),
            unit: "none".to_string(),
            kind: MetricKind::Counter,
            attributes: vec![],
        }
    }

    #[test]
    fn envelope_layout_has_three_lines() {
        let bytes = build_envelope_bytes(&[one_item()]);
        // Trim trailing newline before splitting so we don't see a phantom 4th.
        let trimmed = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let lines: Vec<&[u8]> = trimmed.split(|&b| b == b'\n').collect();
        assert_eq!(
            lines.len(),
            3,
            "expected env header + item header + payload"
        );
    }

    #[test]
    fn item_header_declares_correct_type_and_content_type() {
        let bytes = build_envelope_bytes(&[one_item()]);
        let mut iter = bytes.split(|&b| b == b'\n');
        let _env = iter.next().unwrap();
        let item_header = iter.next().unwrap();
        let parsed: Value = serde_json::from_slice(item_header).unwrap();
        assert_eq!(parsed["type"], "trace_metric");
        assert_eq!(
            parsed["content_type"],
            "application/vnd.sentry.items.trace-metric+json"
        );
        assert!(parsed["length"].as_u64().unwrap() > 0);
    }

    #[test]
    fn item_header_length_matches_payload() {
        let bytes = build_envelope_bytes(&[one_item()]);
        let mut iter = bytes.split(|&b| b == b'\n');
        let _env = iter.next().unwrap();
        let item_header = iter.next().unwrap();
        let payload = iter.next().unwrap();
        let parsed: Value = serde_json::from_slice(item_header).unwrap();
        assert_eq!(parsed["length"].as_u64().unwrap() as usize, payload.len());
    }

    #[test]
    fn empty_input_yields_no_envelope() {
        assert!(build_envelope(&[]).is_none());
    }
}
