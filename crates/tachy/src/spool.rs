//! stdin spooling (`-`, spec §3, M1-08).
//!
//! mmap needs a seekable file, so stdin is first copied into a temp file
//! (`tachy-stdin-*` in `--tmp`), which the tab then opens as a normal
//! `Source`. The tab owns the `NamedTempFile`, so the copy is deleted when
//! the tab closes or the app exits.
//!
//! Deviation from M1-08: the task places `spool_stdin` in
//! `tachy-core/src/spool.rs`; it lives here because that file belongs to
//! another work stream. It has no UI dependency and can move as is.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Size of one read: 1 MiB (M1-08).
const CHUNK: usize = 1 << 20;

/// Copies `reader` (stdin, or anything in tests) into `out` in 1 MiB chunks
/// until EOF, adding the bytes copied to `progress` as it goes. Returns the
/// total.
///
/// Cancelling `cancel` stops the copy, even while a read is blocked, with
/// an `Interrupted` error.
pub async fn spool_stdin<R, W>(
    mut reader: R,
    mut out: W,
    progress: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            n = reader.read(&mut buf) => n?,
        };
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).await?;
        total += n as u64;
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    out.flush().await?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn copies_everything_and_counts_progress() {
        let data: Vec<u8> = (0..3 * CHUNK + 17).map(|i| (i % 251) as u8).collect();
        let progress = Arc::new(AtomicU64::new(0));
        let mut out = Vec::new();
        let n = spool_stdin(
            &data[..],
            &mut out,
            Arc::clone(&progress),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(out, data);
        assert_eq!(progress.load(Ordering::Relaxed), data.len() as u64);
    }

    #[tokio::test]
    async fn writes_into_a_temp_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let file = tokio::fs::File::from_std(tmp.as_file().try_clone().unwrap());
        let progress = Arc::new(AtomicU64::new(0));
        spool_stdin(&b"a,b\n1,2\n"[..], file, progress, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), b"a,b\n1,2\n");
    }

    #[tokio::test]
    async fn cancel_stops_a_blocked_read() {
        // A reader that never produces anything.
        let (_keep_open, reader) = tokio::io::duplex(64);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(spool_stdin(
            reader,
            Vec::new(),
            Arc::new(AtomicU64::new(0)),
            cancel.clone(),
        ));
        cancel.cancel();
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
