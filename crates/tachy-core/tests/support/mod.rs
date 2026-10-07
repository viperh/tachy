//! Helpers shared by the integration tests (M7-05): temp files, opening and
//! indexing sources, a shared tokio runtime and the sequential reference
//! parser that the parallel indexer is checked against (§19).
//!
//! Use from a test file with `mod support;`. Each test binary compiles its
//! own copy and uses a subset, hence the `dead_code` allowance.
#![allow(dead_code)]

use std::{
    io::Write,
    path::Path,
    sync::{Arc, OnceLock},
};

use tachy_core::{
    dialect::{DEFAULT_SAMPLE_BYTES, Dialect, DialectOverrides, SniffReport},
    exec::Executor,
    index::{IndexOptions, IndexSummary, RowIndex, build_index},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    source::Source,
};
use tokio_util::sync::CancellationToken;

/// A 4-worker multi-threaded runtime shared by every test in the binary.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap()
    })
}

/// A temp file holding `content`, deleted on drop.
pub fn temp_file(content: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    f
}

/// `content` in a temp file, opened with `dialect`. Keep the file alive as
/// long as the source.
pub fn temp_source(content: &[u8], dialect: Dialect) -> (tempfile::NamedTempFile, Arc<Source>) {
    let f = temp_file(content);
    let src = Source::open(f.path(), None).unwrap().with_dialect(dialect);
    (f, Arc::new(src))
}

/// Opens and sniffs `path` with the default sample size and no overrides.
pub fn open_sniffed(path: &Path) -> (Arc<Source>, SniffReport) {
    open_sniffed_with(path, &DialectOverrides::default())
}

/// Opens and sniffs `path` with the default sample size and `overrides`.
pub fn open_sniffed_with(path: &Path, overrides: &DialectOverrides) -> (Arc<Source>, SniffReport) {
    let (src, report) = Source::open_sniffed(path, None, DEFAULT_SAMPLE_BYTES, overrides).unwrap();
    (Arc::new(src), report)
}

/// Builds the full index of `src` with default options on [`runtime`].
pub fn index_source(src: &Arc<Source>, report: SniffReport) -> (Arc<RowIndex>, IndexSummary) {
    let rt = runtime();
    let index = Arc::new(RowIndex::for_source(src));
    let summary = rt
        .block_on(build_index(
            Arc::clone(src),
            Arc::clone(&index),
            report,
            Executor::with_handle(rt.handle().clone(), 4),
            CancellationToken::new(),
            IndexOptions::default(),
        ))
        .unwrap();
    (index, summary)
}

/// What the sequential reference parser finds: record starts, ragged
/// count, unterminated-quote flag.
#[derive(Debug, PartialEq)]
pub struct Reference {
    /// Byte offset of every record after the header.
    pub starts: Vec<u64>,
    /// Records whose field count differs from the source's width.
    pub ragged: u64,
    /// The file ends inside a quoted field.
    pub unterminated: bool,
}

/// Parses `src` record by record from `data_start`, one thread, no index.
pub fn reference(src: &Source) -> Reference {
    let mut p = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let width = src.width();
    let mut r = Reference {
        starts: Vec::new(),
        ragged: 0,
        unterminated: false,
    };
    let mut pos = src.data_start();
    loop {
        let next = match p.parse_at(src.bytes(), pos, &mut rec) {
            ParseOutcome::Eof => break,
            ParseOutcome::Record { next } => next,
            ParseOutcome::UnterminatedQuote { next } => {
                r.unterminated = true;
                next
            }
        };
        r.starts.push(rec.start);
        if rec.fields.len() != width {
            r.ragged += 1;
        }
        pos = next;
    }
    r
}
