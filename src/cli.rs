//! Command-line argument parsing.
//!
//! Two upstream shapes:
//!   - **stdio** (the original): `[flags] -- <program> <args...>` — everything
//!     after `--` is the command toonfmt spawns and pumps over its pipes.
//!   - **HTTP** (this phase): `--http <url> [--bearer-env <VAR>]` — toonfmt
//!     connects to a Streamable HTTP MCP server instead of spawning a child.
//!
//! Exactly one shape must be selected: `--http` and `--` are mutually exclusive,
//! and at least one is required. Flags other than `--http`/`--bearer-env` before
//! `--` remain reserved for later phases and are ignored.

use anyhow::{Result, bail};

/// The upstream MCP server command to spawn (stdio transport).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamCmd {
    pub program: String,
    pub args: Vec<String>,
}

/// An HTTP (Streamable HTTP) MCP upstream. `bearer_env` is the *name* of an
/// environment variable holding the bearer token — never the token itself, and
/// never read from argv (which would leak it into the process list). Resolution
/// to the actual token happens at startup via [`HttpUpstream::resolve_bearer`],
/// with fail-fast semantics. Shaped to let Slice B add OAuth selectors without
/// re-touching the [`Upstream`] enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUpstream {
    pub url: String,
    pub bearer_env: Option<String>,
}

impl HttpUpstream {
    /// Resolve the bearer token from the named env var, fail-fast.
    ///
    /// - `bearer_env: None` → `Ok(None)` (no auth header).
    /// - `bearer_env: Some(var)` and the var is set → `Ok(Some(token))`.
    /// - `bearer_env: Some(var)` and the var is unset/empty → **error**. A
    ///   misspelled or unset var must not silently degrade to an unauthenticated
    ///   request (which would surface as a confusing upstream 401); it stops here.
    pub fn resolve_bearer(&self) -> Result<Option<String>> {
        let Some(var) = self.bearer_env.as_deref() else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(token) if !token.is_empty() => Ok(Some(token)),
            Ok(_) => bail!("--bearer-env {var}: environment variable is set but empty"),
            Err(_) => bail!("--bearer-env {var}: environment variable is not set"),
        }
    }
}

/// The selected upstream: stdio child process or HTTP MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    Stdio(UpstreamCmd),
    Http(HttpUpstream),
}

/// Parse arguments into an [`Upstream`].
///
/// `args` should be the process arguments *excluding* argv[0]. Selection:
///   - `--http <url>` (optionally `--bearer-env <VAR>`) → [`Upstream::Http`].
///   - `-- <program> [args...]` → [`Upstream::Stdio`].
///
/// Errors if both shapes are given (ambiguous), if neither is given, or if a
/// flag is missing its value.
pub fn parse_args(args: impl Iterator<Item = String>) -> Result<Upstream> {
    let mut http_url: Option<String> = None;
    let mut bearer_env: Option<String> = None;
    let mut after_sep: Option<Vec<String>> = None;

    let mut it = args;
    while let Some(arg) = it.next() {
        if let Some(rest) = after_sep.as_mut() {
            // Already past `--`: collect the rest verbatim.
            rest.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => after_sep = Some(Vec::new()),
            "--http" => {
                let Some(url) = it.next() else {
                    bail!("--http requires a URL argument");
                };
                http_url = Some(url);
            }
            "--bearer-env" => {
                let Some(var) = it.next() else {
                    bail!("--bearer-env requires a variable-name argument");
                };
                bearer_env = Some(var);
            }
            // Other pre-`--` tokens are reserved for later-phase flags; ignore.
            _ => {}
        }
    }

    match (http_url, after_sep) {
        (Some(_), Some(_)) => {
            bail!("ambiguous: pass either `--http <url>` or `-- <program>`, not both")
        }
        (Some(url), None) => Ok(Upstream::Http(HttpUpstream { url, bearer_env })),
        (None, Some(after)) => {
            if bearer_env.is_some() {
                bail!("--bearer-env applies to `--http` upstreams, not the stdio `--` form");
            }
            let mut after = after.into_iter();
            let Some(program) = after.next() else {
                bail!(
                    "no upstream command after `--`; usage: toonfmt [flags] -- <program> [args...]"
                );
            };
            Ok(Upstream::Stdio(UpstreamCmd {
                program,
                args: after.collect(),
            }))
        }
        (None, None) => bail!(
            "no upstream selected; usage: toonfmt --http <url> [--bearer-env VAR] | toonfmt -- <program> [args...]"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> Result<Upstream> {
        parse_args(tokens.iter().map(|s| s.to_string()))
    }

    fn stdio(tokens: &[&str]) -> UpstreamCmd {
        match parse(tokens).unwrap() {
            Upstream::Stdio(cmd) => cmd,
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    fn http(tokens: &[&str]) -> HttpUpstream {
        match parse(tokens).unwrap() {
            Upstream::Http(h) => h,
            other => panic!("expected Http, got {other:?}"),
        }
    }

    // --- stdio shape (the original 5 tests, adapted to the enum) ---

    #[test]
    fn well_formed_program_and_args() {
        let cmd = stdio(&["--", "uvx", "some-mcp", "--db", "x"]);
        assert_eq!(cmd.program, "uvx");
        assert_eq!(cmd.args, vec!["some-mcp", "--db", "x"]);
    }

    #[test]
    fn well_formed_program_only() {
        let cmd = stdio(&["--", "cat"]);
        assert_eq!(cmd.program, "cat");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn flags_before_separator_are_ignored() {
        let cmd = stdio(&["--skip-tool", "foo", "--", "cat"]);
        assert_eq!(cmd.program, "cat");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn missing_separator_and_no_http_errors() {
        // Neither `--http` nor `--`: nothing selected.
        assert!(parse(&["uvx", "some-mcp"]).is_err());
    }

    #[test]
    fn empty_after_separator_errors() {
        assert!(parse(&["--"]).is_err());
    }

    // --- HTTP shape (A2: tests a–f) ---

    /// (a) `--http <url>` → Http with no bearer env.
    #[test]
    fn http_url_only() {
        let h = http(&["--http", "https://x.example/mcp"]);
        assert_eq!(h.url, "https://x.example/mcp");
        assert_eq!(h.bearer_env, None);
    }

    /// (b) `--http <url> --bearer-env TOK` → bearer_env captured (the var name).
    #[test]
    fn http_url_with_bearer_env() {
        let h = http(&["--http", "https://x.example/mcp", "--bearer-env", "TOK"]);
        assert_eq!(h.url, "https://x.example/mcp");
        assert_eq!(h.bearer_env, Some("TOK".to_string()));
    }

    /// (d) `--http <url> -- cat` → ambiguous → error.
    #[test]
    fn http_and_stdio_is_ambiguous() {
        assert!(parse(&["--http", "https://x.example/mcp", "--", "cat"]).is_err());
    }

    /// (e) neither shape → error.
    #[test]
    fn neither_shape_errors() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--skip-tool", "foo"]).is_err());
    }

    /// Missing flag values error rather than silently mis-parsing.
    #[test]
    fn dangling_flags_error() {
        assert!(parse(&["--http"]).is_err());
        assert!(parse(&["--http", "https://x", "--bearer-env"]).is_err());
    }

    /// (f) bearer resolution is fail-fast: var present → token; absent/empty → error.
    /// Uses a process-unique var name so the test is independent of the environment.
    #[test]
    fn bearer_env_resolves_fail_fast() {
        let var = "TOONFMT_TEST_BEARER_A2";
        let h = HttpUpstream {
            url: "https://x.example/mcp".to_string(),
            bearer_env: Some(var.to_string()),
        };

        // Absent → error.
        unsafe { std::env::remove_var(var) };
        assert!(h.resolve_bearer().is_err(), "unset var must fail fast");

        // Empty → error (set-but-empty is still a misconfiguration).
        unsafe { std::env::set_var(var, "") };
        assert!(h.resolve_bearer().is_err(), "empty var must fail fast");

        // Present → resolved.
        unsafe { std::env::set_var(var, "secret-token") };
        assert_eq!(h.resolve_bearer().unwrap(), Some("secret-token".to_string()));

        // No bearer_env → Ok(None), no auth.
        unsafe { std::env::remove_var(var) };
        let none = HttpUpstream {
            url: "https://x.example/mcp".to_string(),
            bearer_env: None,
        };
        assert_eq!(none.resolve_bearer().unwrap(), None);
    }
}
