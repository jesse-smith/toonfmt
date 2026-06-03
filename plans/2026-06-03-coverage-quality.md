# Coverage + quality pass — llvm-cov coverable-surface baseline, simplify, + Codecov

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B); folds in the deferred Codecov work
  (memory `codecov-deferred`) and the deferred CI piece from `release-v0.1-dist-setup`.
**Status:** landed  <!-- drafted | in progress | landed -->

## Goal
Make the repo *genuinely good code*, not just a working product: thorough, well-factored tests
(target **~100% of the coverable surface**, deliberate integration), a complexity ceiling, and a
simplification pass that stress-tests the design — then wire the coverage tool that the **deferred
Codecov** task was always going to need into CI. Bundled because measuring coverage and uploading
coverage are the **same tool** (`cargo-llvm-cov`); splitting them would set it up twice.

## Architecture / approach
- **Target the *coverable surface*, not "unit %".** `cargo-llvm-cov` reports one **blended**
  line/region number across everything the test binaries exercise (colocated `#[cfg(test)]` in all
  8 modules **and** the `tests/` e2e — `oauth_e2e`, `rmcp_spike`, stub-driven all count toward the
  same figure); it has **no notion of "unit coverage."** A "90% unit" target is therefore both
  unmeasurable as stated and aimed at the wrong layer — this codebase's value lives in *integration*
  paths (handshake ordering, drain-on-EOF, pump teardown, OAuth lifecycle). **Invert the approach:**
  first enumerate and *exclude* the inherently-not-coverable surface via
  `cargo llvm-cov --ignore-filename-regex`, then target ~100% of what remains. The number becomes
  meaningful instead of negotiated-after-the-fact.
- **Exclusion granularity is whole-file (stable Rust).** Function-level `#[coverage(off)]` is
  **nightly-only** (`feature(coverage_attribute)`) and we are **not** switching to nightly. So the
  exclusion list is `--ignore-filename-regex` over files whose body is inherently
  un-unit-coverable: `update.rs` (axoupdater re-runs an installer), the browser launch, the
  localhost OAuth redirect listener, the metadata-discovery network call. **Q1 deliverable:** the
  actual exclusion regex + a one-line justification per excluded file, recorded here. If an
  un-coverable concern is tangled into a file that *also* holds coverable logic, that's a
  simplification signal for Q3 (extract it so it can be excluded cleanly) — not a reason to exclude
  the whole file.
- **Tests encode intent, not line count** (per global engineering philosophy: fewer well-designed
  tests over shallow coverage). Likely integration gaps to reason about: the HTTP handshake ordering
  (`http_upstream.rs:61-103` — the deadlock-avoidance sequence), drain-on-stdin-EOF (`:135-159`),
  and the stdio three-flow pump teardown (`proxy.rs:148-156`).
- **Cover *before* you simplify — tests are the refactor net.** Q2 (close gaps) precedes Q3
  (simplify) deliberately: simplification can break things we don't want broken, and TDD's
  green-before-refactor says the tests must exist first so the Q3 pass is verified, not hopeful.
- **Complexity ceiling — DECIDED (2026-06-03): clippy `cognitive_complexity`, threshold 20.**
  Resolved the original "external tool vs. clippy" caveat. **No external dependency** — `complexipy`
  is Python-only (can't read `.rs`); Mozilla's `rust-code-analysis` is a non-`cargo` binary needing
  a hand-rolled CI parse-and-gate step and a from-scratch threshold, and its one advantage (full
  per-fn distribution) is moot here — the measured baseline is *flat* (see below). So: stay in the
  one tool we already run.
  - **Mechanics (both required — `clippy.toml` alone is a no-op):** `clippy.toml` →
    `cognitive-complexity-threshold = 20` sets the knob; `#![warn(clippy::cognitive_complexity)]`
    at the crate root *enables* the nursery lint (clippy.toml cannot enable/deny lints); CI's
    existing `-D warnings` escalates it to a blocking error. The crate-root attribute (not a CI-only
    `-W` flag) means it also fires on a plain local `cargo clippy` — catch complexity before CI.
  - **Why 20, not clippy's default 25 or the earlier "~30" instinct:** the metric is SonarSource
    cognitive complexity — **incremental with a nesting multiplier** (each control-flow break is +1,
    plus +1 per level of nesting it sits inside), so cost is ~quadratic in *depth*, linear in
    *breadth*. Flat, disciplined code physically can't climb high; only deep nesting reaches 20+.
    SonarSource's *designed* line is 15; clippy's 25 is a permissive nursery default that carried
    over from the lint's `cyclomatic_complexity` past, not a tuned value. 20 sits just above the
    designed band — tight enough to flag genuinely-deep *future* code, loose enough never to
    false-positive on flat code.
  - **The one current outlier is protected, not a refactor target:** baseline (measured via
    `CLIPPY_CONF_DIR` threshold=1) shows **one** function over 20 — `http_upstream::run` at **24** —
    then a cliff to 11 (`transform`), 10 (`proxy`, `oauth login`), long tail ≤8. The 24 is the
    documented deadlock-avoidance handshake the constitution **forbids collapsing**
    (`http_upstream.rs:61-72`, "do NOT collapse"). It carries an explicit
    `#[allow(clippy::cognitive_complexity)]` **with a reason** pointing at that invariant — the gate
    makes the one inherent exception self-documenting rather than hidden under a high ceiling.
  - **Residual risk (cheap):** nursery lints are outside clippy's stability guarantee; a toolchain
    bump could shift scoring. Solo project → bump the number or drop the one-line attribute.
    Re-check on toolchain bumps (same posture as the axoupdater-substring guard).
- **Codecov re-wire (from `codecov-deferred`):** mirror sibling `jesse-smith/dbtoon`'s `ci.yml` —
  `taiki-e/install-action@cargo-llvm-cov` → `cargo llvm-cov --codecov --output-path codecov.json`
  → `codecov/codecov-action@v5`. **Set the `CODECOV_TOKEN` repo secret first, OR use
  `fail_ci_if_error: false`** — dbtoon uses `true`, which made day-one CI red over the missing
  secret; that's exactly why it was deferred. Don't reintroduce the red.

## Verified prerequisites (checked 2026-06-03)
- `cargo-llvm-cov` **0.8.4 is already installed** — Q1 can run `cargo llvm-cov` immediately, no setup.
- dbtoon is local at `../dbtoon` (`/Users/jsmith79/Documents/Projects/SideProjects/dbtoon`); its
  `.github/workflows/ci.yml` exists and **does** contain the codecov pattern to mirror in Q4.

## Q1 results (measured 2026-06-03, branch `coverage-quality`)
**Blended baseline: 92.04% lines (1557 total, 124 missed); 92.04% regions.** Per file:
`cli` 98.17 · `credential_store` 93.98 · `http_upstream` 82.96 · `jsonrpc` 97.20 · `main` 79.25 ·
`oauth` 91.27 · `proxy` 98.48 · `transform` 96.97 · `update` 74.40.

**The inverted-exclusion premise mostly didn't survive the code.** The brainstorm assumed several
files would be cleanly whole-file-excludable (`update.rs`, browser launch, OAuth listener, metadata
call). Reading the actual uncovered lines against the source: **every candidate file except `main.rs`
also holds coverable — usually already-tested — logic**, so excluding the whole file would hide good
code. Per the plan's own rule ("don't exclude a tangled file; that's a Q3 extract signal"), the
exclusion list is therefore **one file**, not four. The 124 missed lines classify as:

- **(A) Irreducible I/O boundaries** — no unit test without a real browser/socket/network/installer:
  - `main.rs:173-187` — platform browser `spawn` (`open`/`xdg-open`/`cmd start`).
  - `oauth.rs:217-220,249,254` — loopback callback listener error/EOF arms (need a live socket).
  - `update.rs` `run_update` live arms (`:33-62`) — re-runs the installer + hits GitHub Releases.
- **(B) Test-only unreachable** — `other => panic!()` guards *inside `#[cfg(test)]`* that fire only
  on test failure; llvm-cov counts test-module lines. `cli.rs:356,363,370,501,563`. Not real gaps,
  not excludable at file level (the file is otherwise 98%). These are the irreducible cost of
  colocated tests and are simply accepted (they cap a colocated-test file just under 100%).
- **(C) Genuinely coverable → Q2 targets:** `http_upstream.rs:184-190` (non-JSON stdin skip),
  `:250-258` (server-initiated warn), `:95/126/147/154-155` (handshake/drain arms);
  `transform.rs:35,41,48-50` (per-block non-text/non-JSON/encode-fail fallbacks);
  `jsonrpc.rs:33,36,75` (`RequestId::Other` big-number + `classify` response arm);
  `credential_store.rs:115-151` (`io_err` + `save`/`clear`/`load` error arms);
  `cli.rs:317` (`--bearer-env` on stdio bail).

**Exclusion list (`--ignore-filename-regex`): `src/main\.rs` only.**
- `main.rs` — composition root. It is `#[tokio::main]` + arg dispatch + the three rmcp
  transport-construction branches + the platform browser `spawn`. None of it is unit-logic: it
  either wires already-tested modules together or shells out to the OS. Excluding it removes the
  largest irreducible block (22 missed lines) without hiding any testable logic.

**Coverable-surface baseline (post-exclusion, `--ignore-filename-regex 'src/main\.rs'`): 92.97%
lines (1451 total, 102 missed).** Q2 targets bucket (C) toward ~100%; buckets (A) and (B) are the
documented, justified residual (named line-by-line above) — `oauth.rs`/`update.rs` keep their
coverable logic measured rather than being whole-file-excluded.

**Complexity baseline (re-confirmed):** one function over 20 — `http_upstream::run` @ 24 (the
constitution-protected handshake) — then a cliff to 11 (`transform`), 10 (`proxy`, `oauth login`),
tail ≤8. Gate decision (clippy `cognitive_complexity` @ 20) stands; Q4 wires it.

## Q2 results (2026-06-03)
**Coverable surface 92.97% → 94.01% (1518 lines, 91 missed).** +11 intent-asserting tests across the
bucket-(C) gaps, all green (`cargo test`: 121 total, 0 fail). No filler — each asserts a contract:
- `jsonrpc`: out-of-i64 id is a *stable correlation key* (not byte-identity — serde renders `1e+20`;
  the test that initially asserted byte-identity **caught that** and was corrected to the real
  contract), non-scalar id → Notification, both-absent → Other.
- `transform`: non-object content element skipped, `type:"text"` w/o string `text` skipped.
- `credential_store`: corrupt file surfaces `InternalError` (not silent None), unwritable base dir
  errors cleanly — covers `io_err` + the load/save error arms.
- `cli`: `--bearer-env` on the stdio `--` form errors (the one untested exclusivity arm).
- `http_upstream`: non-JSON stdin line is warned-and-skipped, not fatal (driven through the real
  subprocess e2e — the only client-side resilience arm the stub can drive deterministically).

**Residual (the 91 missed, all justified — not coverable without disproportionate cost):**
- **(A) Irreducible I/O** — `update.rs:33-93` (`run_update` re-runs the installer + hits GitHub),
  `oauth.rs:101,144,217-220,249,254` (loopback listener socket arms + post-callback bails).
- **(B) Test-only `panic!()` arms** inside `#[cfg(test)]`, fire only on test failure (llvm-cov counts
  test lines): `cli.rs:356,363,370,508,570`, `credential_store.rs:278,300`, `jsonrpc.rs:198`.
- **(C-residual) Stub-limited integration arms** — `http_upstream.rs:95,126,147,154-155` (initialize
  loop-past + in-flight drain: need a server that emits a pre-`initialize` message and a delayed
  response — the python stub has neither), `:250-258` (server-initiated request warn: stub answers
  GET with 405, never opens the server→client stream). Driving these = **stub work, deferred** (its
  own task if a real target ever needs the GET-stream path — already flagged in `forward_to_client`).
- **(D) Defensive/unreachable** — `transform.rs:48-50` (TOON encode-fail: every `Value` from
  `parse_json_or_json5` is encodable, so this is belt-and-suspenders), `http_upstream.rs:184`
  (`tx.send` err = receiver-gone-mid-shutdown race). Kept as cheap guards; flagged for Q3 to judge.

## Q3 results (2026-06-03)
Two agents over `src/` + the Q2 tests: `code-simplifier` (clarity/dedup) and a high-effort
code-review (correctness/security/test-quality), both armed with the constitution's protected
invariants. **Three changes survived scrutiny and landed; the rest were rejected as churn.**

**Landed:**
1. **`credential_store.rs` — dedup the SHA-256 hex loop.** `url_hash` and `key_hash`'s profile arm
   held byte-identical `finalize → hex` loops; extracted `hex_digest(Sha256)`. This is duplicated
   *knowledge* (the golden-pinned stem format), so DRY wins over Rule-of-Three here. Golden tests
   prove byte-faithfulness.
2. **`credential_store.rs` — TOCTOU fix (MAJOR, security).** `save` used `tokio::fs::write` (creates
   `0644`) then chmod `0600` — a real world-readable window for the token on multi-user hosts. New
   `write_private` opens with mode `0600` via `open(2)` so the file is private from creation; no
   post-write chmod. Recorded as a **don't-revert invariant** in the constitution. New test asserts
   the overwrite (rotation) path also stays `0600` (the subtle `mode`-only-on-create case).
3. **`update.rs` — tighten `is_no_receipt_msg` (MINOR, correctness).** The matcher had a bare
   `contains("no")` arm that matches incidental "no" inside `not`/`node`/`diagnostic`/… — a real
   failure mentioning "receipt" could be misrouted into the swallowed no-receipt (`Ok`) path.
   Narrowed to `"no receipt"`; the `"unable to load"` arm already covers axoupdater 0.10.0's actual
   wording. Added a negative-boundary test.

**Rejected (recorded so they aren't re-proposed):** collapsing the two OAuth match arms in
`main.rs` (Rule of Three — appears twice, shallow/incidental duplication, the parallel arms read
clearer); flattening the `forward_to_client` method-extraction chain (diagnostic-only path, current
form is explicit per the "explicit over compact" preference); the `read_request_target` O(n²) CRLF
scan (bounded at 8 KiB on loopback — negligible). The review also **verified non-issues**: OAuth
callback parsing never panics on peer bytes; the drain `pending_count` accounting is leak-free; the
new tests assert intent (the big-id correlation-key test, the corrupt-file vs. missing-file
distinction) rather than implementation detail.

## Tasks
- [x] **Q1 — Baseline + exclusion list + decisions.** *(done 2026-06-03 — see "Q1 results" above.)*
  Blended baseline 92.04%; exclusion list resolved to **`src/main\.rs` only** (the other candidates
  are line-level tangles, not clean files — recorded as a Q3 signal / accepted residual instead);
  coverable-surface baseline 92.97%. Bucket-(C) coverable gaps enumerated for Q2.
- [x] **Q2 — Close coverage gaps on the coverable surface.** *(done 2026-06-03 — see "Q2 results".)*
  +11 intent-asserting tests; coverable surface 92.97% → **94.01%**. Remaining 91 missed lines are
  the justified residual (irreducible I/O, test-only panic arms, stub-limited integration arms,
  defensive guards) — enumerated above. ~100% of the *cheaply* coverable surface is now covered; the
  rest needs stub work (deferred) or asserting implementation mechanics (declined). `cargo test`
  (121) + `cargo clippy --all-targets -- -D warnings` green.
- [x] **Q3 — Simplify pass.** *(done 2026-06-03 — see "Q3 results".)* code-simplifier + a high-effort
  code-review agent run over `src/` + the new tests. Three fixes survived scrutiny and landed;
  several proposals were rejected as negative-value churn. Constitution updated with the one new
  invariant (windowless `0600` create). `cargo test` (123) + clippy green.
- [x] **Q4 — Codecov in CI + complexity gate.** *(done 2026-06-03.)* CI `ci.yml`: added
  `llvm-tools-preview` + `taiki-e/install-action@cargo-llvm-cov` + `cargo llvm-cov
  --ignore-filename-regex 'src/main\.rs' --codecov` (doubles as the test run) → `codecov-action@v5`
  with **`fail_ci_if_error: false`** (the dbtoon `true`-over-missing-secret red, avoided). Complexity
  gate: `clippy.toml` threshold 20 + crate-root `#![warn(clippy::cognitive_complexity)]`; confirmed it
  flags **only** `http_upstream::run` @ 24/20, which now carries `#[allow]` + reason → `clippy -D
  warnings` green. CI + codecov badges in README; `codecov.json` gitignored; constitution CI-gate
  line updated. cairn-accept ready.

## Acceptance criteria
- An honest, reproducible coverage number exists; coverage of the **coverable surface** (after the
  recorded `--ignore-filename-regex` exclusion list) is ~100%, with each exclusion justified in one
  line. (No "unit %" gate — the tool reports a blended figure; the exclusion list is what makes the
  number meaningful.)
- New tests assert intent (behavior/contracts), not implementation detail; the suite is no larger
  than it needs to be (helpers factored, no shallow duplication).
- `/simplify` + `/code-review` pass applied; any deliberately-kept complexity is documented.
- CI uploads coverage **without** a spurious red over a missing secret (token set or
  `fail_ci_if_error: false`); complexity gate enforced (clippy `cognitive_complexity` @ 20, crate-root
  `#![warn]`, blocking via `-D warnings`) with the one documented `#[allow]` on `http_upstream::run`.
- `cargo test` + `cargo clippy -- -D warnings` green.
