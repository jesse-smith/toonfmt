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
fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, &[&str]) = ("open", &[]);
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, &[&str]) = ("cmd", &["/C", "start", ""]);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let (cmd, args): (&str, &[&str]) = ("xdg-open", &[]);

    let status = std::process::Command::new(cmd)
        .args(args)
        .arg(url)
        .status()
        .with_context(|| format!("launching `{cmd}` to open the browser"))?;
    if !status.success() {
        anyhow::bail!("`{cmd}` exited with {status} while opening the browser");
    }
    Ok(())
}
