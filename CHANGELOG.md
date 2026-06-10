# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-06-10

### Added

- **Token-savings stats (opt-in).** A new `--stats` serve flag (or `TOONFMT_STATS=1`)
  records how many bytes the TOON transform saves, and `toonfmt stats` prints a
  cumulative, per-project readout. Off by default — the proxy stays a zero-overhead
  passthrough and writes nothing to disk until you opt in. Figures are exact bytes and
  a bytes-based percentage, never a token count; deltas are signed, so a block that
  grows is counted against you and flagged.
- **`toonfmt update`.** Self-update for installer-based installs: checks GitHub Releases
  and re-runs the installer only when a newer version exists. Non-installer installs
  (`cargo install`, source) are detected and left untouched.
- **`--profile <name>`.** Give one OAuth upstream multiple independent credential
  identities (e.g. `work` vs. `personal`, or a project-scoped token) without overwriting
  each other. Must match on both `login` and serve; rejected on non-OAuth upstreams.
- **Real `--help`/`-h` and `--version`/`-V` front door** for the CLI.

### Changed

- Stats are persisted to a small SQLite store at `~/.toonfmt/stats.db`, written by an
  async writer off the hot path.

### Internal

- Applied `rustfmt` across the tree and added a `cargo fmt --check` CI gate, with
  `rustfmt.toml` pinning the edition so formatting can't silently drift. The
  pure-formatting commit is recorded in `.git-blame-ignore-revs`.
- Added a pre-commit hook that blocks committing literal secrets/credentials while
  letting `${ENV}` references through.
- Coverage and quality hardening: a TOCTOU fix, deduplication, a tightened matcher, and
  a cognitive-complexity lint gate.

## [0.1.0] - 2026-06-02

### Added

- Initial release. A transparent MCP proxy that re-encodes `tools/call` results as
  [TOON](https://toonformat.dev) to cut the tokens a model ingests per call. Supports
  stdio and Streamable HTTP upstreams, with bearer-token or OAuth auth. Prebuilt
  installers for macOS (Apple Silicon/Intel), Linux x86-64, and Windows x86-64.

[Unreleased]: https://github.com/jesse-smith/toonfmt/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jesse-smith/toonfmt/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jesse-smith/toonfmt/releases/tag/v0.1.0
