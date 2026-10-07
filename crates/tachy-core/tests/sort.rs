//! M5-03: external merge sort.
//!
//! - `SortSpec::parse`.
//! - Acceptance cases (numeric order, descending, nulls last, stability,
//!   long shared prefixes, `ci`, filtered and ordered parents, multi-pass
//!   merges, cancellation cleanup, unwritable temp dir).
//! - Differential test against an in-memory `sort_by` reference with a tiny
//!   memory budget that forces many runs and several merge passes.

use std::{cmp::Ordering, io::Write, path::Path, sync::Arc, time::Duration};

use proptest::prelude::*;
use roaring::RoaringTreemap;
use tachy_core::{
    column::{ColumnMeta, ColumnName},
    dialect::{DEFAULT_SAMPLE_BYTES, DialectOverrides, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    jobs::{JobControl, JobError, SortPhase, SortProgress},
    sort::{
        SortJob, SortKey, SortOptions, SortSpec, estimate, header_indicator, merge::merge_passes,
        run_sort,
    },
    source::Source,
    types::{ColType, NullSet},
    view::{FilterRows, OrderedKind, RowIdList, View},
};
use tokio_util::sync::CancellationToken;

fn columns(spec: &[(&str, ColType)]) -> Vec<ColumnMeta> {
    spec.iter()
        .enumerate()
        .map(|(i, (n, t))| {
            let mut c = ColumnMeta::new(
                ColumnName {
                    display: (*n).to_owned(),
                    query: (*n).to_owned(),
                },
                i,
                false,
            );
            c.set_inferred(*t);
            c
        })
        .collect()
}

struct Fixture {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    tmp: tempfile::TempDir,
}

async fn fixture(content: &str, exec: &Executor) -> Fixture {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let raw = Source::open(f.path(), None).unwrap();
    let report = sniff(
        raw.bytes(),
        DEFAULT_SAMPLE_BYTES,
        &DialectOverrides {
            delimiter: Some(b','),
            header: Some(true),
            ..DialectOverrides::default()
        },
    );
    let src = Arc::new(raw.with_dialect(report.dialect));
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
    Fixture {
        _file: f,
        src,
        index,
        tmp: tempfile::tempdir().unwrap(),
    }
}

fn job(fx: &Fixture, cols: &[ColumnMeta], keys: Vec<SortKey>, parent: View) -> SortJob {
    SortJob {
        src: Arc::clone(&fx.src),
        index: Arc::clone(&fx.index),
        parent,
        keys,
        columns: cols.to_vec(),
        nulls: NullSet::default(),
        ram_cap: 64 << 20,
        tmp_dir: fx.tmp.path().to_path_buf(),
        options: SortOptions::default(),
    }
}

async fn run(job: SortJob, exec: &Executor) -> Vec<u64> {
    let list = run_sort(
        job,
        exec,
        &JobControl::default(),
        Arc::new(SortProgress::default()),
    )
    .await
    .unwrap();
    assert!(!list.is_growing());
    list.read(0, list.len() as usize)
}

fn tmp_entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// SortSpec::parse
// ---------------------------------------------------------------------------

#[test]
fn spec_parse() {
    let cols = columns(&[
        ("price", ColType::F64),
        ("ts", ColType::DateTime),
        ("order id", ColType::Str),
        ("in", ColType::Enum),
    ]);
    let s = SortSpec::parse("price:desc, ts", &cols).unwrap();
    assert_eq!(s.keys, vec![SortKey::desc(0), SortKey::asc(1)]);
    let s = SortSpec::parse(" `order id` : asc : ci ,$4:desc:ci,$2", &cols).unwrap();
    assert_eq!(
        s.keys,
        vec![
            SortKey {
                column: 2,
                descending: false,
                ci: true
            },
            SortKey {
                column: 3,
                descending: true,
                ci: true
            },
            SortKey::asc(1),
        ]
    );
    assert_eq!(s.display(&cols), "`order id`:asc:ci, `in`:desc:ci, ts");
    let s = SortSpec::parse("`a:b`", &columns(&[("a:b", ColType::Str)])).unwrap();
    assert_eq!(s.keys, vec![SortKey::asc(0)]);

    let err = |q: &str| SortSpec::parse(q, &cols).unwrap_err();
    let e = err("price:down");
    assert_eq!(e.span, 6..10);
    assert!(e.message.contains("asc, desc or ci"), "{}", e.message);
    let e = err("prce");
    assert_eq!(e.span, 0..4);
    assert!(
        e.message.contains("did you mean \"price\""),
        "{}",
        e.message
    );
    let e = err("ts, price:ci");
    assert_eq!(e.span, 10..12);
    assert!(e.message.contains("ci only applies"), "{}", e.message);
    assert_eq!(err("$9").span, 0..2);
    assert_eq!(err("").message, "expected a column");
    assert_eq!(err("price,").message, "expected a column");
    assert!(err("in").message.contains("keyword"));
    assert!(err("price:").message.contains("after :"));
    assert!(err("price:desc:asc").message.contains("asc, desc or ci"));
    assert!(err("price ts").message.contains("one column"));
}

#[test]
fn indicators_and_estimate() {
    let keys = [SortKey::asc(3)];
    assert_eq!(header_indicator(&keys, 3).as_deref(), Some("▲"));
    assert_eq!(header_indicator(&keys, 1), None);
    let keys = [SortKey::desc(1), SortKey::asc(0)];
    assert_eq!(header_indicator(&keys, 1).as_deref(), Some("▼1"));
    assert_eq!(header_indicator(&keys, 0).as_deref(), Some("▲2"));
    let e = estimate(2_000_000_000, &keys, 2_800_000_000);
    assert_eq!(
        e.text(),
        "est. 64.0 GB on disk → external merge sort, ~2.1 GB RAM cap"
    );
}

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

const BASIC: &str = "n,s\n10,b\n2,a\n,c\nx,d\n2,e\n-5,f\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn numeric_sort_asc_and_desc_with_nulls_last_and_stable() {
    let exec = Executor::new(4);
    let fx = fixture(BASIC, &exec).await;
    let cols = columns(&[("n", ColType::I64), ("s", ColType::Str)]);
    // 2 < 10 numerically; nulls/unparseable last; equal keys in file order.
    let asc = run(job(&fx, &cols, vec![SortKey::asc(0)], View::All), &exec).await;
    assert_eq!(asc, vec![5, 1, 4, 0, 2, 3]);
    let desc = run(job(&fx, &cols, vec![SortKey::desc(0)], View::All), &exec).await;
    assert_eq!(desc, vec![0, 1, 4, 5, 2, 3]);
    // Lexicographic would put "10" before "2".
    let cols_str = columns(&[("n", ColType::Str), ("s", ColType::Str)]);
    let lex = run(job(&fx, &cols_str, vec![SortKey::asc(0)], View::All), &exec).await;
    assert_eq!(lex, vec![5, 0, 1, 4, 3, 2]);
    assert!(
        tmp_entries(fx.tmp.path()).is_empty(),
        "list dropped, job dir gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_shared_prefixes_and_case_folding() {
    let exec = Executor::new(4);
    let content = "s\nabcdefghijklmnopqrZ\nabcdefghijklmnopqrA\nabcdefghijklmnop\ncherry\nBanana\napple\nabcdefghijklmn\nabcdefghijklmn\n";
    let fx = fixture(content, &exec).await;
    let cols = columns(&[("s", ColType::Str)]);
    let asc = run(job(&fx, &cols, vec![SortKey::asc(0)], View::All), &exec).await;
    assert_eq!(asc, vec![4, 6, 7, 2, 1, 0, 5, 3]);
    let spec = SortSpec::parse("s:asc:ci", &cols).unwrap();
    let ci = run(job(&fx, &cols, spec.keys, View::All), &exec).await;
    // apple, Banana, cherry, case-insensitively, among the rest.
    let pos = |id: u64| ci.iter().position(|&x| x == id).unwrap();
    assert!(pos(5) < pos(4) && pos(4) < pos(3));
    let spec = SortSpec::parse("s:desc", &cols).unwrap();
    let desc = run(job(&fx, &cols, spec.keys, View::All), &exec).await;
    assert_eq!(desc, vec![3, 5, 0, 1, 2, 6, 7, 4]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_key_and_non_all_parents() {
    let exec = Executor::new(4);
    let fx = fixture("a,b\n1,z\n0,y\n1,a\n0,b\n1,a\n", &exec).await;
    let cols = columns(&[("a", ColType::I64), ("b", ColType::Str)]);
    let keys = vec![SortKey::desc(0), SortKey::asc(1)];
    let all = run(job(&fx, &cols, keys.clone(), View::All), &exec).await;
    assert_eq!(all, vec![2, 4, 0, 3, 1]);
    // A filtered parent: only its rows.
    let filtered = View::Filtered {
        rows: Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter([
            0u64, 1, 4,
        ]))),
        expr: "x".into(),
        columns: vec![],
    };
    let out = run(job(&fx, &cols, keys.clone(), filtered), &exec).await;
    assert_eq!(out, vec![4, 0, 1]);
    // An ordered parent: same rows, its order doesn't matter (stable by row id).
    let ordered = View::Ordered {
        list: RowIdList::from_ids(fx.tmp.path(), &[3, 1, 2]).unwrap(),
        kind: OrderedKind::Sorted { keys: vec![] },
    };
    let out = run(job(&fx, &cols, vec![SortKey::asc(0)], ordered), &exec).await;
    assert_eq!(out, vec![1, 3, 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waits_for_a_growing_parent() {
    let exec = Executor::new(2);
    let fx = fixture(BASIC, &exec).await;
    let cols = columns(&[("n", ColType::I64), ("s", ColType::Str)]);
    let rows = Arc::new(FilterRows::new_growing());
    rows.insert_many([0, 1]);
    let parent = View::Filtered {
        rows: Arc::clone(&rows),
        expr: "x".into(),
        columns: vec![],
    };
    let j = job(&fx, &cols, vec![SortKey::asc(0)], parent);
    let exec2 = exec.clone();
    let handle = tokio::spawn(async move { run(j, &exec2).await });
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!handle.is_finished());
    rows.insert_many([5]);
    rows.finish();
    assert_eq!(handle.await.unwrap(), vec![5, 1, 0]);
}

/// Many rows, a tiny buffer and fan-in 3: many runs, several passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_budget_spills_and_merges_in_several_passes() {
    let exec = Executor::new(4);
    let n = 20_000u64;
    let mut content = String::from("v\n");
    for i in 0..n {
        content.push_str(&format!("{}\n", (i * 7919) % 1000));
    }
    let fx = fixture(&content, &exec).await;
    let cols = columns(&[("v", ColType::I64)]);
    let mut j = job(&fx, &cols, vec![SortKey::asc(0)], View::All);
    j.ram_cap = 24 * 500;
    j.options.fan_in = Some(3);
    let progress = Arc::new(SortProgress::default());
    let list = run_sort(j, &exec, &JobControl::default(), Arc::clone(&progress))
        .await
        .unwrap();
    let (_, passes) = progress.pass();
    assert!(passes >= 3, "passes = {passes}");
    let got = list.read(0, n as usize);
    let mut want: Vec<u64> = (0..n).collect();
    want.sort_by_key(|&i| ((i * 7919) % 1000, i));
    assert_eq!(got, want);
    drop(list);
    assert!(tmp_entries(fx.tmp.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_mid_merge_leaves_no_files() {
    let exec = Executor::new(4);
    let n = 300_000u64;
    let mut content = String::from("v\n");
    for i in 0..n {
        content.push_str(&format!("{}\n", (i * 7919) % 100_003));
    }
    let fx = fixture(&content, &exec).await;
    let cols = columns(&[("v", ColType::I64)]);
    let mut j = job(&fx, &cols, vec![SortKey::asc(0)], View::All);
    j.ram_cap = 24 * 2_000;
    j.options.fan_in = Some(2);
    let ctl = JobControl::new(CancellationToken::new());
    let progress = Arc::new(SortProgress::default());
    let (ctl2, progress2, exec2) = (ctl.clone(), Arc::clone(&progress), exec.clone());
    let handle = tokio::spawn(async move { run_sort(j, &exec2, &ctl2, progress2).await });
    while progress.phase() != SortPhase::Merge {
        assert!(!handle.is_finished(), "finished before merging");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // Pause first (workers park), then kill: cancel must wake them.
    ctl.pause.pause();
    tokio::time::sleep(Duration::from_millis(20)).await;
    ctl.cancel();
    let r = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancel wakes paused workers")
        .unwrap();
    assert!(matches!(r, Err(JobError::Cancelled)), "{r:?}");
    assert!(
        tmp_entries(fx.tmp.path()).is_empty(),
        "{:?}",
        tmp_entries(fx.tmp.path())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_temp_dir_fails_cleanly() {
    let exec = Executor::new(2);
    let fx = fixture(BASIC, &exec).await;
    let cols = columns(&[("n", ColType::I64), ("s", ColType::Str)]);
    let mut j = job(&fx, &cols, vec![SortKey::asc(0)], View::All);
    j.tmp_dir = fx.tmp.path().join("missing");
    let r = run_sort(
        j,
        &exec,
        &JobControl::default(),
        Arc::new(SortProgress::default()),
    )
    .await;
    assert!(matches!(r, Err(JobError::Io { .. })), "{r:?}");
}

#[test]
fn pass_count() {
    assert_eq!(merge_passes(100, 256), 1);
    assert_eq!(merge_passes(100, 3), 5);
}

// ---------------------------------------------------------------------------
// Differential test
// ---------------------------------------------------------------------------

const COLS: [(&str, ColType); 5] = [
    ("i", ColType::I64),
    ("f", ColType::F64),
    ("s", ColType::Str),
    ("d", ColType::Date),
    ("b", ColType::Bool),
];
const I_VALUES: &[&str] = &[
    "2",
    "10",
    "-5",
    "0",
    "-0",
    "9223372036854775807",
    "-9223372036854775808",
    "x",
    "",
    "NULL",
    "2",
];
const F_VALUES: &[&str] = &[
    "1.5", "-2", "1e3", "-0.0", "0", "-1e-5", "2", "10", "x", "", "1e-310", "-1e308",
];
const S_VALUES: &[&str] = &[
    "abcdefghijklmnopqr1",
    "abcdefghijklmnopqr2",
    "abcdefghijklmnopqr10",
    "abcdefghijklmn",
    "abcdefghijklmnO",
    "ABCDEFGHIJKLMNOPQR1",
    "apple",
    "Banana",
    "cherry",
    "Müller",
    "müller",
    "MÜLLERxxxxxxxxxxxxx",
    "",
    "NA",
    "z",
];
const D_VALUES: &[&str] = &[
    "1969-12-31",
    "1970-01-01",
    "2026-03-01",
    "1600-02-29",
    "2023-02-29",
    "",
    "2026-03-01",
];
const B_VALUES: &[&str] = &["true", "FALSE", "yes", "0", "1", "maybe", ""];

fn pools() -> [&'static [&'static str]; 5] {
    [I_VALUES, F_VALUES, S_VALUES, D_VALUES, B_VALUES]
}

type Row = Vec<&'static str>;

fn row() -> impl Strategy<Value = Row> {
    (0..5usize)
        .map(|c| prop::sample::select(pools()[c].to_vec()))
        .collect::<Vec<_>>()
}

fn ref_date(v: &str) -> Option<(i32, u32, u32)> {
    let p: Vec<&str> = v.split('-').collect();
    if p.len() != 3 {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = (p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    ((1..=12).contains(&m) && d >= 1 && d <= days[m as usize - 1]).then_some((y, m, d))
}

fn ref_bool(v: &str) -> Option<bool> {
    match v.to_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// The value of `v` for column `c` as something orderable, `None` for null
/// or unparseable.
fn ref_cmp(c: usize, ci: bool, a: &str, b: &str) -> Option<Option<Ordering>> {
    let null = |v: &str| ["", "NULL", "null", "NA", "N/A", "\\N"].contains(&v);
    let (na, nb) = (null(a), null(b));
    let parsed = |v: &str| -> bool {
        !null(v)
            && match COLS[c].1 {
                ColType::I64 => v.parse::<i64>().is_ok(),
                ColType::F64 => v.parse::<f64>().is_ok(),
                ColType::Date => ref_date(v).is_some(),
                ColType::Bool => ref_bool(v).is_some(),
                _ => true,
            }
    };
    let _ = (na, nb);
    let (pa, pb) = (parsed(a), parsed(b));
    if !pa || !pb {
        return Some(match (pa, pb) {
            (false, false) => None,
            (false, true) => Some(Ordering::Greater),
            _ => Some(Ordering::Less),
        });
    }
    Some(Some(match COLS[c].1 {
        ColType::I64 => a.parse::<i64>().unwrap().cmp(&b.parse::<i64>().unwrap()),
        ColType::F64 => a
            .parse::<f64>()
            .unwrap()
            .partial_cmp(&b.parse::<f64>().unwrap())
            .unwrap(),
        ColType::Date => ref_date(a).cmp(&ref_date(b)),
        ColType::Bool => ref_bool(a).cmp(&ref_bool(b)),
        _ if ci => a.to_lowercase().as_bytes().cmp(b.to_lowercase().as_bytes()),
        _ => a.as_bytes().cmp(b.as_bytes()),
    }))
}

fn reference(rows: &[Row], ids: &[u64], keys: &[SortKey]) -> Vec<u64> {
    let mut out = ids.to_vec();
    out.sort_by(|&x, &y| {
        for k in keys {
            let (a, b) = (rows[x as usize][k.column], rows[y as usize][k.column]);
            match ref_cmp(k.column, k.ci, a, b).unwrap() {
                None | Some(Ordering::Equal) => {}
                Some(o) => {
                    // Nulls last in both directions: only real values flip.
                    let null_involved = {
                        let p = |v: &str| ref_cmp(k.column, false, v, v) == Some(None);
                        p(a) || p(b)
                    };
                    return if k.descending && !null_involved {
                        o.reverse()
                    } else {
                        o
                    };
                }
            }
        }
        x.cmp(&y)
    });
    out
}

fn key_strategy() -> impl Strategy<Value = SortKey> {
    (0..5usize, any::<bool>(), any::<bool>()).prop_map(|(column, descending, ci)| SortKey {
        column,
        descending,
        ci: ci && column == 2,
    })
}

#[derive(Clone, Debug)]
enum ParentKind {
    All,
    Filtered(Vec<bool>),
    Ordered(Vec<bool>, u64),
}

fn parent_strategy(n: usize) -> impl Strategy<Value = ParentKind> {
    prop_oneof![
        Just(ParentKind::All),
        prop::collection::vec(any::<bool>(), n).prop_map(ParentKind::Filtered),
        (prop::collection::vec(any::<bool>(), n), any::<u64>())
            .prop_map(|(m, seed)| ParentKind::Ordered(m, seed)),
    ]
}

fn case() -> impl Strategy<Value = (Vec<Row>, Vec<SortKey>, ParentKind, usize, usize)> {
    (
        prop::collection::vec(row(), 1..400),
        prop::collection::vec(key_strategy(), 1..4),
    )
        .prop_flat_map(|(rows, keys)| {
            let n = rows.len();
            (
                Just(rows),
                Just(keys),
                parent_strategy(n),
                1usize..64,
                2usize..5,
            )
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn differential((rows, keys, parent, buffer, fan_in) in case()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let exec = Executor::new(3);
            let mut content = String::from("i,f,s,d,b\n");
            for r in &rows {
                content.push_str(&r.join(","));
                content.push('\n');
            }
            let fx = fixture(&content, &exec).await;
            let cols = columns(&COLS);
            let (view, ids): (View, Vec<u64>) = match &parent {
                ParentKind::All => (View::All, (0..rows.len() as u64).collect()),
                ParentKind::Filtered(mask) => {
                    let ids: Vec<u64> = (0..rows.len() as u64).filter(|&i| mask[i as usize]).collect();
                    (
                        View::Filtered {
                            rows: Arc::new(FilterRows::from_bitmap(ids.iter().copied().collect())),
                            expr: String::new(),
                            columns: vec![],
                        },
                        ids,
                    )
                }
                ParentKind::Ordered(mask, seed) => {
                    let mut ids: Vec<u64> = (0..rows.len() as u64).filter(|&i| mask[i as usize]).collect();
                    // Deterministic shuffle.
                    let mut x = *seed | 1;
                    for i in (1..ids.len()).rev() {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        ids.swap(i, (x % (i as u64 + 1)) as usize);
                    }
                    (
                        View::Ordered {
                            list: RowIdList::from_ids(fx.tmp.path(), &ids).unwrap(),
                            kind: OrderedKind::Sorted { keys: vec![] },
                        },
                        ids,
                    )
                }
            };
            let mut j = job(&fx, &cols, keys.clone(), view);
            j.ram_cap = 24 * buffer as u64;
            j.options.fan_in = Some(fan_in);
            j.options.chunk_rows = 7;
            j.options.max_slice_records = 5;
            let got = run(j, &exec).await;
            let want = reference(&rows, &ids, &keys);
            assert_eq!(got, want, "keys {:?} parent {:?}", keys, parent);
        });
    }
}

/// M5-03 acceptance at full size: 10M rows with a 64 MiB budget spill many
/// runs; fan-in 3 forces several merge passes. Slow in debug builds:
/// `cargo test --release -p tachy-core --test sort -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "10M rows; run in release"]
async fn ten_million_rows_with_a_64m_budget() {
    let exec = Executor::new(0);
    let n = 10_000_000u64;
    let mut content = String::with_capacity(n as usize * 12);
    content.push_str("v\n");
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        content.push_str(&((x % 1_000_000_007) as i64 - 500_000_000).to_string());
        content.push('\n');
    }
    let fx = fixture(&content, &exec).await;
    drop(content);
    let cols = columns(&[("v", ColType::I64)]);
    let mut j = job(&fx, &cols, vec![SortKey::asc(0)], View::All);
    j.ram_cap = 64 << 20;
    j.options.fan_in = Some(3);
    let progress = Arc::new(SortProgress::default());
    let t = std::time::Instant::now();
    let list = run_sort(j, &exec, &JobControl::default(), Arc::clone(&progress))
        .await
        .unwrap();
    eprintln!(
        "sorted {n} rows in {:?}, passes {:?}",
        t.elapsed(),
        progress.pass()
    );
    assert!(progress.pass().1 >= 2);
    assert_eq!(list.len(), n);
    // Check order and that it is a permutation.
    let bytes = fx.src.bytes();
    let starts: Vec<usize> = std::iter::once(2)
        .chain(memchr::memchr_iter(b'\n', bytes).map(|i| i + 1))
        .filter(|&i| i < bytes.len())
        .skip(1)
        .collect();
    assert_eq!(starts.len() as u64, n);
    let value = |id: u64| -> i64 {
        let s = starts[id as usize];
        let e = s + memchr::memchr(b'\n', &bytes[s..]).unwrap();
        std::str::from_utf8(&bytes[s..e]).unwrap().parse().unwrap()
    };
    let mut seen = vec![false; n as usize];
    let mut prev: Option<(i64, u64)> = None;
    for chunk in 0..n.div_ceil(1 << 20) {
        for id in list.read(chunk << 20, 1 << 20) {
            assert!(!seen[id as usize]);
            seen[id as usize] = true;
            let cur = (value(id), id);
            if let Some(p) = prev {
                assert!(p < cur, "{p:?} then {cur:?}");
            }
            prev = Some(cur);
        }
    }
}
