//! M6-01: export.
//!
//! - Round trips through an independent reference parser (the `csv` crate)
//!   and through tachy's own parser, across quoting modes and delimiters,
//!   with quotes, delimiters, newlines and invalid UTF-8 in values.
//! - View order (filtered, sorted), column subsets, ragged rows.
//! - Atomicity: cancelled and failing exports leave no `.partial` file and
//!   an existing target untouched.
//! - Refusing to export onto the source (symlink, `./`, hard link).

mod support;

use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use roaring::RoaringTreemap;
use support::{runtime, temp_source};
use tachy_core::{
    column::ColumnMeta,
    dialect::{DEFAULT_SAMPLE_BYTES, Dialect, DialectOverrides, sniff},
    exec::Executor,
    export::{
        ExportColumn, ExportContext, ExportOptions, ExportRows, Quoting, TargetError, check_target,
        partial_path, run_export, run_export_with,
    },
    index::{IndexOptions, RowIndex, build_index},
    jobs::{ExportProgress, JobControl},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    source::Source,
    view::{FilterRows, RowIdList},
};
use tokio_util::sync::CancellationToken;

struct Fx {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    cols: Vec<ColumnMeta>,
}

fn fixture_with(content: &[u8], dialect: Option<Dialect>) -> Fx {
    let report = sniff(content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let (file, src) = temp_source(content, dialect.unwrap_or(report.dialect));
    let rt = runtime();
    let index = Arc::new(RowIndex::with_stride(src.data_start(), 8));
    rt.block_on(build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report,
        Executor::with_handle(rt.handle().clone(), 4),
        CancellationToken::new(),
        IndexOptions {
            chunk_size: 1024,
            ..IndexOptions::default()
        },
    ))
    .unwrap();
    let mut cols: Vec<ColumnMeta> = src
        .column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| ColumnMeta::new(n, i, false))
        .collect();
    // Synthetic `_extraN` columns after the header (ragged long rows).
    let max_fields = all_records(&src).iter().map(Vec::len).max().unwrap_or(0);
    for i in cols.len()..max_fields {
        let n = i - src.width() + 1;
        let name = tachy_core::parse::extra_column_name(n);
        cols.push(ColumnMeta::new(
            tachy_core::column::ColumnName {
                display: name.clone(),
                query: name,
            },
            i,
            true,
        ));
    }
    Fx {
        _file: file,
        src,
        index,
        cols,
    }
}

fn fixture(content: &[u8]) -> Fx {
    fixture_with(content, None)
}

/// Every record's unescaped field values, with tachy's parser.
fn all_records(src: &Source) -> Vec<Vec<Vec<u8>>> {
    let bytes = src.bytes();
    let mut p = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut pos = src.data_start();
    let mut out = Vec::new();
    loop {
        match p.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                out.push(
                    (0..rec.fields.len())
                        .map(|i| p.field_value(bytes, &rec, i, &mut scratch).to_vec())
                        .collect(),
                );
                pos = next;
            }
            ParseOutcome::Eof => return out,
        }
    }
}

/// The expected exported rows: `rows` in order, `fields` selected, missing
/// cells empty.
fn expected(src: &Source, rows: &[u64], fields: &[usize]) -> Vec<Vec<Vec<u8>>> {
    let all = all_records(src);
    rows.iter()
        .map(|&r| {
            fields
                .iter()
                .map(|&f| all[r as usize].get(f).cloned().unwrap_or_default())
                .collect()
        })
        .collect()
}

/// Reads `path` with the `csv` crate (strict field counts).
fn read_csv(path: &Path, delim: u8) -> Vec<Vec<Vec<u8>>> {
    let mut r = csv::ReaderBuilder::new()
        .has_headers(false)
        .delimiter(delim)
        .flexible(false)
        .from_path(path)
        .unwrap();
    r.byte_records()
        .map(|rec| rec.unwrap().iter().map(<[u8]>::to_vec).collect())
        .collect()
}

/// Reads `path` back with tachy (`delim`, standard quoting).
fn read_tachy(path: &Path, delim: u8, header: bool) -> Vec<Vec<Vec<u8>>> {
    let src = Source::open(path, None).unwrap().with_dialect(Dialect {
        delimiter: delim,
        header,
        ..Dialect::default()
    });
    all_records(&src)
}

fn ctx(threads: usize) -> (ExportContext, Arc<ExportProgress>) {
    let rt = runtime();
    let progress = Arc::new(ExportProgress::default());
    (
        ExportContext {
            exec: Executor::with_handle(rt.handle().clone(), threads),
            ctl: JobControl::new(CancellationToken::new()),
            progress: Arc::clone(&progress),
        },
        progress,
    )
}

fn opts(delim: u8, quoting: Quoting, header: bool) -> ExportOptions {
    ExportOptions {
        delimiter: delim,
        quoting,
        header,
        write_buffer: 4096,
        chunk_bytes: 700,
        batch_rows: 13,
    }
}

const TRICKY: &[u8] = b"id,name,note,raw\n\
1,plain,simple,a\n\
2,\"with, comma\",\"say \"\"hi\"\"\",b\n\
3,\"multi\nline\",\"tab\there\",c\n\
4,pipe|semi;colon,\"\",\xff\xfeinvalid\n\
5, spaced ,\"cr\r\nlf\",\n\
6,short\n\
7,long,x,y,extra1,extra2\n\
8,\"\",\"\"\"\",\"a\"\"b\"\"c\"\n";

#[test]
fn round_trips_across_quoting_and_delimiters() {
    let fx = fixture(TRICKY);
    let total = fx.index.total_rows().unwrap();
    assert_eq!(total, 8);
    let fields: Vec<usize> = (0..fx.cols.len()).collect();
    let cols = ExportColumn::select(&fx.cols, &fields, false);
    assert_eq!(cols.len(), 6, "header + 2 extra columns");
    let want = expected(&fx.src, &(0..total).collect::<Vec<_>>(), &fields);
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime();
    for delim in *b",\t|; " {
        for quoting in [Quoting::Minimal, Quoting::All] {
            for threads in [1, 4] {
                let target = dir
                    .path()
                    .join(format!("out-{delim}-{quoting}-{threads}.csv"));
                let (ctx, progress) = ctx(threads);
                let summary = rt
                    .block_on(run_export(
                        ExportRows::All,
                        Arc::clone(&fx.src),
                        Arc::clone(&fx.index),
                        cols.clone(),
                        opts(delim, quoting, true),
                        target.clone(),
                        ctx,
                    ))
                    .unwrap();
                assert_eq!(summary.rows, total);
                assert_eq!(summary.bytes, fs::metadata(&target).unwrap().len());
                assert_eq!(progress.rows_written(), total);
                assert_eq!(progress.bytes_written(), summary.bytes);
                assert!(!partial_path(&target).exists());

                let got = read_csv(&target, delim);
                let names: Vec<Vec<u8>> = cols.iter().map(|c| c.name.as_bytes().to_vec()).collect();
                assert_eq!(got[0], names, "header");
                assert_eq!(got[1..], want[..], "csv crate, {delim} {quoting}");
                if delim != b' ' {
                    // tachy reads ` spaced ` with a space delimiter differently
                    // only when unquoted; the csv crate is the reference.
                    let back = read_tachy(&target, delim, true);
                    assert_eq!(back, want, "tachy, {delim} {quoting}");
                }
                let text = fs::read(&target).unwrap();
                assert!(text.ends_with(b"\n"));
                assert!(!text.starts_with(b"\xef\xbb\xbf"));
            }
        }
    }
}

#[test]
fn invalid_utf8_and_bom() {
    let mut content = b"\xef\xbb\xbfa,b\n".to_vec();
    content.extend_from_slice(b"\xff\xfe\x80,ok\n\xe9t\xe9,\x1b[31m\n");
    let fx = fixture(&content);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("o.csv");
    let (ctx, _) = ctx(2);
    runtime()
        .block_on(run_export(
            ExportRows::All,
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            ExportColumn::select(&fx.cols, &[0, 1], true),
            opts(b',', Quoting::Minimal, true),
            target.clone(),
            ctx,
        ))
        .unwrap();
    assert_eq!(
        fs::read(&target).unwrap(),
        b"a,b\n\xff\xfe\x80,ok\n\xe9t\xe9,\x1b[31m\n",
        "raw bytes kept, no BOM, no escaping"
    );
}

fn export_ids(fx: &Fx, view: ExportRows, fields: &[usize], threads: usize) -> Vec<Vec<Vec<u8>>> {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("o.csv");
    let (ctx, _) = ctx(threads);
    runtime()
        .block_on(run_export(
            view,
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            ExportColumn::select(&fx.cols, fields, true),
            opts(b',', Quoting::Minimal, false),
            target.clone(),
            ctx,
        ))
        .unwrap();
    read_csv(&target, b',')
}

fn generated(rows: usize) -> Vec<u8> {
    let mut out = b"id,name,city,amount\n".to_vec();
    for i in 0..rows {
        let city = match i % 5 {
            0 => "\"Paris, FR\"".to_owned(),
            1 => "\"multi\nline\"".to_owned(),
            2 => "\"q\"\"uote\"".to_owned(),
            _ => format!("city{}", i % 13),
        };
        out.extend_from_slice(format!("{i},name{},{city},{}\n", i * 7 % 101, i % 1000).as_bytes());
    }
    out
}

#[test]
fn filtered_and_sorted_views_in_view_order() {
    let fx = fixture(&generated(3000));
    let total = fx.index.total_rows().unwrap();
    // Filtered: every third row, file order.
    let ids: Vec<u64> = (0..total).filter(|r| r % 3 == 0).collect();
    let rows = Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter(
        ids.iter().copied(),
    )));
    for threads in [1, 6] {
        let got = export_ids(
            &fx,
            ExportRows::Filtered(Arc::clone(&rows)),
            &[0, 1, 2, 3],
            threads,
        );
        assert_eq!(got, expected(&fx.src, &ids, &[0, 1, 2, 3]));
    }
    // Sorted: a permutation, visible columns in display order.
    let mut perm: Vec<u64> = (0..total).rev().collect();
    perm.swap(0, 1500);
    let dir = tempfile::tempdir().unwrap();
    let list = RowIdList::from_ids(dir.path(), &perm).unwrap();
    for threads in [1, 6] {
        let got = export_ids(
            &fx,
            ExportRows::Ordered(Arc::clone(&list)),
            &[3, 0],
            threads,
        );
        assert_eq!(got, expected(&fx.src, &perm, &[3, 0]));
    }
    // All, visible subset.
    let got = export_ids(&fx, ExportRows::All, &[2], 3);
    assert_eq!(
        got,
        expected(&fx.src, &(0..total).collect::<Vec<_>>(), &[2])
    );
}

#[test]
fn waits_for_a_growing_view() {
    let fx = fixture(&generated(500));
    let rows = Arc::new(FilterRows::new_growing());
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("o.csv");
    let (ctx, progress) = ctx(2);
    let rt = runtime();
    let summary = rt.block_on(async {
        let job = tokio::spawn(run_export(
            ExportRows::Filtered(Arc::clone(&rows)),
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            ExportColumn::select(&fx.cols, &[0], true),
            opts(b',', Quoting::Minimal, false),
            target.clone(),
            ctx,
        ));
        rows.insert_many([5, 7]);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!job.is_finished());
        rows.insert_many([9]);
        rows.finish();
        job.await.unwrap().unwrap()
    });
    assert_eq!(summary.rows, 3);
    assert_eq!(progress.total_rows(), 3);
    assert_eq!(fs::read(&target).unwrap(), b"5\n7\n9\n");
}

/// A writer that fails once `limit` bytes were written (a full disk).
struct Failing {
    inner: fs::File,
    left: usize,
}

impl Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "No space left on device",
            ));
        }
        let n = buf.len().min(self.left);
        self.left -= n;
        self.inner.write(&buf[..n])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn failing_writer_and_cancel_leave_no_partial_file() {
    let fx = fixture(&generated(5000));
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("o.csv");
    fs::write(&target, b"previous content").unwrap();
    let rt = runtime();

    for limit in [0, 10, 100_000] {
        let (ctx, _) = ctx(4);
        let r = rt.block_on(run_export_with(
            ExportRows::All,
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            ExportColumn::select(&fx.cols, &[0, 1, 2, 3], true),
            opts(b',', Quoting::Minimal, true),
            target.clone(),
            ctx,
            Some(Box::new(move |f| {
                Box::new(Failing {
                    inner: f,
                    left: limit,
                })
            })),
        ));
        let e = r.unwrap_err();
        assert!(!e.is_cancelled());
        assert!(e.to_string().contains("No space left"), "{e}");
        assert!(!partial_path(&target).exists(), "partial removed");
        assert_eq!(fs::read(&target).unwrap(), b"previous content");
    }

    // Killed while paused mid-way.
    let (ctx, _) = ctx(2);
    let ctl = ctx.ctl.clone();
    ctl.pause.pause();
    let r = rt.block_on(async {
        let job = tokio::spawn(run_export(
            ExportRows::All,
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            ExportColumn::select(&fx.cols, &[0, 1, 2, 3], true),
            opts(b',', Quoting::All, true),
            target.clone(),
            ctx,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            partial_path(&target).exists(),
            "writing to .partial: finished={} {:?}",
            job.is_finished(),
            fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>()
        );
        ctl.cancel();
        job.await.unwrap()
    });
    assert!(r.unwrap_err().is_cancelled());
    assert!(!partial_path(&target).exists());
    assert_eq!(fs::read(&target).unwrap(), b"previous content");
}

#[test]
fn refuses_the_source_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("orders.csv");
    fs::write(&source, b"a\n1\n").unwrap();
    let refused = |t: &Path| matches!(check_target(t, &source), Err(TargetError::SourceFile));
    assert!(refused(&source));
    assert!(refused(&dir.path().join(".").join("orders.csv")));
    fs::create_dir(dir.path().join("sub")).unwrap();
    assert!(refused(&dir.path().join("sub/../orders.csv")));
    #[cfg(unix)]
    {
        let link = dir.path().join("link.csv");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(refused(&link), "through a symlink");
        let hard = dir.path().join("hard.csv");
        fs::hard_link(&source, &hard).unwrap();
        assert!(refused(&hard), "through a hard link");
        let dlink = dir.path().join("dlink");
        std::os::unix::fs::symlink(dir.path(), &dlink).unwrap();
        assert!(
            refused(&dlink.join("orders.csv")),
            "through a symlinked directory"
        );
    }
    // Relative `./` path from the source's directory.
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let rel = check_target(Path::new("./orders.csv"), &source);
    let rel2 = check_target(Path::new("orders.csv"), Path::new("./orders.csv"));
    let ok = check_target(Path::new("./other.csv"), &source);
    std::env::set_current_dir(cwd).unwrap();
    assert!(matches!(rel, Err(TargetError::SourceFile)));
    assert!(matches!(rel2, Err(TargetError::SourceFile)));
    ok.unwrap();

    check_target(&dir.path().join("orders.filtered.csv"), &source).unwrap();
    assert!(matches!(
        check_target(&dir.path().join("missing/x.csv"), &source),
        Err(TargetError::NoDirectory(_))
    ));
    assert!(matches!(
        check_target(dir.path(), &source),
        Err(TargetError::IsDirectory(_)) | Err(TargetError::NoFileName)
    ));
    assert_eq!(
        TargetError::SourceFile.to_string(),
        "refusing to overwrite the source file"
    );
}

#[test]
fn windows_1252_header_and_values() {
    let content = b"n\xe9,city\nRen\xe9,Z\xfcrich\n";
    let d = Dialect {
        encoding: tachy_core::dialect::Encoding::Windows1252,
        ..Dialect::default()
    };
    let fx = fixture_with(content, Some(d));
    assert_eq!(fx.cols[0].name.display, "né");
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("o.csv");
    let (ctx, _) = ctx(1);
    runtime()
        .block_on(run_export(
            ExportRows::All,
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            ExportColumn::select(&fx.cols, &[0, 1], true),
            opts(b';', Quoting::Minimal, true),
            target.clone(),
            ctx,
        ))
        .unwrap();
    assert_eq!(
        fs::read(&target).unwrap(),
        b"n\xe9;city\nRen\xe9;Z\xfcrich\n"
    );
}
