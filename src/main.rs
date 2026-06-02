//! `toonfmt` — a transparent MCP stdio proxy.
//!
//! Phase 1: passthrough only. Spawns the upstream server (everything after `--`)
//! and pumps JSON-RPC both directions unchanged. The TOON transform lands later.

mod cli;
mod jsonrpc;
mod proxy;
mod transform;

use std::process::ExitCode;

use anyhow::{Result, bail};

use cli::Upstream;

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
        Upstream::Stdio(cmd) => {
            let status = proxy::run(cmd).await?;
            // Propagate the child's exit code where possible.
            let code = status.code().unwrap_or(1);
            Ok(ExitCode::from(code as u8))
        }
        // Wired in A4 (HTTP driver). Resolve the bearer token now so a
        // misconfigured `--bearer-env` fails fast even before the driver lands.
        Upstream::Http(http) => {
            let _bearer = http.resolve_bearer()?;
            bail!("HTTP upstream (`--http`) is not wired yet; lands in task A4");
        }
    }
}
