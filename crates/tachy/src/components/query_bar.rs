//! Query bar: the filter and search bars (spec §12.4, M4-04, M4-05) and the
//! `Ctrl-o` prompt (D8). Two lines.
//!
//! - `open ›` (M1-08): line 1 is the prompt and the input with a block
//!   cursor, line 2 lists completion candidates (dim) or an error. Its state
//!   lives in `AppState::prompt`.
//! - `filter ›` / `search ›`: line 1 is the prompt and the input, the filter
//!   with live syntax highlighting (§9.4), scrolled horizontally so the
//!   cursor stays visible. Line 2 is the hint, or the validation error with
//!   a `^~~~` marker under its span (§9.2). The state is
//!   `AppState::find.bar`; editing is done by `App` (`app/find.rs`).
//!
//! - While a filter job fills the active view (after `Enter`, §12.4): line 1
//!   is `filter ›` and the highlighted expression, line 2 the amber gauge
//!   `██████░░░░ 42%`, then
//!   `3.1 GB / 7.4 GB scanned · 8 threads · 12,345 matches so far`.

use std::ops::Range;

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};
use tachy_core::{
    jobs::Progress,
    query::{self, TokenClass},
    size::{format_count, format_size},
    text,
};
use unicode_segmentation::UnicodeSegmentation;

use super::{Component, draw_placeholder, put};
use crate::{
    jobs::JobHandle,
    path_complete::{SHOWN_CANDIDATES, separator_for},
    search_ui::{BarKind, QueryInput, SaveFlow, SaveStep, Validation, column_names},
    state::{AppState, OpenPrompt, PromptMessage},
    theme::Theme,
};

const PROMPT: &str = "open › ";
/// Line 2 of the filter and search bars before `Enter` (§12.4).
pub const HINT: &str = "history ↑↓ · Tab complete · Enter apply";
/// Below this many free cells after the `^~~~` marker, the error message is
/// drawn alone.
const MIN_MESSAGE_ROOM: usize = 12;

/// Draws the query bar region.
#[derive(Debug, Default)]
pub struct QueryBar;

impl Component for QueryBar {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        if let Some(prompt) = &state.prompt {
            draw_open_prompt(frame.buffer_mut(), area, state, prompt);
        } else if let Some(bar) = &state.find.bar {
            draw_bar(frame.buffer_mut(), area, state, bar);
        } else if let Some(job) = state.running_filter() {
            draw_running_filter(frame.buffer_mut(), area, state, job);
        } else {
            draw_placeholder(frame, area, state.theme.base(), "query bar", state);
        }
        Ok(())
    }
}

fn draw_open_prompt(buf: &mut Buffer, area: Rect, state: &AppState, prompt: &OpenPrompt) {
    let theme = &state.theme;
    let base = theme.surface();
    buf.set_style(area, base);
    if area.height == 0 {
        return;
    }
    let (y1, y2) = (area.y, area.y + 1);

    // Line 1: prompt, input, block cursor. Long input scrolls so the
    // cursor stays visible.
    let x0 = put(
        buf,
        area.x + 1,
        y1,
        PROMPT,
        base.patch(theme.query_prompt()),
    );
    let avail = usize::from(area.right().saturating_sub(x0 + 1));
    let input = prompt.input.text();
    let (before, after) = input.split_at(prompt.input.cursor());
    let mut shown_before = before.to_owned();
    while text::width(&shown_before) + 1 > avail && !shown_before.is_empty() {
        shown_before.remove(0);
    }
    let x = put(buf, x0, y1, &shown_before, base);
    let mut chars = after.chars();
    let under = chars.next().map_or(" ".to_owned(), |c| c.to_string());
    let x = put(buf, x, y1, &under, theme.cursor_cell());
    let rest = text::truncate_end(chars.as_str(), usize::from(area.right().saturating_sub(x)));
    put(buf, x, y1, &rest, base);

    // Line 2: candidates, an error or progress.
    if area.height < 2 {
        return;
    }
    let width = usize::from(area.width.saturating_sub(2));
    match &prompt.message {
        PromptMessage::None => {}
        PromptMessage::Opening => {
            put(buf, x0, y2, "opening…", base.patch(theme.dim()));
        }
        PromptMessage::Error(err) => {
            let err = text::truncate_end(err, width);
            put(buf, area.x + 1, y2, &err, base.patch(theme.inline_error()));
        }
        PromptMessage::Candidates(list) => {
            let sep = separator_for(prompt.input.text());
            let mut line = list
                .iter()
                .take(SHOWN_CANDIDATES)
                .map(|c| c.completed(sep))
                .collect::<Vec<_>>()
                .join("  ");
            if list.len() > SHOWN_CANDIDATES {
                line.push_str(&format!("  +{} more", list.len() - SHOWN_CANDIDATES));
            }
            if list.is_empty() {
                line = "no matches".to_owned();
            }
            let line = text::truncate_end(&line, width);
            put(buf, x0, y2, &line, base.patch(theme.dim()));
        }
    }
}

/// The style of a highlighted token (§9.4).
fn token_style(theme: &Theme, class: TokenClass) -> Style {
    match class {
        TokenClass::Operator => theme.query_operator(),
        TokenClass::Keyword => theme.query_keyword(),
        TokenClass::String => theme.query_string(),
        TokenClass::Number => theme.query_number(),
        TokenClass::Column => theme.query_column(),
        TokenClass::UnknownColumn | TokenClass::Error => theme.query_error(),
    }
}

/// Token classes of the filter text, by byte range. Search text is not
/// highlighted.
fn token_spans(state: &AppState, bar: &QueryInput) -> Vec<(Range<usize>, TokenClass)> {
    if bar.kind != BarKind::Filter {
        return Vec::new();
    }
    let names = state
        .tabs
        .iter()
        .find(|t| t.id == bar.tab)
        .and_then(|t| t.loaded.as_ref())
        .map(|l| column_names(&l.columns));
    query::highlight(bar.edit.text(), names.as_deref())
}

/// Line 2 while `Ctrl-S` saves the filter as a named view (M6-04):
/// `save as › name`, `glob › orders_*.csv`, or the overwrite question, then
/// an error (coral) or a note (dim).
fn draw_save_flow(buf: &mut Buffer, x0: u16, y: u16, right: u16, theme: &Theme, flow: &SaveFlow) {
    let base = theme.surface();
    let prompt = base.patch(theme.query_prompt());
    let mut x = match flow.step {
        SaveStep::Name | SaveStep::Glob => {
            let (label, edit) = if flow.step == SaveStep::Name {
                ("save as › ", &flow.name)
            } else {
                ("glob › ", &flow.glob)
            };
            let x = put(buf, x0, y, label, prompt);
            let input = edit.text();
            let (before, after) = input.split_at(edit.cursor());
            let x = put(buf, x, y, before, base);
            let mut chars = after.chars();
            let under = chars.next().map_or(" ".to_owned(), |c| c.to_string());
            let x = put(buf, x, y, &under, theme.cursor_cell());
            put(buf, x, y, chars.as_str(), base)
        }
        SaveStep::Overwrite => {
            let q = format!("overwrite view \"{}\"? (y/n)", flow.name.text().trim());
            put(buf, x0, y, &q, prompt)
        }
        SaveStep::Writing => put(buf, x0, y, "saving…", base.patch(theme.dim())),
    };
    let (extra, style) = match (&flow.error, &flow.note) {
        (Some(e), _) => (e.as_str(), base.patch(theme.inline_error())),
        (None, Some(n)) => (n.as_str(), base.patch(theme.dim())),
        _ => return,
    };
    x += 2;
    let extra = text::truncate_end(extra, usize::from(right.saturating_sub(x)));
    put(buf, x, y, &extra, style);
}

/// Width of the scan gauge (§12.4).
const GAUGE_CELLS: usize = 10;

/// The query bar while a filter job runs on the active view (§12.4).
fn draw_running_filter(buf: &mut Buffer, area: Rect, state: &AppState, job: &JobHandle) {
    let theme = &state.theme;
    let base = theme.surface();
    buf.set_style(area, base);
    if area.height == 0 {
        return;
    }
    let (y1, y2) = (area.y, area.y + 1);
    let x0 = put(
        buf,
        area.x + 1,
        y1,
        BarKind::Filter.prompt(),
        base.patch(theme.query_prompt()),
    );
    let right = area.right().saturating_sub(1).max(x0);
    let expr = state
        .active_tab()
        .and_then(|t| t.views.active_view().filter_expr())
        .unwrap_or_default();
    let spans = query::highlight(expr, None);
    let mut x = x0;
    for (i, g) in expr.grapheme_indices(true) {
        let w = text::width(g) as u16;
        if x + w > right {
            break;
        }
        let style = spans
            .iter()
            .find(|(r, _)| r.contains(&i))
            .map_or(base, |(_, c)| base.patch(token_style(theme, *c)));
        put(buf, x, y1, g, style);
        x += w;
    }
    if area.height < 2 {
        return;
    }
    let Progress::Filter(p) = &job.progress else {
        return;
    };
    let fraction = p.fraction().unwrap_or(0.0).clamp(0.0, 1.0);
    let filled = ((fraction * GAUGE_CELLS as f64).floor() as usize).min(GAUGE_CELLS);
    let mut x = put(
        buf,
        x0,
        y2,
        &"█".repeat(filled),
        base.patch(theme.gauge(theme.amber)),
    );
    x = put(
        buf,
        x,
        y2,
        &"░".repeat(GAUGE_CELLS - filled),
        base.patch(theme.gauge(theme.amber))
            .patch(theme.bar_track()),
    );
    let status = if job.state == tachy_core::jobs::JobState::Paused {
        " paused".to_owned()
    } else {
        String::new()
    };
    let detail = format!(
        " {:.0}%{status} · {} / {} scanned · {} threads · {} matches so far",
        fraction * 100.0,
        format_size(p.bytes_done()),
        format_size(p.bytes_total()),
        state.settings.threads,
        format_count(p.matches()),
    );
    let detail = text::truncate_end(&detail, usize::from(right.saturating_sub(x)));
    put(buf, x, y2, &detail, base.patch(theme.dim()));
}

fn draw_bar(buf: &mut Buffer, area: Rect, state: &AppState, bar: &QueryInput) {
    let theme = &state.theme;
    let base = theme.surface();
    buf.set_style(area, base);
    if area.height == 0 {
        return;
    }
    let (y1, y2) = (area.y, area.y + 1);

    // Line 1: prompt, highlighted input, block cursor.
    let x0 = put(
        buf,
        area.x + 1,
        y1,
        bar.kind.prompt(),
        base.patch(theme.query_prompt()),
    );
    let right = area.right().saturating_sub(1).max(x0);
    let avail = usize::from(right - x0);
    let input = bar.edit.text();
    let cursor = bar.edit.cursor();
    let scroll = bar.edit.window(avail);
    let spans = token_spans(state, bar);
    let class_at = |i: usize| spans.iter().find(|(r, _)| r.contains(&i)).map(|(_, c)| *c);
    let mut x = x0;
    for (off, g) in input[scroll..].grapheme_indices(true) {
        let i = scroll + off;
        let w = text::width(g) as u16;
        if x + w > right {
            break;
        }
        let style = if i == cursor {
            theme.cursor_cell()
        } else {
            match class_at(i) {
                Some(class) => base.patch(token_style(theme, class)),
                None => base,
            }
        };
        put(buf, x, y1, g, style);
        x += w;
    }
    if cursor == input.len() && x < right {
        put(buf, x, y1, " ", theme.cursor_cell());
    }

    // Line 2: the save-view steps (M6-04), the hint or the error.
    if area.height < 2 {
        return;
    }
    if let Some(flow) = &state.find.save {
        draw_save_flow(buf, x0, y2, right, theme, flow);
        return;
    }
    match &bar.validation {
        Validation::Hint => {
            let hint = text::truncate_end(HINT, avail);
            put(buf, x0, y2, &hint, base.patch(theme.dim()));
        }
        Validation::Invalid { message, span } => {
            let error = base.patch(theme.inline_error());
            let mut x = x0;
            if let Some(span) = span {
                // `^~~~` under the span (from the first shown grapheme when
                // it starts left of the window), then the message.
                let start = span.start.clamp(scroll, input.len());
                let end = span.end.clamp(start, input.len());
                let at = x0 + text::width(&input[scroll..start]) as u16;
                let len = text::width(&input[start..end]).max(1);
                let marker = format!("^{}", "~".repeat(len - 1));
                let after = at + text::width(&marker) as u16 + 1;
                if usize::from(right.saturating_sub(after)) >= MIN_MESSAGE_ROOM {
                    put(buf, at, y2, &marker, error);
                    x = after;
                }
            }
            let message = text::truncate_end(message, usize::from(right.saturating_sub(x)));
            put(buf, x, y2, &message, error);
        }
    }
}
