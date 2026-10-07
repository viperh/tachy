//! Input modes (spec §12.1) and key-binding contexts.

use serde::{Deserialize, Serialize};

/// The input mode (§12.1). There is exactly one `Mode` enum in the workspace.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Normal,
    Filter,
    Search,
    Command,
    /// A modal dialog is open. The pill keeps showing the mode that was
    /// active before (`AppState::pill_mode`), so this never reaches it.
    Dialog,
}

impl Mode {
    /// Text of the mode pill in the top bar (§11.2, §12.1).
    ///
    /// `Dialog` never reaches the pill: it panics in debug builds and shows
    /// `NORMAL` in release builds.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Filter => "FILTER",
            Mode::Search => "SEARCH",
            Mode::Command => "COMMAND",
            Mode::Dialog => {
                if cfg!(debug_assertions) {
                    unreachable!("Mode::Dialog has no pill; show AppState::pill_mode");
                }
                "NORMAL"
            }
        }
    }
}

/// The key map a key press is looked up in. Separate from [`Mode`] because
/// focus areas (inspector, jobs drawer) and individual dialogs need their own
/// maps. These are the top-level keys of `keybindings` in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyContext {
    Normal,
    Filter,
    Search,
    Command,
    Prompt,
    Inspector,
    JobsDrawer,
    DetectedFormat,
    Goto,
    ColumnChooser,
    Export,
    ValuePopup,
    Confirm,
    Help,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(Mode::Normal.label(), "NORMAL");
        assert_eq!(Mode::Filter.label(), "FILTER");
        assert_eq!(Mode::Search.label(), "SEARCH");
        assert_eq!(Mode::Command.label(), "COMMAND");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "pill_mode")]
    fn dialog_has_no_label() {
        Mode::Dialog.label();
    }
}
