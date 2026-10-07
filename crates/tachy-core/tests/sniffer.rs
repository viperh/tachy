//! Sniffer fixture corpus (spec §19): every file in `fixtures/sniff/` must be
//! detected exactly as `expected.json` says.

use std::{collections::BTreeMap, path::PathBuf};

use pretty_assertions::assert_eq;
use serde::Deserialize;
use tachy_core::dialect::{
    DEFAULT_SAMPLE_BYTES, DialectOverrides, Encoding, EscapeStyle, LineEnding, SniffReport,
    column_names, sniff,
};

#[derive(Debug, Deserialize, PartialEq)]
struct Expected {
    delimiter: String,
    quote: Option<String>,
    escape: String,
    line_ending: String,
    header: bool,
    encoding: String,
    quoted_newlines: bool,
    sample_truncated: bool,
}

impl Expected {
    fn from_report(r: &SniffReport) -> Self {
        let d = &r.dialect;
        Expected {
            delimiter: (d.delimiter as char).to_string(),
            quote: d.quote.map(|q| (q as char).to_string()),
            escape: match d.escape {
                EscapeStyle::Doubled => "doubled",
                EscapeStyle::Backslash => "backslash",
            }
            .to_string(),
            line_ending: match d.line_ending {
                LineEnding::Lf => "lf",
                LineEnding::CrLf => "crlf",
                LineEnding::Mixed => "mixed",
            }
            .to_string(),
            header: d.header,
            encoding: match d.encoding {
                Encoding::Utf8 => "utf-8",
                Encoding::Utf8Bom => "utf-8-bom",
                Encoding::Utf16Le => "utf-16le",
                Encoding::Utf16Be => "utf-16be",
                Encoding::Windows1252 => "windows-1252",
            }
            .to_string(),
            quoted_newlines: r.quoted_newlines,
            sample_truncated: r.sample_truncated,
        }
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sniff")
}

fn expected() -> BTreeMap<String, Expected> {
    let text = std::fs::read_to_string(fixtures().join("expected.json")).unwrap();
    json5::from_str(&text).unwrap()
}

#[test]
fn every_fixture_matches_expected_json() {
    let expected = expected();
    let mut on_disk: Vec<String> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "expected.json")
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk,
        expected.keys().cloned().collect::<Vec<_>>(),
        "every fixture needs an entry"
    );
    let mut failures = Vec::new();
    for (name, want) in &expected {
        let bytes = std::fs::read(fixtures().join(name)).unwrap();
        let report = sniff(&bytes, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
        // Without overrides, detected == dialect.
        assert_eq!(report.detected, report.dialect, "{name}");
        let got = Expected::from_report(&report);
        if &got != want {
            failures.push(format!("{name}:\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn corpus_covers_the_required_cases() {
    let e = expected();
    let has = |f: &dyn Fn(&Expected) -> bool| e.values().any(f);
    for d in [",", "\t", "|", ";", ":", " "] {
        assert!(has(&|x| x.delimiter == d), "delimiter {d:?}");
    }
    assert!(has(&|x| x.quoted_newlines));
    assert!(has(&|x| x.escape == "backslash"));
    assert!(has(&|x| x.quote.as_deref() == Some("'")));
    assert!(has(&|x| x.line_ending == "crlf"));
    assert!(has(&|x| x.line_ending == "mixed"));
    for enc in ["utf-8-bom", "utf-16le", "utf-16be", "windows-1252"] {
        assert!(has(&|x| x.encoding == enc), "{enc}");
    }
    assert!(has(&|x| !x.header));
    assert!(has(&|x| x.sample_truncated));
    for name in [
        "quoted_delims.csv",
        "doubled_quotes.csv",
        "identifiers_header.csv",
        "single_column.txt",
        "one_line.csv",
        "no_trailing_newline.csv",
        "long_mid_record.csv",
        "duplicate_headers.csv",
    ] {
        assert!(e.contains_key(name), "{name}");
    }
}

#[test]
fn long_mid_record_sample_is_cut_inside_quotes() {
    let bytes = std::fs::read(fixtures().join("long_mid_record.csv")).unwrap();
    assert!(bytes.len() > DEFAULT_SAMPLE_BYTES);
    let r = sniff(&bytes, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    assert!(r.sample_len < bytes.len());
    assert!(r.sample_len >= DEFAULT_SAMPLE_BYTES);
}

#[test]
fn duplicate_header_names_get_suffixes() {
    let bytes = std::fs::read(fixtures().join("duplicate_headers.csv")).unwrap();
    let first: Vec<Vec<u8>> = bytes
        .split(|&b| b == b'\n')
        .next()
        .unwrap()
        .split(|&b| b == b',')
        .map(<[u8]>::to_vec)
        .collect();
    let names: Vec<(String, String)> = column_names(&first, true, Encoding::Utf8)
        .into_iter()
        .map(|c| (c.display, c.query))
        .collect();
    let s = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        names,
        [
            s("name", "name"),
            s("name", "name_2"),
            s("value", "value"),
            s("name", "name_3")
        ]
    );
}

#[test]
fn every_override_wins_on_every_fixture() {
    let o = DialectOverrides {
        delimiter: Some(b'|'),
        quote: Some(Some(b'\'')),
        escape: Some(EscapeStyle::Backslash),
        header: Some(false),
        encoding: Some(Encoding::Windows1252),
        comment: Some(b'#'),
    };
    for name in expected().keys() {
        let bytes = std::fs::read(fixtures().join(name)).unwrap();
        let r = sniff(&bytes, DEFAULT_SAMPLE_BYTES, &o);
        let d = r.dialect;
        assert_eq!(
            (
                d.delimiter,
                d.quote,
                d.escape,
                d.header,
                d.encoding,
                d.comment
            ),
            (
                b'|',
                Some(b'\''),
                EscapeStyle::Backslash,
                false,
                Encoding::Windows1252,
                Some(b'#')
            ),
            "{name}"
        );
    }
}

/// §M1-02: sniffing a 64 KiB sample takes < 5 ms in release. Debug builds
/// only check that it finishes.
#[test]
fn sniffing_64k_is_fast() {
    let mut text = b"id,name,price,when,flag\n".to_vec();
    let mut i = 0;
    while text.len() < 2 * DEFAULT_SAMPLE_BYTES {
        text.extend_from_slice(
            format!("{i},\"name {i}, quoted\",{}.25,2024-01-02,true\n", i * 3).as_bytes(),
        );
        i += 1;
    }
    let runs = 20;
    let t = std::time::Instant::now();
    for _ in 0..runs {
        std::hint::black_box(sniff(
            &text,
            DEFAULT_SAMPLE_BYTES,
            &DialectOverrides::default(),
        ));
    }
    let per = t.elapsed() / runs;
    eprintln!("sniff 64 KiB: {per:?}");
    if !cfg!(debug_assertions) {
        assert!(per.as_micros() < 5_000, "{per:?}");
    }
}
