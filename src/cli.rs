//! Command-line argument parsing.
//!
//! Two top-level commands:
//!   - **serve** (the default, no subcommand): run the proxy. Selects one upstream
//!     shape — **stdio** (`[flags] -- <program> <args...>`) or **HTTP**
//!     (`--http <url> [--bearer-env <VAR>] [--oauth | --oauth-interactive]`).
//!   - **login** (`login --http <url>`): run the OAuth authorization-code flow for
//!     an HTTP upstream once, persist the tokens, and exit. The serve path then
//!     loads those tokens — with `--oauth` it never launches a browser, so
//!     `initialize` is never blocked on a human.
//!
//! For the serve/HTTP shape, exactly one auth *family* may be given: `--bearer-env`
//! (static token) is mutually exclusive with the OAuth flags. `--oauth` and
//! `--oauth-interactive` are the same authorization-code grant differing only in the
//! missing-token case (fail-fast vs. auto-launch the browser at serve time); giving
//! both resolves to interactive. `--http` and `--` are mutually exclusive, and one
//! is required.

use anyhow::{Result, bail};

/// Canonical usage text for the `--help` front door.
///
/// **Additive (Fork C):** the scattered contextual `bail!` diagnostics elsewhere
/// in this module stay exactly as they read — they are fail-fast specifics
/// ("`--bearer-env` applies to `--http` upstreams…"), not generic usage fragments.
/// This block is the one canonical thing `--help` prints; it does not replace them.
pub const USAGE: &str = "\
toonfmt — a transparent MCP stdio proxy that reshapes tool-result JSON to TOON.

USAGE:
    toonfmt [flags] -- <program> [args...]    serve a stdio upstream
    toonfmt --http <url> [auth]               serve an HTTP (Streamable HTTP) upstream
    toonfmt login --http <url>                run the OAuth flow once, persist tokens
    toonfmt update                            self-update an installer-based build
    toonfmt --help | --version

AUTH (HTTP upstreams only):
    --bearer-env <VAR>     static bearer token, read from environment variable <VAR>
    --oauth                use tokens from a prior `toonfmt login` (fail-fast if absent)
    --oauth-interactive    like --oauth, but run the browser flow at serve time if absent
    --profile <name>       namespace OAuth credentials (must match on login + serve);
                           use --profile ${CLAUDE_PROJECT_DIR} for project-scoped tokens

OPTIONS:
    -h, --help             print this help and exit
    -V, --version          print version and exit

Everything after `--` is the upstream program and its argv, forwarded byte-verbatim.";

/// The upstream MCP server command to spawn (stdio transport).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamCmd {
    pub program: String,
    pub args: Vec<String>,
}

/// How an HTTP upstream authenticates. Bearer-vs-OAuth is mutually exclusive at the
/// type level (a value is exactly one of these), which is why the auth selector
/// lives here rather than as independent `Option` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpAuth {
    /// No auth header (public upstream).
    None,
    /// Static bearer token, resolved at startup from the named env var (never the
    /// token on argv). `env` is the *variable name*.
    Bearer { env: String },
    /// OAuth 2.1 authorization-code grant. The token is obtained out-of-band by
    /// `toonfmt login` and loaded from the credential store at serve time.
    OAuth,
    /// OAuth 2.1, but the serve path itself runs the authorization-code flow when
    /// no token is stored (auto-launching the browser), instead of fail-fast. A
    /// superset of [`HttpAuth::OAuth`]: a present token is reused identically; only
    /// the missing-token case differs (interactive login vs. error). Gated behind
    /// the explicit `--oauth-interactive` opt-in because it can block `initialize`
    /// on a human — acceptable only when the user asked for it.
    OAuthInteractive,
}

/// An HTTP (Streamable HTTP) MCP upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUpstream {
    pub url: String,
    pub auth: HttpAuth,
    /// Optional credential-store namespace (Fork A — part of the connection
    /// identity, parallel to `url`). `None` = the shared default key
    /// `sha256(url)`; `Some(p)` keys the store by `sha256(p ⊕ "\0" ⊕ url)`, so
    /// distinct identities for one URL coexist. Only the OAuth auth modes read the
    /// store, so a `Some` here with non-OAuth auth is rejected at parse time
    /// (Fork B) — the field is never a silent dead value.
    pub profile: Option<String>,
}

impl HttpUpstream {
    /// Resolve the bearer token, fail-fast — meaningful only for [`HttpAuth::Bearer`].
    ///
    /// - [`HttpAuth::None`] / [`HttpAuth::OAuth`] → `Ok(None)` (the OAuth token is
    ///   loaded from the credential store on the serve path, not here).
    /// - [`HttpAuth::Bearer`] and the var is set non-empty → `Ok(Some(token))`.
    /// - [`HttpAuth::Bearer`] and the var is unset/empty → **error**. A misspelled
    ///   or unset var must not silently degrade to an unauthenticated request
    ///   (which would surface as a confusing upstream 401); it stops here.
    pub fn resolve_bearer(&self) -> Result<Option<String>> {
        let HttpAuth::Bearer { env } = &self.auth else {
            return Ok(None);
        };
        match std::env::var(env) {
            Ok(token) if !token.is_empty() => Ok(Some(token)),
            Ok(_) => bail!("--bearer-env {env}: environment variable is set but empty"),
            Err(_) => bail!("--bearer-env {env}: environment variable is not set"),
        }
    }
}

/// The selected upstream: stdio child process or HTTP MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    Stdio(UpstreamCmd),
    Http(HttpUpstream),
}

/// Arguments for the `login` subcommand: run the OAuth flow for an HTTP upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginArgs {
    pub url: String,
    /// Credential-store namespace — must match the `--profile` used on the serve
    /// path (extends the URL-match gotcha to "URL **and** profile"). `None` =
    /// shared default. See [`HttpUpstream::profile`].
    pub profile: Option<String>,
}

/// Top-level command. `serve` is the default (no subcommand); `login` runs the
/// one-shot OAuth flow; `update` self-updates an installer-based build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve(Upstream),
    Login(LoginArgs),
    /// `toonfmt update`: self-update via the install receipt. Takes no arguments.
    Update,
    /// `--help` / `-h` / `help`: print [`USAGE`] to stdout and exit 0.
    Help,
    /// `--version` / `-V`: print the crate version to stdout and exit 0.
    Version,
}

/// Is `tok` a meta *flag* (`--help`/`-h`/`--version`/`-V`)?
///
/// Recognized **only among pre-`--` tokens** (Gap E): everything after `--` is
/// the upstream program's argv and is forwarded verbatim, so this is never
/// consulted past the separator. Each parse loop calls this at the top of its
/// iteration, *before* matching value-taking flags — so a value already consumed
/// by the parser (e.g. the `<VAR>` of `--bearer-env <VAR>`) is never re-examined
/// and can't be misread as a meta flag. The bare word `help` is handled
/// separately as a first-token subcommand (it isn't a flag and would collide with
/// a legitimate flag value if matched anywhere).
fn meta_command(tok: &str) -> Option<Command> {
    match tok {
        "--help" | "-h" => Some(Command::Help),
        "--version" | "-V" => Some(Command::Version),
        _ => None,
    }
}

/// Parse process arguments (excluding argv[0]) into a [`Command`].
///
/// Grammar:
///   - `login --http <url>` → [`Command::Login`].
///   - `update` → [`Command::Update`] (no arguments).
///   - `--http <url> [--bearer-env <VAR> | --oauth]` → [`Command::Serve`] HTTP.
///   - `-- <program> [args...]` → [`Command::Serve`] stdio.
///
/// Errors on: both upstream shapes, neither shape, a flag missing its value, both
/// auth selectors together, auth flags applied to the stdio form, or any argument
/// after `update`.
pub fn parse_args(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut it = args.peekable();

    // Subcommands are distinguished by the first token. Everything else is the
    // implicit `serve` command.
    match it.peek().map(String::as_str) {
        // Bare `help` as the leading word → Help. (Only here, not in
        // `meta_command`, so it can't shadow a legitimate flag value downstream.)
        Some("help") => return Ok(Command::Help),
        Some("login") => {
            it.next(); // consume `login`
            return parse_login(it);
        }
        Some("update") => {
            it.next(); // consume `update`
            // `update` takes no arguments, but `--help`/`-h` pre-empts that
            // validation (a trailing meta flag prints help rather than erroring).
            return match it.next() {
                Some(tok) => match meta_command(&tok) {
                    Some(meta) => Ok(meta),
                    None => {
                        bail!("unexpected argument to `update`: {tok} (usage: toonfmt update)")
                    }
                },
                None => Ok(Command::Update),
            };
        }
        _ => {}
    }

    parse_serve(it)
}

/// Parse `login --http <url>`. Only `--http` is accepted — login *is* the OAuth
/// flow, so no auth selector is needed (or allowed). `--help`/`-h` anywhere here
/// pre-empts validation and yields [`Command::Help`].
fn parse_login(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut url: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut it = args;
    while let Some(arg) = it.next() {
        if let Some(meta) = meta_command(&arg) {
            return Ok(meta);
        }
        match arg.as_str() {
            "--http" => {
                let Some(u) = it.next() else {
                    bail!("login --http requires a URL argument");
                };
                url = Some(u);
            }
            "--profile" => {
                let Some(p) = it.next() else {
                    bail!("--profile requires a name argument");
                };
                profile = Some(p);
            }
            other => bail!("unexpected argument to `login`: {other} (usage: toonfmt login --http <url> [--profile <name>])"),
        }
    }
    match url {
        Some(url) => Ok(Command::Login(LoginArgs { url, profile })),
        None => bail!("login requires an upstream; usage: toonfmt login --http <url> [--profile <name>]"),
    }
}

/// Parse the serve forms (HTTP or stdio). May instead return a meta
/// [`Command::Help`]/[`Command::Version`] if such a flag appears pre-`--`.
fn parse_serve(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut http_url: Option<String> = None;
    let mut bearer_env: Option<String> = None;
    let mut oauth = false;
    let mut oauth_interactive = false;
    let mut profile: Option<String> = None;
    let mut after_sep: Option<Vec<String>> = None;

    let mut it = args;
    while let Some(arg) = it.next() {
        if let Some(rest) = after_sep.as_mut() {
            // Already past `--`: collect the rest verbatim. Meta flags here belong
            // to the upstream argv (Gap E) — never inspected.
            rest.push(arg);
            continue;
        }
        // Pre-`--` meta flags pre-empt the whole serve spec (Gap E: pre-`--` only).
        if let Some(meta) = meta_command(&arg) {
            return Ok(meta);
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
            "--oauth" => oauth = true,
            "--oauth-interactive" => oauth_interactive = true,
            "--profile" => {
                let Some(p) = it.next() else {
                    bail!("--profile requires a name argument");
                };
                profile = Some(p);
            }
            // Other pre-`--` tokens are reserved for later-phase flags; ignore.
            _ => {}
        }
    }

    match (http_url, after_sep) {
        (Some(_), Some(_)) => {
            bail!("ambiguous: pass either `--http <url>` or `-- <program>`, not both")
        }
        (Some(url), None) => {
            // Bearer is mutually exclusive with either OAuth flag (static token vs.
            // authorization-code grant). `--oauth-interactive` is a superset of
            // `--oauth` (same reuse path, only the missing-token case differs), so
            // giving both is redundant-but-harmless and resolves to interactive.
            if bearer_env.is_some() && (oauth || oauth_interactive) {
                bail!("--bearer-env and --oauth/--oauth-interactive are mutually exclusive (pick one auth mode)");
            }
            let auth = match (bearer_env, oauth_interactive, oauth) {
                (Some(env), _, _) => HttpAuth::Bearer { env },
                (None, true, _) => HttpAuth::OAuthInteractive,
                (None, false, true) => HttpAuth::OAuth,
                (None, false, false) => HttpAuth::None,
            };
            // Fork B: `--profile` only namespaces the OAuth credential store, which
            // only the OAuth modes read. With bearer/no-auth it would be a silent
            // dead value — fail fast instead (mirrors the bearer/OAuth exclusivity).
            if profile.is_some() && !matches!(auth, HttpAuth::OAuth | HttpAuth::OAuthInteractive) {
                bail!("--profile applies only to OAuth upstreams (--oauth / --oauth-interactive)");
            }
            Ok(Command::Serve(Upstream::Http(HttpUpstream { url, auth, profile })))
        }
        (None, Some(after)) => {
            if bearer_env.is_some() {
                bail!("--bearer-env applies to `--http` upstreams, not the stdio `--` form");
            }
            if oauth {
                bail!("--oauth applies to `--http` upstreams, not the stdio `--` form");
            }
            if oauth_interactive {
                bail!("--oauth-interactive applies to `--http` upstreams, not the stdio `--` form");
            }
            if profile.is_some() {
                bail!("--profile applies to `--http` OAuth upstreams, not the stdio `--` form");
            }
            let mut after = after.into_iter();
            let Some(program) = after.next() else {
                bail!(
                    "no upstream command after `--`; usage: toonfmt [flags] -- <program> [args...]"
                );
            };
            Ok(Command::Serve(Upstream::Stdio(UpstreamCmd {
                program,
                args: after.collect(),
            })))
        }
        (None, None) => bail!(
            "no upstream selected; usage: toonfmt --http <url> [--bearer-env VAR | --oauth | --oauth-interactive] | toonfmt -- <program> [args...]"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> Result<Command> {
        parse_args(tokens.iter().map(|s| s.to_string()))
    }

    fn serve(tokens: &[&str]) -> Upstream {
        match parse(tokens).unwrap() {
            Command::Serve(u) => u,
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    fn stdio(tokens: &[&str]) -> UpstreamCmd {
        match serve(tokens) {
            Upstream::Stdio(cmd) => cmd,
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    fn http(tokens: &[&str]) -> HttpUpstream {
        match serve(tokens) {
            Upstream::Http(h) => h,
            other => panic!("expected Http, got {other:?}"),
        }
    }

    // --- stdio shape (the original tests, adapted to Command) ---

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

    // --- HTTP shape ---

    /// `--http <url>` → Http with no auth.
    #[test]
    fn http_url_only() {
        let h = http(&["--http", "https://x.example/mcp"]);
        assert_eq!(h.url, "https://x.example/mcp");
        assert_eq!(h.auth, HttpAuth::None);
    }

    /// `--http <url> --bearer-env TOK` → Bearer auth with the var name.
    #[test]
    fn http_url_with_bearer_env() {
        let h = http(&["--http", "https://x.example/mcp", "--bearer-env", "TOK"]);
        assert_eq!(h.url, "https://x.example/mcp");
        assert_eq!(h.auth, HttpAuth::Bearer { env: "TOK".to_string() });
    }

    /// `--http <url> --oauth` → OAuth auth mode.
    #[test]
    fn http_url_with_oauth() {
        let h = http(&["--http", "https://x.example/mcp", "--oauth"]);
        assert_eq!(h.url, "https://x.example/mcp");
        assert_eq!(h.auth, HttpAuth::OAuth);
    }

    /// `--http <url> --oauth-interactive` → interactive OAuth auth mode.
    #[test]
    fn http_url_with_oauth_interactive() {
        let h = http(&["--http", "https://x.example/mcp", "--oauth-interactive"]);
        assert_eq!(h.url, "https://x.example/mcp");
        assert_eq!(h.auth, HttpAuth::OAuthInteractive);
    }

    /// `--oauth` + `--oauth-interactive` together → interactive (the superset wins,
    /// not an error — they're the same grant, redundant but harmless).
    #[test]
    fn oauth_and_interactive_resolves_to_interactive() {
        let h = http(&["--http", "https://x", "--oauth", "--oauth-interactive"]);
        assert_eq!(h.auth, HttpAuth::OAuthInteractive);
        // Order-independent.
        let h = http(&["--http", "https://x", "--oauth-interactive", "--oauth"]);
        assert_eq!(h.auth, HttpAuth::OAuthInteractive);
    }

    /// `--bearer-env` and either OAuth flag together → error (mutually exclusive).
    #[test]
    fn bearer_and_oauth_mutually_exclusive() {
        assert!(parse(&["--http", "https://x", "--bearer-env", "TOK", "--oauth"]).is_err());
        // Order-independent.
        assert!(parse(&["--http", "https://x", "--oauth", "--bearer-env", "TOK"]).is_err());
        // --oauth-interactive is an OAuth mode too: also exclusive with bearer.
        assert!(parse(&["--http", "https://x", "--bearer-env", "TOK", "--oauth-interactive"]).is_err());
        assert!(parse(&["--http", "https://x", "--oauth-interactive", "--bearer-env", "TOK"]).is_err());
    }

    /// `--oauth` / `--oauth-interactive` on the stdio form → error.
    #[test]
    fn oauth_on_stdio_errors() {
        assert!(parse(&["--oauth", "--", "cat"]).is_err());
        assert!(parse(&["--oauth-interactive", "--", "cat"]).is_err());
    }

    /// `--http <url> -- cat` → ambiguous → error.
    #[test]
    fn http_and_stdio_is_ambiguous() {
        assert!(parse(&["--http", "https://x.example/mcp", "--", "cat"]).is_err());
    }

    /// Neither shape → error.
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

    // --- login subcommand ---

    /// `login --http <url>` → Login with the URL.
    #[test]
    fn login_well_formed() {
        match parse(&["login", "--http", "https://x.example/mcp"]).unwrap() {
            Command::Login(LoginArgs { url, profile }) => {
                assert_eq!(url, "https://x.example/mcp");
                assert_eq!(profile, None, "no --profile → shared default");
            }
            other => panic!("expected Login, got {other:?}"),
        }
    }

    /// `login` with no URL → error.
    #[test]
    fn login_requires_url() {
        assert!(parse(&["login"]).is_err());
        assert!(parse(&["login", "--http"]).is_err());
    }

    /// `login` rejects stray arguments (it's OAuth-only — no `--bearer-env`).
    #[test]
    fn login_rejects_stray_args() {
        assert!(parse(&["login", "--http", "https://x", "--bearer-env", "TOK"]).is_err());
        assert!(parse(&["login", "--", "cat"]).is_err());
    }

    // --- --profile (H2) ---

    /// `--profile` on an OAuth HTTP upstream → carried on the upstream.
    #[test]
    fn profile_on_oauth_http() {
        let h = http(&["--http", "https://x.example/mcp", "--oauth", "--profile", "work"]);
        assert_eq!(h.profile.as_deref(), Some("work"));
        assert_eq!(h.auth, HttpAuth::OAuth);
    }

    /// `--profile` on interactive OAuth → carried too.
    #[test]
    fn profile_on_oauth_interactive_http() {
        let h = http(&[
            "--http", "https://x.example/mcp", "--oauth-interactive", "--profile", "personal",
        ]);
        assert_eq!(h.profile.as_deref(), Some("personal"));
        assert_eq!(h.auth, HttpAuth::OAuthInteractive);
    }

    /// No `--profile` → None (shared default — today's behavior).
    #[test]
    fn no_profile_is_none() {
        let h = http(&["--http", "https://x.example/mcp", "--oauth"]);
        assert_eq!(h.profile, None);
    }

    /// A path-valued profile (`--profile ${CLAUDE_PROJECT_DIR}`) parses verbatim.
    #[test]
    fn profile_accepts_path_value() {
        let h = http(&[
            "--http", "https://x.example/mcp", "--oauth", "--profile", "/Users/me/project",
        ]);
        assert_eq!(h.profile.as_deref(), Some("/Users/me/project"));
    }

    /// `login --profile` → carried on LoginArgs.
    #[test]
    fn profile_on_login() {
        match parse(&["login", "--http", "https://x.example/mcp", "--profile", "work"]).unwrap() {
            Command::Login(LoginArgs { url, profile }) => {
                assert_eq!(url, "https://x.example/mcp");
                assert_eq!(profile.as_deref(), Some("work"));
            }
            other => panic!("expected Login, got {other:?}"),
        }
    }

    /// Fork B: `--profile` is meaningless without OAuth — reject on no-auth HTTP,
    /// with `--bearer-env`, and on the stdio `--` form (fail-fast, not silent drop).
    #[test]
    fn profile_rejected_without_oauth() {
        // no-auth HTTP
        assert!(parse(&["--http", "https://x", "--profile", "work"]).is_err());
        // with --bearer-env
        assert!(
            parse(&["--http", "https://x", "--bearer-env", "TOK", "--profile", "work"]).is_err()
        );
        // stdio `--` form
        assert!(parse(&["--profile", "work", "--", "cat"]).is_err());
    }

    /// `--profile` with a missing value errors rather than mis-parsing.
    #[test]
    fn profile_dangling_value_errors() {
        assert!(parse(&["--http", "https://x", "--oauth", "--profile"]).is_err());
        assert!(parse(&["login", "--http", "https://x", "--profile"]).is_err());
    }

    // --- help / version (H1) ---

    /// `--help` / `-h` → Help (anywhere among pre-`--` tokens).
    #[test]
    fn help_flag_long_and_short() {
        assert_eq!(parse(&["--help"]).unwrap(), Command::Help);
        assert_eq!(parse(&["-h"]).unwrap(), Command::Help);
    }

    /// Bare `help` first token → Help (subcommand-position word).
    #[test]
    fn help_bare_word() {
        assert_eq!(parse(&["help"]).unwrap(), Command::Help);
    }

    /// `--version` / `-V` → Version.
    #[test]
    fn version_flag_long_and_short() {
        assert_eq!(parse(&["--version"]).unwrap(), Command::Version);
        assert_eq!(parse(&["-V"]).unwrap(), Command::Version);
    }

    /// Meta flags pre-empt even a full, otherwise-valid upstream spec — they win
    /// among any pre-`--` tokens.
    #[test]
    fn meta_flags_win_among_other_pre_sep_args() {
        assert_eq!(
            parse(&["--http", "https://x", "--oauth", "--help"]).unwrap(),
            Command::Help
        );
        assert_eq!(
            parse(&["--version", "--http", "https://x"]).unwrap(),
            Command::Version
        );
    }

    /// Gap E — passthrough fidelity: a meta flag *after* `--` belongs to the
    /// upstream program's argv, never to toonfmt.
    #[test]
    fn meta_flags_after_separator_pass_through_to_upstream() {
        let cmd = stdio(&["--", "echo", "--help"]);
        assert_eq!(cmd.program, "echo");
        assert_eq!(cmd.args, vec!["--help"]);

        let cmd = stdio(&["--", "echo", "--version"]);
        assert_eq!(cmd.program, "echo");
        assert_eq!(cmd.args, vec!["--version"]);
    }

    /// Subcommands honor `--help`: it pre-empts their own arg validation, so
    /// `login --help` does not error on the missing URL.
    #[test]
    fn subcommands_honor_help() {
        assert_eq!(parse(&["login", "--help"]).unwrap(), Command::Help);
        assert_eq!(parse(&["update", "--help"]).unwrap(), Command::Help);
    }

    // --- update subcommand ---

    /// `update` (no args) → Update.
    #[test]
    fn update_well_formed() {
        assert_eq!(parse(&["update"]).unwrap(), Command::Update);
    }

    /// `update` takes no arguments — any trailing token is an error.
    #[test]
    fn update_rejects_extra_args() {
        assert!(parse(&["update", "foo"]).is_err());
        assert!(parse(&["update", "--http", "https://x"]).is_err());
    }

    // --- bearer resolution (fail-fast), now driven by HttpAuth ---

    /// Bearer resolution is fail-fast: var present → token; absent/empty → error;
    /// non-bearer auth modes → Ok(None). Uses a process-unique var name.
    #[test]
    fn bearer_env_resolves_fail_fast() {
        let var = "TOONFMT_TEST_BEARER_B1";
        let h = HttpUpstream {
            url: "https://x.example/mcp".to_string(),
            auth: HttpAuth::Bearer { env: var.to_string() },
            profile: None,
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

        unsafe { std::env::remove_var(var) };

        // No auth → Ok(None).
        let none = HttpUpstream {
            url: "https://x.example/mcp".to_string(),
            auth: HttpAuth::None,
            profile: None,
        };
        assert_eq!(none.resolve_bearer().unwrap(), None);

        // OAuth → Ok(None) here (token loaded from the store on the serve path).
        let oauth = HttpUpstream {
            url: "https://x.example/mcp".to_string(),
            auth: HttpAuth::OAuth,
            profile: None,
        };
        assert_eq!(oauth.resolve_bearer().unwrap(), None);
    }
}
