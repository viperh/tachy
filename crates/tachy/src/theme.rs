//! Tachy colour theme.
//!
//! All colours are 24-bit (`Color::Rgb`). Terminals without truecolor
//! support will approximate them.

use ratatui::style::{Color, Modifier, Style};

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
}

impl Theme {
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
    };

    // ---- base ----------------------------------------------------------

    pub fn base(&self) -> Style {
        Style::new().fg(self.fg).bg(self.bg)
    }

    pub fn surface(&self) -> Style {
        Style::new().fg(self.fg).bg(self.surface)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.fg_muted)
    }

    pub fn dim(&self) -> Style {
        Style::new().fg(self.fg_dim)
    }

    pub fn border(&self) -> Style {
        Style::new().fg(self.border)
    }

    // ---- top bar -------------------------------------------------------

    pub fn app_name(&self) -> Style {
        Style::new().fg(self.amber).add_modifier(Modifier::BOLD)
    }

    pub fn tab(&self) -> Style {
        Style::new().fg(self.fg_dim).bg(self.surface)
    }

    pub fn tab_active(&self) -> Style {
        Style::new()
            .fg(self.fg)
            .bg(self.bg)
            .underline_color(self.amber)
            .add_modifier(Modifier::UNDERLINED)
    }

    /// The coloured pill in the top-right corner.
    pub fn mode_pill(&self, mode: Mode) -> Style {
        let bg = match mode {
            Mode::Normal => self.teal,
            Mode::Filter => self.amber,
            Mode::Command => self.purple,
        };
        Style::new().fg(self.bg).bg(bg).add_modifier(Modifier::BOLD)
    }

    // ---- table ---------------------------------------------------------

    pub fn header(&self) -> Style {
        Style::new()
            .fg(self.fg)
            .bg(self.surface)
            .add_modifier(Modifier::BOLD)
    }

    pub fn header_type(&self) -> Style {
        Style::new().fg(self.fg_dim).bg(self.surface)
    }

    /// Header of the column the cursor is in.
    pub fn header_active(&self) -> Style {
        self.header().fg(self.amber)
    }

    pub fn gutter(&self) -> Style {
        Style::new().fg(self.fg_dim)
    }

    pub fn gutter_selected(&self) -> Style {
        Style::new().fg(self.amber)
    }

    /// Striped rows: pass the row's index in the view.
    pub fn row(&self, index: usize) -> Style {
        let bg = if index % 2 == 1 {
            self.bg_alt_row
        } else {
            self.bg
        };
        Style::new().fg(self.fg).bg(bg)
    }

    pub fn row_selected(&self) -> Style {
        Style::new().fg(self.fg).bg(self.selection_bg)
    }

    pub fn cursor_cell(&self) -> Style {
        Style::new()
            .fg(self.bg)
            .bg(self.amber)
            .add_modifier(Modifier::BOLD)
    }

    /// A cell (or substring) matching the active filter or search.
    pub fn match_highlight(&self) -> Style {
        Style::new().fg(self.amber).bg(self.amber_tint)
    }

    pub fn value_error(&self) -> Style {
        Style::new().fg(self.coral)
    }

    pub fn value_ok(&self) -> Style {
        Style::new().fg(self.green)
    }

    // ---- status + hints -----------------------------------------------

    pub fn status_line(&self) -> Style {
        Style::new().fg(self.fg).bg(self.status_bg)
    }

    pub fn status_file(&self) -> Style {
        Style::new()
            .fg(self.teal)
            .bg(self.status_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn hint(&self) -> Style {
        Style::new().fg(self.fg_dim)
    }

    pub fn hint_key(&self) -> Style {
        Style::new().fg(self.amber).add_modifier(Modifier::BOLD)
    }

    pub fn gauge(&self, fill: Color) -> Style {
        Style::new().fg(fill).bg(self.track)
    }

    pub fn query_operator(&self) -> Style {
        Style::new().fg(self.teal)
    }

    pub fn query_string(&self) -> Style {
        Style::new().fg(self.green)
    }

    pub fn query_number(&self) -> Style {
        Style::new().fg(self.purple)
    }

    pub fn dialog(&self) -> Style {
        Style::new().fg(self.fg).bg(self.surface_raised)
    }

    pub fn dialog_border(&self, accent: Color) -> Style {
        Style::new().fg(accent).bg(self.surface_raised)
    }

    pub fn dialog_title(&self, accent: Color) -> Style {
        Style::new()
            .fg(accent)
            .bg(self.dialog_title_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn button(&self) -> Style {
        Style::new().fg(self.fg).bg(self.surface_raised)
    }

    pub fn button_primary(&self) -> Style {
        Style::new()
            .fg(self.bg)
            .bg(self.teal)
            .add_modifier(Modifier::BOLD)
    }

    pub fn palette_item_selected(&self) -> Style {
        Style::new().fg(self.purple).bg(self.purple_tint)
    }

    pub fn backdrop(&self) -> Style {
        Style::new().fg(self.fg_dim).add_modifier(Modifier::DIM)
    }

    pub fn job(&self, kind: JobKind) -> Style {
        let fg = match kind {
            JobKind::Sort => self.purple,
            JobKind::Export => self.amber,
            JobKind::Index => self.teal,
            JobKind::Done => self.green,
        };
        Style::new().fg(fg)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::DARK
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Sort,
    Export,
    Index,
    Done,
}
