//! §17 resident memory (M7-05): after opening and fully indexing a large
//! file, the process's own memory stays under 150 MB + the index.
//!
//! `VmRSS` also counts the page-cache pages of the mmapped file that the
//! scan touched (`RssFile`); the kernel reclaims those on demand and they
//! grow with the file, not with tachy. The target applies to the anonymous
//! part (`RssAnon`: heap, stacks), which is what this test asserts. Both are
//! printed.
//!
//! ```sh
//! TACHY_PERF_FILE=target/bench-data/x.csv \
//!   cargo test --release -p tachy-core --test memory -- --ignored --nocapture
//! ```
//!
//! Without `TACHY_PERF_FILE`, a 1 GiB file is generated in the temp dir.
#![cfg(target_os = "linux")]

use std::{
    io::{BufWriter, Write},
    path::PathBuf,
    sync::Arc,
};

use tachy_core::{
    dialect::{DialectOverrides, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    source::Source,
};
use tokio_util::sync::CancellationToken;

/// `name:` from /proc/self/status, in bytes.
fn status_kib(name: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn perf_file() -> (PathBuf, Option<tempfile::NamedTempFile>) {
    if let Some(p) = std::env::var_os("TACHY_PERF_FILE") {
        return (PathBuf::from(p), None);
    }
    let f = tempfile::NamedTempFile::new().unwrap();
    let mut w = BufWriter::with_capacity(8 << 20, f.reopen().unwrap());
    writeln!(w, "id,price,country,status,notes").unwrap();
    let (mut written, mut i) = (0u64, 0u64);
    while written < 1 << 30 {
        let line = format!("{i},{}.5,C{},s{},\"a, b\"\n", i % 9973, i % 30, i % 6);
        written += line.len() as u64;
        w.write_all(line.as_bytes()).unwrap();
        i += 1;
    }
    w.flush().unwrap();
    (f.path().to_path_buf(), Some(f))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "memory: run in release with --ignored"]
async fn resident_memory_after_indexing() {
    let (path, _guard) = perf_file();
    let anon_before = status_kib("RssAnon:");
    let src = Source::open(&path, None).unwrap();
    let report = sniff(src.bytes(), 65_536, &DialectOverrides::default());
    let src = Arc::new(src.with_dialect(report.dialect));
    let index = Arc::new(RowIndex::for_source(&src));
    let summary = build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report,
        Executor::new(0),
        CancellationToken::new(),
        IndexOptions::default(),
    )
    .await
    .unwrap();
    let anon = status_kib("RssAnon:");
    let file = status_kib("RssFile:");
    let index_bytes = index.memory_bytes();
    let mb = |b: u64| b as f64 / 1e6;
    println!(
        "{} rows, {:.0} MB file: RssAnon {:.1} MB (before {:.1} MB), RssFile {:.1} MB, index {:.2} MB",
        summary.total_rows,
        mb(src.len()),
        mb(anon),
        mb(anon_before),
        mb(file),
        mb(index_bytes),
    );
    let limit = 150_000_000 + index_bytes;
    assert!(
        anon < limit,
        "RssAnon {:.1} MB over the §17 target {:.1} MB",
        mb(anon),
        mb(limit)
    );
}
