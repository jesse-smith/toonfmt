//! `toonfmt` — a transparent MCP stdio proxy.
//!
//! Phase 1: passthrough only. Spawns the upstream server (everything after `--`)
//! and pumps JSON-RPC both directions unchanged. The TOON transform lands later.

mod cli;
mod credential_store;
mod http_upstream;
mod oauth;
mod jsonrpc;
mod proxy;
mod transform;
mod update;

use std::process::ExitCode;

use anyhow::{Context, Result};

use cli::{Command, HttpAuth, HttpUpstream, LoginArgs, Upstream};
use credential_store::FileCredentialStore;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr only; stdout is the protocol channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match run().await {
        Ok(code) => code,
        Err(e) => {
            // anyhow chain to stderr, nonzero exit.
            eprintln!("toonfmt: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    match cli::parse_args(std::env::args().skip(1))? {
        Command::Serve(upstream) => serve(upstream).await,
        Command::Login(args) => login(args).await,
        // `run_update` is a sync fn (the axoupdater `blocking` feature), but
        // `run_sync` calls `block_on` *internally* — which panics if invoked on a
        // thread already driving a runtime, and `main` is `#[tokio::main]`. So run
        // it on a blocking thread where no runtime is active. It returns `Ok(())`
        // for the graceful no-receipt / already-current cases, `Err` only on real
        // failures.
        Command::Update => {
            tokio::task::spawn_blocking(update::run_update)
                .await
                .context("update task panicked")?
                .map(|()| ExitCode::SUCCESS)
        }
        // Meta commands: print to stdout (the human front door, not the protocol
        // channel — but help/version never coexist with a serve session, so stdout
        // is correct and conventional here) and exit 0.
        Command::Help => {
            println!("{}", cli::USAGE);
            Ok(ExitCode::SUCCESS)
        }
        Command::Version => {
            println!("toonfmt {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `toonfmt login --http <url>`: run the one-shot OAuth authorization-code flow,
/// persist the tokens to the credential store, and exit. The browser is opened by
/// the platform opener; progress/success go to **stderr** (stdout is reserved for
/// the protocol on the serve path, and keeping login's output on stderr too means
/// the two never disagree about which stream is for humans).
async fn login(args: LoginArgs) -> Result<ExitCode> {
    let store = FileCredentialStore::for_url(&args.url)?;
    eprintln!("toonfmt: starting OAuth login for {}", args.url);
    let creds = oauth::login(&args.url, &store, open_in_browser).await?;
    eprintln!(
        "toonfmt: login succeeded (client_id={}); credentials saved to {}",
        creds.client_id,
        store.path().display(),
    );
    eprintln!("toonfmt: you can now serve with `--http {} --oauth`", args.url);
    Ok(ExitCode::SUCCESS)
}

async fn serve(upstream: Upstream) -> Result<ExitCode> {
    match upstream {
        Upstream::Stdio(cmd) => {
            let status = proxy::run(cmd).await?;
            // Propagate the child's exit code where possible.
            let code = status.code().unwrap_or(1);
            Ok(ExitCode::from(code as u8))
        }
        Upstream::Http(http) => serve_http(http).await,
    }
}

/// Construct the right concrete rmcp transport for the chosen auth mode, then run
/// the shared generic driver. Auth selection is a **construction-time** branch here
/// — `http_upstream::run` is generic over the transport and never sees auth mode.
async fn serve_http(http: HttpUpstream) -> Result<ExitCode> {
    let config = StreamableHttpClientTransportConfig::with_uri(http.url.clone());
    match &http.auth {
        // OAuth: load the cached token (fail-fast if absent — never launches a
        // browser on the serve path) and wrap it in an AuthClient transport.
        HttpAuth::OAuth => {
            let store = FileCredentialStore::for_url(&http.url)?;
            let auth_client = oauth::serve_auth_client(&http.url, store).await?;
            let transport = StreamableHttpClientTransport::with_client(auth_client, config);
            http_upstream::run(transport).await?;
        }
        // Interactive OAuth (Slice C): same as OAuth when a token is stored; when
        // none is, run the authorization-code flow inline (auto-launch the browser)
        // *before* `initialize`. Opt-in only — it can block the host's startup on a
        // human at the consent page.
        HttpAuth::OAuthInteractive => {
            let store = FileCredentialStore::for_url(&http.url)?;
            let auth_client =
                oauth::serve_auth_client_interactive(&http.url, store, open_in_browser_logged)
                    .await?;
            let transport = StreamableHttpClientTransport::with_client(auth_client, config);
            http_upstream::run(transport).await?;
        }
        // Bearer / no-auth: resolve the token (fail-fast on a misconfigured
        // `--bearer-env`) and set it as the static auth header.
        HttpAuth::None | HttpAuth::Bearer { .. } => {
            let config = match http.resolve_bearer()? {
                Some(token) => config.auth_header(token),
                None => config,
            };
            let transport = StreamableHttpClientTransport::from_config(config);
            http_upstream::run(transport).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Open `url` in the user's default browser by shelling out to the platform opener.
/// No crate dependency (the `open` crate isn't in the offline cargo cache); this is
/// the production `open_browser` closure for [`oauth::login`].
///
/// **Spawn-and-return, don't wait:** opening a browser is fire-and-forget — the
/// opener hands off to the windowing system and the user authorizes asynchronously
/// while [`oauth::login`] blocks on its loopback callback. Waiting on the child
/// would deadlock if the launched process only returns after the callback completes
/// (which the e2e's headless browser command does). We surface launch failures
/// (command not found) but not the browser's own exit status.
///
/// **Override:** if `TOONFMT_BROWSER_CMD` is set, that command is run instead of the
/// platform opener (with `url` appended as its final argument). Useful for users on
/// an unusual setup — and it's the seam the e2e uses to drive the flow headlessly.
fn open_in_browser(url: &str) -> Result<()> {
    if let Some(custom) = std::env::var_os("TOONFMT_BROWSER_CMD") {
        let custom = custom.to_string_lossy();
        // Split on whitespace so callers can pass `program arg1 arg2`.
        let mut parts = custom.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("TOONFMT_BROWSER_CMD is set but empty"))?;
        std::process::Command::new(program)
            .args(parts)
            .arg(url)
            .spawn()
            .with_context(|| format!("launching TOONFMT_BROWSER_CMD `{custom}`"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, &[&str]) = ("open", &[]);
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, &[&str]) = ("cmd", &["/C", "start", ""]);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let (cmd, args): (&str, &[&str]) = ("xdg-open", &[]);

    std::process::Command::new(cmd)
        .args(args)
        .arg(url)
        .spawn()
        .with_context(|| format!("launching `{cmd}` to open the browser"))?;
    Ok(())
}

/// [`open_in_browser`] with the authorization URL also echoed to **stderr** first.
///
/// The interactive serve path's primary UX is the auto-launched browser — nothing
/// for the user to read. But B6 measured that Claude Code captures a stdio
/// subprocess's stderr verbatim into a per-server cache log (not the `/mcp` menu),
/// so echoing the URL is a zero-cost fallback: if the auto-open fails or the user is
/// debugging, the clickable URL is recoverable from that log. Used only by
/// `--oauth-interactive`; the explicit `login` path keeps the bare opener.
fn open_in_browser_logged(url: &str) -> Result<()> {
    eprintln!("toonfmt: authorize at: {url}");
    open_in_browser(url)
}
