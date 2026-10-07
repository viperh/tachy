//! `Source`: one opened file, memory-mapped read-only (spec §4.2, §5.1, §16).
//!
//! A `Source` is immutable once built and is shared with background tasks as
//! `Arc<Source>`. Changing the dialect (M2-04) or reloading (M7-03) builds a
//! new `Source` with [`Source::with_dialect`], which shares the same mapping,
//! so a running job never sees a dialect change mid-scan.
//!
//! The row index, column metadata and statistics change over time, so they do
//! not live here: the UI-side `Tab` keeps them next to its `Arc<Source>`
//! (`Arc<RowIndex>`, `ColumnMeta`).
//!
//! The file is never opened for writing (§1).

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use memmap2::Mmap;
use thiserror::Error;

use crate::{
    dialect::{self, ColumnName, Dialect, DialectOverrides, SniffReport},
    parse::{ParseOutcome, RecordParser, RecordRanges},
};

/// Why a file could not be opened. Each variant has text fit for a toast.
#[derive(Debug, Error)]
pub enum SourceError {
    /// The path does not exist.
    #[error("{}: file not found", .0.display())]
    NotFound(PathBuf),
    /// The file exists but cannot be read.
    #[error("{}: permission denied", .0.display())]
    PermissionDenied(PathBuf),
    /// The path is a directory.
    #[error("{}: is a directory", .0.display())]
    IsDirectory(PathBuf),
    /// A FIFO, socket or device: it cannot be memory-mapped.
    #[error("{}: not a regular file (use `-` to read a pipe)", .0.display())]
    NotRegular(PathBuf),
    /// Any other I/O error.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl SourceError {
    fn from_io(path: &Path, e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => SourceError::NotFound(path.to_path_buf()),
            io::ErrorKind::PermissionDenied => SourceError::PermissionDenied(path.to_path_buf()),
            io::ErrorKind::IsADirectory => SourceError::IsDirectory(path.to_path_buf()),
            _ => SourceError::Io {
                path: path.to_path_buf(),
                source: e,
            },
        }
    }
}

/// Shared between every `Source` built over the same mapping: the count of
/// active sequential scans, which decides the `madvise` hint.
#[derive(Debug, Default)]
struct ScanState {
    /// Active scans. A mutex rather than an atomic so the advice calls of a
    /// 0→1 and a concurrent 1→0 transition can't be applied out of order.
    active: Mutex<usize>,
    #[cfg(test)]
    advised_sequential: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    advised_random: std::sync::atomic::AtomicUsize,
}

/// One opened file. See the module docs.
#[derive(Debug)]
pub struct Source {
    path: PathBuf,
    display_name: String,
    /// `None` for an empty file (mapping 0 bytes fails on Linux).
    map: Option<Arc<Mmap>>,
    len: u64,
    mtime: SystemTime,
    scans: Arc<ScanState>,
    dialect: Dialect,
    header: Option<Vec<String>>,
    /// Field values of the header record (unescaped), for `column_names`.
    header_raw: Option<Vec<Vec<u8>>>,
    data_start: u64,
    /// Column count `H` (§6.4), computed on first use when headerless.
    width: OnceLock<usize>,
}

impl Source {
    /// Opens `path` read-only and memory-maps it.
    ///
    /// The source starts with a neutral dialect (`Dialect::default()` without
    /// a header). Sniff it and call [`Source::with_dialect`], or use
    /// [`Source::open_sniffed`]. `display_name` defaults to the file name.
    ///
    /// This is blocking I/O: the UI task calls it in `spawn_blocking`.
    pub fn open(path: &Path, display_name: Option<String>) -> Result<Source, SourceError> {
        // `metadata` first: opening a FIFO for reading would block.
        let meta = std::fs::metadata(path).map_err(|e| SourceError::from_io(path, e))?;
        if meta.is_dir() {
            return Err(SourceError::IsDirectory(path.to_path_buf()));
        }
        if !meta.is_file() {
            return Err(SourceError::NotRegular(path.to_path_buf()));
        }
        let file = File::open(path).map_err(|e| SourceError::from_io(path, e))?;
        let meta = file.metadata().map_err(|e| SourceError::from_io(path, e))?;
        if !meta.is_file() {
            return Err(SourceError::NotRegular(path.to_path_buf()));
        }
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let map = if len == 0 {
            None
        } else {
            // SAFETY: the map is read-only and tachy never writes through it
            // or to the file. Another process can still modify or truncate
            // the file while it is mapped: reading a page past the new end
            // raises SIGBUS (handled in M7-03), and modified bytes may show
            // up in later reads. Both are documented limitations of mmap
            // (spec §16).
            let map = unsafe { Mmap::map(&file) }.map_err(|e| SourceError::from_io(path, e))?;
            Some(Arc::new(map))
        };
        let display_name = display_name.unwrap_or_else(|| {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        });
        let raw = Source {
            path: path.to_path_buf(),
            display_name,
            map,
            len,
            mtime,
            scans: Arc::default(),
            dialect: Dialect::default(),
            header: None,
            header_raw: None,
            data_start: 0,
            width: OnceLock::new(),
        };
        Ok(raw.with_dialect(Dialect {
            header: false,
            ..Dialect::default()
        }))
    }

    /// Opens `path`, sniffs it (§2.1) and applies the resulting dialect.
    ///
    /// For a UTF-16 file the returned source still holds the UTF-16 bytes:
    /// transcode it with `spool::transcode_to_utf8`, open the temp file and
    /// apply `report.dialect.transcoded()`.
    pub fn open_sniffed(
        path: &Path,
        display_name: Option<String>,
        sample_bytes: usize,
        overrides: &DialectOverrides,
    ) -> Result<(Source, SniffReport), SourceError> {
        let raw = Source::open(path, display_name)?;
        let report = dialect::sniff(raw.bytes(), sample_bytes, overrides);
        Ok((raw.with_dialect(report.dialect), report))
    }

    /// A new `Source` over the same mapping with another dialect. The header
    /// and `data_start` are recomputed; the mapping is shared, not copied.
    pub fn with_dialect(&self, dialect: Dialect) -> Source {
        let bytes = self.map.as_deref().map_or(&[][..], |m| &m[..]);
        let mut parser = RecordParser::new(&dialect);
        let bom = dialect.encoding.bom_len_in(bytes);
        let first = parser.skip_ignorable(bytes, bom);
        let (header, header_raw, data_start) = if dialect.header {
            let mut rec = RecordRanges::default();
            match parser.parse_at(bytes, first as u64, &mut rec) {
                ParseOutcome::Eof => (Some(Vec::new()), Some(Vec::new()), bytes.len() as u64),
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                    let mut scratch = Vec::new();
                    let raw: Vec<Vec<u8>> = (0..rec.fields.len())
                        .map(|i| parser.field_value(bytes, &rec, i, &mut scratch).to_vec())
                        .collect();
                    let names = raw
                        .iter()
                        .map(|v| crate::parse::decode_field(v, dialect.encoding).into_owned())
                        .collect();
                    (Some(names), Some(raw), next)
                }
            }
        } else {
            (None, None, first as u64)
        };
        let width = OnceLock::new();
        if let Some(h) = &header {
            let _ = width.set(h.len());
        }
        Source {
            path: self.path.clone(),
            display_name: self.display_name.clone(),
            map: self.map.clone(),
            len: self.len,
            mtime: self.mtime,
            scans: Arc::clone(&self.scans),
            dialect,
            header,
            header_raw,
            data_start,
            width,
        }
    }

    /// The whole file. Empty for an empty file.
    pub fn bytes(&self) -> &[u8] {
        self.map.as_deref().map_or(&[], |m| &m[..])
    }

    /// File size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True for a 0-byte file (§16 "empty file" placeholder).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The path the file was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tab title: the file name, or `stdin` for spooled input (M1-08).
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Modification time when opened (M7-03 change detection).
    pub fn mtime(&self) -> SystemTime {
        self.mtime
    }

    /// The dialect this source is parsed with.
    pub fn dialect(&self) -> &Dialect {
        &self.dialect
    }

    /// Decoded header names, as written. `None` when the dialect has no
    /// header.
    pub fn header(&self) -> Option<&[String]> {
        self.header.as_deref()
    }

    /// Offset of the first data byte: after the BOM, leading blank and
    /// comment lines, and the header record. Checkpoint 0 of the row index.
    pub fn data_start(&self) -> u64 {
        self.data_start
    }

    /// The column count `H` (§6.4): the header's field count, or the first
    /// record's field count when there is no header (0 without records).
    pub fn width(&self) -> usize {
        *self.width.get_or_init(|| {
            let bytes = self.bytes();
            let mut rec = RecordRanges::default();
            match RecordParser::new(&self.dialect).parse_at(bytes, self.data_start, &mut rec) {
                ParseOutcome::Eof => 0,
                _ => rec.fields.len(),
            }
        })
    }

    /// Column names (§9.1): header names, or `col1`… when headerless.
    pub fn column_names(&self) -> Vec<ColumnName> {
        match &self.header_raw {
            Some(raw) => dialect::column_names(raw, true, self.dialect.encoding),
            None => {
                let fake = vec![Vec::new(); self.width()];
                dialect::column_names(&fake, false, self.dialect.encoding)
            }
        }
    }

    /// True when both sources share one memory mapping (same file, built
    /// with [`Source::with_dialect`]).
    pub fn shares_mapping(&self, other: &Source) -> bool {
        match (&self.map, &other.map) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => Arc::ptr_eq(&self.scans, &other.scans),
            _ => false,
        }
    }

    /// Marks the start of a sequential scan (§5.1). While at least one guard
    /// is alive the mapping is advised `Sequential`; when the last one drops
    /// it goes back to `Random` (interactive browsing). The indexer, filter,
    /// profile, export and sort extraction each hold one while they run.
    pub fn begin_scan(&self) -> ScanGuard {
        let guard = ScanGuard {
            scans: Arc::clone(&self.scans),
            map: self.map.clone(),
        };
        let mut active = guard.scans.active.lock().unwrap_or_else(|e| e.into_inner());
        *active += 1;
        if *active == 1 {
            guard.advise(true);
        }
        drop(active);
        guard
    }
}

/// Keeps the mapping advised for sequential access. See
/// [`Source::begin_scan`].
#[derive(Debug)]
#[must_use = "the scan ends when the guard is dropped"]
pub struct ScanGuard {
    scans: Arc<ScanState>,
    #[cfg_attr(not(unix), allow(dead_code))]
    map: Option<Arc<Mmap>>,
}

impl ScanGuard {
    fn advise(&self, sequential: bool) {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::Relaxed;
            if sequential {
                self.scans.advised_sequential.fetch_add(1, Relaxed);
            } else {
                self.scans.advised_random.fetch_add(1, Relaxed);
            }
        }
        #[cfg(unix)]
        if let Some(map) = &self.map {
            let advice = if sequential {
                memmap2::Advice::Sequential
            } else {
                memmap2::Advice::Random
            };
            if let Err(e) = map.advise(advice) {
                tracing::debug!("madvise({advice:?}) failed: {e}");
            }
        }
        // `memmap2::Advice` does not exist on Windows: nothing to do there.
        #[cfg(not(unix))]
        let _ = sequential;
    }
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        let mut active = self.scans.active.lock().unwrap_or_else(|e| e.into_inner());
        *active -= 1;
        if *active == 0 {
            self.advise(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, sync::atomic::Ordering::Relaxed};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::dialect::DEFAULT_SAMPLE_BYTES;

    fn file_with(content: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn empty_file() {
        let f = file_with(b"");
        let s = Source::open(f.path(), None).unwrap();
        assert!(s.is_empty());
        assert_eq!(s.bytes(), b"");
        assert_eq!(s.width(), 0);
        let s = s.with_dialect(Dialect::default());
        assert_eq!(s.header(), Some(&[][..]));
        assert_eq!(s.data_start(), 0);
    }

    #[test]
    fn bytes_are_exact() {
        let content = b"a,b\n1,2\n";
        let f = file_with(content);
        let s = Source::open(f.path(), None).unwrap();
        assert_eq!(s.bytes(), content);
        assert_eq!(s.len(), content.len() as u64);
        assert!(!s.is_empty());
        assert_eq!(
            s.display_name(),
            f.path().file_name().unwrap().to_str().unwrap()
        );
        let s2 = Source::open(f.path(), Some("stdin".into())).unwrap();
        assert_eq!(s2.display_name(), "stdin");
    }

    #[test]
    fn missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = Source::open(&dir.path().join("nope.csv"), None).unwrap_err();
        assert!(matches!(err, SourceError::NotFound(_)), "{err:?}");
        assert!(err.to_string().ends_with("nope.csv: file not found"));
    }

    #[test]
    fn directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = Source::open(dir.path(), None).unwrap_err();
        assert!(matches!(err, SourceError::IsDirectory(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let f = file_with(b"x\n");
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(f.path()).is_ok() {
            eprintln!("skipped: running as root");
            return;
        }
        let err = Source::open(f.path(), None).unwrap_err();
        assert!(matches!(err, SourceError::PermissionDenied(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn devices_and_fifos_are_not_regular() {
        let err = Source::open(Path::new("/dev/null"), None).unwrap_err();
        assert!(matches!(err, SourceError::NotRegular(_)), "{err:?}");
        assert!(err.to_string().contains("use `-` to read a pipe"));
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        if made {
            let err = Source::open(&fifo, None).unwrap_err();
            assert!(matches!(err, SourceError::NotRegular(_)), "{err:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn stdin_pipe_is_not_regular() {
        // `/dev/stdin` of a child whose stdin is a pipe.
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "source::tests::stdin_probe",
                "--nocapture",
                "--ignored",
            ])
            .stdin(std::process::Stdio::piped())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("PROBE NotRegular"), "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "run by stdin_pipe_is_not_regular with a piped stdin"]
    fn stdin_probe() {
        match Source::open(Path::new("/dev/stdin"), None) {
            Err(SourceError::NotRegular(_)) => println!("PROBE NotRegular"),
            other => println!("PROBE {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_100gb_file_opens_fast() {
        let f = tempfile::NamedTempFile::new().unwrap();
        if f.as_file().set_len(100 << 30).is_err() {
            eprintln!("skipped: filesystem has no sparse files");
            return;
        }
        let t = std::time::Instant::now();
        let s = Source::open(f.path(), None).unwrap();
        let elapsed = t.elapsed();
        assert_eq!(s.len(), 100 << 30);
        assert!(elapsed.as_millis() < 10, "open took {elapsed:?}");
    }

    #[test]
    fn with_dialect_shares_the_mapping() {
        let f = file_with(b"\xEF\xBB\xBF# comment\nname,age\nbob,3\n");
        let raw = Source::open(f.path(), None).unwrap();
        let d = Dialect {
            comment: Some(b'#'),
            encoding: crate::dialect::Encoding::Utf8Bom,
            ..Dialect::default()
        };
        let s = raw.with_dialect(d);
        assert!(Arc::ptr_eq(
            raw.map.as_ref().unwrap(),
            s.map.as_ref().unwrap()
        ));
        assert!(s.shares_mapping(&raw));
        assert_eq!(
            s.header(),
            Some(&["name".to_string(), "age".to_string()][..])
        );
        assert_eq!(&s.bytes()[s.data_start() as usize..], b"bob,3\n");
        assert_eq!(s.width(), 2);
        assert_eq!(s.column_names()[1].query, "age");
        // Headerless: data starts after the BOM and the comment.
        let s = raw.with_dialect(Dialect { header: false, ..d });
        assert_eq!(&s.bytes()[s.data_start() as usize..], b"name,age\nbob,3\n");
        assert_eq!(s.column_names()[0].query, "col1");
    }

    #[test]
    fn open_sniffed_applies_the_dialect() {
        let f = file_with(b"id;name\n1;a\n2;b\n");
        let (s, report) =
            Source::open_sniffed(f.path(), None, DEFAULT_SAMPLE_BYTES, &Default::default())
                .unwrap();
        assert_eq!(report.dialect.delimiter, b';');
        assert_eq!(s.dialect(), &report.dialect);
        assert_eq!(s.header().unwrap(), ["id", "name"]);
    }

    #[test]
    fn nested_scan_guards_advise_once() {
        let f = file_with(b"a\n");
        let s = Source::open(f.path(), None).unwrap();
        let other = s.with_dialect(Dialect::default());
        let seq = || s.scans.advised_sequential.load(Relaxed);
        let rnd = || s.scans.advised_random.load(Relaxed);
        let g1 = s.begin_scan();
        let g2 = other.begin_scan();
        assert_eq!((seq(), rnd()), (1, 0));
        drop(g1);
        assert_eq!((seq(), rnd()), (1, 0));
        drop(g2);
        assert_eq!((seq(), rnd()), (1, 1));
        let _g3 = s.begin_scan();
        assert_eq!((seq(), rnd()), (2, 1));
    }
}
