# CLI hardening — per-profile credentials + `--help`/`--version`

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B — self-update; this is independent, queue after it)
**Status:** landed  <!-- drafted | in progress | landed -->
**Landed:** 2026-06-03 — H1+H2+H3 in one accept pass on branch `cli-hardening`. No
  surprises vs. the brainstorm resolutions. Notes worth keeping: (1) `parse_login` and
  `parse_serve` had to change return type from `LoginArgs`/`Upstream` to `Command` so a
  `--help` mid-parse can short-circuit to `Command::Help` — `parse_args` no longer wraps
  with `.map(Command::…)`. (2) Bare `help` is matched **only** as the leading subcommand
  word (in `parse_args`), never in `meta_command`, so it can't shadow a legitimate flag
  value like `--bearer-env help`. (3) Gap D verified live: `key_hash(None, url)` reproduces
  the frozen golden `7692f47…b296c` and the existing per-URL tests pass unchanged with
  `, None` appended. (4) rustfmt isn't installed in this toolchain, but it's not in the
  verify gate (`cargo test` + `cargo build`); 99 unit + all e2e green, clippy `-D warnings`
  clean. (5) `-- echo --help` confirmed to forward `--help` to echo on the real binary.

## Goal
Two small, independent new-user / multi-identity fixes, bundled because they share one file
(`src/cli.rs`) and one acceptance pass:

1. **Per-profile OAuth credentials.** The credential store is keyed **only** by `sha256(url)`
   (`src/credential_store.rs:42-46`), so two projects authenticating to the *same* upstream URL
   with *different* identities (prod vs. staging, work vs. personal) **collide** — the second
   `toonfmt login` silently overwrites the first's token. Add an explicit `--profile <name>`
   discriminator so distinct identities for one URL coexist. **This also delivers project- vs.
   user-scoped tokens** (see Architecture) without any auto-detection — the user expresses scope
   in `.mcp.json` via `--profile ${CLAUDE_PROJECT_DIR}`.
2. **Real `--help` / `--version`.** There is **no** help or version handling today. `parse_args`
   silently ignores unrecognized pre-`--` tokens (`src/cli.rs:175`), so `toonfmt --help` falls
   through to the "no upstream selected" `bail!` and dumps a usage string *by accident*. Usage
   text lives only inside scattered `bail!` messages. A first-time user has no real front door.

## Architecture

### Resolved forks (brainstorm 2026-06-03)
- **Fork A — `profile: Option<String>` field on `HttpUpstream`** (alongside `url`), *not* a
  payload on the `HttpAuth::OAuth*` variants. Profile is part of the connection identity, parallel
  to URL; `None` = shared default, `Some(p)` = explicit profile. Accepted cost: the field is inert
  for `Bearer`/`None` auth (Fork B turns that inertness into a hard error, so it's never a silent
  dead value). Keeps the `HttpAuth` enum unit-variants untouched (no churn across every match arm).
- **Fork B — `--profile` on a non-OAuth upstream is an ERROR, not ignored.** Profile only feeds the
  credential store, which only the OAuth modes read. `--profile` with `--bearer-env`, with no auth,
  or on the stdio `--` form → `bail!` (mirrors the bearer-vs-OAuth exclusivity at
  `src/cli.rs:203`). Fail-fast over silent no-op — a meaningless flag that's quietly dropped is a
  trap.
- **Fork C — `const USAGE` is ADDITIVE; contextual `bail!`s stay.** The scattered `bail!` strings
  are *fail-fast diagnostics* ("`--bearer-env` applies to `--http` upstreams, not the stdio `--`
  form"), not generic usage fragments — collapsing them into one string would destroy the
  diagnostic specificity that *is* the hardening. Add one canonical `const USAGE` for the `--help`
  front door; leave every contextual error exactly as it reads today.

- **Explicit `--profile`, not auto-derived — because auto-detection is provably impossible.**
  Claude Code does **not** transmit config scope (user/project/local) to a spawned stdio MCP
  subprocess; verified against the Claude Code MCP docs 2026-06-03 (see memory
  `claude-code-mcp-subprocess-env`). The only project signal available is the **`CLAUDE_PROJECT_DIR`
  env var** (project root), which is *also set for a user-scoped server* running inside a project —
  so keying on it automatically would wrongly re-isolate a user-scoped server per project and force
  re-login everywhere. Fully-automatic scope detection therefore can't be built. **The scope
  decision lives in `.mcp.json` instead** (its location *is* the scope), expressed by the user:
  - **project-scoped** server → put `--profile ${CLAUDE_PROJECT_DIR}` in its `.mcp.json` args
    (Claude Code expands `${CLAUDE_PROJECT_DIR}` inside args) → tokens keyed to that project;
  - **user-scoped** server → omit `--profile` → tokens shared across all projects (today's
    behavior);
  - **same-URL multi-identity** → `--profile work` / `--profile personal`, explicit.
  The same `--profile` string must appear on `login` and `serve` (extends the **URL-match gotcha**,
  ARCHITECTURE.md:117 — now "URL **and** profile must match").
- **Default is SHARED (no profile); project-scoping is opt-in.** The collision is the minority
  case; defaulting to per-project keying would re-prompt login in every new project (surprising,
  and it auto-creates per-project state — the annoyance we're avoiding). Least-surprise +
  explicit-over-implicit ⇒ shared default.
- **Creds NEVER live in the project.** `--profile ${CLAUDE_PROJECT_DIR}` feeds the project path
  into the *key*, not the storage *location* — files always stay under `~/.toonfmt-auth/`. No
  in-project files, ever (security + the "don't litter my repo" rule).
- **Key derivation: branch on the `Option`, don't fold an empty string (Gap D).** The formula is
  **two arms, not one** — folding `profile ⊕ "\0" ⊕ url` with an empty-string default does **not**
  reproduce `sha256(url)` (the `\0` prefix changes the digest → every existing user logged out).
  Correct:
  - `None` (flag absent) → **`url_hash(url)`**, byte-identical to today's path, untouched;
  - `Some(p)` → `sha256(p + "\0" + url)` (the `\0` separator matters only in this arm, so a
    profile `"a"` + url `"bc"` can't collide with profile `"ab"` + url `"c"`).

  Folding the profiled case into the hash (not a subdirectory) is what lets a **path-valued**
  profile like `/Users/me/project` work with no sanitization or nested-dir perms — and it's
  consistent with *why the URL is already hashed* (`credential_store.rs:8-10`: no secret-adjacent
  path on the filesystem). The `0700`/`0600` perms logic is untouched. Keep the existing
  `url_hash(url)` fn as the literal `None` branch (don't rename it to `key_hash` and reimplement the
  no-profile case through it — that's what makes the Gap-F regression test non-circular).
- **No clap.** The bespoke grammar (`-- <program> args…` verbatim passthrough; mutually-exclusive
  `bearer` vs. OAuth *families*) is precisely the case clap's derive handles awkwardly, and
  Rule-of-Three says don't pull a framework in for one help string. Extend the hand-rolled
  parser and lift the scattered usage fragments into one `const USAGE: &str`.
- **`--help`/`--version` are pre-empting top-level tokens**, handled in `parse_args` *before* the
  serve/login split (like `login` at `src/cli.rs:113`), since they're valid with no upstream.
  Add a `Command::Help` / `Command::Version` pair (or a small `enum Meta`) dispatched in
  `src/main.rs:45` to print and exit 0. `--version` reads `env!("CARGO_PKG_VERSION")`.
- **Meta-token detection stops at `--` (Gap E — passthrough fidelity).** `--help`/`--version` are
  recognized **only among pre-`--` tokens**. `toonfmt -- echo --help` must pass `--help` verbatim to
  `echo` — stealing it would violate the byte-verbatim stdio passthrough guarantee (ARCHITECTURE.md
  core loop). The rule: toonfmt's own flags (including meta tokens) come before `--`; everything
  after `--` is the upstream program and its argv, untouched. Mechanically this is automatic if meta
  detection lives in the same pre-split scan as the other serve flags (the `parse_serve` loop
  already stops consuming at `--`, `src/cli.rs:174`), but H1's tests must pin it explicitly.
- **Subcommands honor `--help` too.** `toonfmt login --help`, `toonfmt update --help` (and any
  future subcommand) print `USAGE` and exit 0 — `--help` is checked inside `parse_login` /
  the `update` arm before their own arg validation, so it pre-empts "login requires a URL" etc.
  (One shared `USAGE`; subcommand-specific help text is out of scope — the canonical block covers
  all forms.)

## Tech Stack
- Edits only: `src/cli.rs` (parser, `--profile` plumbing, `const USAGE`, help/version variants),
  `src/credential_store.rs` (`for_url` takes an optional profile), `src/main.rs` (login/serve
  call sites pass the profile; dispatch help/version).
- No new dependencies. `anyhow` / `bail!` as elsewhere.

## Tasks
<!-- Concrete file paths and code where reasonable. NO placeholders. -->

- [x] **H1 — `--help`/`-h`/`help` + `--version`/`-V`.** Add one canonical `const USAGE` in
  `src/cli.rs` (additive — Fork C; leave contextual `bail!`s untouched). Detect the meta tokens in
  the **pre-`--` scan only** (Gap E); return new no-arg `Command` variants; dispatch in
  `src/main.rs` to print (`USAGE` to stdout for help; `toonfmt <version>` for version) and return
  `ExitCode::SUCCESS`. Also honor `--help` inside `parse_login` and the `update` arm before their
  own validation. Tests: `--help`/`-h`/`help` → Help; `--version`/`-V` → Version; they win even
  with other pre-`--` args present; **`-- echo --help` keeps `--help` in the upstream argv** (Gap E
  boundary); `login --help`/`update --help` → Help (pre-empts "requires a URL"/"unexpected
  argument"); existing serve/login/update parse tests still green.
- [x] **H2 — `--profile <name>` on login + serve.** Add `profile: Option<String>` to `LoginArgs`
  and to `HttpUpstream` (a field alongside `url` — Fork A); parse `--profile` in both `parse_login`
  and `parse_serve`. **Reject** `--profile` on the stdio `--` form, with `--bearer-env`, and with no
  auth (Fork B — `bail!`, mirroring the bearer/OAuth exclusivity at `src/cli.rs:203`). Add a
  profile-aware key derivation that **branches on the Option** (Gap D): `None` → existing
  `url_hash(url)` verbatim; `Some(p)` → `sha256(p + "\0" + url)`. Thread the optional profile
  through `FileCredentialStore::for_url`/`new` (keep `url_hash` as the `None` branch — don't
  reimplement it). **Regression-critical (Gap F):** anchor the no-profile case to a **frozen golden
  hex**, not a self-comparison —
  `key_hash(None, "https://x.example/mcp") == "7692f47862b33fb9640daca253ada81dc3493105bf505fc78077be6a835b296c"`
  (verified: `printf '%s' 'https://x.example/mcp' | shasum -a 256`, no trailing newline). A
  path-valued profile (`/Users/me/project`) must work unchanged (no separators reach the
  filesystem — it's hashed). Tests: distinct profiles → distinct files for one URL; path-valued
  profile accepted; `None` arm == golden hex; `Some` arm != golden hex; `--profile` rejected on
  stdio / with `--bearer-env` / with no-auth HTTP.
- [x] **H3 — Docs + cairn-accept.** README: document `--profile` (and that login/serve must match
  on *both* URL and profile) + the `--help`/`--version` front door. ARCHITECTURE.md "Config
  surface" → Commands/OAuth: note per-profile keying extends the URL-match gotcha. `cargo clippy
  --all-targets -- -D warnings` clean, `cargo test` green; cairn-accept.

## Acceptance criteria
<!-- What cairn-accept checks before "landed": conditions, not steps. -->
- `toonfmt --help` and `toonfmt --version` print to stdout and exit 0 (no "no upstream" error).
  `toonfmt login --help` / `toonfmt update --help` also print help and exit 0.
- `toonfmt -- echo --help` passes `--help` through to `echo` (meta tokens are pre-`--` only — the
  byte-verbatim passthrough guarantee holds).
- Two `login`s to the same URL under different `--profile` names produce two token files; serving
  with each `--profile` loads the matching one. No `--profile` reproduces the frozen golden stem
  `7692f47…b296c` for `https://x.example/mcp` (existing logins unaffected — verified against a
  constant, not a self-comparison).
- A path-valued profile (`--profile ${CLAUDE_PROJECT_DIR}`, i.e. an absolute path with slashes) is
  accepted and never creates any file inside the project — creds stay under `~/.toonfmt-auth/`.
- `--profile` errors on the stdio `--` form, with `--bearer-env`, and on a no-auth HTTP upstream
  (meaningful only for OAuth modes). All prior CLI/credential-store tests still pass.
- `cargo test` + `cargo clippy -- -D warnings` green.
