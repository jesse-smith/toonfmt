//! `toonfmt` — a transparent MCP stdio proxy.
//!
//! Phase 1: passthrough only. Spawns the upstream server (everything after `--`)
//! and pumps JSON-RPC both directions unchanged. The TOON transform lands later.

mod cli;
mod http_upstream;
mod jsonrpc;
mod proxy;
mod transform;

use std::process::ExitCode;

use anyhow::Result;

use cli::{Command, HttpAuth, Upstream};

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
        // `toonfmt login --http <url>`: run the one-shot OAuth flow, persist the
        // tokens, exit. Wired in B4.
        Command::Login(_args) => {
            anyhow::bail!("`toonfmt login` is not yet implemented (OAuth lands in Slice B / B4)")
        }
    }
}

async fn serve(upstream: Upstream) -> Result<ExitCode> {
    match upstream {
        Upstream::Stdio(cmd) => {
            let status = proxy::run(cmd).await?;
            // Propagate the child's exit code where possible.
            let code = status.code().unwrap_or(1);
            Ok(ExitCode::from(code as u8))
        }
        // HTTP upstream. Bearer: resolve the token (fail-fast on a misconfigured
        // `--bearer-env`) before connecting. OAuth: load the cached token from the
        // credential store (wired in B4) — the serve path never launches a browser.
        Upstream::Http(http) => match &http.auth {
            HttpAuth::OAuth => {
                anyhow::bail!(
                    "OAuth serve path is not yet wired (Slice B / B4); run `toonfmt login --http {}` \
                     once it lands",
                    http.url
                )
            }
            HttpAuth::None | HttpAuth::Bearer { .. } => {
                let bearer = http.resolve_bearer()?;
                http_upstream::run(http, bearer).await?;
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
