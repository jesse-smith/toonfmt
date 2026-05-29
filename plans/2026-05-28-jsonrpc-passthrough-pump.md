# JSON-RPC Passthrough Pump — transparent MCP stdio proxy (no transform yet)

**Date:** 2026-05-28
**Status:** in progress  <!-- drafted | in progress | landed -->

## Goal
Stand up the `toonfmt` Rust binary as a transparent MCP proxy: it spawns an upstream
server (everything after `--`), pumps JSON-RPC both directions over stdio, and forwards
every message byte-for-byte unchanged. This is the load-bearing skeleton the TOON transform
plugs into later — proving the pump is invisible *before* any transformation exists, so
Phase 2 can change exactly one thing (the upstream→client `tools/call` result) against a
known-good baseline.

Explicitly **out of scope** (Phase 2+): TOON encoding, JSON/JSON5 parsing of `content`
blocks, `structuredContent` handling, `--skip-tool`/`--strictness`/`--structured-content`
flags. This phase ships passthrough only.

## Architecture
Per `ARCHITECTURE.md` "Core loop": match `tools/call` requests to responses by `id`; pass
everything else through. This phase builds the full pump and the id-correlation machinery
but stops short of mutating any result — every block is forwarded unchanged.

- **Transport framing.** MCP stdio is newline-delimited JSON-RPC: one message object per
  line, no embedded newlines. Framing = read lines. Non-JSON / unparseable lines forward
  verbatim (fail-safe passthrough), matching the doc's per-block fallback philosophy.
- **Three concurrent flows**, two of which carry JSON-RPC:
  - client→upstream (our stdin → child stdin): requests/notifications. The pump records the
    `method` for each request `id` here.
  - upstream→client (child stdout → our stdout): responses/notifications. This is the future
    transform site; for now, forward unchanged.
  - upstream stderr → our stderr: logging passthrough, never parsed.
- **Request correlation.** A shared `RequestTracker` (`Arc<Mutex<HashMap<RequestId, String>>>`)
  records `id → method` on the way up and resolves it on the way down, so Phase 2 can ask
  "is this response the result of a `tools/call`?" without re-parsing intent. Built now,
  exercised minimally (correlation is logged at trace level; nothing acts on it yet).
- **Shutdown.** EOF on our stdin closes child stdin; child exit (or its stdout EOF) tears
  down the remaining tasks and we exit with the child's status code. Fail fast on spawn error.
- **Concurrency model.** Tokio multi-threaded runtime; one spawned task per flow; tasks
  abstracted over `AsyncRead`/`AsyncWrite` so the pump is unit-testable over in-memory
  `tokio::io::duplex` pipes without real processes.

## Tech Stack
- `tokio` (rt-multi-thread, process, io-util, macros, sync) — async runtime, child process, pipes
- `serde` + `serde_json` — JSON-RPC envelope parse (to read `id`/`method`); value-preserving
  so re-serialization isn't required in passthrough (we forward the original bytes)
- `anyhow` — error handling at the binary boundary
- `tracing` + `tracing-subscriber` — structured logging to **stderr** (never stdout, which is the protocol channel)
- std only for CLI arg split (no `clap` yet — the only arg shape is `[flags] -- <cmd> <args...>`; revisit when flags land in Phase 2)

## Tasks
<!-- TDD order: each pump task pairs a duplex-pipe test with the impl. -->
- [x] Crate scaffold (already `cargo init`'d, edition 2024, toolchain 1.96 — keep 2024); add deps above to `Cargo.toml`; replace generated `src/main.rs` placeholder
- [x] `src/cli.rs`: `parse_args(args: impl Iterator<Item=String>) -> Result<UpstreamCmd>` splitting on the first `--`; `UpstreamCmd { program, args }`; error if no `--` or empty upstream command. Unit tests for: well-formed, missing `--`, empty after `--`
- [x] `src/jsonrpc.rs`: minimal envelope types — `RequestId` (string|number per JSON-RPC), `enum Message { Request{id, method}, Response{id}, Notification{method}, Other }` parsed leniently from a `&str` line; `parse_line(&str) -> Message` returning `Message::Other` for non-JSON (never errors). Unit tests covering each variant + a non-JSON line
- [x] `src/jsonrpc.rs`: `RequestTracker` (`Arc<Mutex<HashMap<RequestId,String>>>`) with `record_request(id, method)` and `take_method(id) -> Option<String>`. Unit test: record then take returns method; take of unknown id returns None
- [x] `src/proxy.rs`: `pump<R: AsyncRead, W: AsyncWrite>(reader, writer, on_line)` — line-framed copy that invokes a callback per line and writes the (unchanged) line through. Unit test over `tokio::io::duplex`: bytes in == bytes out, including a non-JSON line and a line without trailing newline at EOF
- [x] `src/proxy.rs`: `run(cmd: UpstreamCmd) -> Result<ExitStatus>` — spawn child (piped stdin/stdout/stderr), wire three flows (up records request methods via tracker; down resolves via tracker + trace-logs `tools/call` correlation; stderr raw copy), await child exit, propagate status. EOF on our stdin closes child stdin
- [x] `src/main.rs`: parse args → `run` → exit with child status code; init `tracing` to stderr; `anyhow` error → stderr + nonzero exit
- [x] `tests/passthrough.rs`: integration smoke test spawning `toonfmt -- cat` (cat echoes stdin→stdout), feed a canned `initialize` request line, assert it returns byte-identical; second case feeds a non-JSON line and asserts identical passthrough
- [x] Update `.gitignore` for Rust (`/target`) — already present from `cargo init`

## Acceptance criteria
<!-- What cairn-accept checks before "landed". -->
- `cargo build` and `cargo test` pass with zero warnings (incl. clippy clean)
- `toonfmt -- <upstream>` spawns the upstream and a JSON-RPC line sent to toonfmt's stdin arrives at the upstream unchanged, and the upstream's response arrives back at toonfmt's stdout **byte-identical**
- A non-JSON / unparseable line on either flow passes through unchanged (no crash, no drop)
- Upstream stderr is forwarded to toonfmt's stderr; toonfmt's own logs go to stderr only — stdout carries protocol bytes exclusively
- EOF on toonfmt's stdin closes the upstream's stdin; upstream exit propagates as toonfmt's exit code
- No TOON conversion occurs anywhere (this phase is passthrough-only by definition)
