//! Minimal JSON-RPC envelope inspection.
//!
//! This phase only needs to *classify* a line (request / response / notification /
//! other) and extract the `id` and `method` so requests can be correlated to
//! responses by id. It deliberately does not model params, results, or errors —
//! passthrough forwards the original bytes unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// A JSON-RPC request id: per the spec, a string or a number (or null, which we
/// don't treat as correlatable). Floats are stored by their string form so the
/// type stays `Hash`/`Eq` for use as a map key; integer and string ids — the
/// only forms real MCP servers use — round-trip exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Str(String),
    Num(i64),
    /// Non-integer numeric id, preserved as its JSON text. Rare; kept for completeness.
    Other(String),
}

impl RequestId {
    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::String(s) => Some(RequestId::Str(s.clone())),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(RequestId::Num(i))
                } else {
                    Some(RequestId::Other(n.to_string()))
                }
            }
            _ => None,
        }
    }
}

/// Classification of a single JSON-RPC line. `Other` is the catch-all for
/// anything we don't act on, including non-JSON lines (which must still forward).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Request { id: RequestId, method: String },
    Response { id: RequestId },
    Notification { method: String },
    Other,
}

/// Classify an already-parsed JSON-RPC value. A non-object value (array, scalar)
/// is `Other`. Pure inspection — never allocates beyond the extracted strings.
///
/// The downstream flow parses each line to a `Value` exactly once (the tool-result
/// payloads are the largest blobs we handle), classifies it here, and reuses the
/// same owned value for the transform — so this operates on `&Value` rather than
/// re-parsing. `parse_line` is the string entry point for the tiny request lines.
pub fn classify(value: &Value) -> Message {
    let Some(obj) = value.as_object() else {
        return Message::Other;
    };

    let method = obj.get("method").and_then(Value::as_str);
    let id = obj.get("id").and_then(RequestId::from_value);

    match (method, id) {
        (Some(method), Some(id)) => Message::Request {
            id,
            method: method.to_string(),
        },
        (Some(method), None) => Message::Notification {
            method: method.to_string(),
        },
        (None, Some(id)) => Message::Response { id },
        (None, None) => Message::Other,
    }
}

/// Classify a single line. Never errors: a non-JSON or unexpected line is `Other`,
/// which the pump forwards unchanged (fail-safe passthrough). Thin wrapper over
/// [`classify`] — parses the line to a `Value`, then delegates.
pub fn parse_line(line: &str) -> Message {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Message::Other;
    };
    classify(&value)
}

/// Correlates request ids to their methods so the downstream flow can later ask
/// "is this response the result of a `tools/call`?". Built this phase; only
/// trace-logged for now — nothing acts on the correlation until the transform lands.
#[derive(Debug, Clone, Default)]
pub struct RequestTracker {
    inner: Arc<Mutex<HashMap<RequestId, String>>>,
}

impl RequestTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the method associated with an outgoing request id.
    pub fn record_request(&self, id: RequestId, method: String) {
        self.inner
            .lock()
            .expect("RequestTracker mutex poisoned")
            .insert(id, method);
    }

    /// Resolve and remove the method for a response id, if we recorded its request.
    pub fn take_method(&self, id: &RequestId) -> Option<String> {
        self.inner
            .lock()
            .expect("RequestTracker mutex poisoned")
            .remove(id)
    }

    /// Number of requests sent but not yet answered (recorded, not yet taken).
    /// Used by the HTTP driver to drain in-flight responses after the client's
    /// stdin closes, rather than abandoning them.
    pub fn pending_count(&self) -> usize {
        self.inner
            .lock()
            .expect("RequestTracker mutex poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_with_string_id() {
        let m = parse_line(r#"{"jsonrpc":"2.0","id":"abc","method":"tools/call","params":{}}"#);
        assert_eq!(
            m,
            Message::Request {
                id: RequestId::Str("abc".into()),
                method: "tools/call".into()
            }
        );
    }

    #[test]
    fn parses_request_with_numeric_id() {
        let m = parse_line(r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#);
        assert_eq!(
            m,
            Message::Request {
                id: RequestId::Num(7),
                method: "initialize".into()
            }
        );
    }

    #[test]
    fn parses_response() {
        let m = parse_line(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#);
        assert_eq!(
            m,
            Message::Response {
                id: RequestId::Num(7)
            }
        );
    }

    #[test]
    fn parses_notification() {
        let m = parse_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(
            m,
            Message::Notification {
                method: "notifications/initialized".into()
            }
        );
    }

    #[test]
    fn non_json_is_other() {
        assert_eq!(parse_line("this is not json"), Message::Other);
        assert_eq!(parse_line(""), Message::Other);
    }

    #[test]
    fn json_non_object_is_other() {
        assert_eq!(parse_line("[1,2,3]"), Message::Other);
        assert_eq!(parse_line("42"), Message::Other);
    }

    #[test]
    fn out_of_i64_range_id_is_a_stable_correlation_key() {
        // An id outside i64 range (here: past i64::MAX, which serde parses as f64)
        // falls to `RequestId::Other`. We do NOT promise byte-identity with the wire
        // form — serde may render it canonically (e.g. "1e+20"). What we DO promise,
        // and what correlation actually needs, is that the *same* wire id produces the
        // *same* key on the request and on its response, so take_method still matches.
        let wire = "100000000000000000000"; // > i64::MAX
        let req = parse_line(&format!(r#"{{"jsonrpc":"2.0","id":{wire},"method":"x"}}"#));
        let resp = parse_line(&format!(r#"{{"jsonrpc":"2.0","id":{wire},"result":{{}}}}"#));
        let (Message::Request { id: req_id, .. }, Message::Response { id: resp_id }) =
            (&req, &resp)
        else {
            panic!("expected Request + Response, got {req:?} / {resp:?}");
        };
        assert!(
            matches!(req_id, RequestId::Other(_)),
            "big id → Other, got {req_id:?}"
        );
        assert_eq!(
            req_id, resp_id,
            "same wire id must yield the same correlation key"
        );

        // And the key round-trips through the tracker like any other id.
        let t = RequestTracker::new();
        t.record_request(req_id.clone(), "x".into());
        assert_eq!(t.take_method(resp_id), Some("x".into()));
    }

    #[test]
    fn non_scalar_id_makes_it_unclassifiable() {
        // A structured (non-string/number) id can't be a JSON-RPC id; with a method
        // present but the id unusable, the result is Notification (method, no id) —
        // the id is simply not extracted, never panics.
        let m = parse_line(r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#);
        assert_eq!(m, Message::Notification { method: "x".into() });
    }

    #[test]
    fn object_with_neither_method_nor_id_is_other() {
        // Fail-safe passthrough: a JSON object that is neither request, response, nor
        // notification classifies as Other and is forwarded unchanged.
        assert_eq!(parse_line(r#"{"jsonrpc":"2.0","foo":1}"#), Message::Other);
    }

    #[test]
    fn classify_operates_on_parsed_value_directly() {
        // The downstream path classifies an already-parsed Value (no re-parse).
        let v: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"result":{"content":[]}}"#).unwrap();
        assert_eq!(
            classify(&v),
            Message::Response {
                id: RequestId::Num(3)
            }
        );
        // Non-object value is Other, same as the non-JSON line path.
        assert_eq!(classify(&Value::Array(vec![])), Message::Other);
    }

    #[test]
    fn tracker_records_and_takes() {
        let t = RequestTracker::new();
        t.record_request(RequestId::Num(1), "tools/call".into());
        assert_eq!(t.take_method(&RequestId::Num(1)), Some("tools/call".into()));
    }

    #[test]
    fn tracker_take_unknown_is_none() {
        let t = RequestTracker::new();
        assert_eq!(t.take_method(&RequestId::Num(99)), None);
    }

    #[test]
    fn tracker_take_consumes() {
        let t = RequestTracker::new();
        t.record_request(RequestId::Str("x".into()), "initialize".into());
        assert_eq!(
            t.take_method(&RequestId::Str("x".into())),
            Some("initialize".into())
        );
        // second take returns None — entry was consumed
        assert_eq!(t.take_method(&RequestId::Str("x".into())), None);
    }
}
