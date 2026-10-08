//! Duplicate rows (`tachy_core::dupes`).
//!
//! - `DupeSpec::parse`, `key_text`, `display`.
//! - `dupes` and `dedupe` on `All`, `Filtered` and `Ordered` parents
//!   (view order kept), on chosen columns and on whole rows.
//! - Keys compare edited values (`edit name: trim`).
//! - Spilling to partition files gives the in-memory result, and the job's
//!   temp dir is removed afterwards.
//! - Differential test against a `HashMap` reference over random files.
//! - Waiting for a growing parent, cancellation.

mod support;

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use proptest::prelude::*;
use roaring::RoaringTreemap;
use support::{index_source, open_sniffed, runtime, temp_file};
use tachy_core::{
    column::ColumnMeta,
    dupes::{DupeJob, DupeMode, DupeOptions, DupeResult, DupeRows, DupeSpec, run_dupes},
    edit::{Edits, parse_ops},
    exec::Executor,
    index::RowIndex,
    jobs::{JobControl, JobError, RowsProgress},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    sort::SortKey,
    source::Source,
    types::NullSet,
    view::{FilterRows, OrderedKind, RowIdList, View},
};
use tokio_util::sync::CancellationToken;

struct Fx {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    tmp: tempfile::TempDir,
}

fn fixture(content: &[u8]) -> Fx {
    let file = temp_file(content);
    let (src, report) = open_sniffed(file.path());
    let (index, _) = index_source(&src, report);
    Fx {
        _file: file,
        src,
        index,
        tmp: tempfile::tempdir().unwrap(),
    }
}

fn cols(src: &Source) -> Vec<ColumnMeta> {
    src.column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| ColumnMeta::new(n, i, false))
        .collect()
}

fn job(fx: &Fx, parent: View, fields: &[usize], mode: DupeMode) -> DupeJob {
    DupeJob {
        src: Arc::clone(&fx.src),
        index: Arc::clone(&fx.index),
        parent,
        fields: fields.to_vec(),
        mode,
        ram_cap: 64 << 20,
        tmp_dir: fx.tmp.path().to_path_buf(),
        options: DupeOptions {
            chunk_rows: 3,
            partitions: None,
        },
    }
}

fn run(job: DupeJob) -> DupeResult {
    let rt = runtime();
    let exec = Executor::with_handle(rt.handle().clone(), 3);
    let progress = Arc::new(RowsProgress::default());
    let rows = job.parent.len(&job.index);
    let r = rt
        .block_on(run_dupes(
            job,
            &exec,
            &JobControl::default(),
            Arc::clone(&progress),
        ))
        .unwrap();
    assert_eq!(progress.done(), rows * 2, "extract + group");
    assert_eq!(progress.total(), rows * 2);
    r
}

fn ids(r: &DupeResult) -> Vec<u64> {
    match &r.rows {
        DupeRows::Rows(rows) => {
            assert!(!rows.is_growing());
            rows.with_bitmap(|b| b.iter().collect())
        }
        DupeRows::List(list) => {
            assert!(!list.is_growing());
            list.read(0, list.len() as usize)
        }
    }
}

fn tmp_entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

const PEOPLE: &[u8] = b"email,name,city\n\
a@x,Ann,Rome\n\
b@x,Bob,Oslo\n\
a@x,Ann,Rome\n\
c@x,Cid,Rome\n\
b@x,Bo,Oslo\n\
a@x,Ann,Rome\n\
d@x,Dee,Lima\n";

// ---------------------------------------------------------------------------
// DupeSpec
// ---------------------------------------------------------------------------

#[test]
fn parse_columns() {
    let fx = fixture(b"sku,odd name,price\n1,2,3\n");
    let c = cols(&fx.src);
    let spec = DupeSpec::parse("sku, price", DupeMode::Show, &c).unwrap();
    assert_eq!(spec.columns, [0, 2]);
    assert_eq!(spec.key_text(&c), "sku, price");
    let odd = DupeSpec::parse("`odd name`", DupeMode::Show, &c).unwrap();
    assert_eq!(odd.columns, [1]);
    assert_eq!(odd.key_text(&c), "`odd name`");
    assert_eq!(spec.display(&c), "dupes sku, price");
    // `$N`, duplicates collapsed.
    let spec = DupeSpec::parse("$3,price", DupeMode::Remove, &c).unwrap();
    assert_eq!(spec.columns, [2]);
    assert_eq!(spec.display(&c), "dedupe price");
    // Nothing: every column.
    let spec = DupeSpec::parse("  ", DupeMode::Remove, &c).unwrap();
    assert!(spec.columns.is_empty());
    assert_eq!(spec.key_text(&c), "all columns");
    assert_eq!(spec.display(&c), "dedupe");
    // Errors carry spans.
    let e = DupeSpec::parse("sku,,price", DupeMode::Show, &c).unwrap_err();
    assert_eq!(e.span, 4..4);
    let e = DupeSpec::parse("sku, nope", DupeMode::Show, &c).unwrap_err();
    assert_eq!(e.span, 5..9);
}

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

#[test]
fn show_lists_whole_groups_in_file_order() {
    let fx = fixture(PEOPLE);
    let r = run(job(&fx, View::All, &[0], DupeMode::Show));
    assert_eq!(ids(&r), [0, 1, 2, 4, 5]);
    assert_eq!((r.groups, r.parent_rows), (2, 7));
    assert_eq!(r.summary(DupeMode::Show), "5 duplicate rows in 2 groups");
}

#[test]
fn remove_keeps_the_first_row_of_each_key() {
    let fx = fixture(PEOPLE);
    let r = run(job(&fx, View::All, &[0], DupeMode::Remove));
    assert_eq!(ids(&r), [0, 1, 3, 6]);
    assert_eq!(
        r.summary(DupeMode::Remove),
        "removed 3 duplicate rows (2 groups)"
    );
}

#[test]
fn whole_rows_and_multiple_columns() {
    let fx = fixture(PEOPLE);
    // `b@x,Bob` and `b@x,Bo` differ in `name`.
    let r = run(job(&fx, View::All, &[], DupeMode::Show));
    assert_eq!(ids(&r), [0, 2, 5]);
    assert_eq!(r.groups, 1);
    let r = run(job(&fx, View::All, &[0, 2], DupeMode::Show));
    assert_eq!(ids(&r), [0, 1, 2, 4, 5]);
    // Column order and value boundaries matter: `ab|c` ≠ `a|bc`.
    let fx = fixture(b"x,y\nab,c\na,bc\nab,c\n");
    let r = run(job(&fx, View::All, &[0, 1], DupeMode::Show));
    assert_eq!(ids(&r), [0, 2]);
}

#[test]
fn no_duplicates_and_empty_views() {
    let fx = fixture(b"k\n1\n2\n3\n");
    let r = run(job(&fx, View::All, &[0], DupeMode::Show));
    assert!(ids(&r).is_empty());
    assert_eq!(r.summary(DupeMode::Show), "no duplicate rows");
    let r = run(job(&fx, View::All, &[0], DupeMode::Remove));
    assert_eq!(ids(&r), [0, 1, 2]);
    assert_eq!(r.summary(DupeMode::Remove), "no duplicate rows to remove");
    let fx = fixture(b"k\n");
    let r = run(job(&fx, View::All, &[0], DupeMode::Remove));
    assert!(ids(&r).is_empty());
}

#[test]
fn missing_cells_equal_empty_ones() {
    let fx = fixture(b"a,b\n1,\n1\n2,x\n");
    let r = run(job(&fx, View::All, &[0, 1], DupeMode::Show));
    assert_eq!(ids(&r), [0, 1]);
}

#[test]
fn filtered_parent() {
    let fx = fixture(PEOPLE);
    // Rows 1..=5 only: `a@x` at 2 and 5, `b@x` at 1 and 4.
    let rows = Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter(1..6u64)));
    let parent = View::Filtered {
        rows,
        expr: "true".into(),
        columns: vec![],
    };
    let r = run(job(&fx, parent.clone(), &[0], DupeMode::Show));
    assert_eq!(ids(&r), [1, 2, 4, 5]);
    let r = run(job(&fx, parent, &[0], DupeMode::Remove));
    assert_eq!(ids(&r), [1, 2, 3]);
    assert_eq!(r.parent_rows, 5);
    // A duplicates view is a parent too.
    let dupes = run(job(&fx, View::All, &[0], DupeMode::Show));
    let DupeRows::Rows(rows) = dupes.rows else {
        panic!("a bitmap for an All parent")
    };
    let spec = DupeSpec {
        columns: vec![0],
        mode: DupeMode::Show,
    };
    let r = run(job(&fx, View::Dupes { rows, spec }, &[1], DupeMode::Remove));
    assert_eq!(ids(&r), [0, 1, 4]);
}

#[test]
fn ordered_parent_keeps_its_order() {
    let fx = fixture(PEOPLE);
    // Reverse file order: "first" is the first row in this order.
    let list = RowIdList::from_ids(fx.tmp.path(), &[6, 5, 4, 3, 2, 1, 0]).unwrap();
    let parent = View::Ordered {
        list,
        kind: OrderedKind::Sorted {
            keys: vec![SortKey::desc(0)],
        },
    };
    let r = run(job(&fx, parent.clone(), &[0], DupeMode::Remove));
    assert!(matches!(r.rows, DupeRows::List(_)));
    assert_eq!(ids(&r), [6, 5, 4, 3]);
    let r = run(job(&fx, parent, &[0], DupeMode::Show));
    assert_eq!(ids(&r), [5, 4, 2, 1, 0]);
}

#[test]
fn keys_compare_edited_values() {
    let fx = fixture(b"name,n\nBob ,1\nbob,2\n  BOB,3\nann,4\n");
    let raw = run(job(&fx, View::All, &[0], DupeMode::Show));
    assert!(ids(&raw).is_empty());
    let mut e = Edits::new(fx.src.dialect().encoding, NullSet::default());
    e.push(0, parse_ops("trim | lower").unwrap());
    let edited = Fx {
        _file: temp_file(b""),
        src: Arc::new(fx.src.with_edits(e)),
        index: Arc::clone(&fx.index),
        tmp: tempfile::tempdir().unwrap(),
    };
    let r = run(job(&edited, View::All, &[0], DupeMode::Show));
    assert_eq!(ids(&r), [0, 1, 2]);
    let r = run(job(&edited, View::All, &[0], DupeMode::Remove));
    assert_eq!(ids(&r), [0, 3]);
}

#[test]
fn spilled_partitions_equal_memory_and_are_removed() {
    let mut content = String::from("k,v\n");
    for i in 0..2_000 {
        content.push_str(&format!("{},{}\n", i % 307, i % 5));
    }
    let fx = fixture(content.as_bytes());
    for mode in [DupeMode::Show, DupeMode::Remove] {
        let memory = ids(&run(job(&fx, View::All, &[0], mode)));
        for parts in [2, 16, 64] {
            let mut j = job(&fx, View::All, &[0], mode);
            j.options = DupeOptions {
                chunk_rows: 97,
                partitions: Some(parts),
            };
            let spilled = run(j);
            assert_eq!(ids(&spilled), memory, "{mode:?} {parts} partitions");
            assert_eq!(spilled.groups, 307);
        }
        assert!(tmp_entries(fx.tmp.path()).is_empty(), "job dirs removed");
    }
    // Partitions come from the budget with a 1 MiB floor: 2,000 rows ×
    // 24 bytes fit, so even a 1-byte budget groups in memory.
    let mut j = job(&fx, View::All, &[0], DupeMode::Remove);
    j.ram_cap = 1;
    assert_eq!(ids(&run(j)).len(), 307);
}

// ---------------------------------------------------------------------------
// Differential test
// ---------------------------------------------------------------------------

/// Reference: every record's key values, then a `HashMap` grouping.
fn reference(src: &Source, fields: &[usize], mode: DupeMode) -> Vec<u64> {
    let mut p = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut keys: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut pos = src.data_start();
    loop {
        let next = match p.parse_at(src.bytes(), pos, &mut rec) {
            ParseOutcome::Eof => break,
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
        };
        let n = if fields.is_empty() {
            rec.fields.len().max(src.width())
        } else {
            fields.len()
        };
        let key = (0..n)
            .map(|k| {
                let f = if fields.is_empty() { k } else { fields[k] };
                if f < rec.fields.len() {
                    p.field_value(src.bytes(), &rec, f, &mut scratch).to_vec()
                } else {
                    Vec::new()
                }
            })
            .collect();
        keys.push(key);
        pos = next;
    }
    let mut count: HashMap<&Vec<Vec<u8>>, usize> = HashMap::new();
    for k in &keys {
        *count.entry(k).or_default() += 1;
    }
    let mut seen = std::collections::HashSet::new();
    keys.iter()
        .enumerate()
        .filter(|(_, k)| match mode {
            DupeMode::Show => count[k] > 1,
            DupeMode::Remove => seen.insert(*k),
        })
        .map(|(i, _)| i as u64)
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn matches_a_hashmap_reference(
        rows in prop::collection::vec(
            prop::collection::vec(prop::sample::select(vec!["", "a", "b", "ab", "\"x,y\""]), 1..4),
            0..60,
        ),
        key in prop::sample::select(vec![vec![], vec![0], vec![1], vec![1, 0], vec![2]]),
        parts in prop::sample::select(vec![None, Some(4)]),
        show in any::<bool>(),
    ) {
        let mut content = String::from("c0,c1,c2\n");
        for r in &rows {
            content.push_str(&r.join(","));
            content.push('\n');
        }
        let fx = fixture(content.as_bytes());
        let mode = if show { DupeMode::Show } else { DupeMode::Remove };
        let mut j = job(&fx, View::All, &key, mode);
        j.options.partitions = parts;
        let got = ids(&run(j));
        prop_assert_eq!(got, reference(&fx.src, &key, mode));
    }
}

// ---------------------------------------------------------------------------
// Growing parents, cancellation
// ---------------------------------------------------------------------------

#[test]
fn waits_for_a_growing_parent() {
    let fx = fixture(PEOPLE);
    let rows = Arc::new(FilterRows::new_growing());
    let parent = View::Filtered {
        rows: Arc::clone(&rows),
        expr: "true".into(),
        columns: vec![],
    };
    let rt = runtime();
    let j = job(&fx, parent, &[0], DupeMode::Show);
    let r = rt.block_on(async {
        let task = tokio::spawn(async move {
            let exec = Executor::new(2);
            run_dupes(
                j,
                &exec,
                &JobControl::default(),
                Arc::new(RowsProgress::default()),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!task.is_finished(), "waits while the parent grows");
        rows.insert_many([0, 2, 3]);
        rows.finish();
        task.await.unwrap()
    });
    assert_eq!(ids(&r.unwrap()), [0, 2]);
}

#[test]
fn cancelled_jobs_remove_their_files() {
    let mut content = String::from("k\n");
    for i in 0..5_000 {
        content.push_str(&format!("{}\n", i % 10));
    }
    let fx = fixture(content.as_bytes());
    let mut j = job(&fx, View::All, &[0], DupeMode::Show);
    j.options.partitions = Some(8);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let rt = runtime();
    let r = rt.block_on(run_dupes(
        j,
        &Executor::with_handle(rt.handle().clone(), 2),
        &JobControl::new(cancel),
        Arc::new(RowsProgress::default()),
    ));
    assert!(matches!(r, Err(JobError::Cancelled)), "{r:?}");
    assert!(tmp_entries(fx.tmp.path()).is_empty());
}
