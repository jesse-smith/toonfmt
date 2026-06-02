# HTTP upstream — toonfmt bridges a stdio client to a Streamable HTTP MCP server

**Date:** 2026-06-02  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-05-28-toon-transform.md (Phase 2 — transform, landed)
**Status:** in progress  <!-- drafted | in progress | landed -->

## Goal
toonfmt is stdio↔stdio today: it spawns an upstream child and pumps JSON-RPC over its
pipes. Some real targets are **HTTP** MCP servers (Streamable HTTP). This phase lets the
upstream leg optionally be an HTTP MCP server while the **client leg stays stdio** (Claude
Code speaks stdio; we don't emit HTTP). toonfmt keeps being a passthrough + mutator: same
`tools/call` TOON transform, same everything-else-passes-through — just reached over HTTP
instead of a pipe. This unblocks the HTTP servers in the user's workflow with a single
binary (no Node/`mcp-remote` runtime at each consumer's site).

## Architecture
Per `ARCHITECTURE.md` "Core loop". The transform (`transform::tools_call_result`) is a pure
function over a parsed `Value` — **transport-blind, unchanged this phase.** What changes is
the upstream *seam*: today a byte-pump over child stdio; now optionally an rmcp-backed
Streamable HTTP client. The mutation site is identical.

**Decisions / constitutional notes:**

- **The seam lifts from bytes to messages on the HTTP leg only.** stdio keeps its
  byte-fidelity pump (`proxy::pump`, untouched). HTTP is message-oriented (POST →
  JSON-or-SSE response; optional server→client GET stream; `Mcp-Session-Id` lifecycle), so
  the HTTP driver works at the `Value`/`Message` level. The two drivers share one
  transport-blind downstream handler (parse → classify → correlate → transform → serialize).
- **AMENDMENT to the byte-identical passthrough property.** Phase 1's invariant — non-owned
  messages forward *byte-for-byte* — **cannot hold on the HTTP leg**: there are no original
  stdio bytes to round-trip (the bytes were an HTTP/SSE body). The honest target on the HTTP
  leg is **semantic passthrough**: every non-`tools/call` message is re-emitted as
  structurally-equal JSON-RPC (`serde_json::Value` equal), framed as one stdio line. This is
  strictly weaker than the stdio guarantee and is a real amendment, recorded in
  `ARCHITECTURE.md` by this phase. The stdio leg's byte-identical property is **retained
  unchanged.**
- **`rmcp` (official MCP Rust SDK) supplies the HTTP adapter — we do not hand-roll SSE /
  session / auth.** `StreamableHttpClientTransport` (feature
  `transport-streamable-http-client-reqwest`) handles `Mcp-Session-Id`, SSE response framing,
  `MCP-Protocol-Version`, and `Authorization: Bearer`. This is the hard part of "approach B"
  written and maintained by the protocol authors; we *depend on* it rather than build it.
- **LOAD-BEARING CHECK (spike, A1) — characterize, don't pass/fail.** Not "the pivot" — the
  `Value`↔rmcp mapping is near-automatic for spec traffic (`from_value`/`to_value`, no
  hand-written dispatch). But it *is* load-bearing, because rmcp is built for **endpoints, not
  proxies**: an endpoint generates from / consumes into typed structs and **never round-trips**
  `Value → type → Value`. That round-trip is *our* peculiar usage, and canonical-ness does not
  guarantee it's the identity. Confirmed facts:
  - ✅ **Transport drivable without `serve_client`.** `StreamableHttpClientTransport` is a
    `WorkerTransport` whose worker starts at *construction*; `.send()`/`.receive()` work directly.
    `Mcp-Session-Id` capture + re-application happen inside the worker. "Raw pipe access" is *not*
    the question.
  - ⚠️ **Typed, not a `Value` pipe.** `send()` takes `JsonRpcMessage<ClientRequest, ClientResult,
    ClientNotification>`; `receive()` returns the `Server*` form. Crossing into method-*specific*
    typed variants can in principle (a) **reshape** — e.g. `#[serde(skip_serializing_if =
    "Option::is_none")]` turns an explicit `null` into an absent key on re-emit (correct for an
    endpoint, lossy for our round-trip); or (b) hit the **`#[serde(untagged)]` ordering hazard**
    if variants match structurally rather than by `method`. Both would bite hardest on
    `tools/call` results — the one payload we transform — which is exactly what the spike checks.
  - ✅ **`Custom*` catch-all variants carry raw `method` + `Value`.** The escape hatch for
    messages that *can't* go typed (unknown methods) or *shouldn't* (typed round-trip reshapes).
  - **DESIGN — typed by default, `Custom` only by necessity (per-message-type, not whole-driver):**
    Typed deserialization is rmcp's intended, best-tested path, and it **validates against the
    spec at the boundary** (a malformed message fails fast there rather than surfacing as a
    confusing downstream error) — which `Custom` does not, since it swallows any `method`+`params`
    shape. So we prefer typed wherever it's faithful and cheap. A message drops to `Custom` only
    when typed is **impossible** (an unknown/extension method has no variant to deserialize into)
    or **unfaithful** (typed round-trip reshapes it → would violate semantic passthrough). Note:
    on the HTTP leg we re-serialize from `Value` either way, so where typed is faithful it is
    *equivalent* to `Custom` on fidelity and strictly better on validation — there's no fidelity
    argument for defaulting to `Custom`.
  - **A1 OUTPUT = the per-method routing map + a viability verdict:**
    - For each forwarded method, record: typed round-trips faithfully (→ **typed**), or it
      reshapes / has no variant (→ **`Custom`**). This map *is* A4's dispatch rule.
    - **Viable (expected):** every method is either faithfully-typed or cleanly `Custom`-routable;
      A4 implements the typed-default/`Custom`-fallback split. Keeps rmcp's session/SSE/auth (and
      the free `auth` integration for Slice B).
    - **NOT viable → hand-rolled `reqwest` + `sse-stream` (LAST RESORT, narrow trigger):** only if
      rmcp won't carry some message *either* way (typed errors *and* `Custom` won't round-trip
      it), **or** the worker's handshake rigidity is fundamentally incompatible with proxying the
      client's own `initialize`. Forfeits rmcp's session/SSE/auth; re-scope A4 only.
  - The spike tests **all** of `initialize`, `tools/call`, `tools/list`, a `notifications/*`, and
    a deliberately-unknown `x/bogus`: typed round-trip with structural `Value`-equality first
    (does it stay faithful?), then `Custom` round-trip as the fallback probe for any method that
    fails typed. `tools/call` results get the closest scrutiny. Testing only `initialize` would
    pass and prove nothing.
- **Auth ships in two slices: bearer/none first (Slice A), OAuth second (Slice B).** Token
  is read from an **env var** (`--bearer-env <VAR>`), never argv (no leak in the process
  list). OAuth is **in scope for this plan** but deliberately sequenced *after* Slice A is
  verified end-to-end — it's rmcp's additive `auth` feature, so it's a flag-flip + auth-state
  wiring on a proven base, **not** a rewrite. Splitting it this way de-risks the common path
  (token auth, the immediate Databricks SQL target) before taking on the OAuth browser-loop
  surface. This also pre-absorbs the "OAuth could change without warning" risk: the base is
  built ready for it.
- **SSE buffering — transform only on a COMPLETE message.** The transform requires a whole
  JSON-RPC `Value`; a partial object cannot be TOON-encoded. On the `text/event-stream` path,
  an SSE *event* is one complete JSON-RPC message, but its `data:` may span multiple lines
  (joined by `\n`, dispatched on the blank line). We must assemble the full event before
  parsing/transforming — never fire on a fragment. **Who owns this assembly determines what the
  multi-`data:` test proves:** on the rmcp path, **rmcp's `sse-stream` owns assembly** and
  `.receive()` only ever yields complete messages — so the `probe_sse_split` test is an
  **rmcp-integration smoke check** (confirms rmcp doesn't have a framing bug + our stub is
  well-formed), *not* a test of toonfmt code, and it is **not** load-bearing evidence of toonfmt's
  own correctness. It becomes a real test of *our* buffering only in the **hand-rolled fallback**,
  where we own assembly. The plan keeps the test either way but labels it honestly (see A5).
- **Single-binary value UPHELD.** rmcp bundles into the binary; the `.mcp.json` story stays
  `"command": "toonfmt"`. The Node dependency that `mcp-remote` would impose is precisely
  what this avoids (the in-org deployment blockers: no-Node hosts, `npx` cold-fetch on
  air-gapped/proxied networks, transitive-npm supply-chain audit surface, two-runtime process
  trees, version drift).
- **Server→client GET stream — deferred, but NOT silently (this is a real hole).** The optional
  server-initiated GET SSE stream carries server→client `sampling` requests, `elicitation`
  requests, and async notifications. If the upstream uses it and we don't subscribe, those
  messages are **dropped** — which violates the semantic-passthrough property we claim. So
  deferral is conditional, not free: (1) **A4 must emit a `tracing::warn!`** if the transport
  ever surfaces a server-initiated message we aren't forwarding (don't fail silently); and
  (2) **A6 must explicitly check whether the Databricks SQL MCP uses the GET stream** and record
  the finding — only then is "defer" justified for the stated target. If a target needs it,
  subscribing to the GET stream and forwarding its messages to the stdio client becomes its own
  task (likely a Slice A addendum, not Phase 4).
- **Out of scope:** emitting HTTP (client leg is always stdio); legacy HTTP+SSE two-endpoint
  transport (2024-11-05); the Phase-3 config flags (`--skip-tool`, `--strictness`,
  `--structured-content`) — those follow this phase, renumbered Phase 4. **OAuth is IN scope**
  (Slice B) but sequenced after the bearer/none path (Slice A) is verified.

**Dispatch shape.** `cli::parse_args` returns an `Upstream` enum: `Stdio(UpstreamCmd)` (the
current shape, behind `--`) or `Http(HttpUpstream { url, bearer_env })` (behind `--http`).
`proxy::run` matches: `Stdio` → today's three-flow pump (unchanged); `Http` → the new rmcp
driver. The **`transform_downstream` handler** (the parse→classify→transform core) is shared
verbatim — it's genuinely transport-blind. **`RequestTracker`, however, is likely vestigial on
the HTTP leg** and is NOT assumed reused: on stdio, request and response cross separate
unidirectional streams and must be correlated by id across an async boundary; on HTTP, a POST
response is causally tied to the request just sent, so the method is already known from the
local loop variable. A4 decides deliberately whether to carry the method locally (preferred —
no id round-trip, no id-reuse failure mode) or thread the tracker through; default is **carry
locally** unless the transport's async send/receive ordering forces correlation.

## Tech Stack
- `rmcp` (1.7.0) — official MCP Rust SDK (`modelcontextprotocol/rust-sdk`). Features:
  `default-features = false`, then `client`, `transport-streamable-http-client-reqwest`,
  `reqwest` (rustls TLS). **Not** `server`/`macros` (our stdio side stays hand-rolled to keep
  the byte pump), **not** `auth` yet (OAuth deferred — additive later). Exact feature set
  confirmed in task 1.
- `reqwest` (transitively, rustls) — the HTTP client behind rmcp's transport. Main new binary
  weight; accepted (the alternative is a Node runtime at every consumer).
- `tokio` — already present (async runtime; rmcp is tokio-based).
- `serde_json` — already present. **The `Value` ↔ rmcp-typed-message boundary is the core
  integration surface** (see A1): outbound, `from_value` lifts a client `Value` into rmcp's typed
  enum (typed variant by default; `Custom*` only for methods that have no variant or that reshape);
  inbound, `to_value(server_message)` back to a `Value` for the transform. A1 produces the
  per-method routing map; A4 builds on it.
- Python 3 stdlib (dev/test only) — a Streamable HTTP MCP stub fixture
  (`tests/fixtures/http_mcp_stub.py`), sibling to the existing stdio stub. Exercises both a
  plain `application/json` POST response and one `text/event-stream` (SSE) response so the
  framing path is covered. E2E skips with a clear message if `python3` is absent.

## Tasks
<!-- TDD order: test/spike before impl. Concrete paths.
     SLICE A = bearer/none HTTP upstream (the immediate Databricks SQL target).
     SLICE B = OAuth, built on A once A is verified. Each slice ends at cairn-accept. -->

### Slice A — bearer/none HTTP upstream
<!-- A0 RESEQUENCED (2026-06-02, user decision): deferred to the A6 real-MCP batch.
  A0 needs a live HTTP MCP URL; its value is highest pointed at a target the user
  actually cares about (the Databricks SQL MCP, also A6's target). A1 already gave
  the viability verdict, so A0 is a non-load-bearing reality check — run it in the
  same USER-GATED session as A6 rather than picking a random public MCP now. -->
- [ ] **A0 — composition smoke test (zero toonfmt code; status-quo sanity, NOT a fallback). [DEFERRED → A6 batch]**
  Run `toonfmt -- npx mcp-remote <a real HTTP MCP url>` against a live HTTP target; confirm the
  `tools/call` TOON transform fires over an HTTP-delivered result (and over an SSE-delivered
  one if the target streams). Record findings (did it work? SSE seen? any envelope surprises?)
  in an HTML comment in this plan. This is just toonfmt wrapping another child process (the
  thing it already does) — a cheap reality check that the transform behaves over real HTTP-origin
  payloads before committing to rmcp. **It is not the architectural fallback**: the whole point
  of this phase is to *eliminate* the Node/npx dependency, so "ship the mcp-remote wrapper" is a
  non-goal. The real fallback if A1 fails is the hand-rolled `reqwest` + `sse-stream` driver.
- [x] **A1 — SPIKE: characterize the `Value`↔rmcp boundary; produce the per-method routing map.**
  (Reframed — see the LOAD-BEARING CHECK note. "Can we drive Transport raw?" is already YES; the
  spike maps which methods go **typed** vs. **`Custom`**, it does not pass/fail the whole rmcp
  path.) Add the `rmcp` dep with the candidate features; `cargo build` clean. Write `#[ignore]`d
  integration tests (`tests/rmcp_spike.rs`) constructing `StreamableHttpClientTransport` against
  the A3 stub. For **each** of `tools/call`, `tools/list`, a `notifications/*`, and a
  deliberately-unknown `x/bogus`:
  - **Typed probe (preferred routing):** lift the client `Value` into rmcp's message type via the
    **method-specific typed variant**, `.send()`, convert `.receive()` back via `to_value`, and
    assert the round-trip is **structurally equal** to a direct (non-rmcp) round-trip. Pass →
    method routes **typed**. Fail (errors, or reshapes — unequal) → method needs `Custom`.
  - **`Custom` probe (necessity fallback):** for any method that failed the typed probe (incl.
    `x/bogus`, which has no typed variant), confirm the `Custom*` variant round-trips it
    faithfully. This is the escape hatch, used only where typed won't serve.
  - **`tools/call` results get the closest scrutiny** — that's the payload we TOON-encode, so
    typed-faithfulness there is what actually matters.
  - **Handshake probe:** confirm the construction-time worker (no `serve_client`), that
    `initialize`/`initialized` drive session setup, and that `Mcp-Session-Id` from the init
    response is auto-applied to a subsequent POST. Note any ordering rigidity (feeds A4's explicit
    sequencing).
  **DECISION:** every method is faithfully-typed or cleanly `Custom`-routable → **viable**; record
  the per-method routing map (this becomes A4's dispatch rule) and build A4 typed-default /
  `Custom`-by-necessity. Some message rmcp won't carry *either* way, **or** handshake incompatible
  with proxying the client's own `initialize` → **only then** hand-rolled `reqwest` + `sse-stream`
  (re-scope A4 only). Document the map + verdict in this plan before A4.
- [x] **A2 — CLI: `Upstream` enum + `--http`/`--bearer-env` (`src/cli.rs`).** Replace the
  bare `UpstreamCmd` return with `enum Upstream { Stdio(UpstreamCmd), Http(HttpUpstream) }`
  where `HttpUpstream { url: String, bearer_env: Option<String> }`. Parse: presence of
  `--http <url>` selects HTTP (optionally `--bearer-env <VAR>`); the existing `-- <program>
  …` selects Stdio. Error if both or neither are given. Tests: (a) `--http https://x`
  → `Http{url, bearer_env: None}`; (b) `--http https://x --bearer-env TOK`
  → `bearer_env: Some("TOK")`; (c) existing `-- cat` cases still parse to `Stdio` (keep all
  5 current tests green, adapted to the enum); (d) `--http https://x -- cat` errors
  (ambiguous); (e) neither errors. **The named env var is resolved at startup with fail-fast
  semantics:** `--bearer-env MISSING` where `MISSING` is unset → **error and exit**, not a
  silent unauthenticated request (a misspelled var must not degrade to confusing upstream 401s).
  Add test (f): `bearer_env: Some` + var present → token resolved; var absent → startup error.
  (CLI is shaped now so Slice B adds `--oauth`-style flags without re-touching the enum.)
- [x] **A3 — HTTP MCP stub fixture (`tests/fixtures/http_mcp_stub.py`).** Zero-dep stdlib
  Streamable HTTP MCP server (sibling to `json_mcp_stub.py`). Handles POST at one endpoint:
  `initialize` (assigns an `Mcp-Session-Id`), `tools/list`, and `tools/call` for three probes:
  `probe_content_only` (JSON-object content string → `application/json` response);
  `probe_sse` (same payload returned as a `text/event-stream` SSE response, single-line
  `data:`); and **`probe_sse_split`** (same payload returned as SSE where the JSON is split
  across **multiple `data:` lines** within one event — the buffering canary). Echoes the
  session id; returns 202 for notifications. Binds `127.0.0.1:0` (ephemeral port). **Port
  discovery: print the chosen port on the *first line of stdout* as `PORT <n>` and flush; the
  e2e test reads exactly that line before connecting** (the stdio stub `json_mcp_stub.py` has no
  port to advertise, so there's no existing pattern to mirror — this is the contract, stated
  here so A5 doesn't improvise).
- [x] **A4 — HTTP driver (`src/http_upstream.rs`) + dispatch (`src/proxy.rs`,
  `src/main.rs`).** New module bridging stdio client ↔ rmcp HTTP transport. Wire `proxy::run` to
  `match Upstream { Stdio → existing, Http → this }`. Keep trace/debug logging; stdout carries
  protocol bytes only (logs → stderr).
  - **Init-handshake sequencing is EXPLICIT, not a naive read-loop (this is a deadlock trap).**
    rmcp's `StreamableHttpClientWorker` hard-codes a startup order: it expects the **first**
    sent message to be `initialize`, and after relaying the initialize response it **blocks until
    it receives the client's `initialized` notification** before processing any further message.
    A simple "for each stdin line, `transport.send()`" loop will deadlock or misclassify. The
    driver must therefore drive startup as an ordered sequence: (1) read the client's
    `initialize` from stdio → send → `receive()` the `InitializeResult` → forward it to stdout;
    (2) read the client's `initialized` notification → send; (3) only then enter the steady-state
    bidirectional loop. Encode this as distinct startup steps, with a comment citing the worker's
    expectation, so a future reader doesn't "simplify" it back into the deadlock.
  - **Steady-state loop.** Two concurrent halves: client-stdin→`Value`→lift into rmcp message
    →`send()`; and `receive()`→server message→`to_value`→`transform_downstream`→one framed stdout
    line. **The lift uses A1's per-method routing map: typed variant by default, `Custom*` only
    for the methods A1 marked unfaithful-or-unmodeled.** **Never transform a partial message** —
    `receive()` yields complete messages (rmcp owns SSE assembly on the rmcp path; our buffering
    only in the hand-rolled fallback).
  - **Correlation: carry the method locally, do NOT reuse `RequestTracker` by default.** On the
    HTTP leg the response is causally tied to the request just sent, so the method is known
    without an id lookup (see Dispatch shape). Use the tracker only if A1 reveals the transport's
    send/receive ordering is not 1:1 request→response; document the choice in-code.
  - **GET-stream / server-initiated messages: warn, never drop silently.** If `receive()` ever
    surfaces a **server→client request or notification we are not forwarding** (sampling,
    elicitation, async notifications carried on the optional GET stream), emit a
    `tracing::warn!` naming the method — so the deferred GET-stream hole is *observable*, not a
    silent passthrough violation (see the GET-stream decision note).
  - *(extract, pure refactor in `src/proxy.rs`):* lift the downstream closure body (parse
    `Value` → `classify` → `take_method` → `tools/call`? → `transform::tools_call_result`) into a
    named fn `transform_downstream(value: Value, tracker: &RequestTracker) -> Option<String>`
    callable by both the stdio pump and the HTTP driver. All existing proxy + e2e tests stay
    green unchanged. Add a unit test that targets the **correlation/dispatch** logic specifically
    (not a re-test of `tools_call_result`, which is already covered): a `tools/call`-correlated
    response → `Some(TOON)`; a response whose id the tracker doesn't know → `None` (tracker-miss
    path, *not* an absent-`result` path); a non-`tools/call`-correlated response → `None`.
- [x] **A5 — e2e (`tests/http_upstream_e2e.rs`).** Spawn the A3 stub, read the `PORT <n>` line,
  spawn `toonfmt --http http://127.0.0.1:<port>/…`; drive
  initialize/initialized/tool-calls/tools-list over stdio. Assert: (1) `probe_content_only` →
  `content[0].text` is TOON (not JSON-parseable) + payload preserved; (2) `probe_sse`
  (single-line SSE) → **also** TOON'd; (3) `probe_sse_split` (multi-`data:`-line SSE) → TOON'd
  correctly; (4) `tools/list` passes through structurally intact. Skip with a clear message if
  `python3` is unavailable. **Honest labeling:** assertion (3) is an **rmcp-integration smoke
  check** on the rmcp path (rmcp owns SSE assembly — it verifies rmcp + our stub, not toonfmt's
  own buffering) and only becomes a test of *our* code in the hand-rolled fallback. Comment it as
  such so its evidentiary weight isn't overstated. The toonfmt-owned behavior under test here is
  the bridge wiring (1, 2, 4) + that the transform fired identically to the stdio path.
- [ ] **A6 — real-MCP verification (USER-GATED).** The stub proves wiring; this proves
  reality. **User adds the targets when ready** — primary: the **Databricks SQL MCP** (token
  auth). Drive a real `tools/call` through `toonfmt --http <databricks-url> --bearer-env <VAR>`
  and confirm a real result returns TOON'd, data faithful. **Also** verify against a target the
  user supplies that genuinely **streams over SSE** (chunked `tools/call` result) to confirm
  the buffer-until-complete behavior holds on a real server, not just the stub. **And explicitly
  check the GET-stream question:** observe whether the Databricks SQL MCP (and the SSE target)
  emit any server-initiated messages — watch for the A4 `tracing::warn!` and/or inspect traffic.
  If nothing appears on the GET stream, the deferral is justified for these targets — record
  that. If something does, the GET-stream-forwarding addendum is required before Slice A lands.
  Record all outcomes in this plan. (This is the verification step the user explicitly wants —
  it gates Slice A landing, alongside cairn-accept.)
<!-- A6 RESULT — real-MCP verification (2026-06-02), against the LIVE Databricks SQL MCP
  (https://adb-3403296355644133.13.azuredatabricks.net/api/2.0/mcp/sql, token auth via
  --bearer-env DATABRICKS_TOKEN). Driven directly through the release binary's native
  --http path (the rmcp driver), not the stub. ALL PASS:
  - Handshake: initialize → real DatabricksMCPServer response forwarded; initialized
    accepted; bearer auth worked (no 401).
  - tools/list: real toolset (execute_sql, execute_sql_read_only, poll_sql_result),
    structurally intact.
  - tools/call (execute_sql_read_only, "SHOW CATALOGS"): state SUCCEEDED, 19 real
    catalogs returned, result came back TOON-encoded (statement_id/manifest/
    columns[1]{...}/data_array[19] are TOON, not JSON) — transform fired on a real
    payload, data faithful (19 rows, real names: bmtct, caboodle_src, cerner_src, …).
  - GET-stream check: NO server-initiated message observed (RUST_LOG=warn stderr clean
    across handshake + tools/list + tools/call) → the GET-stream DEFERRAL IS JUSTIFIED
    for this target. (No tracing::warn! fired.)
  - structuredContent: absent on the result → no strip needed (Databricks is content-only,
    like dbmcp).

  BUG FOUND (not an A6 blocker; the transform is proven): the driver tears down the
  instant client stdin closes (rx.recv() → None → break), abandoning in-flight
  requests. A request immediately followed by EOF loses its response. Harmless for a
  synchronous client that keeps stdin open (Claude Code), but a real correctness gap —
  a quick `printf ... | toonfmt --http` drops the tools/call response unless stdin is
  held open until the reply arrives. FIX (follow-up): on stdin EOF, stop sending but
  keep draining transport.receive() until any outstanding request ids are answered (or
  a timeout), THEN tear down. Tracked for a Slice-A-addendum / hardening commit.

  SSE-STREAMING TARGET: still needed from the user (Databricks returned application/json,
  not chunked SSE, for SHOW CATALOGS) to confirm buffer-until-complete on a real SSE
  server. A6 is otherwise satisfied for the primary (Databricks) target.

  A0 (mcp-remote composition) folded into this batch: .mcp.json has
  `databricks-toon-viamcpremote` (toonfmt -- npx mcp-remote …) for the post-restart
  in-client check; not yet exercised from the shell. -->
<!-- A7 STATUS (2026-06-02): docs DONE + committed (1c9a917); cairn-verify DONE
  (cargo build + cargo test green, 51 tests, clippy -D warnings clean). The box
  stays unchecked only because A7 also bundles cairn-ACCEPT, which gates on A6
  (the user-supplied real-MCP check). Flip to [x] when A6 lands. -->
- [ ] **A7 — docs + cairn-verify/accept for Slice A.** `ARCHITECTURE.md`: record the
  **semantic-vs-byte-identical passthrough amendment** (HTTP leg); add HTTP upstream to the
  transport description; add `--http`/`--bearer-env` to the config surface; note OAuth as the
  **next slice** (not merely deferred); renumber the config-flags phase to **Phase 4**; update
  the scope note. Save a memory only if A1's spike yields a durable non-obvious fact (e.g.
  "rmcp Transport IS/ISN'T drivable raw"). cairn-verify (zero warnings, clippy `-D warnings`,
  `cargo test` green incl. new e2e when python3 present), then cairn-accept Slice A.

### Slice B — OAuth
> **Scope honesty:** "additive flag-flip on a proven base" describes the *architecture*, not the
> *effort*. OAuth in a CLI proxy is a real chunk of work — browser launch, a localhost redirect
> listener (bind a port, receive the auth code), code↔token exchange, persistent token storage,
> and refresh. rmcp's `auth` feature carries the protocol mechanics, but the UX/lifecycle plumbing
> is ours. mcp-remote spends ~500 LOC here. B is correctly gated behind a landed Slice A, but it
> is **not small** — estimate it as its own multi-task slice, not a wiring afterthought.
- [ ] **B0 — verify Slice A landed first.** Slice B does not start until A is at
  `cairn-accept` with the real-MCP check (A6) done. Hard dependency.
- [ ] **B1 — enable rmcp `auth` feature; wire OAuth state.** Add the `auth` feature to the
  `rmcp` dep; build clean. Wire `OAuthState`/`AuthorizationManager` → `AuthClient` into the
  same `StreamableHttpClientTransport` (rmcp injects bearer tokens automatically). CLI: add the
  OAuth selector to the `HttpUpstream` shape from A2 (e.g. `--oauth` plus discovery/callback
  config) without disturbing the bearer path.
- [ ] **B2 — token cache / callback handling.** Persist tokens (path TBD in plan-mode
  fleshing — analogous to mcp-remote's `~/.mcp-auth/` per-URL isolation); handle the local
  callback for the authorization-code redirect; refresh on expiry (rmcp handles refresh given
  the stored refresh token).
- [ ] **B3 — OAuth verification (USER-GATED).** **User supplies an OAuth MCP target** (req #1
  the user named). Confirm the full flow: unauthorized → browser auth → token stored →
  `tools/call` result returns TOON'd; second run reuses the cached token without re-auth.
  Record in plan.
- [ ] **B4 — docs + cairn-verify/accept for Slice B.** `ARCHITECTURE.md`: OAuth moves from
  "next slice" to "supported"; document the auth config surface and token storage. cairn-verify
  clean, then cairn-accept Slice B.

<!-- VERIFICATION TARGETS the user will supply when ready (do not invent / hardcode):
  1. Databricks SQL MCP (token auth) — primary, Slice A (A6).
  2. An MCP that streams over Streamable HTTP / SSE (chunked result) — Slice A (A6),
     to test buffer-until-complete-block on a real server.
  3. An MCP that uses OAuth — Slice B (B3).
-->

<!-- ============================================================================
  A1 RESULT — rmcp boundary routing map (MEASURED 2026-06-02, rmcp 1.7.0).
  Source: tests/rmcp_spike.rs. Pure-serde probes (always-on) + one #[ignore]d
  live-transport probe against the A3 stub. VERDICT: VIABLE — let rmcp's
  `from_value` auto-select the typed-or-Custom variant; no hand-rolled fallback.

  Per-method routing (verdict = what rmcp's typed layer does to the Value):
  | Method                       | Direction          | Verdict                                  |
  |------------------------------|--------------------|------------------------------------------|
  | initialize                   | client→server req  | Faithful (typed)                         |
  | notifications/initialized    | client→server noti | Faithful (typed)                         |
  | tools/call                   | client→server req  | Faithful (typed)                         |
  | tools/list                   | client→server req  | Reshapes: adds `params:{}` (inert*)      |
  | x/bogus (unknown)            | client→server req  | Faithful via CustomRequest (method+params kept) |
  | **tools/call RESULT**        | server→client      | **Faithful (typed CallToolResult)** ⟵ the payload we transform |
  | tools/list result            | server→client      | Faithful (typed)                         |
  | sampling/createMessage       | server→client req  | Faithful (representable; Custom path)    |

  * tools/list gaining `params:{}` on the OUTBOUND request is semantically inert
    (an absent optional params vs an empty object; servers treat them alike). It is
    NOT on the transform path. Recorded for honesty; needs no special handling.

  KEY FACTS for A4:
  - rmcp's `receive()` has ALREADY forced typed deserialization before we see a
    message (ServerJsonRpcMessage is the receive type) — so inbound routing is not
    our choice; we measure it, and it's faithful for tools/call results.
  - CallToolResult deserialize (model.rs:2794) requires ≥1 known field (so it won't
    shadow CustomResult in the untagged enum) and DEFAULTS content to `[]` when
    absent; serializes with skip_serializing_if on structuredContent/isError/_meta.
    The content[].text JSON-string survives VERBATIM (live-wire confirmed).
  - Unknown methods (both directions) hit Custom* (method+params preserved) — never
    rejected. So A4 needs NO method allow-list; lift every Value via from_value and
    rmcp picks typed-or-Custom. A4's only routing decision is the EXISTING one:
    "is this a correlated tools/call response?" → transform; else passthrough.
  - Construction-time worker drives send/receive with NO serve_client (live test
    passed). Handshake order initialize → (worker blocks) → initialized confirmed.
============================================================================ -->

## Open questions (resolve in plan-mode fleshing or as encountered)
- **A1 produces the per-method routing map (not a go/no-go on rmcp).** Raw transport drivability
  and session handling are *confirmed*. A1's output is the map of which methods route **typed**
  (default — best-tested, validates against spec) vs. **`Custom`** (only where typed is impossible
  or reshapes). A4 implements that split. Hand-rolled `reqwest`+`sse-stream` is the last resort,
  triggered only if some message rmcp won't carry *either* way or the handshake is incompatible
  with proxying — that case forfeits rmcp's free `auth` integration and grows Slice B. Settle the
  map before estimating B.
- **Token storage location for Slice B** — `~/.toonfmt-auth/` per-URL hash (mcp-remote's
  model) vs. OS keychain. Decide at B2.

## Acceptance criteria
<!-- What cairn-accept checks per slice before "landed". Conditions, not steps.
     Each slice is accepted independently; Slice B is gated on Slice A. -->

### Slice A (bearer/none) — landed when:
- `cargo build` and `cargo test` pass with **zero warnings** (incl. `cargo clippy -- -D
  warnings`).
- `toonfmt --http <url>` connects to a Streamable HTTP MCP server, completes the
  client-driven `initialize` handshake (toonfmt forwarding it, not authoring it), and a
  `tools/call` result returned over HTTP comes out of toonfmt's stdout with its JSON content
  block re-encoded as TOON — **identical transform behavior to the stdio path.**
- A `tools/call` result delivered as SSE (`text/event-stream`) is transformed the same as a
  plain `application/json` one, **including when the JSON spans multiple `data:` lines**
  (`probe_sse_split`) — proving the transform fires only on a complete, buffered message,
  never on a fragment.
- `--bearer-env <VAR>` sends the configured token as `Authorization: Bearer`; the token never
  appears in argv/process list. No token env → no auth header.
- Non-`tools/call` messages over the HTTP leg pass through as **structurally-equal** JSON-RPC
  (semantic passthrough — the documented amendment); the **stdio leg remains byte-identical**
  (Phase 1 property intact — existing stdio e2e + pump tests still green).
- The equality-gated `structuredContent` strip and the per-block JSON→TOON fallback behave
  identically on the HTTP path (shared, transport-blind transform).
- **Real-MCP check (A6):** a live `tools/call` against the user-supplied Databricks SQL MCP
  (token auth) returns TOON'd, data faithful; and a user-supplied SSE-streaming target
  confirms buffer-until-complete on a real server.
- `ARCHITECTURE.md` reflects the HTTP upstream, the passthrough amendment, the renumbered
  Phase 4, and OAuth as the next slice; A1's rmcp `Transport`-drivability verdict is recorded.

### Slice B (OAuth) — landed when:
- Slice A is already landed (hard dependency).
- `toonfmt --http <oauth-url> --oauth …` completes the authorization-code flow against a
  **user-supplied OAuth MCP target**: unauthorized → browser auth → token stored → a
  `tools/call` result returns TOON'd; a subsequent run reuses the cached token without
  re-prompting, and refresh-on-expiry works.
- Token storage is per-target isolated and the decision (filesystem vs. keychain) is documented.
- `cargo build`/`cargo test` zero-warning + clippy `-D warnings` clean (auth feature enabled).
- `ARCHITECTURE.md` lists OAuth as supported with its config surface and token-storage model.
