//! Jobs drawer (spec §10, §11.1, §12.5, M5-02): a panel between the table
//! and the status line listing every job of every tab, with live progress.
//!
//! - Title line: `JOBS · N running` (purple when > 0); when focused, the
//!   drawer's keys on the right (from the live keymap).
//! - Up to 7 job lines; with more it scrolls, keeping the selection
//!   visible. Order: [`JobManager::drawer_order`](crate::jobs::JobManager::drawer_order).
//! - A line: `{icon} {kind} {[tab]} {title}  {gauge}  {progress}  {elapsed}`.
//!   Icon and colour from `theme.job_state`, the kind in `theme.job(kind)`.
//!   The title is cut in the middle; gauges, progress texts and elapsed
//!   times are aligned in columns on the right. A failed job shows its error
//!   (coral, cut with `…`) instead of the gauge and the text. Paused and
//!   queued jobs have dim text and a frozen or empty gauge. An unknown total
//!   (fraction `None`) is a bouncing 3-cell block, one cell per tick.
//!
//! Progress is read from the jobs' shared counters on every frame: there is
//! no message per update.

use std::time::Duration;

use ratatui::{Frame, buffer::Buffer, layout::Rect};
use tachy_core::{
    jobs::{JobKind, JobState, Progress, SortPhase},
    size::{format_count, format_count_compact, format_rate, format_size},
    text,
};

use super::{Component, put};
use crate::{
    action::Action,
    config::Config,
    jobs::JobHandle,
    keymap::Keymap,
    mode::KeyContext,
    state::{AppState, Focus},
    theme::Theme,
};

/// Cells of a job's gauge (M5-02).
pub const GAUGE_CELLS: usize = 12;
/// Width of the bouncing block of an indeterminate gauge.
const BOUNCE_CELLS: usize = 3;
/// Width of the kind label column (`profile` is the longest).
const KIND_WIDTH: usize = 7;
/// The title keeps at least this many cells before the progress text is
/// dropped.
const MIN_TITLE: usize = 12;

/// Draws the drawer. Keeps its scroll offset; the selection lives in
/// `AppState::jobs_selected`.
#[derive(Debug, Default)]
pub struct JobsDrawer {
    keymap: Keymap,
    offset: usize,
}

/// `kind` as shown in the drawer (§10.1).
pub fn kind_label(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Index => "index",
        JobKind::Filter => "filter",
        JobKind::Sort => "sort",
        JobKind::Profile => "profile",
        JobKind::Export => "export",
    }
}

/// `12 s`, `3 min`, `1 h 5 min`.
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}

/// The progress text of a job (§10.1), formatted as in the status line.
/// `rate` is the index throughput in bytes per second, if known.
pub fn progress_text(progress: &Progress, rate: Option<f64>) -> String {
    match progress {
        Progress::Bytes(p) => {
            let mut s = if p.total() > 0 {
                format!("{} / {}", format_size(p.done()), format_size(p.total()))
            } else {
                format_size(p.done())
            };
            if let Some(r) = rate {
                s.push_str(&format!(" · {}", format_rate(r)));
            }
            s
        }
        Progress::Filter(p) => {
            let n = p.matches();
            let noun = if n == 1 { "match" } else { "matches" };
            format!(
                "{} / {} · {} {noun}",
                format_size(p.bytes_done()),
                format_size(p.bytes_total()),
                format_count(n)
            )
        }
        Progress::Sort(p) => match p.phase() {
            SortPhase::Extract => {
                let pct = p.fraction().map_or(0, |f| (f * 100.0).floor() as u64);
                format!("extract {pct}%")
            }
            SortPhase::SortRuns => format!("sort runs {}/{}", p.done(), p.total()),
            SortPhase::Merge => {
                let (pass, passes) = p.pass();
                format!("merge pass {pass}/{passes}")
            }
        },
        Progress::Rows(p) => {
            if p.total() > 0 {
                format!(
                    "{} / {} rows",
                    format_count_compact(p.done()),
                    format_count_compact(p.total())
                )
            } else {
                format!("{} rows", format_count_compact(p.done()))
            }
        }
        Progress::Export(p) => format!(
            "{} rows · {} written",
            format_count_compact(p.rows_written()),
            format_size(p.bytes_written())
        ),
    }
}

/// The filled cells of a gauge: `fraction` filled from the left, or, when
/// it is `None` (unknown total), a 3-cell block bouncing with `tick`.
pub fn gauge_cells(fraction: Option<f64>, tick: u64) -> [bool; GAUGE_CELLS] {
    let mut cells = [false; GAUGE_CELLS];
    match fraction {
        Some(f) => {
            let n = ((f.clamp(0.0, 1.0) * GAUGE_CELLS as f64).floor() as usize).min(GAUGE_CELLS);
            cells[..n].fill(true);
        }
        None => {
            let span = (GAUGE_CELLS - BOUNCE_CELLS) as u64;
            let p = tick % (2 * span);
            let start = if p <= span { p } else { 2 * span - p } as usize;
            cells[start..start + BOUNCE_CELLS].fill(true);
        }
    }
    cells
}

/// One drawer line, before layout.
struct Line<'a> {
    job: &'a JobHandle,
    /// `[2] ` when several tabs are open.
    tab: String,
    text: String,
    elapsed: String,
}

/// Widths of the variable columns, shared by every visible line so the
/// gauges line up.
struct Columns {
    tab: usize,
    title: usize,
    /// 0 when the progress text is dropped for lack of room.
    text: usize,
    elapsed: usize,
}

fn columns(lines: &[Line], width: usize) -> Columns {
    let max = |f: &dyn Fn(&Line) -> usize| lines.iter().map(f).max().unwrap_or(0);
    let text_w = max(&|l| text::width(&l.text));
    let elapsed = max(&|l| text::width(&l.elapsed));
    let tab = max(&|l| text::width(&l.tab));
    // pad, icon, space, kind, space, tab, title, 2, gauge, 2, elapsed, pad
    let fixed = 1 + 1 + 1 + KIND_WIDTH + 1 + tab + 2 + GAUGE_CELLS + 2 + elapsed + 1;
    let with_text = fixed + text_w + 2;
    if width >= with_text + MIN_TITLE {
        Columns {
            tab,
            title: width - with_text,
            text: text_w,
            elapsed,
        }
    } else {
        Columns {
            tab,
            title: width.saturating_sub(fixed),
            text: 0,
            elapsed,
        }
    }
}

impl JobsDrawer {
    /// The focused title's hints: `j/k select  p pause  K kill  d dismiss
    /// Esc back`, keys from the keymap.
    fn hints(&self) -> Vec<(String, &'static str)> {
        let ctx = KeyContext::JobsDrawer;
        let key = |a: &Action| self.keymap.display_chord(ctx, a);
        let mut out = Vec::new();
        if let (Some(n), Some(p)) = (key(&Action::SelectNext), key(&Action::SelectPrev)) {
            out.push((format!("{n}/{p}"), "select"));
        }
        for (a, label) in [
            (Action::PauseJob, "pause"),
            (Action::KillJob, "kill"),
            (Action::DismissJob, "dismiss"),
            (Action::Cancel, "back"),
        ] {
            if let Some(k) = key(&a) {
                out.push((k, label));
            }
        }
        out
    }

    fn draw_title(&self, buf: &mut Buffer, area: Rect, state: &AppState) {
        let theme = &state.theme;
        let base = theme.surface();
        let y = area.y;
        let running = state
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .count();
        let x = put(
            buf,
            area.x + 1,
            y,
            "JOBS",
            base.patch(theme.inspector_label()),
        );
        let x = put(buf, x, y, " · ", base.patch(theme.dim()));
        let style = if running > 0 {
            theme.jobs_running()
        } else {
            theme.dim()
        };
        let title_end = put(buf, x, y, &format!("{running} running"), base.patch(style));
        if state.focus != Focus::JobsDrawer {
            return;
        }
        let hints = self.hints();
        let width: usize = hints
            .iter()
            .map(|(k, l)| text::width(k) + 1 + text::width(l))
            .sum::<usize>()
            + 2 * hints.len().saturating_sub(1);
        let Some(mut x) = area.right().checked_sub(width as u16 + 1) else {
            return;
        };
        if x < title_end + 2 {
            return;
        }
        for (i, (k, l)) in hints.iter().enumerate() {
            if i > 0 {
                x += 2;
            }
            x = put(buf, x, y, k, base.patch(theme.hint_key()));
            x = put(buf, x, y, &format!(" {l}"), base.patch(theme.hint()));
        }
    }
}

fn draw_line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    line: &Line,
    cols: &Columns,
    selected: bool,
    state: &AppState,
) {
    let theme: &Theme = &state.theme;
    let job = line.job;
    let base = if selected {
        theme.row_selected()
    } else {
        theme.surface()
    };
    buf.set_style(Rect::new(area.x, y, area.width, 1), base);
    let (state_style, icon) = theme.job_state(job.kind, &job.state);
    let quiet = matches!(job.state, JobState::Paused | JobState::Queued);
    let text_style = if quiet { base.patch(theme.dim()) } else { base };

    let mut x = put(buf, area.x + 1, y, icon, base.patch(state_style)) + 1;
    let kind = format!("{:<KIND_WIDTH$}", kind_label(job.kind));
    x = put(buf, x, y, &kind, base.patch(theme.job(job.kind))) + 1;
    put(buf, x, y, &line.tab, base.patch(theme.dim()));
    x += cols.tab as u16;
    let title = text::truncate_middle(&job.title, cols.title);
    put(buf, x, y, &title, text_style);

    // Right-hand columns, laid out from the right edge.
    let elapsed_x = usize::from(area.right()) - 1 - cols.elapsed;
    put(
        buf,
        elapsed_x as u16,
        y,
        &format!("{:>w$}", line.elapsed, w = cols.elapsed),
        base.patch(theme.dim()),
    );
    let text_x = elapsed_x - 2 - cols.text;
    let gauge_x = if cols.text > 0 {
        text_x - 2 - GAUGE_CELLS
    } else {
        elapsed_x - 2 - GAUGE_CELLS
    };
    if let JobState::Failed(err) = &job.state {
        // The error replaces the gauge and the text.
        let err = text::truncate_end(err, elapsed_x - 2 - gauge_x);
        put(
            buf,
            gauge_x as u16,
            y,
            &err,
            base.patch(theme.value_error()),
        );
        return;
    }
    let fraction = match job.state {
        JobState::Queued => Some(0.0),
        JobState::Done => Some(1.0),
        _ => job.overall_fraction(),
    };
    let fill = base.patch(theme.job(job.kind));
    let track = base.patch(theme.bar_track());
    for (i, filled) in gauge_cells(fraction, state.ticks).iter().enumerate() {
        let (glyph, style) = if *filled {
            ("█", fill)
        } else {
            ("░", track)
        };
        put(buf, (gauge_x + i) as u16, y, glyph, style);
    }
    if cols.text > 0 {
        let t = text::truncate_end(&line.text, cols.text);
        put(buf, text_x as u16, y, &t, text_style);
    }
}

impl Component for JobsDrawer {
    fn register_config_handler(&mut self, config: Config) -> color_eyre::Result<()> {
        self.keymap = config.keybindings;
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let buf = frame.buffer_mut();
        buf.set_style(area, theme.surface());
        if area.height == 0 || area.width < 40 {
            return Ok(());
        }
        self.draw_title(buf, area, state);

        let jobs = state.jobs.drawer_order();
        let rows = usize::from(area.height - 1);
        if jobs.is_empty() || rows == 0 {
            self.offset = 0;
            return Ok(());
        }
        let selected = state.jobs_selected.min(jobs.len() - 1);
        if selected < self.offset {
            self.offset = selected;
        } else if selected >= self.offset + rows {
            self.offset = selected + 1 - rows;
        }
        self.offset = self.offset.min(jobs.len().saturating_sub(rows));

        let now = crate::app::now();
        let many_tabs = state.tabs.len() > 1;
        let lines: Vec<Line> = jobs
            .iter()
            .skip(self.offset)
            .take(rows)
            .map(|job| {
                let tab_pos = state.tabs.iter().position(|t| t.id == job.tab);
                let rate = if job.kind == JobKind::Index {
                    tab_pos
                        .and_then(|i| state.tabs[i].loaded.as_ref())
                        .and_then(|l| l.rate.rate())
                } else {
                    None
                };
                let text = match job.state {
                    JobState::Queued => "queued".to_owned(),
                    _ => progress_text(&job.progress, rate),
                };
                Line {
                    job,
                    tab: match tab_pos {
                        Some(i) if many_tabs => format!("[{}] ", i + 1),
                        _ => String::new(),
                    },
                    text,
                    elapsed: job.elapsed(now).map(format_elapsed).unwrap_or_default(),
                }
            })
            .collect();
        let cols = columns(&lines, usize::from(area.width));
        let focused = state.focus == Focus::JobsDrawer;
        for (i, line) in lines.iter().enumerate() {
            let y = area.y + 1 + i as u16;
            let is_selected = focused && self.offset + i == selected;
            draw_line(buf, area, y, line, &cols, is_selected, state);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tachy_core::jobs::{
        BytesProgress, ExportProgress, FilterProgress, RowsProgress, SortProgress,
    };

    use super::*;

    #[test]
    fn progress_texts() {
        let bytes = BytesProgress::new(7_400_000_000);
        bytes.add(3_100_000_000);
        assert_eq!(
            progress_text(&Progress::Bytes(Arc::new(bytes)), Some(3.4e9)),
            format!(
                "{} / {} · {}",
                format_size(3_100_000_000),
                format_size(7_400_000_000),
                format_rate(3.4e9)
            )
        );
        let f = FilterProgress::new(1000);
        f.add(500, 12_345);
        assert!(progress_text(&Progress::Filter(Arc::new(f)), None).ends_with("· 12,345 matches"));
        let s = Arc::new(SortProgress::new(100));
        s.add(42);
        assert_eq!(
            progress_text(&Progress::Sort(Arc::clone(&s)), None),
            "extract 42%"
        );
        s.start_phase(SortPhase::SortRuns, 12);
        s.add(3);
        assert_eq!(
            progress_text(&Progress::Sort(Arc::clone(&s)), None),
            "sort runs 3/12"
        );
        s.start_merge_pass(1, 2, 100);
        assert_eq!(progress_text(&Progress::Sort(s), None), "merge pass 1/2");
        let r = RowsProgress::new(412_000_000);
        r.add(12_300_000);
        assert_eq!(
            progress_text(&Progress::Rows(Arc::new(r)), None),
            "12.3M / 412M rows"
        );
        let e = ExportProgress::new(0);
        e.add(1_200_000, 340 * 1024 * 1024);
        assert_eq!(
            progress_text(&Progress::Export(Arc::new(e)), None),
            "1.2M rows · 340.0 MB written"
        );
    }

    #[test]
    fn elapsed() {
        assert_eq!(format_elapsed(Duration::from_secs(12)), "12 s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3 min");
        assert_eq!(format_elapsed(Duration::from_secs(3900)), "1 h 5 min");
    }

    #[test]
    fn gauges() {
        let s = |c: [bool; GAUGE_CELLS]| -> String {
            c.iter().map(|&f| if f { '█' } else { '░' }).collect()
        };
        assert_eq!(s(gauge_cells(Some(0.5), 0)), "██████░░░░░░");
        assert_eq!(s(gauge_cells(Some(1.0), 0)), "████████████");
        assert_eq!(s(gauge_cells(None, 0)), "███░░░░░░░░░");
        assert_eq!(s(gauge_cells(None, 1)), "░███░░░░░░░░");
        assert_eq!(s(gauge_cells(None, 9)), "░░░░░░░░░███");
        assert_eq!(s(gauge_cells(None, 10)), "░░░░░░░░███░");
        assert_eq!(s(gauge_cells(None, 18)), "███░░░░░░░░░");
    }
}
