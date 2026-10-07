//! A tab: one opened file, its index, row cache, cursor and column layout
//! (spec §4.2, §11.3, M1-05, M1-06, M1-08).
//!
//! A tab appears as soon as a file is requested, as a placeholder
//! ([`Phase::Opening`] or [`Phase::Spooling`]); the body shows `opening…`
//! until the `Source` arrives ([`Tab::loaded`]). Opening, indexing and
//! spooling run off the UI task; this module only holds their results and the
//! navigation logic, which is pure and unit-tested.

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    },
};

use tachy_core::{
    cache::{ParsedRow, RowCache},
    column::ColumnMeta,
    dialect::{self, ColumnName, Dialect, DialectOverrides, Encoding, SniffReport},
    index::{IndexSummary, RowIndex},
    parse::{RecordParser, decode_field, display_segments, extra_column_name},
    query::Predicate,
    sample::SampleResult,
    source::Source,
    text,
    types::{ColType, NullSet},
    view::View,
};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::{
    jobs::{JobId, ViewKey},
    rate::RateWindow,
    watch::{FileChange, FileStamp},
};

/// Identifies a tab for the lifetime of the app. Monotonically increasing,
/// never reused, so a late message for a closed tab can't hit a new one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

/// A tab's cursor and scroll position in one view (M4-03). Saved in each
/// [`ViewEntry`] while another view is active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorState {
    pub cursor_row: u64,
    pub cursor_col: usize,
    pub top_row: u64,
    pub col_offset: usize,
}

/// One view of a tab's [`ViewStack`].
#[derive(Debug, Clone)]
pub struct ViewEntry {
    pub view: View,
    /// The cursor saved when another entry became active.
    pub cursor: CursorState,
    /// The job producing this view (a running filter), cancelled when the
    /// view is popped.
    pub job: Option<JobId>,
    /// A filter view's compiled predicate, for match highlighting (§12.4).
    pub pred: Option<Predicate>,
}

/// The per-tab view stack (§8.1, M4-03, D1).
///
/// - Entry 0 is always [`View::All`].
/// - Applying a filter or a sort **pushes** an entry and makes it active
///   ([`Tab::push_view`]). It always goes on top, also while `Enter` shows
///   `All` over a derived view; the caller chooses the parent the job reads
///   (normally the visible view).
/// - `x` pops the top entry (never `All`), restoring the parent's saved
///   cursor. When `active != top` (after `Enter` jumped to the source row),
///   `x` first returns to the top entry and does **not** pop.
/// - `Enter` in a derived view shows `All` with the cursor on the same
///   record; `Enter` again returns to the derived view at its saved cursor.
///
/// The tab's live cursor fields always belong to the active entry; an
/// entry's `cursor` is only meaningful while it is not active.
#[derive(Debug, Clone)]
pub struct ViewStack {
    entries: Vec<ViewEntry>,
    active: usize,
}

impl Default for ViewStack {
    fn default() -> Self {
        ViewStack {
            entries: vec![ViewEntry {
                view: View::All,
                cursor: CursorState::default(),
                job: None,
                pred: None,
            }],
            active: 0,
        }
    }
}

impl ViewStack {
    /// The view the table shows.
    pub fn active_view(&self) -> &View {
        &self.entries[self.active].view
    }

    /// The top entry's view.
    pub fn top_view(&self) -> &View {
        &self.entries[self.top()].view
    }

    pub fn active(&self) -> ViewKey {
        self.active
    }

    pub fn top(&self) -> ViewKey {
        self.entries.len() - 1
    }

    /// Number of entries (at least 1).
    pub fn depth(&self) -> usize {
        self.entries.len()
    }

    pub fn entries(&self) -> &[ViewEntry] {
        &self.entries
    }

    /// `Enter` moved to the source row: `All` is shown over a derived view.
    pub fn is_toggled(&self) -> bool {
        self.active != self.top()
    }
}

/// What [`Tab::pop_view`] did.
#[derive(Debug)]
pub enum PopOutcome {
    /// Only `All` is left: nothing to pop.
    Nothing,
    /// `All` was shown over a derived view: back to the top entry.
    Returned,
    /// The top entry was dropped. The caller cancels its job; the view's
    /// temp files go when the last reference to it does.
    Popped(ViewEntry),
}

/// Where the cursor goes after a reload (M7-03): the same record (by row
/// id) and the same column (by name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    pub row_id: u64,
    pub column: Option<String>,
}

/// Column order, visibility and widths (§11.3, M3-04). Indexed by source
/// column (`ColumnMeta` position), except `order`, which lists source
/// columns in display order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnLayout {
    pub order: Vec<usize>,
    pub visible: Vec<bool>,
    pub widths: Vec<u16>,
    /// Set by `<` / `>` (M3-04): automatic re-measuring leaves it alone.
    pub manual: Vec<bool>,
    /// Number of leading visible columns that don't scroll (`:freeze`).
    pub freeze: usize,
}

impl ColumnLayout {
    fn push(&mut self, width: u16) {
        self.order.push(self.widths.len());
        self.visible.push(true);
        self.widths.push(width);
        self.manual.push(false);
    }

    /// Visible source columns in display order. `cursor_col` and
    /// `col_offset` index into this list.
    pub fn display(&self) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|&c| self.visible.get(c).copied().unwrap_or(false))
            .collect()
    }
}

/// Where a pending jump goes (M2-03, D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpTarget {
    /// `G` before the index is complete.
    LastRow,
    /// A 0-based view position past the indexed range (`g 123456789`).
    Row(u64),
    /// `g 50%` while indexing: the first record starting at or after this
    /// byte offset, resolved once the published checkpoints pass it.
    ByteOffset(u64),
}

/// A jump waiting for the indexer (D9): a state inside Normal mode, shown
/// as a spinner on the status line and cancelled by `Esc` or any movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingJump {
    pub target: JumpTarget,
    pub started: std::time::Instant,
    /// Spinner frame: advances once per 100 ms tick.
    pub frame: usize,
}

/// What [`Tab::poll_jump`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpOutcome {
    /// Still waiting for the index.
    Waiting,
    /// The cursor moved to the target.
    Jumped,
    /// The target was past the end: the cursor went to the last row of a
    /// view of `len` rows (`only 1,234 rows — went to the last row`).
    Clamped { len: u64 },
}

/// Smallest width `<` shrinks a column to: one character, `…` and a spare
/// cell (M3-04).
pub const MIN_COL_WIDTH: u16 = 3;
/// The frozen block leaves at least this many cells to the scrolling
/// columns (M3-04).
const FREEZE_RESERVE: u16 = 10;
/// Bytes of one raw line shown in raw mode (M2-04).
const RAW_LINE_BYTES: usize = 4096;

/// Progress of the open step (`open_blocking`), shared with the opener.
#[derive(Debug, Default)]
pub struct OpenProgress {
    /// The file is UTF-16 and is being transcoded (M1-03).
    pub transcoding: AtomicBool,
    /// Input bytes transcoded so far.
    pub bytes: AtomicU64,
    /// Size of the UTF-16 file.
    pub total: AtomicU64,
}

/// Where a tab is in its life.
#[derive(Debug, Clone)]
pub enum Phase {
    /// Copying stdin into the temp file (`reading stdin`, M1-08).
    Spooling { bytes: Arc<AtomicU64> },
    /// Opening and sniffing (and transcoding UTF-16) off the UI task.
    Opening { progress: Arc<OpenProgress> },
    /// `Tab::loaded` is set.
    Ready,
}

/// Body height and table width, in cells: what navigation needs to know
/// about the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    pub body_height: u16,
    pub table_width: u16,
}

/// The rows prepared for the next frame ([`Tab::prepare_frame`]).
#[derive(Debug, Clone, Default)]
pub struct FrameRows {
    /// View position of `rows[0]`.
    pub first: u64,
    /// Row id of each prepared position (the gutter's source row in
    /// derived views).
    pub ids: Vec<u64>,
    pub rows: Vec<Option<Arc<ParsedRow>>>,
}

/// Everything that exists once the file is open.
pub struct Loaded {
    pub source: Arc<Source>,
    /// Kept for the Detected format dialog (M2-04) and re-indexing.
    pub sniff: SniffReport,
    pub index: Arc<RowIndex>,
    /// Cancels the running indexer only (a child of the tab's token).
    pub index_cancel: CancellationToken,
    /// Set by `Msg::IndexReady` for the current generation.
    pub index_summary: Option<IndexSummary>,
    /// Set by `Msg::IndexFailed` for the current generation.
    pub index_error: Option<String>,
    /// Throughput samples of the indexer (status line gauge).
    pub rate: RateWindow,
    pub columns: Vec<ColumnMeta>,
    pub cache: RowCache,
    /// The UTF-16 encoding of a transcoded file, for the status line
    /// (`utf-16le→utf-8`).
    pub original_encoding: Option<Encoding>,
    /// The UTF-8 copy of a UTF-16 file; held only to delete it on drop.
    #[allow(dead_code)]
    pub transcoded: Option<NamedTempFile>,
    pub frame: FrameRows,
    /// One past the last row the cache has parsed: rows the user can reach
    /// before the indexer publishes them (speculative parsing).
    pub known_rows: u64,
    /// Ragged rows the cache has seen (`≥ N ragged` while indexing).
    pub ragged_seen: u64,
    /// The latest type-inference sample (M3-01): phase 1, then phase 2.
    pub sample: Option<Arc<SampleResult>>,
    /// Cancels the running sample task (a child of the tab's token).
    pub sample_cancel: CancellationToken,
    /// The phase-2 sample was spawned for this generation.
    pub spread_started: bool,
    /// Raw mode (M2-04): the physical lines of the window, from `top_row`.
    pub raw_lines: Vec<String>,
    widths_measured: bool,
}

/// The result of a successful open, sent back to the UI task.
pub struct Opened {
    pub source: Source,
    pub report: SniffReport,
    pub transcoded: Option<NamedTempFile>,
    pub original_encoding: Option<Encoding>,
    /// Length and mtime of the file as given (the original of a UTF-16
    /// transcode), captured at open, for the change watcher (M7-03).
    pub stamp: Option<FileStamp>,
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("source", &self.source.path())
            .field("transcoded", &self.transcoded.is_some())
            .finish_non_exhaustive()
    }
}

/// One tab. See the module docs.
pub struct Tab {
    pub id: TabId,
    /// The tab title: the file name, or `stdin`.
    pub name: String,
    /// The path as given (the spool file for stdin).
    pub path: PathBuf,
    pub phase: Phase,
    /// Parent of every job and indexer token of this tab (README §A2).
    /// Cancelled when the tab is dropped.
    pub cancel: CancellationToken,
    /// Bumped whenever indexing restarts (dialect change, reload); stale
    /// `Msg::IndexReady` / `IndexFailed` are dropped.
    pub generation: u64,
    pub loaded: Option<Loaded>,
    /// The stdin spool file; deleted on drop.
    pub temp: Option<NamedTempFile>,
    /// Throughput of the stdin copy.
    pub spool_rate: RateWindow,

    /// View position of the cursor.
    pub cursor_row: u64,
    /// Index into `layout.display()`.
    pub cursor_col: usize,
    /// View position of the first body row.
    pub top_row: u64,
    /// First scrolling column shown, as an index into the scrolling part of
    /// `layout.display()` (after the frozen block).
    pub col_offset: usize,
    pub layout: ColumnLayout,
    /// Filters and sorts (M4-03).
    pub views: ViewStack,
    /// The file changed on disk (M7-03): a sticky status-line warning until
    /// `R`.
    pub file_changed: Option<FileChange>,
    /// Cursor to restore once a reload's open finishes (M7-03).
    pub restore: Option<Restore>,
    /// The Detected format dialog was shown for this tab (M2-04).
    pub dialog_shown: bool,
    /// A jump waiting for the indexer (M2-03).
    pub pending_jump: Option<PendingJump>,
    /// Show the file's physical lines instead of parsed records (M2-04 `r`).
    pub raw_mode: bool,
    /// Null spellings (`settings.null_values`) for samples and stats.
    pub nulls: NullSet,
    /// `(requested, shown)` of the last `freeze N (M shown)` toast, so it is
    /// shown once per change (M3-04).
    pub freeze_notice: Option<(usize, usize)>,
    max_column_width: u16,
}

impl fmt::Debug for Tab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tab")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("phase", &self.phase)
            .field("loaded", &self.loaded.is_some())
            .field("cursor", &(self.cursor_row, self.cursor_col))
            .field("top_row", &self.top_row)
            .field("col_offset", &self.col_offset)
            .finish_non_exhaustive()
    }
}

impl Drop for Tab {
    /// Closing a tab cancels its indexer and jobs (§4.3). Temp files go with
    /// the `NamedTempFile`s.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Horizontal placement of one column in the table area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColSlot {
    /// Index into `layout.display()`.
    pub display_idx: usize,
    /// Source column.
    pub col: usize,
    /// Offset from the left edge of the table area.
    pub x: u16,
    /// Cells drawn (less than the column width when cut).
    pub width: u16,
    pub truncated: bool,
}

/// Where everything goes horizontally (§11.3 "Horizontal layout").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HLayout {
    /// Cells for the row number.
    pub gutter_digits: u16,
    /// Number, space, `│`, space.
    pub gutter_width: u16,
    pub frozen: Vec<ColSlot>,
    /// `x` of the `│` after the frozen block.
    pub frozen_divider: Option<u16>,
    pub scrolling: Vec<ColSlot>,
    /// Visible columns right of the last drawn one (`→ N more cols`).
    pub more_cols: usize,
}

/// A scrolling column is drawn cut only if at least this many cells are
/// left for it (§11.3).
const MIN_PARTIAL: u16 = 4;
/// Gutter digits in Filtered / Sorted views, so `source row` fits.
const SOURCE_ROW_DIGITS: u16 = 10;

impl Tab {
    /// A placeholder tab (`opening…`).
    pub fn new(
        id: TabId,
        name: String,
        path: PathBuf,
        phase: Phase,
        freeze: usize,
        max_column_width: u16,
    ) -> Self {
        Tab {
            id,
            name,
            path,
            phase,
            cancel: CancellationToken::new(),
            generation: 0,
            loaded: None,
            temp: None,
            spool_rate: RateWindow::default(),
            cursor_row: 0,
            cursor_col: 0,
            top_row: 0,
            col_offset: 0,
            layout: ColumnLayout {
                freeze,
                ..ColumnLayout::default()
            },
            views: ViewStack::default(),
            file_changed: None,
            restore: None,
            dialog_shown: false,
            pending_jump: None,
            raw_mode: false,
            nulls: NullSet::default(),
            freeze_notice: None,
            max_column_width: max_column_width.max(1),
        }
    }

    /// Installs the opened source with a fresh index and cache. The caller
    /// spawns the indexer.
    pub fn set_loaded(&mut self, opened: Opened) {
        let source = Arc::new(opened.source);
        let index = Arc::new(RowIndex::for_source(&source));
        let columns = columns_for(&source);
        self.name = source.display_name().to_owned();
        self.phase = Phase::Ready;
        self.loaded = Some(Loaded {
            source,
            sniff: opened.report,
            index,
            index_cancel: self.cancel.child_token(),
            index_summary: None,
            index_error: None,
            rate: RateWindow::default(),
            columns: Vec::new(),
            cache: RowCache::new(),
            original_encoding: opened.original_encoding,
            transcoded: opened.transcoded,
            frame: FrameRows::default(),
            known_rows: 0,
            ragged_seen: 0,
            sample: None,
            sample_cancel: self.cancel.child_token(),
            spread_started: false,
            raw_lines: Vec::new(),
            widths_measured: false,
        });
        self.reset_columns(columns);
    }

    /// Switches to another dialect (M2-04 "Applying a dialect change",
    /// M7-03 reload). The caller spawns the indexer and the sample again.
    ///
    /// 1. Cancels the running indexer and sample.
    /// 2. Builds a new `Source` over the same mapping (new `data_start` and
    ///    header names). If the quote char or escape changed, sniff step 5
    ///    (quoted newlines) runs again with the new settings, to choose the
    ///    indexer's fast or slow path.
    /// 3. Empties the row cache (§6.3).
    /// 4. Rebuilds the column metadata (types and stats cleared) and the
    ///    column layout (order, visibility, widths).
    /// 5. Bumps the generation and creates a fresh `RowIndex`.
    /// 6. Drops every view but `All` (filters and sorts are invalid under a
    ///    new dialect), deleting their temp files once their jobs end. The
    ///    caller cancels the tab's jobs.
    /// 7. Puts the cursor on row 0, keeping the column index (clamped).
    /// 8. Cancels a pending jump. The caller (`App::restart_indexing`)
    ///    cancels a search in flight on the tab and clears its highlight
    ///    (M4-05).
    pub fn apply_dialect(&mut self, dialect: Dialect, sample_bytes: usize) {
        let Some(l) = self.loaded.as_mut() else {
            return;
        };
        l.index_cancel.cancel();
        l.sample_cancel.cancel();
        let old = *l.source.dialect();
        let source = Arc::new(l.source.with_dialect(dialect));
        if old.quote != dialect.quote || old.escape != dialect.escape {
            l.sniff.quoted_newlines = quoted_newlines(&source, sample_bytes);
        }
        l.index = Arc::new(RowIndex::for_source(&source));
        l.source = source;
        l.sniff.dialect = dialect;
        l.index_cancel = self.cancel.child_token();
        l.sample_cancel = self.cancel.child_token();
        l.sample = None;
        l.spread_started = false;
        l.index_summary = None;
        l.index_error = None;
        l.rate.clear();
        l.cache.invalidate_all();
        l.frame = FrameRows::default();
        l.raw_lines.clear();
        l.known_rows = 0;
        l.ragged_seen = 0;
        l.widths_measured = false;
        let columns = columns_for(&l.source);
        self.generation += 1;
        let cursor_col = self.cursor_col;
        self.reset_columns(columns);
        self.cursor_col = cursor_col.min(self.visible_cols().saturating_sub(1));
        self.views = ViewStack::default();
        self.pending_jump = None;
        self.cursor_row = 0;
        self.top_row = 0;
    }

    fn reset_columns(&mut self, columns: Vec<ColumnMeta>) {
        let freeze = self.layout.freeze;
        self.layout = ColumnLayout {
            freeze,
            ..ColumnLayout::default()
        };
        for c in &columns {
            self.layout.push(header_cells(c));
        }
        if let Some(l) = self.loaded.as_mut() {
            l.columns = columns;
        }
        self.cursor_col = 0;
        self.col_offset = 0;
    }

    /// Whether the indexer is still running (and hasn't failed).
    pub fn indexing(&self) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|l| !l.index.is_complete() && l.index_error.is_none())
    }

    /// Something on screen changes without input: opening, spooling,
    /// indexing or a pending jump's spinner (the tick must run, §4.1).
    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Ready) || self.indexing() || self.pending_jump.is_some()
    }

    /// Rows in the current view: exact once indexed, otherwise the lower
    /// bound `max(indexed rows, rows the cache already parsed)` (§5.2).
    /// Read live from the index on every call, so navigation always sees the
    /// latest count.
    pub fn view_len(&self) -> u64 {
        let Some(l) = &self.loaded else {
            return 0;
        };
        match self.views.active_view() {
            View::All => match l.index.total_rows() {
                Some(total) => total,
                None => l.index.indexed_rows().max(l.known_rows),
            },
            view => view.len(&l.index),
        }
    }

    /// Whether [`Tab::view_len`] is exact: the index is complete (`All`) or
    /// the view's job has finished.
    pub fn view_len_exact(&self) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|l| match self.views.active_view() {
                View::All => l.index.total_rows().is_some(),
                view => !view.is_growing(&l.index),
            })
    }

    /// The row id at view position `pos` of the active view, if any.
    pub fn row_id(&self, pos: u64) -> Option<u64> {
        self.views.active_view().row_id_at(pos)
    }

    /// `view: <label>` in the top bar (§8.1): the active view's label, plus
    /// `↩ <top label>` while `Enter` shows `All` over a derived view (D1).
    pub fn view_label(&self) -> String {
        let active = self.views.active_view().label();
        if self.views.is_toggled() {
            format!("{active} ↩ {}", self.views.top_view().label())
        } else {
            active.to_owned()
        }
    }

    // ---- view stack (M4-03, D1) ----------------------------------------

    /// The live cursor, for saving in a [`ViewEntry`].
    pub fn cursor_state(&self) -> CursorState {
        CursorState {
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            top_row: self.top_row,
            col_offset: self.col_offset,
        }
    }

    fn set_cursor_state(&mut self, c: CursorState) {
        self.cursor_row = c.cursor_row;
        self.cursor_col = c.cursor_col;
        self.top_row = c.top_row;
        self.col_offset = c.col_offset;
    }

    /// Pushes a filter or sort view and makes it active (§8.1): the current
    /// cursor is saved in the parent entry, the new view starts at row 0 in
    /// the same column. Cancels a pending jump. Returns the new entry's key.
    pub fn push_view(&mut self, view: View, job: Option<JobId>) -> ViewKey {
        let saved = self.cursor_state();
        let vs = &mut self.views;
        vs.entries[vs.active].cursor = saved;
        vs.entries.push(ViewEntry {
            view,
            cursor: CursorState::default(),
            job,
            pred: None,
        });
        vs.active = vs.top();
        self.cursor_row = 0;
        self.top_row = 0;
        self.pending_jump = None;
        vs.top()
    }

    /// Links view `key` to the job producing it and the filter predicate
    /// used for highlighting (M4-04).
    pub fn set_view_job(&mut self, key: ViewKey, job: Option<JobId>, pred: Option<Predicate>) {
        if let Some(entry) = self.views.entries.get_mut(key) {
            entry.job = job;
            entry.pred = pred;
        }
    }

    /// The active view's entry.
    pub fn active_entry(&self) -> &ViewEntry {
        &self.views.entries[self.views.active]
    }

    /// `x` (§13): see [`ViewStack`] and [`PopOutcome`].
    pub fn pop_view(&mut self, viewport: Viewport) -> PopOutcome {
        self.pending_jump = None;
        let top = self.views.top();
        if self.views.is_toggled() {
            self.views.active = top;
            self.set_cursor_state(self.views.entries[top].cursor);
            self.clamp(viewport);
            return PopOutcome::Returned;
        }
        if top == 0 {
            return PopOutcome::Nothing;
        }
        let Some(entry) = self.views.entries.pop() else {
            return PopOutcome::Nothing;
        };
        let top = self.views.top();
        self.views.active = top;
        self.set_cursor_state(self.views.entries[top].cursor);
        self.clamp(viewport);
        PopOutcome::Popped(entry)
    }

    /// Drops the entries from `key` up (never `All`), e.g. when a filter's
    /// job is killed (M5-02). The new top becomes active with its saved
    /// cursor. Returns the dropped entries, so the caller can cancel their
    /// jobs.
    pub fn truncate_views(&mut self, key: ViewKey, viewport: Viewport) -> Vec<ViewEntry> {
        let key = key.max(1);
        if key >= self.views.depth() {
            return Vec::new();
        }
        self.pending_jump = None;
        let dropped = self.views.entries.split_off(key);
        // `All` shown over a dropped view (`Enter`) stays where it is.
        if self.views.active >= key {
            let top = self.views.top();
            self.views.active = top;
            self.set_cursor_state(self.views.entries[top].cursor);
        }
        self.clamp(viewport);
        dropped
    }

    /// `Enter` (§13, D1). In a derived view: shows `All` with the cursor on
    /// the current record (a pending jump when the index hasn't reached it
    /// yet, M2-03). Again: back to the derived view at its saved cursor. In
    /// `All` with nothing stacked it does nothing. Returns whether something
    /// changed.
    pub fn jump_to_source(&mut self, viewport: Viewport) -> bool {
        let top = self.views.top();
        if top == 0 {
            return false;
        }
        if self.views.is_toggled() {
            self.views.active = top;
            self.pending_jump = None;
            self.set_cursor_state(self.views.entries[top].cursor);
            self.clamp(viewport);
            return true;
        }
        let Some(row_id) = self.row_id(self.cursor_row) else {
            return false;
        };
        self.views.entries[top].cursor = self.cursor_state();
        self.views.active = 0;
        self.pending_jump = None;
        if row_id < self.view_len() {
            self.cursor_row = row_id;
            self.clamp(viewport);
            self.center_cursor(viewport);
        } else {
            self.start_jump(JumpTarget::Row(row_id), viewport);
        }
        true
    }

    /// Number of visible columns.
    pub fn visible_cols(&self) -> usize {
        self.layout.display().len()
    }

    /// The source column under the cursor.
    pub fn cursor_source_col(&self) -> Option<usize> {
        self.layout.display().get(self.cursor_col).copied()
    }

    /// The parsed row under the cursor, if it is on the prepared screen.
    pub fn cursor_row_data(&self) -> Option<&Arc<ParsedRow>> {
        let l = self.loaded.as_ref()?;
        let i = self.cursor_row.checked_sub(l.frame.first)?;
        l.frame.rows.get(usize::try_from(i).ok()?)?.as_ref()
    }

    /// The parsed record under the cursor, through the cache (`y` / `Y`,
    /// M6-05): it may not be on the last prepared screen when keys were
    /// coalesced. `None` in an empty view.
    pub fn cursor_record(&mut self) -> Option<Arc<ParsedRow>> {
        if self.view_len() == 0 {
            return None;
        }
        let id = self.row_id(self.cursor_row)?;
        let l = self.loaded.as_mut()?;
        let window = l.cache.get_window(&[id], &l.source, &l.index);
        window.rows.into_iter().next().flatten()
    }

    // ---- reload (M7-03) ------------------------------------------------

    /// The overrides that force the current dialect on a re-open: every
    /// sniff step is skipped, so `R` keeps the user's adjustments (no
    /// re-sniff, no dialog). A transcoded file forces its original UTF-16
    /// encoding, so it is transcoded again. `None` before the file is open.
    pub fn reload_overrides(&self) -> Option<DialectOverrides> {
        let l = self.loaded.as_ref()?;
        let d = *l.source.dialect();
        Some(DialectOverrides {
            delimiter: Some(d.delimiter),
            quote: Some(d.quote),
            escape: Some(d.escape),
            header: Some(d.header),
            encoding: Some(l.original_encoding.unwrap_or(d.encoding)),
            comment: d.comment,
        })
    }

    /// The first half of `R` (M7-03 steps 1–2): cancels the tab's work (the
    /// old token) and gives it a new token, remembers the cursor's row id
    /// and column name, drops every view but `All` and the loaded file
    /// (its mapping and temp files), bumps the generation and shows
    /// `opening…`. Returns the dropped views (for their jobs) and the new
    /// open progress. The caller re-opens the file with
    /// [`Tab::reload_overrides`] (taken before this call).
    pub fn begin_reload(&mut self) -> (Vec<ViewEntry>, Arc<OpenProgress>) {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        let row_id = self.row_id(self.cursor_row).unwrap_or(self.cursor_row);
        let column = self.cursor_source_col().and_then(|c| {
            self.loaded
                .as_ref()
                .and_then(|l| l.columns.get(c))
                .map(|m| m.name.display.clone())
        });
        if self.loaded.is_some() {
            self.restore = Some(Restore { row_id, column });
        }
        let mut dropped = self.views.entries.split_off(1);
        dropped.retain(|e| e.job.is_some());
        self.views = ViewStack::default();
        self.loaded = None;
        self.generation += 1;
        self.file_changed = None;
        self.pending_jump = None;
        self.raw_mode = false;
        self.cursor_row = 0;
        self.top_row = 0;
        let progress = Arc::new(OpenProgress::default());
        self.phase = Phase::Opening {
            progress: Arc::clone(&progress),
        };
        (dropped, progress)
    }

    /// The last step of `R`: once the file is open again, puts the cursor
    /// on the same row id (a pending jump while the index hasn't reached it;
    /// clamped if the file got shorter) and the same column by name.
    pub fn apply_restore(&mut self, viewport: Viewport) {
        let Some(r) = self.restore.take() else {
            return;
        };
        if let Some(name) = r.column
            && let Some(col) = self
                .loaded
                .as_ref()
                .and_then(|l| l.columns.iter().position(|c| c.name.display == name))
        {
            let _ = self.goto_column(col, viewport);
        }
        if r.row_id > 0 {
            self.start_jump(JumpTarget::Row(r.row_id), viewport);
        }
    }

    // ---- frame preparation (M1-05 "Data flow") -------------------------

    /// Parses the rows of the visible window (at most one screen) so that
    /// `draw` never parses. Also appends `_extraN` columns for rows with
    /// more fields than the header (§6.4), and measures the column widths
    /// on the first screen until the sample arrives (M3-04).
    pub fn prepare_frame(&mut self, viewport: Viewport) {
        self.clamp(viewport);
        let first = self.top_row;
        let all_view = self.views.active_view().is_all();
        let ids = self
            .views
            .active_view()
            .row_ids(first, usize::from(viewport.body_height));
        let raw_mode = self.raw_mode;
        let Some(l) = self.loaded.as_mut() else {
            return;
        };
        if raw_mode {
            l.raw_lines = raw_lines(&l.source, first, usize::from(viewport.body_height));
        }
        let window = l.cache.get_window(&ids, &l.source, &l.index);
        if all_view && let Some(last) = window.rows.iter().rposition(Option::is_some) {
            l.known_rows = l.known_rows.max(first + last as u64 + 1);
        }
        l.ragged_seen = window.ragged_seen;
        l.frame = FrameRows {
            first,
            ids,
            rows: window.rows,
        };
        // `_extra1`, `_extra2`, … for long ragged rows.
        let base = l.columns.len();
        let mut added = Vec::new();
        for i in base..window.max_fields_seen {
            let n = i - l.source.width() + 1;
            let name = extra_column_name(n);
            l.columns.push(ColumnMeta::new(
                ColumnName {
                    display: name.clone(),
                    query: name,
                },
                i,
                true,
            ));
            added.push(i);
        }
        if let Some(sample) = &l.sample {
            sample.apply(&mut l.columns[base..], &self.nulls);
        }
        let measure_all = !l.widths_measured && l.frame.rows.iter().any(Option::is_some);
        if measure_all {
            l.widths_measured = true;
        }
        for _ in &added {
            self.layout.push(1);
        }
        let to_measure: Vec<usize> = if measure_all {
            (0..self.layout.widths.len()).collect()
        } else {
            added
        };
        for col in to_measure {
            self.remeasure(col);
        }
        self.clamp(viewport);
    }

    /// Sets the automatic width of `col` unless the user sized it by hand.
    fn remeasure(&mut self, col: usize) {
        if !self.layout.manual.get(col).copied().unwrap_or(true) {
            self.layout.widths[col] = self.auto_width(col);
        }
    }

    /// Automatic width of `col` (§11.3, M3-04): the 95th percentile of the
    /// sampled value widths (before the sample arrives: the widest value on
    /// the prepared screen), clamped to `[header width, max_column_width]`.
    /// The header width wins over the maximum, so the 2-line header is never
    /// cut by an automatic width.
    pub fn auto_width(&self, col: usize) -> u16 {
        let Some(l) = &self.loaded else {
            return 1;
        };
        let Some(meta) = l.columns.get(col) else {
            return 1;
        };
        let content = match meta.sample_widths.p95() {
            Some(w) => w,
            None => self.screen_width(col),
        };
        auto_clamp(content, header_cells(meta), self.max_column_width)
    }

    /// The widest value of `col` among the prepared rows.
    fn screen_width(&self, col: usize) -> usize {
        let Some(l) = &self.loaded else {
            return 0;
        };
        l.frame
            .rows
            .iter()
            .flatten()
            .map(|row| cell_width(row.display(&l.source, col)))
            .max()
            .unwrap_or(0)
    }

    /// Installs a sample result for the current generation (M3-01): types,
    /// stats and width histograms; then re-measures every column the user
    /// didn't size by hand (M3-04). Phase 2 never overwrites a `set type`
    /// override or a manual width.
    pub fn apply_sample(&mut self, sample: Arc<SampleResult>) {
        let Some(l) = self.loaded.as_mut() else {
            return;
        };
        sample.apply(&mut l.columns, &self.nulls);
        l.sample = Some(sample);
        l.widths_measured = true;
        for col in 0..self.layout.widths.len() {
            self.remeasure(col);
        }
    }

    /// `set type <col> <type>` (M3-01; the palette command is M6-02):
    /// sets the override of source column `col` (`None` clears it),
    /// recomputes its stats from the cached sample without touching the file
    /// (M3-02), and its automatic width (the type label may be wider).
    /// Without a sample yet, the old stats stay and show as stale.
    /// `App::set_column_type` also cancels the tab's running Profile jobs
    /// (M5-04): their stats would be for the old type.
    pub fn set_column_type(&mut self, col: usize, ty: Option<ColType>) -> bool {
        let Some(l) = self.loaded.as_mut() else {
            return false;
        };
        let Some(meta) = l.columns.get_mut(col) else {
            return false;
        };
        meta.type_override = ty;
        if let Some(sample) = &l.sample
            && let Some(stats) = sample.stats_for(meta.source_index, meta.ty(), &self.nulls)
        {
            meta.stats = Some(stats);
        }
        self.remeasure(col);
        true
    }

    // ---- horizontal layout ---------------------------------------------

    /// Digits of the gutter: the largest row number on screen or in the
    /// view, rounded up to a multiple of 3 so it doesn't jitter while the
    /// count grows (§11.3).
    pub fn gutter_digits(&self, body_height: u16) -> u16 {
        let largest = self
            .view_len()
            .max(self.top_row + u64::from(body_height))
            .max(1);
        let digits = largest.to_string().len() as u16;
        let rounded = digits.div_ceil(3) * 3;
        if self.views.active_view().is_all() {
            rounded
        } else {
            rounded.max(SOURCE_ROW_DIGITS)
        }
    }

    /// Cells of the frozen block made of the first `k` columns of `display`:
    /// widths, the spaces between them and the ` │ ` divider.
    fn frozen_block_width(&self, display: &[usize], k: usize) -> u16 {
        if k == 0 {
            return 0;
        }
        let widths: u32 = display[..k]
            .iter()
            .map(|&c| u32::from(self.layout.widths[c]))
            .sum();
        u16::try_from(widths + k as u32 - 1 + 3).unwrap_or(u16::MAX)
    }

    /// Frozen columns actually drawn (M3-04): the first `freeze` visible
    /// columns, fewer when that block would leave less than 10 cells to the
    /// scrolling columns. `layout.freeze` keeps the requested number.
    pub fn frozen_count(&self, viewport: Viewport) -> usize {
        let display = self.layout.display();
        let mut k = self.layout.freeze.min(display.len());
        if k == display.len() {
            // Nothing scrolls: there is no scrolling column to keep visible.
            return k;
        }
        let gutter = self.gutter_digits(viewport.body_height) + 3;
        let avail = viewport
            .table_width
            .saturating_sub(gutter)
            .saturating_sub(FREEZE_RESERVE);
        while k > 0 && self.frozen_block_width(&display, k) > avail {
            k -= 1;
        }
        k
    }

    /// Places the gutter, the frozen block and the scrolling columns from
    /// `col_offset` on, in `viewport.table_width` cells. The single
    /// placement function: drawing and navigation both use it.
    pub fn h_layout(&self, viewport: Viewport) -> HLayout {
        let digits = self.gutter_digits(viewport.body_height);
        let gutter_width = digits + 3;
        let total = viewport.table_width;
        let display = self.layout.display();
        let n_frozen = self.frozen_count(viewport);
        let mut out = HLayout {
            gutter_digits: digits,
            gutter_width,
            ..HLayout::default()
        };
        let mut x = gutter_width;

        for (i, &col) in display.iter().enumerate().take(n_frozen) {
            let sep = u16::from(i > 0);
            if x + sep >= total {
                break;
            }
            let w = self.layout.widths[col].min(total - x - sep);
            out.frozen.push(ColSlot {
                display_idx: i,
                col,
                x: x + sep,
                width: w,
                truncated: w < self.layout.widths[col],
            });
            x += sep + w;
        }
        if !out.frozen.is_empty() && x + 3 <= total {
            out.frozen_divider = Some(x + 1);
            x += 3;
        } else if !out.frozen.is_empty() {
            x = total;
        }

        let scrolling = &display[n_frozen..];
        let start = self.col_offset.min(scrolling.len());
        let mut drawn = 0;
        for (k, &col) in scrolling.iter().enumerate().skip(start) {
            let sep = u16::from(drawn > 0);
            let w = self.layout.widths[col];
            let left = total.saturating_sub(x + sep);
            if w <= left {
                out.scrolling.push(ColSlot {
                    display_idx: n_frozen + k,
                    col,
                    x: x + sep,
                    width: w,
                    truncated: false,
                });
                x += sep + w;
                drawn += 1;
            } else {
                if left >= MIN_PARTIAL.min(w) && left > 0 {
                    out.scrolling.push(ColSlot {
                        display_idx: n_frozen + k,
                        col,
                        x: x + sep,
                        width: left,
                        truncated: true,
                    });
                    drawn += 1;
                }
                break;
            }
        }
        out.more_cols = scrolling.len() - start - drawn;
        out
    }

    // ---- navigation (M1-06) --------------------------------------------

    /// Re-clamps the cursor into the view and scrolls minimally so the
    /// cursor cell is visible (after a move, a resize or new columns).
    pub fn clamp(&mut self, viewport: Viewport) {
        let len = self.view_len();
        self.cursor_row = self.cursor_row.min(len.saturating_sub(1));
        let n = self.visible_cols();
        self.cursor_col = self.cursor_col.min(n.saturating_sub(1));
        self.ensure_row_visible(viewport.body_height);
        self.ensure_col_visible(viewport);
    }

    fn ensure_row_visible(&mut self, height: u16) {
        let h = u64::from(height.max(1));
        if self.cursor_row < self.top_row {
            self.top_row = self.cursor_row;
        } else if self.cursor_row >= self.top_row + h {
            self.top_row = self.cursor_row + 1 - h;
        }
    }

    fn ensure_col_visible(&mut self, viewport: Viewport) {
        let n = self.visible_cols();
        let n_frozen = self.frozen_count(viewport);
        let n_scroll = n - n_frozen;
        self.col_offset = self.col_offset.min(n_scroll.saturating_sub(1));
        if self.cursor_col < n_frozen {
            return;
        }
        let idx = self.cursor_col - n_frozen;
        if idx < self.col_offset {
            self.col_offset = idx;
            return;
        }
        while self.col_offset < idx {
            let fully = self
                .h_layout(viewport)
                .scrolling
                .iter()
                .any(|s| s.display_idx == self.cursor_col && !s.truncated);
            if fully {
                break;
            }
            self.col_offset += 1;
        }
    }

    /// Moves the cursor by `delta` rows, clamped to the view (§5.2: the
    /// lower bound while indexing).
    pub fn move_rows(&mut self, delta: i64, viewport: Viewport) {
        self.cursor_row = self.cursor_row.saturating_add_signed(delta);
        self.clamp(viewport);
    }

    /// `Ctrl-d` / `Ctrl-u` (half a page) and `PgDn` / `PgUp` (a page):
    /// cursor and `top_row` move together.
    pub fn page(&mut self, delta: i64, viewport: Viewport) {
        let len = self.view_len();
        let h = u64::from(viewport.body_height.max(1));
        let max_top = len.saturating_sub(h);
        self.top_row = self.top_row.saturating_add_signed(delta).min(max_top);
        self.cursor_row = self.cursor_row.saturating_add_signed(delta);
        self.clamp(viewport);
    }

    /// `Home`.
    pub fn first_row(&mut self, viewport: Viewport) {
        self.cursor_row = 0;
        self.clamp(viewport);
    }

    /// `G` (M2-03): the last row once the view's length is final;
    /// otherwise a pending jump that fires when the index completes.
    /// In a filtered view whose job is still growing, it goes to the
    /// current last match without waiting ("last row (so far)").
    pub fn last_row(&mut self, viewport: Viewport) {
        let derived = !matches!(self.views.active_view(), View::All);
        if derived || self.view_len_exact() || self.loaded.is_none() {
            self.cursor_row = self.view_len().saturating_sub(1);
            self.clamp(viewport);
        } else {
            self.start_jump(JumpTarget::LastRow, viewport);
        }
    }

    /// Sets a pending jump and tries it at once.
    fn start_jump(&mut self, target: JumpTarget, viewport: Viewport) -> JumpOutcome {
        self.pending_jump = Some(PendingJump {
            target,
            started: crate::app::now(),
            frame: 0,
        });
        self.poll_jump(viewport).unwrap_or(JumpOutcome::Jumped)
    }

    /// Performs the pending jump if the index now covers its target (on each
    /// tick and on `IndexReady`). `None` when no jump is pending.
    pub fn poll_jump(&mut self, viewport: Viewport) -> Option<JumpOutcome> {
        let jump = self.pending_jump?;
        let len = self.view_len();
        let exact = self.view_len_exact();
        let outcome = match jump.target {
            JumpTarget::LastRow if exact => {
                self.cursor_row = len.saturating_sub(1);
                JumpOutcome::Jumped
            }
            JumpTarget::Row(r) if r < len => {
                self.cursor_row = r;
                JumpOutcome::Jumped
            }
            JumpTarget::Row(_) if exact => {
                self.cursor_row = len.saturating_sub(1);
                JumpOutcome::Clamped { len }
            }
            JumpTarget::ByteOffset(t) => match self.loaded.as_ref().and_then(|l| row_at_byte(l, t))
            {
                Some(row) => {
                    self.cursor_row = row;
                    JumpOutcome::Jumped
                }
                None if exact => {
                    self.cursor_row = len.saturating_sub(1);
                    JumpOutcome::Jumped
                }
                None => JumpOutcome::Waiting,
            },
            _ => JumpOutcome::Waiting,
        };
        if outcome != JumpOutcome::Waiting {
            self.pending_jump = None;
            self.clamp(viewport);
            self.center_cursor(viewport);
        }
        Some(outcome)
    }

    /// After a long jump, puts the cursor row in the middle of the screen
    /// when it had to scroll.
    fn center_cursor(&mut self, viewport: Viewport) {
        let h = u64::from(viewport.body_height.max(1));
        let len = self.view_len();
        if self.cursor_row >= self.top_row + h || self.cursor_row < self.top_row {
            self.top_row = self.cursor_row.saturating_sub(h / 2);
        }
        self.top_row = self.top_row.min(len.saturating_sub(h));
        self.ensure_row_visible(viewport.body_height);
    }

    /// `g N` (M2-03): 1-based view position. Within the known length it
    /// moves now; past the indexed range while indexing it waits; past the
    /// final length it goes to the last row (`Clamped`).
    pub fn goto_row(&mut self, n: u64, viewport: Viewport) -> JumpOutcome {
        self.start_jump(JumpTarget::Row(n.saturating_sub(1)), viewport)
    }

    /// `g P%` (M2-03). Once the length is final: row
    /// `floor(P/100 × (len − 1))`. In the `All` view while indexing the row
    /// count isn't known yet, so P% means **P% of the file's bytes**: the
    /// first record starting at or after
    /// `data_start + P% × (len − data_start)`. That is exact as soon as the
    /// published checkpoints pass the offset; until then it is a pending
    /// jump.
    pub fn goto_percent(&mut self, p: f64, viewport: Viewport) -> JumpOutcome {
        self.pending_jump = None;
        let p = p.clamp(0.0, 100.0);
        let len = self.view_len();
        let all_view = self.views.active_view().is_all();
        match &self.loaded {
            Some(l) if !self.view_len_exact() && all_view => {
                let src = &l.source;
                let span = src.len().saturating_sub(src.data_start());
                let t = src.data_start() + (span as f64 * p / 100.0).floor() as u64;
                self.start_jump(JumpTarget::ByteOffset(t), viewport)
            }
            _ => {
                let row = (len.saturating_sub(1) as f64 * p / 100.0).floor() as u64;
                self.cursor_row = row;
                self.clamp(viewport);
                self.center_cursor(viewport);
                JumpOutcome::Jumped
            }
        }
    }

    /// `g <column>` (M2-03): moves the column cursor (and `col_offset`) to
    /// source column `col`. Errors when it is hidden.
    pub fn goto_column(&mut self, col: usize, viewport: Viewport) -> Result<(), String> {
        let display = self.layout.display();
        match display.iter().position(|&c| c == col) {
            Some(i) => {
                self.cursor_col = i;
                self.clamp(viewport);
                Ok(())
            }
            None => {
                let name = self
                    .loaded
                    .as_ref()
                    .and_then(|l| l.columns.get(col))
                    .map_or_else(String::new, |c| c.name.display.clone());
                Err(format!("column \"{name}\" is hidden — show it with c"))
            }
        }
    }

    // ---- column widths (M3-04) -----------------------------------------

    /// The largest width the cursor column may take: the table width minus
    /// the gutter, the other frozen columns (or the whole frozen block for a
    /// scrolling column) and one cell.
    fn max_width_for_cursor(&self, viewport: Viewport) -> u16 {
        let display = self.layout.display();
        let gutter = self.gutter_digits(viewport.body_height) + 3;
        let n_frozen = self.frozen_count(viewport);
        let frozen = if self.cursor_col < n_frozen {
            let others: u16 = display[..n_frozen]
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != self.cursor_col)
                .map(|(_, &c)| self.layout.widths[c] + 1)
                .sum();
            others + 3
        } else {
            self.frozen_block_width(&display, n_frozen)
        };
        viewport
            .table_width
            .saturating_sub(gutter)
            .saturating_sub(frozen)
            .saturating_sub(1)
            .max(MIN_COL_WIDTH)
    }

    /// Sets the cursor column's width by hand (`<`, `>`, `=`).
    fn set_cursor_width(&mut self, width: u16, viewport: Viewport) {
        let Some(col) = self.cursor_source_col() else {
            return;
        };
        let max = self.max_width_for_cursor(viewport);
        self.layout.widths[col] = width.clamp(MIN_COL_WIDTH.min(max), max);
        self.layout.manual[col] = true;
        self.clamp(viewport);
    }

    /// `<`: one cell narrower, at least [`MIN_COL_WIDTH`].
    pub fn shrink_col(&mut self, viewport: Viewport) {
        if let Some(col) = self.cursor_source_col() {
            let w = self.layout.widths[col].saturating_sub(1).max(MIN_COL_WIDTH);
            self.set_cursor_width(w, viewport);
        }
    }

    /// `>`: one cell wider, up to the available width.
    pub fn grow_col(&mut self, viewport: Viewport) {
        if let Some(col) = self.cursor_source_col() {
            let w = self.layout.widths[col].saturating_add(1);
            self.set_cursor_width(w, viewport);
        }
    }

    /// `=`: the widest value among the rows on screen (and the header),
    /// clamped to the available width but not to `max_column_width`: the
    /// user asked for it.
    pub fn autofit_col(&mut self, viewport: Viewport) {
        let Some(col) = self.cursor_source_col() else {
            return;
        };
        let header = self
            .loaded
            .as_ref()
            .and_then(|l| l.columns.get(col))
            .map_or(1, header_cells);
        let widest = u16::try_from(self.screen_width(col)).unwrap_or(u16::MAX);
        self.set_cursor_width(widest.max(header), viewport);
    }

    /// `:freeze N` (M3-04; the palette command is M6-02). `0` disables
    /// freezing.
    pub fn set_freeze(&mut self, n: usize, viewport: Viewport) {
        self.layout.freeze = n;
        self.col_offset = 0;
        self.freeze_notice = None;
        self.clamp(viewport);
    }

    /// `Some((requested, shown))` when fewer frozen columns are drawn than
    /// requested, the first time this pair occurs (the `freeze 3 (2 shown)`
    /// toast is shown once).
    pub fn take_freeze_notice(&mut self, viewport: Viewport) -> Option<(usize, usize)> {
        let requested = self.layout.freeze.min(self.visible_cols());
        let shown = self.frozen_count(viewport);
        if shown >= requested {
            self.freeze_notice = None;
            return None;
        }
        let pair = (self.layout.freeze, shown);
        if self.freeze_notice == Some(pair) {
            return None;
        }
        self.freeze_notice = Some(pair);
        Some(pair)
    }

    /// Applies a new column order and visibility (column chooser `Enter`).
    /// The cursor stays on the same source column when it is still visible.
    pub fn set_column_order(&mut self, order: Vec<usize>, visible: Vec<bool>, viewport: Viewport) {
        let current = self.cursor_source_col();
        self.layout.order = order;
        self.layout.visible = visible;
        let display = self.layout.display();
        self.cursor_col = current
            .and_then(|c| display.iter().position(|&d| d == c))
            .unwrap_or(self.cursor_col);
        self.col_offset = 0;
        self.clamp(viewport);
    }

    /// `h` / `l`: across frozen and scrolling columns alike.
    pub fn move_cols(&mut self, delta: isize, viewport: Viewport) {
        self.cursor_col = self.cursor_col.saturating_add_signed(delta);
        self.clamp(viewport);
    }

    /// `0` / `$`: first / last visible column.
    pub fn first_col(&mut self, viewport: Viewport) {
        self.cursor_col = 0;
        self.clamp(viewport);
    }

    pub fn last_col(&mut self, viewport: Viewport) {
        self.cursor_col = self.visible_cols().saturating_sub(1);
        self.clamp(viewport);
    }

    /// `w`: next scrolling column. From a frozen column it lands on the
    /// first scrolling column shown (`col_offset`); otherwise like `l`.
    pub fn next_col(&mut self, viewport: Viewport) {
        let n = self.visible_cols();
        let n_frozen = self.frozen_count(viewport);
        if self.cursor_col < n_frozen {
            if n_frozen + self.col_offset < n {
                self.cursor_col = n_frozen + self.col_offset;
            }
            self.clamp(viewport);
        } else {
            self.move_cols(1, viewport);
        }
    }

    /// `b`: previous scrolling column. It never enters the frozen block:
    /// on the first scrolling column it stays put. Inside the frozen block
    /// it moves like `h`.
    pub fn prev_col(&mut self, viewport: Viewport) {
        let n_frozen = self.frozen_count(viewport);
        if self.cursor_col != n_frozen {
            self.move_cols(-1, viewport);
        }
    }

    /// Mouse wheel: scrolls `top_row` by `delta` and drags the cursor along
    /// only when it would leave the screen (§1).
    pub fn scroll(&mut self, delta: i64, viewport: Viewport) {
        let h = u64::from(viewport.body_height.max(1));
        let max_top = self.view_len().saturating_sub(h);
        self.top_row = self.top_row.saturating_add_signed(delta).min(max_top);
        if self.cursor_row < self.top_row {
            self.cursor_row = self.top_row;
        } else if self.cursor_row >= self.top_row + h {
            self.cursor_row = self.top_row + h - 1;
        }
        self.clamp(viewport);
    }
}

/// Columns of a freshly opened source: header names, or `col1`….
fn columns_for(source: &Source) -> Vec<ColumnMeta> {
    source
        .column_names()
        .into_iter()
        .enumerate()
        .map(|(i, name)| ColumnMeta::new(name, i, false))
        .collect()
}

/// Cells the 2-line header of `meta` needs: its name and its type label.
/// Not clamped to `max_column_width`: headers are never cut by automatic
/// widths (M3-04).
pub fn header_cells(meta: &ColumnMeta) -> u16 {
    let w = text::width(&meta.name.display).max(text::width(meta.ty().label()));
    u16::try_from(w).unwrap_or(u16::MAX).max(1)
}

/// `content` (p95 or screen width) clamped to `[header, max]`; the header
/// wins.
fn auto_clamp(content: usize, header: u16, max: u16) -> u16 {
    let content = u16::try_from(content.min(usize::from(max))).unwrap_or(max);
    content.max(header).max(1)
}

/// Sniff step 5 again with the dialect of `src` forced: does a quoted field
/// in the sample hold a newline? Chooses the indexer's path (M2-04).
fn quoted_newlines(src: &Source, sample_bytes: usize) -> bool {
    let d = *src.dialect();
    if d.quote.is_none() {
        return false;
    }
    let forced = DialectOverrides {
        delimiter: Some(d.delimiter),
        quote: Some(d.quote),
        escape: Some(d.escape),
        header: Some(d.header),
        encoding: Some(d.encoding),
        comment: d.comment,
    };
    dialect::sniff(src.bytes(), sample_bytes, &forced).quoted_newlines
}

/// The row whose record is the first to start at or after byte `t`, once
/// the published checkpoints reach `t` (or the index is complete). A forward
/// skip of at most one stride from the last checkpoint ≤ `t` (M2-03).
fn row_at_byte(l: &Loaded, t: u64) -> Option<u64> {
    let index = &l.index;
    let published = index.published_checkpoints();
    let last = index.checkpoint(published.checked_sub(1)?)?;
    let complete = index.is_complete();
    if !complete && last < t {
        return None;
    }
    // The last checkpoint ≤ t (checkpoint 0 is data_start).
    let (mut lo, mut hi) = (0, published);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if index.checkpoint(mid)? <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let src = &l.source;
    let parser = RecordParser::new(src.dialect());
    let mut row = lo * index.stride();
    let mut pos = index.checkpoint(lo)?;
    let found = loop {
        match parser.next_record(src.bytes(), pos) {
            Some((start, _, _)) if start >= t => break row,
            Some((_, next, _)) => {
                pos = next;
                row += 1;
            }
            // Past the last record: the last row.
            None => break row.saturating_sub(1),
        }
    };
    if complete {
        Some(found.min(index.indexed_rows().saturating_sub(1)))
    } else if found < index.indexed_rows() {
        Some(found)
    } else {
        None
    }
}

/// Raw mode (M2-04): `count` physical lines from line `first` (0-based),
/// split on `\n` only, without quote awareness, after the BOM. A `\r` before
/// the `\n` is kept (it is part of the true structure and shows escaped).
/// Each line is cut at 4 KiB.
fn raw_lines(src: &Source, first: u64, count: usize) -> Vec<String> {
    let bytes = src.bytes();
    let enc = src.dialect().encoding;
    let mut pos = enc.bom_len_in(bytes);
    let mut out = Vec::with_capacity(count);
    let mut line = 0u64;
    while pos < bytes.len() && out.len() < count {
        let end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| pos + i);
        if line >= first {
            let raw = &bytes[pos..end.min(pos + RAW_LINE_BYTES)];
            out.push(decode_field(raw, enc).into_owned());
        }
        line += 1;
        pos = end + 1;
    }
    out
}

/// Width of a value as drawn: control characters take their escaped form.
pub fn cell_width(value: &str) -> usize {
    display_segments(value)
        .iter()
        .map(|s| text::width(&s.text))
        .sum()
}

#[cfg(test)]
pub mod tests {
    use std::io::Write;

    use pretty_assertions::assert_eq;
    use tachy_core::dialect::{self, DialectOverrides};

    use super::*;

    /// A loaded tab over `content`, with a complete index (built
    /// synchronously by walking the parser).
    pub fn loaded_tab(content: &str, freeze: usize) -> (NamedTempFile, Tab) {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        let (source, report) = Source::open_sniffed(
            f.path(),
            Some("t.csv".into()),
            dialect::DEFAULT_SAMPLE_BYTES,
            &DialectOverrides::default(),
        )
        .unwrap();
        let mut tab = Tab::new(
            TabId(1),
            "t.csv".into(),
            f.path().to_path_buf(),
            Phase::Opening {
                progress: Arc::default(),
            },
            freeze,
            40,
        );
        tab.set_loaded(Opened {
            source,
            report,
            transcoded: None,
            original_encoding: None,
            stamp: None,
        });
        complete_index(&tab);
        // A fixture is an already-confirmed tab: no Detected format dialog.
        tab.dialog_shown = true;
        (f, tab)
    }

    /// Fills the tab's index synchronously (tests only).
    pub fn complete_index(tab: &Tab) {
        let l = tab.loaded.as_ref().unwrap();
        let src = &l.source;
        let mut p = tachy_core::parse::RecordParser::new(src.dialect());
        let mut rec = tachy_core::parse::RecordRanges::default();
        let mut pos = src.data_start();
        let mut rows = 0u64;
        let mut ragged = 0u64;
        let mut checkpoints = Vec::new();
        loop {
            let start = p.skip_ignorable(src.bytes(), pos as usize) as u64;
            match p.parse_at(src.bytes(), start, &mut rec) {
                tachy_core::parse::ParseOutcome::Eof => break,
                tachy_core::parse::ParseOutcome::Record { next }
                | tachy_core::parse::ParseOutcome::UnterminatedQuote { next } => {
                    if rows > 0 && rows.is_multiple_of(l.index.stride()) {
                        checkpoints.push(start);
                    }
                    if rec.fields.len() != src.width() {
                        ragged += 1;
                    }
                    rows += 1;
                    pos = next;
                }
            }
        }
        l.index.push_checkpoints(&checkpoints);
        l.index.set_ragged_rows(ragged);
        l.index.set_bytes_scanned(src.len() - src.data_start());
        l.index.finish(rows, pos);
    }

    /// `rows` × `cols` of `rNcM` values, with a header.
    pub fn grid(rows: usize, cols: usize) -> String {
        let mut s = (0..cols)
            .map(|c| format!("c{c}"))
            .collect::<Vec<_>>()
            .join(",");
        s.push('\n');
        for r in 0..rows {
            let line = (0..cols)
                .map(|c| format!("r{r}c{c}"))
                .collect::<Vec<_>>()
                .join(",");
            s.push_str(&line);
            s.push('\n');
        }
        s
    }

    const VP: Viewport = Viewport {
        body_height: 10,
        table_width: 60,
    };

    #[test]
    fn rows_clamp_at_both_ends() {
        let (_f, mut tab) = loaded_tab(&grid(100, 20), 1);
        tab.prepare_frame(VP);
        assert_eq!(tab.view_len(), 100);
        tab.move_rows(-1, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (0, 0));
        tab.move_rows(9, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (9, 0));
        tab.move_rows(1, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (10, 1));
        tab.move_rows(1_000, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (99, 90));
        tab.move_rows(-5, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (94, 90));
        tab.first_row(VP);
        assert_eq!((tab.cursor_row, tab.top_row), (0, 0));
        tab.last_row(VP);
        assert_eq!((tab.cursor_row, tab.top_row), (99, 90));
    }

    #[test]
    fn pages_move_cursor_and_top_together() {
        let (_f, mut tab) = loaded_tab(&grid(100, 3), 1);
        tab.page(5, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (5, 5));
        tab.page(10, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (15, 15));
        tab.page(-10, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (5, 5));
        tab.page(-10, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (0, 0));
        tab.page(1_000, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (99, 90));
    }

    #[test]
    fn mouse_wheel_scrolls_and_drags_the_cursor() {
        let (_f, mut tab) = loaded_tab(&grid(100, 3), 1);
        tab.scroll(3, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (3, 3));
        tab.move_rows(5, VP);
        tab.scroll(3, VP);
        // The cursor (8) is still on screen (6..16): it stays.
        assert_eq!((tab.cursor_row, tab.top_row), (8, 6));
        tab.scroll(-6, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (8, 0));
        tab.scroll(-3, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (8, 0));
        tab.scroll(1_000, VP);
        assert_eq!((tab.cursor_row, tab.top_row), (90, 90));
    }

    #[test]
    fn columns_clamp_and_scroll_minimally() {
        // 20 columns of width 6 (`r10c10` etc. are at most 6 wide).
        let (_f, mut tab) = loaded_tab(&grid(100, 20), 1);
        tab.prepare_frame(VP);
        tab.move_cols(-1, VP);
        assert_eq!((tab.cursor_col, tab.col_offset), (0, 0));
        tab.last_col(VP);
        assert_eq!(tab.cursor_col, 19);
        let h = tab.h_layout(VP);
        let last = h.scrolling.last().unwrap();
        assert_eq!(last.display_idx, 19);
        assert!(!last.truncated);
        assert_eq!(h.more_cols, 0);
        tab.move_cols(1, VP);
        assert_eq!(tab.cursor_col, 19);
        // The frozen column is always visible: no scrolling back.
        tab.first_col(VP);
        assert_eq!(tab.cursor_col, 0);
        assert!(tab.col_offset > 0);
        // Moving right through the scrolling columns scrolls back minimally.
        tab.move_cols(1, VP);
        assert_eq!((tab.cursor_col, tab.col_offset), (1, 0));
    }

    #[test]
    fn w_from_frozen_lands_on_first_scrolling_column_and_b_never_enters_frozen() {
        let (_f, mut tab) = loaded_tab(&grid(10, 20), 2);
        tab.prepare_frame(VP);
        tab.last_col(VP);
        tab.first_col(VP);
        let offset = tab.col_offset;
        assert!(offset > 0);
        // The frozen block stays visible while scrolled.
        assert_eq!(tab.h_layout(VP).frozen.len(), 2);
        tab.next_col(VP);
        assert_eq!(tab.cursor_col, 2 + offset);
        assert_eq!(tab.col_offset, offset);
        tab.next_col(VP);
        assert_eq!(tab.cursor_col, 3 + offset);
        // `b` walks back to the first scrolling column, then stays.
        for _ in 0..30 {
            tab.prev_col(VP);
        }
        assert_eq!((tab.cursor_col, tab.col_offset), (2, 0));
        // Inside the frozen block, `b` moves like `h`.
        tab.cursor_col = 1;
        tab.prev_col(VP);
        assert_eq!(tab.cursor_col, 0);
    }

    #[test]
    fn resize_keeps_the_cursor_visible() {
        let (_f, mut tab) = loaded_tab(&grid(100, 20), 1);
        let big = Viewport {
            body_height: 30,
            table_width: 160,
        };
        tab.prepare_frame(big);
        tab.move_rows(25, big);
        for _ in 0..12 {
            tab.move_cols(1, big);
        }
        assert_eq!(tab.col_offset, 0);
        tab.clamp(VP);
        assert_eq!(tab.top_row, 25 + 1 - 10);
        let h = tab.h_layout(VP);
        assert!(
            h.scrolling
                .iter()
                .any(|s| s.display_idx == 12 && !s.truncated),
            "{h:?}"
        );
    }

    #[test]
    fn horizontal_layout_and_more_cols() {
        let (_f, mut tab) = loaded_tab(&grid(5, 12), 1);
        tab.prepare_frame(VP);
        let h = tab.h_layout(VP);
        assert_eq!(h.gutter_digits, 3);
        assert_eq!(h.gutter_width, 6);
        // `c0` holds `r4c0`: 4 wide.
        assert_eq!(h.frozen[0].x, 6);
        assert_eq!(h.frozen[0].width, 4);
        assert_eq!(h.frozen_divider, Some(11));
        assert_eq!(h.scrolling[0].x, 13);
        let drawn = h.scrolling.len();
        assert!(drawn < 11);
        assert_eq!(h.more_cols, 11 - drawn);
        // Hidden columns don't count.
        tab.layout.visible[11] = false;
        assert_eq!(tab.h_layout(VP).more_cols, 10 - drawn);
    }

    #[test]
    fn gutter_rounds_to_three_digits() {
        let (_f, tab) = loaded_tab(&grid(5, 2), 1);
        assert_eq!(tab.gutter_digits(10), 3);
        assert_eq!(tab.gutter_digits(1_000), 6);
        let mut sorted = tab;
        sorted.push_view(sorted_view(&[1, 0]), None);
        assert_eq!(sorted.gutter_digits(10), 10);
    }

    /// A finished sorted view over `ids`.
    pub fn sorted_view(ids: &[u64]) -> View {
        let list = tachy_core::view::RowIdList::from_ids(&std::env::temp_dir(), ids).unwrap();
        View::Ordered {
            list,
            kind: tachy_core::view::OrderedKind::Sorted {
                keys: vec![tachy_core::sort::SortKey::desc(0)],
            },
        }
    }

    /// A finished filtered view over `ids`.
    pub fn filtered_view(ids: &[u64], columns: Vec<usize>) -> View {
        let bitmap: roaring::RoaringTreemap = ids.iter().copied().collect();
        View::Filtered {
            rows: Arc::new(tachy_core::view::FilterRows::from_bitmap(bitmap)),
            expr: "x".into(),
            columns,
        }
    }

    #[test]
    fn derived_views_map_positions_to_row_ids() {
        let (_f, mut tab) = loaded_tab(&grid(100, 3), 1);
        tab.push_view(filtered_view(&[3, 50, 99], vec![1]), None);
        assert_eq!(tab.view_len(), 3);
        assert!(tab.view_len_exact());
        tab.prepare_frame(VP);
        let l = tab.loaded.as_ref().unwrap();
        assert_eq!(l.frame.ids, [3, 50, 99]);
        assert_eq!(l.frame.rows.len(), 3);
        assert_eq!(
            l.frame.rows[1].as_ref().unwrap().display(&l.source, 0),
            "r50c0"
        );
        assert_eq!(tab.row_id(2), Some(99));
        assert_eq!(tab.row_id(3), None);
        assert_eq!(tab.view_label(), "filtered");

        // A sort over the filter keeps its own order.
        tab.push_view(sorted_view(&[99, 3, 50]), None);
        tab.prepare_frame(VP);
        assert_eq!(tab.loaded.as_ref().unwrap().frame.ids, [99, 3, 50]);
        assert_eq!(tab.view_label(), "sorted");

        // An empty derived view.
        tab.push_view(filtered_view(&[], vec![]), None);
        tab.prepare_frame(VP);
        assert_eq!(tab.view_len(), 0);
        assert!(tab.loaded.as_ref().unwrap().frame.rows.is_empty());
        assert!(tab.cursor_record().is_none());
    }

    #[test]
    fn filter_sort_pop_pop_restores_the_original_cursor() {
        let (_f, mut tab) = loaded_tab(&grid(100, 5), 1);
        tab.move_rows(42, VP);
        tab.move_cols(2, VP);
        let original = tab.cursor_state();
        tab.push_view(
            filtered_view(&(0..100).step_by(2).collect::<Vec<_>>(), vec![]),
            None,
        );
        // A new view starts at row 0 in the same column.
        assert_eq!((tab.cursor_row, tab.cursor_col), (0, 2));
        tab.move_rows(7, VP);
        let in_filter = tab.cursor_state();
        tab.push_view(sorted_view(&[5, 4, 3]), None);
        tab.move_rows(1, VP);
        assert!(matches!(tab.pop_view(VP), PopOutcome::Popped(_)));
        assert_eq!(tab.cursor_state(), in_filter);
        assert!(matches!(tab.pop_view(VP), PopOutcome::Popped(_)));
        assert_eq!(tab.cursor_state(), original);
        assert!(matches!(tab.pop_view(VP), PopOutcome::Nothing));
        assert_eq!(tab.view_label(), "all rows");
    }

    #[test]
    fn enter_toggles_between_a_view_and_the_source_row() {
        let (_f, mut tab) = loaded_tab(&grid(100, 3), 1);
        // `Enter` in `All` with nothing stacked does nothing.
        assert!(!tab.jump_to_source(VP));
        tab.push_view(filtered_view(&[10, 20, 77], vec![]), None);
        tab.move_rows(2, VP);
        assert!(tab.jump_to_source(VP));
        assert_eq!(tab.cursor_row, 77, "the same record in all rows");
        assert_eq!(tab.view_label(), "all rows ↩ filtered");
        tab.move_rows(-5, VP);
        // `Enter` again: back to the filter at the same position.
        assert!(tab.jump_to_source(VP));
        assert_eq!(tab.cursor_row, 2);
        assert_eq!(tab.view_label(), "filtered");
        // `x` while toggled returns to the top without popping.
        tab.jump_to_source(VP);
        assert!(matches!(tab.pop_view(VP), PopOutcome::Returned));
        assert_eq!((tab.views.depth(), tab.cursor_row), (2, 2));
    }

    #[test]
    fn enter_past_the_indexed_rows_waits_for_the_index() {
        let (_f, mut tab) = loaded_tab(&grid(100, 3), 1);
        // Pretend the index is still at 0 rows.
        let l = tab.loaded.as_mut().unwrap();
        l.index = Arc::new(RowIndex::for_source(&l.source));
        tab.push_view(filtered_view(&[60], vec![]), None);
        assert!(tab.jump_to_source(VP));
        assert_eq!(
            tab.pending_jump.map(|j| j.target),
            Some(JumpTarget::Row(60))
        );
    }

    #[test]
    fn truncating_views_restores_the_new_top() {
        let (_f, mut tab) = loaded_tab(&grid(100, 3), 1);
        tab.move_rows(9, VP);
        tab.push_view(filtered_view(&[1, 2], vec![]), Some(JobId(7)));
        tab.push_view(sorted_view(&[2, 1]), None);
        let dropped = tab.truncate_views(1, VP);
        assert_eq!(dropped.len(), 2);
        assert_eq!(dropped[0].job, Some(JobId(7)));
        assert_eq!((tab.views.depth(), tab.cursor_row), (1, 9));
        // `All` is never dropped.
        assert!(tab.truncate_views(0, VP).is_empty());
    }

    #[test]
    fn reload_remembers_the_record_and_column() {
        let (_f, mut tab) = loaded_tab(&grid(100, 4), 1);
        tab.move_rows(30, VP);
        tab.move_cols(2, VP);
        tab.push_view(filtered_view(&[30, 31], vec![]), Some(JobId(1)));
        tab.move_rows(1, VP);
        let old = tab.cancel.clone();
        let overrides = tab.reload_overrides().unwrap();
        assert_eq!(overrides.delimiter, Some(b','));
        let (dropped, _) = tab.begin_reload();
        assert!(old.is_cancelled());
        assert!(!tab.cancel.is_cancelled());
        assert_eq!(dropped.len(), 1);
        assert_eq!(tab.generation, 1);
        assert!(tab.loaded.is_none());
        assert_eq!(
            tab.restore,
            Some(Restore {
                row_id: 31,
                column: Some("c2".into())
            })
        );
    }

    #[test]
    fn rendering_a_window_of_a_huge_filtered_view_is_fast() {
        let mut bitmap = roaring::RoaringTreemap::new();
        bitmap.insert_range(0..100_000_000);
        let view = View::Filtered {
            rows: Arc::new(tachy_core::view::FilterRows::from_bitmap(bitmap)),
            expr: String::new(),
            columns: Vec::new(),
        };
        let t = std::time::Instant::now();
        let ids = view.row_ids(99_000_000, 50);
        let took = t.elapsed();
        assert_eq!(ids.len(), 50);
        assert_eq!(ids[0], 99_000_000);
        // < 1 ms in release builds; debug builds get some slack.
        assert!(took < std::time::Duration::from_millis(20), "{took:?}");
    }

    #[test]
    fn extra_columns_for_long_rows() {
        let (_f, mut tab) = loaded_tab("a,b\n1,2\n3,4,5,6\n", 1);
        tab.prepare_frame(VP);
        let l = tab.loaded.as_ref().unwrap();
        let names: Vec<_> = l.columns.iter().map(|c| c.name.display.clone()).collect();
        assert_eq!(names, ["a", "b", "_extra1", "_extra2"]);
        assert_eq!(tab.visible_cols(), 4);
        assert_eq!(l.ragged_seen, 1);
    }

    #[test]
    fn widths_come_from_the_first_screen() {
        let (_f, mut tab) = loaded_tab("id,name\n1,日本語テキスト\n2,x\n", 1);
        tab.prepare_frame(VP);
        assert_eq!(tab.layout.widths, [3, 14]);
        let long = format!("a\n{}\n", "x".repeat(100));
        let (_f, mut tab) = loaded_tab(&long, 1);
        tab.prepare_frame(VP);
        assert_eq!(tab.layout.widths, [40]);
    }

    #[test]
    fn dialect_restart_bumps_the_generation() {
        let (_f, mut tab) = loaded_tab("a;b\n1;2\n", 1);
        let old = tab.loaded.as_ref().unwrap().index_cancel.clone();
        let mut d = *tab.loaded.as_ref().unwrap().source.dialect();
        d.delimiter = b',';
        tab.apply_dialect(d, 65_536);
        assert!(old.is_cancelled());
        assert_eq!(tab.generation, 1);
        let l = tab.loaded.as_ref().unwrap();
        assert!(!l.index.is_complete());
        assert_eq!(l.columns.len(), 1);
        assert!(l.cache.is_empty());
    }

    #[test]
    fn automatic_widths_clamp_to_the_header_and_the_maximum() {
        // p95 inside the range.
        assert_eq!(auto_clamp(10, 3, 40), 10);
        // Below the header: the header.
        assert_eq!(auto_clamp(2, 5, 40), 5);
        // Above the maximum: the maximum.
        assert_eq!(auto_clamp(256, 5, 40), 40);
        // A header wider than the maximum wins.
        assert_eq!(auto_clamp(10, 47, 40), 47);
        assert_eq!(auto_clamp(0, 1, 40), 1);
    }

    #[test]
    fn raw_lines_split_on_newlines_only() {
        let (_f, mut tab) = loaded_tab("a,\"x\ny\"\r\n1,2\n", 1);
        tab.raw_mode = true;
        tab.prepare_frame(VP);
        let l = tab.loaded.as_ref().unwrap();
        assert_eq!(l.raw_lines, ["a,\"x", "y\"\r", "1,2"]);
    }

    #[test]
    fn dropping_a_tab_cancels_its_work() {
        let (_f, tab) = loaded_tab("a\n1\n", 1);
        let token = tab.loaded.as_ref().unwrap().index_cancel.clone();
        drop(tab);
        assert!(token.is_cancelled());
    }
}
