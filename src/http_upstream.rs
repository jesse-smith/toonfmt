//! HTTP upstream driver: bridges a **stdio** MCP client to a **Streamable HTTP**
//! MCP server via rmcp's `StreamableHttpClientTransport`.
//!
//! The client leg stays stdio (Claude Code speaks stdio); only the upstream leg
//! is HTTP. The transform is identical to the stdio path — the downstream decision
//! is `proxy::transform_downstream`, shared verbatim — so this module is purely
//! the transport bridge: lift client `Value`s into rmcp's typed message, `send`;
//! `receive` server messages, `to_value`, forward (transforming `tools/call`).
//!
//! **Passthrough is semantic, not byte-identical here** (the documented amendment):
//! there are no original stdio bytes to round-trip — they were an HTTP/SSE body —
//! so each forwarded message is re-serialized from the `Value` rmcp produced. The
//! stdio leg's byte-identical guarantee is untouched.
//!
//! Routing is governed by A1's measured map (`tests/rmcp_spike.rs`): rmcp's
//! `from_value`/`to_value` auto-select the typed-or-`Custom` variant, the
//! `tools/call` result we transform round-trips faithfully, and unknown methods
//! fall to `Custom*` rather than erroring — so there is **no method allow-list**.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, from_value, to_value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::mpsc;

use rmcp::RoleClient;
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

use crate::cli::HttpUpstream;
use crate::jsonrpc::{Message, RequestTracker, classify};
use crate::proxy::transform_downstream;

/// Drive the HTTP upstream until the client's stdin closes or the upstream ends.
///
/// `bearer` is the already-resolved token (the CLI resolved `--bearer-env` with
/// fail-fast semantics before we got here), set as `Authorization: Bearer` on
/// every request by rmcp's transport.
pub async fn run(upstream: HttpUpstream, bearer: Option<String>) -> Result<()> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(upstream.url.clone());
    if let Some(token) = bearer {
        config = config.auth_header(token);
    }
    let mut transport = StreamableHttpClientTransport::from_config(config);

    // Correlate request ids to methods so the downstream decision knows whether a
    // response is a `tools/call`. On the HTTP leg the correlation is still causal
    // (we record on the way out, consume on the way in), so the tracker is reused
    // rather than carrying the method in a parallel structure — but unlike stdio,
    // the two directions share one task, so there's no cross-thread race to fear.
    let tracker = RequestTracker::new();
    let mut stdout = tokio::io::stdout();

    // Read client stdin on a dedicated task, pushing parsed `Value`s over a
    // channel. Keeping the reader separate lets the main loop `select!` between
    // "client sent something" and "upstream sent something" without one blocking
    // the other.
    let (tx, mut rx) = mpsc::channel::<Value>(64);
    let reader = tokio::spawn(read_stdin(tx));

    // === Explicit handshake (do NOT collapse into the steady-state loop) =======
    // rmcp's StreamableHttpClientWorker hard-codes the startup order: it posts the
    // first message expecting `initialize`, relays the response, then BLOCKS until
    // it receives the client's `initialized` notification before processing
    // anything further. Independently, the *client* won't emit `initialized` until
    // it sees the `initialize` response on our stdout. So the order is forced:
    //   (1) client initialize → upstream;
    //   (2) upstream initialize response → client stdout  ← unblocks the client;
    //   (3) client initialized notification → upstream    ← unblocks the worker.
    // A naive "read each stdin line and send" loop that defers all receives would
    // deadlock here (client waits on the response we never forward). Keep these as
    // distinct steps.

    // (1) initialize.
    let init = rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("client closed stdin before sending `initialize`"))?;
    send_client(&mut transport, &tracker, init).await?;

    // (2) forward the initialize response. Loop past any server-initiated message
    // that may precede it (warned + dropped) until the actual response/error lands.
    loop {
        let msg = transport
            .receive()
            .await
            .ok_or_else(|| anyhow!("HTTP upstream closed during initialize"))?;
        let is_response = matches!(
            msg,
            JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_)
        );
        forward_to_client(&mut stdout, &tracker, msg).await?;
        if is_response {
            break;
        }
    }

    // (3) initialized notification.
    let initialized = rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("client closed stdin before sending `initialized`"))?;
    send_client(&mut transport, &tracker, initialized).await?;

    // === Steady state =========================================================
    // Two concurrent halves in one task. Only the `receive()` branch borrows the
    // transport inside `select!`; a stdin message is stashed and sent *after*
    // `select!` returns, so we never hold two `&mut transport` borrows at once.
    let mut stdin_open = true;
    while stdin_open {
        let mut pending: Option<Value> = None;
        tokio::select! {
            from_client = rx.recv() => {
                match from_client {
                    Some(v) => pending = Some(v),
                    // Client stdin closed. Don't abandon in-flight requests:
                    // stop reading stdin and fall through to the drain phase.
                    None => stdin_open = false,
                }
            }
            from_upstream = transport.receive() => {
                match from_upstream {
                    Some(msg) => forward_to_client(&mut stdout, &tracker, msg).await?,
                    // Upstream closed the session: nothing left to receive. Break to
                    // teardown; the drain loop below no-ops (receive() returns None).
                    None => break,
                }
            }
        }
        if let Some(v) = pending.take() {
            send_client(&mut transport, &tracker, v).await?;
        }
    }

    // === Drain ================================================================
    // Client stdin closed but responses to already-sent requests may still be in
    // flight (e.g. an async SQL result). Keep receiving until every outstanding
    // request id is answered, or a grace period elapses — then tear down. Without
    // this, a request immediately followed by EOF loses its response. Bounded so a
    // server that never replies can't hang shutdown.
    if tracker.pending_count() > 0 {
        let grace = std::time::Duration::from_secs(30);
        let drain = async {
            while tracker.pending_count() > 0 {
                match transport.receive().await {
                    Some(msg) => forward_to_client(&mut stdout, &tracker, msg).await?,
                    None => break, // upstream closed
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        match tokio::time::timeout(grace, drain).await {
            Ok(result) => result?,
            Err(_) => tracing::warn!(
                outstanding = tracker.pending_count(),
                "stdin closed; gave up draining in-flight responses after 30s"
            ),
        }
    }

    transport.close().await.ok();
    reader.abort();
    Ok(())
}

/// Read newline-framed JSON-RPC from the client's stdin, forwarding each parsed
/// message over `tx`. Non-JSON lines are warned and skipped: unlike the stdio
/// pump there are no raw bytes to pass through over HTTP. Ends at EOF.
async fn read_stdin(tx: mpsc::Sender<Value>) -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .await
            .context("reading client stdin")?;
        if n == 0 {
            break; // EOF
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(v) => {
                if tx.send(v).await.is_err() {
                    break; // receiver gone — driver is shutting down
                }
            }
            Err(_) => {
                let s = String::from_utf8_lossy(&line);
                tracing::warn!(
                    line = %s.trim_end(),
                    "client→upstream: skipping non-JSON line (cannot forward over HTTP)"
                );
            }
        }
    }
    Ok(())
}

/// Lift a client `Value` into rmcp's typed message and send it upstream,
/// recording the request id→method correlation on the way out.
async fn send_client<T>(transport: &mut T, tracker: &RequestTracker, val: Value) -> Result<()>
where
    T: Transport<RoleClient>,
{
    if let Message::Request { id, method } = classify(&val) {
        tracker.record_request(id, method);
    }
    let typed: ClientJsonRpcMessage =
        from_value(val).context("lifting client message into rmcp client type")?;
    transport
        .send(typed)
        .await
        .context("sending message to HTTP upstream")?;
    Ok(())
}

/// Forward a received server message to the client's stdout.
///
/// - **Response / Error** (the reply to a request we sent): serialize, run the
///   shared `transform_downstream` (TOON-encodes a correlated `tools/call`
///   result; passes everything else through), write one framed line.
/// - **Request / Notification** (server-initiated — the GET-stream surface:
///   sampling, elicitation, async notifications): **not forwarded in Slice A**,
///   but **warned, never silently dropped**, so the deferred hole is observable.
///   A6 checks whether the real target actually uses this.
async fn forward_to_client(
    stdout: &mut Stdout,
    tracker: &RequestTracker,
    msg: JsonRpcMessage<
        rmcp::model::ServerRequest,
        rmcp::model::ServerResult,
        rmcp::model::ServerNotification,
    >,
) -> Result<()> {
    match msg {
        JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_) => {
            let val = to_value(&msg).context("serializing server response")?;
            // Pre-serialize for the passthrough case; `transform_downstream` takes
            // ownership of `val` and returns `Some` only when it rewrote it.
            let passthrough = val.to_string();
            let line = transform_downstream(val, tracker).unwrap_or(passthrough);
            stdout
                .write_all(line.as_bytes())
                .await
                .context("writing to client stdout")?;
            stdout.write_all(b"\n").await.context("writing line framing")?;
            stdout.flush().await.context("flushing client stdout")?;
        }
        JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => {
            let method = to_value(&msg)
                .ok()
                .and_then(|v| {
                    v.get("method")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "<unknown>".to_string());
            tracing::warn!(
                %method,
                "server-initiated message not forwarded to client (GET-stream handling \
                 deferred in Slice A); if a real target needs this, it becomes its own task"
            );
        }
    }
    Ok(())
}
