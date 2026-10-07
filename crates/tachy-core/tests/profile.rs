//! M5-04: the profile job.
//!
//! - Exactness against a naive single-threaded reference (exact sample-mode
//!   accumulators over every row).
//! - Identical results with 1 and 8 threads.
//! - Waiting for the index, cancellation, ragged rows.

mod support;

use std::{sync::Arc, time::Duration};

use support::{runtime, temp_source};
use tachy_core::{
    column::ColumnMeta,
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    jobs::{JobControl, RowsProgress},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    source::Source,
    stats::{
        Accumulate, ColumnStats, StatsMode, StatsSource,
        profile::{ProfileColumn, ProfileOptions, ProfileResult, run_profile},
    },
    types::{ColType, NullSet, Value, parse_value},
};
use tokio_util::sync::CancellationToken;

const TYPES: [ColType; 7] = [
    ColType::I64,
    ColType::F64,
    ColType::I64,
    ColType::Enum,
    ColType::Date,
    ColType::Str,
    ColType::Str,
];

fn generate(rows: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    let mut r = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut out = b"id,price,qty,country,day,name,note\n".to_vec();
    for i in 0..rows {
        let qty = match r() % 10 {
            0 => String::new(),
            1 => "NULL".to_owned(),
            2 => "n/a".to_owned(), // unparseable as i64
            _ => format!("{}", r() % 1000),
        };
        let country = ["FR", "DE", "US", "JP", "Ünïcødé", ""][(r() % 6) as usize];
        let line = format!(
            "{i},{}.{:02},{qty},{country},2024-{:02}-{:02},name{},\"note, {}\"\n",
            (r() % 100_000) as i64 - 50_000,
            r() % 100,
            r() % 12 + 1,
            r() % 28 + 1,
            r() % 20_000,
            "x".repeat((r() % 30) as usize),
        );
        out.extend_from_slice(line.as_bytes());
        if r() % 97 == 0 {
            // A short ragged row: missing cells count as nulls.
            out.extend_from_slice(format!("{},1.5\n", rows + i).as_bytes());
        }
    }
    out
}

fn columns(src: &Source) -> Vec<ColumnMeta> {
    src.column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let mut c = ColumnMeta::new(n, i, false);
            c.set_inferred(TYPES[i]);
            c
        })
        .collect()
}

struct Fx {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    cols: Vec<ColumnMeta>,
}

fn fixture(rows: usize, seed: u64) -> Fx {
    let content = generate(rows, seed);
    let report = sniff(&content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let (file, src) = temp_source(&content, report.dialect);
    let rt = runtime();
    let index = Arc::new(RowIndex::with_stride(src.data_start(), 32));
    rt.block_on(build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report,
        Executor::with_handle(rt.handle().clone(), 4),
        CancellationToken::new(),
        IndexOptions {
            chunk_size: 16 << 10,
            ..IndexOptions::default()
        },
    ))
    .unwrap();
    let cols = columns(&src);
    Fx {
        _file: file,
        src,
        index,
        cols,
    }
}

fn nulls() -> NullSet {
    NullSet::new(["", "NULL"])
}

/// Exact stats of every column, one pass, one thread.
fn reference(fx: &Fx) -> Vec<ColumnStats> {
    let bytes = fx.src.bytes();
    let mut p = RecordParser::new(fx.src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let n = nulls();
    let mut stats: Vec<ColumnStats> = fx
        .cols
        .iter()
        .map(|c| ColumnStats::new(c.ty(), StatsMode::Sample))
        .collect();
    let mut pos = fx.src.data_start();
    loop {
        match p.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                for (i, s) in stats.iter_mut().enumerate() {
                    if i < rec.fields.len() {
                        let v = p.field_value(bytes, &rec, i, &mut scratch);
                        let parsed = parse_value(fx.cols[i].ty(), v, &n);
                        s.push(v, &parsed);
                    } else {
                        s.push(&[], &Value::Null);
                    }
                }
                pos = next;
            }
            ParseOutcome::Eof => return stats,
        }
    }
}

fn profile(fx: &Fx, cols: Vec<ProfileColumn>, threads: usize, chunk: u64) -> ProfileResult {
    let rt = runtime();
    let progress = Arc::new(RowsProgress::default());
    let r = rt
        .block_on(run_profile(
            Arc::clone(&fx.src),
            Arc::clone(&fx.index),
            cols,
            nulls(),
            Executor::with_handle(rt.handle().clone(), threads),
            JobControl::new(CancellationToken::new()),
            Arc::clone(&progress),
            ProfileOptions {
                chunk_bytes: chunk,
                ..ProfileOptions::default()
            },
        ))
        .unwrap();
    let total = fx.index.total_rows().unwrap();
    assert_eq!(progress.done(), total);
    assert_eq!(progress.total(), total);
    assert_eq!(r.rows, total);
    r
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
fn exact_fields_match_the_reference() {
    let fx = fixture(30_000, 3);
    let want = reference(&fx);
    let r = profile(&fx, ProfileColumn::all(&fx.cols), 4, 32 << 10);
    assert_eq!(r.capacity, 1024);
    assert_eq!(r.columns.len(), 7, "every column in one pass");
    for (i, got) in &r.columns {
        let w = &want[*i];
        let name = &fx.cols[*i].name.display;
        assert_eq!(got.source, StatsSource::AllRows);
        assert_eq!(got.label(), "all rows");
        assert_eq!(got.rows_seen, w.rows_seen, "{name}");
        assert_eq!(got.nulls, w.nulls, "{name}");
        assert_eq!(got.null_percent(), w.null_percent(), "{name}");
        assert_eq!(got.max_width, w.max_width, "{name}");
        assert_eq!(got.for_type, fx.cols[*i].ty());
        match (&got.numeric, &w.numeric) {
            (Some(g), Some(w)) => {
                assert_eq!(g.min(), w.min(), "{name}");
                assert_eq!(g.max(), w.max(), "{name}");
                assert!(close(g.mean().unwrap(), w.mean().unwrap()), "{name}");
                assert!(g.quantiles_approximate(), "p50/p95 come from KLL");
                let (gp, wp) = (g.p50().unwrap(), w.p50().unwrap());
                let span = w.max().unwrap().as_f64() - w.min().unwrap().as_f64();
                assert!((gp - wp).abs() <= 0.05 * span, "{name} p50 {gp} vs {wp}");
            }
            (None, None) => {}
            _ => panic!("{name}: numeric stats mismatch"),
        }
        match (&got.temporal, &w.temporal) {
            (Some(g), Some(w)) => assert_eq!(g, w, "{name}"),
            (None, None) => {}
            _ => panic!("{name}: temporal stats mismatch"),
        }
        // Distinct stays an HLL estimate (shown with `~`).
        let (gd, wd) = (got.distinct.estimate(), w.distinct.estimate());
        assert!(gd.abs_diff(wd) * 50 <= wd.max(1), "{name}: {gd} vs {wd}");
        if !got.top.is_approximate() {
            assert_eq!(got.top5(), w.top5(), "{name}: low cardinality is exact");
        }
    }
    // Low-cardinality columns: exact top values.
    let country = &r.columns[3].1;
    assert!(!country.top.is_approximate());
    assert_eq!(country.top5(), want[3].top5());
    assert_eq!(country.other(), want[3].other());
    // High-cardinality `id`: approximate counts.
    assert!(r.columns[0].1.top.is_approximate());
}

#[test]
fn one_thread_equals_eight_threads() {
    let fx = fixture(25_000, 9);
    let cols = ProfileColumn::all(&fx.cols);
    let a = profile(&fx, cols.clone(), 1, 16 << 10);
    let b = profile(&fx, cols, 8, 16 << 10);
    for ((ia, sa), (ib, sb)) in a.columns.iter().zip(&b.columns) {
        assert_eq!(ia, ib);
        assert_eq!(sa.rows_seen, sb.rows_seen);
        assert_eq!(sa.nulls, sb.nulls);
        assert_eq!(sa.max_width, sb.max_width);
        assert_eq!(sa.numeric, sb.numeric, "min/max/mean and the sketch");
        assert_eq!(sa.temporal, sb.temporal);
        assert_eq!(sa.top, sb.top);
        assert_eq!(sa.distinct.estimate(), sb.distinct.estimate());
        if let (Some(x), Some(y)) = (&sa.numeric, &sb.numeric) {
            assert_eq!(x.mean().map(f64::to_bits), y.mean().map(f64::to_bits));
        }
    }
}

#[test]
fn single_column_and_apply() {
    let fx = fixture(5_000, 5);
    let mut cols = fx.cols.clone();
    let sample = ColumnStats::new(ColType::F64, StatsMode::Sample);
    cols[1].stats = Some(sample);
    let r = profile(&fx, vec![ProfileColumn::from_meta(1, &cols[1])], 3, 8 << 10);
    assert_eq!(r.columns.len(), 1);
    r.apply(&mut cols);
    assert_eq!(cols[1].stats.as_ref().unwrap().label(), "all rows");
    assert_eq!(
        cols[1].stats.as_ref().unwrap().rows_seen,
        fx.index.total_rows().unwrap()
    );
    // A type override since the job started: the stats are not applied.
    let mut cols2 = fx.cols.clone();
    cols2[1].type_override = Some(ColType::Str);
    r.apply(&mut cols2);
    assert!(cols2[1].stats.is_none());
}

#[test]
fn waits_for_the_index_and_cancels() {
    let content = generate(8_000, 21);
    let report = sniff(&content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let (_f, src) = temp_source(&content, report.dialect);
    let cols = columns(&src);
    let rt = runtime();

    // Cancelled while waiting for the index.
    let index = Arc::new(RowIndex::for_source(&src));
    let cancel = CancellationToken::new();
    let r = rt.block_on(async {
        let job = tokio::spawn(run_profile(
            Arc::clone(&src),
            Arc::clone(&index),
            ProfileColumn::all(&cols),
            nulls(),
            Executor::with_handle(rt.handle().clone(), 2),
            JobControl::new(cancel.clone()),
            Arc::new(RowsProgress::default()),
            ProfileOptions {
                poll: Duration::from_millis(5),
                ..ProfileOptions::default()
            },
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!job.is_finished(), "waits for the index");
        cancel.cancel();
        job.await.unwrap()
    });
    assert!(r.unwrap_err().is_cancelled());

    // Started before the index is built: completes once it is.
    let progress = Arc::new(RowsProgress::default());
    let r = rt.block_on(async {
        let exec = Executor::with_handle(rt.handle().clone(), 2);
        let job = tokio::spawn(run_profile(
            Arc::clone(&src),
            Arc::clone(&index),
            ProfileColumn::all(&cols),
            nulls(),
            exec.clone(),
            JobControl::new(CancellationToken::new()),
            Arc::clone(&progress),
            ProfileOptions {
                poll: Duration::from_millis(5),
                chunk_bytes: 4096,
                ..ProfileOptions::default()
            },
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(progress.total(), 0, "total unknown while waiting");
        build_index(
            Arc::clone(&src),
            Arc::clone(&index),
            report,
            exec,
            CancellationToken::new(),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        job.await.unwrap()
    });
    let r = r.unwrap();
    assert_eq!(r.rows, index.total_rows().unwrap());
    assert_eq!(progress.done(), r.rows);

    // Cancelled while running (a paused job, then cancel).
    let ctl = JobControl::new(CancellationToken::new());
    ctl.pause.pause();
    let r = rt.block_on(async {
        let job = tokio::spawn(run_profile(
            Arc::clone(&src),
            Arc::clone(&index),
            ProfileColumn::all(&cols),
            nulls(),
            Executor::with_handle(rt.handle().clone(), 2),
            ctl.clone(),
            Arc::new(RowsProgress::default()),
            ProfileOptions {
                chunk_bytes: 4096,
                ..ProfileOptions::default()
            },
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!job.is_finished(), "paused");
        ctl.cancel();
        job.await.unwrap()
    });
    assert!(r.unwrap_err().is_cancelled());
}
