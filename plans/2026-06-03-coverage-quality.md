# Coverage + quality pass — llvm-cov baseline, 90% unit, simplify, + Codecov

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B); folds in the deferred Codecov work
  (memory `codecov-deferred`) and the deferred CI piece from `release-v0.1-dist-setup`.
**Status:** drafted  <!-- drafted | in progress | landed -->

## Goal
Make the repo *genuinely good code*, not just a working product: thorough, well-factored tests
(target **90%+ unit**, deliberate integration), a complexity ceiling, and a simplification pass
that stress-tests the design — then wire the coverage tool that the **deferred Codecov** task was
always going to need into CI. Bundled because measuring coverage and uploading coverage are the
**same tool** (`cargo-llvm-cov`); splitting them would set it up twice.

## Architecture / approach
- **Baseline first, then target.** Run `cargo-llvm-cov` to get an honest current number before
  setting goals — the code is already well-tested (colocated `#[cfg(test)]` in all 8 modules +
  `tests/` e2e: `oauth_e2e`, `rmcp_spike`, stub-driven), so the gap may be small and concentrated.
  Find *what* is uncovered, don't chase a percentage blindly.
- **Tests encode intent, not line count** (per global engineering philosophy: fewer well-designed
  tests over shallow coverage). Likely integration gaps to reason about, not just unit %:
  the HTTP handshake ordering (`http_upstream.rs:61-103` — the deadlock-avoidance sequence),
  drain-on-stdin-EOF (`:135-159`), and the stdio three-flow pump teardown (`proxy.rs:148-156`).
- **Complexity ceiling — honest caveat.** Rust has **no clean built-in cyclomatic-complexity
  gate**: clippy's `cognitive_complexity` is nursery/unstable, not a `-D warnings` citizen. A hard
  threshold means an external tool (`rust-code-analysis` or `cargo-geiger`-adjacent). **Decide in
  Q1** whether a CI-enforced numeric gate is worth the dependency, or whether the `/simplify` +
  `/code-review` pass plus clippy is sufficient design pressure. Don't over-engineer a gate.
- **Codecov re-wire (from `codecov-deferred`):** mirror sibling `jesse-smith/dbtoon`'s `ci.yml` —
  `taiki-e/install-action@cargo-llvm-cov` → `cargo llvm-cov --codecov --output-path codecov.json`
  → `codecov/codecov-action@v5`. **Set the `CODECOV_TOKEN` repo secret first, OR use
  `fail_ci_if_error: false`** — dbtoon uses `true`, which made day-one CI red over the missing
  secret; that's exactly why it was deferred. Don't reintroduce the red.

## Verified prerequisites (checked 2026-06-03)
- `cargo-llvm-cov` **0.8.4 is already installed** — Q1 can run `cargo llvm-cov` immediately, no setup.
- dbtoon is local at `../dbtoon` (`/Users/jsmith79/Documents/Projects/SideProjects/dbtoon`); its
  `.github/workflows/ci.yml` exists and **does** contain the codecov pattern to mirror in Q4.

## Tasks
- [ ] **Q1 — Baseline + decisions.** `cargo-llvm-cov` baseline number + per-module gaps. Decide:
  the complexity-gate question (external tool vs. clippy+review), and the unit/integration split to
  target. Record here before refactoring.
- [ ] **Q2 — Close coverage gaps to 90%+ unit.** Add the *missing-intent* tests Q1 surfaced
  (not filler); add deliberate integration tests for the handshake/drain/teardown paths if Q1
  shows them thin. Keep the suite minimal-but-complete — refactor/dedupe test helpers, don't bloat.
- [ ] **Q3 — Simplify pass.** Run `/simplify` and `/code-review` (high effort) over the tree;
  apply design/clarity fixes that survive scrutiny. This is the "stress-test the implementation"
  step — record any decision that *should not* change and why (so it isn't re-relitigated).
- [ ] **Q4 — Codecov in CI + (optional) complexity gate.** Add the llvm-cov coverage upload to
  `.github/workflows/ci.yml` per the dbtoon pattern with the token/​`fail_ci_if_error` fix; add the
  complexity gate iff Q1 decided for it. Badge in README. cairn-accept.

## Acceptance criteria
- An honest, reproducible coverage number exists; unit coverage ≥ 90% (or a documented reason a
  specific path is integration-only / not unit-coverable).
- New tests assert intent (behavior/contracts), not implementation detail; the suite is no larger
  than it needs to be (helpers factored, no shallow duplication).
- `/simplify` + `/code-review` pass applied; any deliberately-kept complexity is documented.
- CI uploads coverage **without** a spurious red over a missing secret (token set or
  `fail_ci_if_error: false`); complexity gate either enforced or explicitly deferred with reason.
- `cargo test` + `cargo clippy -- -D warnings` green.
