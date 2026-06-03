# CLI hardening — per-profile credentials + `--help`/`--version`

**Date:** 2026-06-03  <!-- last worked on (or created); rename on meaningful revisit -->
**Prior:** plans/2026-06-02-self-update.md (Phase B — self-update; this is independent, queue after it)
**Status:** drafted  <!-- drafted | in progress | landed -->

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
- **Key derivation: fold profile into the hash, stay flat.** `key = sha256(profile ⊕ "\0" ⊕ url)`;
  default profile (flag absent) must hash to **exactly** today's `sha256(url)` so existing logins
  keep working with zero migration. Folding into the hash (not a subdirectory) is what lets a
  **path-valued** profile like `/Users/me/project` work with no sanitization or nested-dir perms —
  and it's consistent with *why the URL is already hashed* (`credential_store.rs:8-10`: no
  secret-adjacent path on the filesystem). The `0700`/`0600` perms logic is untouched.
- **No clap.** The bespoke grammar (`-- <program> args…` verbatim passthrough; mutually-exclusive
  `bearer` vs. OAuth *families*) is precisely the case clap's derive handles awkwardly, and
  Rule-of-Three says don't pull a framework in for one help string. Extend the hand-rolled
  parser and lift the scattered usage fragments into one `const USAGE: &str`.
- **`--help`/`--version` are pre-empting top-level tokens**, handled in `parse_args` *before* the
  serve/login split (like `login` at `src/cli.rs:113`), since they're valid with no upstream.
  Add a `Command::Help` / `Command::Version` pair (or a small `enum Meta`) dispatched in
  `src/main.rs:45` to print and exit 0. `--version` reads `env!("CARGO_PKG_VERSION")`.

## Tech Stack
- Edits only: `src/cli.rs` (parser, `--profile` plumbing, `const USAGE`, help/version variants),
  `src/credential_store.rs` (`for_url` takes an optional profile), `src/main.rs` (login/serve
  call sites pass the profile; dispatch help/version).
- No new dependencies. `anyhow` / `bail!` as elsewhere.

## Tasks
<!-- Concrete file paths and code where reasonable. NO placeholders. -->

- [ ] **H1 — `--help`/`-h`/`help` + `--version`/`-V`.** Lift every usage fragment into one
  `const USAGE` in `src/cli.rs`. Detect the meta tokens in `parse_args` before the command split;
  return new no-arg `Command` variants; dispatch in `src/main.rs` to print (`USAGE` to stdout for
  help; `toonfmt <version>` for version) and return `ExitCode::SUCCESS`. Tests: `--help`/`-h`/
  `help` → Help; `--version`/`-V` → Version; they win even with other args present; existing
  serve/login parse tests still green.
- [ ] **H2 — `--profile <name>` on login + serve.** Add `profile: Option<String>` to `LoginArgs`
  and the HTTP serve auth path; parse `--profile` in both `parse_login` and `parse_serve`
  (rejecting it on the stdio `--` form, like the OAuth flags at `src/cli.rs:203`). Change
  `url_hash` → `key_hash(profile, url)` = `sha256(profile ⊕ "\0" ⊕ url)` and thread the optional
  profile through `FileCredentialStore::for_url`/`new`. **Regression-critical:** flag absent must
  produce **byte-identical** the current `sha256(url)` filename (guard test: a fixed URL with no
  profile hashes to the same stem as today's `url_hash`). A path-valued profile
  (`/Users/me/project`) must work unchanged (no path separators reach the filesystem — it's
  hashed). Tests: distinct profiles → distinct files for one URL; path-valued profile is accepted;
  default == legacy stem; `--profile` rejected on stdio.
- [ ] **H3 — Docs + cairn-accept.** README: document `--profile` (and that login/serve must match
  on *both* URL and profile) + the `--help`/`--version` front door. ARCHITECTURE.md "Config
  surface" → Commands/OAuth: note per-profile keying extends the URL-match gotcha. `cargo clippy
  --all-targets -- -D warnings` clean, `cargo test` green; cairn-accept.

## Acceptance criteria
<!-- What cairn-accept checks before "landed": conditions, not steps. -->
- `toonfmt --help` and `toonfmt --version` print to stdout and exit 0 (no "no upstream" error).
- Two `login`s to the same URL under different `--profile` names produce two token files; serving
  with each `--profile` loads the matching one. No `--profile` reproduces the exact current
  `sha256(url)` filename stem (existing logins unaffected — verified, not assumed).
- A path-valued profile (`--profile ${CLAUDE_PROJECT_DIR}`, i.e. an absolute path with slashes) is
  accepted and never creates any file inside the project — creds stay under `~/.toonfmt-auth/`.
- `--profile` errors on the stdio `--` form; all prior CLI/credential-store tests still pass.
- `cargo test` + `cargo clippy -- -D warnings` green.
