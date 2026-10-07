//! The `gen` test-data binary (M7-05): deterministic output, valid files.
//!
//! Needs the `gen` feature (`cargo test -p tachy-core --features gen`); CI
//! runs with `--all-features`.
#![cfg(feature = "gen")]

mod support;

use std::{path::Path, process::Command};

use support::{index_source, open_sniffed_with, reference};
use tachy_core::dialect::DialectOverrides;

/// Runs `gen` with `args`, writing to `out`.
fn generate(out: &Path, args: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_gen"))
        .args(args)
        .arg("--out")
        .arg(out)
        .status()
        .unwrap();
    assert!(status.success(), "gen {args:?} failed");
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("tachy-gen-")
        .tempdir()
        .unwrap()
}

#[test]
fn same_seed_same_bytes() {
    let dir = tmp();
    let (a, b, c) = (
        dir.path().join("a.csv"),
        dir.path().join("b.csv"),
        dir.path().join("c.csv"),
    );
    let args = ["--rows", "20k", "--cols", "12", "--quoted-newlines", "0.01"];
    generate(&a, &args);
    generate(&b, &args);
    generate(&c, &[&args[..], &["--seed", "43"]].concat());
    let (a, b, c) = (
        std::fs::read(a).unwrap(),
        std::fs::read(b).unwrap(),
        std::fs::read(c).unwrap(),
    );
    assert!(a.len() > 1_000_000);
    assert!(a == b, "same seed, same output");
    assert!(a != c, "another seed, another output");
}

#[test]
fn stdout_matches_out_file() {
    let dir = tmp();
    let path = dir.path().join("a.csv");
    let args = ["--rows", "1000", "--cols", "9"];
    generate(&path, &args);
    let stdout = Command::new(env!("CARGO_BIN_EXE_gen"))
        .args(args)
        .output()
        .unwrap();
    assert!(stdout.status.success());
    assert_eq!(stdout.stdout, std::fs::read(path).unwrap());
}

#[test]
fn rejects_bad_flags() {
    let out = Command::new(env!("CARGO_BIN_EXE_gen"))
        .args(["--rows", "ten"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage error");
}

/// Every flag mix produces a file that sniffs and indexes to exactly the
/// requested rows, in agreement with the sequential reference parser.
#[test]
fn output_indexes_to_the_requested_rows() {
    let dir = tmp();
    let cases: &[(&[&str], u64)] = &[
        (&["--cols", "12"], 0),
        (&["--cols", "8", "--quoted-newlines", "0.05"], 0),
        (&["--cols", "16", "--crlf", "--quoted-newlines", "0.02"], 0),
        (&["--cols", "8", "--delimiter", "tab"], 0),
        (&["--cols", "8", "--delimiter", ";", "--nulls", "0.2"], 0),
        (
            &[
                "--cols",
                "8",
                "--delimiter",
                "|",
                "--encoding",
                "windows-1252",
            ],
            0,
        ),
        (&["--cols", "8", "--ragged", "0.1"], 1),
    ];
    for (i, &(args, ragged)) in cases.iter().enumerate() {
        let path = dir.path().join(format!("{i}.csv"));
        generate(&path, &[&["--rows", "3000"], args].concat());
        // 10 % ragged rows defeat delimiter and header detection; that is
        // the sniffer's call, not the generator's, so give them.
        let overrides = DialectOverrides {
            delimiter: (ragged > 0).then_some(b','),
            header: (ragged > 0).then_some(true),
            ..DialectOverrides::default()
        };
        let (src, report) = open_sniffed_with(&path, &overrides);
        assert!(src.dialect().header, "{args:?}: header detected");
        assert_eq!(src.width(), args[1].parse::<usize>().unwrap(), "{args:?}");
        let want = reference(&src);
        let (_, summary) = index_source(&src, report);
        assert_eq!(summary.total_rows, 3000, "{args:?}");
        assert_eq!(want.starts.len(), 3000, "{args:?}");
        assert!(
            !summary.unterminated_quote && !want.unterminated,
            "{args:?}"
        );
        assert_eq!(summary.ragged_rows, want.ragged, "{args:?}");
        if ragged == 0 {
            assert_eq!(summary.ragged_rows, 0, "{args:?}");
        } else {
            // ~10 % of 3,000 rows.
            assert!((150..450).contains(&summary.ragged_rows), "{args:?}");
        }
    }
}

#[test]
fn no_header_flag() {
    let dir = tmp();
    let path = dir.path().join("a.csv");
    generate(&path, &["--rows", "50", "--no-header", "--cols", "3"]);
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"1,"));
    assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 50);
}
