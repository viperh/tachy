//! App-level tests of the go-to dialog and pending jumps (M2-03), the
//! Detected format dialog (M2-04), the sample wiring (M3-01, M3-02), the
//! inspector (M3-03) and the column layout keys and chooser (M3-04).

use tachy_core::{
    exec::Executor,
    parse::RecordParser,
    sample::{SamplePhase, sample_head},
    types::NullSet,
};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    components::dialogs::detected_format::DEBOUNCE,
    state::Focus,
    tab::{JumpTarget, MIN_COL_WIDTH},
};

// ---- helpers --------------------------------------------------------------

/// Runs the phase-1 sample of the active tab and installs it, as
/// `Msg::SampleReady` would.
async fn sample_now(app: &mut App) {
    let t = app.state.active_tab_mut().unwrap();
    let src = Arc::clone(&t.loaded.as_ref().unwrap().source);
    let exec = Executor::new(2);
    let sample = sample_head(src, &exec, NullSet::default(), CancellationToken::new())
        .await
        .unwrap();
    t.apply_sample(Arc::new(sample));
}

fn enter<B: Backend>(app: &mut App, tui: &mut Tui<B>) {
    press(app, tui, KeyCode::Enter, KeyModifiers::NONE);
}

fn esc<B: Backend>(app: &mut App, tui: &mut Tui<B>) {
    press(app, tui, KeyCode::Esc, KeyModifiers::NONE);
}

/// `g`, the input, `Enter`.
fn goto<B: Backend>(app: &mut App, tui: &mut Tui<B>, input: &str) {
    ch(app, tui, 'g');
    assert_eq!(app.state.dialog(), Some(DialogKind::Goto));
    type_text(app, tui, input);
    enter(app, tui);
}

/// Columns `x0..x1` of row `y`.
fn region(terminal: &Terminal<TestBackend>, y: u16, x0: u16, x1: u16) -> String {
    let buf = terminal.backend().buffer();
    (x0..x1).map(|x| buf[(x, y)].symbol()).collect()
}

/// The inspector panel (columns 80..120 at 120×40) as text.
fn inspector_text(terminal: &Terminal<TestBackend>) -> String {
    let h = terminal.backend().buffer().area.height;
    (1..h - 2)
        .map(|y| region(terminal, y, 80, 120).trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tab whose index the test publishes by hand: `full` is the complete
/// index of the same file, `live` the one the tab reads.
struct FakeIndex {
    full: Arc<RowIndex>,
    live: Arc<RowIndex>,
}

impl FakeIndex {
    fn install(tab: &mut Tab) -> FakeIndex {
        let l = tab.loaded.as_mut().unwrap();
        let full = Arc::clone(&l.index);
        let live = Arc::new(RowIndex::for_source(&l.source));
        l.index = Arc::clone(&live);
        FakeIndex { full, live }
    }

    /// Publishes the checkpoints of rows `< rows` and the row count.
    fn advance(&self, rows: u64) {
        let stride = self.full.stride();
        let mut k = self.live.published_checkpoints();
        let mut cps = Vec::new();
        while k * stride < rows && k < self.full.published_checkpoints() {
            cps.push(self.full.checkpoint(k).unwrap());
            k += 1;
        }
        self.live.push_checkpoints(&cps);
        self.live.set_indexed_rows(rows);
    }

    fn finish(&self) {
        let total = self.full.total_rows().unwrap();
        self.advance(total);
        self.live.finish(total, self.full.end_offset());
    }
}

/// A 5,000-row tab with a fake index at 2,048 rows.
fn indexing_grid() -> (App, NamedTempFile, FakeIndex, Tui<TestBackend>) {
    let mut app = new_app();
    let (f, mut t) = loaded_tab(&grid(5000, 3), 1);
    let fake = FakeIndex::install(&mut t);
    fake.advance(2048);
    app.state.tabs.push(t);
    draw(&mut app, 120, 40);
    (app, f, fake, tui(120, 40))
}

/// Runs the loop until every tab is indexed and its final sample is in.
async fn settle_sampled(app: &mut App, tui: &mut Tui<TestBackend>) {
    settle(app, tui).await;
    let done = |app: &App| {
        app.state.tabs.iter().all(|t| {
            t.loaded.as_ref().is_some_and(|l| {
                l.sample
                    .as_ref()
                    .is_some_and(|s| s.phase == SamplePhase::Spread || s.reached_eof)
            })
        })
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done(app) {
            app.step(tui).await.unwrap();
        }
    })
    .await
    .expect("samples did not arrive");
}

// ---- M2-03: pending jumps -------------------------------------------------

#[test]
fn g_waits_for_the_index_with_a_spinner() {
    let (mut app, _f, fake, mut tui) = indexing_grid();
    press(&mut app, &mut tui, KeyCode::Char('G'), KeyModifiers::SHIFT);
    let jump = tab(&app).pending_jump.unwrap();
    assert_eq!(jump.target, JumpTarget::LastRow);
    assert_eq!(tab(&app).cursor_row, 0);
    assert!(app.state.has_running_work());
    let status = row_text(&draw(&mut app, 120, 40), 38);
    assert!(
        status.contains("⠋ waiting for index… Esc to cancel"),
        "{status:?}"
    );
    assert!(status.contains("≥ 2,048 rows (counting…)"), "{status:?}");

    // The spinner advances per tick; rows published so far don't satisfy G.
    tick(&mut app, &mut tui);
    fake.advance(4096);
    tick(&mut app, &mut tui);
    assert_eq!(tab(&app).pending_jump.unwrap().frame, 2);
    let status = row_text(&draw(&mut app, 120, 40), 38);
    assert!(status.contains("⠹ waiting for index…"), "{status:?}");
    assert_eq!(tab(&app).cursor_row, 0);

    fake.finish();
    tick(&mut app, &mut tui);
    assert!(tab(&app).pending_jump.is_none());
    assert_eq!(tab(&app).cursor_row, 4999);
    let status = row_text(&draw(&mut app, 120, 40), 38);
    assert!(status.contains("R 5,000 / 5,000"), "{status:?}");
    assert!(!status.contains("waiting"), "{status:?}");
}

#[test]
fn esc_and_movement_cancel_a_pending_jump() {
    let (mut app, _f, _fake, mut tui) = indexing_grid();
    press(&mut app, &mut tui, KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert!(tab(&app).pending_jump.is_some());
    esc(&mut app, &mut tui);
    assert!(tab(&app).pending_jump.is_none());
    assert_eq!(tab(&app).cursor_row, 0);

    press(&mut app, &mut tui, KeyCode::Char('G'), KeyModifiers::SHIFT);
    ch(&mut app, &mut tui, 'j');
    assert!(tab(&app).pending_jump.is_none());
    assert_eq!(tab(&app).cursor_row, 1, "j cancels the jump, then moves");

    // A toast is dismissed before the jump is cancelled.
    press(&mut app, &mut tui, KeyCode::Char('G'), KeyModifiers::SHIFT);
    app.state
        .toasts
        .push(ToastMsg::new(ToastLevel::Info, "hello"), now());
    esc(&mut app, &mut tui);
    assert!(app.state.toasts.is_empty());
    assert!(tab(&app).pending_jump.is_some());
    esc(&mut app, &mut tui);
    assert!(tab(&app).pending_jump.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn index_ready_performs_the_jump() {
    let (mut app, _f, fake, mut tui) = indexing_grid();
    press(&mut app, &mut tui, KeyCode::Char('G'), KeyModifiers::SHIFT);
    fake.finish();
    let id = tab(&app).id;
    let summary = tachy_core::index::IndexSummary {
        total_rows: 5000,
        ragged_rows: 0,
        unterminated_quote: false,
        path_used: tachy_core::index::IndexPath::Fast,
    };
    app.handle_msg(Msg::IndexReady {
        tab: id,
        generation: 0,
        summary,
    });
    assert_eq!(tab(&app).cursor_row, 4999);
    assert!(tab(&app).pending_jump.is_none());
}

// ---- M2-03: the go-to dialog ----------------------------------------------

#[test]
fn goto_rows_percentages_and_columns() {
    let (mut app, _f, mut tui) = nav_app();
    goto(&mut app, &mut tui, "50");
    assert_eq!(app.state.dialog(), None);
    assert_eq!(app.state.mode, Mode::Normal);
    assert_eq!(tab(&app).cursor_row, 49);
    goto(&mut app, &mut tui, "1");
    assert_eq!(tab(&app).cursor_row, 0);
    goto(&mut app, &mut tui, "50%");
    assert_eq!(tab(&app).cursor_row, 49);
    goto(&mut app, &mut tui, "100%");
    assert_eq!(tab(&app).cursor_row, 99);
    goto(&mut app, &mut tui, "0%");
    assert_eq!(tab(&app).cursor_row, 0);
    goto(&mut app, &mut tui, "C15");
    assert_eq!(tab(&app).cursor_col, 15);
    assert_eq!(tab(&app).cursor_row, 0, "a column keeps the row");
    let h = tab(&app).h_layout(app.viewport());
    assert!(h.scrolling.iter().any(|s| s.col == 15 && !s.truncated));
}

#[test]
fn goto_clamps_past_the_end_with_a_toast() {
    let (mut app, _f, mut tui) = nav_app();
    goto(&mut app, &mut tui, "999999999");
    assert_eq!(tab(&app).cursor_row, 99);
    let toast = app.state.toasts.front().unwrap();
    assert_eq!(toast.text, "only 100 rows — went to the last row");
}

#[test]
fn goto_accepts_thousands_separators() {
    let mut app = new_app();
    let _f = with_tab(&mut app, &grid(2000, 2));
    draw(&mut app, 120, 40);
    let mut tui = tui(120, 40);
    goto(&mut app, &mut tui, "1,000");
    assert_eq!(tab(&app).cursor_row, 999);
    goto(&mut app, &mut tui, "1");
    goto(&mut app, &mut tui, "1_000");
    assert_eq!(tab(&app).cursor_row, 999);
}

#[test]
fn goto_errors_keep_the_dialog_open() {
    let (mut app, _f, mut tui) = nav_app();
    goto(&mut app, &mut tui, "zzz");
    assert_eq!(app.state.dialog(), Some(DialogKind::Goto));
    assert_eq!(
        app.state.goto.error.as_deref(),
        Some("no such column \"zzz\"")
    );
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!("goto_error_120x40", terminal.backend());
    // Typing clears the error; Esc closes without moving.
    press(&mut app, &mut tui, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.state.goto.error, None);
    esc(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    assert_eq!(pos(&app), (0, 0, 0, 0));

    // A hidden column.
    app.state.active_tab_mut().unwrap().layout.visible[3] = false;
    goto(&mut app, &mut tui, "c3");
    assert_eq!(
        app.state.goto.error.as_deref(),
        Some("column \"c3\" is hidden — show it with c")
    );
    esc(&mut app, &mut tui);
}

#[test]
fn goto_dialog_keeps_the_normal_pill_and_dims_the_table() {
    let (mut app, _f, mut tui) = nav_app();
    ch(&mut app, &mut tui, 'g');
    type_text(&mut app, &mut tui, "jk");
    // Letters are text, not movement.
    assert_eq!(app.state.goto.input.text(), "jk");
    assert_eq!(pos(&app), (0, 0, 0, 0));
    let terminal = draw(&mut app, 120, 40);
    assert!(row_text(&terminal, 0).contains("NORMAL"));
    let buf = terminal.backend().buffer();
    assert!(buf[(2, 10)].modifier.contains(Modifier::DIM));
}

#[test]
fn goto_percent_while_indexing_is_a_byte_position() {
    let (mut app, _f, fake, mut tui) = indexing_grid();
    let (src, data_start, len) = {
        let l = tab(&app).loaded.as_ref().unwrap();
        (Arc::clone(&l.source), l.source.data_start(), l.source.len())
    };
    let t = data_start + (len - data_start) / 2;
    // The middle of the file isn't indexed yet: the jump waits.
    goto(&mut app, &mut tui, "50%");
    assert_eq!(
        tab(&app).pending_jump.map(|j| j.target),
        Some(JumpTarget::ByteOffset(t))
    );
    fake.advance(4096);
    tick(&mut app, &mut tui);
    let row = tab(&app).cursor_row;
    assert!(tab(&app).pending_jump.is_none());
    // The first record starting at or after the offset.
    let mut p = RecordParser::new(src.dialect());
    let start = fake.full.offset_of(row, &src, &mut p).unwrap();
    let before = fake.full.offset_of(row - 1, &src, &mut p).unwrap();
    assert!(before < t && t <= start, "{before} < {t} <= {start}");
    // Once complete, 50% means the middle row.
    fake.finish();
    goto(&mut app, &mut tui, "50%");
    assert_eq!(tab(&app).cursor_row, 2499);
}

#[test]
fn goto_past_the_indexed_rows_waits() {
    let (mut app, _f, fake, mut tui) = indexing_grid();
    goto(&mut app, &mut tui, "3000");
    assert_eq!(
        tab(&app).pending_jump.map(|j| j.target),
        Some(JumpTarget::Row(2999))
    );
    fake.advance(3072);
    tick(&mut app, &mut tui);
    assert_eq!(tab(&app).cursor_row, 2999);
    // Past the end once complete: the last row and a toast.
    goto(&mut app, &mut tui, "9000");
    assert!(tab(&app).pending_jump.is_some());
    fake.finish();
    tick(&mut app, &mut tui);
    assert_eq!(tab(&app).cursor_row, 4999);
    assert_eq!(
        app.state.toasts.front().unwrap().text,
        "only 5,000 rows — went to the last row"
    );
}

// ---- M2-04: the Detected format dialog ------------------------------------

const SEMI: &str = "id;name;city\n1;\"Doe, J\";Berlin\n2;Smith;\"Lis;bon\"\n3;Ng;Osaka\n";

async fn opened(args: &[&str], content: &str) -> (App, tempfile::TempDir, Tui<TestBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(dir.path(), "people.csv", content);
    let mut all: Vec<&str> = args.to_vec();
    let p = s(&path).to_owned();
    all.push(&p);
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &all);
    let mut tui = tui(120, 40);
    app.start();
    settle_sampled(&mut app, &mut tui).await;
    (app, dir, tui)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dialog_opens_after_open_unless_told_not_to() {
    let (app, _d, _t) = opened(&[], SEMI).await;
    assert_eq!(app.state.dialog(), Some(DialogKind::DetectedFormat));
    assert_eq!(app.state.mode, Mode::Dialog);
    assert_eq!(app.state.visible_mode(), Mode::Normal);
    for args in [&["-y"][..], &["-d", ";"]] {
        let (app, _d, _t) = opened(args, SEMI).await;
        assert_eq!(app.state.dialog(), None, "{args:?}");
    }
    // `sniff.confirm: false`.
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(dir.path(), "people.csv", SEMI);
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &[s(&path)]);
    app.state.settings.sniff_confirm = false;
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert_eq!(app.state.dialog(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_dialog_per_tab_when_it_becomes_active() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(dir.path(), "a.csv", SEMI);
    let b = write_file(dir.path(), "b.csv", "x,y\n1,2\n");
    let mut app = app_files(Config::embedded(), ColorSupport::TrueColor, &[s(&a), s(&b)]);
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    assert_eq!(app.state.dialog(), Some(DialogKind::DetectedFormat));
    assert!(app.state.tabs[0].dialog_shown);
    assert!(!app.state.tabs[1].dialog_shown, "never stacked");
    enter(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    ch(&mut app, &mut tui, '2');
    assert_eq!(app.state.dialog(), Some(DialogKind::DetectedFormat));
    assert_eq!(app.state.detected.as_ref().unwrap().tab, tab(&app).id);
    enter(&mut app, &mut tui);
    ch(&mut app, &mut tui, '1');
    assert_eq!(app.state.dialog(), None, "shown once per tab");
}

#[tokio::test(flavor = "multi_thread")]
async fn d_highlights_at_once_and_reindexes_after_the_debounce() {
    let (mut app, _d, mut tui) = opened(&[], SEMI).await;
    assert_eq!(tab(&app).loaded.as_ref().unwrap().columns.len(), 3);
    ch(&mut app, &mut tui, 'd');
    let d = app.state.detected.as_ref().unwrap();
    assert_eq!(d.current.delimiter, b':');
    // `q` here cycles quoting; it does not quit.
    ch(&mut app, &mut tui, 'q');
    assert!(!app.should_quit);
    ch(&mut app, &mut tui, 'q');
    ch(&mut app, &mut tui, 'q');
    // Preview first, table later.
    assert_eq!(tab(&app).generation, 0);
    tick(&mut app, &mut tui);
    assert_eq!(tab(&app).generation, 0);
    tokio::time::sleep(DEBOUNCE + Duration::from_millis(20)).await;
    tick(&mut app, &mut tui);
    assert_eq!(tab(&app).generation, 1, "one re-index for several keys");
    assert_eq!(
        tab(&app)
            .loaded
            .as_ref()
            .unwrap()
            .source
            .dialect()
            .delimiter,
        b':'
    );
    settle(&mut app, &mut tui).await;
    let l = tab(&app).loaded.as_ref().unwrap();
    assert_eq!(l.columns.len(), 1);
    assert_eq!(l.index.total_rows(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn esc_reverts_to_the_detected_dialect() {
    // Without changes: no re-index.
    let (mut app, _d, mut tui) = opened(&[], SEMI).await;
    esc(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    assert_eq!(tab(&app).generation, 0);

    // `d d`, applied, then Esc: back to `;` with a new index.
    let (mut app, _d, mut tui) = opened(&[], SEMI).await;
    ch(&mut app, &mut tui, 'd');
    ch(&mut app, &mut tui, 'd');
    tokio::time::sleep(DEBOUNCE + Duration::from_millis(20)).await;
    tick(&mut app, &mut tui);
    assert_eq!(tab(&app).generation, 1);
    let old_index = Arc::clone(&tab(&app).loaded.as_ref().unwrap().index);
    esc(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    assert_eq!(app.state.mode, Mode::Normal);
    assert_eq!(tab(&app).generation, 2);
    let l = tab(&app).loaded.as_ref().unwrap();
    assert_eq!(l.source.dialect().delimiter, b';');
    assert!(!Arc::ptr_eq(&old_index, &l.index));
    // Stale messages of generation 1 are ignored by the generation check.
    settle(&mut app, &mut tui).await;
    assert_eq!(tab(&app).loaded.as_ref().unwrap().columns.len(), 3);

    // `d` then Esc before the debounce: nothing was applied, nothing to undo.
    let (mut app, _d, mut tui) = opened(&[], SEMI).await;
    ch(&mut app, &mut tui, 'd');
    esc(&mut app, &mut tui);
    assert_eq!(tab(&app).generation, 0);
    assert!(app.state.detected.is_none());
    assert!(
        app.state.toasts.is_empty(),
        "{:?}",
        app.state.toasts.front()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_applies_a_pending_edit_and_leaves_raw_mode() {
    let (mut app, _d, mut tui) = opened(&[], SEMI).await;
    ch(&mut app, &mut tui, 'h');
    ch(&mut app, &mut tui, 'r');
    assert!(tab(&app).raw_mode);
    enter(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    assert!(!tab(&app).raw_mode);
    assert_eq!(tab(&app).generation, 1);
    assert!(!tab(&app).loaded.as_ref().unwrap().source.dialect().header);
}

#[tokio::test(flavor = "multi_thread")]
async fn detected_format_snapshots() {
    for (w, h) in [(120, 40), (80, 24)] {
        let (mut app, _d, mut tui) = opened(&[], SEMI).await;
        let terminal = draw(&mut app, w, h);
        insta::assert_snapshot!(
            format!("detected_format_unchanged_{w}x{h}"),
            terminal.backend()
        );
        // The highlighted delimiters are amber.
        if w == 120 {
            let buf = terminal.backend().buffer();
            let theme = Theme::DARK;
            let y = (0..h)
                .find(|&y| row_text(&terminal, y).contains("id;name;city"))
                .unwrap();
            let x = (0..w).find(|&x| buf[(x, y)].symbol() == ";").unwrap();
            assert_eq!(buf[(x, y)].bg, theme.amber);
            assert!(row_text(&terminal, 0).contains("NORMAL"));
            assert!(buf[(0, 0)].modifier.contains(Modifier::DIM), "backdrop");
        }

        ch(&mut app, &mut tui, 'd');
        ch(&mut app, &mut tui, 'h');
        let terminal = draw(&mut app, w, h);
        insta::assert_snapshot!(
            format!("detected_format_changed_{w}x{h}"),
            terminal.backend()
        );
        let text = terminal.backend().to_string();
        assert!(text.contains(": (colon)*"), "{text}");
        assert!(text.contains("no*"), "{text}");

        ch(&mut app, &mut tui, 'r');
        let terminal = draw(&mut app, w, h);
        insta::assert_snapshot!(format!("detected_format_raw_{w}x{h}"), terminal.backend());
    }
}

#[test]
fn apply_dialect_side_effects() {
    let (_f, mut t) = loaded_tab("a;b\n1;2\n3;4\n", 1);
    let index_token = t.loaded.as_ref().unwrap().index_cancel.clone();
    let sample_token = t.loaded.as_ref().unwrap().sample_cancel.clone();
    let old_index = Arc::clone(&t.loaded.as_ref().unwrap().index);
    t.prepare_frame(Viewport {
        body_height: 10,
        table_width: 80,
    });
    assert!(!t.loaded.as_ref().unwrap().cache.is_empty());
    t.cursor_row = 1;
    t.cursor_col = 1;
    t.pending_jump = Some(crate::tab::PendingJump {
        target: JumpTarget::LastRow,
        started: std::time::Instant::now(),
        frame: 0,
    });
    t.push_view(crate::tab::tests::sorted_view(&[1, 0]), None);
    t.cursor_row = 1;
    t.cursor_col = 1;
    t.layout.visible[0] = false;
    let mut d = *t.loaded.as_ref().unwrap().source.dialect();
    d.quote = Some(b'\'');
    t.apply_dialect(d, 65_536);
    assert!(index_token.is_cancelled());
    assert!(sample_token.is_cancelled());
    assert_eq!(t.generation, 1);
    let l = t.loaded.as_ref().unwrap();
    assert!(l.cache.is_empty());
    assert!(!Arc::ptr_eq(&old_index, &l.index));
    assert!(!l.index.is_complete());
    assert!(l.sample.is_none());
    assert_eq!(l.sniff.dialect.quote, Some(b'\''));
    assert!(l.columns.iter().all(|c| c.stats.is_none()));
    assert_eq!(t.views.depth(), 1, "views popped");
    assert!(t.views.active_view().is_all());
    assert!(t.pending_jump.is_none());
    assert_eq!((t.cursor_row, t.top_row), (0, 0));
    assert_eq!(t.cursor_col, 1, "the column index is kept");
    assert!(t.layout.visible.iter().all(|v| *v), "layout reset");
}

// ---- M3-01 / M3-02: the sample wiring --------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn samples_arrive_in_two_phases() {
    let mut content = String::from("id,score,day\n");
    for i in 0..30_000 {
        content.push_str(&format!("{i},{}.5,2024-01-{:02}\n", i % 97, i % 28 + 1));
    }
    let (app, _d, _t) = opened(&["-y"], &content).await;
    let l = tab(&app).loaded.as_ref().unwrap();
    let sample = l.sample.as_ref().unwrap();
    assert_eq!(sample.phase, SamplePhase::Spread);
    assert_eq!(sample.rows_sampled, 20_000);
    let types: Vec<_> = l.columns.iter().map(|c| c.ty()).collect();
    assert_eq!(types, [ColType::I64, ColType::F64, ColType::Date]);
    let stats = l.columns[0].stats.as_ref().unwrap();
    assert_eq!(stats.label(), "sample 20k rows");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_type_recomputes_stats_and_survives_phase_two() {
    let mut app = new_app();
    let _f = with_tab(&mut app, &table_fixture());
    sample_now(&mut app).await;
    assert_eq!(
        tab(&app).loaded.as_ref().unwrap().columns[0].ty(),
        ColType::I64
    );
    assert!(app.set_column_type(0, Some(ColType::Str)));
    let meta = &tab(&app).loaded.as_ref().unwrap().columns[0];
    assert_eq!(meta.ty(), ColType::Str);
    let stats = meta.stats.as_ref().unwrap();
    assert_eq!(stats.for_type, ColType::Str);
    assert!(stats.numeric.is_none());
    // Another sample (phase 2) keeps the override.
    sample_now(&mut app).await;
    let meta = &tab(&app).loaded.as_ref().unwrap().columns[0];
    assert_eq!(meta.ty(), ColType::Str);
    assert_eq!(meta.inferred, ColType::I64);
    assert!(!app.set_column_type(99, None));

    // Without a cached sample the old stats stay, marked stale.
    app.state
        .active_tab_mut()
        .unwrap()
        .loaded
        .as_mut()
        .unwrap()
        .sample = None;
    assert!(app.set_column_type(0, Some(ColType::F64)));
    let text = inspector_text(&draw(&mut app, 120, 40));
    assert!(text.contains("COLUMN (stats for str)"), "{text}");
    assert!(text.contains("id · f64 (set) · col 1/5"), "{text}");
}

// ---- M3-03: the inspector --------------------------------------------------

/// The fixture with a sample, cursor on column `col`.
async fn inspected(col: usize) -> (App, NamedTempFile, Tui<TestBackend>) {
    let mut app = new_app();
    let f = with_tab(&mut app, &table_fixture());
    sample_now(&mut app).await;
    draw(&mut app, 120, 40);
    let mut tui = tui(120, 40);
    for _ in 0..col {
        ch(&mut app, &mut tui, 'l');
    }
    (app, f, tui)
}

#[tokio::test(flavor = "multi_thread")]
async fn inspector_snapshots() {
    // id (i64), name (str), city (enum), amount (f64).
    for (col, name) in [(0, "numeric"), (1, "string"), (2, "enum"), (3, "float")] {
        let (mut app, _f, _t) = inspected(col).await;
        let terminal = draw(&mut app, 120, 40);
        insta::assert_snapshot!(format!("inspector_{name}"), inspector_text(&terminal));
    }
    // Before the sample.
    let mut app = new_app();
    let _f = with_tab(&mut app, &table_fixture());
    let terminal = draw(&mut app, 120, 40);
    let text = inspector_text(&terminal);
    insta::assert_snapshot!("inspector_sampling", text);
    assert!(text.contains("sampling…"));
}

#[tokio::test(flavor = "multi_thread")]
async fn inspector_stats_follow_the_cursor() {
    let (mut app, _f, mut tui) = inspected(0).await;
    let text = inspector_text(&draw(&mut app, 120, 40));
    assert!(text.contains("id · i64 · col 1/5"), "{text}");
    assert!(text.contains("min       1"), "{text}");
    assert!(text.contains("max       30"), "{text}");
    assert!(text.contains("mean      15.5"), "{text}");
    ch(&mut app, &mut tui, 'l');
    ch(&mut app, &mut tui, 'l');
    let text = inspector_text(&draw(&mut app, 120, 40));
    assert!(text.contains("city · enum · col 3/5"), "{text}");
    assert!(text.contains("TOP VALUES · sample 30 rows"), "{text}");
    // The record follows the row.
    ch(&mut app, &mut tui, 'j');
    let text = inspector_text(&draw(&mut app, 120, 40));
    assert!(text.contains("name          user002"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn top_value_bars_are_proportional() {
    let mut app = new_app();
    let mut content = String::from("k\n");
    for (v, n) in [("a", 40), ("b", 20), ("c", 10)] {
        for _ in 0..n {
            content.push_str(v);
            content.push('\n');
        }
    }
    let _f = with_tab(&mut app, &content);
    sample_now(&mut app).await;
    let terminal = draw(&mut app, 120, 40);
    let rows: Vec<String> = (1..38).map(|y| region(&terminal, y, 81, 120)).collect();
    let bar = |label: &str| -> String {
        let row = rows.iter().find(|r| r.starts_with(label)).unwrap();
        row.chars().skip(15).take(16).collect()
    };
    assert_eq!(bar("a "), "█".repeat(16), "the largest bar fills 16 cells");
    let b = bar("b ");
    assert!(b.starts_with(&"█".repeat(8)), "{b:?}");
    // The track is drawn in the track colour.
    let buf = terminal.backend().buffer();
    let y = 1 + rows.iter().position(|r| r.starts_with("b ")).unwrap() as u16;
    assert_eq!(buf[(81 + 15 + 8, y)].fg, Theme::DARK.track);
    assert_eq!(buf[(81 + 15, y)].fg, Theme::DARK.teal);
}

#[tokio::test(flavor = "multi_thread")]
async fn bars_use_glyphs_without_color() {
    let mut app = app_with(Config::embedded(), ColorSupport::NoColor);
    let _f = with_tab(&mut app, "k\na\na\nb\n");
    sample_now(&mut app).await;
    let text = inspector_text(&draw(&mut app, 120, 40));
    assert!(text.contains("████████░░░░░░░░"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_columns_stay_in_the_record() {
    let (mut app, _f, _tui) = inspected(0).await;
    app.state.active_tab_mut().unwrap().layout.visible[2] = false;
    let terminal = draw(&mut app, 120, 40);
    let text = inspector_text(&terminal);
    assert!(text.contains("city          Lisbon"), "{text}");
    assert!(!row_text(&terminal, 1).contains("city"));
}

#[tokio::test(flavor = "multi_thread")]
async fn focus_select_and_the_value_popup() {
    let mut app = new_app();
    let long = "x".repeat(300);
    let _f = with_tab(&mut app, &format!("id,text\n1,{long}\n"));
    sample_now(&mut app).await;
    draw(&mut app, 120, 40);
    let mut tui = tui(120, 40);
    press(&mut app, &mut tui, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.state.focus, Focus::Inspector);
    // j selects a field; the table cursor stays.
    ch(&mut app, &mut tui, 'j');
    assert_eq!(app.state.inspector.selected, 1);
    assert_eq!(pos(&app), (0, 0, 0, 0));
    ch(&mut app, &mut tui, 'l');
    assert_eq!(pos(&app), (0, 0, 0, 0), "table keys don't move the cursor");
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!("inspector_focused", inspector_text(&terminal));
    let buf = terminal.backend().buffer();
    assert_eq!(
        buf[(81, 1)].fg,
        Theme::DARK.amber,
        "focused labels are amber"
    );

    enter(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), Some(DialogKind::ValuePopup));
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!("value_popup", terminal.backend());
    ch(&mut app, &mut tui, 'j');
    esc(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    assert_eq!(app.state.focus, Focus::Inspector);
    esc(&mut app, &mut tui);
    assert_eq!(app.state.focus, Focus::Table);
    // Tab cycles back to the table.
    press(&mut app, &mut tui, KeyCode::Tab, KeyModifiers::NONE);
    press(&mut app, &mut tui, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.state.focus, Focus::Table);
}

#[test]
fn the_inspector_hides_below_120_columns_and_focus_falls_back() {
    let (mut app, _f, mut tui) = nav_app();
    press(&mut app, &mut tui, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.state.focus, Focus::Inspector);
    app.handle_event(Event::Resize(119, 40)).unwrap();
    app.handle_actions(&mut tui).unwrap();
    assert_eq!(app.state.focus, Focus::Table);
    let terminal = draw(&mut app, 119, 40);
    assert!(!terminal.backend().to_string().contains("RECORD"));
    // Tab can't focus a hidden inspector.
    press(&mut app, &mut tui, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.state.focus, Focus::Table);
    app.handle_event(Event::Resize(120, 40)).unwrap();
    app.handle_actions(&mut tui).unwrap();
    assert!(
        draw(&mut app, 120, 40)
            .backend()
            .to_string()
            .contains("RECORD")
    );
    // `i` hides it; focus falls back.
    press(&mut app, &mut tui, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.state.focus, Focus::Inspector);
    ch(&mut app, &mut tui, 'i');
    assert!(!app.state.inspector_visible);
    assert_eq!(app.state.focus, Focus::Table);
}

// ---- M3-04: widths, freeze, the column chooser -----------------------------

#[tokio::test(flavor = "multi_thread")]
async fn widths_follow_the_p95_and_never_cut_headers() {
    let mut content = String::from("v,a_header_name_much_longer_than_forty_cells_wide\n");
    for i in 0..100 {
        let v = if i % 50 == 7 {
            "y".repeat(200)
        } else {
            "x".repeat(10)
        };
        content.push_str(&format!("{v},1\n"));
    }
    let mut app = new_app();
    let _f = with_tab(&mut app, &content);
    // Before the sample, the first screen (with an outlier) decides.
    draw(&mut app, 120, 40);
    assert_eq!(tab(&app).layout.widths[0], 40);
    sample_now(&mut app).await;
    assert_eq!(tab(&app).layout.widths, [10, 47]);
}

#[tokio::test(flavor = "multi_thread")]
async fn resize_keys_stick_through_scrolling_and_resampling() {
    let (mut app, _f, mut tui) = inspected(1).await;
    let w = tab(&app).layout.widths[1];
    ch(&mut app, &mut tui, '>');
    ch(&mut app, &mut tui, '>');
    assert_eq!(tab(&app).layout.widths[1], w + 2);
    ch(&mut app, &mut tui, '<');
    assert_eq!(tab(&app).layout.widths[1], w + 1);
    for _ in 0..50 {
        ch(&mut app, &mut tui, '<');
    }
    assert_eq!(tab(&app).layout.widths[1], MIN_COL_WIDTH);
    assert!(tab(&app).layout.manual[1]);
    press(&mut app, &mut tui, KeyCode::PageDown, KeyModifiers::NONE);
    draw(&mut app, 120, 40);
    sample_now(&mut app).await;
    assert_eq!(tab(&app).layout.widths[1], MIN_COL_WIDTH);
    // `=`: the widest visible value (`user0NN`) or the header.
    ch(&mut app, &mut tui, '=');
    assert_eq!(tab(&app).layout.widths[1], 7);
    // `>` stops at the available width.
    for _ in 0..200 {
        ch(&mut app, &mut tui, '>');
    }
    let vp = app.viewport();
    let max = tab(&app).layout.widths[1];
    let h = tab(&app).h_layout(vp);
    assert!(
        h.scrolling.iter().any(|s| s.col == 1 && !s.truncated),
        "{h:?}"
    );
    assert!(max < vp.table_width);
}

#[test]
fn freeze_keeps_columns_while_scrolling() {
    let (mut app, _f, mut tui) = nav_app();
    app.set_freeze(2);
    ch(&mut app, &mut tui, '$');
    let h = tab(&app).h_layout(app.viewport());
    let frozen: Vec<_> = h.frozen.iter().map(|s| s.col).collect();
    assert_eq!(frozen, [0, 1]);
    assert!(h.scrolling.iter().all(|s| s.col >= 2));
    app.set_freeze(0);
    ch(&mut app, &mut tui, '$');
    let h = tab(&app).h_layout(app.viewport());
    assert!(h.frozen.is_empty());
    assert_eq!(h.frozen_divider, None);
    // More than there are columns: everything is frozen, nothing scrolls.
    app.set_freeze(50);
    let mut app2 = new_app();
    let _g = with_tab(&mut app2, &grid(5, 3));
    app2.set_freeze(50);
    let terminal = draw(&mut app2, 120, 40);
    let h = tab(&app2).h_layout(app2.viewport());
    assert_eq!(h.frozen.len(), 3);
    assert_eq!(h.more_cols, 0);
    assert!(row_text(&terminal, 3).contains("r0c2"));
}

#[test]
fn a_wide_frozen_block_is_reduced_with_a_notice() {
    let (mut app, _f, mut tui) = nav_app();
    // Widen the first columns until three of them can't fit in 80 cells.
    {
        let t = app.state.active_tab_mut().unwrap();
        for c in 0..3 {
            t.layout.widths[c] = 35;
            t.layout.manual[c] = true;
        }
    }
    app.set_freeze(3);
    app.handle_event(Event::Resize(80, 24)).unwrap();
    app.handle_actions(&mut tui).unwrap();
    let vp = app.viewport();
    assert_eq!(tab(&app).frozen_count(vp), 1);
    assert_eq!(tab(&app).layout.freeze, 3);
    let toasts: Vec<_> = std::iter::from_fn(|| {
        let t = app.state.toasts.front().map(|t| t.text.clone());
        app.state.toasts.dismiss(now());
        t
    })
    .collect();
    assert_eq!(toasts, ["freeze 3 (1 shown)"]);
    // Shown once: moving around doesn't repeat it.
    ch(&mut app, &mut tui, 'l');
    assert!(app.state.toasts.is_empty());
    // At least one scrolling column stays visible.
    ch(&mut app, &mut tui, '$');
    let h = tab(&app).h_layout(vp);
    assert!(!h.scrolling.is_empty(), "{h:?}");
}

#[test]
fn the_chooser_hides_reorders_and_discards() {
    let mut app = new_app();
    let _f = with_tab(&mut app, &table_fixture());
    draw(&mut app, 120, 40);
    let mut tui = tui(120, 40);
    ch(&mut app, &mut tui, 'c');
    assert_eq!(app.state.dialog(), Some(DialogKind::ColumnChooser));
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!("column_chooser", terminal.backend());

    // Hide `name`, move `note` to the front, apply.
    ch(&mut app, &mut tui, 'j');
    ch(&mut app, &mut tui, ' ');
    for _ in 0..3 {
        ch(&mut app, &mut tui, 'j');
    }
    for _ in 0..4 {
        ch(&mut app, &mut tui, 'K');
    }
    enter(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), None);
    let t = tab(&app);
    assert_eq!(t.layout.order, [4, 0, 1, 2, 3]);
    assert_eq!(t.layout.display(), [4, 0, 2, 3]);
    let terminal = draw(&mut app, 120, 40);
    let header = row_text(&terminal, 1);
    assert!(header.contains("note"), "{header}");
    assert!(!header.contains("name"), "{header}");
    assert!(inspector_text(&terminal).contains("name"));
    // `0` and `$` follow the new order.
    ch(&mut app, &mut tui, '$');
    assert_eq!(tab(&app).cursor_source_col(), Some(3));
    ch(&mut app, &mut tui, '0');
    assert_eq!(tab(&app).cursor_source_col(), Some(4));

    // Esc discards.
    ch(&mut app, &mut tui, 'c');
    ch(&mut app, &mut tui, 'a');
    ch(&mut app, &mut tui, 'J');
    esc(&mut app, &mut tui);
    assert_eq!(tab(&app).layout.order, [4, 0, 1, 2, 3]);
    assert_eq!(tab(&app).layout.display(), [4, 0, 2, 3]);
}

#[test]
fn the_chooser_filter_and_the_last_column_error() {
    let mut app = new_app();
    let _f = with_tab(&mut app, "price,name,unit_price\n1,a,2\n");
    draw(&mut app, 120, 40);
    let mut tui = tui(120, 40);
    ch(&mut app, &mut tui, 'c');
    ch(&mut app, &mut tui, '/');
    type_text(&mut app, &mut tui, "pri");
    let c = app.state.chooser.as_ref().unwrap();
    assert!(c.filtering);
    assert_eq!(c.filter.text(), "pri");
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!("column_chooser_filtered", terminal.backend());
    // Enter ends the filter input; the next Enter would apply.
    enter(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), Some(DialogKind::ColumnChooser));
    ch(&mut app, &mut tui, ' ');
    ch(&mut app, &mut tui, 'j');
    ch(&mut app, &mut tui, ' ');
    // Clear the filter and try to hide the last visible column.
    ch(&mut app, &mut tui, '/');
    esc(&mut app, &mut tui);
    assert_eq!(app.state.dialog(), Some(DialogKind::ColumnChooser));
    // The selection (row 1) is now `name`, the only visible column.
    ch(&mut app, &mut tui, ' ');
    let c = app.state.chooser.as_ref().unwrap();
    assert_eq!(c.visible, [false, true, false]);
    assert_eq!(
        c.error.as_deref(),
        Some("at least one column must be visible")
    );
    let terminal = draw(&mut app, 120, 40);
    insta::assert_snapshot!("column_chooser_error", terminal.backend());
    enter(&mut app, &mut tui);
    assert_eq!(tab(&app).layout.display(), [1]);
}

#[tokio::test(start_paused = true)]
async fn queued_keys_see_the_context_earlier_keys_left() {
    let (mut app, _f, mut tui) = nav_app();
    // Typed faster than a frame: `g`, `4`, `0`, Enter, then `c`, Space,
    // Enter, `>`. Each key must resolve after the previous one ran.
    let key = |c: KeyCode| Event::Key(KeyEvent::new(c, KeyModifiers::NONE));
    for code in [
        KeyCode::Char('g'),
        KeyCode::Char('4'),
        KeyCode::Char('0'),
        KeyCode::Enter,
        KeyCode::Char('c'),
        KeyCode::Char(' '),
        KeyCode::Enter,
        KeyCode::Char('>'),
    ] {
        tui.event_tx.send(key(code)).unwrap();
    }
    app.step(&mut tui).await.unwrap();
    let t = tab(&app);
    assert_eq!(t.cursor_row, 39, "4 and 0 went to the go-to input");
    assert_eq!(app.state.active_tab, 0);
    assert!(!t.layout.visible[0], "the chooser hid c0");
    assert!(t.layout.manual[1], "`>` grew the new cursor column");
}
