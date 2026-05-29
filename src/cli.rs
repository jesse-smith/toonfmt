//! Command-line argument parsing.
//!
//! The only argument shape this phase supports is `[flags] -- <program> <args...>`.
//! Flags are reserved for later phases (`--skip-tool`, `--strictness`, etc.); for now
//! everything before `--` is ignored and everything after is the upstream command.

use anyhow::{Result, bail};

/// The upstream MCP server command to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamCmd {
    pub program: String,
    pub args: Vec<String>,
}

/// Parse arguments, splitting on the first `--`. Everything after `--` is the
/// upstream command; the first such token is the program, the rest its args.
///
/// `args` should be the process arguments *excluding* argv[0].
///
/// Errors if there is no `--` separator or if nothing follows it.
pub fn parse_args(args: impl Iterator<Item = String>) -> Result<UpstreamCmd> {
    let mut after_sep = Vec::new();
    let mut seen_sep = false;
    for arg in args {
        if seen_sep {
            after_sep.push(arg);
        } else if arg == "--" {
            seen_sep = true;
        }
        // tokens before `--` are reserved for flags (future phases); ignore for now
    }

    if !seen_sep {
        bail!("missing `--` separator; usage: toonfmt [flags] -- <program> [args...]");
    }

    let mut it = after_sep.into_iter();
    let Some(program) = it.next() else {
        bail!("no upstream command after `--`; usage: toonfmt [flags] -- <program> [args...]");
    };

    Ok(UpstreamCmd {
        program,
        args: it.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> Result<UpstreamCmd> {
        parse_args(tokens.iter().map(|s| s.to_string()))
    }

    #[test]
    fn well_formed_program_and_args() {
        let cmd = parse(&["--", "uvx", "some-mcp", "--db", "x"]).unwrap();
        assert_eq!(cmd.program, "uvx");
        assert_eq!(cmd.args, vec!["some-mcp", "--db", "x"]);
    }

    #[test]
    fn well_formed_program_only() {
        let cmd = parse(&["--", "cat"]).unwrap();
        assert_eq!(cmd.program, "cat");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn flags_before_separator_are_ignored() {
        let cmd = parse(&["--skip-tool", "foo", "--", "cat"]).unwrap();
        assert_eq!(cmd.program, "cat");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn missing_separator_errors() {
        assert!(parse(&["uvx", "some-mcp"]).is_err());
    }

    #[test]
    fn empty_after_separator_errors() {
        assert!(parse(&["--"]).is_err());
    }
}
