//! M1-03: a UTF-16 file with a BOM is transcoded to a UTF-8 temp file and
//! shows the same table as its UTF-8 twin.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicU64},
};

use pretty_assertions::assert_eq;
use tachy_core::{
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, Encoding},
    parse::{ParseOutcome, RecordParser, RecordRanges, decode_field},
    source::Source,
    spool::transcode_to_utf8,
};
use tokio_util::sync::CancellationToken;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sniff")
        .join(name)
}

/// Header plus decoded rows of a source.
fn table(src: &Source) -> (Vec<String>, Vec<Vec<String>>) {
    let mut p = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut rows = Vec::new();
    let mut pos = src.data_start();
    loop {
        match p.parse_at(src.bytes(), pos, &mut rec) {
            ParseOutcome::Eof => break,
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                rows.push(
                    (0..rec.fields.len())
                        .map(|i| {
                            let v = p.field_value(src.bytes(), &rec, i, &mut scratch);
                            decode_field(v, src.dialect().encoding).into_owned()
                        })
                        .collect(),
                );
                pos = next;
            }
        }
    }
    (src.header().unwrap_or_default().to_vec(), rows)
}

async fn open_transcoded(path: &Path, want: Encoding) -> (tempfile::NamedTempFile, Source) {
    let (raw, report) = Source::open_sniffed(
        path,
        None,
        DEFAULT_SAMPLE_BYTES,
        &DialectOverrides::default(),
    )
    .unwrap();
    assert_eq!(report.dialect.encoding, want);
    let progress = Arc::new(AtomicU64::new(0));
    let tmp = transcode_to_utf8(
        path,
        report.dialect.encoding,
        &std::env::temp_dir(),
        Arc::clone(&progress),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        raw.len()
    );
    assert!(
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("tachy-utf16-")
    );
    let src = Source::open(tmp.path(), Some(raw.display_name().to_string()))
        .unwrap()
        .with_dialect(report.dialect.transcoded());
    (tmp, src)
}

#[tokio::test]
async fn utf16_files_show_the_same_table_as_their_utf8_twin() {
    let (twin, _) = Source::open_sniffed(
        &fixture("utf16_twin_utf8.csv"),
        None,
        DEFAULT_SAMPLE_BYTES,
        &DialectOverrides::default(),
    )
    .unwrap();
    let want = table(&twin);
    assert_eq!(want.1.len(), 3);
    for (name, enc) in [
        ("utf16le.csv", Encoding::Utf16Le),
        ("utf16be.csv", Encoding::Utf16Be),
    ] {
        let (tmp, src) = open_transcoded(&fixture(name), enc).await;
        assert_eq!(src.display_name(), name);
        assert_eq!(src.dialect().encoding, Encoding::Utf8);
        assert_eq!(table(&src), want, "{name}");
        // The temp file is removed with the handle (tab close).
        let path = tmp.path().to_path_buf();
        drop(src);
        drop(tmp);
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn cancelled_transcode_removes_the_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let r = transcode_to_utf8(
        &fixture("utf16le.csv"),
        Encoding::Utf16Le,
        dir.path(),
        Arc::new(AtomicU64::new(0)),
        cancel,
    )
    .await;
    assert!(matches!(r, Err(tachy_core::Error::Cancelled)));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
