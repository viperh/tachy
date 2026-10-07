//! Shared application state, owned by `App` (spec §4.1, §4.2, README §A3).
//!
//! Only the UI task mutates it. Components get `&AppState` to handle keys and
//! draw, and `&mut AppState` in `update`.

use crate::{
    components::{
        dialogs::{
            column_chooser::ColumnChooserState, confirm::ConfirmState,
            detected_format::DetectedFormatState, export::ExportForm, goto::GotoDialog,
            value_popup::ValuePopupState,
        },
        layout::LayoutInput,
    },
    input::LineInput,
    jobs::JobManager,
    mode::{KeyContext, Mode},
    path_complete::{Candidate, Cycle},
    settings::Settings,
    tab::{Tab, TabId},
    theme::{ColorSupport, Theme},
    toast::ToastQueue,
};

/// Which panel receives keys (M3-03, M5-02).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Table,
    Inspector,
    JobsDrawer,
}

/// The dialogs of §11.7 (and the small ones later tasks add).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    DetectedFormat,
    Goto,
    ColumnChooser,
    Export,
    ValuePopup,
    Confirm,
}

/// The modal drawn over the base regions, behind a dimmed backdrop (§11.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Dialog(DialogKind),
    Palette,
    Help,
}

/// What the `open ›` prompt shows on its second line (M1-08).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PromptMessage {
    #[default]
    None,
    /// Completion candidates (the first [`crate::path_complete::SHOWN_CANDIDATES`]
    /// are listed, then `+N more`).
    Candidates(Vec<Candidate>),
    /// The file is being opened.
    Opening,
    /// An open or completion error, shown in `inline_error`.
    Error(String),
}

/// The `Ctrl-o` prompt in the query bar region (M1-08, D8).
#[derive(Debug, Clone, Default)]
pub struct OpenPrompt {
    pub input: LineInput,
    pub message: PromptMessage,
    /// Cycling through several matches with repeated `Tab`s.
    pub cycle: Option<Cycle>,
    /// The tab opened from this prompt that hasn't finished opening: its
    /// error shows here instead of as a toast.
    pub pending: Option<TabId>,
}

/// Everything the UI shows. Later tasks add their fields.
#[derive(Debug)]
pub struct AppState {
    /// Effective settings (M0-01).
    pub settings: Settings,
    /// The theme, already adapted to the terminal (M0-03, M7-01).
    pub theme: Theme,
    pub mode: Mode,
    /// The mode shown in the pill while `mode == Dialog` (§12.1).
    pub pill_mode: Mode,
    pub focus: Focus,
    /// Open tabs, in top-bar order. May be empty (D4).
    pub tabs: Vec<Tab>,
    /// Index into `tabs`; meaningless while `tabs` is empty.
    pub active_tab: usize,
    /// The `Ctrl-o` prompt, when open (D8).
    pub prompt: Option<OpenPrompt>,
    /// Inspector toggled on (`i`). It is still hidden below 120 columns (§11.1).
    pub inspector_visible: bool,
    /// Key-hint line shown (`:set hints`).
    pub hints_visible: bool,
    pub jobs_drawer_open: bool,
    /// The open modal, if any.
    pub overlay: Option<Overlay>,
    pub toasts: ToastQueue,
    /// The `g` dialog's input (M2-03).
    pub goto: GotoDialog,
    /// The Detected format dialog (M2-04), while open.
    pub detected: Option<DetectedFormatState>,
    /// The column chooser's working copy (M3-04), while open.
    pub chooser: Option<ColumnChooserState>,
    /// The full-value popup (M3-03), while open.
    pub value_popup: Option<ValuePopupState>,
    /// The Export dialog's form (M6-01), while open.
    pub export: Option<ExportForm>,
    /// The inspector's RECORD selection (M3-03).
    pub inspector: InspectorState,
    /// The filter and search bars, histories and the search (M4-04, M4-05).
    pub find: crate::search_ui::FindState,
    /// Saved views: `views.json` plus the config's read-only `views` (M6-04).
    pub views: crate::views_store::ViewsStore,
    /// Every job of every tab (M5-01).
    pub jobs: JobManager,
    /// The jobs drawer's selected line: an index into
    /// `jobs.drawer_order()` (M5-02).
    pub jobs_selected: usize,
    /// The open `y`/`n` confirm dialog (M5-01).
    pub confirm: Option<ConfirmState>,
    /// 100 ms ticks seen, for animations (the indeterminate gauge).
    pub ticks: u64,
}

/// Local state of the inspector's RECORD section (M3-03).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InspectorState {
    /// Selected field: an index into the tab's columns (source order). The
    /// scroll offset is the component's own.
    pub selected: usize,
}

impl AppState {
    /// `color` is detected once at startup (`ColorSupport::detect`, M7-01);
    /// the theme is adapted here, never per draw.
    pub fn new(settings: Settings, color: ColorSupport) -> Self {
        // `--theme` and the config are validated against `Theme::NAMES`.
        let theme = Theme::by_name(&settings.theme)
            .unwrap_or_default()
            .adapt(color);
        let jobs = JobManager::new(settings.memory, settings.tmp_dir.clone());
        Self {
            theme,
            mode: Mode::Normal,
            pill_mode: Mode::Normal,
            focus: Focus::Table,
            tabs: Vec::new(),
            active_tab: 0,
            prompt: None,
            inspector_visible: settings.inspector,
            hints_visible: settings.hints,
            jobs_drawer_open: false,
            overlay: None,
            toasts: ToastQueue::default(),
            goto: GotoDialog::default(),
            detected: None,
            chooser: None,
            value_popup: None,
            export: None,
            inspector: InspectorState::default(),
            find: crate::search_ui::FindState::default(),
            views: crate::views_store::ViewsStore::empty(&crate::config::get_config_dir()),
            jobs,
            jobs_selected: 0,
            confirm: None,
            ticks: 0,
            settings,
        }
    }

    /// Whether the 100 ms tick must run: something on screen changes without
    /// input (§4.1).
    pub fn has_running_work(&self) -> bool {
        // The search spinner and a debounced query validation (M4-04,
        // M4-05).
        // A running job moves its gauge (M5-01). A visible toast expires on a tick (M1-09); opening, spooling and
        // indexing move gauges (M1-08, M2-02); a pending jump spins and
        // waits (M2-03); a Detected format edit applies after its debounce
        // (M2-04).
        !self.toasts.is_empty()
            || self.jobs.has_running()
            || self.tabs.iter().any(Tab::busy)
            || self.detected.as_ref().is_some_and(|d| d.apply_at.is_some())
            || self.find.busy()
    }

    /// Opens a dialog: the pill keeps showing the current mode (§12.1).
    pub fn open_dialog(&mut self, kind: DialogKind) {
        if self.mode != Mode::Dialog {
            self.pill_mode = self.mode;
        }
        self.mode = Mode::Dialog;
        self.overlay = Some(Overlay::Dialog(kind));
    }

    /// Closes the open dialog and restores the mode it was opened from.
    pub fn close_dialog(&mut self) {
        if matches!(self.overlay, Some(Overlay::Dialog(_))) {
            self.overlay = None;
        }
        if self.mode == Mode::Dialog {
            self.mode = self.pill_mode;
        }
    }

    /// The open dialog, if any.
    pub fn dialog(&self) -> Option<DialogKind> {
        match self.overlay {
            Some(Overlay::Dialog(kind)) => Some(kind),
            _ => None,
        }
    }

    /// The active tab, if any.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    /// The key map a key press is looked up in (M1-06): open overlay
    /// (dialog kind, palette, help) > prompt > mode > focus > Normal.
    pub fn key_context(&self) -> KeyContext {
        match self.overlay {
            Some(Overlay::Dialog(kind)) => {
                return match kind {
                    DialogKind::DetectedFormat => KeyContext::DetectedFormat,
                    DialogKind::Goto => KeyContext::Goto,
                    DialogKind::ColumnChooser => KeyContext::ColumnChooser,
                    DialogKind::Export => KeyContext::Export,
                    DialogKind::ValuePopup => KeyContext::ValuePopup,
                    DialogKind::Confirm => KeyContext::Confirm,
                };
            }
            Some(Overlay::Palette) => return KeyContext::Command,
            Some(Overlay::Help) => return KeyContext::Help,
            None => {}
        }
        if self.prompt.is_some() {
            return KeyContext::Prompt;
        }
        match self.mode {
            Mode::Filter => return KeyContext::Filter,
            Mode::Search => return KeyContext::Search,
            Mode::Command => return KeyContext::Command,
            Mode::Normal | Mode::Dialog => {}
        }
        match self.focus {
            Focus::Table => KeyContext::Normal,
            Focus::Inspector => KeyContext::Inspector,
            Focus::JobsDrawer => KeyContext::JobsDrawer,
        }
    }

    /// The mode the user sees: `pill_mode` while a dialog is open (§12.1).
    pub fn visible_mode(&self) -> Mode {
        if self.mode == Mode::Dialog {
            self.pill_mode
        } else {
            self.mode
        }
    }

    /// The filter job filling the active view of the active tab, while it
    /// runs, is paused or is queued (M4-04).
    pub fn running_filter(&self) -> Option<&crate::jobs::JobHandle> {
        let tab = self.active_tab()?;
        let job = self.jobs.get(tab.active_entry().job?)?;
        (job.kind == tachy_core::jobs::JobKind::Filter && job.is_pending()).then_some(job)
    }

    /// The inputs of `compute_layout` for the current state.
    pub fn layout_input(&self) -> LayoutInput {
        LayoutInput {
            // A running filter job shows its gauge there (§12.4).
            query_bar: self.prompt.is_some()
                || self.running_filter().is_some()
                || matches!(self.visible_mode(), Mode::Filter | Mode::Search),
            jobs_drawer_rows: self
                .jobs_drawer_open
                .then(|| u16::try_from(self.jobs.drawer_len()).unwrap_or(u16::MAX)),
            toast: !self.toasts.is_empty(),
            hints: self.hints_visible,
            inspector: self.inspector_visible,
        }
    }
}
