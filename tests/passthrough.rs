//! End-to-end passthrough: drive the real `toonfmt` binary wrapping `cat`.
//!
//! `cat` echoes stdin → stdout, so whatever we write to toonfmt's stdin must come
//! back on its stdout byte-for-byte. This proves the proxy is invisible before any
//! transform exists.

use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Feed `input` to `toonfmt -- cat` and return what comes back on stdout.
async fn roundtrip(input: &[u8]) -> Vec<u8> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_toonfmt"))
        .args(["--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn toonfmt");

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).await.unwrap();
    drop(stdin); // EOF closes upstream stdin; cat exits

    let mut out = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut out)
        .await
        .unwrap();

    let status = child.wait().await.unwrap();
    assert!(status.success(), "toonfmt exited non-zero: {status:?}");
    out
}

#[tokio::test]
async fn initialize_request_passes_through_byte_identical() {
    let line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";
    let out = roundtrip(line).await;
    assert_eq!(out, line);
}

#[tokio::test]
async fn non_json_line_passes_through_unchanged() {
    let line = b"this is not json at all\n";
    let out = roundtrip(line).await;
    assert_eq!(out, line);
}

#[tokio::test]
async fn multiple_lines_pass_through_in_order() {
    let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\nnot json\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
    let out = roundtrip(input).await;
    assert_eq!(out, input);
}
