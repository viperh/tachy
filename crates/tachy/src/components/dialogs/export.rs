//! The Export dialog `e` (spec §8.5, §11.7, M6-01).
//!
//! An amber dialog (export jobs are amber, §10.1), `min(80, w − 4)` wide and
//! 13 high, with five fields. `Tab` / `↓` moves to the next field,
//! `shift-Tab` / `↑` to the previous one; the focused field's label is amber.
//!
//! | Field | Keys | Default |
//! |---|---|---|
//! | Path | text input; `Tab` completes the path when the cursor is at the end and completions exist, otherwise moves on | `<source dir>/<stem>.<view>.csv`, `$PWD/stdin.<view>.csv` for stdin |
//! | Delimiter | `←` `→` cycle `,` `\t` `\|` `;`; `c` then a key types a custom single byte | the source delimiter |
//! | Quoting | `←` `→`: `minimal` / `all` | `minimal` |
//! | Header | `←` `→` `Space`: on / off | on if the source has a header |
//! | Columns | `←` `→`: `all` / `visible` | `visible` |
//!
//! The footer shows `12,345 rows · est. 4.1 MB` (`≥ 12,345 rows (growing)`
//! while the view is still growing). `Enter` checks the target
//! ([`tachy_core::export::check_target`]: never the source file, the
//! directory must exist and be writable); an existing target asks
//! `overwrite <name>? (y/n)` on the message line. A valid `Enter` yields an
//! [`ExportRequest`], which `App::submit_export` turns into a job.
//!
//! The form is plain data in `AppState::export`; `App` routes the dialog's
//! actions (`Submit`, `Cancel`, `Complete`, `SelectNext`, `SelectPrev`) and
//! its unbound keys ([`ExportForm::handle_key`]) to it.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Frame, layout::Rect, style::Modifier};
use tachy_core::{
    export::{ExportColumn, Quoting, check_target, default_target},
    sample::SampleResult,
    size::{format_count, format_size},
    text,
    view::View,
};

use super::{dialog_frame, draw_input};
use crate::{
    components::{layout::centered_rect, put},
    input::LineInput,
    path_complete::{self, Candidate, Cycle},
    tab::{Tab, TabId},
    theme::Theme,
};

/// Height of the dialog; the width is `min(80, area.width − 4)`.
pub const HEIGHT: u16 = 13;
const MAX_WIDTH: u16 = 80;
/// Label column width (`Delimiter` plus padding).
const LABEL_WIDTH: u16 = 12;
/// The `←`/`→` delimiter cycle (§8.5).
const DELIMITERS: [u8; 4] = *b",\t|;";
const EMPTY_PATH: &str = "enter a file path";
const CUSTOM_PROMPT: &str = "type the delimiter character (Esc to keep the current one)";

/// What `Enter` hands to `App::submit_export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRequest {
    /// Where to write (resolved: `~` expanded, relative to `$PWD`).
    pub target: PathBuf,
    pub delimiter: u8,
    pub quoting: Quoting,
    /// Write a header row.
    pub header: bool,
    /// `visible`: the visible columns in display order; otherwise every
    /// column in source order (`ExportColumn::select`).
    pub columns_visible_only: bool,
}

/// The dialog's fields, in focus order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Path,
    Delimiter,
    Quoting,
    Header,
    Columns,
}

impl Field {
    const ALL: [Field; 5] = [
        Field::Path,
        Field::Delimiter,
        Field::Quoting,
        Field::Header,
        Field::Columns,
    ];

    fn label(self) -> &'static str {
        match self {
            Field::Path => "Path",
            Field::Delimiter => "Delimiter",
            Field::Quoting => "Quoting",
            Field::Header => "Header",
            Field::Columns => "Columns",
        }
    }

    fn index(self) -> usize {
        Field::ALL.iter().position(|&f| f == self).unwrap_or(0)
    }
}

/// What the dialog needs to know about the tab and its current view,
/// captured when it opens.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportInfo {
    pub tab: TabId,
    /// `all`, `filtered` or `sorted`: the default file name's middle part.
    pub view: &'static str,
    /// The tab reads stdin: the default goes to `$PWD/stdin.<view>.csv`.
    pub stdin: bool,
    /// The file the user opened (`Tab::path`; the spool file for stdin).
    pub source_path: PathBuf,
    /// The file actually read (`Source::path`): differs from `source_path`
    /// for a transcoded UTF-16 file. Both are refused as targets.
    pub read_path: PathBuf,
    /// The source delimiter (the Delimiter default).
    pub delimiter: u8,
    /// The source has a header (the Header default).
    pub header: bool,
    /// Rows in the view (a lower bound while `growing`).
    pub rows: u64,
    /// The view is still growing (indexing, a running filter).
    pub growing: bool,
    /// Estimated bytes of one exported record, header excluded, with
    /// `minimal` quoting: every column / the visible ones.
    pub record_bytes_all: f64,
    pub record_bytes_visible: f64,
    /// Fields per record: every column / the visible ones (for the extra
    /// quotes of `all` quoting).
    pub fields_all: usize,
    pub fields_visible: usize,
}

impl ExportInfo {
    /// The info of `tab`'s current view; `None` until it is loaded.
    pub fn from_tab(tab: &Tab) -> Option<ExportInfo> {
        let l = tab.loaded.as_ref()?;
        let display = tab.layout.display();
        let all = ExportColumn::select(&l.columns, &display, false);
        let visible = ExportColumn::select(&l.columns, &display, true);
        let all: Vec<usize> = all.iter().map(|c| c.field).collect();
        let visible: Vec<usize> = visible.iter().map(|c| c.field).collect();
        let indexed = l.index.indexed_rows();
        let data = l.source.len().saturating_sub(l.source.data_start());
        let fallback = if indexed > 0 {
            data as f64 / indexed as f64
        } else {
            0.0
        };
        let sample = l.sample.as_deref();
        let dialect = l.source.dialect();
        Some(ExportInfo {
            tab: tab.id,
            view: view_name(tab.views.active_view()),
            stdin: tab.temp.is_some(),
            source_path: tab.path.clone(),
            read_path: l.source.path().to_path_buf(),
            delimiter: dialect.delimiter,
            header: dialect.header,
            rows: tab.view_len(),
            growing: !tab.view_len_exact(),
            record_bytes_all: record_bytes(sample, &all, all.len(), fallback),
            record_bytes_visible: record_bytes(sample, &visible, all.len(), fallback),
            fields_all: all.len(),
            fields_visible: visible.len(),
        })
    }
}

/// The view's name in the default file name: `all`, `filtered` (also a
/// filter over a sorted view, D10) or `sorted`.
pub fn view_name(view: &View) -> &'static str {
    match view {
        View::All => "all",
        view => view.label(),
    }
}

/// Estimated bytes of one record made of `fields`: the sampled average
/// length of each field, plus one delimiter between fields and the `\n`.
/// Without a sample, `fallback` (the average source record length) scaled by
/// the share of the `total` columns exported.
pub fn record_bytes(
    sample: Option<&SampleResult>,
    fields: &[usize],
    total: usize,
    fallback: f64,
) -> f64 {
    match sample {
        Some(s) if s.rows_sampled > 0 => {
            let avg = |f: usize| {
                s.per_column.get(f).map_or(0.0, |c| {
                    let n = c.values.len();
                    if n == 0 {
                        return 0.0;
                    }
                    let sum: usize = (0..n).map(|i| c.values.get(i).map_or(0, <[u8]>::len)).sum();
                    sum as f64 / n as f64
                })
            };
            fields.iter().map(|&f| avg(f)).sum::<f64>() + fields.len() as f64
        }
        _ if total == 0 => 0.0,
        _ => fallback * fields.len() as f64 / total as f64,
    }
}

/// What an unbound key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    Ignored,
    Changed,
    /// `y` on the overwrite confirm line.
    Submit(ExportRequest),
}

/// What `Tab` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteStep {
    /// Handled (moved to the next field, or the next cycle match).
    Done,
    /// List the directory for this input off the UI task, then call
    /// [`ExportForm::completions`] with the result.
    List(String),
}

/// The dialog's state. `App` keeps it in `AppState::export`.
#[derive(Debug, Clone)]
pub struct ExportForm {
    pub info: ExportInfo,
    pub focus: Field,
    pub path: LineInput,
    pub delimiter: u8,
    /// A delimiter outside [`DELIMITERS`] (the source's or a typed one): it
    /// joins the `←`/`→` cycle, first.
    pub custom: Option<u8>,
    pub quoting: Quoting,
    pub header: bool,
    pub visible_only: bool,
    /// After `c` in the Delimiter field: the next key is the delimiter.
    pub typing_delimiter: bool,
    /// The `overwrite …? (y/n)` line, with the request `y` submits.
    pub confirm: Option<ExportRequest>,
    /// Shown in coral on the message line until the next edit.
    pub error: Option<String>,
    /// Shown dim on the message line (completion matches).
    pub note: Option<String>,
    /// Cycling through several path matches with repeated `Tab`s.
    pub cycle: Option<Cycle>,
    /// The input a directory listing was requested for.
    pub completing: Option<String>,
}

impl ExportForm {
    /// The form with its defaults; `cwd` is where stdin exports go.
    pub fn new(info: ExportInfo, cwd: &Path) -> ExportForm {
        let source = (!info.stdin).then_some(info.source_path.as_path());
        let target = default_target(source, info.view, cwd);
        let mut path = LineInput::default();
        path.set(target.to_string_lossy());
        let delimiter = info.delimiter;
        ExportForm {
            focus: Field::Path,
            path,
            delimiter,
            custom: (!DELIMITERS.contains(&delimiter)).then_some(delimiter),
            quoting: Quoting::Minimal,
            header: info.header,
            visible_only: true,
            typing_delimiter: false,
            confirm: None,
            error: None,
            note: None,
            cycle: None,
            completing: None,
            info,
        }
    }

    /// `Tab` (outside the path completion) / `↓`.
    pub fn next_field(&mut self) {
        self.move_focus(1);
    }

    /// `shift-Tab` / `↑`.
    pub fn prev_field(&mut self) {
        self.move_focus(Field::ALL.len() - 1);
    }

    fn move_focus(&mut self, by: usize) {
        if self.confirm.is_some() {
            return;
        }
        self.typing_delimiter = false;
        self.completing = None;
        self.note = None;
        let i = (self.focus.index() + by) % Field::ALL.len();
        self.focus = Field::ALL[i];
    }

    /// `←` (`forward = false`) / `→` on a choice field.
    pub fn change(&mut self, forward: bool) {
        match self.focus {
            Field::Path => {}
            Field::Delimiter => self.cycle_delimiter(forward),
            Field::Quoting => {
                self.quoting = match self.quoting {
                    Quoting::Minimal => Quoting::All,
                    Quoting::All => Quoting::Minimal,
                }
            }
            Field::Header => self.header = !self.header,
            Field::Columns => self.visible_only = !self.visible_only,
        }
        self.error = None;
    }

    /// The delimiter cycle: the custom one (if any), then `,` `\t` `|` `;`.
    fn delimiter_cycle(&self) -> Vec<u8> {
        self.custom.into_iter().chain(DELIMITERS).collect()
    }

    fn cycle_delimiter(&mut self, forward: bool) {
        let cycle = self.delimiter_cycle();
        let n = cycle.len();
        let i = cycle.iter().position(|&d| d == self.delimiter).unwrap_or(0);
        let next = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.delimiter = cycle[next];
    }

    /// The custom delimiter typed after `c`: one printable ASCII character
    /// (space included), not `"` (the output quote).
    pub fn set_custom_delimiter(&mut self, c: char) -> Result<(), String> {
        let b = u8::try_from(c)
            .ok()
            .filter(|b| *b == b' ' || b.is_ascii_graphic())
            .ok_or_else(|| format!("{c:?} is not a single-byte character"))?;
        if b == b'"' {
            return Err("\" is the quote character".to_owned());
        }
        if !DELIMITERS.contains(&b) {
            self.custom = Some(b);
        }
        self.delimiter = b;
        Ok(())
    }

    /// An unbound key (the keymap resolved `Enter`, `Esc`, `Tab`, `↑`, `↓`
    /// first).
    pub fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let ctrl_alt = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let Some(req) = &self.confirm {
            return match key.code {
                KeyCode::Char('y' | 'Y') if !ctrl_alt => KeyOutcome::Submit(req.clone()),
                KeyCode::Char('n' | 'N') if !ctrl_alt => {
                    self.confirm = None;
                    KeyOutcome::Changed
                }
                _ => KeyOutcome::Ignored,
            };
        }
        if self.typing_delimiter {
            return match key.code {
                KeyCode::Char(c) if !ctrl_alt => {
                    match self.set_custom_delimiter(c) {
                        Ok(()) => {
                            self.typing_delimiter = false;
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e),
                    }
                    KeyOutcome::Changed
                }
                _ => KeyOutcome::Ignored,
            };
        }
        let changed = match (self.focus, key.code) {
            (Field::Path, _) => {
                let changed = self.path.handle_key(key);
                if changed {
                    self.error = None;
                    self.note = None;
                    self.completing = None;
                }
                changed
            }
            (_, KeyCode::Left) => {
                self.change(false);
                true
            }
            (_, KeyCode::Right) => {
                self.change(true);
                true
            }
            (Field::Header, KeyCode::Char(' ')) => {
                self.change(true);
                true
            }
            (Field::Delimiter, KeyCode::Char('c')) if !ctrl_alt => {
                self.typing_delimiter = true;
                self.error = None;
                true
            }
            _ => false,
        };
        if changed {
            KeyOutcome::Changed
        } else {
            KeyOutcome::Ignored
        }
    }

    /// `Esc`: leaves the confirm line or the custom delimiter entry first.
    /// Returns whether the dialog should close.
    pub fn cancel(&mut self) -> bool {
        if self.confirm.take().is_some() {
            return false;
        }
        if self.typing_delimiter {
            self.typing_delimiter = false;
            self.error = None;
            return false;
        }
        true
    }

    /// `Tab`. In the Path field with the cursor at the end: the next cycle
    /// match, or a directory listing to complete from. Anywhere else, the
    /// next field.
    pub fn complete(&mut self) -> CompleteStep {
        if self.confirm.is_some() {
            return CompleteStep::Done;
        }
        if self.focus != Field::Path || self.path.cursor() != self.path.text().len() {
            self.next_field();
            return CompleteStep::Done;
        }
        if let Some(cycle) = self.cycle.as_mut()
            && cycle.continues(self.path.text())
        {
            let next = cycle.advance();
            self.path.set(next);
            return CompleteStep::Done;
        }
        self.cycle = None;
        let input = self.path.text().to_owned();
        self.completing = Some(input.clone());
        CompleteStep::List(input)
    }

    /// The directory listing for `input` arrived. Stale results (typed on,
    /// focus moved) are dropped. Without a completion that changes the
    /// input, `Tab` moves to the next field. Returns whether it was used.
    pub fn completions(&mut self, input: &str, result: Result<Vec<Candidate>, String>) -> bool {
        if self.completing.as_deref() != Some(input) || self.path.text() != input {
            return false;
        }
        self.completing = None;
        let candidates = result.unwrap_or_default();
        let count = candidates.len();
        match path_complete::complete(input, candidates) {
            Some(c) if c.input != input || c.cycle.is_some() => {
                if let Some(cycle) = &c.cycle {
                    let sep = path_complete::separator_for(input);
                    let names: Vec<String> =
                        cycle.matches.iter().map(|m| m.completed(sep)).collect();
                    self.note = Some(format!("{count} matches: {}", names.join("  ")));
                }
                self.path.set(c.input);
                self.cycle = c.cycle;
            }
            _ => self.next_field(),
        }
        true
    }

    /// The target path the input names.
    pub fn target(&self, cwd: &Path, home: Option<&Path>) -> PathBuf {
        path_complete::resolve(self.path.text().trim(), cwd, home)
    }

    fn request(&self, target: PathBuf) -> ExportRequest {
        ExportRequest {
            target,
            delimiter: self.delimiter,
            quoting: self.quoting,
            header: self.header,
            columns_visible_only: self.visible_only,
        }
    }

    /// `Enter`. A confirm line or custom delimiter entry waits for its key.
    /// Otherwise checks the target: an error stays inline; an existing file
    /// asks `overwrite …? (y/n)`; else the request to submit.
    pub fn submit(&mut self, cwd: &Path, home: Option<&Path>) -> Option<ExportRequest> {
        if self.confirm.is_some() || self.typing_delimiter {
            return None;
        }
        self.note = None;
        if self.path.text().trim().is_empty() {
            self.error = Some(EMPTY_PATH.to_owned());
            self.focus = Field::Path;
            return None;
        }
        let target = self.target(cwd, home);
        let mut sources = vec![&self.info.source_path];
        if self.info.read_path != self.info.source_path {
            sources.push(&self.info.read_path);
        }
        for source in sources {
            if let Err(e) = check_target(&target, source) {
                self.error = Some(e.to_string());
                self.focus = Field::Path;
                return None;
            }
        }
        self.error = None;
        let req = self.request(target);
        if req.target.exists() {
            self.confirm = Some(req);
            return None;
        }
        Some(req)
    }

    /// `12,345 rows · est. 4.1 MB`, or `≥ 12,345 rows (growing) · est. ≥ …`.
    pub fn estimate(&self) -> String {
        let i = &self.info;
        let (record, fields) = if self.visible_only {
            (i.record_bytes_visible, i.fields_visible)
        } else {
            (i.record_bytes_all, i.fields_all)
        };
        let quotes = match self.quoting {
            Quoting::Minimal => 0.0,
            Quoting::All => 2.0 * fields as f64,
        };
        let bytes = ((record + quotes) * i.rows as f64).round() as u64;
        let rows = format_count(i.rows);
        let size = format_size(bytes);
        if i.growing {
            format!("≥ {rows} rows (growing) · est. ≥ {size}")
        } else {
            format!("{rows} rows · est. {size}")
        }
    }

    /// The key hints of the focused field.
    fn hints(&self) -> &'static str {
        if self.confirm.is_some() {
            return "y overwrite · n back";
        }
        if self.typing_delimiter {
            return "type a character · Esc back";
        }
        match self.focus {
            Field::Path => "Enter export · Esc cancel · Tab complete/next · ↑↓ field",
            Field::Delimiter => "Enter export · Esc cancel · ←→ change · c custom · ↑↓ field",
            Field::Header => "Enter export · Esc cancel · ←→/Space toggle · ↑↓ field",
            Field::Quoting | Field::Columns => "Enter export · Esc cancel · ←→ change · ↑↓ field",
        }
    }
}

/// `,` `\t` `|` `;` as shown in the Delimiter field; `space`, or `0x1f`.
pub fn delimiter_label(b: u8) -> String {
    match b {
        b'\t' => "\\t".to_owned(),
        b' ' => "space".to_owned(),
        0x21..=0x7e => (b as char).to_string(),
        _ => format!("0x{b:02x}"),
    }
}

pub fn draw(frame: &mut Frame, area: Rect, form: &ExportForm, theme: &Theme) {
    let width = MAX_WIDTH.min(area.width.saturating_sub(4));
    let rect = centered_rect(width, HEIGHT, area);
    let inner = dialog_frame(frame, rect, "Export", theme.amber, theme);
    if inner.height < HEIGHT - 2 || inner.width < LABEL_WIDTH + 8 {
        return;
    }
    let buf = frame.buffer_mut();
    let base = theme.dialog();
    let x = inner.x + 1;
    let w = inner.width - 2;
    let value_x = x + LABEL_WIDTH;
    let value_w = w - LABEL_WIDTH;
    let selected = base.add_modifier(Modifier::BOLD | Modifier::REVERSED);
    let other = base.patch(theme.dim());

    for (row, field) in Field::ALL.into_iter().enumerate() {
        let y = inner.y + 1 + row as u16;
        let focused = form.focus == field && form.confirm.is_none();
        let (marker, label_style) = if focused {
            ("› ", base.patch(theme.value_changed()))
        } else {
            ("  ", base.patch(theme.dim()))
        };
        let after = put(buf, x, y, marker, label_style);
        put(buf, after, y, field.label(), label_style);
        // Choice fields list every option; the chosen one is reversed.
        let options: Vec<(String, bool)> = match field {
            Field::Path => {
                // One cell in, aligned with the text of the padded options.
                let (px, pw) = (value_x + 1, value_w.saturating_sub(1));
                if focused {
                    draw_input(buf, px, y, pw, &form.path, base, theme);
                } else {
                    let p = text::truncate_end(form.path.text(), usize::from(pw));
                    put(buf, px, y, &p, base);
                }
                continue;
            }
            Field::Delimiter => form
                .delimiter_cycle()
                .into_iter()
                .map(|d| (delimiter_label(d), d == form.delimiter))
                .collect(),
            Field::Quoting => vec![
                ("minimal".to_owned(), form.quoting == Quoting::Minimal),
                ("all".to_owned(), form.quoting == Quoting::All),
            ],
            Field::Header => vec![
                ("on".to_owned(), form.header),
                ("off".to_owned(), !form.header),
            ],
            Field::Columns => vec![
                ("all".to_owned(), !form.visible_only),
                ("visible".to_owned(), form.visible_only),
            ],
        };
        let mut xx = value_x;
        for (label, chosen) in options {
            let style = if chosen { selected } else { other };
            xx = put(buf, xx, y, &format!(" {label} "), style);
            xx = put(buf, xx, y, " ", base);
        }
        if field == Field::Delimiter && form.typing_delimiter {
            put(buf, xx, y, "custom: _", base.patch(theme.value_changed()));
        }
    }

    // The message line: confirm, error, custom prompt or completion note.
    let y = inner.y + 7;
    let line = |s: &str| text::truncate_end(s, usize::from(w));
    if let Some(req) = &form.confirm {
        let name = req.target.file_name().map_or_else(
            || req.target.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let msg = line(&format!("overwrite {name}? (y/n)"));
        put(buf, x, y, &msg, base.patch(theme.value_changed()));
    } else if let Some(err) = &form.error {
        put(buf, x, y, &line(err), base.patch(theme.inline_error()));
    } else if form.typing_delimiter {
        put(buf, x, y, &line(CUSTOM_PROMPT), base.patch(theme.dim()));
    } else if let Some(note) = &form.note {
        put(buf, x, y, &line(note), base.patch(theme.dim()));
    }
    put(buf, x, inner.y + 8, &line(&form.estimate()), base);
    put(
        buf,
        x,
        inner.bottom() - 1,
        &line(form.hints()),
        base.patch(theme.hint()),
    );
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crossterm::event::KeyEventKind;
    use pretty_assertions::assert_eq;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    /// `TargetError::SourceFile`.
    const SOURCE_REFUSED: &str = "refusing to overwrite the source file";

    fn info(source: &Path) -> ExportInfo {
        ExportInfo {
            tab: TabId(1),
            view: "filtered",
            stdin: false,
            source_path: source.to_path_buf(),
            read_path: source.to_path_buf(),
            delimiter: b',',
            header: true,
            rows: 12_345,
            growing: false,
            record_bytes_all: 400.0,
            record_bytes_visible: 348.0,
            fields_all: 10,
            fields_visible: 4,
        }
    }

    fn form(source: &Path) -> ExportForm {
        ExportForm::new(info(source), Path::new("/work"))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    #[test]
    fn defaults_name_the_view() {
        let f = form(Path::new("/data/orders.csv"));
        assert_eq!(f.path.text(), "/data/orders.filtered.csv");
        assert_eq!(f.focus, Field::Path);
        assert_eq!(f.delimiter, b',');
        assert_eq!(f.custom, None);
        assert_eq!(f.quoting, Quoting::Minimal);
        assert!(f.header);
        assert!(f.visible_only);

        for (view, name) in [("sorted", "orders.sorted.csv"), ("all", "orders.all.csv")] {
            let mut i = info(Path::new("/data/orders.csv"));
            i.view = view;
            let f = ExportForm::new(i, Path::new("/work"));
            assert_eq!(f.path.text(), format!("/data/{name}"));
        }

        let mut i = info(Path::new("/tmp/tachy-stdin-abc"));
        i.stdin = true;
        i.view = "all";
        i.header = false;
        i.delimiter = b':';
        let f = ExportForm::new(i, Path::new("/work"));
        assert_eq!(f.path.text(), "/work/stdin.all.csv");
        assert!(!f.header);
        assert_eq!(f.delimiter, b':');
        assert_eq!(f.custom, Some(b':'));
    }

    #[test]
    fn view_names() {
        assert_eq!(view_name(&View::All), "all");
    }

    #[test]
    fn fields_cycle_both_ways() {
        let mut f = form(Path::new("/data/orders.csv"));
        let order: Vec<Field> = (0..5)
            .map(|_| {
                f.next_field();
                f.focus
            })
            .collect();
        assert_eq!(
            order,
            [
                Field::Delimiter,
                Field::Quoting,
                Field::Header,
                Field::Columns,
                Field::Path
            ]
        );
        f.prev_field();
        assert_eq!(f.focus, Field::Columns);
        f.prev_field();
        assert_eq!(f.focus, Field::Header);
    }

    #[test]
    fn choice_fields_change_with_arrows_and_space() {
        let mut f = form(Path::new("/data/orders.csv"));
        // In the Path field arrows move the cursor.
        assert_eq!(f.handle_key(key(KeyCode::Left)), KeyOutcome::Changed);
        assert_eq!(f.path.text(), "/data/orders.filtered.csv");
        assert_eq!(f.delimiter, b',');

        f.next_field();
        let mut seen = Vec::new();
        for _ in 0..4 {
            f.handle_key(key(KeyCode::Right));
            seen.push(f.delimiter);
        }
        assert_eq!(seen, b"\t|;,");
        f.handle_key(key(KeyCode::Left));
        assert_eq!(f.delimiter, b';');

        f.next_field();
        f.handle_key(key(KeyCode::Right));
        assert_eq!(f.quoting, Quoting::All);
        f.handle_key(key(KeyCode::Left));
        assert_eq!(f.quoting, Quoting::Minimal);

        f.next_field();
        f.handle_key(ch(' '));
        assert!(!f.header);
        f.handle_key(key(KeyCode::Right));
        assert!(f.header);

        f.next_field();
        f.handle_key(key(KeyCode::Left));
        assert!(!f.visible_only);
        // Space only toggles the header.
        assert_eq!(f.handle_key(ch(' ')), KeyOutcome::Ignored);
        assert!(!f.visible_only);
    }

    #[test]
    fn custom_delimiter() {
        let mut f = form(Path::new("/data/orders.csv"));
        f.next_field();
        f.handle_key(ch('c'));
        assert!(f.typing_delimiter);
        f.handle_key(ch('é'));
        assert!(f.typing_delimiter);
        assert!(f.error.is_some());
        f.handle_key(ch('"'));
        assert!(f.typing_delimiter);
        f.handle_key(ch(':'));
        assert!(!f.typing_delimiter);
        assert_eq!(f.error, None);
        assert_eq!(f.delimiter, b':');
        // It joins the cycle, first.
        f.handle_key(key(KeyCode::Right));
        assert_eq!(f.delimiter, b',');
        f.handle_key(key(KeyCode::Left));
        assert_eq!(f.delimiter, b':');
        // `Esc` leaves the entry without closing the dialog.
        f.handle_key(ch('c'));
        assert!(!f.cancel());
        assert!(!f.typing_delimiter);
        assert_eq!(f.delimiter, b':');
        assert!(f.cancel());
    }

    #[test]
    fn tab_completes_only_at_the_end_of_the_path() {
        let mut f = form(Path::new("/data/orders.csv"));
        assert_eq!(
            f.complete(),
            CompleteStep::List("/data/orders.filtered.csv".to_owned())
        );
        // The file exists: completing it changes nothing → next field.
        let same = vec![Candidate {
            name: "orders.filtered.csv".to_owned(),
            is_dir: false,
        }];
        assert!(f.completions("/data/orders.filtered.csv", Ok(same)));
        assert_eq!(f.focus, Field::Delimiter);

        let mut f = form(Path::new("/data/orders.csv"));
        f.path.set("/data/ord");
        let c = |n: &str| Candidate {
            name: n.to_owned(),
            is_dir: false,
        };
        assert_eq!(f.complete(), CompleteStep::List("/data/ord".to_owned()));
        assert!(f.completions("/data/ord", Ok(vec![c("orders.a.csv"), c("orders.b.csv")])));
        assert_eq!(f.path.text(), "/data/orders.");
        assert_eq!(f.focus, Field::Path);
        assert!(f.note.as_deref().unwrap().starts_with("2 matches"));
        assert_eq!(f.complete(), CompleteStep::Done);
        assert_eq!(f.path.text(), "/data/orders.a.csv");
        // Stale listing: dropped.
        f.path.set("/x");
        assert!(!f.completions("/data/ord", Ok(Vec::new())));
        // Cursor not at the end: next field.
        f.path.handle_key(key(KeyCode::Left));
        assert_eq!(f.complete(), CompleteStep::Done);
        assert_eq!(f.focus, Field::Delimiter);
        // Other fields: next field.
        assert_eq!(f.complete(), CompleteStep::Done);
        assert_eq!(f.focus, Field::Quoting);
    }

    #[test]
    fn submit_validates_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("orders.csv");
        fs::write(&source, "a\n1\n").unwrap();
        let mut f = ExportForm::new(info(&source), dir.path());

        // Empty path.
        f.path.set("  ");
        f.focus = Field::Header;
        assert_eq!(f.submit(dir.path(), None), None);
        assert_eq!(f.error.as_deref(), Some(EMPTY_PATH));
        assert_eq!(f.focus, Field::Path);

        // The source, also through `./` and a symlink.
        for p in ["orders.csv", "./orders.csv"] {
            f.path.set(p);
            assert_eq!(f.submit(dir.path(), None), None);
            assert_eq!(f.error.as_deref(), Some(SOURCE_REFUSED));
        }
        #[cfg(unix)]
        {
            let link = dir.path().join("link.csv");
            std::os::unix::fs::symlink(&source, &link).unwrap();
            f.path.set(link.to_string_lossy());
            assert_eq!(f.submit(dir.path(), None), None);
            assert_eq!(f.error.as_deref(), Some(SOURCE_REFUSED));
        }

        // A missing directory.
        f.path.set("nope/out.csv");
        assert_eq!(f.submit(dir.path(), None), None);
        assert!(f.error.as_deref().unwrap().contains("does not exist"));

        // A directory.
        fs::create_dir(dir.path().join("sub")).unwrap();
        f.path.set("sub");
        assert_eq!(f.submit(dir.path(), None), None);
        assert!(f.error.as_deref().unwrap().contains("is a directory"));

        // A new file: the request.
        f.path.set("out.csv");
        f.delimiter = b'|';
        f.quoting = Quoting::All;
        f.header = false;
        f.visible_only = false;
        let req = f.submit(dir.path(), None).unwrap();
        assert_eq!(
            req,
            ExportRequest {
                target: dir.path().join("out.csv"),
                delimiter: b'|',
                quoting: Quoting::All,
                header: false,
                columns_visible_only: false,
            }
        );
        assert_eq!(f.error, None);

        // An existing file: confirm first; `n` goes back, `y` submits.
        fs::write(dir.path().join("out.csv"), "old").unwrap();
        assert_eq!(f.submit(dir.path(), None), None);
        assert_eq!(f.confirm.as_ref(), Some(&req));
        assert_eq!(f.handle_key(ch('x')), KeyOutcome::Ignored);
        assert_eq!(f.handle_key(ch('n')), KeyOutcome::Changed);
        assert_eq!(f.confirm, None);
        assert_eq!(f.submit(dir.path(), None), None);
        // `Esc` leaves the confirm line, not the dialog.
        assert!(!f.cancel());
        assert_eq!(f.submit(dir.path(), None), None);
        assert_eq!(f.handle_key(ch('y')), KeyOutcome::Submit(req));
    }

    #[test]
    fn submit_refuses_the_transcoded_original_too() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("utf16.csv");
        let copy = dir.path().join("copy.csv");
        fs::write(&original, "x").unwrap();
        fs::write(&copy, "x").unwrap();
        let mut i = info(&original);
        i.read_path = copy;
        let mut f = ExportForm::new(i, dir.path());
        for p in ["utf16.csv", "copy.csv"] {
            f.path.set(p);
            assert_eq!(f.submit(dir.path(), None), None, "{p}");
            assert_eq!(f.error.as_deref(), Some(SOURCE_REFUSED));
        }
    }

    #[test]
    fn estimate_follows_the_columns_and_quoting() {
        let mut f = form(Path::new("/data/orders.csv"));
        // 348 × 12,345 = 4,296,060 B.
        assert_eq!(f.estimate(), "12,345 rows · est. 4.1 MB");
        f.visible_only = false;
        assert_eq!(f.estimate(), "12,345 rows · est. 4.7 MB");
        f.quoting = Quoting::All;
        // (400 + 20) × 12,345.
        assert_eq!(f.estimate(), "12,345 rows · est. 4.9 MB");
        f.info.growing = true;
        assert!(
            f.estimate()
                .starts_with("≥ 12,345 rows (growing) · est. ≥ ")
        );
    }

    #[test]
    fn record_bytes_without_a_sample_scales_the_average() {
        assert_eq!(record_bytes(None, &[0, 1], 4, 100.0), 50.0);
        assert_eq!(record_bytes(None, &[], 0, 100.0), 0.0);
    }

    fn render(f: &ExportForm) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(84, 15)).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| draw(frame, frame.area(), f, &theme))
            .unwrap();
        terminal
    }

    #[test]
    fn snapshot_normal() {
        let mut f = form(Path::new("/data/orders.csv"));
        f.next_field();
        insta::assert_snapshot!("export_normal", render(&f).backend());
    }

    #[test]
    fn snapshot_overwrite_confirm() {
        let mut f = form(Path::new("/data/orders.csv"));
        let req = f.request(PathBuf::from("/data/orders.filtered.csv"));
        f.confirm = Some(req);
        insta::assert_snapshot!("export_overwrite_confirm", render(&f).backend());
    }

    #[test]
    fn snapshot_source_refused() {
        let mut f = form(Path::new("/data/orders.csv"));
        f.path.set("/data/orders.csv");
        f.error = Some(SOURCE_REFUSED.to_owned());
        insta::assert_snapshot!("export_source_refused", render(&f).backend());
    }
}
