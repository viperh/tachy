//! Filter throughput.
//!
//! - `filter_eval_1_thread` (M4-02): `price > 100` at ≥ 300 MB/s per worker,
//!   so 8 workers reach the §17 target of 1.5 GB/s. Each iteration parses
//!   every record with `RecordParser` and evaluates the predicate, on one
//!   thread.
//! - `filter_job` (M4-04): the whole parallel job (`filter::run_filter_with`)
//!   on an indexed file with all available threads, with and without the
//!   `memmem` pre-filter. Target: ≥ 1.5 GB/s for simple comparisons (§17).
//!
//! `cargo bench -p tachy-core --bench filter`
//! The file is `TACHY_BENCH_MB` MiB
//! (default 256; the task's reference is 1 GB) of ~100-byte rows with 12
//! columns, in the system temp dir.

use std::{
    hint::black_box,
    io::{BufWriter, Write},
    time::Duration,
};

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tachy_core::{
    column::ColumnMeta,
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff},
    exec::Executor,
    filter::{FilterOptions, FilterOutput, ParentRows, run_filter_with},
    index::{IndexOptions, RowIndex, build_index},
    jobs::{FilterProgress, PauseToken},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    query::{self, EvalScratch, Predicate},
    source::Source,
    types::{ColType, NullSet},
    view::FilterRows,
};
use tokio_util::sync::CancellationToken;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn generate(bytes: usize) -> tempfile::NamedTempFile {
    let f = tempfile::Builder::new()
        .prefix("tachy-bench-")
        .suffix(".csv")
        .tempfile()
        .unwrap();
    let mut w = BufWriter::with_capacity(1 << 20, f.as_file());
    let header = b"id,ts,user,country,price,qty,flag,category,score,lat,lon,note\n";
    w.write_all(header).unwrap();
    let mut written = header.len();
    let mut line = String::with_capacity(256);
    let mut i = 0u64;
    while written < bytes {
        use std::fmt::Write as _;
        line.clear();
        writeln!(
            line,
            "{i},2024-03-{:02}T12:{:02}:00,user{},{},{}.{:02},{},{},cat{},{},{}.123,{}.456,{}",
            i % 28 + 1,
            i % 60,
            i % 10_000,
            ["FR", "DE", "US", "JP"][(i % 4) as usize],
            (i * 7919) % 200,
            i % 100,
            i % 17,
            i.is_multiple_of(2),
            i % 9,
            i % 101,
            i % 90,
            i % 180,
            if i.is_multiple_of(7) {
                "\"quoted, with comma\""
            } else {
                "plain"
            },
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

fn compile(src: &Source, q: &str) -> Predicate {
    let names = src.column_names();
    let types = [
        ColType::I64,
        ColType::DateTime,
        ColType::Str,
        ColType::Enum,
        ColType::F64,
        ColType::I64,
        ColType::Bool,
        ColType::Enum,
        ColType::I64,
        ColType::F64,
        ColType::F64,
        ColType::Str,
    ];
    let cols: Vec<ColumnMeta> = names
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let mut c = ColumnMeta::new(n, i, false);
            c.set_inferred(types[i]);
            c
        })
        .collect();
    let ast = query::parse(q).unwrap();
    let names: Vec<_> = cols.iter().map(|c| c.name.clone()).collect();
    let resolved = query::resolve(ast, &names).unwrap();
    query::compile(&resolved, &cols, src.dialect(), &NullSet::default()).unwrap()
}

/// Parses and evaluates every record; returns the match count.
fn scan(src: &Source, p: &Predicate) -> u64 {
    let bytes = src.bytes();
    let mut parser = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = EvalScratch::new();
    let mut pos = src.data_start();
    let mut matches = 0;
    loop {
        match parser.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                matches += u64::from(p.eval_record(bytes, &rec, &mut scratch));
                pos = next;
            }
            ParseOutcome::Eof => return matches,
        }
    }
}

fn bench_filter(c: &mut Criterion) {
    let mb = env_usize("TACHY_BENCH_MB", 256);
    let file = generate(mb << 20);
    let raw = Source::open(file.path(), None).unwrap();
    let report = sniff(
        raw.bytes(),
        DEFAULT_SAMPLE_BYTES,
        &DialectOverrides::default(),
    );
    let src = raw.with_dialect(report.dialect);
    // Warm the page cache.
    black_box(
        src.bytes()
            .iter()
            .step_by(4096)
            .map(|&b| u64::from(b))
            .sum::<u64>(),
    );
    let mut group = c.benchmark_group("filter_eval_1_thread");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Bytes(src.len()));
    for (name, q) in [
        ("parse_only", "true"),
        ("price_gt_100", "price > 100"),
        (
            "and_country_price",
            "country == \"DE\" && price > 100 && note != \"x\"",
        ),
        ("contains_ci", "user contains \"ER12\"i"),
    ] {
        let p = compile(&src, q);
        group.bench_function(BenchmarkId::new(name, format!("{mb}MiB")), |b| {
            b.iter(|| black_box(scan(&src, &p)))
        });
    }
    group.finish();

    // The parallel job (M4-04).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let src = Arc::new(src);
    let exec = Executor::with_handle(rt.handle().clone(), 0);
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
    let mut group = c.benchmark_group(format!("filter_job_{}_threads", exec.threads()));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Bytes(src.len()));
    for (name, q, prefilter) in [
        ("price_gt_100", "price > 100", false),
        (
            "and_country_price",
            "country == \"DE\" && price > 100",
            false,
        ),
        ("contains_user123", "user contains \"user123\"", false),
        (
            "contains_user123_prefilter",
            "user contains \"user123\"",
            true,
        ),
    ] {
        let p = compile(&src, q);
        group.bench_function(BenchmarkId::new(name, format!("{mb}MiB")), |b| {
            b.iter(|| {
                let rows = Arc::new(FilterRows::new_growing());
                rt.block_on(run_filter_with(
                    ParentRows::All,
                    Arc::clone(&src),
                    Arc::clone(&index),
                    p.clone(),
                    FilterOutput::Bitmap(Arc::clone(&rows)),
                    exec.clone(),
                    CancellationToken::new(),
                    PauseToken::new(),
                    Arc::new(FilterProgress::default()),
                    FilterOptions {
                        prefilter,
                        ..FilterOptions::default()
                    },
                ))
                .unwrap();
                black_box(rows.len())
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_filter);
criterion_main!(benches);
