//! The command registry of the palette (spec §1, §12.5, M6-02).
//!
//! Every bindable [`Action`] is a command named [`Action::command_name`],
//! except the ones that only make sense inside a dialog or a list
//! ([`DIALOG_ONLY`]). On top of them come the commands that take arguments
//! (`sort`, `filter`, `set type`, …) and, through the palette's
//! `extra_entries`, one `view: <name>` entry per saved view (M6-04).
//!
//! Commands are pure: [`Command::parse`] validates the typed arguments
//! against the active tab and returns an [`Invocation`], which `App` runs
//! (it alone can start jobs and open tabs). [`Command::preview`] gives the
//! palette's context line (§12.5) and [`Command::completions`] its argument
//! list.

use nucleo_matcher::{
    Config as MatchConfig, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use tachy_core::{
    column::ColumnMeta,
    dupes::{DupeMode, DupeSpec},
    edit::{EditOp, OP_NAMES, parse_ops},
    jobs::MemoryBudget,
    query::{
        self,
        lexer::{Keyword, is_ident},
    },
    search::match_column,
    size::format_count,
    sort::{SortSpec, estimate, format_bytes},
    types::ColType,
};

use crate::{
    action::Action,
    goto::{GotoTarget, parse_goto},
    keymap::Keymap,
    mode::KeyContext,
    path_complete::{self, Candidate},
    state::AppState,
    tab::{Loaded, Tab},
    views_store::NameStatus,
};

/// Bindable actions that only mean something inside a dialog, a text input
/// or a list (the jobs drawer, the column chooser, the palette itself). They
/// are not listed in the palette.
pub const DIALOG_ONLY: &[Action] = &[
    Action::Submit,
    Action::Cancel,
    Action::HistoryPrev,
    Action::HistoryNext,
    Action::Complete,
    Action::SaveView,
    Action::SelectNext,
    Action::SelectPrev,
    Action::PauseJob,
    Action::KillJob,
    Action::DismissJob,
    Action::OpenValue,
    Action::CycleDelimiter,
    Action::ToggleHeader,
    Action::CycleQuote,
    Action::ToggleRaw,
    Action::ToggleVisible,
    Action::MoveColumnDown,
    Action::MoveColumnUp,
    Action::ShowAll,
    Action::FilterList,
];

/// Key contexts searched, in order, for the binding shown next to a command
/// (Normal first, §13).
const CONTEXTS: [KeyContext; 14] = [
    KeyContext::Normal,
    KeyContext::Filter,
    KeyContext::Search,
    KeyContext::Command,
    KeyContext::Prompt,
    KeyContext::Inspector,
    KeyContext::JobsDrawer,
    KeyContext::DetectedFormat,
    KeyContext::Goto,
    KeyContext::ColumnChooser,
    KeyContext::Export,
    KeyContext::ValuePopup,
    KeyContext::Confirm,
    KeyContext::Help,
];

/// The arguments a command takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgSpec {
    None,
    /// `<col>[:asc|:desc][:ci], …` (§8.4).
    Sort,
    /// A filter expression (§8.2).
    Expr,
    /// `<col> <type|auto>` (§7.1).
    SetType,
    /// `on|off`.
    OnOff,
    /// A number of columns (`freeze`, §11.3).
    Count,
    /// `<col>|all` (§7.2).
    Profile,
    /// A file path (§13).
    Path,
    /// `<row|N%|col>` (§13).
    Goto,
    /// The name of a view in `views.json` (M6-04).
    ViewName,
    /// `[<col>]: <op> [| <op>]…`, `undo` or `reset` (`tachy_core::edit`).
    Edit,
    /// `[<col>, …]`: duplicate keys; none = every column.
    Columns,
}

impl ArgSpec {
    /// The argument syntax, shown after the command name.
    pub fn usage(self) -> &'static str {
        match self {
            ArgSpec::None => "",
            ArgSpec::Sort => "<col>[:asc|:desc][:ci], …",
            ArgSpec::Expr => "<expr>",
            ArgSpec::SetType => "<col> <bool|i64|f64|date|datetime|enum|str|auto>",
            ArgSpec::OnOff => "on|off",
            ArgSpec::Count => "<N>",
            ArgSpec::Profile => "<col>|all",
            ArgSpec::Path => "<path>",
            ArgSpec::Goto => "<row|N%|col>",
            ArgSpec::ViewName => "<name>",
            ArgSpec::Edit => "[<col>]: <op> [| <op>]… | undo | reset",
            ArgSpec::Columns => "[<col>, …]",
        }
    }
}

/// What a command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandKind {
    /// Dispatches `Command::action`.
    Action,
    Sort,
    Filter,
    SetType,
    SetHints,
    SetInspector,
    Freeze,
    Profile,
    Open,
    Export,
    Goto,
    /// A saved view (M6-04): applies its filter.
    View {
        filter: String,
    },
    /// `delete view <name>` (M6-04).
    DeleteView,
    /// `edit <col>: <ops>` (`tachy_core::edit`).
    Edit,
    /// `reset edits`: removes every column edit of the tab.
    ResetEdits,
    /// `dupes` / `dedupe` (`tachy_core::dupes`).
    Dupes(DupeMode),
}

/// One palette entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// `toggle_inspector`, `set type`, `view: DE`. Unique.
    pub name: String,
    pub description: String,
    pub args: ArgSpec,
    /// The bound action: its key is shown next to the entry, and running the
    /// command without arguments dispatches it (`open` → the `open ›`
    /// prompt, `filter` → the filter bar, `goto` → the go-to dialog).
    pub action: Option<Action>,
    pub kind: CommandKind,
}

/// `profile <col>` or `profile all` (M5-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileTarget {
    /// Index into the tab's columns.
    Column(usize),
    All,
}

/// What `edit` does to a column's op chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditChange {
    /// Appends the ops, as validated text (parsed again when it runs: a
    /// compiled regex has no `PartialEq`).
    Append(String),
    /// Removes the last op.
    Undo,
    /// Removes every op.
    Reset,
}

/// A validated command line, run by `App`.
#[derive(Debug, Clone, PartialEq)]
pub enum Invocation {
    Action(Action),
    Sort(SortSpec),
    Filter(String),
    /// `ty: None` removes the override (`auto`).
    SetType {
        col: usize,
        ty: Option<ColType>,
    },
    SetHints(bool),
    SetInspector(bool),
    Freeze(usize),
    Profile(ProfileTarget),
    /// The path as typed (`~` is expanded when it runs).
    Open(String),
    Export,
    Goto(GotoTarget),
    /// Removes a view from `views.json` (M6-04).
    DeleteView(String),
    /// Changes the edits of column `col` (an index into the tab's columns).
    Edit {
        col: usize,
        change: EditChange,
    },
    ResetEdits,
    Dupes(DupeSpec),
}

/// The palette's bottom line (§12.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// Dim: an estimate, what will happen, or the usage.
    Info(String),
    /// Coral.
    Error(String),
}

/// One argument completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Replaces the current argument.
    pub text: String,
    /// Appended after `text` (a space before the next argument).
    pub suffix: &'static str,
    /// Dim text after the completion (a column's type).
    pub description: String,
}

/// Directory listings for `open <path>`, kept between keystrokes so the
/// directory is listed once per directory, not once per frame.
#[derive(Debug, Default)]
pub struct CompletionCache {
    dir: Option<(String, Vec<Candidate>)>,
}

impl Command {
    fn new(
        name: &str,
        description: &str,
        args: ArgSpec,
        action: Option<Action>,
        kind: CommandKind,
    ) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            args,
            action,
            kind,
        }
    }

    /// Whether the command takes arguments.
    pub fn takes_args(&self) -> bool {
        self.args != ArgSpec::None
    }

    /// Whether running it without arguments does something (no usage error).
    pub fn runs_without_args(&self) -> bool {
        !self.takes_args()
            || self.action.is_some()
            || matches!(
                self.kind,
                CommandKind::Export | CommandKind::View { .. } | CommandKind::Dupes(_)
            )
    }

    /// `sort <col>[:asc|:desc][:ci], …`.
    pub fn usage(&self) -> String {
        if self.takes_args() {
            format!("{} {}", self.name, self.args.usage())
        } else {
            self.name.clone()
        }
    }

    /// Validates `args` (the text after the name) against the active tab.
    pub fn parse(&self, state: &AppState, args: &str) -> Result<Invocation, String> {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            if let Some(action) = &self.action
                && self.kind != CommandKind::Export
            {
                return Ok(Invocation::Action(action.clone()));
            }
            match &self.kind {
                CommandKind::Export => return Ok(Invocation::Export),
                CommandKind::View { filter } => {
                    loaded(state)?;
                    return Ok(Invocation::Filter(filter.clone()));
                }
                CommandKind::Dupes(mode) => {
                    loaded(state)?;
                    return Ok(Invocation::Dupes(DupeSpec {
                        columns: Vec::new(),
                        mode: *mode,
                    }));
                }
                _ if self.takes_args() => return Err(format!("usage: {}", self.usage())),
                _ => {}
            }
        }
        match &self.kind {
            CommandKind::Action | CommandKind::Export | CommandKind::View { .. } => {
                Err(format!("{} takes no arguments", self.name))
            }
            CommandKind::ResetEdits => {
                let (_, l) = loaded(state)?;
                if !trimmed.is_empty() {
                    return Err(format!("{} takes no arguments", self.name));
                }
                if l.source.edits().is_empty() {
                    return Err("no column is edited".to_owned());
                }
                Ok(Invocation::ResetEdits)
            }
            CommandKind::Edit => {
                let (tab, l) = loaded(state)?;
                let (col, ops) = split_edit(trimmed, tab, &l.columns)?;
                let field = l.columns[col].source_index;
                let change = match ops.trim() {
                    "undo" | "reset" if !l.source.edits().is_edited(field) => {
                        return Err(format!("{} has no edits", l.columns[col].name.display));
                    }
                    "undo" => EditChange::Undo,
                    "reset" => EditChange::Reset,
                    text => {
                        parse_ops(text).map_err(|e| e.message)?;
                        EditChange::Append(text.to_owned())
                    }
                };
                Ok(Invocation::Edit { col, change })
            }
            CommandKind::Dupes(mode) => {
                let (_, l) = loaded(state)?;
                DupeSpec::parse(args, *mode, &l.columns)
                    .map(Invocation::Dupes)
                    .map_err(|e| e.message)
            }
            CommandKind::Sort => {
                let (_, l) = loaded(state)?;
                SortSpec::parse(args, &l.columns)
                    .map(Invocation::Sort)
                    .map_err(|e| e.message)
            }
            CommandKind::Filter => {
                let (tab, l) = loaded(state)?;
                validate_filter(trimmed, tab, l)?;
                Ok(Invocation::Filter(trimmed.to_owned()))
            }
            CommandKind::SetType => {
                let (_, l) = loaded(state)?;
                let (col, ty) = trimmed
                    .rsplit_once(char::is_whitespace)
                    .ok_or_else(|| format!("usage: {}", self.usage()))?;
                let ty = match ty {
                    "auto" => None,
                    t => Some(ColType::from_label(t).ok_or_else(|| {
                        format!(
                            "unknown type \"{t}\": expected bool, i64, f64, date, datetime, enum, str or auto"
                        )
                    })?),
                };
                let col = match_column(col.trim(), &l.columns)?;
                Ok(Invocation::SetType { col, ty })
            }
            CommandKind::SetHints => parse_on_off(trimmed).map(Invocation::SetHints),
            CommandKind::SetInspector => parse_on_off(trimmed).map(Invocation::SetInspector),
            CommandKind::Freeze => {
                loaded(state)?;
                trimmed
                    .parse::<usize>()
                    .map(Invocation::Freeze)
                    .map_err(|_| format!("expected a number of columns, found \"{trimmed}\""))
            }
            CommandKind::Profile => {
                let (_, l) = loaded(state)?;
                if trimmed == "all" {
                    Ok(Invocation::Profile(ProfileTarget::All))
                } else {
                    match_column(trimmed, &l.columns)
                        .map(|c| Invocation::Profile(ProfileTarget::Column(c)))
                }
            }
            CommandKind::Open => Ok(Invocation::Open(trimmed.to_owned())),
            CommandKind::Goto => {
                let (_, l) = loaded(state)?;
                parse_goto(trimmed, &l.columns).map(Invocation::Goto)
            }
            CommandKind::DeleteView => match state.views.name_status(trimmed) {
                NameStatus::Stored => Ok(Invocation::DeleteView(trimmed.to_owned())),
                NameStatus::ConfigOnly => Err(format!(
                    "view \"{trimmed}\" is defined in the config file: edit it there"
                )),
                NameStatus::Free => Err(format!("no saved view named \"{trimmed}\"")),
            },
        }
    }

    /// The context line for `args` (§12.5): usage while nothing is typed,
    /// the parse error, or what running it will do (the sort estimate).
    pub fn preview(&self, state: &AppState, args: &str) -> Preview {
        if args.trim().is_empty() && self.takes_args() && !self.runs_without_args() {
            return Preview::Info(format!("usage: {}", self.usage()));
        }
        let invocation = match self.parse(state, args) {
            Ok(i) => i,
            Err(e) => return Preview::Error(e),
        };
        let Some((tab, l)) = loaded(state).ok() else {
            return Preview::Info(self.description.clone());
        };
        let scan = || format!("full scan of {}", format_bytes(l.source.len()));
        let name = |c: usize| {
            l.columns
                .get(c)
                .map_or_else(String::new, |m| m.name.display.clone())
        };
        Preview::Info(match invocation {
            Invocation::Action(_) | Invocation::Export => self.description.clone(),
            Invocation::Sort(spec) => {
                let free = MemoryBudget::new(state.settings.memory).free();
                estimate(tab.view_len(), &spec.keys, free).text()
            }
            Invocation::Filter(_) => format!("filter: {}", scan()),
            Invocation::SetType { col, ty } => {
                let Some(meta) = l.columns.get(col) else {
                    return Preview::Info(self.description.clone());
                };
                match ty {
                    Some(t) => format!("{}: {} → {}", meta.name.display, meta.ty(), t.label()),
                    None => format!(
                        "{}: {} → {} (inferred)",
                        meta.name.display,
                        meta.ty(),
                        meta.inferred.label()
                    ),
                }
            }
            Invocation::SetHints(on) => format!("key hints {}", on_off(on)),
            Invocation::SetInspector(on) => format!("inspector {}", on_off(on)),
            Invocation::Freeze(n) => format!(
                "freeze the first {n} column{} (now {})",
                if n == 1 { "" } else { "s" },
                tab.layout.freeze
            ),
            Invocation::Profile(ProfileTarget::All) => {
                format!("profile all {} columns: {}", l.columns.len(), scan())
            }
            Invocation::Profile(ProfileTarget::Column(c)) => {
                format!("profile {}: {}", name(c), scan())
            }
            Invocation::Open(path) => return open_preview(&path),
            Invocation::Goto(GotoTarget::Row(n)) => format!("go to row {}", format_count(n)),
            Invocation::Goto(GotoTarget::Percent(p)) => format!("go to {p}%"),
            Invocation::Goto(GotoTarget::Column(c)) => format!("go to column {}", name(c)),
            Invocation::DeleteView(n) => format!("delete the saved view \"{n}\""),
            Invocation::ResetEdits => {
                let n = l.source.edits().fields().count();
                format!(
                    "remove the edits of {n} column{}",
                    if n == 1 { "" } else { "s" }
                )
            }
            Invocation::Edit { col, change } => {
                let Some(meta) = l.columns.get(col) else {
                    return Preview::Info(self.description.clone());
                };
                let edits = l.source.edits();
                let field = meta.source_index;
                let current = edits.describe(field);
                match change {
                    EditChange::Undo => {
                        let mut e = edits.clone();
                        e.undo(field);
                        match e.describe(field) {
                            Some(left) => format!("{}: keep {left}", meta.name.display),
                            None => format!("{}: remove its only edit", meta.name.display),
                        }
                    }
                    EditChange::Reset => format!(
                        "{}: remove {}",
                        meta.name.display,
                        current.unwrap_or_default()
                    ),
                    EditChange::Append(text) => {
                        let ops = parse_ops(&text).unwrap_or_default();
                        edit_example(tab, l, col, &ops).map_or_else(
                            || format!("edit {}", meta.name.display),
                            |ex| format!("{}: {ex}", meta.name.display),
                        )
                    }
                }
            }
            Invocation::Dupes(spec) => {
                let what = match spec.mode {
                    DupeMode::Show => "show rows with duplicate",
                    DupeMode::Remove => "keep the first row of each",
                };
                format!("{what} {}: {}", spec.key_text(&l.columns), scan())
            }
        })
    }

    /// Completions of the argument being typed: the byte offset in `args`
    /// where it starts, and the candidates matching it (case-insensitive
    /// prefix).
    pub fn completions(
        &self,
        state: &AppState,
        args: &str,
        cache: &mut CompletionCache,
    ) -> (usize, Vec<Completion>) {
        let columns = loaded(state).map_or(&[][..], |(_, l)| l.columns.as_slice());
        let (start, candidates): (usize, Vec<Completion>) = match self.args {
            ArgSpec::None | ArgSpec::Count => (args.len(), Vec::new()),
            ArgSpec::ViewName => {
                let names = state.views.stored_names();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                (leading_spaces(args), words(&names))
            }
            ArgSpec::OnOff => (0, words(&["on", "off"])),
            ArgSpec::Sort => {
                let key_start = args.rfind(',').map_or(0, |i| i + 1);
                let key_start = key_start + leading_spaces(&args[key_start..]);
                match args[key_start..].rfind(':') {
                    Some(i) => (key_start + i + 1, words(&["asc", "desc", "ci"])),
                    None => (key_start, query_columns(columns)),
                }
            }
            ArgSpec::Expr => {
                let start = args
                    .rfind(|c: char| c.is_whitespace() || "(),!=<>~".contains(c))
                    .map_or(0, |i| i + 1);
                (start, query_columns(columns))
            }
            ArgSpec::SetType => {
                let lead = leading_spaces(args);
                match args.rfind(char::is_whitespace) {
                    Some(i) if i >= lead && match_column(args[lead..i].trim(), columns).is_ok() => {
                        let mut types: Vec<&str> = ColType::ALL.iter().map(|t| t.label()).collect();
                        types.push("auto");
                        (i + 1, words(&types))
                    }
                    _ => {
                        let mut list = display_columns(columns);
                        for c in &mut list {
                            c.suffix = " ";
                        }
                        (lead, list)
                    }
                }
            }
            ArgSpec::Profile => {
                let mut list = words(&["all"]);
                list.extend(display_columns(columns));
                (leading_spaces(args), list)
            }
            ArgSpec::Goto => (leading_spaces(args), display_columns(columns)),
            ArgSpec::Edit => match args.find(':') {
                // After `<col>:`, the op being typed (after the last `|`).
                Some(colon) => {
                    let op_start = args
                        .rfind('|')
                        .map_or(colon + 1, |i| (i + 1).max(colon + 1));
                    let op_start = op_start + leading_spaces(&args[op_start..]);
                    if args[op_start..].contains(char::is_whitespace) {
                        (args.len(), Vec::new())
                    } else {
                        let mut list = words(OP_NAMES);
                        if args[colon + 1..op_start].trim().is_empty() {
                            list.extend(words(&["undo", "reset"]));
                        }
                        (op_start, list)
                    }
                }
                None => {
                    let mut list = display_columns(columns);
                    for c in &mut list {
                        c.suffix = ": ";
                    }
                    (leading_spaces(args), list)
                }
            },
            ArgSpec::Columns => {
                let start = args.rfind(',').map_or(0, |i| i + 1);
                let start = start + leading_spaces(&args[start..]);
                (start, query_columns(columns))
            }
            ArgSpec::Path => {
                let lead = leading_spaces(args);
                let typed = &args[lead..];
                let (dir, _) = path_complete::split_input(typed);
                (lead + dir.len(), path_candidates(typed, cache))
            }
        };
        let prefix = args[start.min(args.len())..].to_lowercase();
        let candidates = candidates
            .into_iter()
            .filter(|c| c.text.to_lowercase().starts_with(&prefix))
            .collect();
        (start, candidates)
    }
}

/// The command list: argument commands, every listed action, then `extra`
/// (saved views). An argument command with the same name as an action
/// (`open`, `filter`, `goto`, `export`) replaces its entry and keeps its
/// action (for the key, and to run without arguments).
pub fn registry(extra: &[Command]) -> Vec<Command> {
    use CommandKind as K;
    let mut list = vec![
        Command::new(
            "sort",
            "sort the view by one or more columns",
            ArgSpec::Sort,
            None,
            K::Sort,
        ),
        Command::new(
            "filter",
            "filter the view",
            ArgSpec::Expr,
            Some(Action::Filter),
            K::Filter,
        ),
        Command::new(
            "set type",
            "override a column's type",
            ArgSpec::SetType,
            None,
            K::SetType,
        ),
        Command::new(
            "set hints",
            "show or hide the key-hint line",
            ArgSpec::OnOff,
            None,
            K::SetHints,
        ),
        Command::new(
            "set inspector",
            "show or hide the inspector",
            ArgSpec::OnOff,
            None,
            K::SetInspector,
        ),
        Command::new(
            "freeze",
            "freeze the first N columns",
            ArgSpec::Count,
            None,
            K::Freeze,
        ),
        Command::new(
            "profile",
            "full-column statistics",
            ArgSpec::Profile,
            None,
            K::Profile,
        ),
        Command::new(
            "open",
            "open a file in a new tab",
            ArgSpec::Path,
            Some(Action::OpenFile),
            K::Open,
        ),
        Command::new(
            "export",
            "export the view",
            ArgSpec::None,
            Some(Action::Export),
            K::Export,
        ),
        Command::new(
            "goto",
            "go to a row, a percentage or a column",
            ArgSpec::Goto,
            Some(Action::Goto),
            K::Goto,
        ),
        Command::new(
            "edit",
            "edit a column's values: drop, chop, trim, upper, s/re/rep/, …",
            ArgSpec::Edit,
            None,
            K::Edit,
        ),
        Command::new(
            "reset edits",
            "remove every column edit",
            ArgSpec::None,
            None,
            K::ResetEdits,
        ),
        Command::new(
            "dupes",
            "show rows whose columns have duplicate values",
            ArgSpec::Columns,
            None,
            K::Dupes(DupeMode::Show),
        ),
        Command::new(
            "dedupe",
            "remove duplicate rows, keeping the first of each",
            ArgSpec::Columns,
            None,
            K::Dupes(DupeMode::Remove),
        ),
        Command::new(
            "delete view",
            "delete a saved view",
            ArgSpec::ViewName,
            None,
            K::DeleteView,
        ),
    ];
    for action in Action::all() {
        if DIALOG_ONLY.contains(action) {
            continue;
        }
        let name = action.command_name();
        if list.iter().any(|c| c.name == name) {
            continue;
        }
        list.push(Command::new(
            name,
            action.description(),
            ArgSpec::None,
            Some(action.clone()),
            K::Action,
        ));
    }
    for c in extra {
        if !list.iter().any(|x| x.name == c.name) {
            list.push(c.clone());
        }
    }
    list
}

/// A `view: <name>` entry for a saved view (M6-04).
pub fn view_entry(name: &str, filter: &str) -> Command {
    Command::new(
        &format!("view: {name}"),
        &format!("filter: {filter}"),
        ArgSpec::None,
        None,
        CommandKind::View {
            filter: filter.to_owned(),
        },
    )
}

/// The key shown for `action`: its chord in Normal, else in the first
/// context that binds it (the live, merged keymap, so remaps show).
pub fn key_for(keymap: &Keymap, action: &Action) -> Option<String> {
    CONTEXTS
        .iter()
        .find_map(|&ctx| keymap.display_chord(ctx, action))
}

/// The command whose name, plus a space, starts `input` (the longest one,
/// so `set type` wins over a `set` command), and the byte offset where
/// its arguments start.
pub fn split_command(commands: &[Command], input: &str) -> Option<(usize, usize)> {
    commands
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            input.len() > c.name.len()
                && input.starts_with(c.name.as_str())
                && input[c.name.len()..].starts_with(' ')
        })
        .max_by_key(|(_, c)| c.name.len())
        .map(|(i, c)| (i, c.name.len() + 1))
}

/// Fuzzy matches `pattern` against the command names (`nucleo`): each hit
/// is the command index and the matched character indices, best score
/// first, ties alphabetically. An empty pattern lists every command
/// alphabetically, argument commands first.
pub fn fuzzy_match(commands: &[Command], pattern: &str) -> Vec<(usize, Vec<usize>)> {
    if pattern.trim().is_empty() {
        let mut all: Vec<usize> = (0..commands.len()).collect();
        all.sort_by(|&a, &b| {
            let (a, b) = (&commands[a], &commands[b]);
            b.takes_args()
                .cmp(&a.takes_args())
                .then_with(|| a.name.cmp(&b.name))
        });
        return all.into_iter().map(|i| (i, Vec::new())).collect();
    }
    let mut matcher = Matcher::new(MatchConfig::DEFAULT);
    let pattern = Pattern::parse(pattern, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut indices = Vec::new();
    let mut hits: Vec<(u32, usize, Vec<usize>)> = Vec::new();
    for (i, c) in commands.iter().enumerate() {
        indices.clear();
        if let Some(score) =
            pattern.indices(Utf32Str::new(&c.name, &mut buf), &mut matcher, &mut indices)
        {
            indices.sort_unstable();
            indices.dedup();
            hits.push((score, i, indices.iter().map(|&x| x as usize).collect()));
        }
    }
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| commands[a.1].name.cmp(&commands[b.1].name))
    });
    hits.into_iter().map(|(_, i, ix)| (i, ix)).collect()
}

// ---- helpers ---------------------------------------------------------------

/// The active tab and its loaded file, or why there is none.
fn loaded(state: &AppState) -> Result<(&Tab, &Loaded), String> {
    let tab = state.active_tab().ok_or("no file open")?;
    let l = tab.loaded.as_ref().ok_or("the file is still opening")?;
    Ok((tab, l))
}

fn parse_on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(format!("expected on or off, found \"{other}\"")),
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// Parses, resolves and type-checks a filter expression (M4-01, M4-02).
fn validate_filter(expr: &str, tab: &Tab, l: &Loaded) -> Result<(), String> {
    let names: Vec<_> = l.columns.iter().map(|c| c.name.clone()).collect();
    let ast = query::parse(expr).map_err(|e| e.message)?;
    let resolved = query::resolve(ast, &names).map_err(|e| e.message)?;
    query::compile(&resolved, &l.columns, l.source.dialect(), &tab.nulls).map_err(|e| e.message)?;
    Ok(())
}

/// Splits `edit` arguments into the column (an index into `columns`) and
/// the op text: `<col>: <ops>`, or `: <ops>` for the cursor column. The
/// longest text before a `:` that is exactly a column name wins, so names
/// containing `:` work (`a:b: upper`, even next to an `a` column); otherwise
/// the text before the first `:` is matched with the go-to column rules
/// (case, prefix).
fn split_edit<'a>(
    args: &'a str,
    tab: &Tab,
    columns: &[ColumnMeta],
) -> Result<(usize, &'a str), String> {
    const USAGE: &str = "usage: edit <col>: <op> [| <op>]… (or undo, reset)";
    let colons: Vec<usize> = args.match_indices(':').map(|(i, _)| i).collect();
    let first = *colons.first().ok_or(USAGE)?;
    let name = args[..first].trim();
    if name.is_empty() {
        let col = tab
            .cursor_source_col()
            .ok_or("no column under the cursor")?;
        return Ok((col, &args[first + 1..]));
    }
    for &i in colons.iter().rev() {
        let name = args[..i].trim();
        if let Some(col) = columns
            .iter()
            .position(|c| c.name.display == name || c.name.query == name)
        {
            return Ok((col, &args[i + 1..]));
        }
    }
    let col = match_column(name, columns)?;
    Ok((col, &args[first + 1..]))
}

/// `"ABC-1001" → "BC-1001"`: the cursor row's value of `col` before and
/// after `ops` (on top of the column's current edits). `None` when the
/// cursor row is not on screen or the cell is missing.
fn edit_example(tab: &Tab, l: &Loaded, col: usize, ops: &[EditOp]) -> Option<String> {
    let field = l.columns.get(col)?.source_index;
    let row = tab.cursor_row_data()?;
    if field >= row.field_count() {
        return None;
    }
    let before = row.display(&l.source, field);
    if tab.nulls.is_null(before.as_bytes()) {
        return Some(format!("\"{before}\" (null, unchanged)"));
    }
    let after = ops.iter().fold(before.to_owned(), |s, op| op.apply(&s));
    let shorten = |s: &str| tachy_core::text::truncate_end(&s.escape_debug().to_string(), 40);
    Some(format!("\"{}\" → \"{}\"", shorten(before), shorten(&after)))
}

fn open_preview(path: &str) -> Preview {
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = path_complete::home_dir();
    let resolved = path_complete::resolve(path, &cwd, home.as_deref());
    match std::fs::metadata(&resolved) {
        Ok(m) if m.is_dir() => Preview::Error(format!("{} is a directory", resolved.display())),
        Ok(m) => Preview::Info(format!(
            "open {} ({})",
            resolved.display(),
            format_bytes(m.len())
        )),
        Err(e) => Preview::Error(format!("cannot open {}: {e}", resolved.display())),
    }
}

fn leading_spaces(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

fn words(list: &[&str]) -> Vec<Completion> {
    list.iter()
        .map(|w| Completion {
            text: (*w).to_owned(),
            suffix: "",
            description: String::new(),
        })
        .collect()
}

/// A column name as written in a query: backticked unless it is a plain
/// identifier.
fn query_ident(name: &str) -> String {
    if is_ident(name) && Keyword::from_ident(name).is_none() {
        name.to_owned()
    } else {
        format!("`{name}`")
    }
}

fn query_columns(columns: &[ColumnMeta]) -> Vec<Completion> {
    columns
        .iter()
        .map(|c| Completion {
            text: query_ident(&c.name.query),
            suffix: "",
            description: c.ty().label().to_owned(),
        })
        .collect()
}

fn display_columns(columns: &[ColumnMeta]) -> Vec<Completion> {
    columns
        .iter()
        .map(|c| Completion {
            text: c.name.display.clone(),
            suffix: "",
            description: c.ty().label().to_owned(),
        })
        .collect()
}

/// Entries of the directory part of `typed`, from the cache when that
/// directory was listed already.
fn path_candidates(typed: &str, cache: &mut CompletionCache) -> Vec<Completion> {
    let (dir, _) = path_complete::split_input(typed);
    let fresh = cache.dir.as_ref().is_none_or(|(d, _)| d != dir);
    if fresh {
        let cwd = std::env::current_dir().unwrap_or_default();
        let home = path_complete::home_dir();
        // `list_candidates` filters by the typed name; list the whole
        // directory once and filter here instead.
        let list = path_complete::list_candidates(dir, &cwd, home.as_deref()).unwrap_or_default();
        cache.dir = Some((dir.to_owned(), list));
    }
    let list = cache.dir.as_ref().map_or(&[][..], |(_, l)| l.as_slice());
    let sep = path_complete::separator_for(typed);
    list.iter()
        .map(|c| Completion {
            text: c.completed(sep),
            suffix: "",
            description: if c.is_dir { "dir" } else { "" }.to_owned(),
        })
        .collect()
}

#[cfg(test)]
pub mod tests {
    use std::collections::{HashMap, HashSet};

    use clap::Parser;
    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;

    use super::*;
    use crate::{
        cli::Cli, config::Config, keymap::parse_key_chord, settings::Settings,
        tab::tests::loaded_tab, theme::ColorSupport,
    };

    pub const CSV: &str = "name,price,qty,city\n\
        Apple,1.5,3,Paris\n\
        Banana,0.25,12,Berlin\n\
        Cherry,7,1,Rome\n";

    /// A state with one loaded tab over [`CSV`] (index complete).
    pub fn state_with_tab() -> (NamedTempFile, AppState) {
        let mut state = empty_state();
        let (f, tab) = loaded_tab(CSV, state.settings.freeze);
        state.tabs.push(tab);
        (f, state)
    }

    pub fn empty_state() -> AppState {
        let cli = Cli::try_parse_from(["tachy", "x.csv"]).unwrap();
        let settings = Settings::resolve(&cli, &Config::embedded()).unwrap();
        AppState::new(settings, ColorSupport::TrueColor)
    }

    fn cmd(name: &str) -> Command {
        registry(&[])
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no command {name}"))
    }

    fn parse(name: &str, args: &str, state: &AppState) -> Result<Invocation, String> {
        cmd(name).parse(state, args)
    }

    #[test]
    fn every_listed_action_has_one_entry_and_names_are_unique() {
        let list = registry(&[]);
        for action in Action::all() {
            let n = list
                .iter()
                .filter(|c| c.action.as_ref() == Some(action))
                .count();
            if DIALOG_ONLY.contains(action) {
                assert_eq!(n, 0, "{action} is dialog-only");
            } else {
                assert_eq!(n, 1, "{action}");
            }
        }
        let mut seen = HashSet::new();
        for c in &list {
            assert!(seen.insert(c.name.as_str()), "duplicate {}", c.name);
            assert!(!c.description.is_empty(), "{}", c.name);
        }
        for name in [
            "sort",
            "filter",
            "set type",
            "set hints",
            "set inspector",
            "freeze",
            "profile",
            "open",
            "export",
            "goto",
        ] {
            assert!(seen.contains(name), "{name}");
        }
    }

    #[test]
    fn extra_entries_are_appended_once() {
        let view = view_entry("DE", "country = \"DE\"");
        let list = registry(&[view.clone(), view.clone()]);
        assert_eq!(list.iter().filter(|c| c.name == "view: DE").count(), 1);
        let (_f, state) = state_with_tab();
        assert_eq!(
            view.parse(&state, ""),
            Ok(Invocation::Filter("country = \"DE\"".to_owned()))
        );
        assert!(view.parse(&state, "x").is_err());
    }

    #[test]
    fn keys_come_from_the_live_keymap() {
        let keymap = Config::embedded().keybindings;
        assert_eq!(
            key_for(&keymap, &Action::ToggleInspector).as_deref(),
            Some("i")
        );
        assert_eq!(key_for(&keymap, &Action::Quit).as_deref(), Some("q"));
        // A remap shows; a binding only in another context is found there.
        let mut normal = HashMap::new();
        normal.insert(parse_key_chord("Z").unwrap(), Action::ToggleInspector);
        let mut help = HashMap::new();
        help.insert(parse_key_chord("?").unwrap(), Action::Help);
        let remapped = Keymap(HashMap::from([
            (KeyContext::Normal, normal),
            (KeyContext::Help, help),
        ]));
        assert_eq!(
            key_for(&remapped, &Action::ToggleInspector).as_deref(),
            Some("Z")
        );
        assert_eq!(key_for(&remapped, &Action::Help).as_deref(), Some("?"));
        assert_eq!(key_for(&remapped, &Action::Quit), None);
    }

    #[test]
    fn fuzzy_matching() {
        let list = registry(&[]);
        let hits = fuzzy_match(&list, "srt");
        assert_eq!(list[hits[0].0].name, "sort");
        assert_eq!(hits[0].1, vec![0, 2, 3]);
        let hits = fuzzy_match(&list, "set ty");
        assert_eq!(list[hits[0].0].name, "set type");
        assert!(fuzzy_match(&list, "zzzz").is_empty());
        // Empty: argument commands first, each group alphabetical.
        let all = fuzzy_match(&list, "");
        assert_eq!(all.len(), list.len());
        let names: Vec<&str> = all.iter().map(|(i, _)| list[*i].name.as_str()).collect();
        assert_eq!(
            &names[..13],
            [
                "dedupe",
                "delete view",
                "dupes",
                "edit",
                "filter",
                "freeze",
                "goto",
                "open",
                "profile",
                "set hints",
                "set inspector",
                "set type",
                "sort"
            ]
        );
        let rest = &names[13..];
        assert!(rest.windows(2).all(|w| w[0] < w[1]), "{rest:?}");
    }

    #[test]
    fn longest_command_name_wins() {
        let list = registry(&[]);
        let split = |s: &str| split_command(&list, s).map(|(i, at)| (list[i].name.as_str(), at));
        assert_eq!(split("set type price str"), Some(("set type", 9)));
        assert_eq!(split("sort "), Some(("sort", 5)));
        assert_eq!(split("sort"), None);
        assert_eq!(split("sorted x"), None);
        assert_eq!(split("set "), None);
    }

    #[test]
    fn sort_arguments() {
        let (_f, state) = state_with_tab();
        let Ok(Invocation::Sort(spec)) = parse("sort", "price:desc, name", &state) else {
            panic!()
        };
        assert_eq!(spec.keys.len(), 2);
        assert_eq!((spec.keys[0].column, spec.keys[0].descending), (1, true));
        assert_eq!((spec.keys[1].column, spec.keys[1].descending), (0, false));
        assert!(parse("sort", "nope", &state).is_err());
        assert!(
            parse("sort", "price:up", &state)
                .unwrap_err()
                .contains("asc, desc or ci")
        );
        assert!(
            parse("sort", "", &state)
                .unwrap_err()
                .starts_with("usage: sort")
        );
        assert_eq!(
            parse("sort", "price", &empty_state()),
            Err("no file open".to_owned())
        );
    }

    #[test]
    fn sort_preview_is_the_estimate() {
        let (_f, state) = state_with_tab();
        let Preview::Info(text) = cmd("sort").preview(&state, "price:desc") else {
            panic!()
        };
        assert!(
            text.starts_with("est. ") && text.ends_with("in-memory sort"),
            "{text}"
        );
        assert!(matches!(
            cmd("sort").preview(&state, "nope"),
            Preview::Error(_)
        ));
        assert_eq!(
            cmd("sort").preview(&state, ""),
            Preview::Info("usage: sort <col>[:asc|:desc][:ci], …".to_owned())
        );
    }

    #[test]
    fn filter_arguments() {
        let (_f, state) = state_with_tab();
        assert_eq!(
            parse("filter", " city == \"Rome\" ", &state),
            Ok(Invocation::Filter("city == \"Rome\"".to_owned()))
        );
        assert!(parse("filter", "nope = 1", &state).is_err());
        assert!(parse("filter", "city = ", &state).is_err());
        // Without arguments it opens the filter bar.
        assert_eq!(
            parse("filter", "", &state),
            Ok(Invocation::Action(Action::Filter))
        );
    }

    #[test]
    fn set_type_arguments() {
        let (_f, state) = state_with_tab();
        assert_eq!(
            parse("set type", "price str", &state),
            Ok(Invocation::SetType {
                col: 1,
                ty: Some(ColType::Str)
            })
        );
        assert_eq!(
            parse("set type", "PRICE  auto", &state),
            Ok(Invocation::SetType { col: 1, ty: None })
        );
        assert!(
            parse("set type", "price", &state)
                .unwrap_err()
                .starts_with("usage")
        );
        assert!(
            parse("set type", "price float", &state)
                .unwrap_err()
                .contains("unknown type")
        );
        assert!(
            parse("set type", "nope str", &state)
                .unwrap_err()
                .contains("no such column")
        );
    }

    #[test]
    fn on_off_arguments() {
        let state = empty_state();
        assert_eq!(
            parse("set hints", "off", &state),
            Ok(Invocation::SetHints(false))
        );
        assert_eq!(
            parse("set hints", "on", &state),
            Ok(Invocation::SetHints(true))
        );
        assert_eq!(
            parse("set inspector", " off ", &state),
            Ok(Invocation::SetInspector(false))
        );
        assert!(
            parse("set hints", "maybe", &state)
                .unwrap_err()
                .contains("on or off")
        );
        assert!(
            parse("set hints", "", &state)
                .unwrap_err()
                .starts_with("usage")
        );
    }

    #[test]
    fn freeze_arguments() {
        let (_f, state) = state_with_tab();
        assert_eq!(parse("freeze", "2", &state), Ok(Invocation::Freeze(2)));
        assert_eq!(parse("freeze", "0", &state), Ok(Invocation::Freeze(0)));
        assert!(
            parse("freeze", "two", &state)
                .unwrap_err()
                .contains("number")
        );
        assert!(parse("freeze", "-1", &state).is_err());
        assert!(
            parse("freeze", "", &state)
                .unwrap_err()
                .starts_with("usage")
        );
    }

    #[test]
    fn profile_arguments() {
        let (_f, state) = state_with_tab();
        assert_eq!(
            parse("profile", "all", &state),
            Ok(Invocation::Profile(ProfileTarget::All))
        );
        assert_eq!(
            parse("profile", "qty", &state),
            Ok(Invocation::Profile(ProfileTarget::Column(2)))
        );
        assert!(parse("profile", "nope", &state).is_err());
        assert!(
            parse("profile", "", &state)
                .unwrap_err()
                .starts_with("usage")
        );
    }

    #[test]
    fn open_export_and_goto_arguments() {
        let (_f, state) = state_with_tab();
        assert_eq!(
            parse("open", " ~/a.csv ", &state),
            Ok(Invocation::Open("~/a.csv".to_owned()))
        );
        assert_eq!(
            parse("open", "", &state),
            Ok(Invocation::Action(Action::OpenFile))
        );
        assert_eq!(parse("export", "", &state), Ok(Invocation::Export));
        assert!(
            parse("export", "x", &state)
                .unwrap_err()
                .contains("no arguments")
        );
        assert_eq!(
            parse("goto", "2", &state),
            Ok(Invocation::Goto(GotoTarget::Row(2)))
        );
        assert_eq!(
            parse("goto", "city", &state),
            Ok(Invocation::Goto(GotoTarget::Column(3)))
        );
        assert!(parse("goto", "0", &state).is_err());
        assert_eq!(
            parse("goto", "", &state),
            Ok(Invocation::Action(Action::Goto))
        );
    }

    #[test]
    fn plain_actions_take_no_arguments() {
        let state = empty_state();
        assert_eq!(
            parse("inspector", "", &state),
            Ok(Invocation::Action(Action::ToggleInspector))
        );
        assert!(
            parse("inspector", "x", &state)
                .unwrap_err()
                .contains("no arguments")
        );
    }

    #[test]
    fn edit_arguments() {
        let (_f, mut state) = state_with_tab();
        let append = |col: usize, ops: &str| {
            Ok(Invocation::Edit {
                col,
                change: EditChange::Append(ops.to_owned()),
            })
        };
        assert_eq!(parse("edit", "name: drop 1", &state), append(0, "drop 1"));
        assert_eq!(
            parse("edit", "city:trim | s/a|b/x/g", &state),
            append(3, "trim | s/a|b/x/g")
        );
        // No column: the cursor column.
        state.tabs[0].cursor_col = 2;
        assert_eq!(parse("edit", ": upper", &state), append(2, "upper"));
        // Prefixes and case follow the go-to rules.
        assert_eq!(parse("edit", "CIT: upper", &state), append(3, "upper"));
        // Errors: unknown column, bad op, no `:`, nothing to undo.
        assert!(parse("edit", "nope: upper", &state).is_err());
        assert!(
            parse("edit", "name: frob", &state)
                .unwrap_err()
                .contains("unknown edit")
        );
        assert!(
            parse("edit", "name upper", &state)
                .unwrap_err()
                .starts_with("usage")
        );
        assert_eq!(
            parse("edit", "name: undo", &state),
            Err("name has no edits".to_owned())
        );
        assert!(parse("edit", "", &state).unwrap_err().starts_with("usage"));
        // With an edit: undo / reset are accepted.
        state.tabs[0].append_edit(0, "drop 1").unwrap();
        assert_eq!(
            parse("edit", "name: undo", &state),
            Ok(Invocation::Edit {
                col: 0,
                change: EditChange::Undo
            })
        );
        assert_eq!(
            parse("edit", "name:reset", &state),
            Ok(Invocation::Edit {
                col: 0,
                change: EditChange::Reset
            })
        );
    }

    #[test]
    fn edit_column_names_may_contain_a_colon() {
        let mut state = empty_state();
        let (_f, tab) = crate::tab::tests::loaded_tab("a:b,a\n1,2\n3,4\n", 1);
        state.tabs.push(tab);
        let names: Vec<String> = state.tabs[0]
            .loaded
            .as_ref()
            .unwrap()
            .columns
            .iter()
            .map(|c| c.name.display.clone())
            .collect();
        assert_eq!(names, ["a:b", "a"]);
        let edit = |col: usize, ops: &str| {
            Ok(Invocation::Edit {
                col,
                change: EditChange::Append(ops.to_owned()),
            })
        };
        assert_eq!(parse("edit", "a:b: upper", &state), edit(0, "upper"));
        assert_eq!(parse("edit", "a: upper", &state), edit(1, "upper"));
    }

    #[test]
    fn edit_preview_shows_the_cursor_value_before_and_after() {
        let (_f, mut state) = state_with_tab();
        state.tabs[0].prepare_frame(crate::tab::Viewport {
            body_height: 10,
            table_width: 80,
        });
        let preview = |state: &AppState, args: &str| cmd("edit").preview(state, args);
        assert_eq!(
            preview(&state, "name: drop 1 | upper"),
            Preview::Info("name: \"Apple\" → \"PPLE\"".to_owned())
        );
        assert_eq!(
            preview(&state, "name: frob"),
            Preview::Error(
                "unknown edit \"frob\" (drop, chop, take, slice, trim, upper, lower, title, \
                 lpad, rpad, prefix, suffix, replace, s/re/rep/)"
                    .to_owned()
            )
        );
        // On top of the current edits.
        state.tabs[0].append_edit(0, "drop 1").unwrap();
        state.tabs[0].prepare_frame(crate::tab::Viewport {
            body_height: 10,
            table_width: 80,
        });
        assert_eq!(
            preview(&state, "name: chop 1"),
            Preview::Info("name: \"pple\" → \"ppl\"".to_owned())
        );
        assert_eq!(
            preview(&state, "name: undo"),
            Preview::Info("name: remove its only edit".to_owned())
        );
        assert_eq!(
            preview(&state, "name: reset"),
            Preview::Info("name: remove drop 1".to_owned())
        );
    }

    #[test]
    fn reset_edits_arguments() {
        let (_f, mut state) = state_with_tab();
        assert_eq!(
            parse("reset edits", "", &state),
            Err("no column is edited".to_owned())
        );
        state.tabs[0].append_edit(1, "drop 1").unwrap();
        state.tabs[0].append_edit(2, "upper").unwrap();
        assert_eq!(parse("reset edits", "", &state), Ok(Invocation::ResetEdits));
        assert_eq!(
            cmd("reset edits").preview(&state, ""),
            Preview::Info("remove the edits of 2 columns".to_owned())
        );
        assert!(
            parse("reset edits", "x", &state)
                .unwrap_err()
                .contains("no arguments")
        );
    }

    #[test]
    fn dupes_arguments() {
        let (_f, state) = state_with_tab();
        let spec = |columns: Vec<usize>, mode| Ok(Invocation::Dupes(DupeSpec { columns, mode }));
        assert_eq!(parse("dupes", "", &state), spec(vec![], DupeMode::Show));
        assert_eq!(
            parse("dupes", "city, name", &state),
            spec(vec![3, 0], DupeMode::Show)
        );
        assert_eq!(
            parse("dedupe", "$2", &state),
            spec(vec![1], DupeMode::Remove)
        );
        assert!(parse("dedupe", "nope", &state).is_err());
        assert!(cmd("dupes").runs_without_args());
        assert_eq!(
            cmd("dupes").preview(&state, "city"),
            Preview::Info("show rows with duplicate city: full scan of 76 B".to_owned())
        );
        assert_eq!(
            cmd("dedupe").preview(&state, ""),
            Preview::Info("keep the first row of each all columns: full scan of 76 B".to_owned())
        );
        // No file: an error, not a job.
        assert!(parse("dupes", "", &empty_state()).is_err());
    }

    #[test]
    fn argument_completions() {
        let (_f, state) = state_with_tab();
        let mut cache = CompletionCache::default();
        let texts = |name: &str, args: &str, cache: &mut CompletionCache| {
            let (start, list) = cmd(name).completions(&state, args, cache);
            (start, list.into_iter().map(|c| c.text).collect::<Vec<_>>())
        };
        assert_eq!(
            texts("sort", "pr", &mut cache),
            (0, vec!["price".to_owned()])
        );
        assert_eq!(
            texts("sort", "price:d", &mut cache),
            (6, vec!["desc".to_owned()])
        );
        assert_eq!(
            texts("sort", "price, q", &mut cache),
            (7, vec!["qty".to_owned()])
        );
        assert_eq!(
            texts("set type", "price ", &mut cache).1,
            [
                "bool", "i64", "f64", "date", "datetime", "enum", "str", "auto"
            ]
        );
        assert_eq!(
            texts("set type", "ci", &mut cache),
            (0, vec!["city".to_owned()])
        );
        assert_eq!(texts("set hints", "o", &mut cache).1, ["on", "off"]);
        assert_eq!(texts("profile", "", &mut cache).1[0], "all");
        assert_eq!(texts("freeze", "", &mut cache).1, Vec::<String>::new());
        // `edit`: columns (with `: `), then op names after the colon, then
        // nothing while an op's arguments are typed.
        let (start, list) = cmd("edit").completions(&state, "ci", &mut cache);
        assert_eq!(start, 0);
        assert_eq!(
            list.iter()
                .map(|c| (c.text.as_str(), c.suffix))
                .collect::<Vec<_>>(),
            [("city", ": ")]
        );
        assert_eq!(
            texts("edit", "city: dr", &mut cache),
            (6, vec!["drop".to_owned()])
        );
        assert_eq!(
            texts("edit", "city: re", &mut cache).1,
            ["replace", "reset"]
        );
        assert_eq!(
            texts("edit", "city: trim | u", &mut cache),
            (13, vec!["upper".to_owned()])
        );
        assert_eq!(
            texts("edit", "city: drop 1", &mut cache).1,
            Vec::<String>::new()
        );
        assert_eq!(
            texts("dupes", "name, c", &mut cache),
            (6, vec!["city".to_owned()])
        );
        assert_eq!(texts("dedupe", "", &mut cache).1.len(), 4);
    }
}
