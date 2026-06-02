//! OAuth 2.1 authorization-code flow driver for the explicit `toonfmt login`.
//!
//! **Division of labour (rmcp 1.7.0, pinned in the B3 spike):** rmcp's `auth`
//! feature owns the *protocol* — metadata discovery, dynamic client registration,
//! PKCE, the code↔token exchange, and token refresh — exposed through the
//! `OAuthState` state machine. This module owns the *lifecycle/UX* rmcp leaves to
//! the caller:
//!   1. bind a localhost listener and derive the `redirect_uri`;
//!   2. drive `OAuthState` (start → get URL → callback → exchange);
//!   3. open the user's browser at the authorization URL (an **injected** closure,
//!      so CI never needs a human or a real browser);
//!   4. receive the `code`+`state` on the loopback callback;
//!   5. persist the resulting credentials to a [`CredentialStore`] so the separate
//!      `serve` process can load them.
//!
//! Persistence note: `OAuthState::new` builds its `AuthorizationManager` with the
//! default in-memory store and the enum is `#[non_exhaustive]`, so we can't inject
//! our file store into *it*. Instead we extract the credentials after the exchange
//! (`get_credentials()` + `get_current_scopes()`) and `save()` them ourselves. The
//! serve path (B4) rebuilds a manager with our store wired in, so rmcp's auto-refresh
//! then persists rotated tokens through it.

// The driver is exercised by its unit tests (and the B5 e2e), but the *binary*
// doesn't call `login` until B4 wires the `login` subcommand. B4 removes this allow.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow, bail};
use rmcp::transport::{CredentialStore, StoredCredentials};
use rmcp::transport::auth::OAuthState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Scopes requested at authorization. Empty = let rmcp/​the server decide (it adds
/// `offline_access` when supported, which is what enables refresh tokens).
const SCOPES: &[&str] = &[];

/// The client name advertised during dynamic client registration.
const CLIENT_NAME: &str = "toonfmt";

/// Run the full authorization-code flow for `url`, persisting credentials to `store`.
///
/// `open_browser` is invoked with the authorization URL; in production it shells out
/// to the platform opener, in tests it performs the redirect itself (a bare GET).
/// Returns the persisted [`StoredCredentials`] for inspection.
pub async fn login<S, B>(url: &str, store: &S, open_browser: B) -> Result<StoredCredentials>
where
    S: CredentialStore,
    B: FnOnce(&str) -> Result<()>,
{
    // (1) Bind the loopback callback listener first so we know the port for the
    // redirect_uri before registering the client (the URI is registered with the
    // authorization server, so it must be final before `start_authorization`).
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding localhost OAuth callback listener")?;
    let port = listener
        .local_addr()
        .context("reading callback listener port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // (2) Discovery + dynamic registration + PKCE session. `None` http client → rmcp
    // builds its own; the login process needs no direct reqwest dependency.
    let mut oauth = OAuthState::new(url, None)
        .await
        .map_err(|e| anyhow!("initializing OAuth state for {url}: {e}"))?;
    oauth
        .start_authorization(SCOPES, &redirect_uri, Some(CLIENT_NAME))
        .await
        .map_err(|e| anyhow!("starting OAuth authorization (discovery/registration): {e}"))?;
    let auth_url = oauth
        .get_authorization_url()
        .await
        .map_err(|e| anyhow!("building authorization URL: {e}"))?;

    // (3) Hand the URL to the user's browser (or the test closure).
    open_browser(&auth_url).context("opening the authorization URL")?;

    // (4) Wait for the authorization server to redirect back to our loopback.
    let (code, state) = accept_callback(&listener)
        .await
        .context("receiving the OAuth callback on localhost")?;

    // (5) Exchange the code for a token (rmcp drives PKCE + the token endpoint).
    oauth
        .handle_callback(&code, &state)
        .await
        .map_err(|e| anyhow!("exchanging the authorization code for a token: {e}"))?;

    // Extract what was just minted and persist it to our cross-process store. The
    // manager already holds the exact StoredCredentials internally, but its store is
    // private and `OAuthState` is non-exhaustive, so we take the manager out
    // (`into_authorization_manager`, valid only in the Authorized state we're now in)
    // and read the public getters — faithful: client_id + token are exact, scopes
    // come from the manager, and "received now" is accurate since the exchange just
    // completed.
    let manager = oauth
        .into_authorization_manager()
        .ok_or_else(|| anyhow!("OAuth state was not Authorized after callback (internal)"))?;
    let (client_id, token_response) = manager
        .get_credentials()
        .await
        .map_err(|e| anyhow!("reading minted credentials: {e}"))?;
    if token_response.is_none() {
        bail!("authorization completed but no token was returned by the server");
    }
    let granted_scopes = manager.get_current_scopes().await;
    let credentials = StoredCredentials::new(
        client_id,
        token_response,
        granted_scopes,
        Some(now_epoch_secs()),
    );
    store
        .save(credentials.clone())
        .await
        .map_err(|e| anyhow!("persisting credentials: {e}"))?;

    Ok(credentials)
}

/// Accept exactly one HTTP request on the loopback listener, parse the OAuth
/// `code`+`state` from its query string, send a minimal browser-facing response,
/// and return `(code, state)`.
async fn accept_callback(listener: &TcpListener) -> Result<(String, String)> {
    let (mut stream, _peer) = listener
        .accept()
        .await
        .context("accepting the callback connection")?;

    let target = read_request_target(&mut stream).await?;
    let parsed = parse_callback_query(&target);

    // Always answer the browser so the user isn't left on a hung tab, even on error.
    let (status, body) = match &parsed {
        Ok(_) => (
            "200 OK",
            "toonfmt: authorization complete. You can close this tab and return to the terminal.",
        ),
        Err(_) => (
            "400 Bad Request",
            "toonfmt: authorization callback was missing the expected code/state parameters.",
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("writing the callback browser response")?;
    stream.flush().await.ok();

    parsed
}

/// Read the request line of an HTTP request from `stream` and return its target
/// (the `path?query` between the method and the version). Bounded so a misbehaving
/// client can't stream forever.
async fn read_request_target(stream: &mut TcpStream) -> Result<String> {
    // The request line ends at the first CRLF; reading the head is enough.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .context("reading the callback HTTP request")?;
        if n == 0 {
            break; // EOF before a full request line
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(2).any(|w| w == b"\r\n") || buf.len() > 8192 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let request_line = head
        .lines()
        .next()
        .ok_or_else(|| anyhow!("empty callback request"))?;
    // "GET /callback?code=…&state=… HTTP/1.1"
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed callback request line: {request_line:?}"))?;
    Ok(target.to_string())
}

/// Extract `code` and `state` from a callback request target's query string. Both
/// are required (the spec mandates `state` echo for CSRF protection); a missing
/// either, or an `error=` response from the server, is a failure.
fn parse_callback_query(target: &str) -> Result<(String, String)> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut server_error = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => server_error = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(err) = server_error {
        bail!("authorization server returned an error: {err}");
    }
    match (code, state) {
        (Some(code), Some(state)) => Ok((code, state)),
        _ => bail!("callback missing required `code`/`state` parameters: {target:?}"),
    }
}

/// Seconds since the Unix epoch (matches rmcp's `token_received_at` convention).
fn now_epoch_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- query parsing (the part we own; the OAuthState dance is e2e-tested in B5) ---

    #[test]
    fn parses_code_and_state() {
        let (code, state) =
            parse_callback_query("/callback?code=abc123&state=xyz789").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz789");
    }

    #[test]
    fn parses_url_encoded_values() {
        let (code, state) =
            parse_callback_query("/callback?code=a%2Fb%2Bc&state=s%20t").unwrap();
        assert_eq!(code, "a/b+c");
        assert_eq!(state, "s t");
    }

    #[test]
    fn extra_params_are_ignored() {
        let (code, state) =
            parse_callback_query("/callback?state=s&iss=https://x&code=c&scope=mcp").unwrap();
        assert_eq!(code, "c");
        assert_eq!(state, "s");
    }

    #[test]
    fn missing_code_or_state_errors() {
        assert!(parse_callback_query("/callback?code=only").is_err());
        assert!(parse_callback_query("/callback?state=only").is_err());
        assert!(parse_callback_query("/callback").is_err());
    }

    #[test]
    fn server_error_response_is_surfaced() {
        let e = parse_callback_query("/callback?error=access_denied&state=s").unwrap_err();
        assert!(e.to_string().contains("access_denied"), "got: {e}");
    }

    // --- the loopback listener round-trip (also ours) ---

    #[tokio::test]
    async fn accept_callback_reads_code_and_state_and_answers_browser() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server side: accept one callback.
        let server = tokio::spawn(async move { accept_callback(&listener).await });

        // Client side: act as the browser hitting the redirect_uri.
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client
            .write_all(b"GET /callback?code=THECODE&state=THESTATE HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        client.flush().await.unwrap();

        // The browser must get a response (not a hung tab).
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp:?}");
        assert!(resp.contains("authorization complete"), "got: {resp:?}");

        let (code, state) = server.await.unwrap().unwrap();
        assert_eq!(code, "THECODE");
        assert_eq!(state, "THESTATE");
    }
}
