//! Inspector panel: column, top values, record (spec §11.4, §7.2, M3-03).
//!
//! Three sections, each under a dim uppercase label (amber while the panel
//! is focused), separated by a blank line:
//!
//! 1. **COLUMN**: `name · type · col 5/13`, then nulls, distinct, max width,
//!    and min / max / mean / p50 / p95 for numeric columns (min / max for
//!    dates). `sampling…` until the sample arrives; `(stats for i64)` when
//!    the type changed since.
//! 2. **TOP VALUES · sample 20k rows**: 5 bars plus `other`, scaled to the
//!    largest bar, teal over the track colour, with eighth-block precision.
//! 3. **RECORD**: every field of the cursor row in source order (hidden
//!    columns and `_extraN` included). Takes the remaining height; while the
//!    panel is focused `j`/`k` select a field and the list scrolls.

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};
use tachy_core::{
    column::ColumnMeta,
    parse::{decode_field, display_segments},
    size::{format_count, format_count_compact},
    source::Source,
    stats::{ColumnStats, Scalar},
    text::{self, Align},
    types::ColType,
};

use super::{Component, put};
use crate::{
    state::{AppState, Focus},
    tab::Tab,
    theme::Theme,
};

/// Cells of a top-values label.
const VALUE_CELLS: u16 = 14;
/// Cells of a top-values bar.
pub const BAR_CELLS: u16 = 16;
/// Cells of a RECORD field name.
const FIELD_CELLS: u16 = 12;
/// Label column of the COLUMN section.
const LABEL_CELLS: usize = 10;
/// Eighth blocks, 1/8 to 7/8 of a cell.
const EIGHTHS: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// Draws the inspector. Keeps the RECORD scroll offset (local UI state).
#[derive(Debug, Default)]
pub struct Inspector {
    scroll: usize,
}

impl Component for Inspector {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let buf = frame.buffer_mut();
        let base = theme.surface();
        buf.set_style(area, base);
        if area.width < 4 || area.height == 0 {
            return Ok(());
        }
        let focused = state.focus == Focus::Inspector;
        let mut p = Painter {
            buf,
            x: area.x + 1,
            width: area.width - 2,
            y: area.y,
            bottom: area.bottom(),
            base,
            theme,
            label: if focused {
                base.patch(theme.inspector_label_focused())
            } else {
                base.patch(theme.inspector_label())
            },
        };
        let Some(tab) = state.active_tab() else {
            return Ok(());
        };
        let Some(l) = &tab.loaded else {
            return Ok(());
        };
        if let Some(col) = tab.cursor_source_col()
            && let Some(meta) = l.columns.get(col)
        {
            column_section(&mut p, meta, col, l.columns.len());
            p.y += 1;
            top_values_section(&mut p, meta, &l.source);
            p.y += 1;
        }
        self.record_section(&mut p, tab, state, focused);
        Ok(())
    }
}

/// A cursor over the panel's rows.
struct Painter<'a> {
    buf: &'a mut Buffer,
    x: u16,
    width: u16,
    y: u16,
    bottom: u16,
    base: Style,
    theme: &'a Theme,
    label: Style,
}

impl Painter<'_> {
    fn has_room(&self) -> bool {
        self.y < self.bottom
    }

    /// Writes `s` on the current row and moves down.
    fn line(&mut self, s: &str, style: Style) {
        if self.has_room() {
            let s = text::truncate_end(s, usize::from(self.width));
            put(self.buf, self.x, self.y, &s, style);
        }
        self.y += 1;
    }

    /// `label     value`.
    fn pair(&mut self, label: &str, value: &str) {
        if self.has_room() {
            let x = put(
                self.buf,
                self.x,
                self.y,
                &format!("{label:<LABEL_CELLS$}"),
                self.base.patch(self.theme.dim()),
            );
            let w = usize::from(self.width).saturating_sub(LABEL_CELLS);
            put(
                self.buf,
                x,
                self.y,
                &text::truncate_end(value, w),
                self.base,
            );
        }
        self.y += 1;
    }
}

fn column_section(p: &mut Painter, meta: &ColumnMeta, col: usize, n_cols: usize) {
    if p.has_room() {
        let x = put(p.buf, p.x, p.y, "COLUMN", p.label);
        if meta.stats_stale()
            && let Some(s) = &meta.stats
        {
            let note = format!(" (stats for {})", s.for_type.label());
            put(p.buf, x, p.y, &note, p.base.patch(p.theme.dim()));
        }
    }
    p.y += 1;

    // `name · type · col 5/13`: the name bold, the rest dim.
    if p.has_room() {
        let ty = if meta.type_override.is_some() {
            format!("{} (set)", meta.ty().label())
        } else {
            meta.ty().label().to_owned()
        };
        let rest = format!(" · {ty} · col {}/{n_cols}", col + 1);
        let rest_w = text::width(&rest);
        let name_w = usize::from(p.width).saturating_sub(rest_w).max(1);
        let name = text::truncate_end(&meta.name.display, name_w);
        let x = put(p.buf, p.x, p.y, &name, p.base.patch(p.theme.header()));
        put(p.buf, x, p.y, &rest, p.base.patch(p.theme.dim()));
    }
    p.y += 1;

    let Some(stats) = &meta.stats else {
        p.line("sampling…", p.base.patch(p.theme.dim()));
        return;
    };
    let nulls = stats
        .null_percent()
        .map_or_else(|| "–".to_owned(), |v| format!("{v:.1}%"));
    p.pair("nulls", &nulls);
    p.pair(
        "distinct",
        &format!("~{}", format_count(stats.distinct.estimate())),
    );
    p.pair("max width", &stats.max_width.to_string());
    if let Some(n) = &stats.numeric {
        let approx = if n.quantiles_approximate() { "~" } else { "" };
        let ty = stats.for_type;
        if let Some(v) = n.min() {
            p.pair("min", &format_scalar(v, ty));
        }
        if let Some(v) = n.max() {
            p.pair("max", &format_scalar(v, ty));
        }
        if let Some(v) = n.mean() {
            p.pair("mean", &format_f64(v));
        }
        if let Some(v) = n.p50() {
            p.pair("p50", &format!("{approx}{}", format_number(v, ty)));
        }
        if let Some(v) = n.p95() {
            p.pair("p95", &format!("{approx}{}", format_number(v, ty)));
        }
    }
    if let Some(t) = &stats.temporal {
        if let Some(v) = t.min {
            p.pair("min", &v.to_string());
        }
        if let Some(v) = t.max {
            p.pair("max", &v.to_string());
        }
    }
}

fn top_values_section(p: &mut Painter, meta: &ColumnMeta, src: &Source) {
    let Some(stats) = &meta.stats else {
        p.line("TOP VALUES", p.label);
        p.line("sampling…", p.base.patch(p.theme.dim()));
        return;
    };
    p.line(&format!("TOP VALUES · {}", stats.label()), p.label);
    let rows = top_rows(stats, src);
    if rows.is_empty() {
        p.line("no values", p.base.patch(p.theme.dim()));
        return;
    }
    let max = rows.iter().map(|r| r.count).max().unwrap_or(0);
    for row in &rows {
        if !p.has_room() {
            break;
        }
        let (x, y) = (p.x, p.y);
        let label_style = if row.other {
            p.base.patch(p.theme.dim())
        } else {
            p.base
        };
        let label = text::fit(&row.label, VALUE_CELLS, Align::Left);
        let after = put(p.buf, x, y, &label.render(), label_style);
        draw_bar(p.buf, after + 1, y, row.count, max, p.base, p.theme);
        let count_x = after + 1 + BAR_CELLS + 1;
        let count_w = (p.x + p.width).saturating_sub(count_x);
        let count = text::fit(&row.count_text, count_w, Align::Right).render();
        put(p.buf, count_x, y, &count, p.base);
        p.y += 1;
    }
}

/// One TOP VALUES row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopRow {
    pub label: String,
    pub count: u64,
    pub count_text: String,
    pub other: bool,
}

/// The top 5 values plus `other` (when non-zero), labels decoded and
/// escaped, counts compact (`~` when approximate).
pub fn top_rows(stats: &ColumnStats, src: &Source) -> Vec<TopRow> {
    let enc = src.dialect().encoding;
    let approximate = stats.top.is_approximate();
    let mut rows: Vec<TopRow> = stats
        .top5()
        .into_iter()
        .map(|e| {
            let decoded = decode_field(&e.value, enc);
            let label: String = display_segments(&decoded)
                .iter()
                .map(|s| s.text.as_ref())
                .collect();
            let approx = approximate && e.max_overcount > 0;
            TopRow {
                label,
                count: e.count,
                count_text: format!(
                    "{}{}",
                    if approx { "~" } else { "" },
                    format_count_compact(e.count)
                ),
                other: false,
            }
        })
        .collect();
    let other = stats.other();
    if other > 0 {
        rows.push(TopRow {
            label: "other".to_owned(),
            count: other,
            count_text: format!(
                "{}{}",
                if approximate { "~" } else { "" },
                format_count_compact(other)
            ),
            other: true,
        });
    }
    rows
}

/// Full cells and eighths (0–7) of a bar for `count` out of `max`, over
/// `cells` cells. The largest bar fills every cell; a non-zero count gets
/// at least one eighth.
pub fn bar_cells(count: u64, max: u64, cells: u16) -> (u16, u8) {
    if max == 0 || count == 0 {
        return (0, 0);
    }
    let total = u128::from(cells) * 8;
    let eighths = ((u128::from(count) * total + u128::from(max) / 2) / u128::from(max))
        .clamp(1, total) as u64;
    ((eighths / 8) as u16, (eighths % 8) as u8)
}

fn draw_bar(buf: &mut Buffer, x: u16, y: u16, count: u64, max: u64, base: Style, theme: &Theme) {
    let (full, part) = bar_cells(count, max, BAR_CELLS);
    let fill = base.patch(theme.bar_fill());
    let track = base.patch(theme.bar_track());
    // Under NO_COLOR the track must stay visible as a glyph (M7-01).
    let track_glyph = if theme.monochrome { "░" } else { "█" };
    let mut cx = x;
    for _ in 0..full {
        cx = put(buf, cx, y, "█", fill);
    }
    let mut used = full;
    if part > 0 {
        cx = put(
            buf,
            cx,
            y,
            EIGHTHS[usize::from(part) - 1],
            base.patch(theme.bar_partial()),
        );
        used += 1;
    }
    for _ in used..BAR_CELLS {
        cx = put(buf, cx, y, track_glyph, track);
    }
}

impl Inspector {
    fn record_section(&mut self, p: &mut Painter, tab: &Tab, state: &AppState, focused: bool) {
        p.line("RECORD", p.label);
        let Some(l) = &tab.loaded else {
            return;
        };
        let Some(row) = tab.cursor_row_data() else {
            p.line("no row", p.base.patch(p.theme.dim()));
            return;
        };
        let height = usize::from(p.bottom.saturating_sub(p.y));
        if height == 0 {
            return;
        }
        let n = l.columns.len();
        let selected = state.inspector.selected.min(n.saturating_sub(1));
        // Keep the selection visible, scrolling minimally.
        if focused {
            if selected < self.scroll {
                self.scroll = selected;
            } else if selected >= self.scroll + height {
                self.scroll = selected + 1 - height;
            }
        }
        self.scroll = self.scroll.min(n.saturating_sub(height));
        let value_x = p.x + FIELD_CELLS + 2;
        let value_w = (p.x + p.width).saturating_sub(value_x);
        for (i, meta) in l.columns.iter().enumerate().skip(self.scroll).take(height) {
            let y = p.y;
            let is_sel = focused && i == selected;
            let line_base = if is_sel {
                p.base.patch(p.theme.row_selected())
            } else {
                p.base
            };
            if is_sel {
                p.buf
                    .set_style(Rect::new(p.x - 1, y, p.width + 2, 1), line_base);
            }
            let name = text::fit(&meta.name.display, FIELD_CELLS, Align::Left).render();
            put(
                p.buf,
                p.x,
                y,
                &name,
                line_base.patch(p.theme.record_field()),
            );
            let value = row.display(&l.source, meta.source_index);
            draw_value(
                p.buf,
                value_x,
                y,
                value_w,
                value,
                line_base.patch(p.theme.record_value()),
                line_base.patch(p.theme.control_char()),
            );
            p.y += 1;
        }
    }
}

/// A value with control characters escaped (in `escaped`), cut with `…`.
fn draw_value(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    value: &str,
    plain: Style,
    escaped: Style,
) {
    let segments = display_segments(value);
    let joined: String = segments.iter().map(|s| &*s.text).collect();
    let fitted = text::fit(&joined, width, Align::Left);
    let mut cx = x;
    let mut remaining = fitted.text.len();
    for seg in &segments {
        if remaining == 0 {
            break;
        }
        let take = seg.text.len().min(remaining);
        cx = put(
            buf,
            cx,
            y,
            &seg.text[..take],
            if seg.escaped { escaped } else { plain },
        );
        remaining -= take;
    }
    if fitted.truncated {
        put(buf, x + width - 1, y, text::ELLIPSIS, plain);
    }
}

/// `i64` with thousands separators: `-1,234,567`.
pub fn format_i64(v: i64) -> String {
    let s = format_count(v.unsigned_abs());
    if v < 0 { format!("-{s}") } else { s }
}

/// `f64` with up to 6 significant digits, trailing zeros dropped, and no
/// exponent unless `|x| ≥ 1e15` or `0 < |x| < 1e-4`.
pub fn format_f64(x: f64) -> String {
    if !x.is_finite() {
        return x.to_string();
    }
    if x == 0.0 {
        return "0".to_owned();
    }
    let a = x.abs();
    if !(1e-4..1e15).contains(&a) {
        let s = format!("{x:.5e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = trim_zeros(mantissa);
        return format!("{mantissa}e{exp}");
    }
    let magnitude = a.log10().floor() as i32 + 1;
    let decimals = (6 - magnitude).max(0) as usize;
    let s = format!("{x:.decimals$}");
    trim_zeros(&s).to_owned()
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// A quantile of a column: integral values of an `i64` column as integers.
fn format_number(v: f64, ty: ColType) -> String {
    if ty == ColType::I64 && v.fract() == 0.0 && v.abs() < 9.0e15 {
        format_i64(v as i64)
    } else {
        format_f64(v)
    }
}

fn format_scalar(v: Scalar, ty: ColType) -> String {
    match v {
        Scalar::I64(i) => format_i64(i),
        Scalar::F64(f) => format_number(f, ty),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn integers_get_separators() {
        assert_eq!(format_i64(0), "0");
        assert_eq!(format_i64(1234), "1,234");
        assert_eq!(format_i64(-1_234_567), "-1,234,567");
        assert_eq!(format_i64(i64::MIN), "-9,223,372,036,854,775,808");
    }

    #[test]
    fn floats_have_six_significant_digits() {
        assert_eq!(format_f64(0.0), "0");
        assert_eq!(format_f64(1204.5), "1204.5");
        assert_eq!(format_f64(1.23456789), "1.23457");
        assert_eq!(format_f64(123456.789), "123457");
        assert_eq!(format_f64(-0.5), "-0.5");
        assert_eq!(format_f64(0.0001), "0.0001");
        assert_eq!(format_f64(0.00001234), "1.234e-5");
        assert_eq!(format_f64(1e15), "1e15");
        assert_eq!(format_f64(2.0), "2");
    }

    #[test]
    fn bars_scale_to_the_largest() {
        assert_eq!(bar_cells(100, 100, 16), (16, 0));
        assert_eq!(bar_cells(50, 100, 16), (8, 0));
        assert_eq!(bar_cells(1, 100, 16), (0, 1));
        assert_eq!(bar_cells(0, 100, 16), (0, 0));
        assert_eq!(bar_cells(33, 100, 16), (5, 2));
        assert_eq!(bar_cells(5, 0, 16), (0, 0));
    }
}
