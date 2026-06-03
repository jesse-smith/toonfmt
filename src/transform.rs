//! The TOON transform: the one mutation site on the passthrough baseline.
//!
//! Operates on a single, already-parsed `tools/call` **result** envelope. Every
//! `content[]` text block whose text parses as JSON (strict, then JSON5) is
//! re-encoded as TOON in place; everything else is left exactly as received.
//! `structuredContent` is stripped only when a transformed block is structurally
//! equal to it (loss-free by construction — see `ARCHITECTURE.md`).

use serde_json::Value;

/// Transform a `tools/call` result envelope. Returns `Some(serialized)` iff
/// something changed (a content block was re-encoded as TOON, and/or a redundant
/// `structuredContent` was stripped); `None` means "nothing to do — forward the
/// original bytes unchanged" (the Phase 1 fidelity path).
///
/// Receives the already-parsed envelope (the downstream pump parses each line to a
/// `Value` once and reuses it) — does not re-parse.
pub fn tools_call_result(mut value: Value) -> Option<String> {
    // 1. Navigate to `result`. Absent (an error response carrying `error`) → None.
    //    `result.isError == true` → None (error payloads are often prose).
    let result_obj = value.get_mut("result").and_then(Value::as_object_mut)?;
    if result_obj.get("isError").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    // 2. `result.content` must be an array, else None.
    let content = result_obj.get_mut("content").and_then(Value::as_array_mut)?;

    // 3. Re-encode each convertible text block as TOON, retaining the parsed
    //    `Value` for the equality gate. Per-block fallback: a block that doesn't
    //    parse (or fails to encode) is left untouched.
    let mut transformed: Vec<Value> = Vec::new();
    for block in content.iter_mut() {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = obj.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Some(parsed) = parse_json_or_json5(text) else {
            continue; // genuinely not convertible (prose, etc.)
        };
        let toon = match toon_format::encode_default(&parsed) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "TOON encode failed; leaving block unchanged");
                continue;
            }
        };
        obj.insert("text".to_string(), Value::String(toon));
        transformed.push(parsed);
    }

    // 4. Equality-gated structuredContent strip. The `content` borrow ends here.
    let should_strip = match result_obj.get("structuredContent") {
        Some(sc) => {
            let equal = transformed.iter().any(|t| t == sc);
            if !equal {
                tracing::debug!(
                    "structuredContent not structurally equal to any transformed block; keeping it"
                );
            }
            equal
        }
        None => false,
    };
    if should_strip {
        result_obj.remove("structuredContent");
    }

    // 5. Nothing changed → None (preserve original bytes). Else re-serialize the
    //    whole envelope compact. `preserve_order` keeps key order intact, so the
    //    only on-wire deltas are the rewritten `content[].text` and the removed
    //    `structuredContent` key (when stripped).
    let changed = !transformed.is_empty() || should_strip;
    if !changed {
        return None;
    }
    serde_json::to_string(&value).ok()
}

/// Strict JSON first, then JSON5 (trailing commas, single quotes, unquoted keys,
/// comments — the LLM-output flavor). Stops there: no json-repair salvage, which
/// can "succeed" on malformed input by guessing and yield confident-but-wrong TOON.
/// A successful JSON5-only parse is debug-logged so misbehaving upstreams are auditable.
fn parse_json_or_json5(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    match json5::from_str::<Value>(text) {
        Ok(v) => {
            tracing::debug!("text block parsed via JSON5 fallback (strict JSON failed)");
            Some(v)
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a `tools/call` response envelope around a `result` object.
    fn envelope(result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": result})
    }

    /// (a) Single strict-JSON text block → block text becomes TOON; envelope
    /// stays valid with the same id/result shape and the block's `type` intact.
    #[test]
    fn strict_json_block_becomes_toon() {
        let inner = json!({"users": [{"id": 1, "name": "ann"}]});
        let text = serde_json::to_string(&inner).unwrap();
        let env = envelope(json!({"content": [{"type": "text", "text": text}]}));

        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["id"], json!(1));
        assert_eq!(parsed["result"]["content"][0]["type"], json!("text"));
        let block_text = parsed["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(block_text, toon_format::encode_default(&inner).unwrap());
        assert_ne!(block_text, text, "the block was actually rewritten");
    }

    /// (b) JSON5 (trailing comma) text → converts to TOON of the same data.
    #[test]
    fn json5_block_converts() {
        let env = envelope(json!({"content": [{"type": "text", "text": r#"{"a":1,"b":2,}"#}]}));

        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        let expected = toon_format::encode_default(&json!({"a": 1, "b": 2})).unwrap();
        assert_eq!(parsed["result"]["content"][0]["text"].as_str().unwrap(), expected);
    }

    /// (c) Prose text → None (nothing convertible, forward original bytes).
    #[test]
    fn prose_block_returns_none() {
        let env = envelope(json!({"content": [{"type": "text", "text": "This is a plain answer."}]}));
        assert!(tools_call_result(env).is_none());
    }

    /// (d) Multi-block [JSON, prose, image]: only the JSON converts; the prose and
    /// image blocks pass through byte-identical in value.
    #[test]
    fn multi_block_converts_only_json() {
        let inner = json!({"k": "v"});
        let json_text = serde_json::to_string(&inner).unwrap();
        let image = json!({"type": "image", "data": "base64==", "mimeType": "image/png"});
        let env = envelope(json!({"content": [
            {"type": "text", "text": json_text},
            {"type": "text", "text": "just words here"},
            image,
        ]}));

        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(
            parsed["result"]["content"][0]["text"].as_str().unwrap(),
            toon_format::encode_default(&inner).unwrap()
        );
        assert_eq!(parsed["result"]["content"][1]["text"], json!("just words here"));
        assert_eq!(
            parsed["result"]["content"][2],
            json!({"type": "image", "data": "base64==", "mimeType": "image/png"})
        );
    }

    #[test]
    fn non_object_content_element_is_skipped() {
        // A `content` array may legally hold non-object elements (the schema is
        // loose). A bare scalar is not a block to transform — skip it, don't panic,
        // and (with nothing else convertible) return None.
        let env = envelope(json!({"content": ["a bare string", 42]}));
        assert!(tools_call_result(env).is_none());
    }

    #[test]
    fn text_typed_block_without_string_text_is_skipped() {
        // type == "text" but `text` is absent / not a string. Don't transform, don't
        // panic — the block passes through and the call returns None (no change).
        let env = envelope(json!({"content": [
            {"type": "text"},                       // no `text` field
            {"type": "text", "text": {"not": "a string"}}, // `text` is an object
        ]}));
        assert!(tools_call_result(env).is_none());
    }

    /// (e) `result.isError == true` → None (error payloads left as prose).
    #[test]
    fn is_error_returns_none() {
        let env = envelope(json!({
            "isError": true,
            "content": [{"type": "text", "text": r#"{"x":1}"#}]
        }));
        assert!(tools_call_result(env).is_none());
    }

    /// (f) `content` missing or not an array → None.
    #[test]
    fn no_content_array_returns_none() {
        assert!(tools_call_result(envelope(json!({"content": "not an array"}))).is_none());
        assert!(tools_call_result(envelope(json!({"foo": "bar"}))).is_none());
    }

    /// (g) Equality strip — equal: structuredContent structurally equal to the
    /// transformed block → text converts AND the key is removed.
    #[test]
    fn structured_content_equal_is_stripped() {
        let data = json!({"id": 1, "name": "ann"});
        let text = serde_json::to_string(&data).unwrap();
        let env = envelope(json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": data,
        }));

        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(
            parsed["result"]["content"][0]["text"].as_str().unwrap(),
            toon_format::encode_default(&data).unwrap()
        );
        assert!(
            parsed["result"].get("structuredContent").is_none(),
            "redundant structuredContent should be stripped"
        );
    }

    /// (g2) Strip — object keys reordered: same data, different key order →
    /// `Value ==` is key-order-insensitive → still stripped.
    #[test]
    fn structured_content_reordered_keys_is_stripped() {
        // content text: name-then-id; structuredContent: id-then-name.
        let env = envelope(json!({
            "content": [{"type": "text", "text": r#"{"name":"ann","id":1}"#}],
            "structuredContent": {"id": 1, "name": "ann"},
        }));

        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert!(parsed["result"].get("structuredContent").is_none());
    }

    /// (g3) Keep — not equal: an extra field, or a reordered array (array
    /// equality is order-sensitive), keeps structuredContent intact.
    #[test]
    fn structured_content_not_equal_is_kept() {
        // Extra field.
        let sc = json!({"id": 1, "extra": true});
        let env = envelope(json!({
            "content": [{"type": "text", "text": r#"{"id":1}"#}],
            "structuredContent": sc.clone(),
        }));
        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["result"]["content"][0]["text"].as_str().unwrap(),
            toon_format::encode_default(&json!({"id": 1})).unwrap()
        );
        assert_eq!(parsed["result"]["structuredContent"], sc);

        // Reordered array → order-sensitive inequality → kept.
        let sc2 = json!({"rows": [3, 2, 1]});
        let env2 = envelope(json!({
            "content": [{"type": "text", "text": r#"{"rows":[1,2,3]}"#}],
            "structuredContent": sc2.clone(),
        }));
        let out2 = tools_call_result(env2).unwrap();
        let parsed2: Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(parsed2["result"]["structuredContent"], sc2);
    }

    /// (g4) Keep — no transformed block: structuredContent present but content is
    /// prose only → nothing transformed, nothing stripped → None (original
    /// forwarded unchanged, so structuredContent rides through intact).
    #[test]
    fn structured_content_kept_when_nothing_transformed() {
        let env = envelope(json!({
            "content": [{"type": "text", "text": "prose only, not json"}],
            "structuredContent": {"id": 1},
        }));
        assert!(tools_call_result(env).is_none());
    }

    /// (h) Error response (`error`, no `result`) → None.
    #[test]
    fn error_response_returns_none() {
        let env = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "boom"}});
        assert!(tools_call_result(env).is_none());
    }

    /// Canary: pins the actual TOON wire format so a silent encoder behavior
    /// change is caught here rather than only end-to-end.
    #[test]
    fn toon_output_format_canary() {
        let inner = json!({"rows": [{"id": 1, "name": "a"}]});
        let text = serde_json::to_string(&inner).unwrap();
        let env = envelope(json!({"content": [{"type": "text", "text": text}]}));

        let out = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(
            parsed["result"]["content"][0]["text"].as_str().unwrap(),
            "rows[1]{id,name}:\n  1,a"
        );
    }
}
