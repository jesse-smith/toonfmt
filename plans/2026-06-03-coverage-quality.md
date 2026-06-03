# Coverage + quality pass — llvm-cov baseline, 90% unit, simplify, + Codecov

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B); folds in the deferred Codecov work
  (memory `codecov-deferred`) and the deferred CI piece from `release-v0.1-dist-setup`.
**Status:** in progress  <!-- drafted | in progress | landed -->

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

## Tasks
- [x] **Q1 — Baseline + exclusion list + decisions.** *(done 2026-06-03 — see "Q1 results" above.)*
  Blended baseline 92.04%; exclusion list resolved to **`src/main\.rs` only** (the other candidates
  are line-level tangles, not clean files — recorded as a Q3 signal / accepted residual instead);
  coverable-surface baseline 92.97%. Bucket-(C) coverable gaps enumerated for Q2.
- [ ] **Q2 — Close coverage gaps on the coverable surface (~100%).** Add the *missing-intent* tests
  Q1 surfaced (not filler); add deliberate integration tests for the handshake/drain/teardown paths
  if Q1 shows them thin. Keep the suite minimal-but-complete — refactor/dedupe test helpers, don't
  bloat. (Runs **before** Q3 by design — these tests are the net for the simplify pass.)
- [ ] **Q3 — Simplify pass.** Run `/simplify` and `/code-review` (high effort) over the tree;
  apply design/clarity fixes that survive scrutiny. This is the "stress-test the implementation"
  step — record any decision that *should not* change and why (so it isn't re-relitigated).
- [ ] **Q4 — Codecov in CI + complexity gate.** Add the llvm-cov coverage upload to
  `.github/workflows/ci.yml` per the dbtoon pattern with the token/​`fail_ci_if_error` fix. Add the
  complexity gate: `clippy.toml` with `cognitive-complexity-threshold = 20`,
  `#![warn(clippy::cognitive_complexity)]` at the crate root, and the
  `#[allow(clippy::cognitive_complexity)]` + reason on `http_upstream::run` (CI's `-D warnings`
  already makes it blocking — confirm clippy stays green). Badge in README. cairn-accept.

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
