#![allow(clippy::unwrap_used)]

use std::process::Command;

fn exact_case(mode: &str, stdout: &[u8], stderr: &[u8]) {
    let output = Command::new(env!("CARGO_BIN_EXE_managed-reporting-prototype"))
        .arg(mode)
        .output()
        .unwrap();
    assert!(output.status.success(), "{mode}: {:?}", output.status);
    assert_eq!(output.stdout, stdout, "{mode}: stdout");
    assert_eq!(output.stderr, stderr, "{mode}: stderr");
}

#[test]
fn static_message_keeps_exact_prefix_and_newline() {
    exact_case("static", b"", b"shards: literal\n");
}

#[test]
fn unicode_and_numeric_formatting_preserve_exact_bytes() {
    exact_case(
        "unicode",
        b"",
        "shards: α 😀 count=18446744073709551615 hex=0xffffffffffffffff\n".as_bytes(),
    );
}

#[test]
fn same_thread_recursive_display_uses_the_c_stream_lock() {
    exact_case("recursive", b"", b"shards: shards: inner\nouter\n");
}

#[test]
fn closed_fd2_returns_the_actual_io_error_in_an_owned_child() {
    exact_case("closed", b"io:EBADF\n", b"");
}

#[test]
fn formatter_failure_is_distinct_and_adds_no_fallback() {
    exact_case("formatter", b"formatter\n", b"shards: ");
}
