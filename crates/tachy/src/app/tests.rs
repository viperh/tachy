use std::{io::Write, path::Path};

use clap::Parser;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};
use ratatui::{Terminal, backend::TestBackend, style::Modifier};
use tachy_core::{column::ColumnMeta, index::RowIndex, types::ColType};
use tempfile::NamedTempFile;

use super::*;
use crate::{
    cli::Cli,
    components::table::NO_FILE,
    state::{DialogKind, Overlay},
    tab::tests::{complete_index, grid, loaded_tab},
    theme::Theme,
    toast::{Toast as ToastMsg, ToastLevel},
};

// ---- helpers --------------------------------------------------------------

fn new_app() -> App {
    app_with(Config::embedded(), ColorSupport::TrueColor)
}

fn app_with(config: Config, color: ColorSupport) -> App {
    app_files(config, color, &["x.csv"])
}

fn app_files(config: Config, color: ColorSupport, args: &[&str]) -> App {
    let args = std::iter::once("tachy").chain(args.iter().copied());
    let cli = Cli::try_parse_from(args).unwrap();
    let settings = Settings::resolve(&cli, &config).unwrap();
    let mut app = App::new(config.clone(), settings, color).unwrap();
    for component in app.ui.all_mut() {
        component.register_config_handler(config.clone()).unwrap();
    }
    app
}

/// Adds a loaded tab over `content` (index complete) and activates it.
fn with_tab(app: &mut App, content: &str) -> NamedTempFile {
    let (f, tab) = loaded_tab(content, app.state.settings.freeze);
    app.state.tabs.push(tab);
    app.state.active_tab = app.state.tabs.len() - 1;
    f
}

fn tab(app: &App) -> &Tab {
    app.state.active_tab().unwrap()
}

fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buf = terminal.backend().buffer();
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// Feeds one 100 ms tick through the loop, as the event task would.
fn tick<B: Backend>(app: &mut App, tui: &mut Tui<B>) {
    app.handle_event(Event::Tick).unwrap();
    app.handle_actions(tui).unwrap();
}

fn press<B: Backend>(app: &mut App, tui: &mut Tui<B>, code: KeyCode, mods: KeyModifiers) {
    app.handle_event(Event::Key(KeyEvent::new(code, mods)))
        .unwrap();
    app.handle_actions(tui).unwrap();
}

fn ch<B: Backend>(app: &mut App, tui: &mut Tui<B>, c: char) {
    press(app, tui, KeyCode::Char(c), KeyModifiers::NONE);
}

fn type_text<B: Backend>(app: &mut App, tui: &mut Tui<B>, s: &str) {
    for c in s.chars() {
        ch(app, tui, c);
    }
}

/// Every distinct style on screen, row by row, as `x0-x1 fg/bg/modifiers`
/// runs: snapshots then show colours, not only text.
fn style_dump(terminal: &Terminal<TestBackend>) -> String {
    use std::fmt::Write;
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut runs: Vec<(u16, u16, String)> = Vec::new();
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            let key = format!("{:?}/{:?}/{:?}", c.fg, c.bg, c.modifier);
            match runs.last_mut() {
                Some(run) if run.2 == key => run.1 = x,
                _ => runs.push((x, x, key)),
            }
        }
        let row: Vec<String> = runs
            .iter()
            .map(|(a, b, k)| format!("{a}-{b} {k}"))
            .collect();
        writeln!(out, "{y:2}: {}", row.join(" | ")).unwrap();
    }
    out
}

fn draw(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw_frame(frame)).unwrap();
    terminal
}

fn tui(width: u16, height: u16) -> Tui<TestBackend> {
    Tui::with_backend(TestBackend::new(width, height)).unwrap()
}

/// Runs the loop until every tab is opened and indexed (or removed).
async fn settle(app: &mut App, tui: &mut Tui<TestBackend>) {
    let done = |app: &App| {
        app.state.tabs.iter().all(|t| {
            t.loaded
                .as_ref()
                .is_some_and(|l| l.index_summary.is_some() || l.index_error.is_some())
        })
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done(app) {
            app.step(tui).await.unwrap();
        }
    })
    .await
    .expect("tabs did not settle");
}

fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    path
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

impl App {
    /// The viewport at a given terminal size, for tests.
    fn viewport_for(&mut self, w: u16, h: u16) -> Viewport {
        self.area = Rect::new(0, 0, w, h);
        self.viewport()
    }
}

// ---- event loop (M0-02) ---------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_burst_of_messages_renders_once() {
    let mut app = new_app();
    let mut tui = tui(120, 40);
    for i in 0..100 {
        app.msg_tx
            .send(Msg::Toast(ToastMsg::new(ToastLevel::Info, format!("{i}"))))
            .unwrap();
    }
    app.step(&mut tui).await.unwrap();
    assert_eq!(app.state.toasts.len(), 100);
    assert_eq!(app.renders, 1);
    assert!(!app.dirty);

    // Nothing else happens: the loop stays blocked and draws nothing.
    let idle = tokio::time::timeout(Duration::from_secs(1), app.step(&mut tui)).await;
    assert!(idle.is_err());
    assert_eq!(app.renders, 1);
}

#[tokio::test(start_paused = true)]
async fn ticking_follows_running_work() {
    let mut app = new_app();
    let mut tui = tui(120, 40);
    app.step(&mut tui).await.unwrap();
    assert!(!tui.is_ticking());
}

// ---- layout (M0-04) and the no-tab state (D4) -----------------------------

#[test]
fn no_tab_layout_120x40() {
    let mut app = new_app();
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!(terminal.backend());
    let text = terminal.backend().to_string();
    assert!(text.contains(NO_FILE), "{text}");

    // Backgrounds come from the theme.
    let theme = Theme::DARK;
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(0, 0)].bg, theme.surface); // top bar
    assert_eq!(buf[(0, 1)].bg, theme.surface); // table header
    assert_eq!(buf[(0, 3)].bg, theme.bg); // table body
    assert_eq!(buf[(79, 5)].symbol(), "│"); // inspector divider
    assert_eq!(buf[(80, 5)].bg, theme.surface); // inspector
    assert_eq!(buf[(0, 38)].bg, theme.status_bg); // status line
    assert_eq!(buf[(0, 39)].bg, theme.bg); // hints
}

#[test]
fn no_tab_layout_80x24() {
    let mut app = new_app();
    insta::assert_snapshot!(draw(&mut app, 80, 24).backend());
}

#[test]
fn too_small_79x24() {
    let mut app = new_app();
    let terminal = draw(&mut app, 79, 24);
    insta::assert_snapshot!(terminal.backend());
    assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Theme::DARK.bg);
}

#[test]
fn too_small_80x23() {
    let mut app = new_app();
    let terminal = draw(&mut app, 80, 23);
    let text = terminal.backend().to_string();
    assert!(text.contains("terminal too small"), "{text}");
    assert!(text.contains("80×23 — need 80×24"), "{text}");
    assert!(!text.contains("no file open"), "{text}");
}

#[test]
fn hidden_hints_and_toast_overlay() {
    let mut app = new_app();
    app.state.hints_visible = false;
    app.state
        .toasts
        .push(ToastMsg::new(ToastLevel::Error, "cannot open x.csv"), now());
    let terminal = draw(&mut app, 120, 40);
    let buf = terminal.backend().buffer();
    // Status line moves to the last row; the toast sits right above it.
    assert_eq!(buf[(0, 39)].bg, Theme::DARK.status_bg);
    assert_eq!(buf[(1, 38)].symbol(), "c");
    assert_eq!(buf[(1, 38)].fg, Theme::DARK.coral);
}

#[test]
fn modal_dims_the_base_regions() {
    let mut app = new_app();
    app.state.mode = Mode::Dialog;
    app.state.overlay = Some(Overlay::Dialog(DialogKind::Goto));
    let terminal = draw(&mut app, 120, 40);
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(0, 0)].fg, Theme::DARK.fg_dim);
    assert!(buf[(0, 0)].modifier.contains(Modifier::DIM));
    // The dialog itself is not dimmed.
    let center = buf[(60, 20)].clone();
    assert_eq!(center.bg, Theme::DARK.surface_raised);
    assert!(!center.modifier.contains(Modifier::DIM));
}

// ---- toasts (M1-09) -------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_toast_disappears_after_five_seconds_without_input() {
    let mut app = new_app();
    let mut tui = tui(80, 24);
    app.msg_tx
        .send(Msg::Toast(ToastMsg::new(
            ToastLevel::Error,
            "cannot open missing.csv: file not found",
        )))
        .unwrap();
    app.step(&mut tui).await.unwrap();
    assert!(tui.is_ticking(), "a visible toast needs the tick");
    assert_eq!(app.state.toasts.len(), 1);

    tokio::time::advance(Duration::from_millis(4900)).await;
    tick(&mut app, &mut tui);
    assert_eq!(app.state.toasts.len(), 1);

    tokio::time::advance(Duration::from_millis(100)).await;
    tick(&mut app, &mut tui);
    assert!(app.state.toasts.is_empty());
    assert!(!app.state.has_running_work());
    assert!(app.dirty, "the toast row must be redrawn");
}

#[tokio::test(start_paused = true)]
async fn two_errors_are_shown_one_after_the_other() {
    let mut app = new_app();
    let mut tui = tui(80, 24);
    for text in [
        "cannot open a.csv: file not found",
        "cannot open b.csv: is a directory",
    ] {
        app.msg_tx
            .send(Msg::Toast(ToastMsg::new(ToastLevel::Error, text)))
            .unwrap();
    }
    app.step(&mut tui).await.unwrap();
    // The toast row overlays the last body row: row 21 at 80×24 with hints.
    let first = row_text(&draw(&mut app, 80, 24), 21);
    assert!(first.starts_with(" cannot open a.csv"), "{first}");
    assert!(first.ends_with("(+1) "), "{first}");

    tokio::time::advance(Duration::from_secs(5)).await;
    tick(&mut app, &mut tui);
    let second = row_text(&draw(&mut app, 80, 24), 21);
    assert!(second.starts_with(" cannot open b.csv"), "{second}");
    assert!(!second.contains("(+"), "{second}");

    // The second one gets its own full 5 s.
    tokio::time::advance(Duration::from_millis(4900)).await;
    tick(&mut app, &mut tui);
    assert_eq!(app.state.toasts.len(), 1);
    tokio::time::advance(Duration::from_millis(100)).await;
    tick(&mut app, &mut tui);
    assert!(app.state.toasts.is_empty());
}

#[tokio::test(start_paused = true)]
async fn esc_dismisses_the_current_toast() {
    let mut app = new_app();
    let mut tui = tui(80, 24);
    let t = now();
    app.state
        .toasts
        .push(ToastMsg::new(ToastLevel::Error, "a"), t);
    app.state
        .toasts
        .push(ToastMsg::new(ToastLevel::Error, "b"), t);

    // Other keys don't dismiss toasts.
    ch(&mut app, &mut tui, 'x');
    assert_eq!(app.state.toasts.len(), 2);

    press(&mut app, &mut tui, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.state.toasts.front().unwrap().text, "b");
    press(&mut app, &mut tui, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.state.toasts.is_empty());
}

#[test]
fn error_toast_80x24() {
    let mut app = new_app();
    let t = now();
    app.state.toasts.push(
        ToastMsg::new(ToastLevel::Error, "cannot open missing.csv: file not found"),
        t,
    );
    app.state.toasts.push(
        ToastMsg::new(ToastLevel::Error, "cannot open /etc: is a directory"),
        t,
    );
    let terminal = draw(&mut app, 80, 24);
    insta::assert_snapshot!(terminal.backend());
    let buf = terminal.backend().buffer();
    // Over the last body row, right above the status line.
    assert_eq!(buf[(1, 21)].fg, Theme::DARK.coral);
    assert_eq!(buf[(0, 21)].bg, Theme::DARK.bg);
    assert_eq!(buf[(75, 21)].symbol(), "(");
    assert_eq!(buf[(75, 21)].fg, Theme::DARK.fg_dim);
    assert_eq!(buf[(0, 22)].bg, Theme::DARK.status_bg);
}

#[test]
fn info_toast_80x24() {
    let mut app = new_app();
    app.state.toasts.push(
        ToastMsg::new(
            ToastLevel::Info,
            "exported 1,234 rows to /home/someone/a/very/long/path/that/does/not/fit/on/one/line/export.csv",
        ),
        now(),
    );
    let terminal = draw(&mut app, 80, 24);
    insta::assert_snapshot!(terminal.backend());
    let row = row_text(&terminal, 21);
    assert!(row.ends_with("… "), "{row}");
    assert_eq!(terminal.backend().buffer()[(1, 21)].fg, Theme::DARK.fg);
}

// ---- colour support (M7-01) -----------------------------------------------

/// The main screen with a table, a toast and the palette closed, in one
/// colour mode.
fn main_screen(color: ColorSupport) -> Terminal<TestBackend> {
    let mut app = app_with(Config::embedded(), color);
    let _f = with_tab(&mut app, &grid(40, 12));
    app.state
        .toasts
        .push(ToastMsg::new(ToastLevel::Error, "cannot open x.csv"), now());
    draw(&mut app, 120, 40)
}

#[test]
fn main_screen_truecolor_120x40() {
    let terminal = main_screen(ColorSupport::TrueColor);
    insta::assert_snapshot!(style_dump(&terminal));
}

#[test]
fn main_screen_ansi256_120x40() {
    let terminal = main_screen(ColorSupport::Ansi256);
    insta::assert_snapshot!(style_dump(&terminal));
    for cell in terminal.backend().buffer().content() {
        for color in [cell.fg, cell.bg] {
            assert!(!format!("{color:?}").starts_with("Rgb"), "{cell:?}");
        }
    }
}

#[test]
fn main_screen_no_color_120x40() {
    let terminal = main_screen(ColorSupport::NoColor);
    insta::assert_snapshot!(style_dump(&terminal));
    for cell in terminal.backend().buffer().content() {
        // `Color::default()` is `Reset`.
        assert_eq!(cell.fg, ratatui::style::Color::default(), "{cell:?}");
        assert_eq!(cell.bg, ratatui::style::Color::default(), "{cell:?}");
    }
    // The cursor cell is still distinguishable: reversed.
    let buf = terminal.backend().buffer();
    let cursor = (0..120)
        .map(|x| &buf[(x, 3)])
        .find(|c| c.modifier.contains(Modifier::REVERSED));
    assert!(cursor.is_some());
}

// ---- table rendering (M1-05) ----------------------------------------------

fn table_fixture() -> String {
    let mut s = String::from("id,name,city,amount,note\n");
    let cities = ["Berlin", "Lisbon", "Osaka", "Toronto", "Nairobi"];
    for i in 1..=30 {
        s.push_str(&format!(
            "{i},user{i:03},{},{}.{:02},{}\n",
            cities[i % cities.len()],
            i * 37 % 1000,
            i * 7 % 100,
            if i % 4 == 0 { "" } else { "ok" }
        ));
    }
    s
}

const WIDE: &str = "id,name,emoji\n\
    1,日本語,🦀\n\
    2,中文字符测试很长的一段文字超过列宽限制了吧真的很长很长很长很长,👨\u{200d}👩\u{200d}👧\n\
    3,ascii,ok\n\
    4,한국어 텍스트,🎉🎉\n";

const CONTROL: &str = "id,value,after\n\
    1,\"\x1b[31mred\x1b[0m\",x\n\
    2,\"tab\there\",y\n\
    3,\"line\nbreak\",z\n\
    4,\"bell\x07\",w\n";

const RAGGED: &str = "a,b,c\n1,2,3\n4,5\n6,7,8,9\n10,11,12\n";

/// Snapshots of one fixture at 120×40 and 80×24.
fn snapshot_both(name: &str, content: &str, setup: impl Fn(&mut App, u16, u16)) {
    for (w, h) in [(120, 40), (80, 24)] {
        let mut app = new_app();
        let _f = with_tab(&mut app, content);
        setup(&mut app, w, h);
        let terminal = draw(&mut app, w, h);
        insta::assert_snapshot!(format!("{name}_{w}x{h}"), terminal.backend());
    }
}

#[test]
fn table_snapshots() {
    snapshot_both("table_normal", &table_fixture(), |_, _, _| {});
    snapshot_both("table_wide_chars", WIDE, |_, _, _| {});
    snapshot_both("table_control_chars", CONTROL, |_, _, _| {});
    snapshot_both("table_ragged", RAGGED, |_, _, _| {});
    snapshot_both("table_empty_file", "", |_, _, _| {});
    snapshot_both("table_header_only", "a,b,c\n", |_, _, _| {});
    snapshot_both("table_scrolled_with_freeze", &grid(30, 25), |app, w, h| {
        // Measure the widths, then go to the last column.
        let viewport = app.viewport_for(w, h);
        let tab = app.state.active_tab_mut().unwrap();
        tab.prepare_frame(viewport);
        tab.last_col(viewport);
    });
}

#[test]
fn wide_characters_stay_aligned() {
    let mut app = new_app();
    let _f = with_tab(&mut app, WIDE);
    let terminal = draw(&mut app, 120, 40);
    let buf = terminal.backend().buffer();
    // The `emoji` column starts at the same cell on every row.
    let x = (0..120u16)
        .find(|&x| buf[(x, 1)].symbol() == "e" && buf[(x + 1, 1)].symbol() == "m")
        .unwrap();
    for y in 3..7 {
        // The cell right before it is the separator space.
        assert_eq!(buf[(x - 1, y)].symbol(), " ", "row {y}");
    }
    assert_eq!(buf[(x, 3)].symbol(), "🦀");
    assert_eq!(buf[(x, 5)].symbol(), "o");
    // The long CJK value is cut with `…` and never spills.
    let row = row_text(&terminal, 4);
    assert!(row.contains('…'), "{row:?}");
}

#[test]
fn numeric_columns_are_right_aligned() {
    let mut app = new_app();
    let _f = with_tab(&mut app, "n,label\n5,a\n12345,b\n");
    let set = |app: &mut App, ty| {
        let tab = app.state.active_tab_mut().unwrap();
        let l = tab.loaded.as_mut().unwrap();
        l.columns[0].type_override = Some(ty);
    };
    set(&mut app, ColType::I64);
    let terminal = draw(&mut app, 80, 24);
    let row = row_text(&terminal, 3);
    // Gutter `  1 │ `, then the 5-wide column with `5` at its right end.
    assert!(row.starts_with("  1 │     5 │ a"), "{row:?}");
    let types = row_text(&terminal, 2);
    assert!(types.starts_with("    │   i64 │ str"), "{types:?}");
    set(&mut app, ColType::Str);
    let row = row_text(&draw(&mut app, 80, 24), 3);
    assert!(row.starts_with("  1 │ 5     │ a"), "{row:?}");
}

#[test]
fn long_values_end_in_an_ellipsis() {
    let mut app = new_app();
    let long = "x".repeat(60);
    let _f = with_tab(&mut app, &format!("a,b\n{long},next\n"));
    let terminal = draw(&mut app, 80, 24);
    let row = row_text(&terminal, 3);
    let cut = format!("{}… │ next", "x".repeat(39));
    assert!(row.contains(&cut), "{row:?}");
}

#[test]
fn control_characters_render_escaped_and_dim() {
    let mut app = new_app();
    let _f = with_tab(&mut app, CONTROL);
    // Move the cursor off row 1, so its style is the plain row style.
    app.state.active_tab_mut().unwrap().cursor_row = 3;
    let terminal = draw(&mut app, 80, 24);
    let row = row_text(&terminal, 3);
    assert!(row.contains("\\x1b[31mred\\x1b[0m"), "{row:?}");
    let buf = terminal.backend().buffer();
    let x = (0..80u16).find(|&x| buf[(x, 3)].symbol() == "\\").unwrap();
    assert!(buf[(x, 3)].modifier.contains(Modifier::DIM));
    assert_eq!(buf[(x, 3)].fg, Theme::DARK.fg_dim);
    // `\x1b[31m` is 7 cells... `[31m` is plain text after the escape.
    let r = x + "\\x1b[31m".len() as u16;
    assert_eq!(buf[(r, 3)].symbol(), "r");
    assert!(!buf[(r, 3)].modifier.contains(Modifier::DIM));
}

#[test]
fn frozen_column_stays_while_scrolling_right() {
    let mut app = new_app();
    let _f = with_tab(&mut app, &grid(30, 25));
    let mut tui = tui(120, 40);
    draw(&mut app, 120, 40);
    ch(&mut app, &mut tui, '$');
    assert!(tab(&app).col_offset > 0);
    let terminal = draw(&mut app, 120, 40);
    let header = row_text(&terminal, 1);
    assert!(header.contains("c0 "), "{header:?}");
    assert!(header.contains("c24"), "{header:?}");
    let row = row_text(&terminal, 3);
    // `c0` is 5 wide (`r29c0`).
    assert!(row.starts_with("  1 │ r0c0  │ "), "{row:?}");
    // The frozen divider is `border_strong`, the gutter's `border`.
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(12, 3)].symbol(), "│");
    assert_eq!(buf[(12, 3)].fg, Theme::DARK.border_strong);
    assert_eq!(buf[(4, 3)].fg, Theme::DARK.border);
}

#[test]
fn more_cols_hint_counts_cut_off_visible_columns() {
    let mut app = new_app();
    let _f = with_tab(&mut app, &grid(5, 25));
    let terminal = draw(&mut app, 80, 24);
    let header = row_text(&terminal, 1);
    let h = tab(&app).h_layout(app.viewport());
    let n = h.more_cols;
    assert_eq!(n, 25 - 1 - h.scrolling.len());
    assert!(header.ends_with(&format!("→ {n} more cols")), "{header:?}");
    // Hidden columns don't count.
    app.state.active_tab_mut().unwrap().layout.visible[24] = false;
    let header = row_text(&draw(&mut app, 80, 24), 1);
    assert!(
        header.ends_with(&format!("→ {} more cols", n - 1)),
        "{header:?}"
    );
    // No hint when everything fits.
    let mut app = new_app();
    let _f = with_tab(&mut app, &grid(5, 3));
    let header = row_text(&draw(&mut app, 80, 24), 1);
    assert!(!header.contains("more cols"), "{header:?}");
}

#[test]
fn empty_and_header_only_placeholders() {
    let mut app = new_app();
    let _f = with_tab(&mut app, "");
    let text = draw(&mut app, 80, 24).backend().to_string();
    assert!(text.contains("empty file"), "{text}");
    let mut app = new_app();
    let _f = with_tab(&mut app, "a,b,c\n");
    let terminal = draw(&mut app, 80, 24);
    let text = terminal.backend().to_string();
    assert!(text.contains("0 rows"), "{text}");
    assert!(row_text(&terminal, 1).contains("a   │ b   c"), "{text}");
}

#[test]
fn ragged_rows_have_a_coral_gutter_and_extra_columns() {
    let mut app = new_app();
    let _f = with_tab(&mut app, RAGGED);
    app.state.active_tab_mut().unwrap().cursor_row = 3;
    let terminal = draw(&mut app, 80, 24);
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(2, 4)].symbol(), "2");
    assert_eq!(buf[(2, 4)].fg, Theme::DARK.coral);
    assert_eq!(buf[(2, 3)].fg, Theme::DARK.fg_dim);
    assert!(row_text(&terminal, 1).contains("_extra1"));
    let status = row_text(&terminal, 22);
    assert!(status.contains("2 ragged"), "{status:?}");
}

#[test]
fn columns_with_forced_types_use_their_label() {
    let mut app = new_app();
    let _f = with_tab(&mut app, "a,b\n1,2\n");
    {
        let l = app.state.active_tab_mut().unwrap().loaded.as_mut().unwrap();
        let meta: &mut ColumnMeta = &mut l.columns[1];
        meta.type_override = Some(ColType::F64);
    }
    let terminal = draw(&mut app, 80, 24);
    let row = row_text(&terminal, 2);
    // Columns are at least as wide as their type label.
    assert!(row.starts_with("    │ str │ f64"), "{row:?}");
}

// ---- navigation and key routing (M1-06) -----------------------------------

fn nav_app() -> (App, NamedTempFile, Tui<TestBackend>) {
    let mut app = new_app();
    let f = with_tab(&mut app, &grid(100, 20));
    draw(&mut app, 120, 40);
    (app, f, tui(120, 40))
}

fn pos(app: &App) -> (u64, usize, u64, usize) {
    let t = tab(app);
    (t.cursor_row, t.cursor_col, t.top_row, t.col_offset)
}

#[test]
fn movement_keys_clamp_at_the_boundaries() {
    let (mut app, _f, mut tui) = nav_app();
    // Body: 40 − top bar − header (2) − status − hints = 35 rows.
    assert_eq!(app.viewport().body_height, 35);
    ch(&mut app, &mut tui, 'k');
    ch(&mut app, &mut tui, 'h');
    assert_eq!(pos(&app), (0, 0, 0, 0));
    ch(&mut app, &mut tui, 'j');
    ch(&mut app, &mut tui, 'l');
    assert_eq!(pos(&app), (1, 1, 0, 0));
    press(&mut app, &mut tui, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, &mut tui, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(pos(&app), (2, 2, 0, 0));
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    );
    assert_eq!((pos(&app).0, pos(&app).2), (19, 17));
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    );
    assert_eq!((pos(&app).0, pos(&app).2), (2, 0));
    press(&mut app, &mut tui, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!((pos(&app).0, pos(&app).2), (37, 35));
    press(&mut app, &mut tui, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!((pos(&app).0, pos(&app).2), (2, 0));
    // `G` with SHIFT reported, `G` without.
    press(&mut app, &mut tui, KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!((pos(&app).0, pos(&app).2), (99, 65));
    ch(&mut app, &mut tui, 'j');
    assert_eq!(pos(&app).0, 99);
    press(&mut app, &mut tui, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!((pos(&app).0, pos(&app).2), (99, 65));
    press(&mut app, &mut tui, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!((pos(&app).0, pos(&app).2), (0, 0));
    ch(&mut app, &mut tui, 'G');
    assert_eq!(pos(&app).0, 99);
    // `$` with and without SHIFT.
    press(&mut app, &mut tui, KeyCode::Char('$'), KeyModifiers::SHIFT);
    assert_eq!(pos(&app).1, 19);
    ch(&mut app, &mut tui, 'l');
    assert_eq!(pos(&app).1, 19);
    // `0` lands on the frozen column without scrolling back.
    ch(&mut app, &mut tui, '0');
    assert_eq!(pos(&app).1, 0);
    assert!(pos(&app).3 > 0);
    ch(&mut app, &mut tui, '$');
    assert_eq!(pos(&app).1, 19);
    // The last column is fully visible.
    let h = tab(&app).h_layout(app.viewport());
    assert!(
        h.scrolling
            .iter()
            .any(|s| s.display_idx == 19 && !s.truncated)
    );
}

#[test]
fn w_and_b_skip_the_frozen_block() {
    let (mut app, _f, mut tui) = nav_app();
    ch(&mut app, &mut tui, '$');
    ch(&mut app, &mut tui, '0');
    let offset = pos(&app).3;
    assert!(offset > 0);
    ch(&mut app, &mut tui, 'w');
    assert_eq!(pos(&app).1, 1 + offset);
    for _ in 0..40 {
        ch(&mut app, &mut tui, 'b');
    }
    assert_eq!((pos(&app).1, pos(&app).3), (1, 0));
}

#[test]
fn resizing_keeps_the_cursor_visible() {
    let (mut app, _f, mut tui) = nav_app();
    for _ in 0..30 {
        ch(&mut app, &mut tui, 'j');
    }
    for _ in 0..12 {
        ch(&mut app, &mut tui, 'l');
    }
    assert_eq!(pos(&app).2, 0);
    app.action_tx.send(Action::Resize(80, 24)).unwrap();
    app.handle_actions(&mut tui).unwrap();
    let vp = app.viewport();
    let (row, col, top, _) = pos(&app);
    assert!(row >= top && row < top + u64::from(vp.body_height));
    let h = tab(&app).h_layout(vp);
    assert!(
        h.scrolling
            .iter()
            .any(|s| s.display_idx == col && !s.truncated)
    );
}

#[test]
fn mouse_wheel_scrolls_the_table() {
    let (mut app, _f, _tui) = nav_app();
    let wheel = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        })
    };
    app.handle_event(wheel(MouseEventKind::ScrollDown)).unwrap();
    // The cursor (row 0) left the screen: it is dragged along.
    assert_eq!((pos(&app).0, pos(&app).2), (3, 3));
    app.handle_event(wheel(MouseEventKind::ScrollDown)).unwrap();
    assert_eq!((pos(&app).0, pos(&app).2), (6, 6));
    // Scrolling up keeps the cursor while it is on screen.
    app.handle_event(wheel(MouseEventKind::ScrollUp)).unwrap();
    assert_eq!((pos(&app).0, pos(&app).2), (6, 3));
}

#[test]
fn letters_are_text_in_text_contexts() {
    let (mut app, _f, mut tui) = nav_app();
    app.state.mode = Mode::Filter;
    ch(&mut app, &mut tui, 'j');
    assert_eq!(pos(&app).0, 0, "j must not move in Filter mode");
    app.state.mode = Mode::Normal;
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    );
    type_text(&mut app, &mut tui, "jkq");
    assert_eq!(app.state.prompt.as_ref().unwrap().input.text(), "jkq");
    assert_eq!(pos(&app).0, 0);
    assert!(!app.should_quit);
}

#[test]
fn unbound_keys_are_ignored() {
    let (mut app, _f, mut tui) = nav_app();
    for code in [
        KeyCode::Char('Z'),
        KeyCode::F(12),
        KeyCode::Insert,
        KeyCode::Char('é'),
        KeyCode::Null,
    ] {
        press(&mut app, &mut tui, code, KeyModifiers::NONE);
        press(&mut app, &mut tui, code, KeyModifiers::ALT);
    }
    assert_eq!(pos(&app), (0, 0, 0, 0));
    assert!(!app.should_quit);
}

#[test]
fn spec_keys_reach_their_actions_with_or_without_shift() {
    let app = new_app();
    let keymap = &app.config.keybindings;
    let resolve = |c: char, mods| {
        keymap
            .resolve(
                KeyContext::Normal,
                KeyChord::from(KeyEvent::new(KeyCode::Char(c), mods)),
            )
            .cloned()
    };
    for mods in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
        assert_eq!(resolve('G', mods), Some(Action::LastRow));
        assert_eq!(resolve('N', mods), Some(Action::SearchPrev));
        assert_eq!(resolve('$', mods), Some(Action::LastCol));
        assert_eq!(resolve('?', mods), Some(Action::Help));
        assert_eq!(resolve('<', mods), Some(Action::ShrinkCol));
        assert_eq!(resolve('>', mods), Some(Action::GrowCol));
    }
}

// ---- top bar, status line, hints (M1-07) ----------------------------------

/// Top bar, status line and hint line.
fn chrome(terminal: &Terminal<TestBackend>) -> String {
    let h = terminal.backend().buffer().area.height;
    [0, h - 2, h - 1]
        .into_iter()
        .map(|y| row_text(terminal, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tab whose index is half done, with 2 s of throughput samples.
fn indexing_app() -> (App, NamedTempFile) {
    let mut app = new_app();
    let (f, mut tab) = loaded_tab(&table_fixture(), 1);
    let l = tab.loaded.as_mut().unwrap();
    let index = Arc::new(RowIndex::for_source(&l.source));
    let total = l.source.len() - l.source.data_start();
    index.set_bytes_scanned(total / 2);
    index.set_indexed_rows(12);
    let t0 = std::time::Instant::now();
    l.rate.push(t0, 0);
    l.rate.push(t0 + Duration::from_secs(2), total / 2);
    l.index = index;
    app.state.tabs.push(tab);
    (app, f)
}

#[test]
fn chrome_snapshots() {
    for (w, h) in [(120, 40), (80, 24)] {
        let (mut app, _f) = indexing_app();
        let terminal = draw(&mut app, w, h);
        insta::assert_snapshot!(format!("chrome_indexing_{w}x{h}"), chrome(&terminal));
        let text = chrome(&terminal);
        assert!(text.contains("rows (counting…)"), "{text}");
        assert!(text.contains("? help"), "{text}");

        let mut app = new_app();
        let _f = with_tab(&mut app, &table_fixture());
        let terminal = draw(&mut app, w, h);
        insta::assert_snapshot!(format!("chrome_ready_{w}x{h}"), chrome(&terminal));
        let text = chrome(&terminal);
        assert!(text.contains("30 rows"), "{text}");
        assert!(text.contains("? help"), "{text}");
    }
}

#[test]
fn indexing_status_shows_the_gauge() {
    let (mut app, _f) = indexing_app();
    let status = row_text(&draw(&mut app, 120, 40), 38);
    assert!(status.contains("indexing ███░░░░░ 49% · "), "{status:?}");
    assert!(status.contains("B/s · ~2 s left"), "{status:?}");
    // The cache already parsed the whole first screen: a lower bound.
    assert!(status.contains("≥ 30 rows (counting…)"), "{status:?}");
    assert!(status.contains("R 1 / ≥ 30  C 1/5"), "{status:?}");
}

#[test]
fn row_count_becomes_exact_when_indexing_finishes() {
    let (mut app, _f) = indexing_app();
    let status = row_text(&draw(&mut app, 120, 40), 38);
    assert!(status.contains("≥ 30 rows (counting…)"), "{status:?}");
    complete_index(tab(&app));
    let status = row_text(&draw(&mut app, 120, 40), 38);
    assert!(status.contains(" 30 rows · "), "{status:?}");
    assert!(!status.contains('≥'), "{status:?}");
    assert!(status.contains("mmap · index ready"), "{status:?}");
}

#[test]
fn position_is_one_based_and_follows_the_cursor() {
    let (mut app, _f, mut tui) = nav_app();
    let status = |app: &mut App| row_text(&draw(app, 120, 40), 38);
    let line = status(&mut app);
    assert!(line.contains("R 1 / 100  C 1/20  0%"), "{line:?}");
    ch(&mut app, &mut tui, 'j');
    ch(&mut app, &mut tui, 'l');
    let line = status(&mut app);
    assert!(line.contains("R 2 / 100  C 2/20  1%"), "{line:?}");
    ch(&mut app, &mut tui, 'G');
    let line = status(&mut app);
    assert!(line.contains("R 100 / 100  C 2/20  100%"), "{line:?}");
}

#[test]
fn narrow_status_drops_items_in_order() {
    let mut app = new_app();
    let _f = with_tab(&mut app, &table_fixture());
    let full = row_text(&draw(&mut app, 120, 40), 38);
    assert!(full.contains("delim ,"), "{full:?}");
    let narrow = row_text(&draw(&mut app, 80, 24), 22);
    assert!(!narrow.contains("delim ,"), "{narrow:?}");
    for kept in ["t.csv", "30 rows", "R 1 / 30"] {
        assert!(narrow.contains(kept), "{narrow:?}");
    }
}

#[test]
fn help_is_always_pinned_right() {
    for (w, h) in [(80, 24), (120, 40)] {
        let mut app = new_app();
        let _f = with_tab(&mut app, "a\n1\n");
        let terminal = draw(&mut app, w, h);
        let hints = row_text(&terminal, h - 1);
        assert!(hints.ends_with("? help "), "{hints:?}");
        app.state.prompt = Some(OpenPrompt::default());
        let hints = row_text(&draw(&mut app, w, h), h - 1);
        assert!(hints.ends_with("? help "), "{hints:?}");
        assert!(hints.contains("Tab complete"), "{hints:?}");
    }
}

#[test]
fn active_tab_eleven_of_twelve_is_visible() {
    let mut app = new_app();
    let mut files = Vec::new();
    for i in 1..=12 {
        let (f, mut tab) = loaded_tab("a\n1\n", 1);
        tab.name = format!("orders_{i:02}.csv");
        tab.id = TabId(i);
        app.state.tabs.push(tab);
        files.push(f);
    }
    app.state.active_tab = 10;
    let terminal = draw(&mut app, 80, 24);
    let top = row_text(&terminal, 0);
    assert!(top.contains(" orders_11.csv "), "{top:?}");
    assert!(top.contains('…'), "{top:?}");
    assert!(top.ends_with(" NORMAL "), "{top:?}");
    let buf = terminal.backend().buffer();
    let x = (0..80u16)
        .find(|&x| buf[(x, 0)].symbol() == "o" && buf[(x + 8, 0)].symbol() == "1")
        .unwrap();
    // The active tab is underlined.
    assert!(buf[(x, 0)].modifier.contains(Modifier::UNDERLINED));
}

// ---- tabs, opening, stdin (M1-08) and the indexer (M2-02) -----------------

#[tokio::test(flavor = "multi_thread")]
async fn three_files_open_three_tabs() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(dir.path(), "a.csv", "x,y\n1,2\n");
    let b = write_file(dir.path(), "b.csv", "x,y\n3,4\n5,6\n");
    let c = write_file(dir.path(), "c.csv", &grid(5000, 3));
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &["-y", s(&a), s(&b), s(&c)],
    );
    let mut tui = tui(120, 40);
    app.start();
    assert_eq!(app.state.tabs.len(), 3, "tabs appear immediately");
    assert_eq!(app.state.active_tab, 0);
    settle(&mut app, &mut tui).await;
    let names: Vec<_> = app.state.tabs.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["a.csv", "b.csv", "c.csv"]);
    ch(&mut app, &mut tui, '2');
    assert_eq!(tab(&app).name, "b.csv");
    ch(&mut app, &mut tui, '9');
    assert_eq!(tab(&app).name, "b.csv");
    // The indexer reported the exact count.
    ch(&mut app, &mut tui, '3');
    let l = tab(&app).loaded.as_ref().unwrap();
    assert_eq!(l.index_summary.unwrap().total_rows, 5000);
    assert_eq!(tab(&app).view_len(), 5000);
    assert!(
        !app.state.has_running_work(),
        "{:?} {:?}",
        app.state.toasts.front(),
        app.state.tabs.iter().map(|t| t.busy()).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_file_is_a_toast_and_no_tab() {
    let dir = tempfile::tempdir().unwrap();
    let b = write_file(dir.path(), "b.csv", "x\n1\n");
    let missing = dir.path().join("missing.csv");
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &[s(&missing), s(&b)],
    );
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert_eq!(app.state.tabs.len(), 1);
    assert_eq!(tab(&app).name, "b.csv");
    let toast = app.state.toasts.front().unwrap();
    assert_eq!(
        toast.text,
        format!("cannot open {}: file not found", missing.display())
    );
}

/// `tachy` with no file starts empty, without a toast; `Ctrl-o` opens one.
#[tokio::test(flavor = "multi_thread")]
async fn no_file_argument_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(dir.path(), "a.csv", "x,y\n1,2\n3,4\n");
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &[]);
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert!(app.state.tabs.is_empty());
    assert!(app.state.toasts.is_empty());
    let text = draw(&mut app, 120, 40).backend().to_string();
    assert!(text.contains(NO_FILE), "{text}");
    assert!(!app.should_quit);

    press(
        &mut app,
        &mut tui,
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    );
    type_text(&mut app, &mut tui, s(&path));
    press(&mut app, &mut tui, KeyCode::Enter, KeyModifiers::NONE);
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.state.tabs.is_empty() {
            app.step(&mut tui).await.unwrap();
        }
    })
    .await
    .unwrap();
    settle(&mut app, &mut tui).await;
    assert_eq!(app.state.tabs.len(), 1);
    assert_eq!(tab(&app).view_len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn all_files_missing_then_ctrl_o_opens_one() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.csv");
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &[s(&missing)]);
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert!(app.state.tabs.is_empty());
    assert_eq!(app.state.toasts.len(), 1);
    let text = draw(&mut app, 120, 40).backend().to_string();
    assert!(text.contains(NO_FILE), "{text}");
    // Tab-dependent keys are no-ops.
    for c in ['j', 'G', '$', '2'] {
        ch(&mut app, &mut tui, c);
    }
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    );
    assert!(!app.should_quit);

    // A bad path keeps the prompt open with the error on line 2.
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    );
    type_text(&mut app, &mut tui, s(&missing));
    press(&mut app, &mut tui, KeyCode::Enter, KeyModifiers::NONE);
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.state.prompt.as_ref().unwrap().pending.is_some() {
            app.step(&mut tui).await.unwrap();
        }
    })
    .await
    .unwrap();
    let prompt = app.state.prompt.as_ref().unwrap();
    assert!(
        matches!(&prompt.message, PromptMessage::Error(e) if e.contains("file not found")),
        "{:?}",
        prompt.message
    );
    assert!(app.state.tabs.is_empty());
    let terminal = draw(&mut app, 120, 40);
    assert!(row_text(&terminal, 1).contains("open › "));
    assert!(row_text(&terminal, 2).contains("file not found"));

    // A good one opens and closes the prompt.
    let good = write_file(dir.path(), "good.csv", "a\n1\n");
    app.state.prompt.as_mut().unwrap().input.set(s(&good));
    press(&mut app, &mut tui, KeyCode::Enter, KeyModifiers::NONE);
    settle(&mut app, &mut tui).await;
    assert!(app.state.prompt.is_none());
    assert_eq!(app.state.tabs.len(), 1);
    assert_eq!(tab(&app).name, "good.csv");
}

async fn complete_once(app: &mut App, tui: &mut Tui<TestBackend>) -> String {
    let before = app.state.prompt.as_ref().unwrap().input.text().to_owned();
    press(app, tui, KeyCode::Tab, KeyModifiers::NONE);
    // A cycle completes at once; otherwise wait for the listing.
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.state.prompt.as_ref().unwrap().input.text() == before {
            app.step(tui).await.unwrap();
        }
    })
    .await
    .unwrap();
    app.state.prompt.as_ref().unwrap().input.text().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_o_completes_paths() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "orders_2023.csv", "a\n");
    write_file(dir.path(), "orders_2024.csv", "a\n");
    write_file(dir.path(), "unique.tsv", "a\n");
    let mut app = new_app();
    let mut tui = tui(120, 40);
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    );
    let set = |app: &mut App, text: String| {
        app.state.prompt.as_mut().unwrap().input.set(text);
    };

    let base = format!("{}/", dir.path().display());
    set(&mut app, format!("{base}un"));
    let got = complete_once(&mut app, &mut tui).await;
    assert_eq!(got, format!("{base}unique.tsv"));
    set(&mut app, format!("{base}or"));
    let got = complete_once(&mut app, &mut tui).await;
    assert_eq!(got, format!("{base}orders_202"));
    let line2 = row_text(&draw(&mut app, 120, 40), 2);
    assert!(
        line2.contains("orders_2023.csv  orders_2024.csv"),
        "{line2:?}"
    );
    let got = complete_once(&mut app, &mut tui).await;
    assert_eq!(got, format!("{base}orders_2023.csv"));
    let got = complete_once(&mut app, &mut tui).await;
    assert_eq!(got, format!("{base}orders_2024.csv"));
    press(&mut app, &mut tui, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.state.prompt.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_w_closes_tabs_and_quits_on_the_last() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(dir.path(), "a.csv", "x\n1\n");
    let b = write_file(dir.path(), "b.csv", "x\n1\n");
    let c = write_file(dir.path(), "c.csv", "x\n1\n");
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &["-y", s(&a), s(&b), s(&c)],
    );
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    ch(&mut app, &mut tui, '2');
    let token = tab(&app).cancel.clone();
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    );
    assert!(token.is_cancelled(), "closing cancels the tab's work");
    // The tab to the right becomes active.
    assert_eq!(tab(&app).name, "c.csv");
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    );
    // c was the last: the left one becomes active.
    assert_eq!(tab(&app).name, "a.csv");
    assert!(!app.should_quit);
    press(
        &mut app,
        &mut tui,
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    );
    assert!(app.should_quit);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_frame_appears_before_a_slow_open_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(dir.path(), "slow.csv", "x\n1\n");
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &[s(&a)]);
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    app.opener = Arc::new(move |req| {
        release_rx.lock().unwrap().recv().unwrap();
        open_blocking(req)
    });
    let mut tui = tui(80, 24);
    app.start();
    app.step(&mut tui).await.unwrap();
    assert_eq!(app.renders, 1);
    let screen = tui.terminal.backend().to_string();
    assert!(screen.contains("opening…"), "{screen}");
    assert!(screen.contains("slow.csv"), "{screen}");
    assert!(tui.is_ticking());
    release_tx.send(()).unwrap();
    settle(&mut app, &mut tui).await;
    assert_eq!(tab(&app).view_len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn utf16_files_are_transcoded() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "name,city\nZoë,Köln\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let path = dir.path().join("u16.csv");
    std::fs::write(&path, bytes).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &["--tmp", s(tmp.path()), s(&path)],
    );
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert_eq!(tab(&app).name, "u16.csv");
    let terminal = draw(&mut app, 120, 40);
    let text = terminal.backend().to_string();
    assert!(text.contains("Zoë"), "{text}");
    assert!(text.contains("utf-16le→utf-8"), "{text}");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    drop(app);
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        0,
        "the transcoded copy is deleted with the tab"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_index_messages_are_ignored() {
    let mut app = new_app();
    let _f = with_tab(&mut app, "a\n1\n");
    let id = tab(&app).id;
    let summary = tachy_core::index::IndexSummary {
        total_rows: 1,
        ragged_rows: 0,
        unterminated_quote: false,
        path_used: tachy_core::index::IndexPath::Fast,
    };
    app.handle_msg(Msg::IndexReady {
        tab: id,
        generation: 7,
        summary,
    });
    app.handle_msg(Msg::IndexFailed {
        tab: id,
        generation: 7,
        error: "boom".into(),
    });
    let l = tab(&app).loaded.as_ref().unwrap();
    assert!(l.index_summary.is_none());
    assert!(l.index_error.is_none());
    assert!(app.state.toasts.is_empty());
    app.handle_msg(Msg::IndexReady {
        tab: id,
        generation: 0,
        summary,
    });
    assert!(tab(&app).loaded.as_ref().unwrap().index_summary.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dialect_restart_reindexes_with_a_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(dir.path(), "a.csv", "a;b\n1;2\n3;4\n");
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &[s(&a)]);
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert_eq!(tab(&app).loaded.as_ref().unwrap().columns.len(), 2);
    let id = tab(&app).id;
    let mut d = *tab(&app).loaded.as_ref().unwrap().source.dialect();
    d.delimiter = b',';
    app.restart_indexing(id, d);
    assert_eq!(tab(&app).generation, 1);
    settle(&mut app, &mut tui).await;
    let l = tab(&app).loaded.as_ref().unwrap();
    assert_eq!(l.columns.len(), 1);
    assert_eq!(l.index_summary.unwrap().total_rows, 2);
}

mod dialogs;
mod perf;
