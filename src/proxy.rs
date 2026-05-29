//! The JSON-RPC pump: spawns the upstream and forwards messages both directions.
//!
//! This phase is passthrough-only. The pump frames on newlines, classifies each
//! line for request/response correlation (trace-logged), and writes every line
//! through **byte-for-byte unchanged**. The transform site is the downstream
//! (upstream → client) flow, where a later phase will rewrite `tools/call` results.

use std::process::{ExitStatus, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::cli::UpstreamCmd;
use crate::jsonrpc::{Message, RequestTracker, classify, parse_line};
use crate::transform;

/// Copy newline-framed messages from `reader` to `writer`, invoking `on_line`
/// with a UTF-8-lossy view of each line. `on_line` decides per line:
///
/// - `None` → forward the **original raw bytes** unchanged (including the trailing
///   `\n`, or none at EOF, and any non-UTF8 content). This is the exact-fidelity
///   passthrough path; the client→upstream direction always takes it.
/// - `Some(replacement)` → write `replacement` instead, re-appending `\n` iff the
///   original line ended in one. Only the transform path allocates.
///
/// Generic over the IO types so the pump is testable over in-memory pipes.
pub async fn pump<R, W, F>(reader: R, mut writer: W, mut on_line: F) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(&str) -> Option<String>,
{
    let mut buf_reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = buf_reader
            .read_until(b'\n', &mut line)
            .await
            .context("reading upstream/client line")?;
        if n == 0 {
            break; // EOF
        }
        // Classify/transform on a lossy view. On Some, write the replacement and
        // re-append the framing newline iff the original line had one; on None,
        // forward the original bytes byte-for-byte.
        match on_line(&String::from_utf8_lossy(&line)) {
            Some(replacement) => {
                writer
                    .write_all(replacement.as_bytes())
                    .await
                    .context("writing transformed line")?;
                if line.last() == Some(&b'\n') {
                    writer.write_all(b"\n").await.context("writing line framing")?;
                }
            }
            None => {
                writer.write_all(&line).await.context("writing line through")?;
            }
        }
        writer.flush().await.context("flushing line")?;
    }
    writer.shutdown().await.ok();
    Ok(())
}

/// Spawn the upstream command and run the three-flow pump until the child exits.
/// Returns the child's exit status so the caller can propagate it.
pub async fn run(cmd: UpstreamCmd) -> Result<ExitStatus> {
    let mut child = Command::new(&cmd.program)
        .args(&cmd.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning upstream `{}`", cmd.program))?;

    let child_stdin = child.stdin.take().context("upstream stdin not piped")?;
    let child_stdout = child.stdout.take().context("upstream stdout not piped")?;
    let child_stderr = child.stderr.take().context("upstream stderr not piped")?;

    let tracker = RequestTracker::new();

    // client → upstream: record the method for each request id.
    let up_tracker = tracker.clone();
    let up = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        pump(stdin, child_stdin, move |line| {
            if let Message::Request { id, method } = parse_line(line) {
                up_tracker.record_request(id, method);
            }
            None // requests are never transformed — byte-identical passthrough
        })
        .await
    });

    // upstream → client: resolve the correlated method (trace only; inert this phase).
    let down_tracker = tracker.clone();
    let down = tokio::spawn(async move {
        let stdout = tokio::io::stdout();
        pump(child_stdout, stdout, move |line| {
            // One parse per downstream line — tool results are the largest blobs
            // we handle. Non-JSON (or non-object) → None → forward raw bytes.
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                return None;
            };
            let Message::Response { id } = classify(&value) else {
                return None;
            };
            let Some(method) = down_tracker.take_method(&id) else {
                return None; // uncorrelated response — pass through
            };
            if method != "tools/call" {
                tracing::trace!(%method, "response correlated (passthrough)");
                return None;
            }
            // The owned, already-parsed envelope goes straight to the transform —
            // no second parse. Some(replacement) rewrites the line; None forwards it.
            transform::tools_call_result(value)
        })
        .await
    });

    // upstream stderr → our stderr: raw copy, never parsed.
    let err = tokio::spawn(async move {
        let mut stderr = tokio::io::stderr();
        copy_raw(child_stderr, &mut stderr).await
    });

    let status = child.wait().await.context("waiting on upstream")?;

    // Child has exited; its stdout/stderr will hit EOF and those pumps finish.
    // Our stdin pump may still be blocked on read — abort it so we don't hang.
    up.abort();
    let _ = down.await;
    let _ = err.await;

    Ok(status)
}

/// Unframed byte copy (for stderr, which is not JSON-RPC).
async fn copy_raw<R, W>(mut reader: R, writer: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tokio::io::copy(&mut reader, writer)
        .await
        .context("copying upstream stderr")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bytes in == bytes out, across a JSON line, a non-JSON line, and a final
    /// line with no trailing newline. A `None`-returning callback is the Phase 1
    /// fidelity path.
    #[tokio::test]
    async fn pump_forwards_bytes_unchanged() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\nthis is not json\n{\"no\":\"newline\"}";
        let reader = &input[..];
        let mut output = Vec::new();

        pump(reader, &mut output, |_| None).await.unwrap();

        assert_eq!(output, input);
    }

    /// The callback sees each framed line, classified correctly.
    #[tokio::test]
    async fn pump_invokes_callback_per_line() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\nnonsense\n";
        let reader = &input[..];
        let mut output = Vec::new();
        let mut seen = Vec::new();

        pump(reader, &mut output, |line| {
            seen.push(parse_line(line));
            None
        })
        .await
        .unwrap();

        assert_eq!(seen.len(), 2);
        assert!(matches!(seen[0], Message::Request { .. }));
        assert_eq!(seen[1], Message::Other);
        assert_eq!(output, input);
    }

    /// Empty input yields empty output, no panic.
    #[tokio::test]
    async fn pump_handles_empty_input() {
        let input: &[u8] = b"";
        let mut output = Vec::new();
        pump(input, &mut output, |_| None).await.unwrap();
        assert!(output.is_empty());
    }

    /// (i) A callback returning `Some` rewrites exactly that line and re-appends
    /// the framing newline; an interleaved `None` line is forwarded verbatim.
    #[tokio::test]
    async fn pump_replaces_line_on_some() {
        let input = b"replace me\nkeep me\n";
        let reader = &input[..];
        let mut output = Vec::new();

        pump(reader, &mut output, |line| {
            if line.starts_with("replace") {
                Some("REPLACED".to_string())
            } else {
                None
            }
        })
        .await
        .unwrap();

        assert_eq!(output, b"REPLACED\nkeep me\n");
    }

    /// `Some` on a final line with no trailing newline must not invent one.
    #[tokio::test]
    async fn pump_some_preserves_absent_trailing_newline() {
        let input = b"only line no newline";
        let reader = &input[..];
        let mut output = Vec::new();

        pump(reader, &mut output, |_| Some("X".to_string()))
            .await
            .unwrap();

        assert_eq!(output, b"X");
    }

    /// (ii) Phase 1 regression guard: a callback that always returns `None` is
    /// byte-identical to the input, even for non-UTF8 and a no-trailing-newline
    /// final line.
    #[tokio::test]
    async fn pump_none_is_byte_identical() {
        let input: &[u8] = b"{\"a\":1}\n\xff\xfe non-utf8 \x00\nlast no newline";
        let reader = input;
        let mut output = Vec::new();

        pump(reader, &mut output, |_| None).await.unwrap();

        assert_eq!(output, input);
    }
}
