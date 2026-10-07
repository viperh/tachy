//! Status line (spec §11.5, M1-07).
//!
//! Left: file · size · rows · dialect · index state · ragged · warnings ·
//! file changed on disk (M7-03, sticky until `R`).
//! Right: running jobs · position · percentage.
//!
//! While a jump waits for the index (M2-03, D9), the right side starts with
//! a braille spinner: `⠋ waiting for index… Esc to cancel`.
//!
//! On narrow terminals items are dropped in this order until the line fits:
//! dialect summary, size, index gauge (keeping only `31%`), ragged count
//! (and warnings), percentage, the spinner's text (keeping the glyph), the
//! jobs count, the file-changed warning. The file name, row count and
//! position are never dropped; the file name is cut in the middle as a last
//! resort.

use std::{sync::atomic::Ordering, time::Instant};

use ratatui::{Frame, layout::Rect, style::Style};
use tachy_core::{
    size::{format_count, format_eta, format_rate, format_size},
    text,
};

use super::{Component, put};
use crate::{
    rate::RateWindow,
    state::AppState,
    tab::{Phase, Tab},
    theme::Theme,
};

/// Cells of the progress gauge (§12.3).
pub const GAUGE_CELLS: usize = 8;
const SEPARATOR: &str = " · ";
/// Braille spinner frames, one per 100 ms tick (M2-03).
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const RIGHT_SEPARATOR: &str = "  ";

/// Draws the status line.
#[derive(Debug, Default)]
pub struct Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Size,
    Rows,
    Dialect,
    IndexState,
    Ragged,
    Warning,
    FileChanged,
    Jobs,
    Spinner,
    Position,
    Percent,
}

/// One status item: styled pieces, plus a compact form for the index state.
#[derive(Debug, Clone)]
struct Item {
    kind: Kind,
    spans: Vec<(String, Style)>,
    compact: Option<Vec<(String, Style)>>,
}

impl Item {
    fn new(kind: Kind, text: impl Into<String>, style: Style) -> Self {
        Item {
            kind,
            spans: vec![(text.into(), style)],
            compact: None,
        }
    }

    fn width(&self) -> usize {
        self.spans.iter().map(|(s, _)| text::width(s)).sum()
    }
}

/// `██░░░░░░` for `fraction` in `[0, 1]`, filled cells in `fill`, the track
/// in `track`. Glyphs, so it stays visible under `NO_COLOR` (M7-01).
fn gauge(fraction: f64, fill: Style, track: Style) -> Vec<(String, Style)> {
    let filled =
        ((fraction.clamp(0.0, 1.0) * GAUGE_CELLS as f64).floor() as usize).min(GAUGE_CELLS);
    let mut out = Vec::new();
    if filled > 0 {
        out.push(("█".repeat(filled), fill));
    }
    if filled < GAUGE_CELLS {
        out.push(("░".repeat(GAUGE_CELLS - filled), track));
    }
    out
}

/// A labelled progress item: `label ██░░ 31% · 3.4 GB/s · ~23 s left`,
/// compact form `31%`.
fn progress_item(label: &str, done: u64, total: u64, rate: &RateWindow, theme: &Theme) -> Item {
    let base = theme.status_line();
    let fraction = if total == 0 {
        1.0
    } else {
        done as f64 / total as f64
    };
    let percent = format!("{}%", (fraction.clamp(0.0, 1.0) * 100.0).floor() as u64);
    let mut spans = vec![(format!("{label} "), base)];
    spans.extend(gauge(
        fraction,
        base.patch(theme.gauge(theme.teal)),
        base.patch(theme.bar_track()),
    ));
    spans.push((format!(" {percent}"), base));
    if let Some(r) = rate.rate() {
        spans.push((format!("{SEPARATOR}{}", format_rate(r)), base));
    }
    spans.push((
        format!(
            "{SEPARATOR}{}",
            format_eta(rate.eta(total.saturating_sub(done)))
        ),
        base,
    ));
    Item {
        kind: Kind::IndexState,
        spans,
        compact: Some(vec![(percent, base)]),
    }
}

/// The left and right items for `tab`; `jobs` running jobs (all tabs).
fn items(tab: &Tab, jobs: usize, theme: &Theme) -> (Vec<Item>, Vec<Item>) {
    let base = theme.status_line();
    let dim = theme.status_dim();
    let warn = theme.status_warning();
    let mut left = vec![Item::new(Kind::File, tab.name.clone(), theme.status_file())];
    let mut right = Vec::new();

    let Some(l) = &tab.loaded else {
        match &tab.phase {
            Phase::Spooling { bytes } => {
                let done = bytes.load(Ordering::Relaxed);
                let mut item = Item::new(
                    Kind::IndexState,
                    format!("reading stdin{SEPARATOR}{}", format_size(done)),
                    base,
                );
                if let Some(r) = tab.spool_rate.rate() {
                    item.spans
                        .push((format!("{SEPARATOR}{}", format_rate(r)), base));
                }
                item.compact = Some(vec![(format_size(done), base)]);
                left.push(item);
            }
            Phase::Opening { progress } if progress.transcoding.load(Ordering::Relaxed) => {
                left.push(progress_item(
                    "transcode",
                    progress.bytes.load(Ordering::Relaxed),
                    progress.total.load(Ordering::Relaxed),
                    &tab.spool_rate,
                    theme,
                ));
            }
            _ => left.push(Item::new(Kind::IndexState, "opening…", dim)),
        }
        return (left, right);
    };

    let src = &l.source;
    left.push(Item::new(Kind::Size, format_size(src.len()), base));
    let len = tab.view_len();
    let exact = tab.view_len_exact();
    left.push(Item::new(
        Kind::Rows,
        if exact {
            format!("{} rows", format_count(len))
        } else {
            format!("≥ {} rows (counting…)", format_count(len))
        },
        base,
    ));
    let mut dialect = src.dialect().summary();
    if let Some(enc) = l.original_encoding {
        dialect = dialect.replacen("  utf-8", &format!("  {}→utf-8", enc.name()), 1);
    }
    left.push(Item::new(Kind::Dialect, dialect, dim));

    if let Some(err) = &l.index_error {
        left.push(Item::new(
            Kind::IndexState,
            format!("index failed: {err}"),
            warn,
        ));
    } else if l.index.is_complete() {
        left.push(Item::new(Kind::IndexState, "mmap · index ready", dim));
    } else {
        let total = src.len().saturating_sub(src.data_start());
        left.push(progress_item(
            "indexing",
            l.index.bytes_scanned().min(total),
            total,
            &l.rate,
            theme,
        ));
    }
    let ragged = l
        .index
        .ragged_rows()
        .max(if exact { 0 } else { l.ragged_seen });
    if ragged > 0 {
        let text = if exact {
            format!("{} ragged", format_count(ragged))
        } else {
            format!("≥ {} ragged", format_count(ragged))
        };
        left.push(Item::new(Kind::Ragged, text, warn));
    }
    if l.index.unterminated_quote() {
        left.push(Item::new(Kind::Warning, "unterminated quote at EOF", warn));
    }

    if let Some(change) = tab.file_changed {
        left.push(Item::new(Kind::FileChanged, change.warning_text(), warn));
    }
    if jobs > 0 {
        let noun = if jobs == 1 { "job" } else { "jobs" };
        right.push(Item::new(
            Kind::Jobs,
            format!("{jobs} {noun}"),
            theme.status_jobs(),
        ));
    }
    if let Some(jump) = &tab.pending_jump {
        let glyph = SPINNER[jump.frame % SPINNER.len()];
        let spin = base.patch(theme.spinner());
        right.push(Item {
            kind: Kind::Spinner,
            spans: vec![
                (glyph.to_owned(), spin),
                (" waiting for index… Esc to cancel".to_owned(), base),
            ],
            compact: Some(vec![(glyph.to_owned(), spin)]),
        });
    }
    let cols = tab.visible_cols();
    let total = if exact {
        format_count(len)
    } else {
        format!("≥ {}", format_count(len))
    };
    let (row, col) = if len == 0 {
        (0, 0)
    } else {
        (tab.cursor_row + 1, (tab.cursor_col + 1).min(cols))
    };
    right.push(Item::new(
        Kind::Position,
        format!("R {} / {total}  C {col}/{cols}", format_count(row)),
        base,
    ));
    let percent = if len == 0 {
        0
    } else {
        tab.cursor_row * 100 / (len - 1).max(1)
    };
    right.push(Item::new(Kind::Percent, format!("{percent}%"), dim));
    (left, right)
}

fn line_width(left: &[Item], right: &[Item]) -> usize {
    let l: usize = left.iter().map(Item::width).sum::<usize>()
        + text::width(SEPARATOR) * left.len().saturating_sub(1);
    let r: usize = right.iter().map(Item::width).sum::<usize>()
        + text::width(RIGHT_SEPARATOR) * right.len().saturating_sub(1);
    // One cell of padding at each end, two between the sides.
    l + r + 2 + if right.is_empty() { 0 } else { 2 }
}

/// Drops items (see the module docs) until the line fits in `width`.
fn fit_items(left: &mut Vec<Item>, right: &mut Vec<Item>, width: usize) {
    enum Step {
        Drop(Kind),
        Compact,
        CompactSpinner,
    }
    let steps = [
        Step::Drop(Kind::Dialect),
        Step::Drop(Kind::Size),
        Step::Compact,
        Step::Drop(Kind::Ragged),
        Step::Drop(Kind::Warning),
        Step::Drop(Kind::Percent),
        Step::CompactSpinner,
        Step::Drop(Kind::Jobs),
        Step::Drop(Kind::FileChanged),
    ];
    for step in steps {
        if line_width(left, right) <= width {
            return;
        }
        match step {
            Step::Drop(kind) => {
                left.retain(|i| i.kind != kind);
                right.retain(|i| i.kind != kind);
            }
            Step::CompactSpinner => {
                for item in right.iter_mut().filter(|i| i.kind == Kind::Spinner) {
                    if let Some(compact) = item.compact.take() {
                        item.spans = compact;
                    }
                }
            }
            Step::Compact => {
                left.retain(|i| i.kind != Kind::IndexState || i.compact.is_some());
                for item in left.iter_mut() {
                    if let Some(compact) = item.compact.take() {
                        item.spans = compact;
                    }
                }
            }
        }
    }
    let over = line_width(left, right).saturating_sub(width);
    if over > 0
        && let Some(file) = left.iter_mut().find(|i| i.kind == Kind::File)
    {
        let (name, style) = file.spans[0].clone();
        let keep = text::width(&name).saturating_sub(over).max(1);
        file.spans = vec![(text::truncate_middle(&name, keep), style)];
    }
}

impl Component for Status {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let buf = frame.buffer_mut();
        let base = theme.status_line();
        buf.set_style(area, base);
        let y = area.y;
        let Some(tab) = state.active_tab() else {
            put(buf, area.x + 1, y, "no file open", theme.status_dim());
            return Ok(());
        };
        let (mut left, mut right) = items(tab, state.jobs.running_count(), theme);
        if let Some(item) = search_spinner(state, tab, theme) {
            let at = right.iter().position(|i| i.kind == Kind::Position);
            right.insert(at.unwrap_or(right.len()), item);
        }
        fit_items(&mut left, &mut right, usize::from(area.width));

        let mut x = area.x + 1;
        for (i, item) in left.iter().enumerate() {
            if i > 0 {
                x = put(buf, x, y, SEPARATOR, theme.status_dim());
            }
            for (s, style) in &item.spans {
                x = put(buf, x, y, s, *style);
            }
        }
        let right_w: usize = right.iter().map(Item::width).sum::<usize>()
            + text::width(RIGHT_SEPARATOR) * right.len().saturating_sub(1);
        let mut x = area.right().saturating_sub(right_w as u16 + 1).max(area.x);
        for (i, item) in right.iter().enumerate() {
            if i > 0 {
                x = put(buf, x, y, RIGHT_SEPARATOR, base);
            }
            for (s, style) in &item.spans {
                x = put(buf, x, y, s, *style);
            }
        }
        Ok(())
    }
}

/// `⠋ searching…` (or `⠋ waiting for index…`) while a search runs on `tab`
/// (M4-05). Its compact form is the glyph.
fn search_spinner(state: &AppState, tab: &Tab, theme: &Theme) -> Option<Item> {
    let inflight = state
        .find
        .search
        .running_request()
        .filter(|i| i.tab == tab.id)?;
    let glyph = SPINNER[inflight.frame % SPINNER.len()];
    let spin = theme.status_line().patch(theme.spinner());
    let text = if inflight.status.waiting_for_index() {
        " waiting for index…"
    } else {
        " searching…"
    };
    Some(Item {
        kind: Kind::Spinner,
        spans: vec![
            (glyph.to_owned(), spin),
            (text.to_owned(), theme.status_line()),
        ],
        compact: Some(vec![(glyph.to_owned(), spin)]),
    })
}

/// Records a throughput sample for every running operation of `tab` (one per
/// tick, M1-07).
pub fn sample_progress(tab: &mut Tab, now: Instant) {
    match &tab.phase {
        Phase::Spooling { bytes } => {
            let done = bytes.load(Ordering::Relaxed);
            tab.spool_rate.push(now, done);
        }
        Phase::Opening { progress } if progress.transcoding.load(Ordering::Relaxed) => {
            let done = progress.bytes.load(Ordering::Relaxed);
            tab.spool_rate.push(now, done);
        }
        _ => {}
    }
    if let Some(l) = tab.loaded.as_mut()
        && !l.index.is_complete()
    {
        let done = l.index.bytes_scanned();
        l.rate.push(now, done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_cells() {
        let s = Style::new();
        let text = |f: f64| -> String { gauge(f, s, s).into_iter().map(|(t, _)| t).collect() };
        assert_eq!(text(0.0), "░░░░░░░░");
        assert_eq!(text(0.31), "██░░░░░░");
        assert_eq!(text(0.999), "███████░");
        assert_eq!(text(1.0), "████████");
        assert_eq!(text(7.0), "████████");
    }
}
