# toonfmt

A transparent [MCP](https://modelcontextprotocol.io) proxy that re-encodes tool-call
results as [TOON](https://toonformat.dev), a compact format that costs a model fewer
tokens to read than the equivalent JSON. You point your MCP client at `toonfmt` instead
of the real server, and `toonfmt` forwards every message untouched except the result of
`tools/call`, which it converts to TOON on the way back.

Your client speaks stdio to `toonfmt`. The upstream server can be either a **stdio**
child process or a **Streamable HTTP** MCP server, with bearer-token or OAuth auth.

## Install

> **Note:** prebuilt binaries and the `curl | sh` installer ship with the first tagged
> release. Until then, install from source with `cargo install`.

<!-- RELEASE-INSTALLER: replaced with the real dist one-liner at v0.1.0 (task A6) -->

```sh
# from source (needs a Rust toolchain)
cargo install --git https://github.com/jesse-smith/toonfmt
```

This puts a single `toonfmt` binary on your `PATH`. There is no runtime to install: the
HTTP client is compiled in, so HTTP upstreams need no Node or `mcp-remote` sidecar.

## Use it

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
keyed by the exact string.

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
