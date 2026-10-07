//! M7-04 edge-case audit, core side: each §16 row (and the implied cases)
//! that can be checked without a terminal. The UI rows (toasts, the
//! too-small screen, resize, panic, SIGBUS) are covered in the `tachy` crate
//! and `docs/manual-tests.md`.

mod support;

use std::sync::Arc;

use support::{open_sniffed, runtime, temp_file};
use tachy_core::{
    exec::Executor,
    index::{IndexOptions, IndexSummary, RowIndex, build_index},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    source::Source,
};
use tokio_util::sync::CancellationToken;

/// Opens and sniffs `content`, then indexes it fully.
fn index(
    content: &[u8],
) -> (
    tempfile::NamedTempFile,
    Arc<Source>,
    Arc<RowIndex>,
    IndexSummary,
) {
    let f = temp_file(content);
    let (src, report) = open_sniffed(f.path());
    let index = Arc::new(RowIndex::for_source(&src));
    let summary = runtime()
        .block_on(build_index(
            Arc::clone(&src),
            Arc::clone(&index),
            report,
            Executor::with_handle(runtime().handle().clone(), 4),
            CancellationToken::new(),
            IndexOptions::default(),
        ))
        .unwrap();
    (f, src, index, summary)
}

/// Every record's field values, by sequential parsing from `data_start`.
fn records(src: &Source) -> Vec<Vec<Vec<u8>>> {
    let mut p = RecordParser::new(src.dialect());
    let mut out = Vec::new();
    let mut rec = RecordRanges::default();
    let mut at = src.data_start();
    while let ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } =
        p.parse_at(src.bytes(), at, &mut rec)
    {
        let mut scratch = Vec::new();
        out.push(
            (0..rec.fields.len())
                .map(|i| p.field_value(src.bytes(), &rec, i, &mut scratch).to_vec())
                .collect(),
        );
        if next <= at {
            break;
        }
        at = next;
    }
    out
}

// ---- the §16 table --------------------------------------------------------

#[test]
fn empty_file_opens_with_zero_rows() {
    let (_f, src, index, summary) = index(b"");
    assert!(src.is_empty());
    assert_eq!(summary.total_rows, 0);
    assert_eq!(index.total_rows(), Some(0));
}

#[test]
fn header_only_has_columns_and_no_rows() {
    let (_f, src, _index, summary) = index(b"id,name,price\n");
    assert_eq!(summary.total_rows, 0);
    let names: Vec<String> = src.column_names().into_iter().map(|c| c.display).collect();
    assert_eq!(names, ["id", "name", "price"]);
}

#[test]
fn ragged_rows_are_counted() {
    let (_f, _src, _index, summary) = index(b"a,b,c\n1,2,3\n1,2\n1,2,3,4,5\n7,8,9\n");
    assert_eq!(summary.total_rows, 4);
    assert_eq!(summary.ragged_rows, 2);
}

#[test]
fn unterminated_quote_ends_at_eof_with_a_warning() {
    let (_f, src, index, summary) = index(b"a,b\n1,\"open\n2,3\n");
    assert!(summary.unterminated_quote);
    assert!(index.unterminated_quote());
    let recs = records(&src);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0][1], b"open\n2,3\n");
}

#[test]
fn invalid_utf8_is_kept_as_raw_bytes() {
    let (_f, src, _index, summary) = index(b"a,b\n\xff\xfe,ok\n");
    assert_eq!(summary.total_rows, 1);
    // Display decoding replaces, the stored bytes don't change (§16).
    assert_eq!(records(&src)[0][0], b"\xff\xfe");
}

// ---- implied edge cases ---------------------------------------------------

#[test]
fn no_trailing_newline_keeps_the_last_row() {
    let (_f, src, _index, summary) = index(b"a,b\n1,2\n3,4");
    assert_eq!(summary.total_rows, 2);
    assert_eq!(records(&src)[1], [b"3".to_vec(), b"4".to_vec()]);
}

#[test]
fn single_column_without_delimiters() {
    let (_f, src, _index, summary) = index(b"name\nalpha\nbeta\ngamma\n");
    assert_eq!(src.dialect().delimiter, b',');
    assert_eq!(src.width(), 1);
    assert_eq!(summary.total_rows, 3);
}

#[test]
fn tiny_files() {
    for content in [&b"x"[..], b"\n", b"\r\n\r\n", b"\n\n\n"] {
        let (_f, _src, index, summary) = index(content);
        assert!(index.total_rows().is_some(), "{content:?}");
        // Blank lines are not records (M1-03).
        assert!(summary.total_rows <= 1, "{content:?}: {summary:?}");
    }
}

#[test]
fn utf8_bom_is_not_part_of_the_first_column_name() {
    let (_f, src, _index, summary) = index(b"\xef\xbb\xbfid,name\n1,a\n");
    assert_eq!(src.column_names()[0].display, "id");
    assert_eq!(summary.total_rows, 1);
}

#[test]
fn very_long_line_is_parsed_whole() {
    let long = "x".repeat(8 << 20);
    let content = format!("a,b\n1,{long}\n2,short\n");
    let (_f, src, _index, summary) = index(content.as_bytes());
    assert_eq!(summary.total_rows, 2);
    let recs = records(&src);
    assert_eq!(recs[0][1].len(), 8 << 20);
    assert_eq!(recs[1][1], b"short");
}

#[test]
fn many_columns() {
    const COLS: usize = 12_000;
    let header: Vec<String> = (0..COLS).map(|i| format!("c{i}")).collect();
    let row: Vec<String> = (0..COLS).map(|i| i.to_string()).collect();
    let content = format!(
        "{}\n{}\n{}\n",
        header.join(","),
        row.join(","),
        row.join(",")
    );
    let (_f, src, _index, summary) = index(content.as_bytes());
    assert_eq!(src.width(), COLS);
    assert_eq!(summary.total_rows, 2);
    assert_eq!(records(&src)[1].len(), COLS);
}

#[test]
fn crlf_and_lf_mixed() {
    let (_f, src, _index, summary) = index(b"a,b\r\n1,2\n3,4\r\n");
    assert_eq!(summary.total_rows, 2);
    assert_eq!(records(&src)[1], [b"3".to_vec(), b"4".to_vec()]);
}
