//! Incremental search over a view, including `re:` and `col:` prefixes
//! (spec §8.3; M4-05).
//!
//! Search moves the cursor; it never changes the view and is not a job
//! (§10.1). The UI runs one request at a time ([`run_search`] on the
//! executor) and cancels the previous one through its token.
//!
//! # Syntax ([`parse_search`])
//!
//! - plain text: a case-sensitive substring, in every searchable column;
//! - `re:<regex>`: a regex (`regex::bytes`), matched against each field
//!   value, so `^` and `$` anchor at the field's ends;
//! - `<col>: <text>` (whitespace after `:` optional and trimmed): only column
//!   `<col>`, matched with the go-to column rules ([`match_column`]). It
//!   combines with a regex: `customer: re:^bec`. When the text before `:`
//!   is not a column (or is ambiguous) the whole input is plain text, so
//!   `error: disk full` searches for that text when there is no `error`
//!   column. An input starting with `re:` is always a regex, even if a
//!   column is named `re`.
//!
//! # Matching engine ([`search_blocking`])
//!
//! Matches are cells, in **view order**: forward from the cursor cell, later
//! cells of the same row first (in display order), then later rows; wrapping
//! to the top when needed ([`SearchOutcome::wrapped`]). Backward is the
//! mirror image.
//!
//! - `All` view, literal needle: the memory map is scanned with
//!   `memmem::Finder` (`FinderRev` backward) on raw bytes, in 64 KiB windows
//!   (the cancellation interval). For each hit, the record that contains it
//!   is found from the nearest checkpoint ≤ the hit, parsing forward (the
//!   last parsed position is cached, so consecutive hits do not re-parse).
//!   The record's searchable fields are then checked on their **unescaped**
//!   values, so hits across a delimiter, inside `""` escapes or across a
//!   quoted newline that do not match a value are rejected (false
//!   positives), and the scan continues after the record.
//!   A hit beyond the published checkpoints waits for the index to cover it
//!   ([`SearchStatus::waiting_for_index`]).
//! - Regex needles, needles containing the quote byte (or `\` with
//!   backslash escapes, whose raw bytes differ from the value), and
//!   `Filtered` / `Ordered` views: row ids are iterated in view order in
//!   batches of 4,096 and each record's searchable fields are tested
//!   directly. Backward, each batch is parsed forward and the last match is
//!   kept.
//! - Windows-1252 files: a literal needle is encoded to Windows-1252 first;
//!   a needle that cannot be encoded matches nothing. A regex is matched
//!   against the decoded (UTF-8) value; its `byte_range_in_value` is then a
//!   range of the decoded text.
//!
//! Known limit (shared with the filter pre-filter): a malformed quoted field
//! with bytes after its closing quote (`"ab"cd`, value `abcd`) can match in
//! its value but not in its raw bytes, so the `All` fast path misses a
//! needle spanning the closing quote.

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
use tokio_util::sync::CancellationToken;

use crate::{
    column::ColumnMeta,
    dialect::{Encoding, EscapeStyle},
    exec::Executor,
    filter::RowSeeker,
    index::RowIndex,
    jobs::{CHECK_INTERVAL, JobError},
    parse::{ParseOutcome, RecordParser, RecordRanges, decode_field},
    query::compile::REGEX_SIZE_LIMIT,
    source::Source,
    view::View,
};

/// Row ids per batch in the record path.
pub const SEARCH_BATCH: u64 = 4096;
/// How long the search sleeps while waiting for the index.
const WAIT: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

/// What to look for.
#[derive(Clone, Debug)]
pub enum SearchPattern {
    /// A case-sensitive substring (UTF-8, as typed).
    Literal(String),
    /// A regex matched against each field value.
    Regex(Regex),
}

/// A parsed search ([`parse_search`]).
#[derive(Clone, Debug)]
pub struct SearchQuery {
    /// The input as typed (for history and the `no match for "x"` toast).
    pub text: String,
    /// The needle.
    pub pattern: SearchPattern,
    /// `Some(c)`: only column `c` (index into the tab's columns) is searched.
    pub column: Option<usize>,
}

impl SearchQuery {
    /// Byte ranges of `cell` (a decoded display string) that match, in order
    /// and non-overlapping: the UI highlights them on every visible cell
    /// (§8.3).
    pub fn find_in_cell(&self, cell: &str) -> Vec<Range<usize>> {
        match &self.pattern {
            SearchPattern::Literal(n) if n.is_empty() => Vec::new(),
            SearchPattern::Literal(n) => cell
                .match_indices(n.as_str())
                .map(|(i, m)| i..i + m.len())
                .collect(),
            SearchPattern::Regex(re) => re
                .find_iter(cell.as_bytes())
                .filter(|m| !m.is_empty())
                .map(|m| m.range())
                .collect(),
        }
    }

    /// Whether `column` is searched, given whether it is visible.
    pub fn searches_column(&self, column: usize, visible: bool) -> bool {
        match self.column {
            Some(c) => c == column,
            None => visible,
        }
    }
}

/// Finds a column by the go-to rules (M2-03): exact display name, exact
/// query name, case-insensitive name, then a unique case-insensitive prefix.
/// Errors: `ambiguous: a, b, …` (at most 5 names) or `no such column "x"`.
pub fn match_column(name: &str, columns: &[ColumnMeta]) -> Result<usize, String> {
    if let Some(i) = columns.iter().position(|c| c.name.display == name) {
        return Ok(i);
    }
    if let Some(i) = columns.iter().position(|c| c.name.query == name) {
        return Ok(i);
    }
    let lower = name.to_lowercase();
    let pick = |hits: Vec<usize>| -> Option<Result<usize, String>> {
        match hits.len() {
            0 => None,
            1 => Some(Ok(hits[0])),
            n => {
                let mut names: Vec<&str> = hits
                    .iter()
                    .take(5)
                    .map(|&i| columns[i].name.display.as_str())
                    .collect();
                if n > 5 {
                    names.push("…");
                }
                Some(Err(format!("ambiguous: {}", names.join(", "))))
            }
        }
    };
    let ci: Vec<usize> = (0..columns.len())
        .filter(|&i| {
            columns[i].name.display.to_lowercase() == lower
                || columns[i].name.query.to_lowercase() == lower
        })
        .collect();
    if let Some(r) = pick(ci) {
        return r;
    }
    let prefix: Vec<usize> = (0..columns.len())
        .filter(|&i| {
            !lower.is_empty()
                && (columns[i].name.display.to_lowercase().starts_with(&lower)
                    || columns[i].name.query.to_lowercase().starts_with(&lower))
        })
        .collect();
    pick(prefix).unwrap_or_else(|| Err(format!("no such column \"{name}\"")))
}

fn compile_regex(src: &str) -> Result<Regex, String> {
    if src.is_empty() {
        return Err("empty regex".to_owned());
    }
    RegexBuilder::new(src)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| {
            let msg = e.to_string();
            let last = msg.lines().rev().find(|l| !l.trim().is_empty());
            format!("invalid regex: {}", last.unwrap_or(&msg).trim())
        })
}

fn pattern(text: &str) -> Result<SearchPattern, String> {
    match text.strip_prefix("re:") {
        Some(re) => compile_regex(re).map(SearchPattern::Regex),
        None if text.trim().is_empty() => Err("empty search".to_owned()),
        None => Ok(SearchPattern::Literal(text.to_owned())),
    }
}

/// Parses a search (see the module docs). Errors are shown inline; an empty
/// (or all-blank) input is `Err("empty search")`, and `Enter` should simply
/// do nothing for it.
pub fn parse_search(input: &str, columns: &[ColumnMeta]) -> Result<SearchQuery, String> {
    if input.trim().is_empty() {
        return Err("empty search".to_owned());
    }
    if !input.starts_with("re:")
        && let Some(i) = input.find(':')
    {
        let name = input[..i].trim();
        if !name.is_empty()
            && let Ok(col) = match_column(name, columns)
        {
            let rest = input[i + 1..].trim_start();
            return Ok(SearchQuery {
                text: input.to_owned(),
                pattern: pattern(rest)?,
                column: Some(col),
            });
        }
    }
    Ok(SearchQuery {
        text: input.to_owned(),
        pattern: pattern(input)?,
        column: None,
    })
}

// ---------------------------------------------------------------------------
// Request / outcome
// ---------------------------------------------------------------------------

/// Search direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// `Enter` and `n`.
    Forward,
    /// `N`.
    Backward,
}

/// A searchable column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchColumn {
    /// Index into the tab's columns.
    pub column: usize,
    /// Field position in the record (`ColumnMeta::source_index`).
    pub field: usize,
    /// Display slot (position among the visible columns); cells are visited
    /// in slot order.
    pub slot: usize,
}

/// One search: what, where from, which way.
#[derive(Clone, Debug)]
pub struct SearchRequest {
    /// The query.
    pub query: Arc<SearchQuery>,
    /// Searchable columns, sorted by slot.
    pub columns: Vec<SearchColumn>,
    /// The cursor's view position.
    pub from_pos: u64,
    /// The cursor cell's display slot.
    pub from_slot: usize,
    /// Which way.
    pub direction: Direction,
    /// Wrap around the view's end (always on in the UI).
    pub wrap: bool,
}

impl SearchRequest {
    /// A request from the cursor at view position `pos`, on column
    /// `cursor_col` (index into `cols`). `display` lists the **visible**
    /// columns in display order. Without `col:`, the visible columns are
    /// searched; with `col:` only that column (even when hidden; it then
    /// sorts after every visible slot).
    pub fn new(
        query: Arc<SearchQuery>,
        cols: &[ColumnMeta],
        display: &[usize],
        pos: u64,
        cursor_col: usize,
        direction: Direction,
    ) -> SearchRequest {
        let slot_of = |c: usize| display.iter().position(|&d| d == c);
        let columns = match query.column {
            Some(c) => cols
                .get(c)
                .map(|m| SearchColumn {
                    column: c,
                    field: m.source_index,
                    slot: slot_of(c).unwrap_or(display.len()),
                })
                .into_iter()
                .collect(),
            None => display
                .iter()
                .enumerate()
                .filter_map(|(slot, &c)| {
                    cols.get(c).map(|m| SearchColumn {
                        column: c,
                        field: m.source_index,
                        slot,
                    })
                })
                .collect(),
        };
        SearchRequest {
            query,
            columns,
            from_pos: pos,
            from_slot: slot_of(cursor_col).unwrap_or(0),
            direction,
            wrap: true,
        }
    }
}

/// A matching cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// Its view position (the row id for `All`).
    pub pos: u64,
    /// Its row id.
    pub row_id: u64,
    /// Its column (index into the tab's columns).
    pub col: usize,
    /// The match inside the unescaped value (source encoding; decoded text
    /// for a regex on a Windows-1252 file).
    pub byte_range_in_value: Range<usize>,
}

/// Result of a search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOutcome {
    /// The match, `None` when nothing matches (`no match for "x"`).
    pub hit: Option<SearchHit>,
    /// The search wrapped around the view's end (`search wrapped`).
    pub wrapped: bool,
}

/// Shared state the UI reads while a search runs (spinner text).
#[derive(Debug, Default)]
pub struct SearchStatus {
    waiting: AtomicBool,
}

impl SearchStatus {
    /// True while the search waits for the index (`waiting for index…`).
    pub fn waiting_for_index(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

enum Matcher {
    /// Never matches (a needle that cannot be encoded).
    Nothing,
    Literal(Box<memmem::Finder<'static>>),
    Regex {
        re: Regex,
        decode: bool,
    },
}

impl Matcher {
    fn new(q: &SearchQuery, enc: Encoding) -> Matcher {
        match &q.pattern {
            SearchPattern::Literal(t) if t.is_empty() => Matcher::Nothing,
            SearchPattern::Literal(t) => match enc {
                Encoding::Windows1252 => {
                    let (bytes, _, unmappable) = encoding_rs::WINDOWS_1252.encode(t);
                    if unmappable {
                        Matcher::Nothing
                    } else {
                        Matcher::Literal(Box::new(memmem::Finder::new(&bytes[..]).into_owned()))
                    }
                }
                _ => Matcher::Literal(Box::new(memmem::Finder::new(t.as_bytes()).into_owned())),
            },
            SearchPattern::Regex(re) => Matcher::Regex {
                re: re.clone(),
                decode: enc == Encoding::Windows1252,
            },
        }
    }

    fn find(&self, value: &[u8], text: &mut String) -> Option<Range<usize>> {
        match self {
            Matcher::Nothing => None,
            Matcher::Literal(f) => f.find(value).map(|i| i..i + f.needle().len()),
            Matcher::Regex { re, decode: false } => re.find(value).map(|m| m.range()),
            Matcher::Regex { re, decode: true } => {
                text.clear();
                text.push_str(&decode_field(value, Encoding::Windows1252));
                re.find(text.as_bytes()).map(|m| m.range())
            }
        }
    }
}

/// A match, before its view position is attached.
struct Found {
    row: u64,
    col: usize,
    range: Range<usize>,
}

/// Which display slots of a row to check.
#[derive(Clone, Copy)]
enum Slots {
    All,
    After(usize),
    Before(usize),
    AtOrBefore(usize),
    AtOrAfter(usize),
}

impl Slots {
    fn contains(self, s: usize) -> bool {
        match self {
            Slots::All => true,
            Slots::After(x) => s > x,
            Slots::Before(x) => s < x,
            Slots::AtOrBefore(x) => s <= x,
            Slots::AtOrAfter(x) => s >= x,
        }
    }
}

type PosFound = Option<(u64, Found)>;

struct Engine<'a> {
    src: &'a Source,
    index: &'a RowIndex,
    view: &'a View,
    req: &'a SearchRequest,
    cancel: &'a CancellationToken,
    status: &'a SearchStatus,
    matcher: Matcher,
    seeker: RowSeeker,
    parser: RecordParser,
    rec: RecordRanges,
    scratch: Vec<u8>,
    /// Edited value (`crate::edit`).
    edited: Vec<u8>,
    text: String,
    since: u64,
}

impl Engine<'_> {
    fn check(&self) -> Result<(), JobError> {
        if self.cancel.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn tick(&mut self, bytes: u64) -> Result<(), JobError> {
        self.since += bytes;
        if self.since >= CHECK_INTERVAL as u64 {
            self.since = 0;
            self.check()?;
        }
        Ok(())
    }

    /// Waits while the index is incomplete and `ready()` is false.
    fn wait_index(&self, ready: impl Fn(&RowIndex) -> bool) -> Result<(), JobError> {
        while !self.index.is_complete() && !ready(self.index) {
            self.status.waiting.store(true, Ordering::Relaxed);
            self.check()?;
            std::thread::sleep(WAIT);
        }
        self.status.waiting.store(false, Ordering::Relaxed);
        self.check()
    }

    /// Waits until at least `rows` rows are indexed (or the index is done).
    fn wait_rows(&self, rows: u64) -> Result<(), JobError> {
        self.wait_index(|i| i.indexed_rows() >= rows)
    }

    /// Waits until the published checkpoints extend past byte `offset`.
    fn wait_offset(&self, offset: u64) -> Result<(), JobError> {
        self.wait_index(|i| {
            let last = i.published_checkpoints().saturating_sub(1);
            i.checkpoint(last).is_some_and(|o| o > offset)
        })
    }

    /// Tests the record in `self.rec`: the first (forward) or last
    /// (`last`, backward) searchable cell within `slots` that matches.
    fn match_record(&mut self, slots: Slots, last: bool) -> Option<(usize, Range<usize>)> {
        let bytes = self.src.bytes();
        let mut best = None;
        for c in &self.req.columns {
            if !slots.contains(c.slot) || c.field >= self.rec.fields.len() {
                continue;
            }
            let v = self
                .parser
                .field_value(bytes, &self.rec, c.field, &mut self.scratch);
            let v = self.src.edits().apply(c.field, v, &mut self.edited);
            if let Some(r) = self.matcher.find(v, &mut self.text) {
                best = Some((c.column, r));
                if !last {
                    break;
                }
            }
        }
        best
    }

    /// Parses row `id` into `self.rec` through the seeker.
    fn load(&mut self, id: u64) -> Result<bool, JobError> {
        match self.seeker.parse(self.src, self.index, id, &mut self.rec) {
            Some(n) => {
                self.tick(n)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Checks some cells of the row at view position `pos`.
    fn check_row(&mut self, pos: u64, slots: Slots, last: bool) -> Result<Option<Found>, JobError> {
        if !self.view.is_all() && pos >= self.view.len(self.index) {
            return Ok(None);
        }
        let Some(id) = self.view.row_id_at(pos) else {
            return Ok(None);
        };
        if self.view.is_all() {
            self.wait_rows(id + 1)?;
        }
        if !self.load(id)? {
            return Ok(None);
        }
        Ok(self.match_record(slots, last).map(|(col, range)| Found {
            row: id,
            col,
            range,
        }))
    }

    /// Current length of the view; for `All` while indexing, waits until
    /// `want` rows exist or the index is complete.
    fn view_len(&self, want: u64) -> Result<u64, JobError> {
        if self.view.is_all() {
            self.wait_rows(want)?;
        }
        Ok(self.view.len(self.index))
    }

    /// The record path, forward over positions `from .. until` (to the
    /// view's end when `None`).
    fn records_forward(&mut self, from: u64, until: Option<u64>) -> Result<PosFound, JobError> {
        let mut p = from;
        loop {
            let len = self.view_len(p + 1)?;
            let end = until.map_or(len, |u| u.min(len));
            if p >= end {
                return Ok(None);
            }
            let n = (end - p).min(SEARCH_BATCH) as usize;
            let ids = self.view.row_ids(p, n);
            if ids.is_empty() {
                return Ok(None);
            }
            for (i, &id) in ids.iter().enumerate() {
                if !self.load(id)? {
                    continue;
                }
                if let Some((col, range)) = self.match_record(Slots::All, false) {
                    return Ok(Some((
                        p + i as u64,
                        Found {
                            row: id,
                            col,
                            range,
                        },
                    )));
                }
            }
            p += ids.len() as u64;
        }
    }

    /// The record path, backward over positions `lower .. upper` (from the
    /// view's end when `None`): each batch is parsed forward and its last
    /// match kept.
    fn records_backward(&mut self, upper: Option<u64>, lower: u64) -> Result<PosFound, JobError> {
        let len = self.view_len(u64::MAX)?;
        let mut hi = upper.map_or(len, |u| u.min(len));
        while hi > lower {
            let lo = hi.saturating_sub(SEARCH_BATCH).max(lower);
            let ids = self.view.row_ids(lo, (hi - lo) as usize);
            let mut best = None;
            for (i, &id) in ids.iter().enumerate() {
                if !self.load(id)? {
                    continue;
                }
                if let Some((col, range)) = self.match_record(Slots::All, true) {
                    best = Some((
                        lo + i as u64,
                        Found {
                            row: id,
                            col,
                            range,
                        },
                    ));
                }
            }
            if best.is_some() {
                return Ok(best);
            }
            hi = lo;
        }
        Ok(None)
    }

    /// Largest checkpoint index whose offset is ≤ `offset`.
    fn checkpoint_le(&self, offset: u64) -> u64 {
        let (mut lo, mut hi) = (0u64, self.index.published_checkpoints());
        // Invariant: checkpoint(lo) ≤ offset (checkpoint 0 is data_start).
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.index.checkpoint(mid).is_some_and(|o| o <= offset) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The record containing byte `h`, walking forward from `walker`
    /// (`(row, offset)` of a record boundary at or before `h`), after
    /// jumping to a closer checkpoint. Returns `(row, start)`, or `None` when
    /// `h` lies in a blank or comment line (or past the last record).
    fn locate(&mut self, walker: &mut (u64, u64), h: u64) -> Result<Option<(u64, u64)>, JobError> {
        let stride = self.index.stride();
        let k = self.checkpoint_le(h);
        if k * stride > walker.0
            && let Some(off) = self.index.checkpoint(k)
        {
            *walker = (k * stride, off);
        }
        let bytes = self.src.bytes();
        loop {
            let Some((start, next, _)) = self.parser.next_record(bytes, walker.1) else {
                return Ok(None);
            };
            if next > h {
                return Ok((start <= h).then_some((walker.0, start)));
            }
            self.tick(next - walker.1)?;
            *walker = (walker.0 + 1, next);
        }
    }

    /// Parses the record at `start` into `self.rec`; returns the offset after
    /// it.
    fn parse_found(&mut self, start: u64) -> u64 {
        let bytes = self.src.bytes();
        match self.parser.parse_at(bytes, start, &mut self.rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
            ParseOutcome::Eof => bytes.len() as u64,
        }
    }

    /// Start offset of row `row` (waiting for the index), or `fallback` when
    /// the row is past the end.
    fn offset_or(&mut self, row: u64, fallback: u64) -> Result<u64, JobError> {
        self.wait_rows(row + 1)?;
        Ok(self
            .index
            .offset_of(row, self.src, &mut self.parser)
            .unwrap_or(fallback))
    }

    /// The `All` fast path, forward over rows `from .. until`.
    fn fast_forward(
        &mut self,
        finder: &memmem::Finder<'_>,
        from: u64,
        until: Option<u64>,
    ) -> Result<Option<Found>, JobError> {
        let n = finder.needle().len();
        let bytes = self.src.bytes();
        let len = bytes.len() as u64;
        let start = self.offset_or(from, len)?;
        let end = match until {
            Some(u) => self.offset_or(u, len)?,
            None => len,
        } as usize;
        let mut walker = (from, start);
        let mut pos = start as usize;
        let window = CHECK_INTERVAL.max(2 * n);
        while pos < end {
            self.check()?;
            let win_end = (pos + window).min(end);
            let search_end = (win_end + n - 1).min(end);
            let hit = match finder.find(&bytes[pos..search_end]) {
                Some(i) if pos + i < win_end => pos + i,
                _ => {
                    pos = win_end;
                    continue;
                }
            };
            self.wait_offset(hit as u64)?;
            let Some((row, rec_start)) = self.locate(&mut walker, hit as u64)? else {
                pos = hit + 1;
                continue;
            };
            if until.is_some_and(|u| row >= u) {
                return Ok(None);
            }
            let next = self.parse_found(rec_start);
            walker = (row + 1, next);
            if let Some((col, range)) = self.match_record(Slots::All, false) {
                return Ok(Some(Found { row, col, range }));
            }
            // A false positive: skip the rest of the record.
            pos = (next as usize).max(hit + 1);
        }
        Ok(None)
    }

    /// The `All` fast path, backward over rows `lower .. upper` (from EOF
    /// when `None`).
    fn fast_backward(
        &mut self,
        finder: &memmem::FinderRev<'_>,
        upper: Option<u64>,
        lower: u64,
    ) -> Result<Option<Found>, JobError> {
        let n = finder.needle().len();
        let bytes = self.src.bytes();
        let len = bytes.len() as u64;
        let lo = self.offset_or(lower, len)? as usize;
        let mut e = match upper {
            Some(u) => self.offset_or(u, len)?,
            None => len,
        } as usize;
        let window = CHECK_INTERVAL.max(2 * n);
        let stride = self.index.stride();
        while e > lo {
            self.check()?;
            let ws = e.saturating_sub(window).max(lo);
            let Some(i) = finder.rfind(&bytes[ws..e]) else {
                // Overlap the next window so a hit straddling `ws` is found.
                e = if ws == lo { lo } else { ws + n - 1 };
                continue;
            };
            let hit = ws + i;
            self.wait_offset(hit as u64)?;
            let k = self.checkpoint_le(hit as u64);
            let mut walker = (k * stride, self.index.checkpoint(k).unwrap_or(lo as u64));
            let Some((row, rec_start)) = self.locate(&mut walker, hit as u64)? else {
                e = hit + n - 1;
                continue;
            };
            if row < lower {
                return Ok(None);
            }
            self.parse_found(rec_start);
            if let Some((col, range)) = self.match_record(Slots::All, true) {
                return Ok(Some(Found { row, col, range }));
            }
            e = rec_start as usize;
        }
        Ok(None)
    }

    /// The literal needle when the `All` fast path applies.
    fn fast_needle(&self) -> Option<Vec<u8>> {
        let Matcher::Literal(f) = &self.matcher else {
            return None;
        };
        let d = self.src.dialect();
        let needle = f.needle();
        // An edited value is not in the raw bytes.
        let edited = self
            .req
            .columns
            .iter()
            .any(|c| self.src.edits().is_edited(c.field));
        (self.view.is_all()
            && !edited
            && d.quote.is_none_or(|q| !needle.contains(&q))
            && !(d.escape == EscapeStyle::Backslash && needle.contains(&b'\\')))
        .then(|| needle.to_vec())
    }

    fn forward_rows(&mut self, from: u64, until: Option<u64>) -> Result<PosFound, JobError> {
        match self.fast_needle() {
            Some(n) => {
                let f = memmem::Finder::new(&n);
                Ok(self.fast_forward(&f, from, until)?.map(|f| (f.row, f)))
            }
            None => self.records_forward(from, until),
        }
    }

    fn backward_rows(&mut self, upper: Option<u64>, lower: u64) -> Result<PosFound, JobError> {
        match self.fast_needle() {
            Some(n) => {
                let f = memmem::FinderRev::new(&n);
                Ok(self.fast_backward(&f, upper, lower)?.map(|f| (f.row, f)))
            }
            None => self.records_backward(upper, lower),
        }
    }

    fn run(&mut self) -> Result<SearchOutcome, JobError> {
        if matches!(self.matcher, Matcher::Nothing) || self.req.columns.is_empty() {
            return Ok(SearchOutcome::default());
        }
        let pos = self.req.from_pos;
        let slot = self.req.from_slot;
        let hit = |p: u64, f: Found| SearchHit {
            pos: p,
            row_id: f.row,
            col: f.col,
            byte_range_in_value: f.range,
        };
        let done = |h: Option<SearchHit>, wrapped: bool| SearchOutcome { hit: h, wrapped };
        match self.req.direction {
            Direction::Forward => {
                if let Some(f) = self.check_row(pos, Slots::After(slot), false)? {
                    return Ok(done(Some(hit(pos, f)), false));
                }
                if let Some((p, f)) = self.forward_rows(pos + 1, None)? {
                    return Ok(done(Some(hit(p, f)), false));
                }
                if !self.req.wrap {
                    return Ok(done(None, false));
                }
                if let Some((p, f)) = self.forward_rows(0, Some(pos))? {
                    return Ok(done(Some(hit(p, f)), true));
                }
                let f = self.check_row(pos, Slots::AtOrBefore(slot), false)?;
                Ok(done(f.map(|f| hit(pos, f)), true))
            }
            Direction::Backward => {
                if let Some(f) = self.check_row(pos, Slots::Before(slot), true)? {
                    return Ok(done(Some(hit(pos, f)), false));
                }
                if let Some((p, f)) = self.backward_rows(Some(pos), 0)? {
                    return Ok(done(Some(hit(p, f)), false));
                }
                if !self.req.wrap {
                    return Ok(done(None, false));
                }
                if let Some((p, f)) = self.backward_rows(None, pos + 1)? {
                    return Ok(done(Some(hit(p, f)), true));
                }
                let f = self.check_row(pos, Slots::AtOrAfter(slot), true)?;
                Ok(done(f.map(|f| hit(pos, f)), true))
            }
        }
    }
}

/// Runs a search on the calling (blocking) thread. See the module docs.
///
/// Cancellation is checked at least every 64 KiB scanned and returns
/// [`JobError::Cancelled`].
pub fn search_blocking(
    req: &SearchRequest,
    view: &View,
    src: &Source,
    index: &RowIndex,
    cancel: &CancellationToken,
    status: &SearchStatus,
) -> Result<SearchOutcome, JobError> {
    let mut engine = Engine {
        src,
        index,
        view,
        req,
        cancel,
        status,
        matcher: Matcher::new(&req.query, src.dialect().encoding),
        seeker: RowSeeker::new(src),
        parser: RecordParser::new(src.dialect()),
        rec: RecordRanges::default(),
        scratch: Vec::new(),
        edited: Vec::new(),
        text: String::new(),
        since: 0,
    };
    let r = engine.run();
    status.waiting.store(false, Ordering::Relaxed);
    r
}

/// Runs [`search_blocking`] as one blocking task on the executor (§8.3).
pub async fn run_search(
    req: SearchRequest,
    view: View,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    exec: Executor,
    cancel: CancellationToken,
    status: Arc<SearchStatus>,
) -> Result<SearchOutcome, JobError> {
    exec.run(move || search_blocking(&req, &view, &src, &index, &cancel, &status))
        .await
}
