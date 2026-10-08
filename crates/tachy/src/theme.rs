//! Tachy colour theme (spec §14).
//!
//! All colours are 24-bit (`Color::Rgb`). Drawing code never uses raw
//! colours: it calls the style helpers below (`tests/no_raw_colors.rs`
//! enforces this). Accents keep meaning: amber = "you are here / act",
//! teal = "file / data info", purple = "command / jobs", coral = "problem".

// Most helpers are only called by the UI tasks of M1–M7.
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};
use tachy_core::jobs::{JobKind, JobState};

use crate::{mode::Mode, toast::ToastLevel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    // Backgrounds and surfaces
    pub bg: Color,
    pub bg_alt_row: Color,
    pub surface: Color,
    pub surface_raised: Color,
    pub status_bg: Color,
    pub selection_bg: Color,
    pub dialog_title_bg: Color,
    pub track: Color,

    // Borders
    pub border: Color,
    pub border_inner: Color,
    pub border_strong: Color,

    // Text
    pub fg: Color,
    pub fg_muted: Color,
    pub fg_dim: Color,

    // Accents
    pub amber: Color,
    pub amber_bright: Color,
    pub amber_tint: Color,
    pub teal: Color,
    pub purple: Color,
    pub purple_tint: Color,
    pub green: Color,
    pub coral: Color,

    /// `NO_COLOR`: every helper returns a modifier-only style (M7-01).
    pub monochrome: bool,
}

/// What the terminal can display (§14 "Fallback", M7-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    /// 24-bit colour (`COLORTERM=truecolor` / `24bit`).
    TrueColor,
    /// The xterm-256 palette.
    Ansi256,
    /// `NO_COLOR` is set: modifiers only.
    NoColor,
}

impl Theme {
    /// Names accepted by [`Theme::by_name`] (and `--theme`). Only `dark` exists in v1.
    pub const NAMES: &[&str] = &["dark"];

    pub const DARK: Theme = Theme {
        bg: Color::Rgb(0x0E, 0x11, 0x16),
        bg_alt_row: Color::Rgb(0x10, 0x14, 0x1A),
        surface: Color::Rgb(0x13, 0x18, 0x20),
        surface_raised: Color::Rgb(0x16, 0x1C, 0x25),
        status_bg: Color::Rgb(0x1A, 0x21, 0x2B),
        selection_bg: Color::Rgb(0x1B, 0x25, 0x33),
        dialog_title_bg: Color::Rgb(0x1E, 0x26, 0x32),
        track: Color::Rgb(0x1E, 0x25, 0x30),

        border: Color::Rgb(0x23, 0x2A, 0x34),
        border_inner: Color::Rgb(0x2C, 0x35, 0x42),
        border_strong: Color::Rgb(0x3A, 0x44, 0x52),

        fg: Color::Rgb(0xD3, 0xD9, 0xE0),
        fg_muted: Color::Rgb(0xA9, 0xB4, 0xC2),
        fg_dim: Color::Rgb(0x8A, 0x94, 0xA3),

        amber: Color::Rgb(0xF2, 0xB0, 0x4A),
        amber_bright: Color::Rgb(0xFF, 0xD0, 0x8A),
        amber_tint: Color::Rgb(0x3A, 0x2F, 0x17),
        teal: Color::Rgb(0x4F, 0xC1, 0xC9),
        purple: Color::Rgb(0xC7, 0xA6, 0xFF),
        purple_tint: Color::Rgb(0x2A, 0x23, 0x40),
        green: Color::Rgb(0x9F, 0xD3, 0x8A),
        coral: Color::Rgb(0xF2, 0x8B, 0x6B),

        monochrome: false,
    };

    /// The theme called `name` (§3 `--theme`, §15 `theme`), if it exists.
    pub fn by_name(name: &str) -> Option<Theme> {
        match name {
            "dark" => Some(Self::DARK),
            _ => None,
        }
    }

    /// This theme mapped to what the terminal can display (§14 "Fallback",
    /// M7-01). Called once at startup; drawing code only ever sees an adapted
    /// theme, so no per-draw work happens.
    ///
    /// - `TrueColor`: unchanged.
    /// - `Ansi256`: every `Color::Rgb` field becomes the nearest xterm-256
    ///   index ([`nearest_xterm256`]). The near-black surfaces collapse onto
    ///   one or two grey-ramp steps; `selection_bg` and `status_bg` are kept
    ///   distinct from `bg` ([`Theme::keep_distinct`]). `bg` and `bg_alt_row`
    ///   map to the same index, so **striped rows disappear in 256-colour
    ///   mode**; that is accepted.
    /// - `NoColor`: every colour becomes `Color::Reset` and `monochrome` is
    ///   set, so each helper returns a modifier-only style (bold, reverse,
    ///   underline, dim).
    pub fn adapt(self, support: ColorSupport) -> Theme {
        match support {
            ColorSupport::TrueColor => self,
            ColorSupport::Ansi256 => {
                let mut t = self.map_colors(|c| match c {
                    Color::Rgb(r, g, b) => Color::Indexed(nearest_xterm256(r, g, b)),
                    other => other,
                });
                t.keep_distinct(&self);
                t
            }
            ColorSupport::NoColor => Theme {
                monochrome: true,
                ..self.map_colors(|_| Color::Reset)
            },
        }
    }

    /// Applies `f` to every colour field.
    fn map_colors(self, f: impl Fn(Color) -> Color) -> Theme {
        Theme {
            bg: f(self.bg),
            bg_alt_row: f(self.bg_alt_row),
            surface: f(self.surface),
            surface_raised: f(self.surface_raised),
            status_bg: f(self.status_bg),
            selection_bg: f(self.selection_bg),
            dialog_title_bg: f(self.dialog_title_bg),
            track: f(self.track),
            border: f(self.border),
            border_inner: f(self.border_inner),
            border_strong: f(self.border_strong),
            fg: f(self.fg),
            fg_muted: f(self.fg_muted),
            fg_dim: f(self.fg_dim),
            amber: f(self.amber),
            amber_bright: f(self.amber_bright),
            amber_tint: f(self.amber_tint),
            teal: f(self.teal),
            purple: f(self.purple),
            purple_tint: f(self.purple_tint),
            green: f(self.green),
            coral: f(self.coral),
            monochrome: self.monochrome,
        }
    }

    /// After 256-colour mapping: the selected row and the status line must
    /// stay visible against `bg`. When one of them collapsed onto `bg`'s
    /// index, it moves one step (index ± 1): up when it is lighter than `bg`
    /// in `original` (the usual case: the next grey-ramp step), down
    /// otherwise. `bg` itself never moves, since it is shared by both pairs.
    fn keep_distinct(&mut self, original: &Theme) {
        let Color::Indexed(bg) = self.bg else {
            return;
        };
        for (mapped, orig) in [
            (&mut self.selection_bg, original.selection_bg),
            (&mut self.status_bg, original.status_bg),
        ] {
            if *mapped != Color::Indexed(bg) {
                continue;
            }
            let up = luma(orig) >= luma(original.bg);
            let nudged = match (up, bg) {
                (true, 255) | (false, 16) => bg ^ 1, // 254 or 17: the only step left
                (true, _) => bg + 1,
                (false, _) => bg - 1,
            };
            *mapped = Color::Indexed(nudged);
        }
    }

    /// `colored` normally; `mono` modifiers only under `NO_COLOR` (§14).
    fn pick(&self, colored: Style, mono: Modifier) -> Style {
        if self.monochrome {
            Style::new().add_modifier(mono)
        } else {
            colored
        }
    }

    // ---- base ----------------------------------------------------------

    /// Default text on the app background (§14 `bg`, `fg`).
    pub fn base(&self) -> Style {
        self.pick(Style::new().fg(self.fg).bg(self.bg), Modifier::empty())
    }

    /// Default text on `surface`: top bar, table header, inspector (§14).
    pub fn surface(&self) -> Style {
        self.pick(Style::new().fg(self.fg).bg(self.surface), Modifier::empty())
    }

    /// Secondary text (§14 `fg_muted`).
    pub fn muted(&self) -> Style {
        self.pick(Style::new().fg(self.fg_muted), Modifier::empty())
    }

    /// Labels, types, hints (§14 `fg_dim`).
    pub fn dim(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim), Modifier::DIM)
    }

    /// Default dividers (§14 `border`).
    pub fn border(&self) -> Style {
        self.pick(Style::new().fg(self.border), Modifier::empty())
    }

    /// Border of the focused panel: inspector, jobs drawer (M3-03, M5-02).
    pub fn focused_border(&self) -> Style {
        self.pick(Style::new().fg(self.amber), Modifier::BOLD)
    }

    /// Spinner while waiting for a pending jump or search (M2-03, M4-05).
    pub fn spinner(&self) -> Style {
        self.pick(Style::new().fg(self.teal), Modifier::empty())
    }

    // ---- top bar -------------------------------------------------------

    /// `tachy` in the top bar: bold amber (§11.2).
    pub fn app_name(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Inactive tab: dim (§11.2).
    pub fn tab(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim).bg(self.surface), Modifier::DIM)
    }

    /// Active tab: default text on the app background, amber underline (§11.2).
    pub fn tab_active(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.fg)
                .bg(self.bg)
                .underline_color(self.amber)
                .add_modifier(Modifier::UNDERLINED),
            Modifier::UNDERLINED | Modifier::BOLD,
        )
    }

    /// The mode pill in the top-right corner: dark text on the mode's accent
    /// (§11.2, §12.1). `Filter` and `Search` share amber. `Dialog` never
    /// reaches the pill (the pill shows `pill_mode`); it falls back to teal.
    pub fn mode_pill(&self, mode: Mode) -> Style {
        let bg = match mode {
            Mode::Normal | Mode::Dialog => self.teal,
            Mode::Filter | Mode::Search => self.amber,
            Mode::Command => self.purple,
        };
        self.pick(
            Style::new().fg(self.bg).bg(bg).add_modifier(Modifier::BOLD),
            Modifier::REVERSED | Modifier::BOLD,
        )
    }

    // ---- table ---------------------------------------------------------

    /// Column name in the table header: bold, on `surface` (§11.3).
    pub fn header(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.fg)
                .bg(self.surface)
                .add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Type label under the column name: dim, on `surface` (§11.3).
    pub fn header_type(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim).bg(self.surface), Modifier::DIM)
    }

    /// Header of the column the cursor is in: amber (§11.3).
    pub fn header_active(&self) -> Style {
        self.pick(
            self.header().fg(self.amber),
            Modifier::BOLD | Modifier::UNDERLINED,
        )
    }

    /// Header of a column the active filter uses: bold amber on `surface` (§12.4).
    pub fn header_used_by_filter(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.amber)
                .bg(self.surface)
                .add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// `▲` / `▼` after the name of a sorted column: bold amber (§11.3).
    pub fn sort_indicator(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// `→ N more cols` in the last header cell: dim on `surface` (§11.3).
    pub fn overflow_hint(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim).bg(self.surface), Modifier::DIM)
    }

    /// Row numbers in the gutter: dim (§11.3).
    pub fn gutter(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim), Modifier::DIM)
    }

    /// Gutter header (`source row` in filtered / sorted views): dim on `surface` (§11.3).
    pub fn gutter_header(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim).bg(self.surface), Modifier::DIM)
    }

    /// Row number of the selected row: amber (§11.3).
    pub fn gutter_selected(&self) -> Style {
        self.pick(Style::new().fg(self.amber), Modifier::BOLD)
    }

    /// Row number of a ragged row: coral (§6.4).
    pub fn gutter_ragged(&self) -> Style {
        self.pick(Style::new().fg(self.coral), Modifier::BOLD)
    }

    /// Divider between columns and next to the gutter: `border` (§11.3).
    pub fn column_divider(&self) -> Style {
        self.pick(Style::new().fg(self.border), Modifier::empty())
    }

    /// Divider after the frozen columns: `border_strong` (§11.3).
    pub fn frozen_divider(&self) -> Style {
        self.pick(Style::new().fg(self.border_strong), Modifier::BOLD)
    }

    /// Striped rows, alternating `bg` / `bg_alt_row`: pass the row's index in the view (§11.3).
    pub fn row(&self, index: usize) -> Style {
        let bg = if index % 2 == 1 {
            self.bg_alt_row
        } else {
            self.bg
        };
        self.pick(Style::new().fg(self.fg).bg(bg), Modifier::empty())
    }

    /// The selected row: `selection_bg` (§11.3).
    pub fn row_selected(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg).bg(self.selection_bg),
            Modifier::BOLD,
        )
    }

    /// The cursor cell: dark bold text on amber (§11.3).
    pub fn cursor_cell(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.bg)
                .bg(self.amber)
                .add_modifier(Modifier::BOLD),
            Modifier::REVERSED | Modifier::BOLD,
        )
    }

    /// A cell (or substring) matching the active filter or search: `amber_tint` background (§14).
    pub fn match_highlight(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).bg(self.amber_tint),
            Modifier::UNDERLINED,
        )
    }

    /// Escaped control characters (`\t`, `\x07`, …) inside a value: dim (§6.2).
    pub fn control_char(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg_dim).add_modifier(Modifier::DIM),
            Modifier::DIM,
        )
    }

    /// A value that is an error or negative: coral (§14).
    pub fn value_error(&self) -> Style {
        self.pick(Style::new().fg(self.coral), Modifier::BOLD)
    }

    /// A value marking success: green (§14).
    pub fn value_ok(&self) -> Style {
        self.pick(Style::new().fg(self.green), Modifier::empty())
    }

    // ---- inspector -----------------------------------------------------

    /// Section label (`COLUMN`, `TOP VALUES`, `RECORD`): dim; the caller uppercases (§11.4).
    pub fn inspector_label(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim), Modifier::DIM)
    }

    /// Section label of the focused inspector section: bold amber (§11.4, M3-03).
    pub fn inspector_label_focused(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Field name in the RECORD section: dim (§11.4).
    pub fn record_field(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim), Modifier::DIM)
    }

    /// Field value in the RECORD section: muted (§11.4).
    pub fn record_value(&self) -> Style {
        self.pick(Style::new().fg(self.fg_muted), Modifier::empty())
    }

    /// Filled part of a top-values bar: teal (§11.4). Bars are drawn with
    /// `█` (fill) and `░` (track) glyphs, so they stay visible under `NO_COLOR`.
    pub fn bar_fill(&self) -> Style {
        self.pick(Style::new().fg(self.teal), Modifier::empty())
    }

    /// Empty part of a top-values bar: `track` (§11.4).
    pub fn bar_track(&self) -> Style {
        self.pick(Style::new().fg(self.track), Modifier::empty())
    }

    /// The cell of a top-values bar holding a partial (eighth-block) glyph:
    /// teal glyph over the track colour, so the rest of the cell reads as
    /// track (M3-03).
    pub fn bar_partial(&self) -> Style {
        self.pick(Style::new().fg(self.teal).bg(self.track), Modifier::empty())
    }

    /// A dialog value the user changed (Detected format `*`): amber (M2-04).
    pub fn value_changed(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    // ---- status + hints -----------------------------------------------

    /// The status line: default text on `status_bg` (§11.5).
    pub fn status_line(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg).bg(self.status_bg),
            Modifier::empty(),
        )
    }

    /// File name in the status line: bold teal (§11.5).
    pub fn status_file(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.teal)
                .bg(self.status_bg)
                .add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Number of running jobs in the status line: bold purple on `status_bg` (§11.5).
    pub fn status_jobs(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.purple)
                .bg(self.status_bg)
                .add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Warnings in the status line (ragged rows, unterminated quote): coral on `status_bg` (§11.5, §16).
    pub fn status_warning(&self) -> Style {
        self.pick(
            Style::new().fg(self.coral).bg(self.status_bg),
            Modifier::BOLD,
        )
    }

    /// Secondary status text (dialect summary): dim on `status_bg` (§11.5).
    pub fn status_dim(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg_dim).bg(self.status_bg),
            Modifier::DIM,
        )
    }

    /// Action names in the key-hint line: dim (§11.6).
    pub fn hint(&self) -> Style {
        self.pick(Style::new().fg(self.fg_dim), Modifier::DIM)
    }

    /// Keys in the key-hint line: bold amber (§11.6).
    pub fn hint_key(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// A toast: errors and warnings coral, info in the normal text colour,
    /// on the app background (§16, M1-09). Under `NO_COLOR` errors and
    /// warnings are bold.
    pub fn toast(&self, level: ToastLevel) -> Style {
        let (fg, mono) = match level {
            ToastLevel::Error | ToastLevel::Warning => (self.coral, Modifier::BOLD),
            ToastLevel::Info => (self.fg, Modifier::empty()),
        };
        self.pick(Style::new().fg(fg).bg(self.bg), mono)
    }

    // ---- progress / gauges --------------------------------------------

    /// For `Gauge` / `LineGauge`: fg = filled part, bg = track (§14 `track`).
    /// Gauges are drawn with `█` (fill) and `░` (track) glyphs, so progress
    /// stays visible under `NO_COLOR`, where this style has no colour.
    pub fn gauge(&self, fill: Color) -> Style {
        self.pick(Style::new().fg(fill).bg(self.track), Modifier::empty())
    }

    // ---- query bar -----------------------------------------------------

    /// The `filter ›` / `search ›` prompt: bold amber (§12.4).
    pub fn query_prompt(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Column names in a query: default text (§9.4).
    pub fn query_column(&self) -> Style {
        self.pick(Style::new().fg(self.fg), Modifier::empty())
    }

    /// Operators in a query: teal (§9.4).
    pub fn query_operator(&self) -> Style {
        self.pick(Style::new().fg(self.teal), Modifier::empty())
    }

    /// Keywords (`and`, `or`, `not`, `in`, …) in a query: teal, like operators (§9.4).
    pub fn query_keyword(&self) -> Style {
        self.query_operator()
    }

    /// String literals in a query: green (§9.4).
    pub fn query_string(&self) -> Style {
        self.pick(Style::new().fg(self.green), Modifier::empty())
    }

    /// Number literals in a query: purple (§9.4).
    pub fn query_number(&self) -> Style {
        self.pick(Style::new().fg(self.purple), Modifier::empty())
    }

    /// The span of a query error: coral, with a coral underline (§9.4).
    pub fn query_error(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.coral)
                .underline_color(self.coral)
                .add_modifier(Modifier::UNDERLINED),
            Modifier::BOLD | Modifier::UNDERLINED,
        )
    }

    /// The error message under the query bar: coral (§9.2).
    pub fn inline_error(&self) -> Style {
        self.pick(Style::new().fg(self.coral), Modifier::BOLD)
    }

    // ---- overlays ------------------------------------------------------

    /// Dialog body: `surface_raised` (§11.7).
    pub fn dialog(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg).bg(self.surface_raised),
            Modifier::empty(),
        )
    }

    /// 1-cell dialog border in the dialog's accent colour (§11.7).
    pub fn dialog_border(&self, accent: Color) -> Style {
        self.pick(
            Style::new().fg(accent).bg(self.surface_raised),
            Modifier::empty(),
        )
    }

    /// Dialog title bar: bold accent on `dialog_title_bg` (§11.7).
    pub fn dialog_title(&self, accent: Color) -> Style {
        self.pick(
            Style::new()
                .fg(accent)
                .bg(self.dialog_title_bg)
                .add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// A dialog button: `surface_raised` (§11.7).
    pub fn button(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg).bg(self.surface_raised),
            Modifier::empty(),
        )
    }

    /// The primary dialog button: dark bold text on teal (§14 `teal`).
    pub fn button_primary(&self) -> Style {
        self.pick(
            Style::new()
                .fg(self.bg)
                .bg(self.teal)
                .add_modifier(Modifier::BOLD),
            Modifier::REVERSED | Modifier::BOLD,
        )
    }

    /// Command palette border: purple on `surface_raised` (§11.7).
    pub fn palette_border(&self) -> Style {
        self.pick(
            Style::new().fg(self.purple).bg(self.surface_raised),
            Modifier::empty(),
        )
    }

    /// Selected palette item: purple on `purple_tint` (§14).
    pub fn palette_item_selected(&self) -> Style {
        self.pick(
            Style::new().fg(self.purple).bg(self.purple_tint),
            Modifier::REVERSED,
        )
    }

    /// Characters matched by the palette's fuzzy search: bold amber (M6-02).
    pub fn palette_match(&self) -> Style {
        self.pick(
            Style::new().fg(self.amber).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Content behind a modal (§14 rules). Terminals have no alpha, so the
    /// 55% overlay and 35% opacity from the mockup are approximated with DIM.
    pub fn backdrop(&self) -> Style {
        self.pick(
            Style::new().fg(self.fg_dim).add_modifier(Modifier::DIM),
            Modifier::DIM,
        )
    }

    // ---- jobs ----------------------------------------------------------

    /// A job's kind colour (§10.1): Index / Profile teal, Filter / Export amber,
    /// Sort / Dupes purple.
    pub fn job(&self, kind: JobKind) -> Style {
        let fg = match kind {
            JobKind::Index | JobKind::Profile => self.teal,
            JobKind::Filter | JobKind::Export => self.amber,
            JobKind::Sort | JobKind::Dupes => self.purple,
        };
        self.pick(Style::new().fg(fg), Modifier::empty())
    }

    /// `· N running` in the jobs drawer's title while jobs run: bold purple
    /// (M5-02).
    pub fn jobs_running(&self) -> Style {
        self.pick(
            Style::new().fg(self.purple).add_modifier(Modifier::BOLD),
            Modifier::BOLD,
        )
    }

    /// Style and symbol of a job's state (§10.2): `✓` green, `✗` coral,
    /// `–` / `⏸` / `…` dim, `▸` in the kind's colour while running.
    pub fn job_state(&self, kind: JobKind, state: &JobState) -> (Style, &'static str) {
        match state {
            JobState::Done => (self.value_ok(), "✓"),
            JobState::Failed(_) => (self.value_error(), "✗"),
            JobState::Cancelled => (self.dim(), "–"),
            JobState::Paused => (self.dim(), "⏸"),
            JobState::Queued => (self.dim(), "…"),
            JobState::Running => (self.job(kind), "▸"),
        }
    }
}

impl ColorSupport {
    /// What the terminal supports, from the process environment (§14).
    pub fn detect() -> ColorSupport {
        Self::from_env(|name| std::env::var(name).ok())
    }

    /// [`ColorSupport::detect`] with an injectable environment:
    /// `NO_COLOR` set and non-empty (no-color.org) → `NoColor`; else
    /// `COLORTERM` = `truecolor` / `24bit` (any case) → `TrueColor`; else
    /// `Ansi256`.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> ColorSupport {
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return ColorSupport::NoColor;
        }
        match get("COLORTERM") {
            Some(v) if v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit") => {
                ColorSupport::TrueColor
            }
            _ => ColorSupport::Ansi256,
        }
    }
}

/// The six levels of each channel in the xterm-256 colour cube (16–231).
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The RGB value of xterm-256 index `i`, for 16–255.
fn xterm256_rgb(i: u8) -> (u8, u8, u8) {
    debug_assert!(i >= 16);
    if i >= 232 {
        let v = 8 + 10 * (i - 232);
        (v, v, v)
    } else {
        let n = usize::from(i - 16);
        (
            CUBE_LEVELS[n / 36],
            CUBE_LEVELS[n / 6 % 6],
            CUBE_LEVELS[n % 6],
        )
    }
}

/// "Redmean" weighted squared distance between two colours: cheap and much
/// closer to perceived difference than plain RGB distance.
fn redmean_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let rmean = (f64::from(a.0) + f64::from(b.0)) / 2.0;
    let dr = f64::from(a.0) - f64::from(b.0);
    let dg = f64::from(a.1) - f64::from(b.1);
    let db = f64::from(a.2) - f64::from(b.2);
    (2.0 + rmean / 256.0) * dr * dr + 4.0 * dg * dg + (2.0 + (255.0 - rmean) / 256.0) * db * db
}

/// The closest xterm-256 index to `#rrggbb` among the 6×6×6 cube (16–231)
/// and the grey ramp (232–255), by redmean distance (§14 "Fallback").
/// Indices 0–15 are skipped: users theme those.
pub fn nearest_xterm256(r: u8, g: u8, b: u8) -> u8 {
    (16..=255u8)
        .min_by(|&x, &y| {
            redmean_distance((r, g, b), xterm256_rgb(x))
                .total_cmp(&redmean_distance((r, g, b), xterm256_rgb(y)))
        })
        .unwrap_or(16)
}

/// Rough brightness of an RGB colour, to tell which of two is lighter.
fn luma(c: Color) -> u32 {
    match c {
        Color::Rgb(r, g, b) => 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b),
        _ => 0,
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::DARK
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn hex(c: Color) -> String {
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02X}{g:02X}{b:02X}"),
            other => panic!("not an RGB colour: {other:?}"),
        }
    }

    /// Every field of `Theme::DARK` against the §14 table.
    #[test]
    fn dark_matches_spec_hex_values() {
        let t = Theme::DARK;
        let table = [
            ("bg", t.bg, "#0E1116"),
            ("bg_alt_row", t.bg_alt_row, "#10141A"),
            ("surface", t.surface, "#131820"),
            ("surface_raised", t.surface_raised, "#161C25"),
            ("status_bg", t.status_bg, "#1A212B"),
            ("selection_bg", t.selection_bg, "#1B2533"),
            ("dialog_title_bg", t.dialog_title_bg, "#1E2632"),
            ("track", t.track, "#1E2530"),
            ("border", t.border, "#232A34"),
            ("border_inner", t.border_inner, "#2C3542"),
            ("border_strong", t.border_strong, "#3A4452"),
            ("fg", t.fg, "#D3D9E0"),
            ("fg_muted", t.fg_muted, "#A9B4C2"),
            ("fg_dim", t.fg_dim, "#8A94A3"),
            ("amber", t.amber, "#F2B04A"),
            ("amber_bright", t.amber_bright, "#FFD08A"),
            ("amber_tint", t.amber_tint, "#3A2F17"),
            ("teal", t.teal, "#4FC1C9"),
            ("purple", t.purple, "#C7A6FF"),
            ("purple_tint", t.purple_tint, "#2A2340"),
            ("green", t.green, "#9FD38A"),
            ("coral", t.coral, "#F28B6B"),
        ];
        assert_eq!(table.len(), 22);
        for (name, color, expected) in table {
            assert_eq!(hex(color), expected, "{name}");
        }
    }

    #[test]
    fn mode_pill_colours() {
        let t = Theme::DARK;
        for (mode, bg) in [
            (Mode::Normal, t.teal),
            (Mode::Filter, t.amber),
            (Mode::Search, t.amber),
            (Mode::Command, t.purple),
            (Mode::Dialog, t.teal),
        ] {
            let style = t.mode_pill(mode);
            assert_eq!(style.bg, Some(bg), "{mode:?}");
            assert_eq!(style.fg, Some(t.bg), "{mode:?}");
            assert!(style.add_modifier.contains(Modifier::BOLD), "{mode:?}");
        }
    }

    #[test]
    fn job_kind_colours() {
        let t = Theme::DARK;
        for (kind, fg) in [
            (JobKind::Index, t.teal),
            (JobKind::Filter, t.amber),
            (JobKind::Sort, t.purple),
            (JobKind::Profile, t.teal),
            (JobKind::Export, t.amber),
        ] {
            assert_eq!(t.job(kind).fg, Some(fg), "{kind:?}");
        }
    }

    #[test]
    fn job_state_styles_and_symbols() {
        let t = Theme::DARK;
        let cases = [
            (JobState::Done, t.green, "✓"),
            (JobState::Failed("disk full".into()), t.coral, "✗"),
            (JobState::Cancelled, t.fg_dim, "–"),
            (JobState::Paused, t.fg_dim, "⏸"),
            (JobState::Queued, t.fg_dim, "…"),
            (JobState::Running, t.purple, "▸"),
        ];
        for (state, fg, symbol) in cases {
            let (style, sym) = t.job_state(JobKind::Sort, &state);
            assert_eq!(style.fg, Some(fg), "{state:?}");
            assert_eq!(sym, symbol, "{state:?}");
        }
        // Running takes the kind's colour.
        assert_eq!(
            t.job_state(JobKind::Index, &JobState::Running).0.fg,
            Some(t.teal)
        );
    }

    #[test]
    fn toast_levels() {
        let t = Theme::DARK;
        assert_eq!(t.toast(ToastLevel::Error).fg, Some(t.coral));
        assert_eq!(t.toast(ToastLevel::Warning).fg, Some(t.coral));
        assert_eq!(t.toast(ToastLevel::Info).fg, Some(t.fg));
        assert_eq!(t.toast(ToastLevel::Info).bg, Some(t.bg));
    }

    #[test]
    fn query_error_is_underlined_coral() {
        let t = Theme::DARK;
        let s = t.query_error();
        assert_eq!(s.fg, Some(t.coral));
        assert_eq!(s.underline_color, Some(t.coral));
        assert!(s.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn by_name() {
        assert_eq!(Theme::by_name("dark"), Some(Theme::DARK));
        assert_eq!(Theme::by_name("light"), None);
        for name in Theme::NAMES {
            assert!(Theme::by_name(name).is_some(), "{name}");
        }
    }

    #[test]
    fn truecolor_is_unchanged() {
        assert_eq!(Theme::DARK.adapt(ColorSupport::TrueColor), Theme::DARK);
    }

    // ---- detection -----------------------------------------------------

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn detection_from_env() {
        use ColorSupport::*;
        let cases: [(&[(&str, &str)], ColorSupport); 9] = [
            (&[], Ansi256),
            (&[("COLORTERM", "truecolor")], TrueColor),
            (&[("COLORTERM", "24bit")], TrueColor),
            (&[("COLORTERM", "TrueColor")], TrueColor),
            (&[("COLORTERM", "24BIT")], TrueColor),
            (&[("COLORTERM", "")], Ansi256),
            (&[("COLORTERM", "yes")], Ansi256),
            (&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")], NoColor),
            // An empty NO_COLOR doesn't count (no-color.org).
            (&[("NO_COLOR", ""), ("COLORTERM", "truecolor")], TrueColor),
        ];
        for (vars, expected) in cases {
            assert_eq!(ColorSupport::from_env(env(vars)), expected, "{vars:?}");
        }
    }

    // ---- xterm-256 mapping ---------------------------------------------

    #[test]
    fn xterm256_palette_values() {
        assert_eq!(xterm256_rgb(16), (0, 0, 0));
        assert_eq!(xterm256_rgb(215), (255, 175, 95));
        assert_eq!(xterm256_rgb(231), (255, 255, 255));
        assert_eq!(xterm256_rgb(232), (8, 8, 8));
        assert_eq!(xterm256_rgb(233), (18, 18, 18));
        assert_eq!(xterm256_rgb(255), (238, 238, 238));
    }

    #[test]
    fn nearest_xterm256_extremes_and_greys() {
        assert_eq!(nearest_xterm256(0, 0, 0), 16); // cube 0,0,0 (exact)
        assert_eq!(nearest_xterm256(255, 255, 255), 231); // cube 5,5,5 (exact)
        assert_eq!(nearest_xterm256(0x80, 0x80, 0x80), 244); // ramp #808080 (exact)
        assert_eq!(nearest_xterm256(0x12, 0x12, 0x12), 233); // ramp #121212 (exact)
        assert_eq!(nearest_xterm256(0xEE, 0xEE, 0xEE), 255); // ramp #eeeeee (exact)
        assert_eq!(nearest_xterm256(0x05, 0x05, 0x05), 232); // ramp #080808: 81 vs 225 to black
    }

    /// Every §14 colour, with the expected index worked out by hand with
    /// the redmean distance `(2 + r̄/256)·ΔR² + 4·ΔG² + (2 + (255 − r̄)/256)·ΔB²`.
    /// The comment gives the chosen palette entry and the runner-up.
    #[test]
    fn nearest_xterm256_for_every_theme_colour() {
        let t = Theme::DARK;
        let cases = [
            ("bg", t.bg, 233),                           // #121212 (vs 232 #080808)
            ("bg_alt_row", t.bg_alt_row, 233),           // #121212 (vs 234)
            ("surface", t.surface, 234),                 // #1c1c1c (vs 233)
            ("surface_raised", t.surface_raised, 234),   // #1c1c1c (vs 235)
            ("status_bg", t.status_bg, 235),             // #262626 (vs 234)
            ("selection_bg", t.selection_bg, 235),       // #262626 (vs 236)
            ("dialog_title_bg", t.dialog_title_bg, 235), // #262626 (vs 236)
            ("track", t.track, 235),                     // #262626 (vs 236)
            ("border", t.border, 236),                   // #303030 (vs 235)
            ("border_inner", t.border_inner, 237),       // #3a3a3a (vs 236)
            ("border_strong", t.border_strong, 238),     // #444444 (vs 239)
            ("fg", t.fg, 253),                           // #dadada (vs 188 #d7d7d7)
            ("fg_muted", t.fg_muted, 249),               // #b2b2b2 (vs 145 #afafaf)
            ("fg_dim", t.fg_dim, 246),                   // #949494 (vs 103 #8787af)
            ("amber", t.amber, 215),                     // cube 5,3,1 #ffaf5f (vs 179)
            ("amber_bright", t.amber_bright, 222),       // cube 5,4,2 #ffd787 (vs 223)
            ("amber_tint", t.amber_tint, 235),           // #262626 (vs 236)
            ("teal", t.teal, 74),                        // cube 1,3,4 #5fafd7 (vs 80)
            ("purple", t.purple, 183),                   // cube 4,3,5 #d7afff (vs 147)
            ("purple_tint", t.purple_tint, 236),         // #303030 (vs 235)
            ("green", t.green, 150),                     // cube 3,4,2 #afd787 (vs 114)
            ("coral", t.coral, 209),                     // cube 5,2,1 #ff875f (vs 210)
        ];
        assert_eq!(cases.len(), 22);
        for (name, color, expected) in cases {
            let Color::Rgb(r, g, b) = color else {
                panic!("{name} is not RGB");
            };
            assert_eq!(nearest_xterm256(r, g, b), expected, "{name}");
        }
    }

    #[test]
    fn ansi256_maps_every_field_to_an_index() {
        let t = Theme::DARK.adapt(ColorSupport::Ansi256);
        assert!(!t.monochrome);
        assert_eq!(t.bg, Color::Indexed(233));
        assert_eq!(t.amber, Color::Indexed(215));
        let fields = [
            t.bg,
            t.bg_alt_row,
            t.surface,
            t.surface_raised,
            t.status_bg,
            t.selection_bg,
            t.dialog_title_bg,
            t.track,
            t.border,
            t.border_inner,
            t.border_strong,
            t.fg,
            t.fg_muted,
            t.fg_dim,
            t.amber,
            t.amber_bright,
            t.amber_tint,
            t.teal,
            t.purple,
            t.purple_tint,
            t.green,
            t.coral,
        ];
        for c in fields {
            assert!(matches!(c, Color::Indexed(16..=255)), "{c:?}");
        }
        // Striped rows collapse onto one index: accepted (see `Theme::adapt`).
        assert_eq!(t.bg_alt_row, t.bg);
    }

    #[test]
    fn ansi256_keeps_selection_and_status_visible() {
        let t = Theme::DARK.adapt(ColorSupport::Ansi256);
        assert_ne!(t.selection_bg, t.bg);
        assert_ne!(t.status_bg, t.bg);
        // The cursor stays amber, distinct from the row backgrounds.
        assert_eq!(t.cursor_cell().bg, Some(Color::Indexed(215)));
        assert_ne!(t.cursor_cell().bg, t.row_selected().bg);
        assert_ne!(t.cursor_cell().bg, t.row(0).bg);
    }

    #[test]
    fn collapsed_pairs_are_nudged_to_the_next_step() {
        // #121212 and #141414 both map to 233.
        let t = Theme {
            bg: Color::Rgb(0x12, 0x12, 0x12),
            selection_bg: Color::Rgb(0x14, 0x14, 0x14),
            status_bg: Color::Rgb(0x13, 0x13, 0x13),
            ..Theme::DARK
        };
        assert_eq!(nearest_xterm256(0x14, 0x14, 0x14), 233);
        assert_eq!(nearest_xterm256(0x13, 0x13, 0x13), 233);
        let a = t.adapt(ColorSupport::Ansi256);
        assert_eq!(a.bg, Color::Indexed(233));
        assert_eq!(a.selection_bg, Color::Indexed(234));
        assert_eq!(a.status_bg, Color::Indexed(234));

        // A darker partner moves down instead.
        let t = Theme {
            bg: Color::Rgb(0x14, 0x14, 0x14),
            selection_bg: Color::Rgb(0x12, 0x12, 0x12),
            ..Theme::DARK
        };
        let a = t.adapt(ColorSupport::Ansi256);
        assert_eq!(a.selection_bg, Color::Indexed(232));

        // At the end of the palette it steps back.
        let t = Theme {
            bg: Color::Rgb(0xEE, 0xEE, 0xEE),
            status_bg: Color::Rgb(0xEF, 0xEF, 0xEF),
            ..Theme::DARK
        };
        let a = t.adapt(ColorSupport::Ansi256);
        assert_eq!(a.bg, Color::Indexed(255));
        assert_eq!(a.status_bg, Color::Indexed(254));
    }

    // ---- NO_COLOR ------------------------------------------------------

    fn mods(style: Style) -> Modifier {
        assert_eq!(style.fg, None, "{style:?}");
        assert_eq!(style.bg, None, "{style:?}");
        assert_eq!(style.underline_color, None, "{style:?}");
        assert_eq!(style.sub_modifier, Modifier::empty(), "{style:?}");
        style.add_modifier
    }

    /// The M7-01 table, helper by helper.
    #[test]
    fn no_color_uses_modifiers_only() {
        let t = Theme::DARK.adapt(ColorSupport::NoColor);
        assert!(t.monochrome);
        let none = Modifier::empty();
        let b = Modifier::BOLD;
        let u = Modifier::UNDERLINED;
        let r = Modifier::REVERSED;
        let d = Modifier::DIM;
        let cases = [
            ("base", t.base(), none),
            ("surface", t.surface(), none),
            ("row(0)", t.row(0), none),
            ("row(1)", t.row(1), none),
            ("dialog", t.dialog(), none),
            ("status_line", t.status_line(), none),
            ("cursor_cell", t.cursor_cell(), r | b),
            ("row_selected", t.row_selected(), b),
            ("mode_pill", t.mode_pill(Mode::Normal), r | b),
            ("mode_pill(Filter)", t.mode_pill(Mode::Filter), r | b),
            ("tab_active", t.tab_active(), u | b),
            ("tab", t.tab(), d),
            ("match_highlight", t.match_highlight(), u),
            ("header", t.header(), b),
            ("header_active", t.header_active(), b | u),
            ("query_error", t.query_error(), b | u),
            ("inline_error", t.inline_error(), b),
            ("toast(Error)", t.toast(ToastLevel::Error), b),
            ("gutter_ragged", t.gutter_ragged(), b),
            ("status_warning", t.status_warning(), b),
            ("hint_key", t.hint_key(), b),
            ("app_name", t.app_name(), b),
            ("status_file", t.status_file(), b),
            ("dim", t.dim(), d),
            ("hint", t.hint(), d),
            ("gutter", t.gutter(), d),
            ("header_type", t.header_type(), d),
            ("control_char", t.control_char(), d),
            ("backdrop", t.backdrop(), d),
            ("palette_item_selected", t.palette_item_selected(), r),
            ("gauge", t.gauge(t.teal), none),
            ("bar_fill", t.bar_fill(), none),
            ("bar_track", t.bar_track(), none),
            ("dialog_border", t.dialog_border(t.teal), none),
            ("palette_border", t.palette_border(), none),
            ("frozen_divider", t.frozen_divider(), b),
        ];
        for (name, style, expected) in cases {
            assert_eq!(mods(style), expected, "{name}");
        }
        // Every colour field is `Reset`, so even a direct field use draws no colour.
        assert_eq!(t.bg, Color::Reset);
        assert_eq!(t.coral, Color::Reset);
    }

    /// The things the user must still tell apart without colour.
    #[test]
    fn no_color_keeps_cursor_selection_pill_and_matches_distinct() {
        let t = Theme::DARK.adapt(ColorSupport::NoColor);
        let styles = [
            t.row(0),
            t.cursor_cell(),
            t.row_selected(),
            t.match_highlight(),
        ];
        for (i, a) in styles.iter().enumerate() {
            for b in &styles[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_ne!(t.mode_pill(Mode::Normal), t.status_line());
    }
}
