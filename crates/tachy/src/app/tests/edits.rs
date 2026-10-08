//! App-level tests of column edits (`edit`, `reset edits`) and duplicate
//! views (`dupes`, `dedupe`) run from the palette.

use super::*;

// ---- helpers --------------------------------------------------------------

const PEOPLE: &str = "name,city,code\n\
ann,Rome,$10\n\
bob,Oslo,$20\n\
cid,rome ,$30\n\
dee,Oslo,$40\n";

/// `:`, the command, `Enter`.
fn palette<B: Backend>(app: &mut App, tui: &mut Tui<B>, command: &str) {
    ch(app, tui, ':');
    assert_eq!(app.state.overlay, Some(Overlay::Palette));
    type_text(app, tui, command);
    press(app, tui, KeyCode::Enter, KeyModifiers::NONE);
}

/// The text of every screen row.
fn screen(terminal: &Terminal<TestBackend>) -> String {
    let h = terminal.backend().buffer().area.height;
    (0..h)
        .map(|y| row_text(terminal, y).trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `PEOPLE` opened from a file (`-y`: no Detected format dialog), indexed
/// and sampled, with the job executor running.
async fn opened() -> (App, tempfile::TempDir, Tui<TestBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(dir.path(), "people.csv", PEOPLE);
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &["-y", s(&path)],
    );
    let mut tui = tui(120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while tab(&app).loaded.as_ref().unwrap().sample.is_none() {
            app.step(&mut tui).await.unwrap();
        }
    })
    .await
    .expect("no sample");
    (app, dir, tui)
}

/// Runs the loop until the active view's label is `label`.
async fn until_view(app: &mut App, tui: &mut Tui<TestBackend>, label: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while tab(app).views.active_view().label() != label {
            app.step(tui).await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {label} view"));
}

fn view_ids(app: &App) -> Vec<u64> {
    let t = tab(app);
    t.views.active_view().row_ids(0, t.view_len() as usize)
}

fn last_toast(app: &App) -> String {
    app.state
        .toasts
        .front()
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

// ---- edit -----------------------------------------------------------------

#[test]
fn edit_from_the_palette_changes_the_table_and_marks_the_header() {
    let mut app = new_app();
    let _f = with_tab(&mut app, PEOPLE);
    let mut tui = tui(120, 40);
    palette(&mut app, &mut tui, "edit name: drop 1 | upper");
    assert_eq!(app.state.overlay, None, "the palette closed");
    let t = draw(&mut app, 120, 40);
    let text = screen(&t);
    assert!(text.contains("NN"), "{text}");
    assert!(text.contains("OB"), "{text}");
    assert!(!text.contains("ann"), "{text}");
    assert!(text.contains("str ✎"), "{text}");
    // Only `name` is marked.
    assert_eq!(text.matches('✎').count(), 1, "{text}");

    // Undo one op, then reset.
    palette(&mut app, &mut tui, "edit name: undo");
    let text = screen(&draw(&mut app, 120, 40));
    assert!(text.contains("nn") && !text.contains("NN"), "{text}");
    palette(&mut app, &mut tui, "edit name: reset");
    let text = screen(&draw(&mut app, 120, 40));
    assert!(text.contains("ann") && !text.contains('✎'), "{text}");
}

#[test]
fn a_bad_edit_keeps_the_palette_open_with_the_error() {
    let mut app = new_app();
    let _f = with_tab(&mut app, PEOPLE);
    let mut tui = tui(120, 40);
    palette(&mut app, &mut tui, "edit name: frob");
    assert_eq!(app.state.overlay, Some(Overlay::Palette));
    let text = screen(&draw(&mut app, 120, 40));
    assert!(text.contains("unknown edit \"frob\""), "{text}");
    assert!(!tab(&app).is_edited(0));
}

#[test]
fn reset_edits_clears_every_column() {
    let mut app = new_app();
    let _f = with_tab(&mut app, PEOPLE);
    let mut tui = tui(120, 40);
    palette(&mut app, &mut tui, "edit name: upper");
    palette(&mut app, &mut tui, "edit code: drop 1");
    assert!(tab(&app).is_edited(0) && tab(&app).is_edited(2));
    palette(&mut app, &mut tui, "reset edits");
    assert!(!tab(&app).is_edited(0) && !tab(&app).is_edited(2));
    let text = screen(&draw(&mut app, 120, 40));
    assert!(text.contains("$10") && text.contains("ann"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_retype_columns_and_show_in_the_inspector() {
    let (mut app, _d, mut tui) = opened().await;
    let ty = |app: &App| tab(app).loaded.as_ref().unwrap().columns[2].ty();
    assert_eq!(ty(&app), ColType::Str);
    palette(&mut app, &mut tui, "edit code: drop 1");
    assert_eq!(ty(&app), ColType::I64, "`$10` → `10` is an integer");
    let text = screen(&draw(&mut app, 120, 40));
    assert!(text.contains("i64 ✎"), "{text}");
    // The inspector on `code` lists the chain.
    app.state.inspector_visible = true;
    tab_mut(&mut app).cursor_col = 2;
    let text = screen(&draw(&mut app, 120, 40));
    assert!(text.contains("edits") && text.contains("drop 1"), "{text}");
}

fn tab_mut(app: &mut App) -> &mut Tab {
    app.state.active_tab_mut().unwrap()
}

// ---- dupes / dedupe -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dupes_pushes_a_view_of_the_duplicate_rows() {
    let (mut app, _d, mut tui) = opened().await;
    let depth = tab(&app).views.depth();
    palette(&mut app, &mut tui, "dupes city");
    until_view(&mut app, &mut tui, "duplicates").await;
    // `rome ` ≠ `Rome`: only the Oslo rows.
    assert_eq!(view_ids(&app), [1, 3]);
    assert_eq!(last_toast(&app), "2 duplicate rows in 1 group");
    let text = screen(&draw(&mut app, 120, 40));
    assert!(
        text.contains("duplicates"),
        "the top bar names the view: {text}"
    );
    // Pushed on the stack like a sort.
    assert_eq!(tab(&app).views.depth(), depth + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn dedupe_compares_edited_values() {
    let (mut app, _d, mut tui) = opened().await;
    palette(&mut app, &mut tui, "edit city: trim | lower");
    palette(&mut app, &mut tui, "dedupe city");
    until_view(&mut app, &mut tui, "deduplicated").await;
    assert_eq!(view_ids(&app), [0, 1]);
    assert_eq!(last_toast(&app), "removed 2 duplicate rows (2 groups)");
}

#[tokio::test(flavor = "multi_thread")]
async fn dupes_without_columns_compares_whole_rows() {
    let (mut app, _d, mut tui) = opened().await;
    palette(&mut app, &mut tui, "dupes");
    until_view(&mut app, &mut tui, "duplicates").await;
    assert!(view_ids(&app).is_empty());
    assert_eq!(last_toast(&app), "no duplicate rows");
}
