//! M3-01 / M3-02: sample collection (phase 1 head, phase 2 spread) and
//! recomputing stats after `set type` without file access.

use std::{io::Write, sync::Arc};

use tachy_core::{
    column::{ColumnMeta, ColumnName},
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    sample::{HEAD_ROWS, SamplePhase, SampleResult, sample_head, sample_spread},
    source::Source,
    stats::StatsSource,
    types::{ColType, NullSet},
};
use tokio_util::sync::CancellationToken;

fn write_file(content: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
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

/// `id,flag,price,day,city,note`: i64, bool, f64, date, enum, str.
fn generated(rows: usize) -> String {
    let mut s = String::from("id,flag,price,day,city,note\n");
    for i in 0..rows {
        s.push_str(&format!(
            "{i},{},{}.5,2024-01-{:02},{},note {i}\n",
            if i % 2 == 0 { "true" } else { "false" },
            i % 1000,
            i % 28 + 1,
            ["Paris", "Berlin", "Rome"][i % 3],
        ));
    }
    s
}

fn types(r: &SampleResult) -> Vec<ColType> {
    r.per_column.iter().map(|c| c.inferred).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_sample_infers_types_and_stats() {
    let f = write_file(&generated(500));
    let (src, _) = open(f.path());
    let exec = Executor::new(2);
    let r = sample_head(src, &exec, NullSet::default(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.phase, SamplePhase::Head);
    assert_eq!(r.rows_sampled, 500);
    assert!(r.reached_eof);
    assert_eq!(
        types(&r),
        [
            ColType::I64,
            ColType::Bool,
            ColType::F64,
            ColType::Date,
            ColType::Enum,
            ColType::Str
        ]
    );
    let price = &r.per_column[2];
    assert_eq!(price.stats.rows_seen, 500);
    assert_eq!(price.stats.source, StatsSource::Sample { rows: 500 });
    let n = price.stats.numeric.as_ref().unwrap();
    assert_eq!(n.min().unwrap().as_f64(), 0.5);
    assert_eq!(n.max().unwrap().as_f64(), 499.5);
    assert_eq!(r.per_column[4].stats.distinct.estimate(), 3);
    assert_eq!(r.per_column[4].widths.total(), 500);
    assert_eq!(r.per_column[4].widths.p95(), Some(6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spread_sample_adds_rows_across_the_file() {
    let rows = 50_000;
    let f = write_file(&generated(rows));
    let (src, report) = open(f.path());
    let exec = Executor::new(4);
    let head = sample_head(
        Arc::clone(&src),
        &exec,
        NullSet::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(head.rows_sampled, HEAD_ROWS);
    assert!(!head.reached_eof);
    // The head never sees ids ≥ 10,000.
    let max_id = |r: &SampleResult| {
        r.per_column[0]
            .values
            .iter()
            .map(|v| {
                std::str::from_utf8(v.unwrap())
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .max()
            .unwrap()
    };
    assert_eq!(max_id(&head), HEAD_ROWS - 1);

    let index = Arc::new(RowIndex::for_source(&src));
    build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report,
        exec.clone(),
        CancellationToken::new(),
        IndexOptions::default(),
    )
    .await
    .unwrap();
    let spread = sample_spread(
        Arc::clone(&src),
        index,
        &exec,
        NullSet::default(),
        Arc::new(head),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(spread.phase, SamplePhase::Spread);
    assert_eq!(spread.rows_sampled, 20_000);
    assert_eq!(spread.per_column[2].stats.rows_seen, 20_000);
    assert!(max_id(&spread) > 45_000);
    // Every sampled id is distinct (no row counted twice).
    let mut ids: Vec<&[u8]> = spread.per_column[0]
        .values
        .iter()
        .map(Option::unwrap)
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 20_000);
    assert_eq!(
        types(&spread)[..4],
        [ColType::I64, ColType::Bool, ColType::F64, ColType::Date]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ragged_rows_add_columns_and_missing_fields_are_null() {
    let f = write_file("a,b\n1,x\n2\n3,y,extra\n");
    let (src, _) = open(f.path());
    let exec = Executor::new(1);
    let r = sample_head(src, &exec, NullSet::new(["-"]), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.per_column.len(), 3);
    assert_eq!(r.per_column[1].values.get(1), None);
    // Missing counts as null even though "" is not a null spelling here.
    assert_eq!(r.per_column[1].stats.nulls, 1);
    assert_eq!(r.per_column[2].stats.nulls, 2);
    assert_eq!(r.per_column[2].values.get(2), Some(&b"extra"[..]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_sample() {
    let f = write_file(&generated(100));
    let (src, _) = open(f.path());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let r = sample_head(src, &Executor::new(1), NullSet::default(), cancel).await;
    assert!(matches!(r, Err(tachy_core::Error::Cancelled)));
}

/// M3-02 acceptance: recomputing after `set type` needs no file access.
///
/// `Source` is a concrete type, so instead of a `Source` that panics on
/// access the test removes every way to reach the file: the `Source` (and
/// its mapping) is dropped and the file deleted before recomputing. A
/// `SampleResult` holds no reference to its `Source`, so this would not
/// compile otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recompute_after_set_type_needs_no_file_access() {
    let f = write_file("code,n\n001,1\n002,2\n010,x\n,3\n");
    let path = f.path().to_path_buf();
    let (src, _) = open(&path);
    let nulls = NullSet::default();
    let r = sample_head(
        Arc::clone(&src),
        &Executor::new(1),
        nulls.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let names = src.column_names();
    drop(src);
    drop(f);
    assert!(!path.exists());

    assert_eq!(r.per_column[0].inferred, ColType::I64);
    let as_str = r.stats_for(0, ColType::Str, &nulls).unwrap();
    assert_eq!(as_str.for_type, ColType::Str);
    assert!(as_str.numeric.is_none());
    assert_eq!(as_str.nulls, 1);
    assert_eq!(as_str.top5().len(), 3);

    let mut cols: Vec<ColumnMeta> = names
        .into_iter()
        .enumerate()
        .map(|(i, n): (usize, ColumnName)| ColumnMeta::new(n, i, false))
        .collect();
    cols[0].type_override = Some(ColType::Str);
    cols[1].type_override = Some(ColType::F64);
    r.apply(&mut cols, &nulls);
    assert_eq!(cols[0].inferred, ColType::I64);
    assert_eq!(cols[0].ty(), ColType::Str);
    assert!(!cols[0].stats_stale());
    assert_eq!(cols[0].stats.as_ref().unwrap().for_type, ColType::Str);
    // `n` infers str (25 % garbage); the override wins and stats follow it.
    assert_eq!(cols[1].inferred, ColType::Str);
    let n = cols[1].stats.as_ref().unwrap().numeric.as_ref().unwrap();
    assert_eq!(n.max().unwrap().as_f64(), 3.0);
    // Phase-2 style re-apply never clears the override.
    r.apply(&mut cols, &nulls);
    assert_eq!(cols[1].type_override, Some(ColType::F64));
}
