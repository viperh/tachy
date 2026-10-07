//! Temporary files that stand in for a source: UTF-16 → UTF-8 transcoding
//! (M1-03), and stdin spooling (`-`, M1-08).
//!
//! UTF-16 files are transcoded once into a UTF-8 temp file so there is a
//! single parser path. The tab opens the temp file as its `Source` (keeping
//! the original file name as the display name and the original encoding for
//! the status line, `utf-16le → utf-8`), and deletes it when the tab closes
//! (`NamedTempFile` drop).

use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::{Error, Result, dialect::Encoding};

/// Size of the blocks read and decoded at a time.
const BLOCK: usize = 1 << 20;

/// Transcodes `src` (UTF-16 with or without its BOM) into a new UTF-8 temp
/// file in `tmp_dir`, named `tachy-utf16-*`.
///
/// Runs on the blocking pool. `progress` counts input bytes read. The temp
/// file is removed if the job is cancelled or fails.
///
/// Deviation from the M1-03 signature: `progress` and `cancel` are taken by
/// value (`Arc`, cloned token) because the work moves into `spawn_blocking`.
pub async fn transcode_to_utf8(
    src: &Path,
    enc: Encoding,
    tmp_dir: &Path,
    progress: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> Result<NamedTempFile> {
    let src = src.to_path_buf();
    let tmp_dir = tmp_dir.to_path_buf();
    tokio::task::spawn_blocking(move || transcode_blocking(&src, enc, &tmp_dir, &progress, &cancel))
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}

/// The blocking body of [`transcode_to_utf8`].
pub fn transcode_blocking(
    src: &Path,
    enc: Encoding,
    tmp_dir: &Path,
    progress: &AtomicU64,
    cancel: &CancellationToken,
) -> Result<NamedTempFile> {
    let io_err = |path: &Path| {
        let path: PathBuf = path.to_path_buf();
        move |source| Error::Io { path, source }
    };
    let mut input = File::open(src).map_err(io_err(src))?;
    let tmp = tempfile::Builder::new()
        .prefix("tachy-utf16-")
        .suffix(".csv")
        .tempfile_in(tmp_dir)
        .map_err(io_err(tmp_dir))?;
    let mut out = BufWriter::with_capacity(BLOCK, tmp.as_file());
    let mut decoder = enc.to_encoding_rs().new_decoder_without_bom_handling();
    let mut buf = vec![0u8; BLOCK];
    let mut decoded = vec![0u8; 3 * BLOCK + 16];
    let mut first = true;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let n = read_full(&mut input, &mut buf).map_err(io_err(src))?;
        progress.fetch_add(n as u64, Ordering::Relaxed);
        let mut chunk = &buf[..n];
        if first {
            first = false;
            if chunk.starts_with(enc.bom()) {
                chunk = &chunk[enc.bom().len()..];
            }
        }
        let last = n < buf.len();
        let mut consumed = 0;
        loop {
            // Malformed input (an unpaired surrogate) becomes `�`.
            let (result, read, written, _) =
                decoder.decode_to_utf8(&chunk[consumed..], &mut decoded, last);
            out.write_all(&decoded[..written])
                .map_err(io_err(tmp.path()))?;
            consumed += read;
            if result == encoding_rs::CoderResult::InputEmpty {
                break;
            }
        }
        if last {
            break;
        }
    }
    out.flush().map_err(io_err(tmp.path()))?;
    drop(out);
    Ok(tmp)
}

/// Reads until `buf` is full or EOF.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}
