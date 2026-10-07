//! Sort throughput (M5-03, §17: 400M rows, one numeric key, < 3 min on the
//! reference machine; this bench runs 10M rows and extrapolates).
//!
//! `cargo bench -p tachy-core --bench sort`
//!
//! Rows: `TACHY_BENCH_ROWS` (default 10M) of `id,value,name`. Two variants:
//! `in_memory` (1.5 GiB RAM cap, no run files) and `external_64m` (64 MiB
//! cap: runs and a multi-way merge). Threads: `TACHY_BENCH_THREADS`
//! (default: all CPUs).

use std::{
    hint::black_box,
    io::{BufWriter, Write},
    sync::Arc,
    time::Duration,
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tachy_core::{
    column::ColumnMeta,
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    jobs::{JobControl, SortProgress},
    sort::{SortJob, SortKey, SortOptions, run_sort},
    source::Source,
    types::{ColType, NullSet},
    view::View,
};
use tokio_util::sync::CancellationToken;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn generate(rows: usize) -> tempfile::NamedTempFile {
    let f = tempfile::Builder::new()
        .prefix("tachy-bench-")
        .suffix(".csv")
        .tempfile()
        .unwrap();
    let mut w = BufWriter::with_capacity(1 << 20, f.as_file());
    w.write_all(b"id,value,name\n").unwrap();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for i in 0..rows {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        writeln!(
            w,
            "{i},{},name{}",
            (x % 2_000_000_000) as i64 - 1_000_000_000,
            x % 1000
        )
        .unwrap();
    }
    w.flush().unwrap();
    drop(w);
    f
}

fn bench_sort(c: &mut Criterion) {
    let rows = env_usize("TACHY_BENCH_ROWS", 10_000_000);
    let threads = env_usize("TACHY_BENCH_THREADS", 0);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let file = generate(rows);
    let raw = Source::open(file.path(), None).unwrap();
    let report = sniff(
        raw.bytes(),
        DEFAULT_SAMPLE_BYTES,
        &DialectOverrides::default(),
    );
    let src = Arc::new(raw.with_dialect(report.dialect));
    let exec = Executor::with_handle(rt.handle().clone(), threads);
    let index = Arc::new(RowIndex::for_source(&src));
    rt.block_on(build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report,
        exec.clone(),
        CancellationToken::new(),
        IndexOptions::default(),
    ))
    .unwrap();
    let columns: Vec<ColumnMeta> = src
        .column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let mut c = ColumnMeta::new(n, i, false);
            c.set_inferred([ColType::I64, ColType::I64, ColType::Str][i]);
            c
        })
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let mut group = c.benchmark_group("sort");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(rows as u64));
    for (name, ram_cap) in [("in_memory", 1536u64 << 20), ("external_64m", 64 << 20)] {
        group.bench_function(BenchmarkId::new(name, rows), |b| {
            b.iter(|| {
                let job = SortJob {
                    src: Arc::clone(&src),
                    index: Arc::clone(&index),
                    parent: View::All,
                    keys: vec![SortKey::asc(1)],
                    columns: columns.clone(),
                    nulls: NullSet::default(),
                    ram_cap,
                    tmp_dir: tmp.path().to_path_buf(),
                    options: SortOptions::default(),
                };
                let list = rt
                    .block_on(run_sort(
                        job,
                        &exec,
                        &JobControl::default(),
                        Arc::new(SortProgress::default()),
                    ))
                    .unwrap();
                black_box(list.len())
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sort);
criterion_main!(benches);
