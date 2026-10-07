//! Parser property tests (M1-03, extended by M2-02 and M7-05):
//!
//! - Round trip: random records written by a simple reference writer parse
//!   back to the same field values, for both escape styles and mixed line
//!   endings.
//! - Oracle: on arbitrary (also malformed) input, the parser agrees with
//!   csv-core record by record (lone `\r` excluded, see `parse` module docs;
//!   inputs end with `\n` to avoid csv-core's EOF quirks).

use csv_core::{ReadRecordResult, ReaderBuilder, Terminator};
use proptest::prelude::*;
use tachy_core::{
    dialect::{Dialect, EscapeStyle},
    parse::{ParseOutcome, RecordParser, RecordRanges},
};

fn dialect(escape: EscapeStyle, comment: Option<u8>) -> Dialect {
    Dialect {
        escape,
        comment,
        ..Dialect::default()
    }
}

/// Every record of `bytes` as unescaped field values.
fn parse_all(d: &Dialect, bytes: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut p = RecordParser::new(d);
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut rows = Vec::new();
    let mut pos = 0;
    loop {
        let next = match p.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Eof => break,
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
        };
        rows.push(
            (0..rec.fields.len())
                .map(|i| p.field_value(bytes, &rec, i, &mut scratch).to_vec())
                .collect(),
        );
        assert!(next > pos);
        pos = next;
    }
    rows
}

/// Reference writer: quotes a field when needed and escapes per style.
fn write_field(out: &mut Vec<u8>, field: &[u8], escape: EscapeStyle, only_field: bool) {
    let needs_quotes = field.is_empty() && only_field
        || field
            .iter()
            .any(|&b| matches!(b, b',' | b'"' | b'\n' | b'\r' | b'\\'))
        || field.first() == Some(&b'#');
    if !needs_quotes {
        out.extend_from_slice(field);
        return;
    }
    out.push(b'"');
    for &b in field {
        match (escape, b) {
            (EscapeStyle::Doubled, b'"') => out.extend_from_slice(b"\"\""),
            (EscapeStyle::Backslash, b'"' | b'\\') => {
                out.push(b'\\');
                out.push(b);
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

fn field_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop::sample::select(vec![
            b'a', b'b', b'z', b'0', b' ', b',', b'"', b'\n', b'\r', b'\\', b'#', b'\t', 0xC3, 0xA9,
        ]),
        0..8,
    )
}

fn records_strategy() -> impl Strategy<Value = Vec<(Vec<Vec<u8>>, bool)>> {
    prop::collection::vec(
        (prop::collection::vec(field_strategy(), 1..6), any::<bool>()),
        0..12,
    )
}

/// Records through csv-core, the oracle.
fn csv_core_all(d: &Dialect, input: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut b = ReaderBuilder::new();
    b.delimiter(d.delimiter)
        .double_quote(d.escape == EscapeStyle::Doubled)
        .comment(d.comment)
        .terminator(Terminator::CRLF);
    match d.quote {
        Some(q) => {
            b.quote(q);
        }
        None => {
            b.quoting(false);
        }
    }
    if d.escape == EscapeStyle::Backslash {
        b.escape(Some(b'\\'));
    }
    let mut reader = b.build();
    let mut out = vec![0u8; input.len() * 2 + 16];
    let mut ends = vec![0usize; input.len() + 16];
    let (mut out_len, mut ends_len) = (0, 0);
    let mut rows = Vec::new();
    let mut pos = 0;
    loop {
        // Output and ends accumulate across calls until a record is done.
        let (res, nin, nout, nend) =
            reader.read_record(&input[pos..], &mut out[out_len..], &mut ends[ends_len..]);
        pos += nin;
        out_len += nout;
        ends_len += nend;
        match res {
            ReadRecordResult::Record => {
                let mut start = 0;
                rows.push(
                    ends[..ends_len]
                        .iter()
                        .map(|&e| {
                            let f = out[start..e].to_vec();
                            start = e;
                            f
                        })
                        .collect(),
                );
                out_len = 0;
                ends_len = 0;
            }
            // Empty input on the next call signals EOF.
            ReadRecordResult::InputEmpty => {}
            ReadRecordResult::End => break,
            ReadRecordResult::OutputFull | ReadRecordResult::OutputEndsFull => {
                panic!("buffers are large enough")
            }
        }
    }
    rows
}

/// Arbitrary text from bytes that matter to the parser. `\r` only appears as
/// part of `\r\n` (a lone `\r` is data for tachy but a terminator for
/// csv-core).
fn messy_input() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop::sample::select(vec![
            &b"a"[..],
            b"b",
            b",",
            b"\"",
            b"\"\"",
            b"\n",
            b"\r\n",
            b"\\",
            b"#",
            b" ",
        ]),
        0..40,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn writer_round_trip(records in records_strategy(), backslash in any::<bool>()) {
        let escape = if backslash { EscapeStyle::Backslash } else { EscapeStyle::Doubled };
        let mut bytes = Vec::new();
        for (fields, crlf) in &records {
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    bytes.push(b',');
                }
                write_field(&mut bytes, f, escape, fields.len() == 1);
            }
            bytes.extend_from_slice(if *crlf { b"\r\n" } else { b"\n" });
        }
        let want: Vec<Vec<Vec<u8>>> = records.into_iter().map(|(f, _)| f).collect();
        prop_assert_eq!(parse_all(&dialect(escape, None), &bytes), want);
    }

    #[test]
    fn agrees_with_csv_core(
        input in messy_input(),
        backslash in any::<bool>(),
        comment in any::<bool>(),
    ) {
        let escape = if backslash { EscapeStyle::Backslash } else { EscapeStyle::Doubled };
        let d = dialect(escape, comment.then_some(b'#'));
        // csv-core has EOF quirks without a final newline (`a,` loses its
        // empty last field, a final `#comment` becomes an empty record), so
        // the comparison input always ends with `\n`. tachy's own EOF rules
        // are unit-tested in `parse.rs`.
        let mut input = input;
        input.push(b'\n');
        prop_assert_eq!(parse_all(&d, &input), csv_core_all(&d, &input));
    }

    #[test]
    fn skip_lands_where_parse_at_does(input in messy_input(), comment in any::<bool>()) {
        let d = dialect(EscapeStyle::Doubled, comment.then_some(b'#'));
        let mut p = RecordParser::new(&d);
        let mut rec = RecordRanges::default();
        let mut pos = 0u64;
        let mut n = 0u64;
        loop {
            match p.parse_at(&input, pos, &mut rec) {
                ParseOutcome::Eof => break,
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                    n += 1;
                    pos = next;
                    prop_assert_eq!(
                        p.skip(&input, 0, n),
                        tachy_core::parse::SkipOutcome::Skipped { next }
                    );
                }
            }
        }
        prop_assert_eq!(
            p.skip(&input, 0, n + 1),
            tachy_core::parse::SkipOutcome::Eof { skipped: n }
        );
    }
}

#[test]
fn no_quoting_agrees_with_csv_core() {
    let d = Dialect {
        quote: None,
        ..Dialect::default()
    };
    let input = b"\"a,b\"\n\"c\r\n\nd,\"\n";
    assert_eq!(parse_all(&d, input), csv_core_all(&d, input));
}
