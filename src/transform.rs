//! The TOON transform: the one mutation site on the passthrough baseline.
//!
//! Operates on a single, already-parsed `tools/call` **result** envelope. Every
//! `content[]` text block whose text parses as JSON (strict, then JSON5) is
//! re-encoded as TOON in place; everything else is left exactly as received.
//! `structuredContent` is stripped only when a transformed block is structurally
//! equal to it (loss-free by construction — see `ARCHITECTURE.md`).

use serde_json::Value;

/// Per-result **delivered** byte savings from the TOON transform, in bytes.
///
/// Populated only for content the model actually ingests — the strip path
/// (redundant `structuredContent` removed) or the content-only path (no
/// `structuredContent` to shadow it). A *kept* (structurally-unequal)
/// `structuredContent` shadows our TOON, so the model never reads it →
/// [`Savings::default`] (both zero), even though the rewritten bytes are still
/// emitted on the wire.
///
/// `saved_bytes` is **signed**: TOON can be *larger* than compact JSON for
/// small or non-tabular payloads (no array-of-uniform-objects to collapse), so
/// a transformed block can post a negative delta. The aggregate must sum signed
/// deltas, never wins-only.
///
/// Fields map 1:1 onto the stats store's columns (S3); the proxy hot path
/// discards this value today (S2 is pure accounting — no IO, no DB, no flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Savings {
    /// Σ of the original `content[].text` byte lengths over delivered transformed blocks.
    pub original_bytes: u64,
    /// Σ of `original − toon` byte deltas over delivered transformed blocks (signed).
    pub saved_bytes: i64,
}

/// Transform a `tools/call` result envelope. Returns `Some((serialized, savings))`
/// iff something changed (a content block was re-encoded as TOON, and/or a
/// redundant `structuredContent` was stripped); `None` means "nothing to do —
/// forward the original bytes unchanged" (the Phase 1 fidelity path). When `None`,
/// nothing was transformed, so delivered savings are zero by definition.
///
/// The `savings` are **delivered-only and signed** — see [`Savings`].
///
/// Receives the already-parsed envelope (the downstream pump parses each line to a
/// `Value` once and reuses it) — does not re-parse.
pub fn tools_call_result(mut value: Value) -> Option<(String, Savings)> {
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
    //    parse (or fails to encode) is left untouched. Accumulate the original and
    //    TOON byte lengths per converted block (captured *before* the `insert`
    //    overwrites the text) — these become the savings if the result turns out
    //    to be delivered (resolved after the strip decision in step 4).
    let mut transformed: Vec<Value> = Vec::new();
    let mut original_bytes: u64 = 0;
    let mut toon_bytes: u64 = 0;
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
        // Capture byte lengths before the text is overwritten; `text` borrows
        // `obj`, so read its length before the `insert` reborrows `obj` mutably.
        original_bytes += text.len() as u64;
        toon_bytes += toon.len() as u64;
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

    // 5. Resolve **delivered** savings. A transformed block is read by the model
    //    iff no `structuredContent` shadows it — i.e. we either stripped a
    //    structurally-equal one, or there was none to begin with. A *kept*
    //    (unequal) `structuredContent` shadows every content block, so the TOON we
    //    wrote is never read → savings are zero even though the bytes ship.
    //    Delivered-ness is a per-result property: once nothing shadows the content
    //    array, all of its transformed blocks are read together. After step 4 the
    //    key is absent iff it was stripped or never present — exactly the delivered
    //    cases — so its presence *now* is the shadow test.
    let delivered = !result_obj.contains_key("structuredContent");
    let savings = if delivered {
        Savings {
            original_bytes,
            saved_bytes: original_bytes as i64 - toon_bytes as i64,
        }
    } else {
        Savings::default()
    };

    // 6. Nothing changed → None (preserve original bytes). Else re-serialize the
    //    whole envelope compact. `preserve_order` keeps key order intact, so the
    //    only on-wire deltas are the rewritten `content[].text` and the removed
    //    `structuredContent` key (when stripped).
    let changed = !transformed.is_empty() || should_strip;
    if !changed {
        return None;
    }
    serde_json::to_string(&value).ok().map(|s| (s, savings))
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

        let (out, _) = tools_call_result(env).unwrap();
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

        let (out, _) = tools_call_result(env).unwrap();
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

        let (out, _) = tools_call_result(env).unwrap();
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

        let (out, _) = tools_call_result(env).unwrap();
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

        let (out, _) = tools_call_result(env).unwrap();
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
        let (out, _) = tools_call_result(env).unwrap();
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
        let (out2, _) = tools_call_result(env2).unwrap();
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

        let (out, _) = tools_call_result(env).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(
            parsed["result"]["content"][0]["text"].as_str().unwrap(),
            "rows[1]{id,name}:\n  1,a"
        );
    }

    // ---- S2: delivered-only, signed savings accounting ----
    //
    // The metric must (1) count exact original bytes + signed delta on delivered
    // paths, (2) go *negative* when TOON grows a block, (3) be *zero* when a kept
    // `structuredContent` shadows the TOON the model never reads, and (4) sum
    // per-block over a multi-block content array. Byte counts are derived from the
    // encoder, never hardcoded, so an encoder change can't silently rot the test.

    /// (s1) Content-only (no structuredContent) → delivered. `original_bytes` is the
    /// exact input `text.len()`; `saved_bytes` is the exact signed delta. A tabular
    /// array-of-objects shrinks, so the delta is positive here.
    #[test]
    fn savings_content_only_is_delivered_and_exact() {
        let inner = json!({"users": [{"id": 1, "name": "ann"}, {"id": 2, "name": "bob"}]});
        let text = serde_json::to_string(&inner).unwrap();
        let toon = toon_format::encode_default(&inner).unwrap();
        let env = envelope(json!({"content": [{"type": "text", "text": text.clone()}]}));

        let (_, savings) = tools_call_result(env).unwrap();
        assert_eq!(savings.original_bytes, text.len() as u64);
        assert_eq!(savings.saved_bytes, text.len() as i64 - toon.len() as i64);
        assert!(savings.saved_bytes > 0, "a uniform-object array should shrink under TOON");
    }

    /// (s2) Delivered but TOON is *larger* → `saved_bytes` is negative. Uses a
    /// non-uniform array (no shared shape to collapse), measured larger as TOON.
    /// This is the property that forbids a `u64`/wins-only metric.
    #[test]
    fn savings_can_be_negative_when_toon_grows() {
        let inner = json!({"items": [{"a": 1}, {"b": 2, "c": 3}]});
        let text = serde_json::to_string(&inner).unwrap();
        let toon = toon_format::encode_default(&inner).unwrap();
        assert!(toon.len() > text.len(), "fixture precondition: TOON larger than JSON");
        let env = envelope(json!({"content": [{"type": "text", "text": text.clone()}]}));

        let (_, savings) = tools_call_result(env).unwrap();
        assert_eq!(savings.original_bytes, text.len() as u64);
        assert_eq!(savings.saved_bytes, text.len() as i64 - toon.len() as i64);
        assert!(savings.saved_bytes < 0, "TOON-larger block must post a negative delta");
    }

    /// (s3) Strip path (structuredContent equal → removed) → delivered, exact.
    #[test]
    fn savings_strip_path_is_delivered() {
        let data = json!({"rows": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]});
        let text = serde_json::to_string(&data).unwrap();
        let toon = toon_format::encode_default(&data).unwrap();
        let env = envelope(json!({
            "content": [{"type": "text", "text": text.clone()}],
            "structuredContent": data,
        }));

        let (_, savings) = tools_call_result(env).unwrap();
        assert_eq!(savings.original_bytes, text.len() as u64);
        assert_eq!(savings.saved_bytes, text.len() as i64 - toon.len() as i64);
    }

    /// (s4) Keep path (structuredContent present & not equal) → the TOON is
    /// shadowed and never read → savings are **zero**, even though the call still
    /// returns `Some` (bytes were rewritten on the wire). This is the delivered-only
    /// rule's whole point.
    #[test]
    fn savings_kept_structured_content_contributes_zero() {
        let sc = json!({"id": 1, "extra": true}); // extra field → not equal → kept
        let env = envelope(json!({
            "content": [{"type": "text", "text": r#"{"id":1}"#}],
            "structuredContent": sc.clone(),
        }));

        let (out, savings) = tools_call_result(env).unwrap();
        // It DID transform (Some) and kept structuredContent...
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["result"]["structuredContent"], sc);
        // ...but nothing was delivered to the model, so savings are zero.
        assert_eq!(savings, Savings::default());
    }

    /// (s5) Multi-block delivered: only convertible blocks count, and they sum.
    /// One shrinking JSON block + one growing JSON block + prose (ignored) → the
    /// aggregate is the signed sum of the two converted blocks' deltas.
    #[test]
    fn savings_sum_over_multiple_blocks() {
        let a = json!({"rows": [{"id": 1, "name": "x"}, {"id": 2, "name": "y"}]}); // shrinks
        let b = json!({"items": [{"a": 1}, {"b": 2, "c": 3}]}); // grows
        let a_text = serde_json::to_string(&a).unwrap();
        let b_text = serde_json::to_string(&b).unwrap();
        let a_toon = toon_format::encode_default(&a).unwrap();
        let b_toon = toon_format::encode_default(&b).unwrap();
        let env = envelope(json!({"content": [
            {"type": "text", "text": a_text.clone()},
            {"type": "text", "text": "just prose, skipped"},
            {"type": "text", "text": b_text.clone()},
        ]}));

        let (_, savings) = tools_call_result(env).unwrap();
        assert_eq!(savings.original_bytes, (a_text.len() + b_text.len()) as u64);
        let expected = (a_text.len() as i64 - a_toon.len() as i64)
            + (b_text.len() as i64 - b_toon.len() as i64);
        assert_eq!(savings.saved_bytes, expected);
    }
}
