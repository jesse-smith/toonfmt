//! A1 SPIKE — characterize the `serde_json::Value` ↔ rmcp typed-message boundary
//! and confirm the Streamable HTTP transport is drivable as a proxy (no
//! `serve_client`). Output = the per-method **routing map** consumed by A4.
//!
//! This is exploratory/characterizing code, not production. It is kept in-tree so
//! the routing-map evidence is reproducible and so a future rmcp bump re-runs the
//! same probes. Two layers, deliberately split:
//!
//!   1. `routing_map_*` — **pure serde**, no network. The load-bearing question is
//!      "does rmcp's typed layer round-trip our Values faithfully?" — that's pure
//!      `from_value`/`to_value` through `Client/ServerJsonRpcMessage`. Isolating it
//!      from the transport removes a confounder: a round-trip diff here is rmcp's
//!      type reshaping, not an HTTP/SSE artifact. These run in CI always.
//!   2. `transport_*` (`#[ignore]`) — the mechanics: construct
//!      `StreamableHttpClientTransport` against the A3 stub, drive the
//!      initialize→initialized handshake, and confirm a Value lifts→sends→receives
//!      over the wire. Needs the python stub + a live socket, hence `#[ignore]`.
//!
//! The "direct (non-rmcp) round-trip" the plan calls for is, in the pure layer,
//! the original `Value` itself (constructed to mirror exactly what the stub emits):
//! the comparison `raw == to_value(from_value::<…>(raw))` measures precisely what
//! rmcp's typed layer does to the payload.

use serde_json::{Value, from_value, json, to_value};

use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};

/// Outcome of round-tripping one message Value through an rmcp typed enum.
/// Fields are surfaced only through `Debug` (printed into the routing-map log),
/// which the dead-code lint doesn't count as a read — hence the allow.
#[derive(Debug)]
#[allow(dead_code)]
enum Routing {
    /// `from_value` succeeded AND `to_value` is structurally equal to the input:
    /// the typed path is faithful for this method.
    Faithful,
    /// `from_value` succeeded but `to_value` differs: typed deserialization
    /// reshaped the payload (would violate semantic passthrough if used).
    Reshapes { before: Value, after: Value },
    /// `from_value` itself failed: no typed variant accepts this shape.
    Rejected { error: String },
}

/// Round-trip a **client→server** message (request or notification) through
/// `ClientJsonRpcMessage` and classify the outcome.
fn route_client(raw: &Value) -> Routing {
    match from_value::<ClientJsonRpcMessage>(raw.clone()) {
        Ok(typed) => match to_value(&typed) {
            Ok(back) if back == *raw => Routing::Faithful,
            Ok(back) => Routing::Reshapes {
                before: raw.clone(),
                after: back,
            },
            Err(e) => Routing::Rejected {
                error: format!("to_value failed: {e}"),
            },
        },
        Err(e) => Routing::Rejected {
            error: e.to_string(),
        },
    }
}

/// Round-trip a **server→client** message through `ServerJsonRpcMessage`. This is
/// the direction toonfmt actually transforms — `tools/call` results land here, and
/// rmcp has *already* forced this typed deserialization inside `receive()` before
/// we ever see the message, so what this measures is non-negotiable, not a choice.
fn route_server(raw: &Value) -> Routing {
    match from_value::<ServerJsonRpcMessage>(raw.clone()) {
        Ok(typed) => match to_value(&typed) {
            Ok(back) if back == *raw => Routing::Faithful,
            Ok(back) => Routing::Reshapes {
                before: raw.clone(),
                after: back,
            },
            Err(e) => Routing::Rejected {
                error: format!("to_value failed: {e}"),
            },
        },
        Err(e) => Routing::Rejected {
            error: e.to_string(),
        },
    }
}

/// The exact `tools/call` result the A3 stub emits for `probe_content_only`:
/// `content[0].text` is a JSON-object *string* (the transform's target), plus
/// `isError: false`. This is the payload whose fidelity matters most.
fn stub_tools_call_result(id: i64) -> Value {
    let inner = json!({
        "routing_probe": "CONTENT_CHANNEL",
        "seen_via": "content",
        "rows": [{"id": 1, "name": "alice", "role": "admin"}],
    });
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": inner.to_string()}],
            "isError": false,
        },
    })
}

// ============================================================================
// PURE-SERDE ROUTING MAP — the load-bearing evidence (no network).
// ============================================================================

/// The headline check: a `tools/call` **result** — the one payload we TOON-encode
/// — must survive rmcp's forced typed round-trip with its `content[].text` and
/// `isError` intact. We assert the fields the transform reads are preserved; we do
/// NOT require byte-identity of the whole envelope (rmcp may normalize cosmetics),
/// because the transform re-serializes from a `Value` anyway. The test PRINTS the
/// full before/after so the routing map records the exact reshape, if any.
#[test]
fn routing_map_tools_call_result_preserves_transform_inputs() {
    let raw = stub_tools_call_result(3);
    let routing = route_server(&raw);
    eprintln!("[routing-map] tools/call result (server→client): {routing:?}");

    // Round-trip through rmcp's ServerJsonRpcMessage and re-extract.
    let typed = from_value::<ServerJsonRpcMessage>(raw.clone())
        .expect("tools/call result must deserialize into ServerJsonRpcMessage");
    let back = to_value(&typed).expect("re-serialize to Value");

    // The fields the transform actually reads must be byte-faithful.
    let orig_text = raw["result"]["content"][0]["text"].as_str().unwrap();
    let back_text = back["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text vanished/reshaped; full: {back}"));
    assert_eq!(
        orig_text, back_text,
        "the JSON-string content block must survive rmcp's typed round-trip verbatim"
    );
    assert_eq!(
        back["result"]["isError"],
        json!(false),
        "isError must survive (the transform short-circuits on isError==true)"
    );

    // Record, but don't fail on, whole-envelope cosmetic reshaping: it's the
    // routing-map datum A4 needs (typed-faithful vs typed-reshapes).
    if back != raw {
        eprintln!(
            "[routing-map] NOTE tools/call envelope reshaped (cosmetic, transform inputs intact):\n  before: {raw}\n  after:  {back}"
        );
    }
}

/// `tools/list` (server→client result): a control. Passes through structurally.
#[test]
fn routing_map_tools_list_result() {
    let raw = json!({
        "jsonrpc": "2.0",
        "id": 99,
        "result": {"tools": [
            {"name": "probe_content_only", "description": "x",
             "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}}
        ]},
    });
    let routing = route_server(&raw);
    eprintln!("[routing-map] tools/list result (server→client): {routing:?}");
    // Whatever the verdict, the tools array must survive for passthrough.
    let typed = from_value::<ServerJsonRpcMessage>(raw.clone()).expect("tools/list deserializes");
    let back = to_value(&typed).unwrap();
    assert!(
        back["result"]["tools"].is_array(),
        "tools/list array must survive the round-trip; got: {back}"
    );
}

/// Client→server requests/notifications we forward upstream: `initialize`,
/// `tools/call`, `tools/list`, `notifications/initialized`. Each should hit a typed
/// variant. `x/bogus` (unknown method) must fall to the `CustomRequest` catch-all,
/// not error. We PRINT each verdict — that table is the outbound routing map.
#[test]
fn routing_map_client_outbound_methods() {
    let cases: &[(&str, Value)] = &[
        (
            "initialize",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"toonfmt-spike","version":"0.0.0"}
            }}),
        ),
        (
            "notifications/initialized",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        ),
        (
            "tools/list",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        ),
        (
            "tools/call",
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                   "params":{"name":"probe_content_only","arguments":{}}}),
        ),
    ];

    for (name, raw) in cases {
        let routing = route_client(raw);
        eprintln!("[routing-map] {name} (client→server): {routing:?}");
        assert!(
            !matches!(routing, Routing::Rejected { .. }),
            "known method {name} must lift into a typed ClientRequest/Notification, got {routing:?}"
        );
    }

    // The unknown-method escape hatch: must NOT error, must round-trip via Custom.
    let bogus = json!({"jsonrpc":"2.0","id":9,"method":"x/bogus","params":{"foo":1}});
    let routing = route_client(&bogus);
    eprintln!("[routing-map] x/bogus (client→server, expect Custom): {routing:?}");
    assert!(
        !matches!(routing, Routing::Rejected { .. }),
        "unknown method must fall to CustomRequest, not be rejected; got {routing:?}"
    );
    // And the method name + params must survive the Custom round-trip.
    let typed = from_value::<ClientJsonRpcMessage>(bogus.clone()).expect("x/bogus via Custom");
    let back = to_value(&typed).unwrap();
    assert_eq!(back["method"], json!("x/bogus"), "Custom must preserve method");
    assert_eq!(back["params"], json!({"foo":1}), "Custom must preserve params");
}

/// A server→client **request** with an unknown method (the GET-stream surface:
/// sampling/elicitation arrive this way). Confirms it deserializes (via Custom)
/// rather than erroring — so if rmcp ever surfaces one, A4 can `to_value` it to
/// warn with the method name rather than panicking.
#[test]
fn routing_map_server_unknown_request_via_custom() {
    let raw = json!({"jsonrpc":"2.0","id":42,"method":"sampling/createMessage","params":{"x":1}});
    let routing = route_server(&raw);
    eprintln!("[routing-map] sampling/createMessage (server→client request): {routing:?}");
    assert!(
        !matches!(routing, Routing::Rejected { .. }),
        "a server→client request must be representable (typed or Custom), got {routing:?}"
    );
}

// ============================================================================
// LIVE TRANSPORT MECHANICS (#[ignore] — needs the python3 A3 stub + a socket).
// ============================================================================

/// Drive the real `StreamableHttpClientTransport` against the A3 stub WITHOUT
/// `serve_client`: confirm (1) the worker starts at construction (send/receive
/// work directly), (2) the hard-coded handshake order — initialize first, then
/// the worker blocks until the `initialized` notification — and (3) a subsequent
/// `tools/call` round-trips over the wire with its content block intact.
///
/// `#[ignore]` because it spawns the python stub and opens a socket; run with
/// `cargo test --test rmcp_spike -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs python3 + live socket; run with --ignored"]
async fn transport_drivable_raw_and_handshake_order() {
    use rmcp::transport::{StreamableHttpClientTransport, Transport};
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    // Spawn the stub; read the `PORT <n>` line off its first stdout line.
    let mut stub = Command::new("python3")
        .arg("tests/fixtures/http_mcp_stub.py")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn http_mcp_stub.py (is python3 on PATH?)");
    let stdout = stub.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read PORT line");
    let port: u16 = line
        .trim()
        .strip_prefix("PORT ")
        .expect("first stub line is `PORT <n>`")
        .parse()
        .expect("port is a number");

    let url = format!("http://127.0.0.1:{port}/mcp");
    let mut transport =
        StreamableHttpClientTransport::from_uri(url.as_str());

    // (1)+(2): handshake in the worker's required order.
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-03-26","capabilities":{},
        "clientInfo":{"name":"toonfmt-spike","version":"0.0.0"}
    }});
    transport
        .send(from_value::<ClientJsonRpcMessage>(init).unwrap())
        .await
        .expect("send initialize");
    let init_resp = transport.receive().await.expect("receive InitializeResult");
    let init_resp_v = to_value(&init_resp).unwrap();
    eprintln!("[transport] initialize result: {init_resp_v}");
    assert_eq!(init_resp_v["result"]["protocolVersion"], json!("2025-03-26"));

    let initialized = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    transport
        .send(from_value::<ClientJsonRpcMessage>(initialized).unwrap())
        .await
        .expect("send initialized notification");

    // (3): a tools/call over the wire (plain application/json probe).
    let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"probe_content_only","arguments":{}}});
    transport
        .send(from_value::<ClientJsonRpcMessage>(call).unwrap())
        .await
        .expect("send tools/call");
    let resp = transport.receive().await.expect("receive tools/call result");
    let resp_v = to_value(&resp).unwrap();
    eprintln!("[transport] tools/call result over wire: {resp_v}");
    let text = resp_v["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text present after wire round-trip");
    let inner: Value = serde_json::from_str(text).expect("content text is JSON-string");
    assert_eq!(inner["routing_probe"], json!("CONTENT_CHANNEL"));

    transport.close().await.ok();
    stub.kill().ok();
    stub.wait().ok();
}
