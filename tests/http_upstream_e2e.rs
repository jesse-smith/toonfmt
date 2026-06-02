//! End-to-end HTTP upstream: drive the real `toonfmt` binary in `--http` mode
//! against the Python Streamable HTTP MCP stub, and assert `tools/call` results
//! come back TOON'd — identically to the stdio path — across three framings:
//! plain `application/json`, single-line SSE, and multi-`data:`-line SSE.
//!
//! This is the first execution of the rmcp-backed driver (`src/http_upstream.rs`),
//! so it doubles as A4's real proof. Exact TOON correctness is pinned by the unit
//! tests in `src/transform.rs`; this suite proves the bridge wiring: handshake →
//! lift → send → receive → `to_value` → shared transform → framed stdout line.
//!
//! Skips with a clear message if `python3` is unavailable.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

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

/// Spawn the stub and read its first stdout line (`PORT <n>`), returning the
/// child (kept alive) and the port it bound.
async fn spawn_stub() -> (Child, u16) {
    let mut stub = Command::new("python3")
        .arg("tests/fixtures/http_mcp_stub.py")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn http_mcp_stub.py");

    let stdout = stub.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    // Bound the wait so a stub that never prints PORT fails the test instead of hanging.
    let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .expect("stub printed PORT line within 10s")
        .expect("read PORT line");
    assert!(read > 0, "stub closed stdout before printing PORT");
    let port: u16 = line
        .trim()
        .strip_prefix("PORT ")
        .unwrap_or_else(|| panic!("first stub line must be `PORT <n>`, got {line:?}"))
        .parse()
        .expect("port is a number");
    (stub, port)
}

/// Read newline-delimited JSON-RPC responses from toonfmt's stdout until we've
/// collected responses for all `want_ids`, or stdout closes. Returns them keyed
/// by id. Bounded by a timeout so a missing response fails rather than hangs.
async fn read_responses(
    stdout: &mut BufReader<ChildStdout>,
    want_ids: &[i64],
) -> std::collections::HashMap<i64, Value> {
    let mut by_id = std::collections::HashMap::new();
    let deadline = Duration::from_secs(15);
    let mut line = String::new();
    while !want_ids.iter().all(|id| by_id.contains_key(id)) {
        line.clear();
        let n = match tokio::time::timeout(deadline, stdout.read_line(&mut line)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("error reading toonfmt stdout: {e}"),
            Err(_) => panic!(
                "timed out waiting for responses {want_ids:?}; got ids {:?}",
                by_id.keys().collect::<Vec<_>>()
            ),
        };
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("non-JSON line on toonfmt stdout: {trimmed:?} ({e})"));
        if let Some(id) = v.get("id").and_then(Value::as_i64) {
            by_id.insert(id, v);
        }
    }
    by_id
}

/// Assert a `tools/call` response's content block is TOON (no longer JSON-parseable)
/// and still carries the payload sentinel.
fn assert_toon(resp: &Value, id: i64) {
    let block = &resp["result"]["content"][0]["text"];
    let text = block
        .as_str()
        .unwrap_or_else(|| panic!("id {id}: content[0].text is not a string: {resp}"));
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "id {id}: content should be TOON (not JSON-parseable), got: {text:?}"
    );
    assert!(
        text.contains("CONTENT_CHANNEL"),
        "id {id}: TOON must preserve the payload sentinel, got: {text:?}"
    );
}

/// The full slice-A assertion set over the three framings + a tools/list control.
#[tokio::test]
async fn http_upstream_transforms_across_framings() {
    if !have_python3() {
        eprintln!("SKIP http_upstream_e2e: python3 not available on PATH");
        return;
    }

    let (mut stub, port) = spawn_stub().await;
    let url = format!("http://127.0.0.1:{port}/mcp");

    let mut child = Command::new(env!("CARGO_BIN_EXE_toonfmt"))
        .args(["--http", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn toonfmt --http");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // (1) initialize — write, then read the response BEFORE sending `initialized`
    // (the worker blocks on initialized; the client must see the response first).
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\
              \"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\
              \"clientInfo\":{\"name\":\"e2e\",\"version\":\"0.0.0\"}}}\n",
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let init = read_responses(&mut stdout, &[1]).await;
    assert_eq!(
        init[&1]["result"]["protocolVersion"], "2025-03-26",
        "initialize handshake must complete and be forwarded to stdout"
    );

    // (2) initialized notification, then the three tool-call framings + a control.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    for (id, name) in [
        (2, "probe_content_only"),
        (3, "probe_sse"),
        (4, "probe_sse_split"),
    ] {
        let req = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\
             \"params\":{{\"name\":\"{name}\",\"arguments\":{{}}}}}}\n"
        );
        stdin.write_all(req.as_bytes()).await.unwrap();
    }
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"tools/list\"}\n")
        .await
        .unwrap();
    stdin.flush().await.unwrap();

    let resp = read_responses(&mut stdout, &[2, 3, 4, 99]).await;

    // (1) plain application/json → TOON.
    assert_toon(&resp[&2], 2);
    assert!(
        resp[&2]["result"].get("structuredContent").is_none(),
        "probe_content_only has no structuredContent"
    );

    // (2) single-line SSE → also TOON (rmcp assembles the event; transform fires).
    assert_toon(&resp[&3], 3);

    // (3) multi-`data:`-line SSE → TOON correctly. HONEST LABELING: on the rmcp
    // path, rmcp's `sse-stream` owns event assembly — `receive()` only ever yields
    // a COMPLETE message — so this is an rmcp-integration smoke check (it confirms
    // rmcp + our stub frame correctly), NOT a test of toonfmt's own buffering. That
    // becomes a test of our code only in the hand-rolled fallback we did not need.
    assert_toon(&resp[&4], 4);

    // (4) control: tools/list passes through with its array intact (not transformed).
    assert!(
        resp[&99]["result"]["tools"].is_array(),
        "tools/list must pass through structurally intact: {}",
        resp[&99]
    );

    // Clean teardown: closing stdin ends the driver's read loop → it exits.
    drop(stdin);
    let mut leftover = String::new();
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        stdout.read_to_string(&mut leftover),
    )
    .await;
    let _ = child.wait().await;
    stub.kill().await.ok();
}
