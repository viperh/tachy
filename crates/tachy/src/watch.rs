//! File-change detection (spec §16, M7-03).
//!
//! Each tab backed by a real file gets a polling task: every
//! [`POLL_INTERVAL`] it reads the file's metadata and compares the length
//! and modification time with the [`FileStamp`] captured at open. On the
//! first difference it sends [`Msg::FileChanged`] once and stops; the reload
//! (`R`) starts a new watcher with the new stamp. The task also stops when
//! the tab's `CancellationToken` is cancelled (tab closed or reloaded).
//!
//! - Spooled stdin is not watched. A UTF-16 tab watches the original file,
//!   not its transcoded temp copy.
//! - A deleted file (`NotFound`) is reported as [`FileChange::Deleted`]; the
//!   mmap keeps working on Unix, because the inode stays alive.
//! - Other metadata errors (a flaky network mount) are ignored and retried
//!   on the next tick.
//!
//! The task wakes every 2 s independently of the UI tick (which only runs
//! while there is work, M0-02), so idle CPU stays ~0%.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use tachy_core::source::Source;
use tokio::{
    sync::mpsc::UnboundedSender,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::{msg::Msg, tab::TabId};

/// How often a file is checked.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The sticky status-line warning (M1-07 warning slot).
pub const CHANGED_TEXT: &str = "file changed on disk — R to reload";
/// The sticky warning when the file is gone.
pub const DELETED_TEXT: &str = "file changed on disk (deleted) — R to reload";

/// What identifies a version of a file: its length and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    /// `UNIX_EPOCH` when the platform has no mtime (as `Source` does).
    pub mtime: SystemTime,
}

impl FileStamp {
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        }
    }

    /// The stamp `Source::open` captured (M1-01).
    pub fn of_source(source: &Source) -> Self {
        Self {
            len: source.len(),
            mtime: source.mtime(),
        }
    }

    /// Reads the stamp of `path` now (blocking). For a UTF-16 tab, call it on
    /// the original file before transcoding.
    #[cfg(test)]
    pub fn read(path: &Path) -> io::Result<Self> {
        std::fs::metadata(path).map(|m| Self::from_metadata(&m))
    }
}

/// How the file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    /// Its length or modification time differs.
    Modified,
    /// It no longer exists.
    Deleted,
}

impl FileChange {
    /// The sticky status-line warning.
    pub fn warning_text(self) -> &'static str {
        match self {
            FileChange::Modified => CHANGED_TEXT,
            FileChange::Deleted => DELETED_TEXT,
        }
    }
}

/// Where the watcher reads metadata from; tests inject a fake.
pub trait MetadataSource: Send + Sync + 'static {
    fn stamp(&self, path: &Path) -> impl Future<Output = io::Result<FileStamp>> + Send;
}

/// The real file system, through `tokio::fs::metadata`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsMetadata;

impl MetadataSource for FsMetadata {
    async fn stamp(&self, path: &Path) -> io::Result<FileStamp> {
        tokio::fs::metadata(path)
            .await
            .map(|m| FileStamp::from_metadata(&m))
    }
}

/// What a watcher needs.
#[derive(Debug, Clone)]
pub struct WatchRequest {
    pub tab: TabId,
    /// The file to poll: the original path, also for UTF-16 tabs.
    pub path: PathBuf,
    /// The stamp captured at open.
    pub initial: FileStamp,
    /// The tab's token (or a child of it).
    pub cancel: CancellationToken,
}

/// Spawns the watcher of one tab on the real file system.
pub fn spawn(req: WatchRequest, tx: UnboundedSender<Msg>) -> JoinHandle<()> {
    spawn_with(FsMetadata, req, tx)
}

/// [`spawn`] with an injected metadata source.
pub fn spawn_with<S: MetadataSource>(
    source: S,
    req: WatchRequest,
    tx: UnboundedSender<Msg>,
) -> JoinHandle<()> {
    tokio::spawn(watch(source, req, tx))
}

/// Polls until the file changes (sends one message), the token is
/// cancelled, or the UI is gone.
async fn watch<S: MetadataSource>(source: S, req: WatchRequest, tx: UnboundedSender<Msg>) {
    let mut interval = tokio::time::interval_at(Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
    // After a suspend (`ctrl-z`), check once, not once per missed tick.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = req.cancel.cancelled() => return,
            _ = interval.tick() => {}
        }
        let change = match source.stamp(&req.path).await {
            Ok(stamp) if stamp == req.initial => None,
            Ok(_) => Some(FileChange::Modified),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(FileChange::Deleted),
            Err(e) => {
                tracing::debug!(path = %req.path.display(), "file watch: {e}");
                None
            }
        };
        if req.cancel.is_cancelled() || tx.is_closed() {
            return;
        }
        if let Some(change) = change {
            let _ = tx.send(Msg::FileChanged {
                tab: req.tab,
                change,
            });
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::*;

    /// A fake file: tests change what the next metadata read returns.
    #[derive(Clone, Default)]
    struct FakeFs {
        state: Arc<Mutex<Option<io::Result<FileStamp>>>>,
        reads: Arc<AtomicUsize>,
    }

    impl FakeFs {
        fn set(&self, r: io::Result<FileStamp>) {
            *self.state.lock().unwrap() = Some(r);
        }
    }

    impl MetadataSource for FakeFs {
        async fn stamp(&self, _path: &Path) -> io::Result<FileStamp> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            match &*self.state.lock().unwrap() {
                Some(Ok(s)) => Ok(*s),
                Some(Err(e)) => Err(io::Error::new(e.kind(), e.to_string())),
                None => Err(io::Error::other("unset")),
            }
        }
    }

    fn stamp(len: u64, secs: u64) -> FileStamp {
        FileStamp {
            len,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    struct Harness {
        fs: FakeFs,
        cancel: CancellationToken,
        rx: mpsc::UnboundedReceiver<Msg>,
        task: JoinHandle<()>,
    }

    fn start() -> Harness {
        let fs = FakeFs::default();
        fs.set(Ok(stamp(100, 10)));
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let req = WatchRequest {
            tab: TabId(7),
            path: PathBuf::from("data.csv"),
            initial: stamp(100, 10),
            cancel: cancel.clone(),
        };
        let task = spawn_with(fs.clone(), req, tx);
        Harness {
            fs,
            cancel,
            rx,
            task,
        }
    }

    /// Lets the paused clock run `d` and the task react.
    async fn advance(d: Duration) {
        tokio::time::sleep(d).await;
        tokio::task::yield_now().await;
    }

    fn changed(msg: Msg) -> (TabId, FileChange) {
        match msg {
            Msg::FileChanged { tab, change } => (tab, change),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn polls_every_two_seconds_and_reports_a_change_once() {
        let mut h = start();
        advance(Duration::from_millis(1900)).await;
        assert_eq!(h.fs.reads.load(Ordering::SeqCst), 0, "no read before 2 s");
        advance(Duration::from_millis(200)).await;
        assert_eq!(h.fs.reads.load(Ordering::SeqCst), 1);
        advance(Duration::from_secs(4)).await;
        assert_eq!(h.fs.reads.load(Ordering::SeqCst), 3);
        assert!(h.rx.try_recv().is_err(), "unchanged file: no message");

        // Appending changes the length.
        h.fs.set(Ok(stamp(150, 10)));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            changed(h.rx.try_recv().unwrap()),
            (TabId(7), FileChange::Modified)
        );
        // Sent once, then the watcher is done.
        advance(Duration::from_secs(10)).await;
        assert!(h.rx.try_recv().is_err());
        assert!(h.task.is_finished());
        assert_eq!(h.fs.reads.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn an_mtime_change_alone_counts() {
        let mut h = start();
        h.fs.set(Ok(stamp(100, 11)));
        advance(Duration::from_secs(2)).await;
        assert_eq!(changed(h.rx.try_recv().unwrap()).1, FileChange::Modified);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deleted_file_is_reported_as_deleted() {
        let mut h = start();
        h.fs.set(Err(io::Error::from(io::ErrorKind::NotFound)));
        advance(Duration::from_secs(2)).await;
        let (_, change) = changed(h.rx.try_recv().unwrap());
        assert_eq!(change, FileChange::Deleted);
        assert_eq!(change.warning_text(), DELETED_TEXT);
        assert_eq!(FileChange::Modified.warning_text(), CHANGED_TEXT);
    }

    #[tokio::test(start_paused = true)]
    async fn other_errors_are_retried() {
        let mut h = start();
        h.fs.set(Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        advance(Duration::from_secs(4)).await;
        assert!(h.rx.try_recv().is_err());
        assert!(!h.task.is_finished());
        h.fs.set(Ok(stamp(0, 10)));
        advance(Duration::from_secs(2)).await;
        assert_eq!(changed(h.rx.try_recv().unwrap()).1, FileChange::Modified);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_the_token_stops_the_watcher() {
        let mut h = start();
        advance(Duration::from_secs(2)).await;
        h.cancel.cancel();
        tokio::task::yield_now().await;
        h.fs.set(Ok(stamp(1, 1)));
        advance(Duration::from_secs(10)).await;
        assert!(h.rx.try_recv().is_err());
        assert!(h.task.is_finished());
        assert_eq!(h.fs.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn real_file_system_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.csv");
        std::fs::write(&path, "a,b\n").unwrap();
        let s = FsMetadata.stamp(&path).await.unwrap();
        assert_eq!(s, FileStamp::read(&path).unwrap());
        assert_eq!(s.len, 4);
        let missing = FsMetadata.stamp(&dir.path().join("nope")).await;
        assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
