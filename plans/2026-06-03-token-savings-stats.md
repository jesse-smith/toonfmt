# Token-savings stats (opt-in) — DESIGN NOTE (pre-plan)

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B — self-update)
**Status:** drafted (design-only — tasks are provisional, confirm the open questions before coding)

## Goal
Let a user *see* what toonfmt buys them: an **opt-in** readout of how many bytes/tokens the TOON
transform saved. Models the same instinct as RTK's `gain` analytics (cumulative savings the user
values), adapted to this proxy. Opt-in because the default path must stay a silent, zero-overhead
passthrough.

## The thinking (settle these BEFORE writing tasks)
This is filed as a design note, not a ready plan, because three correctness questions decide the
whole shape:

1. **Bytes are exact; tokens are an estimate — say so.** Anthropic's tokenizer is not public, so
   a precise per-call token count is impossible to compute locally. **Decision:** report **bytes
   saved exactly**, and a token figure as an explicit estimate (`≈`, bytes/4 heuristic) — never a
   fake-precise integer. A dishonest "saved 1,234 tokens" is worse than an honest "≈300 tokens".
   (Open: is bytes/4 good enough, or pull in a real BPE tokenizer? Leaning heuristic — KISS — but
   the estimate's *visible* approximation is the load-bearing part either way.)
2. **Only count savings actually DELIVERED to the model** — or the metric lies. The transform's
   value depends on the `structuredContent` shadowing rule (ARCHITECTURE.md:34-45):
   - **strip path** (content TOON'd, redundant `structuredContent` removed) → the model now reads
     TOON instead of the JSON it would have read → **real delivered saving**. ✓
   - **`structuredContent` kept** (not structurally equal) → the model reads `structuredContent`
     regardless; TOONing the content block saved *nothing the model sees*. Counting it would
     overstate the win.
   So the measurement must distinguish "transformed" from "transformed **and** the result is what
   the model ingests." The honest baseline≈delivered metric is the strip-path delta (plus the
   content-only no-`structuredContent` case, where TOON content *is* what's read).
3. **Where the numbers live.** stdout is the protocol channel (inviolable). Options: (a) aggregate
   line to **stderr on exit** (like `login`'s output; lands in the host's per-server cache log per
   B6); (b) a sidecar `~/.toonfmt/stats.json` + a `toonfmt stats` readout (the RTK-`gain` shape,
   cross-session cumulative). **Leaning (b)** for parity with the tool the user already reasons
   about, with (a) as the cheap first step. Per-call logging is too noisy — aggregate.

## Where the data already is (no new plumbing to measure)
`transform::tools_call_result` (`src/transform.rs:46-53`) already holds, in one place, the original
block `text` and the new `toon` string, **and** knows whether it stripped `structuredContent`
(`should_strip`, line 58-72). So the raw deltas are computable at the existing transform site with
no architectural change — the work is *accounting and surfacing*, not *capturing*.

## Provisional tasks (do NOT start until the 3 questions above are decided)
- [ ] **S1 — Decide the open questions.** Bytes-vs-token estimate method; delivered-only metric
  definition; stderr-on-exit vs. sidecar+`toonfmt stats`. Record decisions here, then expand tasks.
- [ ] **S2 — Measurement at the transform site.** Return savings (orig bytes, toon bytes, whether
  delivered) from `tools_call_result` without disturbing the `Option<String>` change-detection
  contract; thread through an opt-in counter (gated by `--stats` / `TOONFMT_STATS=1`). Unit-test
  the delivered-vs-not accounting against the existing strip/keep fixtures.
- [ ] **S3 — Surface + persist** per the S1 decision; `toonfmt stats` subcommand if sidecar.
- [ ] **S4 — Docs + cairn-accept.**

## Acceptance criteria (provisional)
- Default path (no opt-in) is byte-for-byte and behaviorally identical to today — zero overhead,
  nothing on stdout, nothing written.
- Reported byte savings are exact; token figures are explicitly marked estimates.
- The metric counts only savings the model actually ingests (strip path + content-only), not
  transforms whose TOON a kept `structuredContent` shadows — verified against the keep/strip tests.
- `cargo test` + `cargo clippy -- -D warnings` green.
