//! Runs the `tachy` binary with bad arguments. Every case here fails before
//! the terminal UI starts, so no TTY is needed.

use std::process::{Command, Output};

fn tachy(args: &[&str]) -> Output {
    let data = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("tachy-cli-test");
    Command::new(env!("CARGO_BIN_EXE_tachy"))
        .args(args)
        // Keep logs and config out of the user's directories.
        .env("TACHY_DATA", &data)
        .env("TACHY_CONFIG", &data)
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run tachy")
}

fn assert_usage_error(args: &[&str], message: &str) {
    let out = tachy(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
    assert!(stderr.contains(message), "{args:?}: {stderr}");
}

#[test]
fn stdin_only_once() {
    assert_usage_error(&["-", "-"], "stdin (-) can only be given once");
}

#[test]
fn multi_byte_delimiter() {
    assert_usage_error(
        &["-d", "||", "x.csv"],
        "multi-byte delimiters are not supported",
    );
}

#[test]
fn non_ascii_delimiter() {
    assert_usage_error(
        &["-d", "é", "x.csv"],
        "delimiter must be a single ASCII byte",
    );
}

#[test]
fn zero_memory() {
    assert_usage_error(&["-m", "0", "x.csv"], "size must be greater than zero");
}

#[test]
fn header_conflicts_with_no_header() {
    assert_usage_error(&["--header", "--no-header", "x.csv"], "--no-header");
}

#[test]
fn unknown_theme_lists_known_ones() {
    assert_usage_error(&["--theme", "light", "x.csv"], "dark");
}

#[test]
fn bad_quote() {
    assert_usage_error(&["-q", "x", "x.csv"], "quote must be");
}

#[test]
fn old_template_flags_are_gone() {
    assert_usage_error(&["--tick-rate", "4", "x.csv"], "--tick-rate");
    assert_usage_error(&["--frame-rate", "60", "x.csv"], "--frame-rate");
}

#[test]
fn missing_tmp_dir_is_a_runtime_error() {
    let out = tachy(&["--tmp", "/definitely/not/here", "x.csv"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");
}

#[test]
fn version_prints_build_info() {
    let out = tachy(&["-V"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
    assert!(stdout.contains("Config directory"), "{stdout}");
}

#[test]
fn help_lists_every_option() {
    let out = tachy(&["--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0));
    for text in [
        "Files to open; each opens in its own tab. Use - for stdin.",
        "files tachy starts empty: open one with Ctrl-o.",
        "[FILE]...",
        "--delimiter <CHAR>",
        "Field delimiter. Accepts a literal or: tab, comma, pipe, semicolon, space",
        "--quote <CHAR>",
        "Quote character, or \"none\"",
        "--escape <STYLE>",
        "Quote escape: double | backslash",
        "--header",
        "--no-header",
        "--encoding <NAME>",
        "utf-8 | utf-16le | utf-16be | latin1 | windows-1252",
        "--comment <CHAR>",
        "Skip lines starting with this character",
        "--filter <EXPR>",
        "Open with this filter applied (query language, §9)",
        "--sort <SPEC>",
        "Open sorted, e.g. \"price:desc,ts\"",
        "--yes",
        "Accept detected dialect without showing the dialog",
        "--threads <N>",
        "Worker threads (default: logical CPUs)",
        "--mem <SIZE>",
        "Memory budget, e.g. 2G, 512M (default: 2G)",
        "--tmp <DIR>",
        "Directory for spill files (default: $TMPDIR)",
        "--theme <NAME>",
        "Theme name (default: dark)",
        "--help",
        "--version",
    ] {
        assert!(stdout.contains(text), "missing {text:?} in:\n{stdout}");
    }
}
