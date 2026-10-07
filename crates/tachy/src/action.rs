use serde::{Deserialize, Serialize};
use strum::Display;

/// Messages passed between the event loop, [`App`](crate::app::App) and every
/// [`Component`](crate::components::Component).
///
/// Every payload-free variant except the internal ones (`Tick`, `Render`,
/// `Resume`, `ClearScreen`) can be bound to a key in the config (§13,
/// README §A4); the serde name is the variant name. [`Action::all`] lists
/// them, [`Action::command_name`] gives the palette name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Display, Serialize, Deserialize)]
pub enum Action {
    // ---- internal (never bound, never listed) ----
    Tick,
    Render,
    Resize(u16, u16),
    Resume,
    ClearScreen,
    Error(String),

    // ---- movement (M1-06) ----
    MoveLeft,
    MoveDown,
    MoveUp,
    MoveRight,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    FirstRow,
    LastRow,
    FirstCol,
    LastCol,
    NextCol,
    PrevCol,
    Goto,

    // ---- tabs (M1-08) ----
    Tab1,
    Tab2,
    Tab3,
    Tab4,
    Tab5,
    Tab6,
    Tab7,
    Tab8,
    Tab9,
    OpenFile,
    CloseTab,

    // ---- views, search, filter, sort ----
    Search,
    SearchNext,
    SearchPrev,
    Filter,
    RefineFilter,
    PopView,
    SortAsc,
    SortDesc,

    // ---- columns, panels ----
    ColumnChooser,
    ShrinkCol,
    GrowCol,
    AutofitCol,
    ToggleInspector,
    FocusNext,
    JumpToSource,
    CopyCell,
    CopyRow,
    Export,
    ToggleJobs,
    CommandPalette,
    Help,
    Quit,
    Reload,
    Suspend,
    /// `Esc` in Normal mode: dismiss the toast on screen, then cancel a
    /// pending jump, then clear the search highlight (M1-09, M2-03, M4-05).
    Dismiss,

    // ---- text inputs and dialogs ----
    Submit,
    Cancel,
    HistoryPrev,
    HistoryNext,
    Complete,
    SaveView,

    // ---- lists: jobs drawer, inspector, column chooser, palette ----
    SelectNext,
    SelectPrev,
    PauseJob,
    KillJob,
    DismissJob,
    OpenValue,

    // ---- Detected format dialog (M2-04) ----
    CycleDelimiter,
    ToggleHeader,
    CycleQuote,
    ToggleRaw,

    // ---- column chooser (M3-04) ----
    ToggleVisible,
    MoveColumnDown,
    MoveColumnUp,
    ShowAll,
    FilterList,
}

/// `(action, command name, description)` for every bindable action.
const BINDABLE: &[(Action, &str, &str)] = &[
    (Action::MoveLeft, "move_left", "move the cursor left"),
    (Action::MoveDown, "move_down", "move the cursor down"),
    (Action::MoveUp, "move_up", "move the cursor up"),
    (Action::MoveRight, "move_right", "move the cursor right"),
    (Action::HalfPageDown, "half_page_down", "half a page down"),
    (Action::HalfPageUp, "half_page_up", "half a page up"),
    (Action::PageDown, "page_down", "one page down"),
    (Action::PageUp, "page_up", "one page up"),
    (Action::FirstRow, "first_row", "go to the first row"),
    (
        Action::LastRow,
        "last_row",
        "go to the last row (so far, while a filter is running)",
    ),
    (Action::FirstCol, "first_col", "go to the first column"),
    (Action::LastCol, "last_col", "go to the last column"),
    (Action::NextCol, "next_col", "next scrolling column"),
    (Action::PrevCol, "prev_col", "previous scrolling column"),
    (
        Action::Goto,
        "goto",
        "go to a row, a percentage or a column",
    ),
    (Action::Tab1, "tab_1", "switch to tab 1"),
    (Action::Tab2, "tab_2", "switch to tab 2"),
    (Action::Tab3, "tab_3", "switch to tab 3"),
    (Action::Tab4, "tab_4", "switch to tab 4"),
    (Action::Tab5, "tab_5", "switch to tab 5"),
    (Action::Tab6, "tab_6", "switch to tab 6"),
    (Action::Tab7, "tab_7", "switch to tab 7"),
    (Action::Tab8, "tab_8", "switch to tab 8"),
    (Action::Tab9, "tab_9", "switch to tab 9"),
    (Action::OpenFile, "open", "open a file in a new tab"),
    (Action::CloseTab, "close_tab", "close the current tab"),
    (Action::Search, "search", "search"),
    (Action::SearchNext, "search_next", "next search match"),
    (Action::SearchPrev, "search_prev", "previous search match"),
    (Action::Filter, "filter", "new filter"),
    (
        Action::RefineFilter,
        "refine_filter",
        "refine the current filter",
    ),
    (Action::PopView, "pop_view", "pop the current view"),
    (
        Action::SortAsc,
        "sort_asc",
        "sort by the current column, ascending",
    ),
    (
        Action::SortDesc,
        "sort_desc",
        "sort by the current column, descending",
    ),
    (
        Action::ColumnChooser,
        "columns",
        "show, hide and reorder columns",
    ),
    (Action::ShrinkCol, "shrink_col", "shrink the current column"),
    (Action::GrowCol, "grow_col", "grow the current column"),
    (
        Action::AutofitCol,
        "autofit_col",
        "auto-fit the current column",
    ),
    (Action::ToggleInspector, "inspector", "toggle the inspector"),
    (
        Action::FocusNext,
        "focus_next",
        "move focus to the next panel",
    ),
    (
        Action::JumpToSource,
        "jump_to_source",
        "jump to the source row",
    ),
    (Action::CopyCell, "copy_cell", "copy the cell"),
    (Action::CopyRow, "copy_row", "copy the row"),
    (Action::Export, "export", "export the view"),
    (Action::ToggleJobs, "jobs", "toggle the jobs drawer"),
    (
        Action::CommandPalette,
        "palette",
        "open the command palette",
    ),
    (Action::Help, "help", "show the key bindings"),
    (Action::Quit, "quit", "quit"),
    (Action::Reload, "reload", "reload the file from disk"),
    (Action::Suspend, "suspend", "suspend to the shell"),
    (Action::Dismiss, "dismiss", "dismiss a message"),
    (Action::Submit, "submit", "apply"),
    (Action::Cancel, "cancel", "cancel"),
    (
        Action::HistoryPrev,
        "history_prev",
        "previous history entry",
    ),
    (Action::HistoryNext, "history_next", "next history entry"),
    (Action::Complete, "complete", "complete"),
    (Action::SaveView, "save_view", "save as a named view"),
    (Action::SelectNext, "select_next", "select the next item"),
    (
        Action::SelectPrev,
        "select_prev",
        "select the previous item",
    ),
    (Action::PauseJob, "pause_job", "pause or resume the job"),
    (Action::KillJob, "kill_job", "kill the job"),
    (
        Action::DismissJob,
        "dismiss_job",
        "dismiss the finished job",
    ),
    (Action::OpenValue, "open_value", "show the full value"),
    (
        Action::CycleDelimiter,
        "cycle_delimiter",
        "try the next delimiter",
    ),
    (
        Action::ToggleHeader,
        "toggle_header",
        "toggle the header row",
    ),
    (Action::CycleQuote, "cycle_quote", "try the next quote char"),
    (
        Action::ToggleRaw,
        "toggle_raw",
        "show the raw lines of the file",
    ),
    (
        Action::ToggleVisible,
        "toggle_visible",
        "show or hide the column",
    ),
    (
        Action::MoveColumnDown,
        "move_column_down",
        "move the column down",
    ),
    (Action::MoveColumnUp, "move_column_up", "move the column up"),
    (Action::ShowAll, "show_all", "show every column"),
    (Action::FilterList, "filter_list", "filter the list by name"),
];

impl Action {
    /// Every bindable action, in a stable order. Internal variants
    /// (`Tick`, `Render`, `Resize`, `Resume`, `ClearScreen`, `Error`) are
    /// not listed.
    pub fn all() -> &'static [Action] {
        static ALL: std::sync::LazyLock<Vec<Action>> =
            std::sync::LazyLock::new(|| BINDABLE.iter().map(|(a, ..)| a.clone()).collect());
        &ALL
    }

    fn entry(&self) -> Option<&'static (Action, &'static str, &'static str)> {
        BINDABLE.iter().find(|(a, ..)| a == self)
    }

    /// The snake_case name used by the command palette (M6-02). Empty for
    /// internal variants.
    pub fn command_name(&self) -> &'static str {
        self.entry().map_or("", |(_, name, _)| name)
    }

    /// One-line description for the palette and help. Empty for internal
    /// variants.
    pub fn description(&self) -> &'static str {
        self.entry().map_or("", |(.., desc)| desc)
    }

    /// `Tab1` … `Tab9` → 0 … 8.
    pub fn tab_number(&self) -> Option<usize> {
        Some(match self {
            Action::Tab1 => 0,
            Action::Tab2 => 1,
            Action::Tab3 => 2,
            Action::Tab4 => 3,
            Action::Tab5 => 4,
            Action::Tab6 => 5,
            Action::Tab7 => 6,
            Action::Tab8 => 7,
            Action::Tab9 => 8,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_bindable_action_round_trips_through_serde() {
        for action in Action::all() {
            let json = json5::to_string(action).unwrap();
            assert_eq!(json, format!("\"{action}\""));
            let back: Action = json5::from_str(&json).unwrap();
            assert_eq!(&back, action);
        }
    }

    #[test]
    fn command_names_are_unique_and_snake_case() {
        let mut seen = HashSet::new();
        for action in Action::all() {
            let name = action.command_name();
            assert!(!name.is_empty(), "{action}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{name}"
            );
            assert!(seen.insert(name), "duplicate {name}");
            assert!(!action.description().is_empty(), "{action}");
        }
    }

    #[test]
    fn internal_variants_are_not_listed() {
        for internal in [
            Action::Tick,
            Action::Render,
            Action::Resize(1, 1),
            Action::Resume,
            Action::ClearScreen,
            Action::Error(String::new()),
        ] {
            assert!(!Action::all().contains(&internal), "{internal}");
            assert_eq!(internal.command_name(), "");
        }
    }
}
