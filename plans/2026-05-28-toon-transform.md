# TOON Transform — convert `tools/call` result text blocks to TOON

**Date:** 2026-05-28
**Status:** in progress  <!-- drafted | in progress | landed -->

## Goal
Make `toonfmt` actually earn its name: on the upstream→client flow, when a response
correlates to a `tools/call` request, parse each `content[]` **text** block, attempt
JSON → JSON5 parse, and on success replace the block's text with TOON. Every other
message — and every block that isn't convertible JSON — still passes through exactly as
in Phase 1. This is the one change Phase 1 was built to host: a single mutation site on a
known-good passthrough baseline.

This phase ships the **default-behavior transform only**. Explicitly **out of scope**
(Phase 3): the CLI flags `--skip-tool`, `--strictness {strict|json5}`,
`--structured-content {keep|strip|minify}`. Phase 2 hardcodes the documented defaults —
strictness = json5, structured-content = **equality-gated strip**, no skip list — so the
transform logic lands and is provable before the config surface is threaded through.

**structuredContent — equality-gated strip (settled 2026-05-29).** The routing probe
measured that `structuredContent` **shadows** `content` in Claude Code (present → model
sees only structuredContent), so leaving a raw-JSON structuredContent intact beside a
TOON'd content block is worse than a no-op. The default this phase: **strip
`structuredContent` iff a content block we successfully transformed parses to a `Value`
structurally equal to it; otherwise keep it (and log).** Loss-free by construction (we
verify the spec's SHOULD-equivalence per-result rather than trusting it); the check is one
`Value ==` on values we already hold (no added parse). See `ARCHITECTURE.md` for the spec
basis, the `probe_structured_string` measurement that killed "TOON-the-field" (2b), and the
order-sensitivity semantics.

Also out of scope: adding a text block when `content[]` has none (constitution's edge
case — equality-gated strip is a no-op there since nothing was transformed; the type-safe
"add a TOON block" fix is option 2a, **deferred to Phase 3**), and any TOON-encoding of the
`structuredContent` field itself (2b — measured to hard-error on Claude Code).

## Architecture
Per `ARCHITECTURE.md` "Core loop". The transform is a pure function over one downstream
line; the pump gains the ability to *replace* a line's bytes, and the downstream flow
invokes the transform only for `tools/call`-correlated responses.

- **Pump contract change (fidelity-preserving).** Today `pump`'s callback is
  `FnMut(&str)` (observe-only) and the pump always writes the original bytes. Phase 2
  changes the callback to `FnMut(&str) -> Option<String>`:
  - returns `None` → write the **original raw bytes** unchanged (the Phase 1 path; exact
    fidelity, including non-UTF8 and the no-trailing-newline-at-EOF case).
  - returns `Some(replacement)` → write `replacement`, re-appending `\n` iff the original
    line ended in `\n`. Only the transform path allocates.
  The client→upstream pump passes a closure that always returns `None` (requests are never
  transformed), so that direction stays byte-identical.
- **Single-parse downstream (no double parse).** Classification is refactored to operate
  on an already-parsed value: `jsonrpc::classify(&Value) -> Message`, with the existing
  `parse_line(&str) -> Message` kept as a thin `from_str + classify` wrapper (all current
  jsonrpc tests stay valid; the upstream request-recording path keeps using `parse_line`
  on its tiny request lines). The downstream closure does **one** `from_str::<Value>` per
  line; on parse failure it returns `None` (forward raw bytes — non-JSON path unchanged).
- **Trigger.** In the downstream closure: parse to `Value` once, `classify(&v)`; if it's a
  `Response` whose `take_method(&id)` resolves to `"tools/call"`, hand the **same owned
  `Value`** to `transform::tools_call_result` and return its `Option<String>`. Anything
  else returns `None`. This kills the redundant second parse on the largest payloads in the
  program (tool results are the big blobs we exist to shrink).
- **Transform (`src/transform.rs`).** `tools_call_result(value: Value) -> Option<String>`
  (receives the already-parsed envelope; does not re-parse):
  1. Navigate to `result`. If absent (an error response with `error`, not `result`) →
     `None`. If `result.isError == true` → `None` (error payloads are often prose).
  2. If `result.content` is not an array → `None`.
  3. Iterate `content[]`; for each object with `type == "text"` and a string `text`:
     attempt-parse the text as strict JSON, else JSON5; on success **retain the parsed
     `Value`** (for the equality gate in step 4), then replace `text` with
     `toon_format::encode_default(&parsed)`; on failure leave that block untouched
     (**per-block** fallback). Non-text blocks (image/resource/resource_link) untouched.
  4. **Equality-gated structuredContent strip.** If `result.structuredContent` is present
     *and* any retained parsed `Value` from step 3 is structurally equal to it
     (`parsed == structured`, the `serde_json::Value` `PartialEq` — object-key-order-
     insensitive, array-order-sensitive), **remove** the `structuredContent` key (its data
     now rides as TOON in the equal content block). If present but **not** equal to any
     transformed block → leave it intact and `tracing::debug!` the non-equivalent server
     (separate/partial/reordered data — never delete what we didn't preserve). Absent → no-op.
  5. If **no** block changed *and* nothing was stripped → return `None` (preserve original
     bytes; nothing to do). Otherwise re-serialize the whole envelope compact via
     `serde_json::to_string(&value)` and return `Some(...)`. Envelope formatting is
     invisible to the model — only the decoded `content` text reaches it (settled reasoning
     from the pre-Phase-2 discussion) — and `preserve_order` (below) keeps key order
     intact, so the on-wire changes are exactly the `content[].text` we rewrote and the
     removed `structuredContent` key (when stripped).
- **Strictness gradient.** strict `serde_json` → `json5`. Stop there (no json-repair —
  guessing at unbalanced braces can yield valid-but-wrong objects we'd emit confident TOON
  over). When the JSON5 path succeeds where strict failed, `tracing::debug!` it (which line
  / tool) so misbehaving upstreams are auditable.
- **structuredContent.** Equality-gated strip (step 4). Never TOON-encoded (measured to
  hard-error on Claude Code — a string in that field fails client-side Zod validation,
  `expected record, received string`). When the gate doesn't fire (absent, or present-but-
  not-equal), it rides through inside the re-serialized envelope unchanged in value.
- **Key-order fidelity.** `serde_json`'s default object map (`BTreeMap`) alphabetizes keys on
  re-serialize — valid JSON-RPC but a gratuitous envelope change. Enable the `preserve_order`
  feature so transformed envelopes keep their original key order; the only on-wire delta is
  the `content[].text` we deliberately rewrote.

## Tech Stack
- `toon-format` (0.5.0) — official TOON encoder (homepage toonformat.dev). API:
  `encode_default(&impl Serialize) -> ToonResult<String>`, accepts `serde_json::Value`.
- `json5` (1.3.1) — serde-based JSON5 deserializer; `json5::from_str::<Value>(s)` covers the
  LLM-output flavor (trailing commas, single quotes, unquoted keys, comments).
- `serde_json` — already present; envelope parse + compact re-serialize. Add the
  `preserve_order` feature (pulls `indexmap`) so re-serialized envelopes keep original key
  order instead of alphabetizing.
- Python 3 (dev/test only) — a tiny fixture MCP stub at `tests/fixtures/json_mcp_stub.py`
  emits JSON (not TOON) `content` blocks, since dbmcp is TOON-native and a no-op for the
  transform. E2E test skips with a clear message if `python3` is absent. Transform
  correctness lives in fast Rust unit tests; the fixture only proves end-to-end wiring.

## Tasks
<!-- TDD order: test before impl for each unit. -->
- [x] Add deps: `cargo add toon-format json5`; `cargo add serde_json --features preserve_order`. Confirm `cargo build` still clean. (Note: `toon-format` default feature is `cli` — pulls ratatui/image/syntect/tiktoken/arboard; disabled via `default-features = false`. `encode_default(&Value) -> Result<String>` is reachable without it. Dep tree ~10 crates, not 180.)
- [x] `src/jsonrpc.rs`: extract `classify(&Value) -> Message`; rewrite `parse_line(&str)` as a thin `from_str + classify` wrapper. Existing 9 tests must stay green unchanged; add one direct `classify` test.
- [x] `src/transform.rs`: write failing unit tests first, then `tools_call_result(value: serde_json::Value) -> Option<String>`. Tests:
  (a) single text block of strict JSON → block text becomes TOON, envelope still valid, same `id`/`result` shape;
  (b) text block of JSON5 (trailing comma) → becomes TOON;
  (c) text block of prose (non-JSON) → returns `None` (unchanged);
  (d) multi-block: [JSON text, prose text, image block] → only the JSON text converts, others byte-equal, others' values intact;
  (e) `result.isError == true` → `None`;
  (f) no `content` / `content` not an array → `None`;
  (g) **equality-gated strip — equal:** `structuredContent` present and structurally equal to the JSON text block's parsed value → text converts AND `structuredContent` key is **removed** from output;
  (g2) **strip — object keys reordered:** content block JSON and `structuredContent` have the same data but different object-key order → still equal (`Value ==` is key-order-insensitive) → **stripped**;
  (g3) **keep — not equal:** `structuredContent` differs from every transformed block (e.g. extra field, or array elements reordered → `Value ==` is array-order-sensitive) → text converts, `structuredContent` **left intact**, debug-logged;
  (g4) **keep — no transformed block:** `structuredContent` present but `content[]` has no convertible text block (prose only / empty) → nothing transformed → `structuredContent` **left intact** (no-op gate);
  (h) error response (`error`, no `result`) → `None`.
- [x] `src/proxy.rs`: change `pump`'s callback to `FnMut(&str) -> Option<String>`; on `Some`, write replacement + conditional `\n`; on `None`, write original bytes. Update the existing 3 pump unit tests; add: (i) a callback returning `Some` rewrites exactly that line and re-appends the newline; (ii) a `None`-returning callback is byte-identical (Phase 1 regression guard).
- [x] `src/proxy.rs`: wire the downstream closure — `from_str::<Value>` once (fail → `None`), `classify(&v)`; if `Response` and `take_method` resolves to `"tools/call"`, pass the owned `Value` to `transform::tools_call_result` and return its result; else `None`. Upstream closure always returns `None`. Keep trace/debug logging.
- [x] `tests/fixtures/json_mcp_stub.py`: zero-dep stdlib MCP stub — **already built** (routing-probe). Advertises `probe_content_only` (content-only JSON object string → plain convert case), `probe_both` (content JSON string + a *structurally equal* `structuredContent` object → equality-strip case), `probe_structured_only`, `probe_structured_string`. Content blocks are JSON-object strings, so they exercise the transform; `probe_both`'s content payload equals its structuredContent, exercising the strip gate. NOTE: `probe_both`'s `CONTENT_PAYLOAD` and `STRUCTURED_PAYLOAD` currently carry *different* sentinels (built to distinguish channels) — for the equality-strip e2e, either add a tool whose two channels are equal, or assert against `probe_content_only` for convert + a dedicated equal-pair tool for strip. Reconcile in the e2e task.
- [x] `tests/transform_e2e.rs`: spawn `toonfmt -- python3 tests/fixtures/json_mcp_stub.py`; drive the handshake, then: (1) call `probe_content_only` → assert the returned `content[0].text` is TOON (not the original JSON), no `structuredContent` appears; (2) call an **equal-pair** tool (added `probe_equal_pair` to the stub: content text block == structuredContent object) → assert `content[0].text` is TOON **and** `structuredContent` is **absent** in toonfmt's output (strip fired end-to-end); (3) control — a `tools/list` response passes through unchanged. Skip with a clear message if `python3` is unavailable.
- [x] Run cairn-verify; tick boxes only on a clean pass (zero warnings, clippy clean).

## Acceptance criteria
<!-- What cairn-accept checks before "landed". -->
- `cargo build` and `cargo test` pass with **zero warnings** (incl. `cargo clippy -- -D warnings` clean).
- A `tools/call` result whose `content[]` has a text block of valid JSON comes out of `toonfmt`'s stdout with that block's `text` re-encoded as TOON; the JSON→TOON is semantically faithful (same data).
- JSON5-flavored text (trailing commas / single quotes) converts; genuinely non-JSON text (prose) passes through unchanged (per-block fallback — a prose block beside a JSON block does not block the JSON block's conversion).
- `result.isError == true` results, error responses, non-`tools/call` responses, requests, and notifications all pass through **byte-identical** (Phase 1 passthrough property preserved for everything the transform doesn't own).
- Equality-gated structuredContent strip works both ways: when a transformed content block is structurally equal to `structuredContent`, the key is **removed** from output (incl. when object keys are reordered); when it differs (extra field, reordered array, or no transformed block at all), `structuredContent` is **left intact** and a debug log notes the non-equivalent server. `structuredContent` is never TOON-encoded.
- The non-strict (JSON5) parse path emits a `debug` log identifying the line/tool; stdout still carries protocol bytes exclusively (logs to stderr only).
- No CLI flags added this phase — defaults are hardcoded; flags remain Phase 3.
