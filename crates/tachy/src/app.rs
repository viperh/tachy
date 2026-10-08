//! The UI task: owns [`AppState`] and the components, runs the event loop
//! (spec §4.1, README §A2, §A3).
//!
//! Tabs (M1-08): every file argument opens in its own tab. The tab appears
//! at once as a placeholder; opening and sniffing run in `spawn_blocking`
//! and come back as `Msg::SourceOpened`. Then the indexer is spawned
//! (M2-02) and reports `Msg::IndexReady` / `Msg::IndexFailed`, tagged with
//! the tab's generation. `-` first copies stdin into a temp file
//! (`Msg::SpoolDone`).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use color_eyre::eyre::eyre;
use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{Frame, backend::Backend, layout::Size, prelude::Rect};
use tachy_core::{
    dialect::{Dialect, DialectOverrides},
    exec::Executor,
    index::{IndexError, IndexOptions, build_index},
    jobs::{JobKind, Progress},
    sample::{SamplePhase, sample_head, sample_spread},
    source::Source,
    spool::transcode_blocking,
    types::NullSet,
};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{
    action::Action,
    components::{
        Component,
        dialogs::{
            Dialogs,
            confirm::{ConfirmAction, ConfirmState},
        },
        help::Help,
        hints::Hints,
        inspector::Inspector,
        jobs_drawer::JobsDrawer,
        layout::{
            apply_backdrop, compute_layout, palette_rect, render_too_small, render_vertical_divider,
        },
        palette::Palette,
        query_bar::QueryBar,
        status::{Status, sample_progress},
        table::Table,
        toast::Toast,
        top_bar::TopBar,
    },
    config::Config,
    jobs::{JobMsg, SHUTDOWN_TIMEOUT},
    keymap::KeyChord,
    mode::{KeyContext, Mode},
    msg::{Msg, OpenFailure},
    path_complete::{self, Candidate},
    settings::Settings,
    spool::spool_stdin,
    state::{AppState, DialogKind, Focus, OpenPrompt, Overlay, PromptMessage},
    tab::{OpenProgress, Opened, Phase, Tab, TabId, Viewport},
    theme::ColorSupport,
    toast::{Toast as ToastMsg, ToastLevel},
    tui::{Event, Tui},
    watch::FileStamp,
};

/// Minimum time between two frames: at most ~60 fps (§4.1).
const FRAME_MIN: Duration = Duration::from_millis(16);
/// Rows the mouse wheel scrolls per notch (§1).
const WHEEL_ROWS: i64 = 3;
/// Directory listings for path completion give up after this (M1-08).
const COMPLETION_TIMEOUT: Duration = Duration::from_millis(200);

/// `SIGHUP` and `SIGTERM` (Unix); never fires elsewhere.
struct Hangup {
    #[cfg(unix)]
    signals: [tokio::signal::unix::Signal; 2],
}

impl Hangup {
    fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Hangup {
                signals: [
                    signal(SignalKind::hangup())?,
                    signal(SignalKind::terminate())?,
                ],
            })
        }
        #[cfg(not(unix))]
        Ok(Hangup {})
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let [hup, term] = &mut self.signals;
            tokio::select! {
                _ = hup.recv() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await
    }
}

/// The app clock (toasts, throughput samples). Read through tokio, so tests
/// with paused time control it.
pub fn now() -> std::time::Instant {
    Instant::now().into_std()
}

/// What the open step needs. Built on the UI task, run in `spawn_blocking`.
pub struct OpenRequest {
    pub path: PathBuf,
    /// The tab title; `None` uses the file name.
    pub display_name: Option<String>,
    pub sample_bytes: usize,
    pub overrides: DialectOverrides,
    pub tmp_dir: PathBuf,
    pub progress: Arc<OpenProgress>,
    pub cancel: CancellationToken,
}

/// The open step, injectable so tests can simulate a slow `open()`.
pub type OpenFn = Arc<dyn Fn(OpenRequest) -> Result<Opened, OpenFailure> + Send + Sync>;

/// Opens and sniffs a file (§2.1); a UTF-16 file is transcoded into a UTF-8
/// temp file first (M1-03), which is opened under the original name with
/// the transcoded dialect. Blocking.
pub fn open_blocking(req: OpenRequest) -> Result<Opened, OpenFailure> {
    let (source, report) = Source::open_sniffed(
        &req.path,
        req.display_name.clone(),
        req.sample_bytes,
        &req.overrides,
    )
    .map_err(OpenFailure::Source)?;
    // The watcher compares against the file as given: for UTF-16, the
    // original, captured before transcoding (M7-03).
    let stamp = Some(FileStamp::of_source(&source));
    let enc = report.dialect.encoding;
    if !enc.is_utf16() {
        return Ok(Opened {
            source,
            report,
            transcoded: None,
            original_encoding: None,
            stamp,
        });
    }
    let name = source.display_name().to_owned();
    req.progress.total.store(source.len(), Ordering::Relaxed);
    req.progress.transcoding.store(true, Ordering::Relaxed);
    drop(source);
    let failed = |e: &dyn std::fmt::Display| {
        OpenFailure::Other(format!("cannot open {}: {e}", req.path.display()))
    };
    let tmp = transcode_blocking(
        &req.path,
        enc,
        &req.tmp_dir,
        &req.progress.bytes,
        &req.cancel,
    )
    .map_err(|e| failed(&e))?;
    let raw = Source::open(tmp.path(), Some(name)).map_err(|e| failed(&e))?;
    Ok(Opened {
        source: raw.with_dialect(report.dialect.transcoded()),
        report,
        transcoded: Some(tmp),
        original_encoding: Some(enc),
        stamp,
    })
}

/// One component per screen region. Named fields, so the layout can map each
/// region to its component and key routing can reach the focused one.
#[derive(Debug, Default)]
struct Components {
    top_bar: TopBar,
    query_bar: QueryBar,
    table: Table,
    inspector: Inspector,
    jobs_drawer: JobsDrawer,
    toast: Toast,
    status: Status,
    hints: Hints,
    palette: Palette,
    help: Help,
    dialogs: Dialogs,
}

impl Components {
    /// Every component, for broadcasting events and actions.
    fn all_mut(&mut self) -> [&mut dyn Component; 11] {
        [
            &mut self.top_bar,
            &mut self.query_bar,
            &mut self.table,
            &mut self.inspector,
            &mut self.jobs_drawer,
            &mut self.toast,
            &mut self.status,
            &mut self.hints,
            &mut self.palette,
            &mut self.help,
            &mut self.dialogs,
        ]
    }
}

pub struct App {
    config: Config,
    state: AppState,
    ui: Components,
    should_quit: bool,
    /// Quitting because of `SIGHUP` / `SIGTERM`.
    hung_up: bool,
    should_suspend: bool,
    /// Something visible changed since the last frame.
    dirty: bool,
    last_draw: Option<Instant>,
    /// The terminal area, for navigation (body height, table width).
    area: Rect,
    /// One per app, built inside the runtime on first use (README §A2).
    executor: Option<Executor>,
    next_tab_id: u64,
    opener: OpenFn,
    /// True while a task is reading stdin (`-`). The read may block on the
    /// runtime's blocking pool, see `main`.
    stdin_reading: Arc<AtomicBool>,
    action_tx: mpsc::UnboundedSender<Action>,
    action_rx: mpsc::UnboundedReceiver<Action>,
    /// Cloned into every spawned task.
    msg_tx: mpsc::UnboundedSender<Msg>,
    msg_rx: mpsc::UnboundedReceiver<Msg>,
    /// OSC 52 sequences (`y` / `Y`, M6-05) to write after the next frame:
    /// never inside `draw`, so they can't interleave with a frame.
    pending_osc: Vec<u8>,
    /// `--sort` was applied (or failed) on the first tab (M5-03).
    startup_sort_done: bool,
    /// What tests "wrote" to the terminal instead of stdout.
    #[cfg(test)]
    osc_written: Vec<u8>,
    /// Frames drawn, for tests.
    #[cfg(test)]
    renders: usize,
}

impl App {
    pub fn new(
        mut config: Config,
        settings: Settings,
        color: ColorSupport,
    ) -> color_eyre::Result<Self> {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        // Config problems found at load time become toasts (M6-03).
        let config_warnings = std::mem::take(&mut config.warnings);
        let mut app = Self {
            config,
            state: AppState::new(settings, color),
            ui: Components::default(),
            should_quit: false,
            hung_up: false,
            should_suspend: false,
            dirty: true,
            last_draw: None,
            area: Rect::default(),
            executor: None,
            next_tab_id: 1,
            opener: Arc::new(open_blocking),
            stdin_reading: Arc::new(AtomicBool::new(false)),
            action_tx,
            action_rx,
            msg_tx,
            msg_rx,
            pending_osc: Vec::new(),
            startup_sort_done: false,
            #[cfg(test)]
            osc_written: Vec::new(),
            #[cfg(test)]
            renders: 0,
        };
        app.state.jobs.set_sender(app.msg_tx.clone());
        for toast in crate::config::warning_toasts(&config_warnings) {
            app.state.toasts.push(toast, now());
        }
        app.load_views();
        Ok(app)
    }

    pub async fn run(&mut self) -> color_eyre::Result<()> {
        let mut tui = Tui::new()?.mouse(true).paste(true);
        tui.enter()?;
        let size = tui.size()?;
        self.area = Rect::new(0, 0, size.width, size.height);
        self.init_components(size)?;
        self.start();
        let mut hangup = Hangup::new()?;

        loop {
            tokio::select! {
                result = self.step(&mut tui) => {
                    if let Err(e) = result {
                        // Drawing to a terminal that just hung up fails
                        // before the signal is seen: wait briefly for it.
                        let hung_up =
                            tokio::time::timeout(Duration::from_millis(200), hangup.recv())
                                .await
                                .is_ok();
                        if !hung_up {
                            return Err(e);
                        }
                        self.should_quit = true;
                        self.hung_up = true;
                    }
                }
                // The terminal went away (`SIGHUP`) or `kill` (`SIGTERM`):
                // quit without asking, so jobs are cancelled and every temp
                // file (stdin spool, sort runs, permutation files) is deleted.
                () = hangup.recv() => {
                    self.should_quit = true;
                    self.hung_up = true;
                }
            }
            if self.should_suspend {
                tui.suspend()?;
                self.should_suspend = false;
                self.action_tx.send(Action::Resume)?;
                self.action_tx.send(Action::ClearScreen)?;
                tui.enter()?;
            } else if self.should_quit {
                break;
            }
        }
        // After a hangup the terminal is gone: restoring it can fail, and
        // must not stop the cleanup below.
        let exited = tui.exit();
        // Cancel every job and give them 2 s to drop their temp files
        // (M5-01).
        self.state.jobs.shutdown(SHUTDOWN_TIMEOUT).await;
        if self.hung_up {
            // Nothing to restore: ratatui's `Terminal::drop` would
            // `eprintln!` its failure to show the cursor, and that panics on
            // the closed stderr (killing the process before the tabs' temp
            // files are deleted). The process is exiting anyway.
            std::mem::forget(tui);
            return Ok(());
        }
        exited?;
        Ok(())
    }

    /// Whether a task may still be blocked reading stdin. tokio's runtime
    /// waits for blocking reads on shutdown, so `main` exits the process
    /// directly in that case (after the tabs, and their temp files, are
    /// dropped).
    pub fn stdin_reading(&self) -> bool {
        self.stdin_reading.load(Ordering::Relaxed)
    }

    fn init_components(&mut self, size: Size) -> color_eyre::Result<()> {
        for component in self.ui.all_mut() {
            component.register_action_handler(self.action_tx.clone())?;
            component.register_config_handler(self.config.clone())?;
            component.init(size)?;
        }
        Ok(())
    }

    /// Opens the command-line files, one tab each, in argument order; the
    /// first is active (M1-08). Must run inside the runtime.
    pub fn start(&mut self) {
        let files = self.state.settings.files.clone();
        for path in files {
            if path.as_os_str() == "-" {
                self.open_stdin();
            } else {
                self.open_path(path, None);
            }
        }
        self.state.active_tab = 0;
    }

    fn executor(&mut self) -> Executor {
        let threads = self.state.settings.threads;
        self.executor
            .get_or_insert_with(|| Executor::new(threads))
            .clone()
    }

    fn new_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        id
    }

    fn tab_index(&self, id: TabId) -> Option<usize> {
        self.state.tabs.iter().position(|t| t.id == id)
    }

    /// Adds a placeholder tab for `path` and starts opening it.
    fn open_path(&mut self, path: PathBuf, display_name: Option<String>) -> TabId {
        let id = self.new_tab_id();
        let name = display_name.clone().unwrap_or_else(|| {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        });
        let progress = Arc::new(OpenProgress::default());
        let settings = &self.state.settings;
        let tab = Tab::new(
            id,
            name,
            path.clone(),
            Phase::Opening {
                progress: Arc::clone(&progress),
            },
            settings.freeze,
            settings.max_column_width,
        );
        let mut tab = tab;
        tab.nulls = NullSet::new(&settings.null_values);
        let request = OpenRequest {
            path,
            display_name,
            sample_bytes: settings.sniff_sample_bytes,
            overrides: settings.dialect_overrides,
            tmp_dir: settings.tmp_dir.clone(),
            progress,
            cancel: tab.cancel.child_token(),
        };
        self.state.tabs.push(tab);
        self.spawn_open(id, 0, request);
        self.update_sigbus_message();
        id
    }

    fn spawn_open(&self, id: TabId, generation: u64, request: OpenRequest) {
        let opener = Arc::clone(&self.opener);
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || opener(request))
                .await
                .unwrap_or_else(|e| Err(OpenFailure::Other(format!("cannot open: {e}"))));
            let _ = tx.send(Msg::SourceOpened {
                tab: id,
                generation,
                result: result.map(Box::new),
            });
        });
    }

    /// `-`: a `stdin` tab that copies stdin into a temp file in `--tmp`
    /// (M1-08), shown as `reading stdin`.
    fn open_stdin(&mut self) {
        let tmp = match tempfile::Builder::new()
            .prefix("tachy-stdin-")
            .tempfile_in(&self.state.settings.tmp_dir)
        {
            Ok(tmp) => tmp,
            Err(e) => {
                self.state.toasts.push(
                    ToastMsg::new(ToastLevel::Error, format!("cannot read stdin: {e}")),
                    now(),
                );
                return;
            }
        };
        let file = match tmp.as_file().try_clone() {
            Ok(file) => tokio::fs::File::from_std(file),
            Err(e) => {
                self.state.toasts.push(
                    ToastMsg::new(ToastLevel::Error, format!("cannot read stdin: {e}")),
                    now(),
                );
                return;
            }
        };
        let id = self.new_tab_id();
        let bytes = Arc::new(AtomicU64::new(0));
        let settings = &self.state.settings;
        let mut tab = Tab::new(
            id,
            "stdin".to_owned(),
            tmp.path().to_path_buf(),
            Phase::Spooling {
                bytes: Arc::clone(&bytes),
            },
            settings.freeze,
            settings.max_column_width,
        );
        tab.temp = Some(tmp);
        tab.nulls = NullSet::new(&settings.null_values);
        tab.spool_rate.push(now(), 0);
        let cancel = tab.cancel.child_token();
        self.state.tabs.push(tab);

        let tx = self.msg_tx.clone();
        let reading = Arc::clone(&self.stdin_reading);
        reading.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            let result = spool_stdin(tokio::io::stdin(), file, bytes, cancel)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            reading.store(false, Ordering::Relaxed);
            let _ = tx.send(Msg::SpoolDone { tab: id, result });
        });
    }

    /// Starts the indexer of tab `idx` for its current generation (M2-02).
    fn spawn_indexer(&mut self, idx: usize) {
        let exec = self.executor();
        let memory = self.state.settings.memory;
        let Some(tab) = self.state.tabs.get_mut(idx) else {
            return;
        };
        let Some(l) = tab.loaded.as_mut() else {
            return;
        };
        let token = tab.cancel.child_token();
        l.index_cancel = token.clone();
        l.rate.push(now(), l.index.bytes_scanned());
        // Shown in the jobs drawer while it runs (M5-02); progress is copied
        // from the index each frame (`sync_index_progress`).
        let total = l.source.len().saturating_sub(l.source.data_start());
        let jobs = &mut self.state.jobs;
        jobs.end_index(tab.id);
        jobs.register_index(
            tab.id,
            tab.name.clone(),
            Progress::for_kind(JobKind::Index, total),
            token.clone(),
        );
        let (src, index) = (Arc::clone(&l.source), Arc::clone(&l.index));
        let report = l.sniff.clone();
        let (id, generation) = (tab.id, tab.generation);
        let opts = IndexOptions {
            memory_budget: memory,
            ..IndexOptions::default()
        };
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let msg = match build_index(src, index, report, exec, token, opts).await {
                Ok(summary) => Msg::IndexReady {
                    tab: id,
                    generation,
                    summary,
                },
                Err(IndexError::Cancelled) => return,
                Err(e) => Msg::IndexFailed {
                    tab: id,
                    generation,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(msg);
        });
    }

    /// Starts the phase-1 sample (the first 10,000 rows) of tab `idx` for its
    /// current generation (M3-01). Runs off the UI task; the result comes
    /// back as `Msg::SampleReady`.
    fn spawn_sample_head(&mut self, idx: usize) {
        let exec = self.executor();
        let Some(tab) = self.state.tabs.get_mut(idx) else {
            return;
        };
        let (id, generation, nulls) = (tab.id, tab.generation, tab.nulls.clone());
        let Some(l) = tab.loaded.as_mut() else {
            return;
        };
        let token = tab.cancel.child_token();
        l.sample_cancel = token.clone();
        l.spread_started = false;
        let src = Arc::clone(&l.source);
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            match sample_head(src, &exec, nulls, token).await {
                Ok(sample) => {
                    let _ = tx.send(Msg::SampleReady {
                        tab: id,
                        generation,
                        sample: Arc::new(sample),
                    });
                }
                Err(e) => debug!("sample of {id:?} stopped: {e}"),
            }
        });
    }

    /// Starts the phase-2 sample (10,000 rows spread across the file) once
    /// the index is complete and the phase-1 sample is in (M3-01). Whichever
    /// comes last starts it.
    fn maybe_spawn_spread(&mut self, idx: usize) {
        let exec = self.executor();
        let Some(tab) = self.state.tabs.get_mut(idx) else {
            return;
        };
        let (id, generation, nulls) = (tab.id, tab.generation, tab.nulls.clone());
        let Some(l) = tab.loaded.as_mut() else {
            return;
        };
        // Phase 2 extends the unedited head sample.
        let Some(head) = l.sample_raw.clone() else {
            return;
        };
        if l.spread_started
            || head.phase != SamplePhase::Head
            || head.reached_eof
            || !l.index.is_complete()
            || l.index_error.is_some()
        {
            return;
        }
        l.spread_started = true;
        let (src, index) = (Arc::clone(&l.source), Arc::clone(&l.index));
        let token = l.sample_cancel.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            match sample_spread(src, index, &exec, nulls, head, token).await {
                Ok(sample) => {
                    let _ = tx.send(Msg::SampleReady {
                        tab: id,
                        generation,
                        sample: Arc::new(sample),
                    });
                }
                Err(e) => debug!("spread sample of {id:?} stopped: {e}"),
            }
        });
    }

    /// Re-indexes tab `id` with another dialect (M2-04 dialog, M7-03
    /// reload): see `Tab::apply_dialect`. Restarts the indexer and the
    /// sample for the new generation.
    pub fn restart_indexing(&mut self, id: TabId, dialect: Dialect) {
        let Some(idx) = self.tab_index(id) else {
            return;
        };
        let sample_bytes = self.state.settings.sniff_sample_bytes;
        // Filters, sorts and profiles are invalid under a new dialect; an
        // export reads its own `Source` and may finish.
        self.state.jobs.cancel_tab(id, true);
        self.state.tabs[idx].apply_dialect(dialect, sample_bytes);
        self.forget_search(id);
        self.spawn_indexer(idx);
        self.spawn_sample_head(idx);
        self.dirty = true;
    }

    /// Removes tab `idx` (dropping it cancels its work and deletes its temp
    /// files) and activates the tab to the right, or the left one if it was
    /// the last.
    fn remove_tab(&mut self, idx: usize) {
        if idx >= self.state.tabs.len() {
            return;
        }
        let id = self.state.tabs[idx].id;
        self.state.jobs.cancel_tab(id, false);
        self.state.tabs.remove(idx);
        self.update_sigbus_message();
        let active = &mut self.state.active_tab;
        if idx < *active {
            *active -= 1;
        }
        *active = (*active).min(self.state.tabs.len().saturating_sub(1));
    }

    /// One turn of the event loop: wait for something to happen, handle it
    /// and everything already queued behind it, then draw if needed.
    ///
    /// Idle (no input, no work) this blocks on the terminal event channel
    /// alone: no tick, no frame timer.
    async fn step<B: Backend>(&mut self, tui: &mut Tui<B>) -> color_eyre::Result<()> {
        // A pending frame that the 60 fps cap held back.
        let frame_at = self
            .dirty
            .then(|| self.last_draw.map_or_else(Instant::now, |t| t + FRAME_MIN));
        tokio::select! {
            Some(event) = tui.next_event() => {
                self.handle_event(event)?;
                self.handle_actions(tui)?;
            }
            Some(msg) = self.msg_rx.recv() => self.handle_msg(msg),
            Some(action) = self.action_rx.recv() => self.handle_action(tui, action)?,
            () = sleep_until(frame_at.unwrap_or_else(Instant::now)), if frame_at.is_some() => {}
        }
        // Coalesce bursts: handle everything already queued before drawing.
        // Each event's actions run before the next event is read, so a key
        // is resolved in the context the previous keys left (`g40⏎` typed
        // fast, or `Enter` closing a dialog then `>`).
        while let Ok(event) = tui.event_rx.try_recv() {
            self.handle_event(event)?;
            self.handle_actions(tui)?;
        }
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.handle_msg(msg);
        }
        self.handle_actions(tui)?;
        tui.set_ticking(self.state.has_running_work());

        let due = self.last_draw.is_none_or(|t| t.elapsed() >= FRAME_MIN);
        if self.dirty && due {
            self.render(tui)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> color_eyre::Result<()> {
        let action_tx = self.action_tx.clone();
        match event {
            Event::Init => self.dirty = true,
            Event::Quit => action_tx.send(Action::Quit)?,
            Event::Tick => {
                // Progress counters change between ticks.
                self.dirty = true;
                action_tx.send(Action::Tick)?;
            }
            Event::Resize(x, y) => action_tx.send(Action::Resize(x, y))?,
            Event::Key(key) => {
                self.dirty = true;
                // Keys are routed by context, not broadcast.
                return self.handle_key_event(key);
            }
            Event::Mouse(mouse) => {
                self.dirty = true;
                self.handle_mouse(mouse);
            }
            Event::Paste(ref text) => {
                self.dirty = true;
                if let Some(prompt) = self.state.prompt.as_mut() {
                    prompt.input.insert(text);
                } else if self.state.find.bar.is_some() {
                    self.bar_paste(text);
                }
            }
            Event::Error | Event::Closed | Event::FocusGained | Event::FocusLost => {}
        }
        for component in self.ui.all_mut() {
            if let Some(action) = component.handle_events(Some(event.clone()), &self.state)? {
                action_tx.send(action)?;
            }
        }
        Ok(())
    }

    /// Mouse wheel scrolls the table by 3 rows (§1). Not bindable.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.state.overlay.is_some() {
            return;
        }
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => WHEEL_ROWS,
            MouseEventKind::ScrollUp => -WHEEL_ROWS,
            _ => return,
        };
        let viewport = self.viewport();
        if let Some(tab) = self.state.active_tab_mut() {
            tab.scroll(delta, viewport);
        }
    }

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Toast(toast) => self.state.toasts.push(toast, now()),
            Msg::SourceOpened {
                tab,
                generation,
                result,
            } => self.source_opened(tab, generation, result),
            Msg::Job(JobMsg::Finished { id, result }) => self.job_finished(id, result),
            Msg::SpoolDone { tab, result } => self.spool_done(tab, result),
            Msg::IndexReady {
                tab,
                generation,
                summary,
            } => {
                if let Some(t) = self.current_tab(tab, generation)
                    && let Some(l) = t.loaded.as_mut()
                {
                    info!(?summary, "index ready");
                    l.index_summary = Some(summary);
                    self.state.jobs.end_index(tab);
                    self.poll_jumps();
                    if let Some(idx) = self.tab_index(tab) {
                        self.maybe_spawn_spread(idx);
                    }
                }
            }
            Msg::SampleReady {
                tab,
                generation,
                sample,
            } => {
                if let Some(t) = self.current_tab(tab, generation) {
                    debug!(rows = sample.rows_sampled, phase = ?sample.phase, "sample ready");
                    t.apply_sample(sample);
                    if let Some(idx) = self.tab_index(tab) {
                        self.maybe_spawn_spread(idx);
                    }
                }
            }
            Msg::IndexFailed {
                tab,
                generation,
                error,
            } => {
                if let Some(t) = self.current_tab(tab, generation)
                    && let Some(l) = t.loaded.as_mut()
                {
                    l.index_error = Some(error.clone());
                    let text = format!("indexing {} failed: {error}", t.name);
                    self.state.jobs.end_index(tab);
                    self.state
                        .toasts
                        .push(ToastMsg::new(ToastLevel::Error, text), now());
                }
            }
            Msg::PathCompletions { input, result } => self.path_completions(&input, result),
            Msg::SearchDone {
                tab,
                request,
                result,
            } => self.search_done(tab, request, result),
            // A sticky status-line warning until `R` (M7-03).
            Msg::FileChanged { tab, change } => {
                if let Some(t) = self.state.tabs.iter_mut().find(|t| t.id == tab) {
                    t.file_changed = Some(change);
                }
            }
            Msg::ViewsWritten {
                store,
                result,
                done,
            } => self.views_written(*store, result, done),
        }
        self.after_change();
        self.dirty = true;
    }

    /// Tab `id`, if it still exists and is at `generation`: older index
    /// messages are stale and dropped (M0-02 rule).
    fn current_tab(&mut self, id: TabId, generation: u64) -> Option<&mut Tab> {
        self.state
            .tabs
            .iter_mut()
            .find(|t| t.id == id && t.generation == generation)
    }

    fn source_opened(
        &mut self,
        id: TabId,
        generation: u64,
        result: Result<Box<Opened>, OpenFailure>,
    ) {
        // The tab may have been closed (or reloaded again) meanwhile: the
        // result is dropped (and its temp file with it).
        let Some(idx) = self.tab_index(id) else {
            return;
        };
        if self.state.tabs[idx].generation != generation {
            return;
        }
        let from_prompt = self
            .state
            .prompt
            .as_ref()
            .is_some_and(|p| p.pending == Some(id));
        match result {
            Ok(opened) => {
                let stamp = opened.stamp;
                let viewport = self.viewport();
                let tab = &mut self.state.tabs[idx];
                tab.set_loaded(*opened);
                // After `R`: the same record and column (M7-03).
                tab.apply_restore(viewport);
                self.spawn_watcher(idx, stamp);
                self.spawn_indexer(idx);
                self.spawn_sample_head(idx);
                if from_prompt {
                    self.state.prompt = None;
                }
            }
            Err(failure) => {
                self.remove_tab(idx);
                match self.state.prompt.as_mut() {
                    Some(prompt) if from_prompt => {
                        prompt.pending = None;
                        prompt.message = PromptMessage::Error(failure.text());
                    }
                    _ => self.state.toasts.push(failure.toast(), now()),
                }
            }
        }
    }

    fn spool_done(&mut self, id: TabId, result: Result<(), String>) {
        let Some(idx) = self.tab_index(id) else {
            return;
        };
        match result {
            Ok(()) => {
                let tab = &mut self.state.tabs[idx];
                let progress = Arc::new(OpenProgress::default());
                tab.phase = Phase::Opening {
                    progress: Arc::clone(&progress),
                };
                let settings = &self.state.settings;
                let request = OpenRequest {
                    path: tab.path.clone(),
                    display_name: Some("stdin".to_owned()),
                    sample_bytes: settings.sniff_sample_bytes,
                    overrides: settings.dialect_overrides,
                    tmp_dir: settings.tmp_dir.clone(),
                    progress,
                    cancel: tab.cancel.child_token(),
                };
                let generation = tab.generation;
                self.spawn_open(id, generation, request);
            }
            Err(e) => {
                self.remove_tab(idx);
                self.state.toasts.push(
                    ToastMsg::new(ToastLevel::Error, format!("cannot read stdin: {e}")),
                    now(),
                );
            }
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> color_eyre::Result<()> {
        let context = self.state.key_context();
        // The column chooser's filter input takes keys before its bindings.
        if context == KeyContext::ColumnChooser && self.chooser_filter_key(key) {
            return Ok(());
        }
        let chord = KeyChord::from(key);
        let keymap = &self.config.keybindings;
        let action = keymap.resolve(context, chord).or_else(|| match context {
            // Side panels fall back to Normal: `q`, `?`, `:` still work, but
            // the panel's own keys (`j`/`k`) win.
            KeyContext::Inspector | KeyContext::JobsDrawer => {
                keymap.resolve(KeyContext::Normal, chord)
            }
            _ => None,
        });
        if let Some(action) = action {
            info!("Got action: {action:?}");
            self.action_tx.send(action.clone())?;
            return Ok(());
        }
        // Unbound in a text-input context: it's an edit.
        let forwarded = match context {
            KeyContext::Prompt => {
                if let Some(prompt) = self.state.prompt.as_mut()
                    && prompt.input.handle_key(key)
                    && prompt.message != PromptMessage::Opening
                {
                    prompt.message = PromptMessage::None;
                }
                None
            }
            KeyContext::Filter | KeyContext::Search => {
                self.bar_key(key);
                None
            }
            KeyContext::Command => self.ui.palette.handle_key_event(key, &self.state)?,
            KeyContext::Goto => {
                if self.state.goto.input.handle_key(key) {
                    self.state.goto.error = None;
                }
                None
            }
            KeyContext::Export => {
                self.export_key(key);
                None
            }
            // Anything else unbound is ignored.
            _ => None,
        };
        if let Some(action) = forwarded {
            self.action_tx.send(action)?;
        }
        Ok(())
    }

    fn handle_actions<B: Backend>(&mut self, tui: &mut Tui<B>) -> color_eyre::Result<()> {
        while let Ok(action) = self.action_rx.try_recv() {
            self.handle_action(tui, action)?;
        }
        Ok(())
    }

    /// Body height and table width for the current terminal size and state.
    fn viewport(&self) -> Viewport {
        let layout = compute_layout(self.area, &self.state.layout_input());
        Viewport {
            body_height: layout.table_body.height,
            table_width: layout.table_body.width,
        }
    }

    fn handle_action<B: Backend>(
        &mut self,
        tui: &mut Tui<B>,
        action: Action,
    ) -> color_eyre::Result<()> {
        if action != Action::Tick && action != Action::Render {
            debug!("{action:?}");
            // Conservative: assume any other action changes something visible.
            self.dirty = true;
        }
        let viewport = self.viewport();
        let tab_count = self.state.tabs.len();
        let in_prompt = self.state.prompt.is_some();
        match action {
            Action::Tick => {
                let t = now();
                self.state.toasts.tick(t);
                for tab in &mut self.state.tabs {
                    sample_progress(tab, t);
                    if let Some(jump) = tab.pending_jump.as_mut() {
                        jump.frame += 1;
                    }
                }
                self.poll_jumps();
                self.apply_due_dialect(t);
                self.find_tick(t);
                self.state.ticks += 1;
            }
            Action::Render => self.dirty = true,
            // A confirm dialog decides (M5-01); `q` inside it is ignored.
            Action::Quit if self.state.dialog() == Some(DialogKind::Confirm) => {}
            Action::Quit => self.request_quit(),
            Action::Suspend => self.should_suspend = true,
            Action::Resume => self.should_suspend = false,
            Action::ClearScreen => tui.terminal.clear().map_err(|e| eyre!("{e}"))?,
            // Re-layout happens on the next frame (§16); the cursor stays
            // visible.
            Action::Resize(w, h) => {
                tui.resize(Rect::new(0, 0, w, h))
                    .map_err(|e| eyre!("{e}"))?;
                self.area = Rect::new(0, 0, w, h);
                let viewport = self.viewport();
                if let Some(tab) = self.state.active_tab_mut() {
                    tab.clamp(viewport);
                }
                if let Some(popup) = self.state.value_popup.as_mut() {
                    popup.rewrap(self.area);
                }
            }
            Action::Error(ref err) => {
                tracing::error!(?err)
            }
            // ---- dialogs: every other action goes to the open one ----
            ref a if self.state.dialog() == Some(DialogKind::Confirm) => self.confirm_action(a),
            ref a if self.state.dialog().is_some() => self.dialog_action(a, viewport),
            // ---- help (M7-02): `?` toggles it, `Esc` closes it; while it
            // is open the Help component scrolls (in `update`, below) ----
            Action::Help if self.state.overlay == Some(Overlay::Help) => {
                crate::components::help::close(&mut self.state);
            }
            Action::Help => crate::components::help::open(&mut self.state),
            Action::Cancel if self.state.overlay == Some(Overlay::Help) => {
                crate::components::help::close(&mut self.state);
            }
            _ if self.state.overlay == Some(Overlay::Help) => {}
            // ---- command palette (M6-02): `↑`/`↓`/`Tab` are handled by the
            // Palette component (`update`, below) ----
            Action::Submit if self.state.overlay == Some(Overlay::Palette) => {
                self.palette_submit();
            }
            Action::Cancel if self.state.overlay == Some(Overlay::Palette) => {
                self.close_palette();
            }
            _ if self.state.overlay == Some(Overlay::Palette) => {}
            Action::CommandPalette => self.open_palette(),
            Action::Dismiss => self.dismiss(),

            // ---- tabs (M1-08) ----
            ref a if a.tab_number().is_some() => {
                let n = a.tab_number().unwrap_or_default();
                if n < tab_count {
                    self.state.active_tab = n;
                }
            }
            Action::OpenFile => {
                if self.state.overlay.is_none() {
                    self.state.prompt = Some(OpenPrompt::default());
                }
            }
            Action::CloseTab => self.close_tab(),

            // ---- the `open ›` prompt ----
            Action::Submit if in_prompt => self.submit_prompt(),
            Action::Cancel if in_prompt => self.state.prompt = None,
            Action::Complete if in_prompt => self.complete_prompt(),

            // ---- the filter and search bars, `n` / `N` (M4-04, M4-05) ----
            ref a if self.find_action(a) => {}

            // ---- dialogs, panels, columns (M2-03, M3-03, M3-04) ----
            Action::Goto => self.open_goto(),
            Action::ColumnChooser => self.open_chooser(),
            Action::Export => self.open_export(),
            Action::SortAsc if self.state.focus == Focus::Table => self.sort_cursor_column(false),
            Action::SortDesc if self.state.focus == Focus::Table => self.sort_cursor_column(true),
            Action::ToggleInspector => {
                self.state.inspector_visible = !self.state.inspector_visible;
            }
            Action::FocusNext => self.focus_next(),
            Action::SelectNext | Action::SelectPrev | Action::OpenValue | Action::Cancel
                if self.state.focus == Focus::Inspector =>
            {
                self.inspector_action(&action);
            }
            Action::Cancel if self.state.focus == Focus::JobsDrawer => {
                self.state.focus = Focus::Table;
            }
            // ---- jobs drawer (M5-02) ----
            Action::ToggleJobs => {
                self.state.jobs_drawer_open = !self.state.jobs_drawer_open;
            }
            Action::SelectNext
            | Action::SelectPrev
            | Action::PauseJob
            | Action::KillJob
            | Action::DismissJob
                if self.state.focus == Focus::JobsDrawer =>
            {
                self.drawer_action(&action);
            }
            // ---- views (M4-03), clipboard (M6-05), reload (M7-03) ----
            Action::PopView if self.state.focus == Focus::Table => self.pop_view(viewport),
            Action::JumpToSource if self.state.focus == Focus::Table => {
                if let Some(tab) = self.state.active_tab_mut() {
                    tab.jump_to_source(viewport);
                }
            }
            Action::CopyCell => self.copy(false),
            Action::CopyRow => self.copy(true),
            Action::Reload => self.reload_active(),
            Action::ShrinkCol | Action::GrowCol | Action::AutofitCol
                if self.state.focus == Focus::Table =>
            {
                if let Some(tab) = self.state.active_tab_mut() {
                    match action {
                        Action::ShrinkCol => tab.shrink_col(viewport),
                        Action::GrowCol => tab.grow_col(viewport),
                        _ => tab.autofit_col(viewport),
                    }
                }
            }

            // ---- navigation (M1-06). Moving cancels a pending jump (D9);
            // a focused side panel keeps the table cursor still (M3-03). ----
            ref nav => {
                if self.state.focus == Focus::Table
                    && let Some(tab) = self.state.active_tab_mut()
                {
                    if is_navigation(nav) {
                        tab.pending_jump = None;
                    }
                    navigate(tab, nav, viewport);
                }
            }
        }
        self.after_change();
        for component in self.ui.all_mut() {
            if let Some(action) = component.update(&action, &mut self.state)? {
                self.action_tx.send(action)?
            };
        }
        Ok(())
    }

    /// `Ctrl-w`: closes the active tab. Closing the last tab quits. With
    /// jobs running in the tab it asks first (M5-01).
    fn close_tab(&mut self) {
        let Some(id) = self.state.active_tab().map(|t| t.id) else {
            return;
        };
        let n = self.state.jobs.pending_count(Some(id));
        if n > 0 {
            self.open_confirm(ConfirmState::jobs_running(
                n,
                "close tab",
                ConfirmAction::CloseTab(id),
            ));
            return;
        }
        self.close_tab_now(id);
    }

    /// Closes tab `id` without asking; the last tab quits.
    fn close_tab_now(&mut self, id: TabId) {
        match (self.state.tabs.len(), self.tab_index(id)) {
            (1, Some(_)) => self.should_quit = true,
            (_, Some(idx)) => self.remove_tab(idx),
            _ => {}
        }
    }

    /// `Enter` in the `open ›` prompt: opens the path in a new, active tab.
    /// The prompt stays open until the open succeeds; an error shows on
    /// its second line.
    fn submit_prompt(&mut self) {
        let Some(prompt) = self.state.prompt.as_ref() else {
            return;
        };
        let input = prompt.input.text().trim().to_owned();
        if input.is_empty() || prompt.pending.is_some() {
            return;
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        let home = path_complete::home_dir();
        let path = path_complete::resolve(&input, &cwd, home.as_deref());
        let id = self.open_path(path, None);
        self.state.active_tab = self.state.tabs.len() - 1;
        if let Some(prompt) = self.state.prompt.as_mut() {
            prompt.pending = Some(id);
            prompt.message = PromptMessage::Opening;
            prompt.cycle = None;
        }
    }

    /// `Tab` in the `open ›` prompt: cycles when nothing was typed since the
    /// last completion, otherwise lists the directory off the UI task with
    /// a timeout (`Msg::PathCompletions`).
    fn complete_prompt(&mut self) {
        let Some(prompt) = self.state.prompt.as_mut() else {
            return;
        };
        if let Some(cycle) = prompt.cycle.as_mut()
            && cycle.continues(prompt.input.text())
        {
            let next = cycle.advance();
            prompt.input.set(next);
            return;
        }
        prompt.cycle = None;
        let input = prompt.input.text().to_owned();
        let cwd = std::env::current_dir().unwrap_or_default();
        let home = path_complete::home_dir();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let listing = tokio::task::spawn_blocking({
                let input = input.clone();
                move || path_complete::list_candidates(&input, &cwd, home.as_deref())
            });
            let result = match tokio::time::timeout(COMPLETION_TIMEOUT, listing).await {
                Ok(Ok(Ok(candidates))) => Ok(candidates),
                Ok(Ok(Err(e))) => Err(e.to_string()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("directory listing timed out".to_owned()),
            };
            let _ = tx.send(Msg::PathCompletions { input, result });
        });
    }

    fn path_completions(&mut self, input: &str, result: Result<Vec<Candidate>, String>) {
        // The Export dialog's path field (M6-01) asked for it.
        let result = match self.export_completions(input, result) {
            Some(result) => result,
            None => return,
        };
        let Some(prompt) = self.state.prompt.as_mut() else {
            return;
        };
        if prompt.input.text() != input {
            return; // Typed on since: stale.
        }
        match result {
            Ok(candidates) => {
                let many = candidates.len() > 1;
                match path_complete::complete(input, candidates.clone()) {
                    None => prompt.message = PromptMessage::Candidates(Vec::new()),
                    Some(completion) => {
                        prompt.input.set(completion.input);
                        prompt.cycle = completion.cycle;
                        prompt.message = if many {
                            PromptMessage::Candidates(candidates)
                        } else {
                            PromptMessage::None
                        };
                    }
                }
            }
            Err(e) => prompt.message = PromptMessage::Error(e),
        }
    }

    /// `Esc` in Normal mode (M1-06 `Dismiss`): the first of these that
    /// applies. Other keys never dismiss toasts, so no input is swallowed.
    fn dismiss(&mut self) {
        if self.state.mode != Mode::Normal {
            return;
        }
        // A running filter scan on the active view first (§13).
        if let Some(id) = self.running_filter_job() {
            self.kill_job(id, false);
            return;
        }
        if self.state.toasts.dismiss(now()) {
            return;
        }
        // Then a pending jump (M2-03).
        let cancelled_jump = self
            .state
            .active_tab_mut()
            .is_some_and(|tab| tab.pending_jump.take().is_some());
        // Then a search in flight and the search highlight (M4-05).
        if !cancelled_jump {
            self.dismiss_search();
        }
    }

    fn render<B: Backend>(&mut self, tui: &mut Tui<B>) -> color_eyre::Result<()> {
        tui.draw(|frame| self.draw_frame(frame))
            .map_err(|e| eyre!("{e}"))?;
        // OSC 52 after the frame, on the same stdout (M6-05).
        if !self.pending_osc.is_empty() {
            let seq = std::mem::take(&mut self.pending_osc);
            #[cfg(not(test))]
            crate::clipboard::write_sequence(&mut std::io::stdout(), &seq)?;
            #[cfg(test)]
            self.osc_written.extend_from_slice(&seq);
        }
        self.dirty = false;
        self.last_draw = Some(Instant::now());
        #[cfg(test)]
        {
            self.renders += 1;
        }
        Ok(())
    }

    /// Draws every region into its own rect (§11.1), then the overlays.
    ///
    /// Order (z-order): base regions → backdrop (if a modal is open) →
    /// dialog / palette / help → toast.
    fn draw_frame(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.area = area;
        let layout = compute_layout(area, &self.state.layout_input());
        if layout.too_small {
            render_too_small(frame.buffer_mut(), area, &self.state.theme);
            return;
        }
        self.sync_index_progress();
        // Parse the visible rows before drawing: `draw` never parses (M1-05).
        let viewport = Viewport {
            body_height: layout.table_body.height,
            table_width: layout.table_body.width,
        };
        if let Some(tab) = self.state.active_tab_mut() {
            tab.prepare_frame(viewport);
        }
        let state = &self.state;
        let theme = &state.theme;
        frame.buffer_mut().set_style(area, theme.base());

        let ui = &mut self.ui;
        let mut results = vec![
            ui.top_bar.draw(frame, layout.top_bar, state),
            ui.table.draw(frame, layout.table(), state),
            ui.status.draw(frame, layout.status, state),
        ];
        if let Some(rect) = layout.query_bar {
            results.push(ui.query_bar.draw(frame, rect, state));
        }
        if let (Some(rect), Some(divider)) = (layout.inspector, layout.inspector_divider) {
            render_vertical_divider(frame.buffer_mut(), divider, theme.column_divider());
            results.push(ui.inspector.draw(frame, rect, state));
        }
        if let Some(rect) = layout.jobs_drawer {
            results.push(ui.jobs_drawer.draw(frame, rect, state));
        }
        if let Some(rect) = layout.hints {
            results.push(ui.hints.draw(frame, rect, state));
        }
        if let Some(overlay) = state.overlay {
            apply_backdrop(frame.buffer_mut(), area, theme);
            results.push(match overlay {
                Overlay::Dialog(_) => ui.dialogs.draw(frame, area, state),
                Overlay::Palette => ui.palette.draw(frame, palette_rect(area), state),
                Overlay::Help => ui.help.draw(frame, area, state),
            });
        }
        if let Some(rect) = layout.toast {
            results.push(ui.toast.draw(frame, rect, state));
        }

        for err in results.into_iter().filter_map(Result::err) {
            let _ = self
                .action_tx
                .send(Action::Error(format!("Failed to draw: {err:?}")));
        }
    }
}

/// Whether `action` moves the table cursor (and so cancels a pending jump).
fn is_navigation(action: &Action) -> bool {
    matches!(
        action,
        Action::MoveDown
            | Action::MoveUp
            | Action::MoveLeft
            | Action::MoveRight
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::PageDown
            | Action::PageUp
            | Action::FirstRow
            | Action::LastRow
            | Action::FirstCol
            | Action::LastCol
            | Action::NextCol
            | Action::PrevCol
    )
}

/// The navigation actions of M1-06 on `tab`. Other actions are ignored.
fn navigate(tab: &mut Tab, action: &Action, viewport: Viewport) {
    let h = i64::from(viewport.body_height.max(1));
    match action {
        Action::MoveDown => tab.move_rows(1, viewport),
        Action::MoveUp => tab.move_rows(-1, viewport),
        Action::MoveLeft => tab.move_cols(-1, viewport),
        Action::MoveRight => tab.move_cols(1, viewport),
        Action::HalfPageDown => tab.page((h / 2).max(1), viewport),
        Action::HalfPageUp => tab.page(-(h / 2).max(1), viewport),
        Action::PageDown => tab.page(h, viewport),
        Action::PageUp => tab.page(-h, viewport),
        Action::FirstRow => tab.first_row(viewport),
        Action::LastRow => tab.last_row(viewport),
        Action::FirstCol => tab.first_col(viewport),
        Action::LastCol => tab.last_col(viewport),
        Action::NextCol => tab.next_col(viewport),
        Action::PrevCol => tab.prev_col(viewport),
        _ => {}
    }
}

mod actions;
mod find;
mod jobs_views;
mod palette;
mod starters;
#[cfg(test)]
mod tests;
mod views;
