//! Parallel indexer (M2-02) against a sequential reference parser (§19).
//!
//! Indexes built with a stride of 1 publish every record start as a
//! checkpoint, so the tests compare every record boundary, not just every
//! 1,024th.

mod support;

use std::{path::PathBuf, sync::Arc, time::Duration};

use pretty_assertions::assert_eq;
use proptest::prelude::*;
use tachy_core::{
    dialect::{DEFAULT_SAMPLE_BYTES, Dialect, DialectOverrides, EscapeStyle, SniffReport, sniff},
    exec::Executor,
    index::{
        INDEX_STRIDE, IndexError, IndexOptions, IndexPath, IndexSummary, RowIndex, build_index,
    },
    parse::RecordParser,
    source::Source,
};
use tokio_util::sync::CancellationToken;

use support::{Reference, reference, runtime, temp_source};

/// What the indexer produced, in the reference's shape (stride-1 index).
fn observed(src: &Source, index: &RowIndex, summary: &IndexSummary) -> Reference {
    assert!(index.is_complete());
    assert_eq!(index.total_rows(), Some(summary.total_rows));
    assert_eq!(index.ragged_rows(), summary.ragged_rows);
    assert_eq!(index.unterminated_quote(), summary.unterminated_quote);
    let n = index.published_checkpoints();
    let mut starts: Vec<u64> = (1..n).map(|k| index.checkpoint(k).unwrap()).collect();
    if summary.total_rows > 0 {
        // Checkpoint 0 is `data_start`; the first record may come after
        // blank or comment lines.
        let p = RecordParser::new(src.dialect());
        starts.insert(
            0,
            p.skip_ignorable(src.bytes(), src.data_start() as usize) as u64,
        );
    }
    Reference {
        starts,
        ragged: summary.ragged_rows,
        unterminated: summary.unterminated_quote,
    }
}

fn report(src: &Source, quoted_newlines: bool) -> SniffReport {
    SniffReport {
        detected: *src.dialect(),
        dialect: *src.dialect(),
        quoted_newlines,
        sample_len: 0,
        sample_truncated: false,
    }
}

async fn build(
    src: &Arc<Source>,
    slow: bool,
    chunk_size: u64,
    threads: usize,
    stride: u64,
) -> (Arc<RowIndex>, IndexSummary) {
    let index = Arc::new(RowIndex::with_stride(src.data_start(), stride));
    let summary = build_index(
        Arc::clone(src),
        Arc::clone(&index),
        report(src, slow),
        Executor::new(threads),
        CancellationToken::new(),
        IndexOptions {
            chunk_size,
            ..IndexOptions::default()
        },
    )
    .await
    .unwrap();
    (index, summary)
}

/// Both paths, chunk sizes 1..=64 plus some larger ones, against the
/// reference.
fn check_all_paths(src: &Arc<Source>, extra_sizes: &[u64]) -> Result<(), TestCaseError> {
    let want = reference(src);
    for &slow in &[false, true] {
        for chunk_size in (1..=64).chain(extra_sizes.iter().copied()) {
            let (index, summary) = runtime().block_on(build(src, slow, chunk_size, 4, 1));
            let got = observed(src, &index, &summary);
            prop_assert_eq!(&got, &want, "slow={} chunk_size={}", slow, chunk_size);
            if slow && src.dialect().quote.is_some() {
                prop_assert_eq!(summary.path_used, IndexPath::Slow);
            }
        }
    }
    Ok(())
}

/// Structured DSV: written records plus blank and comment lines, both escape
/// styles, mixed line endings.
fn structured() -> impl Strategy<Value = (Vec<u8>, bool)> {
    let field = prop::collection::vec(
        prop::sample::select(vec![
            b'a', b'1', b' ', b',', b'"', b'\n', b'\r', b'\\', b'#',
        ]),
        0..6,
    );
    let line = prop_oneof![
        8 => (prop::collection::vec(field, 1..5), any::<bool>()).prop_map(Some),
        1 => Just(None),
    ];
    (
        prop::collection::vec(line, 0..25),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(lines, backslash, comments)| {
            let mut out = Vec::new();
            for line in lines {
                match line {
                    None if comments => out.extend_from_slice(b"# a \"comment\", here\n"),
                    None => out.extend_from_slice(b"\r\n"),
                    Some((fields, crlf)) => {
                        for (i, f) in fields.iter().enumerate() {
                            if i > 0 {
                                out.push(b',');
                            }
                            let quote = f.is_empty() && fields.len() == 1
                                || f.iter().any(|b| b",\"\n\r\\#".contains(b));
                            if quote {
                                out.push(b'"');
                                for &b in f {
                                    match (backslash, b) {
                                        (false, b'"') => out.extend_from_slice(b"\"\""),
                                        (true, b'"' | b'\\') => out.extend_from_slice(&[b'\\', b]),
                                        _ => out.push(b),
                                    }
                                }
                                out.push(b'"');
                            } else {
                                out.extend_from_slice(f);
                            }
                        }
                        out.extend_from_slice(if crlf { b"\r\n" } else { b"\n" });
                    }
                }
            }
            (out, backslash)
        })
}

/// Arbitrary, often malformed input.
fn messy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop::sample::select(vec![
            &b"a"[..],
            b"bb",
            b",",
            b"\"",
            b"\"\"",
            b"\n",
            b"\r\n",
            b"\r",
            b"\\",
            b"#",
        ]),
        0..60,
    )
    .prop_map(|p| p.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn structured_dsv_matches_reference(
        (content, backslash) in structured(),
        comment in any::<bool>(),
        header in any::<bool>(),
    ) {
        let d = Dialect {
            escape: if backslash { EscapeStyle::Backslash } else { EscapeStyle::Doubled },
            comment: comment.then_some(b'#'),
            header,
            ..Dialect::default()
        };
        let (_f, src) = temp_source(&content, d);
        check_all_paths(&src, &[97, 1000])?;
    }

    #[test]
    fn messy_input_matches_reference(
        content in messy(),
        backslash in any::<bool>(),
        comment in any::<bool>(),
        no_quote in any::<bool>(),
    ) {
        let d = Dialect {
            escape: if backslash { EscapeStyle::Backslash } else { EscapeStyle::Doubled },
            comment: comment.then_some(b'#'),
            quote: if no_quote { None } else { Some(b'"') },
            header: false,
            ..Dialect::default()
        };
        let (_f, src) = temp_source(&content, d);
        check_all_paths(&src, &[333])?;
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sniff")
}

#[test]
fn every_fixture_matches_reference() {
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name == "expected.json" || name.starts_with("utf16") && !name.contains("utf8") {
            continue; // UTF-16 is transcoded before indexing.
        }
        let (src, _) = Source::open_sniffed(
            &path,
            None,
            DEFAULT_SAMPLE_BYTES,
            &DialectOverrides::default(),
        )
        .unwrap();
        let src = Arc::new(src);
        check_all_paths(&src, &[4096, 64 << 20]).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// Fixed 50-byte lines; two lines per 100-byte chunk.
fn fixed_lines(rows: usize, quoted_newline_after_row: Option<usize>) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..rows {
        if Some(i) == quoted_newline_after_row {
            // 50 bytes: opens a quote that closes on the next line.
            let line = format!("{i:08},\"spans the chunk boundary{}\n", ".".repeat(15));
            assert_eq!(line.len(), 50);
            out.extend_from_slice(line.as_bytes());
            continue;
        }
        if i > 0 && Some(i - 1) == quoted_newline_after_row {
            let line = format!("still quoted, then closed\",{i:08},xxxxxxxxxxxxx\n");
            assert_eq!(line.len(), 50, "{line:?}");
            out.extend_from_slice(line.as_bytes());
            continue;
        }
        let line = format!("{i:08},{:040}\n", i);
        assert_eq!(line.len(), 50);
        out.extend_from_slice(line.as_bytes());
    }
    out
}

#[test]
fn fast_path_falls_back_at_the_first_quoted_boundary() {
    // Chunk k holds lines 2k and 2k+1. Line 7 (end of chunk 3) opens a quote
    // that closes in line 8 (chunk 4).
    let content = fixed_lines(40, Some(7));
    let d = Dialect {
        header: false,
        ..Dialect::default()
    };
    let (_f, src) = temp_source(&content, d);
    let want = reference(&src);
    assert_eq!(want.starts.len(), 39);
    let (index, summary) = runtime().block_on(build(&src, false, 100, 4, 1));
    assert_eq!(summary.path_used, IndexPath::FastThenSlow { from_chunk: 4 });
    assert_eq!(observed(&src, &index, &summary), want);
    // Chunks 0–3 (rows 0–7) keep their offsets; row 8 is the line after the
    // quoted one.
    for r in 0..8u64 {
        assert_eq!(index.checkpoint(r), Some(r * 50));
    }
    assert_eq!(index.checkpoint(8), Some(450));
    // Also with a real stride and batch sizes that cut through chunk 4.
    for batch in [1, 2, 3, 5, 16] {
        let index = Arc::new(RowIndex::with_stride(0, 1));
        let summary = runtime()
            .block_on(build_index(
                Arc::clone(&src),
                Arc::clone(&index),
                report(&src, false),
                Executor::with_handle(runtime().handle().clone(), 2),
                CancellationToken::new(),
                IndexOptions {
                    chunk_size: 100,
                    batch,
                    ..IndexOptions::default()
                },
            ))
            .unwrap();
        assert_eq!(summary.path_used, IndexPath::FastThenSlow { from_chunk: 4 });
        assert_eq!(observed(&src, &index, &summary), want, "batch {batch}");
    }
    // Without the quoted newline, the fast path never falls back.
    let (_f, src) = temp_source(&fixed_lines(40, None), d);
    let (_, summary) = runtime().block_on(build(&src, false, 100, 4, 1));
    assert_eq!(summary.path_used, IndexPath::Fast);
}

/// A generated file of about `bytes` bytes, with quoted fields and ragged
/// rows.
fn generated(bytes: usize) -> Vec<u8> {
    let mut out = b"id,name,amount,note\n".to_vec();
    let mut i = 0u64;
    while out.len() < bytes {
        let line = match i % 97 {
            0 => format!("{i},\"multi\nline\",{},x\n", i * 3),
            13 => format!("{i},short\n"),
            _ => format!(
                "{i},\"name, {i}\",{}.{:02},plain note {i}\n",
                i * 7,
                i % 100
            ),
        };
        out.extend_from_slice(line.as_bytes());
        i += 1;
    }
    out
}

#[test]
fn checkpoints_match_on_a_larger_file_with_default_stride() {
    let (_f, src) = temp_source(&generated(4 << 20), Dialect::default());
    let want = reference(&src);
    for slow in [false, true] {
        let (index, summary) = runtime().block_on(build(&src, slow, 256 << 10, 8, INDEX_STRIDE));
        assert_eq!(summary.total_rows, want.starts.len() as u64);
        assert_eq!(summary.ragged_rows, want.ragged);
        let n = index.published_checkpoints();
        assert_eq!(n, (want.starts.len() as u64).div_ceil(INDEX_STRIDE));
        for k in 1..n {
            assert_eq!(
                index.checkpoint(k),
                Some(want.starts[(k * INDEX_STRIDE) as usize])
            );
        }
        let mut p = RecordParser::new(src.dialect());
        for r in (0..want.starts.len()).step_by(997) {
            assert_eq!(
                index.offset_of(r as u64, &src, &mut p),
                Some(want.starts[r])
            );
        }
        if !slow {
            assert!(matches!(summary.path_used, IndexPath::FastThenSlow { .. }));
        }
    }
}

#[test]
fn one_and_sixteen_threads_build_identical_indexes() {
    let (_f, src) = temp_source(&generated(1 << 20), Dialect::default());
    let (a, sa) = runtime().block_on(build(&src, true, 4096, 1, 1));
    let (b, sb) = runtime().block_on(build(&src, true, 4096, 16, 1));
    assert_eq!(sa, sb);
    assert_eq!(observed(&src, &a, &sa), observed(&src, &b, &sb));
    assert_eq!(observed(&src, &a, &sa), reference(&src));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rows_are_navigable_while_indexing() {
    let (_f, src) = temp_source(&generated(16 << 20), Dialect::default());
    let index = Arc::new(RowIndex::for_source(&src));
    let task = tokio::spawn(build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report(&src, true),
        Executor::new(1),
        CancellationToken::new(),
        IndexOptions {
            chunk_size: 64 << 10,
            batch: 1,
            ..IndexOptions::default()
        },
    ));
    // The smallest row count seen while `total_rows` was still `None`.
    let mut first_partial: Option<u64> = None;
    let mut last_rows = 0;
    let mut p = RecordParser::new(src.dialect());
    loop {
        // Take one snapshot per poll, `total_rows` first: the indexer can
        // complete between any two reads, so the reads must not be compared
        // as if they were taken at the same instant. (An earlier version
        // checked `!is_complete()` and then asserted `total_rows().is_none()`;
        // under load the indexer finished in between and the assert fired.)
        // `finish` stores `indexed_rows` before `complete` (both Release),
        // so `total_rows() == None` here means `rows` below is at least the
        // partial count published before that read.
        let finished = task.is_finished();
        let total = index.total_rows();
        let rows = index.indexed_rows();
        assert!(
            rows >= last_rows,
            "indexed_rows went back: {last_rows} -> {rows}"
        );
        last_rows = rows;
        if let Some(total) = total {
            assert_eq!(rows, total, "indexed_rows changed after completion");
        }
        if rows > 0 {
            // The last indexed row is reachable whether or not the index is
            // complete yet: checkpoints are published before `indexed_rows`.
            assert!(index.offset_of(rows - 1, &src, &mut p).is_some());
            if total.is_none() {
                first_partial.get_or_insert(rows);
            }
        }
        if finished {
            break;
        }
        // Yield instead of sleeping: a 1 ms timer can stretch to tens of
        // milliseconds under load, long enough to miss every partial state.
        tokio::task::yield_now().await;
    }
    let summary = task.await.unwrap().unwrap();
    // A truly partial state: fewer rows than the final count (the
    // `total_rows() == None` read may precede completion by a hair).
    assert!(
        first_partial.is_some_and(|r| r < summary.total_rows),
        "never saw a partially built index ({first_partial:?} of {} rows)",
        summary.total_rows
    );
    assert_eq!(index.total_rows(), Some(summary.total_rows));
    assert_eq!(last_rows, summary.total_rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_stops_the_scanners() {
    let (_f, src) = temp_source(&generated(64 << 20), Dialect::default());
    let index = Arc::new(RowIndex::for_source(&src));
    let cancel = CancellationToken::new();
    let threads = 4;
    let task = tokio::spawn(build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report(&src, true),
        Executor::new(threads),
        cancel.clone(),
        IndexOptions {
            chunk_size: 1 << 20,
            ..IndexOptions::default()
        },
    ));
    while index.bytes_scanned() == 0 {
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
    cancel.cancel();
    let at_cancel = index.bytes_scanned();
    let result = task.await.unwrap();
    assert_eq!(result, Err(IndexError::Cancelled));
    assert!(!index.is_complete());
    // Every running scanner finishes at most its current 64 KiB block
    // (progress counts one run per chunk; both runs stop alike).
    let after = index.bytes_scanned();
    assert!(
        after - at_cancel <= threads as u64 * 64 * 1024,
        "{} bytes scanned after cancel",
        after - at_cancel
    );
    assert!(after < src.len());
    // No scanner keeps running after the orchestrator returned.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(index.bytes_scanned(), after);
}

#[test]
fn empty_and_header_only_files() {
    for content in [&b""[..], b"a,b\n", b"a,b", b"\n\n", b"a,b\n\n# x\n"] {
        let d = Dialect {
            comment: Some(b'#'),
            ..Dialect::default()
        };
        let (_f, src) = temp_source(content, d);
        let (index, summary) = runtime().block_on(build(&src, false, 64 << 20, 2, 1));
        assert_eq!(summary.total_rows, 0, "{content:?}");
        assert_eq!(index.total_rows(), Some(0));
    }
}

#[test]
fn unterminated_quote_is_flagged() {
    let (_f, src) = temp_source(b"a,b\n1,2\n3,\"open\nmore\n", Dialect::default());
    for slow in [false, true] {
        let (index, summary) = runtime().block_on(build(&src, slow, 3, 2, 1));
        assert!(summary.unterminated_quote);
        assert!(index.unterminated_quote());
        assert_eq!(summary.total_rows, 2);
    }
}

#[test]
fn sniffed_quoted_newlines_select_the_slow_path() {
    let content = generated(64 << 10);
    let r = sniff(&content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    assert!(r.quoted_newlines);
    let (_f, src) = temp_source(&content, r.dialect);
    let index = Arc::new(RowIndex::for_source(&src));
    let summary = runtime()
        .block_on(build_index(
            Arc::clone(&src),
            index,
            r,
            Executor::with_handle(runtime().handle().clone(), 2),
            CancellationToken::new(),
            IndexOptions::default(),
        ))
        .unwrap();
    assert_eq!(summary.path_used, IndexPath::Slow);
}
