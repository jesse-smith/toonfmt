# `toonfmt` — MCP passthrough wrapper that converts tool results to TOON

## Goal

A transparent MCP proxy that wraps an existing server and converts JSON tool-result payloads to [TOON](https://toonformat.dev) to reduce the tokens the model ingests per tool call. The client leg is always **stdio** (Claude Code speaks stdio); the upstream leg is either a **stdio** child process or a **Streamable HTTP** MCP server. Declared in `.mcp.json`:

```jsonc
{
  "mcpServers": {
    "sql": {                                  // stdio upstream: prefix the command after `--`
      "command": "toonfmt",
      "args": ["--", "uvx", "some-sql-mcp", "--db", "..."]
    },
    "remote-sql": {                           // HTTP upstream: --http <url> [--bearer-env VAR]
      "command": "toonfmt",
      "args": ["--http", "https://host/mcp", "--bearer-env", "REMOTE_SQL_TOKEN"]
    },
    "oauth-svc": {                            // HTTP upstream, OAuth: run `toonfmt login --http <url>` once first
      "command": "toonfmt",                   //   (the --http URL must byte-match the login URL)
      "args": ["--http", "https://host/mcp", "--oauth"]
    },
    "oauth-svc-interactive": {                // HTTP upstream, OAuth: auto-opens the browser on first run
      "command": "toonfmt",                   //   (no prior `login` needed; opt-in, can block initialize on a human)
      "args": ["--http", "https://host/mcp", "--oauth-interactive"]
    }
  }
}
```

For a stdio upstream it spawns the server and pumps JSON-RPC over its pipes; for an HTTP upstream it connects via rmcp's Streamable HTTP transport. Either way it transforms only the results of `tools/call`; everything else passes through untouched.

## Why this shape (decisions already made — don’t relitigate)

- **`content` is the write target — but `structuredContent` *shadows* it (MEASURED 2026-05-28, Claude Code).** TOON still goes in a `content` text block, but the routing rule is the **opposite** of what this doc originally assumed. Live probe (labelled sentinels via `tests/fixtures/json_mcp_stub.py`):
  - both `content` + `structuredContent` present → model receives **only `structuredContent`** (raw JSON); the content block is dropped.
  - `content` only → model receives the `content` text block. ✓
  - `structuredContent` only → model receives `structuredContent`.
  - **Rule:** `structuredContent` shadows `content` — present → model sees only it; absent → model sees content. So TOON-in-content reaches the model **only if no `structuredContent` shadows it.** Independently corroborated by dbmcp, which emits content-only TOON (no structuredContent) and whose TOON the model does read.
  - **Consequence — settled (2026-05-29): equality-gated strip is the default.** When a result carries *both* a transformable content block and a `structuredContent`, leaving structuredContent intact makes the transform worse than a no-op (we TOON-encode content the model never reads, while it reads the untouched JSON). The fix: **strip `structuredContent` iff a content block we successfully transformed parses to a `Value` structurally equal to it; otherwise keep it (and log the non-equivalent server).** This is loss-free *by construction*, not merely by trusting the spec's SHOULD — see the spec basis and the equality semantics below.
- **Parse every text block independently; don’t reconcile against `structuredContent`.** `structuredContent` doesn’t map to specific `content[]` blocks, so having it doesn’t save the per-block parse. You have to iterate `content[]` and attempt-parse text blocks anyway. This makes the design simpler than a “read from structuredContent, write to content” split.
- **Spec basis for equality-gated strip (primary source, MCP spec).** *"For backwards compatibility, a tool that returns structured content SHOULD also return the serialized JSON in a TextContent block."* The spec's own example shows the content block holding the serialized JSON of the *same* `structuredContent` object — they're meant to be identical data. So in the spec-compliant both-present case, the content block **is** a copy of structuredContent; once we've TOON'd that block, structuredContent is a redundant raw-JSON duplicate that only shadows our TOON. Stripping it loses nothing. **But the guarantee is a SHOULD, not a MUST** — a non-compliant server could put separate or partially-overlapping data in the two channels, and nothing structural prevents it. We therefore don't *trust* the SHOULD; we *verify* it per-result with a structural `Value` equality check (we already hold both as parsed `Value`s — the structuredContent subtree from the envelope parse, the content value from the transform parse — so the check is one `==`, not an added parse). Equal → strip; unequal → keep + log. False-negative (declining a safe strip) costs only savings; false-positive (stripping unique data) is eliminated.
  - **Equality semantics (with `preserve_order`):** `Value` equality is **order-insensitive for object keys** (`Value::Object` wraps an `IndexMap`, whose `PartialEq` compares key→value membership regardless of insertion order — so a server reordering keys still compares equal and still strips) but **order-sensitive for array elements** (`Value::Array` is a `Vec`; positional). A server that reordered an array relative to structuredContent compares unequal → keep + log. That's correct, not just safe: array order can be semantically meaningful (sorted/ranked rows), so divergent orderings aren't provably the same data. Number-format drift (`1` vs `1.0`) likewise compares unequal → keep. Every permutation fails safe toward keep.
- **Don’t TOON-convert `structuredContent` itself — MEASURED non-viable (2026-05-29).** It has a JSON-object contract on the wire (spec types it as a `record`/object; `outputSchema` makes servers MUST conform and clients SHOULD validate). A string-valued (TOON) structuredContent doesn't degrade gracefully — it **breaks the tool call**: the `probe_structured_string` stub returned a TOON string in the field, and Claude Code rejected the *entire* response client-side with a Zod error (`expected record, received string`, path `structuredContent`) before forwarding anything to the model. Neither the content block nor the TOON string reached the model. So "TOON the field" (option 2b) is not merely risky — it's a hard error on the primary client. Off the table. (This also confirms equality-gated strip is safe: a record-typed-or-absent field always validates; strip can never trigger this error.)
- **Structured-only / no-text-block case (legal, SHOULD-violating) is a deferred gap.** When `structuredContent` is present but `content[]` has no transformable text block, equality-gated strip is a deliberate no-op (we wrote no TOON → we strip nothing → model reads raw JSON, no savings). The only type-safe way to deliver TOON there is to **add** a content text block (`content[].text` is a string by contract, always valid) — option "2a". Reopened as a candidate but **deferred to Phase 4** with the flags; the transform leaves this shape as a documented no-op.
- **Client variance exists.** Some clients (e.g. Google ADK) forward the *entire* result envelope to the model, including `structuredContent`. Equality-gated strip is the right default for them too (the redundant copy is removed; non-equivalent data is preserved). The `--structured-content {keep|strip|minify}` flag (Phase 4) lets schema-validating consumers force `keep`.

## Core loop

Pump JSON-RPC over the upstream’s transport. Match `tools/call` requests to responses by `id`. Pass through `initialize`, `tools/list`, `prompts/*`, `resources/*`, notifications, etc. unchanged. The transform (`transform::tools_call_result`) is a pure function over a parsed `Value` — **transport-blind**; both legs share one downstream decision (`proxy::transform_downstream`).

**Two upstream transports:**

- **stdio** (`-- <program>`): spawn the child, pump newline-framed JSON-RPC over its pipes. Non-transformed messages forward **byte-for-byte identical** (the Phase 1 fidelity property).
- **HTTP** (`--http <url>`): connect via rmcp's `StreamableHttpClientTransport` (`src/http_upstream.rs`), which owns `Mcp-Session-Id` lifecycle, SSE response framing, `MCP-Protocol-Version`, and `Authorization: Bearer`. The handshake is driven as an explicit ordered sequence (client `initialize` → forward the response to stdout → client `initialized`), because rmcp's worker blocks on `initialized` and the client blocks on the init response — collapsing them deadlocks. Unknown methods round-trip via rmcp's `Custom*` catch-all, so there's no method allow-list (measured: `tests/rmcp_spike.rs`).

**Passthrough is semantic on the HTTP leg, byte-identical on the stdio leg (amendment, Phase 3).** Phase 1 promised non-transformed messages forward byte-for-byte. That **cannot hold over HTTP**: there are no original stdio bytes to round-trip — the bytes were an HTTP/SSE body, and rmcp's `receive()` hands us a *typed* message that we re-serialize from a `Value`. So the HTTP leg's guarantee is **semantic passthrough**: every non-`tools/call` message is re-emitted as structurally-equal JSON-RPC (`serde_json::Value`-equal), framed as one stdio line. This is strictly weaker than the stdio guarantee, which is **retained unchanged**. (Measured: rmcp's typed round-trip preserves the `tools/call` result we transform verbatim, including the `content[].text` JSON string; the only observed reshape is an inert `params:{}` added to an outbound `tools/list` request, off the transform path.)

**Server-initiated messages (the optional GET stream — sampling, elicitation, async notifications) are not forwarded in the current slice, but are warned, never silently dropped** (`tracing::warn!` naming the method), so the deferred gap is observable rather than a silent passthrough violation. If a real target needs it, forwarding the GET stream becomes its own task.

For a `tools/call` **result**:

```
if result.isError:           # error payloads are often prose
    pass through unchanged
    return

for block in result.content:               # heterogeneous array
    if block.type != "text":               # image / resource / resource_link
        leave unchanged
        continue
    try:
        obj = strict_json_parse(block.text)
    except:
        try:
            obj = json5_parse(block.text)   # trailing commas, single quotes, etc.
        except:
            leave block unchanged           # genuinely not convertible (prose, etc.)
            continue
    block.text = toon_encode(obj)
    transformed_values.append(obj)   # remember what we successfully parsed+converted

# structuredContent handling — equality-gated strip (default):
#   if structuredContent present AND any value in transformed_values == structuredContent (structural Value ==):
#       remove structuredContent          # redundant raw-JSON copy; equivalent data now lives in content as TOON
#   elif structuredContent present:
#       leave intact + log non-equivalent server   # separate/partial/reordered data — don't delete what we didn't preserve
#   (Phase 4 flag --structured-content {keep|strip|minify} overrides this default.)
```

**Fallback is per-block, not per-result** — one un-convertible block must not tank a multi-block result.

**Edge case (Phase 4):** if `structuredContent` is present but `content[]` has no text block (legal, SHOULD-violating), the equality-gated strip is a no-op (nothing transformed → nothing stripped → model reads raw JSON). Delivering TOON there requires *adding* a text block (option 2a); deferred to Phase 4. The transform leaves this shape unchanged.

## JSON5 strictness boundary

Strictness gradient: `strict json` → **JSON5** → `json-repair`-style salvage. Stop at JSON5.

JSON5 covers the LLM-output flavor (trailing commas, single quotes, unquoted keys, comments). Going further (json-repair) starts *guessing* at unbalanced braces / unterminated strings, which can “succeed” while producing a subtly wrong object — you then emit confident TOON over corrupted data. For unterminated/missing-brace cases, bailing (leave block unchanged) is safer than guessing.

If a non-strict path is taken, **log it** (which upstream server, which tool) so misbehaving servers are auditable rather than silently papered over. If TOON output ever looks subtly wrong, “did JSON5 mis-parse a malformation into a valid-but-wrong object?” is the first thing to check.

## Config surface

**Commands.** The binary has three subcommands: the default **serve** form (an implicit `toonfmt [serve-flags]`, what `.mcp.json` invokes); **`toonfmt login --http <url>`** (a one-shot OAuth authorization, run by hand before serving — see OAuth below); and **`toonfmt update`** (self-update — see Language / distribution). `update` takes no arguments; on a non-installer build (no receipt) it prints guidance and exits 0 rather than erroring or clobbering. The top-level meta flags **`--help`/`-h`/`help`** and **`--version`/`-V`** print to stdout and exit 0; they are recognized only among **pre-`--` tokens** (everything after `--` is the upstream program's argv, forwarded verbatim — `toonfmt -- echo --help` passes `--help` to `echo`), and they pre-empt subcommand validation (`toonfmt login --help` prints help, not a "requires a URL" error).

**Upstream selection (landed):**

- `--  <program> [args...]` — stdio upstream (spawn and pump over pipes). Mutually exclusive with `--http`.
- `--http <url>` — HTTP (Streamable HTTP) upstream. Mutually exclusive with `--`; exactly one is required.
- `--bearer-env <VAR>` — name of an env var holding the bearer token for an HTTP upstream (never the token on argv — no process-list leak). Resolved at startup with **fail-fast** semantics: unset or empty → error and exit (no silent degrade to an unauthenticated request). Absent → no auth header. Mutually exclusive with `--oauth`/`--oauth-interactive` (a server takes a static token *or* an OAuth grant, not both).

**OAuth (landed — explicit `login`):** for HTTP upstreams that require an OAuth 2.1 authorization-code grant rather than a static token. rmcp's additive `auth` feature owns the *protocol* (metadata discovery, dynamic client registration, PKCE, code↔token exchange, automatic token refresh); toonfmt owns the *lifecycle/UX* (browser launch, the localhost redirect listener, persistent cross-process token storage).

- `toonfmt login --http <url>` — run **once, by hand**, before serving. Discovers the server's OAuth metadata, opens the system browser to the consent page (binds an ephemeral `127.0.0.1:<port>/callback` listener for the redirect), completes the code↔token exchange, and **persists** the credentials to disk. Prints success to stderr and exits. (`TOONFMT_BROWSER_CMD` overrides the launcher — the seam the headless e2e drives.)
- `--oauth` (serve flag) — selects the OAuth transport. The serve path only ever **loads** the cached token (then lets rmcp auto-refresh + re-persist rotated tokens); it **never launches a browser**. No stored token → **fail-fast** to stderr (`run \`toonfmt login --http <url>\` first`). This is the property that lets the feature stand alone: serve never blocks `initialize` on a human.
- **Token storage:** `~/.toonfmt-auth/<key_hash>.json`, file `0600` / dir `0700`, one file per `(profile, url)` (holds rmcp's `StoredCredentials`). Simple, inspectable, cross-process — the `mcp-remote` model. The stem is `sha256(url)` with no `--profile` (byte-identical to the original per-URL scheme — existing logins keep working) or `sha256(profile ⊕ "\0" ⊕ url)` with one.
- `--profile <name>` (login + serve flag) — namespaces the credential store so one URL can hold **multiple independent identities** (work vs. personal) instead of the second `login` silently overwriting the first. `None` (no flag) is the shared default. Folding the profile into the *hash* (not a subdirectory) lets a **path-valued** profile — `--profile ${CLAUDE_PROJECT_DIR}` in a project-scoped `.mcp.json`, which Claude Code expands — work with no sanitization and no secret-adjacent path on disk; this is how project- vs. user-scoped tokens are expressed (the `.mcp.json` location *is* the scope, since Claude Code does **not** transmit config scope to a stdio subprocess — see memory `claude-code-mcp-subprocess-env`). **Meaningful only for OAuth** (only the OAuth modes read the store): `--profile` with `--bearer-env`, with no auth, or on the stdio `--` form is a **fail-fast error**, not a silent no-op.
- **⚠️ URL-match gotcha (MEASURED in B6) — now URL *and* profile:** the store is keyed by the **exact upstream URL string** (and the `--profile` string, if any), so the `login` URL/profile must **byte-match** the serve `--http <url>`/`--profile` in `.mcp.json`. Logging in with the bare host (`https://host`) but serving `https://host/mcp`, or logging in with `--profile work` but serving without it, stores/looks-up the token under a different hash → serve fail-fast → the host shows only "disconnected". Use the identical strings in both places.
- **Why toonfmt drives OAuth itself:** to the host (Claude Code), toonfmt is a *stdio subprocess*; the host's native OAuth/reauth flow fires only for servers *it* connects to over HTTP/SSE, and is blind to a stdio subprocess's upstream backend. The OAuth-protected server is on toonfmt's *upstream* leg, so toonfmt drives the browser flow itself (the same reason `mcp-remote` exists).
- `--oauth-interactive` (serve flag, landed — Slice C) — the **convenience** variant: a superset of `--oauth` that, on a *missing* token, runs the authorization-code flow **inline at serve time** (auto-launching the browser) instead of failing fast, so a freshly added `.mcp.json` OAuth server works with **no prior `login`**. A *present* token is reused identically to `--oauth` (no browser). Sound because rmcp requires auth to complete **before** `initialize` — the flow finishes synchronously before the transport is handed to the driver, so `initialize` always sees a valid token. **Opt-in only** (it can block the host's `initialize` on a human at the consent page; Claude Code tolerates ~30s, measured in B6). The auth URL is also echoed to stderr as a recoverable fallback (B6: stderr lands in the host's per-server cache log). Reuses the **same** `login` driver as the explicit path — only the trigger differs.
- **Choosing between them:** `--oauth` is deterministic and host-independent (serve never blocks on a human) — prefer it for unattended/CI-like setups, pairing it with a one-time `toonfmt login`. `--oauth-interactive` trades that determinism for zero-setup convenience; use it for interactive workstations where blocking `initialize` briefly on first run is acceptable.
- **Why a *stderr prompt* was rejected (the original Slice C C1 sketch):** B6 measured that Claude Code captures a stdio subprocess's stderr only into a cache log, not the visible `/mcp` menu — so a "click this URL" prompt would be invisible where the user looks. Auto-opening the browser sidesteps this: there is nothing to read. (See also the discarded elicitation channel below.)
- **Elicitation channel — investigated and discarded (Slice C C2).** Prompting the user *through Claude Code* via MCP `elicitation/create` is **not viable**, on three independent grounds: (1) the OAuth **401 hits `initialize` itself** (the whole HTTP MCP endpoint is token-protected — MCP spec, our stub, Cloudflare), so there is no "let `initialize` succeed → 401 on a later call → elicit" window; (2) rmcp's `AuthClient` returns `AuthorizationRequired` on a 401 and **never starts a flow**, so a lazy model would mean driving OAuth outside `AuthClient`, reopening protocol rmcp owns; (3) **bootstrap circularity** — sending `elicitation/create` requires an initialized client-leg session, but as a passthrough toonfmt can't answer the host's `initialize` without first getting the upstream's (token-gated) `initialize` response. The auto-open variant is the right convenience layer; it fits rmcp's auth-before-`initialize` model exactly.

**Transform flags (Phase 4 — design-only):**

- `--skip-tool <glob>` — tools whose results should never be converted (free-form text tools).
- `--structured-content {keep|strip|minify}` — default is **equality-gated strip** (strip iff a transformed content block is structurally equal to it; else keep). `keep` forces leave-intact (for schema-validating consumers); `strip` forces unconditional removal; `minify` compacts it in place. Phase 4 — the transform currently hardcodes the equality-gated default.
- `--strictness {strict|json5}` — default `json5`.
- (optional) inject a short TOON syntax cheatsheet into the upstream’s `serverInfo.instructions` during `initialize` so consuming clients that pipe instructions into context don’t each need to add it to their system prompt. ~20 LOC, biggest UX win at the wrapper boundary. (Skipped if the consuming model already handles TOON reliably — which it does in this user’s setup.)

## Language / distribution

- **Single binary (Rust)** — cleanest `.mcp.json` story: `"command": "toonfmt"`, no runtime to install. **Chosen.** rmcp (the official MCP Rust SDK) bundles the Streamable HTTP client into the binary, so HTTP upstreams need no Node/`mcp-remote` sidecar — the binary value holds for both transports. (Avoiding that Node dependency is precisely why the HTTP support is in-process: no-Node hosts, `npx` cold-fetch on air-gapped/proxied networks, transitive-npm audit surface, two-runtime process trees.)
- **Python + `uv tool install`** — fine, adds a runtime dep. `toon-python` is the stable lib. (Not chosen.)
- **Node + `npx`** — most idiomatic for the MCP ecosystem, but pays Node startup cost per invocation. (Not chosen.)

**Shipped (v0.1.0, 2026-06-02).** Released via [`dist`](https://opensource.axo.dev/cargo-dist/) (the tool formerly `cargo-dist`), pinned to **0.32.0** in `dist-workspace.toml`. A tag push fires `.github/workflows/release.yml` (plan → build on native runners → host → announce), which publishes a GitHub Release with prebuilt archives + checksums and generated `curl|sh` / `irm|iex` installer scripts. The README installer uses the `/releases/latest/download/` URL so it survives future tags without an edit.

- **License:** MIT (`LICENSE` at root; `Cargo.toml` `license = "MIT"`). Verified unblocked across the dependency tree.
- **Targets (core-4, all native GitHub runners — no cross-compile, keeps aws-lc-rs happy):** `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`. musl / linux-arm64 are out of v0.1 scope (aws-lc-rs cross-build friction; add on request).
- **dist pin is deliberate.** dist is self-hosting: `release.yml` is generated by a pinned dist version that bakes itself into the installer URL, so there is no at-latest mode — the pin guarantees a tag produces a byte-reproducible release. Bumping is an explicit `dist init` re-run. (Everyday CI action tags in `ci.yml` stay at-latest, e.g. `actions/checkout@v6`; only dist's own version is pinned.)
- **Release sequencing gotcha:** `release.yml` must exist on the remote *before* the tag is pushed, or the tag fires nothing. Create the remote and push `main` first, then tag.
- **`install-updater = false`** — dist's standalone updater binary is declined in favor of the in-binary **`toonfmt update`** subcommand (landed Phase B 2026-06-03, `axoupdater` 0.10.0 as a library), keeping the single-binary ethos. `update` reads the install receipt the dist installer wrote, checks GitHub Releases, and re-runs the installer in place; it runs on a `spawn_blocking` thread because axoupdater's `run_sync` calls `block_on` internally (which would panic inside the `#[tokio::main]` runtime). axoupdater surfaces failures as miette diagnostics with no matchable error enum, so no-receipt / network / no-installer cases are classified by substring-matching the rendered message — **re-verify those matches on any axoupdater bump** (0.10.0 reworded the no-receipt error vs. 0.9; a pinned regression test guards it).
- **CI gate (`ci.yml`):** `cargo clippy --all-targets -- -D warnings` + `cargo test` on push/PR. No coverage/Codecov at v0.1 (deferred — a new repo has no `CODECOV_TOKEN`).

## Validation before trusting it

1. ~~**Confirm the routing assumption**: MCP Inspector against the target client (Claude Code) to verify the model reads `content` and not `structuredContent`.~~ **DONE 2026-05-28 — and the assumption was INVERTED.** Measured directly in a Claude Code session (sentinel stub): `structuredContent` *shadows* `content`. The write-to-content strategy holds only when structuredContent doesn't shadow it (see the corrected decision bullet above). This was the load-bearing assumption; it is now measured, not assumed.
1. TOON comprehension on the actual model is **already confirmed** in this user’s setup (TOON already in use for a SQL MCP, model performs better than with JSON) — no need to re-validate.

## Scope notes

- Tools-only by default. Don’t convert `resources/read` — resources carry markdown/prose that conversion would corrupt.
- Built incrementally, not one-shot. **Phase 1** (JSON-RPC passthrough pump) and **Phase 2** (`tools/call` result JSON→TOON transform, equality-gated `structuredContent` strip) are landed and live-verified against Claude Code. **Phase 3** (HTTP upstream — Streamable HTTP via rmcp, `--http`/`--bearer-env` with fail-fast bearer auth) — **Slice A landed (2026-06-02)**, verified end-to-end against the stub e2e **and two live servers** (Databricks SQL with token auth; Parallel Web Search no-auth) — real `tools/call` results return TOON'd, data faithful, bearer on the wire, in-flight responses drained on stdin EOF. One accepted known limitation: real-SSE buffer-until-complete is stub-covered only (both live targets answered `application/json`; on the rmcp path SSE assembly is rmcp's, so fragment-transform is structurally impossible). **Slice B (OAuth — explicit `login`) landed (2026-06-02)**: rmcp's additive `auth` feature drives an OAuth 2.1 authorization-code grant; `toonfmt login --http <url>` persists credentials to `~/.toonfmt-auth/`, and `--oauth` serve loads + auto-refreshes them without ever blocking `initialize` on a browser. Verified by the headless stub e2e (`tests/oauth_e2e.rs` — login→persist→serve→TOON→reuse) **and a live OAuth server** (Cloudflare Workers bindings, `https://bindings.mcp.cloudflare.com/mcp`, in real Claude Code — connected, `tools/call` → TOON, refresh path exercised). **Slice C (interactive OAuth — `--oauth-interactive`) landed (2026-06-02)**: the convenience variant auto-launches the browser inline at serve time when no token is stored (auth completing before `initialize`), reusing the same authorization-code driver; a stored token is reused exactly as `--oauth`. Verified by a headless stub e2e (`interactive_oauth_serve_logs_in_then_reuses` — no separate `login`: mint-on-first-serve, reuse-on-second) **and live in real Claude Code** against the Cloudflare server (missing token → browser auto-opened → consent in ~20s, inside the 30s connect timeout → fresh token minted → connected with tools). The stderr-prompt and `elicitation/create` channels were investigated and **discarded** (stderr is invisible in the host UI; elicitation can't fire before the token-gated `initialize`) — auto-open is the kept channel. **Phase 4** — the transform config surface (`--skip-tool`, `--strictness {strict|json5}`, `--structured-content {keep|strip|minify}`) and the structured-only "add a content block" case (2a) — remains design-only. This doc is the constitution; the hardcoded transform defaults are strictness=json5, structured-content=equality-gated strip, no skip list.
- **Out of scope:** emitting HTTP (the client leg is always stdio — toonfmt does not serve HTTP); the legacy HTTP+SSE two-endpoint transport (2024-11-05).
