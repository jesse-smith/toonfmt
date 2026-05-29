//! End-to-end transform: drive the real `toonfmt` binary wrapping the Python MCP
//! stub, and assert the `tools/call` results come back TOON'd (and that the
//! equality-gated `structuredContent` strip fires) end-to-end.
//!
//! Exact TOON correctness is pinned by the fast unit tests in `src/transform.rs`
//! (incl. a wire-format canary). This suite proves the *wiring*: parse → classify
//! → correlate → transform → rewrite the line on stdout, with everything else
//! passing through. Skips with a clear message if `python3` is unavailable.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// True if `python3` is on PATH (the stub's only runtime requirement).
fn have_python3() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Drive a full handshake + the given `tools/call` tool names through
/// `toonfmt -- python3 tests/fixtures/json_mcp_stub.py`, plus a `tools/list`
/// control. Returns the parsed response envelopes keyed by request id.
///
/// Correlation is causal: each request is recorded by the client→upstream pump
/// (which forwards it) strictly before the stub can emit its response, so the
/// downstream transform always finds the method.
async fn drive(tool_calls: &[(&str, i64)]) -> std::collections::HashMap<i64, Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_toonfmt"))
        .args(["--", "python3", "tests/fixtures/json_mcp_stub.py"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn toonfmt + stub");

    let mut stdin = child.stdin.take().unwrap();

    // Handshake.
    let mut reqs = String::new();
    reqs.push_str(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    reqs.push('\n');
    reqs.push_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    reqs.push('\n');
    // tools/call requests under test.
    for (name, id) in tool_calls {
        reqs.push_str(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}"}}}}"#
        ));
        reqs.push('\n');
    }
    // Control: tools/list (id 99) — not a tools/call, must pass through.
    reqs.push_str(r#"{"jsonrpc":"2.0","id":99,"method":"tools/list"}"#);
    reqs.push('\n');

    stdin.write_all(reqs.as_bytes()).await.unwrap();
    drop(stdin); // EOF → stub reads to end → exits → toonfmt drains and exits

    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    let _ = child.wait().await.unwrap();

    // Index every response line by its id.
    let mut by_id = std::collections::HashMap::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("non-JSON line on stdout: {line:?} ({e})"));
        if let Some(id) = v.get("id").and_then(Value::as_i64) {
            by_id.insert(id, v);
        }
    }
    by_id
}

/// (1) `probe_content_only`: content text block was JSON → comes back TOON (no
/// longer JSON), data preserved, and no `structuredContent` appears.
/// (2) `probe_equal_pair`: content TOON'd AND `structuredContent` stripped.
/// (3) `tools/list` control passes through structurally intact.
#[tokio::test]
async fn transforms_tools_call_results_end_to_end() {
    if !have_python3() {
        eprintln!("SKIP transform_e2e: python3 not available on PATH");
        return;
    }

    let resp = drive(&[("probe_content_only", 2), ("probe_equal_pair", 3)]).await;

    // (1) content-only → TOON in content, never any structuredContent.
    let r2 = resp.get(&2).expect("response for id 2");
    let block2 = &r2["result"]["content"][0]["text"];
    let text2 = block2.as_str().expect("content[0].text is a string");
    assert!(
        serde_json::from_str::<Value>(text2).is_err(),
        "content text should be TOON (no longer parseable as JSON), got: {text2:?}"
    );
    assert!(
        text2.contains("CONTENT_CHANNEL"),
        "TOON must preserve the payload data, got: {text2:?}"
    );
    assert!(
        r2["result"].get("structuredContent").is_none(),
        "probe_content_only has no structuredContent to begin with"
    );

    // (2) equal pair → content TOON'd AND structuredContent stripped end-to-end.
    let r3 = resp.get(&3).expect("response for id 3");
    let text3 = r3["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text is a string");
    assert!(
        serde_json::from_str::<Value>(text3).is_err(),
        "equal-pair content should be TOON, got: {text3:?}"
    );
    assert!(
        text3.contains("quarterly"),
        "TOON must preserve the payload data, got: {text3:?}"
    );
    assert!(
        r3["result"].get("structuredContent").is_none(),
        "redundant structuredContent must be stripped end-to-end"
    );

    // (3) control: tools/list passed through with its tools array intact.
    let r99 = resp.get(&99).expect("response for id 99 (tools/list)");
    assert!(
        r99["result"]["tools"].is_array(),
        "tools/list must pass through unchanged"
    );
}
