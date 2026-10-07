//! UI state of the filter and search bars (M4-04, M4-05), and the pure
//! helpers they use: validation, Tab completion and cell highlighting.
//!
//! `App`'s handlers live in `app/find.rs`; the query bar draws
//! [`QueryInput`]; the table calls [`cell_highlights`] for every visible
//! cell; the status line shows the search spinner from [`SearchState`].

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    },
    time::{Duration, Instant},
};

use tachy_core::{
    column::{ColumnMeta, ColumnName},
    query::{self, Predicate, QueryError, lexer::Keyword},
    search::{SearchQuery, SearchStatus, parse_search},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    input::LineEdit,
    query_history::QueryHistory,
    tab::{Tab, TabId},
};

/// Validation runs this long after the last keystroke (M4-04).
pub const VALIDATE_DEBOUNCE: Duration = Duration::from_millis(100);

/// Which bar is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    /// `filter ›` (`f`, `F`).
    Filter,
    /// `search ›` (`/`).
    Search,
}

impl BarKind {
    /// The prompt drawn before the input.
    pub fn prompt(self) -> &'static str {
        match self {
            BarKind::Filter => "filter › ",
            BarKind::Search => "search › ",
        }
    }
}

/// What line 2 of the bar shows before `Enter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validation {
    /// Empty, valid, or waiting for the debounce: the dim hint.
    Hint,
    /// The text doesn't parse, resolve or compile. `span` (filter only) is
    /// underlined with `^~~~`.
    Invalid {
        message: String,
        span: Option<Range<usize>>,
    },
}

/// Tab completion in progress: a repeated `Tab` cycles through
/// `candidates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Byte offset where the completed word starts.
    pub start: usize,
    /// Texts to insert, in order.
    pub candidates: Vec<String>,
    /// The candidate inserted last.
    pub idx: usize,
    /// The bar's text and cursor right after the last insertion: a `Tab`
    /// with anything else typed since starts a new completion.
    pub after: (String, usize),
}

/// The open filter or search bar.
#[derive(Debug, Clone)]
pub struct QueryInput {
    pub kind: BarKind,
    /// The tab the bar was opened on: its columns validate the text.
    pub tab: TabId,
    pub edit: LineEdit,
    pub validation: Validation,
    /// When the debounced validation runs (`None`: up to date).
    pub validate_at: Option<Instant>,
    pub completion: Option<Completion>,
}

impl QueryInput {
    pub fn new(kind: BarKind, tab: TabId, text: impl Into<String>) -> Self {
        QueryInput {
            kind,
            tab,
            edit: LineEdit::with_text(text),
            validation: Validation::Hint,
            validate_at: None,
            completion: None,
        }
    }
}

/// The search the highlighting and `n`/`N` use (M4-05).
#[derive(Debug, Clone)]
pub struct ActiveSearch {
    pub tab: TabId,
    pub query: Arc<SearchQuery>,
}

/// The one search request running (single-flight, §10.1).
#[derive(Debug)]
pub struct SearchInflight {
    /// Request id: a `Msg::SearchDone` with another id is stale.
    pub id: u64,
    pub tab: TabId,
    pub cancel: CancellationToken,
    /// `waiting for index…` vs `searching…`.
    pub status: Arc<SearchStatus>,
    /// Spinner frame, advanced per 100 ms tick.
    pub frame: usize,
}

/// Search state (M4-05).
#[derive(Debug, Default)]
pub struct SearchState {
    pub active: Option<ActiveSearch>,
    pub inflight: Option<SearchInflight>,
    /// The id of the last request started.
    pub last_request: u64,
    /// The task of the last request. The next request waits for it to end
    /// (it was cancelled first), so two searches never run at once.
    pub last_task: Option<JoinHandle<()>>,
    /// Searches running on the executor right now (at most 1).
    pub running: Arc<AtomicUsize>,
    /// Set if a search ever started while another was running (tests).
    pub overlapped: Arc<AtomicBool>,
}

impl SearchState {
    /// The running request, if it hasn't been cancelled.
    pub fn running_request(&self) -> Option<&SearchInflight> {
        self.inflight.as_ref().filter(|i| !i.cancel.is_cancelled())
    }

    /// Cancels the running request (`Esc`, a dialect change, a new search).
    pub fn cancel(&mut self) {
        if let Some(i) = self.inflight.take() {
            i.cancel.cancel();
        }
    }
}

/// Everything the filter and search bars keep (M4-04, M4-05).
#[derive(Debug, Default)]
pub struct FindState {
    /// The open bar (`Mode::Filter` / `Mode::Search`).
    pub bar: Option<QueryInput>,
    pub filter_history: QueryHistory,
    pub search_history: QueryHistory,
    pub search: SearchState,
    /// `--filter` was applied (or failed) on the first tab.
    pub startup_filter_done: bool,
    /// `Ctrl-S` in the filter bar: saving it as a named view (M6-04).
    pub save: Option<SaveFlow>,
}

/// The steps of `Ctrl-S` (M6-04): a name, a file glob, and a confirmation
/// when the name is already in `views.json`. Drawn on line 2 of the bar.
#[derive(Debug, Clone)]
pub struct SaveFlow {
    /// The filter text being saved (validated when the flow started).
    pub expr: String,
    pub step: SaveStep,
    pub name: LineEdit,
    /// Pre-filled with the current file's name (an exact-match glob).
    pub glob: LineEdit,
    /// An inline error (bad name, bad glob, write failure).
    pub error: Option<String>,
    /// A dim note: the name shadows a config entry.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveStep {
    Name,
    Glob,
    /// `overwrite view "X"? (y/n)`.
    Overwrite,
    /// Writing `views.json`.
    Writing,
}

impl FindState {
    pub fn history_mut(&mut self, kind: BarKind) -> &mut QueryHistory {
        match kind {
            BarKind::Filter => &mut self.filter_history,
            BarKind::Search => &mut self.search_history,
        }
    }

    /// The tick must run: a validation is waiting for its debounce, or a
    /// search spinner turns.
    pub fn busy(&self) -> bool {
        self.bar.as_ref().is_some_and(|b| b.validate_at.is_some())
            || self.search.running_request().is_some()
    }
}

// ---- validation ------------------------------------------------------------

/// Column names for `resolve` and `highlight`, in column order.
pub fn column_names(columns: &[ColumnMeta]) -> Vec<ColumnName> {
    columns.iter().map(|c| c.name.clone()).collect()
}

/// Parses, resolves and compiles a filter against `tab` (§9.2).
pub fn compile_filter(text: &str, tab: &Tab) -> Result<Predicate, QueryError> {
    let Some(l) = &tab.loaded else {
        return Err(QueryError::new("the file is not open yet", 0..0));
    };
    let ast = query::parse(text)?;
    let resolved = query::resolve(ast, &column_names(&l.columns))?;
    query::compile(&resolved, &l.columns, l.source.dialect(), &tab.nulls)
}

/// Line 2 of the bar for `text`: the hint when it is empty or valid.
pub fn validate(kind: BarKind, text: &str, tab: &Tab) -> Validation {
    if text.trim().is_empty() {
        return Validation::Hint;
    }
    match kind {
        BarKind::Filter => match compile_filter(text, tab) {
            Ok(_) => Validation::Hint,
            Err(e) => Validation::Invalid {
                message: e.message,
                span: Some(e.span),
            },
        },
        BarKind::Search => {
            let columns = tab.loaded.as_ref().map_or(&[][..], |l| &l.columns[..]);
            match parse_search(text, columns) {
                Ok(_) => Validation::Hint,
                Err(message) => Validation::Invalid {
                    message,
                    span: None,
                },
            }
        }
    }
}

// ---- Tab completion --------------------------------------------------------

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// How a column name is written in a filter: bare when it is an
/// identifier (and not a keyword), backticked otherwise.
pub fn quote_column(name: &str) -> String {
    let bare = query::lexer::is_ident(name) && Keyword::from_ident(name).is_none();
    if bare {
        name.to_owned()
    } else {
        format!("`{name}`")
    }
}

/// The word being completed in a filter: `(start, prefix)`. Inside an open
/// backtick the word starts at the backtick and may hold any character.
fn filter_word(text: &str, cursor: usize) -> (usize, &str) {
    let head = &text[..cursor];
    if head.matches('`').count() % 2 == 1
        && let Some(tick) = head.rfind('`')
    {
        return (tick, &head[tick + 1..]);
    }
    let start = head
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_ident_char(c))
        .last()
        .map_or(cursor, |(i, _)| i);
    (start, &head[start..])
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Candidates for completing at the cursor: `(start, candidates)`.
///
/// - Filter: column query names with the word as a case-insensitive prefix
///   (backticked when needed), then keywords.
/// - Search: column display names, only before the `:` of a `col:` prefix.
pub fn completion_candidates(
    kind: BarKind,
    text: &str,
    cursor: usize,
    columns: &[ColumnMeta],
) -> Option<(usize, Vec<String>)> {
    match kind {
        BarKind::Filter => {
            let (start, prefix) = filter_word(text, cursor);
            if prefix.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let mut out: Vec<String> = columns
                .iter()
                .filter(|c| starts_with_ci(&c.name.query, prefix))
                .map(|c| quote_column(&c.name.query))
                .collect();
            if !text[start..].starts_with('`') {
                out.extend(
                    Keyword::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .filter(|k| !prefix.is_empty() && starts_with_ci(k, prefix))
                        .map(str::to_owned),
                );
            }
            out.dedup();
            (!out.is_empty()).then_some((start, out))
        }
        BarKind::Search => {
            let head = &text[..cursor];
            if head.contains(':') {
                return None;
            }
            let start = head.len() - head.trim_start().len();
            let prefix = head.trim_start();
            let out: Vec<String> = columns
                .iter()
                .filter(|c| starts_with_ci(&c.name.display, prefix))
                .map(|c| c.name.display.clone())
                .collect();
            (!out.is_empty()).then_some((start, out))
        }
    }
}

/// `Tab` in the bar: completes the word at the cursor, or cycles to the next
/// candidate when nothing changed since the previous `Tab`. Returns whether
/// the text changed.
pub fn complete(bar: &mut QueryInput, columns: &[ColumnMeta]) -> bool {
    let text = bar.edit.text().to_owned();
    let cursor = bar.edit.cursor();
    if let Some(c) = bar.completion.as_mut()
        && c.after == (text.clone(), cursor)
        && c.candidates.len() > 1
    {
        c.idx = (c.idx + 1) % c.candidates.len();
        let next = c.candidates[c.idx].clone();
        let start = c.start;
        bar.edit.replace_range(start..cursor, &next);
        c.after = (bar.edit.text().to_owned(), bar.edit.cursor());
        return true;
    }
    let Some((start, candidates)) = completion_candidates(bar.kind, &text, cursor, columns) else {
        bar.completion = None;
        return false;
    };
    // A closing backtick right after the cursor belongs to the name.
    let end = if text[start..].starts_with('`') && text[cursor..].starts_with('`') {
        cursor + 1
    } else {
        cursor
    };
    bar.edit.replace_range(start..end, &candidates[0]);
    bar.completion = Some(Completion {
        start,
        candidates,
        idx: 0,
        after: (bar.edit.text().to_owned(), bar.edit.cursor()),
    });
    bar.edit.text() != text
}

// ---- cell highlighting -----------------------------------------------------

/// Sorts and merges overlapping ranges.
fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|r| r.start < r.end);
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// The predicate of the active view's filter, for `match_highlight` on the
/// cells of referenced columns (§12.4).
///
/// Kept on the view's `ViewEntry` by the filter starter.
fn active_filter_predicate(tab: &Tab) -> Option<&Predicate> {
    tab.active_entry().pred.as_ref()
}

/// Byte ranges of `cell` (the displayed text of source column `col` on a
/// visible row of `tab`) to draw in `match_highlight` (§8.3, §12.4):
/// - the active search's matches, in the columns it searches;
/// - in a filtered view, the filter's matches: the matched substring for
///   `contains`, `starts`, `ends`, `~` and string `==`, the whole cell for
///   other comparisons on that column.
pub fn cell_highlights(
    search: &SearchState,
    tab: &Tab,
    col: usize,
    cell: &str,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    if let Some(active) = &search.active
        && active.tab == tab.id
        && active.query.searches_column(col, true)
    {
        ranges.extend(active.query.find_in_cell(cell));
    }
    if let Some(pred) = active_filter_predicate(tab) {
        ranges.extend(filter_highlights(pred, col, cell));
    }
    merge(ranges)
}

/// The filter part of [`cell_highlights`].
pub fn filter_highlights(pred: &Predicate, col: usize, cell: &str) -> Vec<Range<usize>> {
    if !pred.columns().contains(&col) {
        return Vec::new();
    }
    let mut rules = pred.highlight_rules(col).peekable();
    if rules.peek().is_none() {
        #[allow(clippy::single_range_in_vec_init)]
        return vec![0..cell.len()];
    }
    merge(rules.flat_map(|r| r.find(cell)).collect())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tachy_core::column::ColumnMeta;

    use super::*;

    fn cols(names: &[&str]) -> Vec<ColumnMeta> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                ColumnMeta::new(
                    ColumnName {
                        display: (*n).to_owned(),
                        query: n.replace(' ', "_"),
                    },
                    i,
                    false,
                )
            })
            .collect()
    }

    fn bar(kind: BarKind, text: &str) -> QueryInput {
        QueryInput::new(kind, TabId(1), text)
    }

    #[test]
    fn filter_completion_prefers_columns_then_keywords() {
        let c = cols(&["price", "country", "customer", "Order Id"]);
        assert_eq!(
            completion_candidates(BarKind::Filter, "c", 1, &c),
            Some((
                0,
                vec![
                    "country".to_owned(),
                    "customer".to_owned(),
                    "contains".to_owned()
                ]
            ))
        );
        assert_eq!(
            completion_candidates(BarKind::Filter, "price > 1 an", 12, &c),
            Some((10, vec!["and".to_owned()]))
        );
        // Case-insensitive prefix; numbers aren't words.
        assert_eq!(
            completion_candidates(BarKind::Filter, "PR", 2, &c),
            Some((0, vec!["price".to_owned()]))
        );
        assert_eq!(
            completion_candidates(BarKind::Filter, "price > 1", 9, &c),
            None
        );
        assert_eq!(completion_candidates(BarKind::Filter, "zz", 2, &c), None);
    }

    #[test]
    fn names_outside_the_identifier_set_are_backticked() {
        assert_eq!(quote_column("price"), "price");
        assert_eq!(quote_column("order id"), "`order id`");
        assert_eq!(quote_column("2nd"), "`2nd`");
        assert_eq!(quote_column("in"), "`in`");
        let mut c = cols(&["price"]);
        c.push(ColumnMeta::new(
            ColumnName {
                display: "order id".into(),
                query: "order id".into(),
            },
            1,
            false,
        ));
        let mut b = bar(BarKind::Filter, "or");
        assert!(complete(&mut b, &c));
        assert_eq!(b.edit.text(), "`order id`");
        // Inside an open backtick: completes and closes it.
        let mut b = bar(BarKind::Filter, "`ord");
        assert!(complete(&mut b, &c));
        assert_eq!(b.edit.text(), "`order id`");
    }

    #[test]
    fn repeated_tab_cycles() {
        let c = cols(&["price", "country", "customer"]);
        let mut b = bar(BarKind::Filter, "x == 1 && c");
        assert!(complete(&mut b, &c));
        assert_eq!(b.edit.text(), "x == 1 && country");
        complete(&mut b, &c);
        assert_eq!(b.edit.text(), "x == 1 && customer");
        complete(&mut b, &c);
        assert_eq!(b.edit.text(), "x == 1 && contains");
        complete(&mut b, &c);
        assert_eq!(b.edit.text(), "x == 1 && country");
        // Typing starts a new completion.
        b.edit.insert("z");
        assert!(!complete(&mut b, &c));
        assert_eq!(b.completion, None);
    }

    #[test]
    fn search_completes_column_names_before_the_colon() {
        let c = cols(&["customer", "country", "price"]);
        let mut b = bar(BarKind::Search, "cus");
        assert!(complete(&mut b, &c));
        assert_eq!(b.edit.text(), "customer");
        assert_eq!(
            completion_candidates(BarKind::Search, "customer: be", 12, &c),
            None
        );
        assert_eq!(
            completion_candidates(BarKind::Search, "  co", 4, &c),
            Some((2, vec!["country".to_owned()]))
        );
    }

    #[test]
    fn ranges_are_merged() {
        assert_eq!(merge(vec![5..7, 0..2, 1..3, 7..8, 9..9]), vec![0..3, 5..8]);
    }
}
