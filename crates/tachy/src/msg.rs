//! Messages from background tasks to the UI task (spec §4.1, README §A2).

use std::sync::Arc;

use tachy_core::{index::IndexSummary, sample::SampleResult, source::SourceError};

use crate::{
    jobs::JobMsg,
    path_complete::Candidate,
    tab::{Opened, TabId},
    toast::{Toast, ToastLevel, open_error_text},
    watch::FileChange,
};

/// Messages from background tasks to the UI task. Not serde: may carry Arcs,
/// bitmaps, errors.
///
/// `App` owns the receiver and hands a clone of the sender to every task it
/// spawns. High-frequency progress is *not* sent here: it lives in shared
/// atomic counters that the UI reads when it renders.
///
/// Messages tied to a tab's index carry the tab's `generation`, a per-tab
/// counter bumped whenever indexing restarts (dialect change, reload). The UI
/// drops messages whose generation is stale, so a finished old run never
/// overwrites a newer one.
#[derive(Debug)]
pub enum Msg {
    /// Show a toast (M1-09).
    #[allow(dead_code)] // No background task raises a toast yet.
    Toast(Toast),
    /// The open step of tab `tab` finished (M1-08). `generation` is the
    /// tab's generation when the open started: a reload (`R`) bumps it, so
    /// an older open's result is dropped.
    SourceOpened {
        tab: TabId,
        generation: u64,
        result: Result<Box<Opened>, OpenFailure>,
    },
    /// stdin was copied into the tab's spool file (M1-08).
    SpoolDone {
        tab: TabId,
        result: Result<(), String>,
    },
    /// The indexer of `tab` finished (M2-02).
    IndexReady {
        tab: TabId,
        generation: u64,
        summary: IndexSummary,
    },
    /// The indexer of `tab` failed (M2-02). Cancellation is not a failure
    /// and sends nothing.
    IndexFailed {
        tab: TabId,
        generation: u64,
        error: String,
    },
    /// Directory listing for the `open ›` prompt's `Tab` (M1-08). `input` is
    /// the prompt text the listing was made for; a stale result is dropped.
    PathCompletions {
        input: String,
        result: Result<Vec<Candidate>, String>,
    },
    /// A type-inference sample of `tab` finished (M3-01): phase 1 (the
    /// first 10,000 rows) after open, phase 2 (plus 10,000 spread rows)
    /// after `IndexReady`.
    SampleReady {
        tab: TabId,
        generation: u64,
        sample: Arc<SampleResult>,
    },
    /// A job's task ended (M5-01).
    Job(JobMsg),
    /// Search request `request` on `tab` finished (M4-05). A cancelled
    /// search sends nothing; a stale id is dropped.
    SearchDone {
        tab: TabId,
        request: u64,
        result: Result<tachy_core::search::SearchOutcome, String>,
    },
    /// The file of `tab` changed or vanished on disk (M7-03). Sent once per
    /// watcher; the reload starts a new one. Shown as a sticky status-line
    /// warning until `R`.
    FileChanged { tab: TabId, change: FileChange },
    /// A save or delete of `views.json` finished (M6-04): the updated store
    /// (unchanged on error) and what happened.
    ViewsWritten {
        store: Box<crate::views_store::ViewsStore>,
        result: Result<crate::views_store::SaveReport, crate::views_store::ViewsError>,
        /// `saved view "DE"` / `deleted view "DE"`.
        done: String,
    },
}

/// Why opening a file failed.
#[derive(Debug)]
pub enum OpenFailure {
    /// `Source::open` failed: the toast text comes from `Toast::open_error`.
    Source(SourceError),
    /// Anything else (transcoding, a cancelled open), as toast text.
    Other(String),
}

impl OpenFailure {
    /// The toast text (`cannot open <path>: …`).
    pub fn text(&self) -> String {
        match self {
            OpenFailure::Source(e) => open_error_text(e),
            OpenFailure::Other(text) => text.clone(),
        }
    }

    pub fn toast(&self) -> Toast {
        match self {
            OpenFailure::Source(e) => Toast::open_error(e),
            OpenFailure::Other(text) => Toast::new(ToastLevel::Error, text.clone()),
        }
    }
}
