//! End-to-end **explicit OAuth** flow: drive the real `toonfmt` binary through the
//! whole `login` → persist → `serve --oauth` → reuse path against the Python
//! Streamable HTTP MCP stub running in OAuth mode (`TOONFMT_STUB_OAUTH=1`).
//!
//! This is the keeper-acceptance proof for Slice B's deterministic landing slice:
//!   1. `toonfmt login --http <url>` runs the authorization-code flow — discovery,
//!      dynamic client registration, the loopback callback, code↔token exchange —
//!      with a **headless browser** (`tests/fixtures/headless_browser.py`) standing
//!      in for the human, and persists the token to an isolated `HOME`.
//!   2. The token file lands under `~/.toonfmt-auth/` (per-URL, the file the serve
//!      path will read).
//!   3. `toonfmt --http <url> --oauth` loads that token, attaches it as the bearer
//!      (the stub 401s otherwise), and a `tools/call` comes back **TOON-encoded** —
//!      proving the OAuth serve path reaches the same transform as bearer/stdio.
//!   4. A second `serve --oauth` **reuses** the cached token with no re-login (no
//!      browser invocation, no new authorize hop) — the property that makes the
//!      serve path non-blocking.
//!
//! Skips with a clear message if `python3` is unavailable (the stub + headless
//! browser are its only runtime requirement — no real browser, no human, no net).

use std::path::PathBuf;
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

/// Spawn the stub **in OAuth mode** and read its `PORT <n>` first line. Returns the
/// child (kept alive via `kill_on_drop`) and the bound port.
async fn spawn_oauth_stub() -> (Child, u16) {
    let mut stub = Command::new("python3")
        .arg("tests/fixtures/http_mcp_stub.py")
        .env("TOONFMT_STUB_OAUTH", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn http_mcp_stub.py (OAuth mode)");

    let stdout = stub.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
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

/// A throwaway `$HOME` for the credential store, removed on drop. Keeps the e2e
/// hermetic — the token lands under `<home>/.toonfmt-auth/` and never touches the
/// developer's real store. (No `tempfile` dep; it isn't in the offline cargo cache.)
struct TempHome(PathBuf);
impl TempHome {
    fn new() -> Self {
        // Process id + a process-wide atomic counter: unique per call even across
        // tests running concurrently in one binary. (An earlier address-of-a-local
        // trick collided — two threads got the same stack address — which let two
        // tests share a HOME and double-count credential files.)
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let p = std::env::temp_dir().join(format!("toonfmt-oauth-e2e-{stamp}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp HOME");
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    /// The credential directory the binary will use under this HOME.
    fn auth_dir(&self) -> PathBuf {
        self.0.join(".toonfmt-auth")
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `toonfmt login --http <url>` with the headless browser and an isolated HOME.
/// Returns once login exits; asserts success.
async fn run_login(url: &str, home: &TempHome) {
    let browser_cmd = format!(
        "python3 {}/tests/fixtures/headless_browser.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_toonfmt"))
        .args(["login", "--http", url])
        .env("HOME", home.path())
        .env("TOONFMT_BROWSER_CMD", browser_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn toonfmt login");
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("toonfmt login completed within 20s")
        .expect("await toonfmt login");
    assert!(
        status.success(),
        "toonfmt login should succeed, got {status}"
    );
}

/// Read newline-delimited JSON-RPC responses from toonfmt's stdout until all
/// `want_ids` are seen or stdout closes. Bounded by a timeout.
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

/// Drive a `serve --oauth` session through the handshake + one `probe_content_only`
/// `tools/call` (id 2), returning that response. `browser_cmd`: if `Some`, set
/// `TOONFMT_BROWSER_CMD` — used to PROVE the second serve does NOT invoke it (we
/// point it at a command that would fail loudly if called).
async fn serve_oauth_one_call(url: &str, home: &TempHome) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_toonfmt"))
        .args(["--http", url, "--oauth"])
        .env("HOME", home.path())
        // If the serve path ever tried to open a browser, this would run and the
        // flow would diverge; pointing it at `false` makes such a regression a
        // launch that exits nonzero rather than a silent re-auth. (The serve path
        // must never call it — that's the property under test.)
        .env("TOONFMT_BROWSER_CMD", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn toonfmt --http --oauth");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // (1) initialize, read the response before sending initialized.
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\
              \"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\
              \"clientInfo\":{\"name\":\"oauth-e2e\",\"version\":\"0.0.0\"}}}\n",
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let init = read_responses(&mut stdout, &[1]).await;
    assert_eq!(
        init[&1]["result"]["protocolVersion"], "2025-03-26",
        "initialize must complete over the OAuth serve path"
    );

    // (2) initialized, then one tools/call.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\
              \"params\":{\"name\":\"probe_content_only\",\"arguments\":{}}}\n",
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();

    let resp = read_responses(&mut stdout, &[2]).await;
    let out = resp[&2].clone();

    drop(stdin);
    let mut leftover = String::new();
    let _ =
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_string(&mut leftover)).await;
    let _ = child.wait().await;
    out
}

/// Drive a `serve --oauth-interactive` session through the handshake + one
/// `probe_content_only` `tools/call`, returning that response. `browser_cmd` is set
/// as `TOONFMT_BROWSER_CMD`: pass the headless browser to exercise the inline login
/// (no token yet), or `"false"` to PROVE a second run reuses the cached token (the
/// closure would exit nonzero if ever invoked).
async fn serve_oauth_interactive_one_call(url: &str, home: &TempHome, browser_cmd: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_toonfmt"))
        .args(["--http", url, "--oauth-interactive"])
        .env("HOME", home.path())
        .env("TOONFMT_BROWSER_CMD", browser_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn toonfmt --http --oauth-interactive");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // (1) initialize — for the no-token case this only returns AFTER the inline
    // authorization-code flow completes, so the headless browser must have run.
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\
              \"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\
              \"clientInfo\":{\"name\":\"oauth-interactive-e2e\",\"version\":\"0.0.0\"}}}\n",
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let init = read_responses(&mut stdout, &[1]).await;
    assert_eq!(
        init[&1]["result"]["protocolVersion"], "2025-03-26",
        "initialize must complete over the interactive OAuth serve path"
    );

    // (2) initialized, then one tools/call.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\
              \"params\":{\"name\":\"probe_content_only\",\"arguments\":{}}}\n",
        )
        .await
        .unwrap();
    stdin.flush().await.unwrap();

    let resp = read_responses(&mut stdout, &[2]).await;
    let out = resp[&2].clone();

    drop(stdin);
    let mut leftover = String::new();
    let _ =
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_string(&mut leftover)).await;
    let _ = child.wait().await;
    out
}

/// Assert a `tools/call` response's content block is TOON (no longer JSON-parseable)
/// and still carries the payload sentinel.
fn assert_toon(resp: &Value) {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text is not a string: {resp}"));
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "content should be TOON (not JSON-parseable), got: {text:?}"
    );
    assert!(
        text.contains("CONTENT_CHANNEL"),
        "TOON must preserve the payload sentinel, got: {text:?}"
    );
}

/// The full explicit-OAuth slice assertion: login → persist → serve → TOON → reuse.
#[tokio::test]
async fn explicit_oauth_login_persist_serve_toon_reuse() {
    if !have_python3() {
        eprintln!("SKIP oauth_e2e: python3 not available on PATH");
        return;
    }

    let (mut stub, port) = spawn_oauth_stub().await;
    let url = format!("http://127.0.0.1:{port}/mcp");
    let home = TempHome::new();

    // --- (1) login: runs the whole authorization-code flow headlessly ---
    run_login(&url, &home).await;

    // --- (2) the token persisted to ~/.toonfmt-auth/<hash>.json ---
    let auth_dir = home.auth_dir();
    let files: Vec<_> = std::fs::read_dir(&auth_dir)
        .unwrap_or_else(|e| panic!("credential dir {auth_dir:?} should exist after login: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    assert_eq!(
        files.len(),
        1,
        "exactly one credential file should be written, found: {files:?}"
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap())
        .expect("credential file is valid JSON");
    assert_eq!(
        stored["client_id"], "stub-client-id",
        "persisted client_id should match the stub's dynamic registration"
    );
    assert!(
        stored["token_response"]["access_token"].as_str().is_some(),
        "persisted credentials must carry an access token: {stored}"
    );

    // --- (3) serve --oauth loads the token and a tools/call returns TOON ---
    let resp = serve_oauth_one_call(&url, &home).await;
    assert_toon(&resp);
    assert!(
        resp["result"].get("structuredContent").is_none(),
        "probe_content_only has no structuredContent"
    );

    // --- (4) a SECOND serve reuses the cached token (no re-login / browser) ---
    // TOONFMT_BROWSER_CMD=false inside serve_oauth_one_call would make any browser
    // launch a nonzero-exit divergence; a clean TOON result proves reuse with no
    // re-auth hop.
    let resp2 = serve_oauth_one_call(&url, &home).await;
    assert_toon(&resp2);

    stub.kill().await.ok();
}

/// Slice C / C1: the **interactive** serve path runs the authorization-code flow
/// inline when no token is stored (auto-launching the browser), completes it before
/// `initialize`, and returns TOON — then a second interactive serve reuses the
/// cached token without touching the browser.
///
/// This is C1's keeper proof: it never calls `login` separately. The first
/// `--oauth-interactive` serve, against a fresh (empty) HOME, must itself drive the
/// browser closure to mint the token; the second must NOT (its browser cmd is
/// `false`, which would diverge if invoked).
#[tokio::test]
async fn interactive_oauth_serve_logs_in_then_reuses() {
    if !have_python3() {
        eprintln!("SKIP oauth_e2e: python3 not available on PATH");
        return;
    }

    let (mut stub, port) = spawn_oauth_stub().await;
    let url = format!("http://127.0.0.1:{port}/mcp");
    let home = TempHome::new();

    // Sanity: no token stored yet — the interactive serve must mint one itself.
    assert!(
        !home.auth_dir().exists(),
        "fresh HOME must have no credential store before interactive serve"
    );

    // --- (1) first interactive serve with NO token: runs the inline flow headlessly,
    // completes auth before initialize, returns TOON ---
    let headless = format!(
        "python3 {}/tests/fixtures/headless_browser.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let resp = serve_oauth_interactive_one_call(&url, &home, &headless).await;
    assert_toon(&resp);

    // The inline flow persisted exactly one credential file (same store the
    // explicit `login` path writes).
    let files: Vec<_> = std::fs::read_dir(home.auth_dir())
        .expect("credential dir should exist after interactive login")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    assert_eq!(
        files.len(),
        1,
        "interactive serve should persist exactly one credential file, found: {files:?}"
    );

    // --- (2) second interactive serve REUSES the token: browser cmd is `false`, so
    // any login attempt would exit nonzero and the flow would diverge. A clean TOON
    // result proves the stored token was loaded without re-auth. ---
    let resp2 = serve_oauth_interactive_one_call(&url, &home, "false").await;
    assert_toon(&resp2);

    stub.kill().await.ok();
}
