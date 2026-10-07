//! Indexer throughput (§17: ≥ 3 GB/s without quoted newlines, ≥ 1 GB/s with
//! them, 8-core NVMe, warm page cache) and random-jump latency (< 5 ms).
//!
//! `cargo bench -p tachy-core --bench index`
//!
//! The generated files live in the system temp dir. Their size is
//! `TACHY_BENCH_MB` MiB (default 512); thread count `TACHY_BENCH_THREADS`
//! (default: all CPUs). A random jump costs one checkpoint lookup plus at
//! most 1,023 record skips, independent of the file size, so the jump bench
//! uses the same file.

use std::{
    hint::black_box,
    io::{BufWriter, Write},
    sync::Arc,
    time::Duration,
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tachy_core::{
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    parse::{RecordParser, RecordRanges},
    source::Source,
};
use tokio_util::sync::CancellationToken;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// ~100-byte rows, 12 columns, some quoted fields; one in 50 rows has a
/// quoted newline when `quoted_newlines`.
fn generate(bytes: usize, quoted_newlines: bool) -> tempfile::NamedTempFile {
    let f = tempfile::Builder::new()
        .prefix("tachy-bench-")
        .suffix(".csv")
        .tempfile()
        .unwrap();
    let mut w = BufWriter::with_capacity(1 << 20, f.as_file());
    let mut written = 0usize;
    let header = b"id,ts,user,country,amount,qty,flag,category,score,lat,lon,note\n";
    w.write_all(header).unwrap();
    written += header.len();
    let mut i = 0u64;
    let mut line = String::with_capacity(256);
    while written < bytes {
        use std::fmt::Write as _;
        line.clear();
        let note = if quoted_newlines && i.is_multiple_of(50) {
            "\"first line\nsecond, line\""
        } else if i.is_multiple_of(7) {
            "\"quoted, with comma\""
        } else {
            "plain"
        };
        writeln!(
            line,
            "{i},2024-03-{:02}T12:{:02}:00,user{},{},{}.{:02},{},{},cat{},{},{}.123,{}.456,{note}",
            i % 28 + 1,
            i % 60,
            i % 10_000,
            ["FR", "DE", "US", "JP"][(i % 4) as usize],
            i % 1000,
            i % 100,
            i % 17,
            i.is_multiple_of(2),
            i % 9,
            i % 101,
            i % 90,
            i % 180,
        )
        .unwrap();
        w.write_all(line.as_bytes()).unwrap();
        written += line.len();
        i += 1;
    }
    w.flush().unwrap();
    drop(w);
    f
}

fn open(path: &std::path::Path) -> (Arc<Source>, tachy_core::dialect::SniffReport) {
    let raw = Source::open(path, None).unwrap();
    let report = sniff(
        raw.bytes(),
        DEFAULT_SAMPLE_BYTES,
        &DialectOverrides::default(),
    );
    (Arc::new(raw.with_dialect(report.dialect)), report)
}

fn bench_index(c: &mut Criterion) {
    let mb = env_usize("TACHY_BENCH_MB", 512);
    let threads = env_usize("TACHY_BENCH_THREADS", 0);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("index");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    for (name, quoted, count_ragged) in [
        ("fast", false, true),
        ("fast_no_ragged", false, false),
        ("slow", true, true),
    ] {
        let file = generate(mb << 20, quoted);
        let (src, report) = open(file.path());
        // Warm the page cache.
        black_box(
            src.bytes()
                .iter()
                .step_by(4096)
                .map(|&b| b as u64)
                .sum::<u64>(),
        );
        group.throughput(Throughput::Bytes(src.len()));
        group.bench_function(BenchmarkId::new(name, format!("{mb}MiB")), |b| {
            b.iter(|| {
                rt.block_on(async {
                    let index = Arc::new(RowIndex::for_source(&src));
                    build_index(
                        Arc::clone(&src),
                        Arc::clone(&index),
                        report.clone(),
                        Executor::new(threads),
                        CancellationToken::new(),
                        IndexOptions {
                            count_ragged,
                            ..IndexOptions::default()
                        },
                    )
                    .await
                    .unwrap()
                })
            })
        });
    }
    group.finish();
}

fn bench_jump(c: &mut Criterion) {
    let mb = env_usize("TACHY_BENCH_MB", 512);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let file = generate(mb << 20, false);
    let (src, report) = open(file.path());
    let index = Arc::new(RowIndex::for_source(&src));
    let summary = rt
        .block_on(build_index(
            Arc::clone(&src),
            Arc::clone(&index),
            report,
            Executor::with_handle(rt.handle().clone(), 0),
            CancellationToken::new(),
            IndexOptions::default(),
        ))
        .unwrap();
    let rows = summary.total_rows;
    let mut group = c.benchmark_group("jump");
    let mut p = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    group.bench_function(BenchmarkId::new("random_row", format!("{mb}MiB")), |b| {
        b.iter(|| {
            // xorshift: a different row every time, worst-case skips included.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let row = x % rows;
            let offset = index.offset_of(row, &src, &mut p).unwrap();
            p.parse_at(src.bytes(), offset, &mut rec);
            black_box(rec.fields.len())
        })
    });
    // The worst case: the last row before a checkpoint (1,023 skips).
    group.bench_function(
        BenchmarkId::new("worst_case_row", format!("{mb}MiB")),
        |b| {
            let row = (rows / 1024 / 2) * 1024 + 1023;
            b.iter(|| black_box(index.offset_of(black_box(row), &src, &mut p)))
        },
    );
    group.finish();
}

criterion_group!(benches, bench_index, bench_jump);
criterion_main!(benches);
