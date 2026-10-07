//! §17 UI timings (M7-05): time to first frame and scroll latency.
//!
//! `tachy` is a binary crate, so a criterion bench can't reach `App`; these
//! are `#[ignore]` tests that print their measurements instead. Run them in
//! release, on a large file:
//!
//! ```sh
//! TACHY_PERF_FILE=target/bench-data/x.csv \
//!   cargo test --release -p tachy -- --ignored --nocapture perf_
//! ```
//!
//! Without `TACHY_PERF_FILE` a 256 MiB file is generated in the temp dir.

use std::{io::BufWriter, time::Instant};

use super::*;

/// Targets from §17.
const FIRST_FRAME_TARGET: Duration = Duration::from_millis(100);
const FRAME_TARGET: Duration = Duration::from_millis(16);

/// The file to open: `TACHY_PERF_FILE`, or a generated one (kept alive by
/// the returned guard).
fn perf_file() -> (PathBuf, Option<NamedTempFile>) {
    if let Some(p) = std::env::var_os("TACHY_PERF_FILE") {
        return (PathBuf::from(p), None);
    }
    let f = NamedTempFile::new().unwrap();
    let mut w = BufWriter::with_capacity(8 << 20, f.reopen().unwrap());
    writeln!(w, "id,price,country,status,customer,notes").unwrap();
    let mut written = 0u64;
    let mut i = 0u64;
    while written < 256 << 20 {
        let line = format!(
            "{i},{}.{:02},C{},s{},customer {},\"note, {} with \"\"quotes\"\"\"\n",
            i % 9973,
            i % 100,
            i % 30,
            i % 6,
            i % 1000,
            i
        );
        written += line.len() as u64;
        w.write_all(line.as_bytes()).unwrap();
        i += 1;
    }
    w.flush().unwrap();
    (f.path().to_path_buf(), Some(f))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing: run in release with --ignored"]
async fn perf_time_to_first_frame() {
    let (path, _guard) = perf_file();
    let path = path.to_str().unwrap().to_owned();
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &["-y", path.as_str()],
    );
    let mut tui = tui(120, 40);
    app.area = Rect::new(0, 0, 120, 40);
    let started = Instant::now();
    app.start();
    // Until the first frame that shows data rows (the file is opened and
    // sniffed; the index is still building).
    loop {
        app.step(&mut tui).await.unwrap();
        let shown = app
            .state
            .active_tab()
            .and_then(|t| t.loaded.as_ref())
            .is_some_and(|l| l.frame.rows.iter().any(Option::is_some));
        if shown {
            app.render(&mut tui).unwrap();
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no first frame"
        );
    }
    let first = started.elapsed();
    let indexing = app
        .state
        .active_tab()
        .and_then(|t| t.loaded.as_ref())
        .is_some_and(|l| l.index_summary.is_none());
    println!(
        "time to first frame: {first:?} (target < {FIRST_FRAME_TARGET:?}); index still building: {indexing}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing: run in release with --ignored"]
async fn perf_scroll_latency() {
    let (path, _guard) = perf_file();
    let path = path.to_str().unwrap().to_owned();
    let mut app = app_files(
        Config::embedded(),
        ColorSupport::TrueColor,
        &["-y", path.as_str()],
    );
    let mut tui = tui(120, 40);
    app.area = Rect::new(0, 0, 120, 40);
    app.start();
    settle(&mut app, &mut tui).await;
    app.render(&mut tui).unwrap();
    // Warm the cache over the range scrolled.
    const MOVES: u32 = 1_000;
    let mut worst = Duration::ZERO;
    let started = Instant::now();
    for _ in 0..MOVES {
        let t = Instant::now();
        ch(&mut app, &mut tui, 'j');
        app.render(&mut tui).unwrap();
        worst = worst.max(t.elapsed());
    }
    let mean = started.elapsed() / MOVES;
    println!(
        "scroll: {MOVES} moves, mean {mean:?}, worst {worst:?} per frame (target < {FRAME_TARGET:?})"
    );
    assert_eq!(tab(&app).cursor_row, u64::from(MOVES));
}
