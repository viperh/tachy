//! The help screen's content (spec §11.7, §13, M7-02), built from the
//! **live** merged keymap (M6-03) and [`Action::description`]. Pure: no
//! drawing here, see `components/help.rs`.
//!
//! Keep in sync when adding things:
//! - a new Normal-mode action needs a row in [`NORMAL_SUBGROUPS`] (a test
//!   checks every action bound in the default Normal keymap has one);
//! - a new key context with user-visible keys needs a place in [`build`];
//! - context-specific wording goes in [`CONTEXT_DESCRIPTIONS`];
//! - keys handled outside the keymap (the column chooser's filter input)
//!   are listed in [`CHOOSER_FILTER_KEYS`].

use crate::{
    action::Action,
    keymap::{KeyChord, Keymap, chord_to_string},
    mode::KeyContext,
};

/// One block of the help screen: a title, `(keys, description)` rows and
/// free-text notes shown under them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpGroup {
    pub title: &'static str,
    /// `(keys, description)`. Several chords for one action are joined with
    /// spaces (`h ←`).
    pub entries: Vec<(String, String)>,
    pub notes: Vec<&'static str>,
    /// Part of the previous group (the inspector's value popup, the column
    /// chooser's filter input): drawn right under it, in the same column.
    pub continues: bool,
}

/// The sub-groups of Normal mode, in display order.
pub const NORMAL_SUBGROUP_TITLES: &[&str] = &[
    "Normal · Navigation",
    "Normal · Tabs & files",
    "Normal · Find",
    "Normal · Sort & columns",
    "Normal · Inspect",
    "Normal · Output",
    "Normal · Jobs & app",
];

/// Shown for Normal actions without a row in [`NORMAL_SUBGROUPS`] (only
/// possible with a user binding of a non-Normal action in Normal).
const NORMAL_OTHER: &str = "Normal · Other";

/// `Action → Normal sub-group` (index into [`NORMAL_SUBGROUP_TITLES`]).
const NORMAL_SUBGROUPS: &[(Action, usize)] = &[
    (Action::MoveLeft, 0),
    (Action::MoveDown, 0),
    (Action::MoveUp, 0),
    (Action::MoveRight, 0),
    (Action::HalfPageDown, 0),
    (Action::HalfPageUp, 0),
    (Action::PageDown, 0),
    (Action::PageUp, 0),
    (Action::FirstRow, 0),
    (Action::LastRow, 0),
    (Action::FirstCol, 0),
    (Action::LastCol, 0),
    (Action::NextCol, 0),
    (Action::PrevCol, 0),
    (Action::Goto, 0),
    (Action::Tab1, 1),
    (Action::Tab2, 1),
    (Action::Tab3, 1),
    (Action::Tab4, 1),
    (Action::Tab5, 1),
    (Action::Tab6, 1),
    (Action::Tab7, 1),
    (Action::Tab8, 1),
    (Action::Tab9, 1),
    (Action::OpenFile, 1),
    (Action::CloseTab, 1),
    (Action::Reload, 1),
    (Action::Search, 2),
    (Action::SearchNext, 2),
    (Action::SearchPrev, 2),
    (Action::Filter, 2),
    (Action::RefineFilter, 2),
    (Action::PopView, 2),
    (Action::JumpToSource, 2),
    (Action::SortAsc, 3),
    (Action::SortDesc, 3),
    (Action::ColumnChooser, 3),
    (Action::ShrinkCol, 3),
    (Action::GrowCol, 3),
    (Action::AutofitCol, 3),
    (Action::ToggleInspector, 4),
    (Action::FocusNext, 4),
    (Action::CopyCell, 5),
    (Action::CopyRow, 5),
    (Action::Export, 5),
    (Action::ToggleJobs, 6),
    (Action::CommandPalette, 6),
    (Action::Help, 6),
    (Action::Quit, 6),
    (Action::Suspend, 6),
    (Action::Dismiss, 6),
];

/// Wording that depends on the context; otherwise [`Action::description`].
const CONTEXT_DESCRIPTIONS: &[(KeyContext, Action, &str)] = &[
    (
        KeyContext::Normal,
        Action::Quit,
        "quit (confirms if jobs are running)",
    ),
    (
        KeyContext::Normal,
        Action::JumpToSource,
        "filtered/sorted view: jump to the source row, and back",
    ),
    (
        KeyContext::Normal,
        Action::NextCol,
        "next column (skips frozen)",
    ),
    (
        KeyContext::Normal,
        Action::PrevCol,
        "previous column (skips frozen)",
    ),
    (
        KeyContext::Normal,
        Action::Goto,
        "go to a row, N% or a column name",
    ),
    (
        KeyContext::Normal,
        Action::Dismiss,
        "dismiss a message, cancel a pending jump",
    ),
    (
        KeyContext::Normal,
        Action::CopyCell,
        "copy the cell (full value)",
    ),
    (
        KeyContext::Normal,
        Action::CopyRow,
        "copy the row as delimited text",
    ),
    (
        KeyContext::Filter,
        Action::Cancel,
        "cancel input, or the running scan",
    ),
    (
        KeyContext::Search,
        Action::Cancel,
        "cancel input, or the running scan",
    ),
    (
        KeyContext::Filter,
        Action::Complete,
        "complete a column name / keyword",
    ),
    (
        KeyContext::Search,
        Action::Complete,
        "complete a column name / keyword",
    ),
    (KeyContext::Command, Action::Submit, "run"),
    (KeyContext::Command, Action::Cancel, "close"),
    (KeyContext::Command, Action::Complete, "fill the arguments"),
    (
        KeyContext::JobsDrawer,
        Action::FocusNext,
        "back to the table",
    ),
    (KeyContext::JobsDrawer, Action::Cancel, "back to the table"),
    (
        KeyContext::JobsDrawer,
        Action::SelectNext,
        "select the next job",
    ),
    (
        KeyContext::JobsDrawer,
        Action::SelectPrev,
        "select the previous job",
    ),
    (KeyContext::Inspector, Action::FocusNext, "next panel"),
    (KeyContext::Inspector, Action::Cancel, "back to the table"),
    (KeyContext::Inspector, Action::SelectNext, "next field"),
    (KeyContext::Inspector, Action::SelectPrev, "previous field"),
    (KeyContext::ValuePopup, Action::SelectNext, "scroll down"),
    (KeyContext::ValuePopup, Action::SelectPrev, "scroll up"),
    (KeyContext::ValuePopup, Action::FirstRow, "top"),
    (KeyContext::ValuePopup, Action::LastRow, "bottom"),
    (KeyContext::ValuePopup, Action::Cancel, "close"),
    (KeyContext::DetectedFormat, Action::Submit, "accept"),
    (
        KeyContext::DetectedFormat,
        Action::Cancel,
        "revert to the sniffed format",
    ),
    (
        KeyContext::ColumnChooser,
        Action::SelectNext,
        "select the next column",
    ),
    (
        KeyContext::ColumnChooser,
        Action::SelectPrev,
        "select the previous column",
    ),
    (KeyContext::ColumnChooser, Action::Submit, "apply and close"),
    (
        KeyContext::ColumnChooser,
        Action::Cancel,
        "discard the changes and close",
    ),
    (KeyContext::Export, Action::Submit, "export"),
    (KeyContext::Export, Action::Cancel, "close"),
    (KeyContext::Export, Action::Complete, "complete the path"),
];

/// Keys of the column chooser's filter input. They are handled before the
/// keymap (`App::chooser_filter_key`), so they can't be remapped and are
/// listed here by hand.
pub const CHOOSER_FILTER_KEYS: &[(&str, &str)] = &[
    ("Enter", "keep the filter"),
    ("Esc", "clear the filter"),
    ("↑ ↓", "move the selection"),
    ("other keys", "edit the filter"),
];

/// The scroll keys of the help screen itself, for its bottom border:
/// `(actions, label)`.
const HELP_KEYS: &[(&[Action], &str)] = &[
    (&[Action::SelectPrev, Action::SelectNext], "scroll"),
    (&[Action::PageUp, Action::PageDown], "page"),
    (&[Action::FirstRow, Action::LastRow], "top/bottom"),
    (&[Action::Help, Action::Cancel], "close"),
];

/// The note under the Output sub-group (M6-05).
pub const CLIPBOARD_NOTE: &str = "copies use OSC 52: the terminal must support it \
     (tmux: set -g allow-passthrough on)";

/// The Normal sub-group of `action` (an index into
/// [`NORMAL_SUBGROUP_TITLES`]).
pub fn normal_subgroup(action: &Action) -> Option<usize> {
    NORMAL_SUBGROUPS
        .iter()
        .find(|(a, _)| a == action)
        .map(|(_, i)| *i)
}

/// The description of `action` in `context`.
pub fn description(context: KeyContext, action: &Action) -> &'static str {
    CONTEXT_DESCRIPTIONS
        .iter()
        .find(|(c, a, _)| *c == context && a == action)
        .map_or_else(|| action.description(), |(.., d)| d)
}

/// Every chord bound to `action` in `context`, in display order: plain keys
/// first, then shorter, then text order (as [`Keymap::display_chord`]).
fn chords(keymap: &Keymap, context: KeyContext, action: &Action) -> Vec<String> {
    let Some(map) = keymap.0.get(&context) else {
        return Vec::new();
    };
    let mut found: Vec<(bool, String)> = map
        .iter()
        .filter(|(_, a)| *a == action)
        .map(|(chord, _)| (!chord.mods.is_empty(), chord_to_string(*chord)))
        .collect();
    found.sort_by(|(ma, a), (mb, b)| {
        ma.cmp(mb)
            .then(a.chars().count().cmp(&b.chars().count()))
            .then(a.cmp(b))
    });
    found.dedup();
    found.into_iter().map(|(_, s)| s).collect()
}

/// The keys of `action` in `context`, joined with spaces; `None` if unbound.
pub fn keys(keymap: &Keymap, context: KeyContext, action: &Action) -> Option<String> {
    let chords = chords(keymap, context, action);
    (!chords.is_empty()).then(|| chords.join(" "))
}

/// The bound actions of `context` as entries, in [`Action::all`] order.
fn context_entries(keymap: &Keymap, context: KeyContext) -> Vec<(String, String)> {
    Action::all()
        .iter()
        .filter_map(|a| {
            Some((
                keys(keymap, context, a)?,
                description(context, a).to_owned(),
            ))
        })
        .collect()
}

/// Whether `Tab1`…`Tab9` are bound in Normal to exactly `1`…`9` (and
/// nothing else), so they can be shown as one `1–9` row.
fn tabs_are_digits(keymap: &Keymap) -> bool {
    Action::all()
        .iter()
        .filter_map(|a| Some((a, a.tab_number()?)))
        .all(|(a, n)| {
            let digit = char::from(b'1' + n as u8);
            chords(keymap, KeyContext::Normal, a) == [digit.to_string()]
                && keymap
                    .resolve(
                        KeyContext::Normal,
                        KeyChord::new(
                            crossterm::event::KeyCode::Char(digit),
                            crossterm::event::KeyModifiers::NONE,
                        ),
                    )
                    .is_some_and(|b| b == a)
        })
}

/// The Normal sub-groups, in order; empty ones are left out.
fn normal_groups(keymap: &Keymap) -> Vec<HelpGroup> {
    let collapse_tabs = tabs_are_digits(keymap);
    let mut groups: Vec<HelpGroup> = NORMAL_SUBGROUP_TITLES
        .iter()
        .chain(std::iter::once(&NORMAL_OTHER))
        .map(|title| HelpGroup {
            title,
            entries: Vec::new(),
            notes: Vec::new(),
            continues: false,
        })
        .collect();
    let other = groups.len() - 1;
    for action in Action::all() {
        let Some(keys) = keys(keymap, KeyContext::Normal, action) else {
            continue;
        };
        let group = normal_subgroup(action).unwrap_or(other);
        match action.tab_number() {
            Some(0) if collapse_tabs => groups[group]
                .entries
                .push(("1–9".to_owned(), "switch to tab N".to_owned())),
            Some(_) if collapse_tabs => {}
            _ => groups[group]
                .entries
                .push((keys, description(KeyContext::Normal, action).to_owned())),
        }
    }
    // The Output sub-group explains the clipboard (M6-05).
    if groups[5].entries.iter().any(|(_, d)| d.starts_with("copy")) {
        groups[5].notes.push(CLIPBOARD_NOTE);
    }
    groups.retain(|g| !g.entries.is_empty());
    groups
}

/// `Filter` and `Search` together: an action with the same keys in both is
/// shown once; keys bound in one only are marked `(filter)` / `(search)`.
fn filter_search_group(keymap: &Keymap) -> HelpGroup {
    let mut entries = Vec::new();
    for action in Action::all() {
        let f = keys(keymap, KeyContext::Filter, action);
        let s = keys(keymap, KeyContext::Search, action);
        let desc_f = description(KeyContext::Filter, action);
        let desc_s = description(KeyContext::Search, action);
        match (f, s) {
            (Some(f), Some(s)) if f == s && desc_f == desc_s => {
                entries.push((f, desc_f.to_owned()));
            }
            (f, s) => {
                if let Some(f) = f {
                    entries.push((f, format!("{desc_f} (filter)")));
                }
                if let Some(s) = s {
                    entries.push((s, format!("{desc_s} (search)")));
                }
            }
        }
    }
    HelpGroup {
        title: "Filter / search",
        entries,
        notes: Vec::new(),
        continues: false,
    }
}

fn group(
    keymap: &Keymap,
    title: &'static str,
    context: KeyContext,
    notes: &[&'static str],
) -> HelpGroup {
    HelpGroup {
        title,
        entries: context_entries(keymap, context),
        notes: notes.to_vec(),
        continues: false,
    }
}

/// The help screen's groups, in display order (M7-02): Normal (by
/// sub-group), Filter / search, Command palette, Jobs drawer, Inspector
/// (and its value popup), Detected format, Column chooser, Export dialog.
/// Groups without a single binding are left out.
pub fn build(keymap: &Keymap) -> Vec<HelpGroup> {
    let mut groups = normal_groups(keymap);
    groups.push(filter_search_group(keymap));
    groups.push(group(keymap, "Command palette", KeyContext::Command, &[]));
    groups.push(group(
        keymap,
        "Jobs drawer",
        KeyContext::JobsDrawer,
        &["focus it with Tab; other keys work as in Normal"],
    ));
    groups.push(group(
        keymap,
        "Inspector",
        KeyContext::Inspector,
        &["other keys work as in Normal"],
    ));
    groups.push(HelpGroup {
        continues: true,
        ..group(
            keymap,
            "Inspector · value popup",
            KeyContext::ValuePopup,
            &[],
        )
    });
    groups.push(group(
        keymap,
        "Detected format",
        KeyContext::DetectedFormat,
        &[],
    ));
    let chooser = group(keymap, "Column chooser", KeyContext::ColumnChooser, &[]);
    let chooser_bound = !chooser.entries.is_empty();
    groups.push(chooser);
    if chooser_bound {
        groups.push(HelpGroup {
            title: "Column chooser · filtering",
            continues: true,
            entries: CHOOSER_FILTER_KEYS
                .iter()
                .map(|(k, d)| ((*k).to_owned(), (*d).to_owned()))
                .collect(),
            notes: Vec::new(),
        });
    }
    groups.push(group(keymap, "Export dialog", KeyContext::Export, &[]));
    groups.retain(|g| !g.entries.is_empty());
    for g in &mut groups {
        merge_same_descriptions(&mut g.entries);
    }
    groups
}

/// Entries of one group with the same description (e.g. `Tab` and `Esc`
/// both going back to the table) become one row: `Tab Esc`.
fn merge_same_descriptions(entries: &mut Vec<(String, String)>) {
    let mut merged: Vec<(String, String)> = Vec::with_capacity(entries.len());
    for (keys, desc) in entries.drain(..) {
        match merged.iter_mut().find(|(_, d)| *d == desc) {
            Some((k, _)) => {
                k.push(' ');
                k.push_str(&keys);
            }
            None => merged.push((keys, desc)),
        }
    }
    *entries = merged;
}

/// The help screen's own keys, for its bottom border: `(keys, label)`,
/// e.g. `("k/j", "scroll")`. Uses one chord per action
/// ([`Keymap::display_chord`]); unbound actions are left out.
pub fn help_keys(keymap: &Keymap) -> Vec<(String, &'static str)> {
    HELP_KEYS
        .iter()
        .filter_map(|(actions, label)| {
            let keys: Vec<String> = actions
                .iter()
                .filter_map(|a| keymap.display_chord(KeyContext::Help, a))
                .collect();
            (!keys.is_empty()).then(|| (keys.join("/"), *label))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{config::Config, keymap::parse_key_chord};

    fn default_keymap() -> Keymap {
        Config::embedded().keybindings
    }

    fn find<'a>(groups: &'a [HelpGroup], title: &str) -> &'a HelpGroup {
        groups
            .iter()
            .find(|g| g.title == title)
            .unwrap_or_else(|| panic!("no group {title}"))
    }

    fn all_entries(groups: &[HelpGroup]) -> Vec<(String, String)> {
        groups.iter().flat_map(|g| g.entries.clone()).collect()
    }

    #[test]
    fn every_default_normal_action_has_a_subgroup() {
        let keymap = default_keymap();
        for action in keymap.0[&KeyContext::Normal].values() {
            assert!(
                normal_subgroup(action).is_some(),
                "{action} has no subgroup"
            );
        }
        // And every Normal entry of the table is a real, bindable action.
        for (action, group) in NORMAL_SUBGROUPS {
            assert!(Action::all().contains(action), "{action}");
            assert!(*group < NORMAL_SUBGROUP_TITLES.len());
        }
        let groups = build(&keymap);
        assert!(groups.iter().all(|g| g.title != NORMAL_OTHER));
    }

    #[test]
    fn group_order() {
        let titles: Vec<&str> = build(&default_keymap()).iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            [
                "Normal · Navigation",
                "Normal · Tabs & files",
                "Normal · Find",
                "Normal · Sort & columns",
                "Normal · Inspect",
                "Normal · Output",
                "Normal · Jobs & app",
                "Filter / search",
                "Command palette",
                "Jobs drawer",
                "Inspector",
                "Inspector · value popup",
                "Detected format",
                "Column chooser",
                "Column chooser · filtering",
                "Export dialog",
            ]
        );
    }

    /// Every action bound in the shown contexts appears with each of its
    /// chords.
    #[test]
    fn every_bound_action_appears() {
        let keymap = default_keymap();
        let groups = build(&keymap);
        let shown = [
            KeyContext::Normal,
            KeyContext::Filter,
            KeyContext::Search,
            KeyContext::Command,
            KeyContext::JobsDrawer,
            KeyContext::Inspector,
            KeyContext::ValuePopup,
            KeyContext::DetectedFormat,
            KeyContext::ColumnChooser,
            KeyContext::Export,
        ];
        let entries = all_entries(&groups);
        for context in shown {
            for (chord, action) in &keymap.0[&context] {
                if action.tab_number().is_some() {
                    continue; // `1–9`, checked below
                }
                let key = chord_to_string(*chord);
                let desc = description(context, action);
                assert!(
                    entries
                        .iter()
                        .any(|(k, d)| d.starts_with(desc) && k.split(' ').any(|c| c == key)),
                    "{context:?}: {key} → {action} missing"
                );
            }
        }
        assert!(entries.contains(&("1–9".to_owned(), "switch to tab N".to_owned())));
    }

    #[test]
    fn several_chords_are_joined() {
        let groups = build(&default_keymap());
        let nav = find(&groups, "Normal · Navigation");
        assert_eq!(
            nav.entries[0],
            ("h ←".to_owned(), "move the cursor left".to_owned())
        );
        let app = find(&groups, "Normal · Jobs & app");
        assert!(app.entries.iter().any(|(k, _)| k == "q ctrl-c ctrl-q"));
        let output = find(&groups, "Normal · Output");
        assert_eq!(output.notes, [CLIPBOARD_NOTE]);
    }

    #[test]
    fn filter_and_search_are_merged() {
        let groups = build(&default_keymap());
        let fs = find(&groups, "Filter / search");
        let enter: Vec<_> = fs.entries.iter().filter(|(k, _)| k == "Enter").collect();
        assert_eq!(enter.len(), 1);
        assert!(fs.entries.contains(&(
            "ctrl-s".to_owned(),
            "save as a named view (filter)".to_owned()
        )));
    }

    fn remap(keymap: &mut Keymap, context: KeyContext, key: &str, action: Option<Action>) {
        let chord = parse_key_chord(key).unwrap();
        let map = keymap.0.entry(context).or_default();
        match action {
            Some(a) => map.insert(chord, a),
            None => map.remove(&chord),
        };
    }

    #[test]
    fn remapping_shows_up() {
        let mut keymap = default_keymap();
        remap(&mut keymap, KeyContext::Normal, "Q", Some(Action::Quit));
        let groups = build(&keymap);
        let app = find(&groups, "Normal · Jobs & app");
        assert!(
            app.entries.iter().any(|(k, _)| k == "Q q ctrl-c ctrl-q"),
            "{app:?}"
        );
    }

    #[test]
    fn remapping_through_the_config_shows_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json5"),
            r#"{ keybindings: { Normal: { "<Q>": "Quit", "f": "None" } } }"#,
        )
        .unwrap();
        let (config, warnings) = Config::load_from(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let groups = build(&config.keybindings);
        let app = find(&groups, "Normal · Jobs & app");
        assert!(
            app.entries.iter().any(|(k, _)| k.starts_with("Q q")),
            "{app:?}"
        );
        let find_group = find(&groups, "Normal · Find");
        assert!(!find_group.entries.iter().any(|(_, d)| d == "new filter"));
    }

    #[test]
    fn unbinding_removes_entries() {
        let mut keymap = default_keymap();
        remap(&mut keymap, KeyContext::Normal, "f", None);
        remap(&mut keymap, KeyContext::Normal, "1", None);
        let groups = build(&keymap);
        let find_group = find(&groups, "Normal · Find");
        assert!(!find_group.entries.iter().any(|(_, d)| d == "new filter"));
        // Tab 1 unbound: the tabs are listed one by one.
        let tabs = find(&groups, "Normal · Tabs & files");
        assert!(!tabs.entries.iter().any(|(k, _)| k == "1–9"));
        assert!(tabs.entries.iter().any(|(k, _)| k == "2"));
        assert!(!tabs.entries.iter().any(|(k, _)| k == "1"));

        // A context with no binding at all disappears.
        keymap.0.remove(&KeyContext::Export);
        let groups = build(&keymap);
        assert!(groups.iter().all(|g| g.title != "Export dialog"));
    }

    #[test]
    fn non_normal_action_bound_in_normal_goes_to_other() {
        let mut keymap = default_keymap();
        remap(
            &mut keymap,
            KeyContext::Normal,
            "ctrl-p",
            Some(Action::PauseJob),
        );
        let groups = build(&keymap);
        let other = find(&groups, NORMAL_OTHER);
        assert_eq!(
            other.entries,
            [("ctrl-p".to_owned(), "pause or resume the job".to_owned())]
        );
    }

    #[test]
    fn help_keys_follow_the_keymap() {
        let keys = help_keys(&default_keymap());
        assert_eq!(keys.last(), Some(&("?/Esc".to_owned(), "close")));
        assert!(keys.iter().any(|(k, l)| k == "k/j" && *l == "scroll"));
    }
}
