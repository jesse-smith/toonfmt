# toonfmt

[![CI](https://github.com/jesse-smith/toonfmt/actions/workflows/ci.yml/badge.svg)](https://github.com/jesse-smith/toonfmt/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/jesse-smith/toonfmt/branch/main/graph/badge.svg)](https://codecov.io/gh/jesse-smith/toonfmt)

A transparent [MCP](https://modelcontextprotocol.io) proxy that re-encodes tool-call
results as [TOON](https://toonformat.dev), a compact format that costs a model fewer
tokens to read than the equivalent JSON. You point your MCP client at `toonfmt` instead
of the real server, and `toonfmt` forwards every message untouched except the result of
`tools/call`, which it converts to TOON on the way back.

Your client speaks stdio to `toonfmt`. The upstream server can be either a **stdio**
child process or a **Streamable HTTP** MCP server, with bearer-token or OAuth auth.

## Install

Prebuilt binaries for macOS (Apple Silicon and Intel), Linux x86-64, and Windows x86-64.
The installer drops a single `toonfmt` binary on your `PATH`.

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/jesse-smith/toonfmt/releases/latest/download/toonfmt-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/jesse-smith/toonfmt/releases/latest/download/toonfmt-installer.ps1 | iex"
```

Or build from source with a Rust toolchain:

```sh
cargo install --git https://github.com/jesse-smith/toonfmt
```

There is no runtime to install: the HTTP client is compiled in, so HTTP upstreams need no
Node or `mcp-remote` sidecar.

## Updating

If you installed via the shell/PowerShell installer above, upgrade in place:

```sh
toonfmt update
```

It checks GitHub Releases and re-runs the installer only if a newer version exists;
otherwise it reports you're already current. If you installed another way — `cargo
install`, or from source — `toonfmt update` says so and exits without changing anything;
update the way you installed (e.g. re-run `cargo install`).

## Use it

`toonfmt --help` prints the full flag reference and `toonfmt --version` prints the version
— the quickest way to see every option.

Declare `toonfmt` as the command in your client's MCP config (`.mcp.json` for Claude
Code). Pick the recipe that matches your upstream.

### stdio upstream

Prefix the real server command with `--`. `toonfmt` spawns it and pumps JSON-RPC over
its pipes.

```jsonc
{
  "mcpServers": {
    "sql": {
      "command": "toonfmt",
      "args": ["--", "uvx", "some-sql-mcp", "--db", "..."]
    }
  }
}
```

### HTTP upstream, bearer token

`--http <url>` connects over Streamable HTTP. `--bearer-env VAR` names the environment
variable holding the token, so the secret never lands on the process argument list. An
unset or empty variable fails fast at startup rather than sending an unauthenticated
request.

```jsonc
{
  "mcpServers": {
    "remote-sql": {
      "command": "toonfmt",
      "args": ["--http", "https://host/mcp", "--bearer-env", "REMOTE_SQL_TOKEN"]
    }
  }
}
```

### HTTP upstream, OAuth (explicit login)

For servers that require an OAuth 2.1 authorization-code grant, log in once by hand:

```sh
toonfmt login --http https://host/mcp
```

This opens your browser to the consent page, completes the token exchange, and saves the
credentials under `~/.toonfmt-auth/`. Then serve with `--oauth`, which loads the cached
token and lets the proxy refresh it. The serve path never opens a browser, so it never
blocks your client's startup on a human.

```jsonc
{
  "mcpServers": {
    "oauth-svc": {
      "command": "toonfmt",
      "args": ["--http", "https://host/mcp", "--oauth"]
    }
  }
}
```

The `--http` URL must byte-match the URL you passed to `login`: the credential store is
keyed by the exact string. If you use `--profile` (below), the **profile must match too** —
the store is keyed by URL *and* profile together.

### Per-profile credentials (`--profile`)

By default, all logins to one URL share a single token file — keyed by the URL alone. Add
`--profile <name>` to give a URL **multiple independent identities** that don't overwrite
each other. The same `--profile` must appear on both `login` and serve.

```sh
toonfmt login --http https://host/mcp --profile work
toonfmt login --http https://host/mcp --profile personal
```

Two common uses:

- **Same URL, different accounts** — `--profile work` vs. `--profile personal` keep their
  tokens in separate files.
- **Project-scoped tokens** — pass `--profile ${CLAUDE_PROJECT_DIR}` in a *project-scoped*
  `.mcp.json` (Claude Code expands the variable). Each project then keys its own token
  while a user-scoped server (no `--profile`) keeps sharing one. The path only feeds the
  store *key* (it is hashed) — credentials always live under `~/.toonfmt-auth/`, never in
  your repo.

```jsonc
{
  "mcpServers": {
    "oauth-svc": {
      "command": "toonfmt",
      "args": ["--http", "https://host/mcp", "--oauth", "--profile", "${CLAUDE_PROJECT_DIR}"]
    }
  }
}
```

`--profile` is meaningful only for OAuth upstreams (`--oauth` / `--oauth-interactive`); it
is rejected on a bearer-token, no-auth, or stdio upstream rather than silently ignored.

### HTTP upstream, OAuth (interactive)

`--oauth-interactive` skips the separate `login`. On a missing token it runs the
authorization-code flow inline on first serve, opening the browser before `initialize`.
A present token reuses the `--oauth` path with no browser. This can block your client's
startup on the consent page, so it is opt-in.

```jsonc
{
  "mcpServers": {
    "oauth-svc-interactive": {
      "command": "toonfmt",
      "args": ["--http", "https://host/mcp", "--oauth-interactive"]
    }
  }
}
```

### Token-savings stats (opt-in)

See how many bytes the TOON transform saves. It is **off by default** — the proxy stays a
silent, zero-overhead passthrough until you opt in, and nothing is ever written to disk
otherwise. Turn it on with the `--stats` serve flag (or set `TOONFMT_STATS=1`):

```jsonc
{
  "mcpServers": {
    "sql": {
      "command": "toonfmt",
      "args": ["--stats", "--", "uvx", "some-sql-mcp", "--db", "..."]
    }
  }
}
```

While enabled, each converted `tools/call` result records its byte delta to a small
SQLite store at `~/.toonfmt/stats.db`. Read the cumulative, per-project total any time:

```sh
toonfmt stats
```

```text
toonfmt — token-savings stats (bytes of JSON the model didn't read)

  /home/you/projects/app                      128 results    142.6 KB → saved    84.1 KB  (  59.0%)
  /home/you/projects/api                       14 results     12.0 KB → saved     -390 B  (  -3.2%)  (2 grew)

  TOTAL                                       142 results    154.6 KB → saved    83.7 KB  (  54.1%)  (2 grew)
```

The figures are **exact bytes and a bytes-based percentage — never a token count**
(Anthropic's tokenizer isn't public, so any token number would be fabricated). Only
savings the model actually reads are counted, deltas are signed (a block TOON makes
*larger* counts against you, and is flagged as "grew"), and `toonfmt stats` run before you
ever enable stats just prints a friendly "no stats recorded yet" — it never creates the
store. See [ARCHITECTURE.md](ARCHITECTURE.md) for the full rationale.

## How it works

`toonfmt` matches `tools/call` requests to their responses by JSON-RPC `id` and rewrites
only those results. For each text block in a result it parses the JSON (strict, then
JSON5 for trailing commas and single quotes), re-encodes it as TOON, and writes it back
to the same block. Blocks it cannot parse pass through unchanged, so one odd block never
breaks a multi-block result. When a result carries both a TOON-converted block and a
duplicate `structuredContent`, the proxy drops the redundant copy only after checking
that the two hold structurally equal data. Everything that is not a `tools/call` result
(`initialize`, `tools/list`, resources, prompts, notifications) forwards as-is.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design: the `structuredContent`
shadowing rule, the equality-gated strip, the two transport legs, and the OAuth
lifecycle.

## License

MIT. See [LICENSE](LICENSE).
