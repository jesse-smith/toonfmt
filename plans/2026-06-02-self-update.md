# Self-update — in-binary `toonfmt update` subcommand

**Date:** 2026-06-02  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-release-v0.1.md (Phase A — v0.1 release, landed; this was its deferred Phase B)
**Status:** landed  <!-- drafted | in progress | landed -->
**Landed:** 2026-06-03 — axoupdater **0.10.0** (no API drift vs. dbtoon's 0.9). Two
  corrections to the plan's guidance, both found by verifying rather than assuming:
  (1) `run_update` must be dispatched via `tokio::task::spawn_blocking` — `run_sync`
  calls `block_on` internally and panics under `#[tokio::main]` (the plan said "call
  directly without .await"; the unit tests alone don't catch it, only the real binary);
  (2) 0.10.0 reworded the no-receipt error ("Unable to load receipt for app <name>"),
  matching none of the ported 0.9 substring matchers → added an "unable to load" arm
  plus a regression test pinning the verbatim wording. Also made `no_receipt_returns_ok`
  hermetic/offline via `AXOUPDATER_CONFIG_PATH` (it was loading a stray receipt + hitting
  the network). 80 unit tests green, clippy `-D warnings` clean, both real-binary paths
  verified (no-receipt → cargo guidance exit 0; real receipt → live "up to date" exit 0).

## Goal
Let a user who installed `toonfmt` via the v0.1.0 shell/PowerShell installer upgrade in place
with `toonfmt update`, instead of re-running the install one-liner by hand. This was carved out
of the release plan as Phase B because it can't be tested until a real release with installer
**receipts** exists — that gate is now cleared (v0.1.0 shipped 2026-06-02, the `dist` installer
writes an `~/.config/toonfmt/.../*-receipt.json`). On a source / `cargo install` build (no
receipt) the command exits gracefully with guidance rather than erroring or clobbering.

## Architecture
Mirrors the sibling project `jesse-smith/dbtoon`'s `src/update.rs` (read verbatim 2026-06-02),
which is the working reference for this exact pattern. Decisions:

- **`axoupdater` as a library, not dist's standalone updater.** The release config sets
  `install-updater = false` (recorded in ARCHITECTURE.md "Language / distribution" + memory
  `release-v0.1-dist-setup.md`); self-update is an **in-binary subcommand**, preserving the
  single-binary ethos (no second `toonfmt-update` executable). axoupdater reads the install
  receipt the dist installer already wrote, checks GitHub Releases, and re-runs the installer.
- **Binary crate, not lib.** toonfmt is a binary (`src/main.rs` with `mod` declarations; **no
  `src/lib.rs`** — corrects the old sketch). So the new module is `mod update;` in `main.rs`,
  and `run_update` is called from the `Command::Update` dispatch arm — *not* `pub mod` in a lib.
- **`update` is a third top-level command.** The CLI already splits `parse_args -> Command`
  with `Command::{Serve(Upstream), Login(LoginArgs)}` (`src/cli.rs:94`, dispatched at
  `src/main.rs:46`). Add `Command::Update` as a no-argument sibling, distinguished by a leading
  `update` token exactly as `login` is (`src/cli.rs:113`). No upstream/auth flags apply.
- **Known fragility — carried forward, not fixed.** axoupdater surfaces errors as miette
  diagnostics; it does **not** expose a matchable error enum. dbtoon classifies no-receipt /
  network / no-installer cases by lowercasing `e.to_string()` and substring-matching. This is
  brittle across axoupdater versions — port it **with a comment** flagging "re-verify these
  string matches on any axoupdater bump." (`AxoupdateError` is the error type to match on.)
- **axoupdater version — decision for B1.** dbtoon pins `0.9`; latest is **0.10.0** (verified
  2026-06-02). User preference is at-latest. **Plan: try `0.10.0` first**; if the dbtoon-derived
  code (the `AxoupdateError` type path, `AxoUpdater::new_for`, `set_current_version`,
  `load_receipt`, `run_sync`, `Version` re-export) doesn't compile cleanly against 0.10, fall
  back to the proven `0.9`. Record the chosen version + any 0.10 API drift in memory.
- **`new_for("toonfmt")`** — the app name must match the dist package name (`toonfmt`) so
  axoupdater finds the right receipt + release assets.

## Tech Stack
- **`axoupdater`** (`default-features = false`, features `["github_releases", "blocking"]`) —
  target `0.10.0`, fallback `0.9`. The `blocking` feature lets `run_update` be a plain `fn`
  (no async runtime needed for the update path), matching dbtoon.
- **`anyhow`** — already a dep; `Result` / `bail!` as in the rest of the crate.
- New file `src/update.rs`; edits to `src/cli.rs` (parser + enum) and `src/main.rs` (mod + dispatch).
- The already-published `v0.1.0` GitHub Release + its installer receipts are the live test fixture.

## Tasks
<!-- Concrete file paths and code where reasonable. NO placeholders. -->

- [x] **B1 — `axoupdater` dep + `Command::Update` CLI variant.** Add `axoupdater` to
  `Cargo.toml` (try `0.10.0`, features `["github_releases", "blocking"]`, `default-features = false`;
  fall back to `0.9` if it won't build). In `src/cli.rs`: add `Update` to `enum Command`
  (`src/cli.rs:94`); in `parse_args` (`src/cli.rs:108`) detect a leading `update` token the same
  way `login` is detected (`src/cli.rs:113`) and return `Command::Update`, erroring on any extra
  args (`toonfmt update` takes none). Red-green parser tests in the existing `mod tests`
  (`src/cli.rs:227`): `update` parses to `Command::Update`; `update --http x` / `update foo`
  error; existing serve/login/stdio/bearer tests still pass. Update the top-level usage string
  if it enumerates commands.
- [x] **B2 — `src/update.rs` (`run_update`) + main dispatch.** New `src/update.rs` mirroring
  dbtoon's: `AxoUpdater::new_for("toonfmt")`, `set_current_version(env!("CARGO_PKG_VERSION").parse()?)`,
  `load_receipt()` with the graceful no-receipt branch (print "installed via cargo/source →
  update that way", return `Ok(())` — **never** error or clobber), then `run_sync()` matching
  `Ok(Some(result))` (print `old => new`) / `Ok(None)` (already current) / `Err(e)` classified by
  the ported `is_no_receipt` / `is_network_error` / `is_no_installer_error` substring helpers.
  **Add the "re-verify string matches on axoupdater bump" comment** above those helpers. Port
  dbtoon's `no_receipt_returns_ok` test (in the test env there's no receipt → `run_update()` must
  return `Ok`). Wire it up: `mod update;` in `src/main.rs` (alongside `mod cli;` etc. at
  `src/main.rs:6`) and a `Command::Update => update::run_update()` arm in the dispatch
  (`src/main.rs:46`) — note the serve/login arms are `async`; `run_update` is sync (blocking
  feature), so call it directly without `.await`.
- [x] **B3 — Docs + cairn-accept (self-update).** Document `toonfmt update` in `README.md` (a
  short "## Updating" section: works for installer-based installs; source/`cargo install` users
  update the way they installed) and in `ARCHITECTURE.md` "Config surface" → Commands (currently
  lists serve + login; add update + the receipt-only caveat). Update memory
  `release-v0.1-dist-setup.md` to note Phase B landed + the chosen axoupdater version.
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test` green; cairn-accept.

## Acceptance criteria
<!-- What cairn-accept checks before "landed": conditions, not steps. -->
- `toonfmt update` exists as a third top-level command; `parse_args` returns `Command::Update`
  for `update` and rejects extra args; all prior CLI tests still pass.
- On this dev machine (a `cargo`/source build, **no** receipt): `toonfmt update` prints the
  "installer-based only / update via cargo" guidance and exits **0** — it does not error, panic,
  or attempt a download.
- On an installer-based install (the published v0.1.0 shell installer into a temp `CARGO_HOME`,
  as in release task A6): `toonfmt update` either reports "already up to date" (when on the
  latest tag) or performs an in-place update — verified end-to-end against the real Release.
- Network-failure and no-installer-for-platform paths produce a clear one-line error (via the
  classified `bail!`), not a panic or a raw miette dump.
- No regression in serve / login / transform paths: `cargo test` (all suites) + `cargo clippy
  -- -D warnings` green.
