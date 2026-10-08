//! Column edits (`tachy_core::edit`) seen by every consumer of field values.
//!
//! - `Source::with_edits`: shares the mapping and header; `with_dialect`
//!   drops the edits.
//! - Display and copy (`ParsedRow::display` / `value`), nulls and missing
//!   cells untouched.
//! - Filter: edited values are compared, the raw-bytes pre-filter literal is
//!   dropped (a match exists only after the edit).
//! - Search: edited values are matched, the `All` fast path is skipped.
//! - Sort keys, the profile job and export use edited values.
//! - `SampleResult::with_edits` re-infers the type without file access.
//! - Windows-1252 sources are edited in Unicode and written back.

mod support;

use std::sync::Arc;

use support::{index_source, open_sniffed, runtime, temp_file};
use tachy_core::{
    cache::RowCache,
    column::{ColumnMeta, ColumnName},
    dialect::Encoding,
    edit::{Edits, parse_ops},
    exec::Executor,
    export::{ExportColumn, ExportContext, ExportOptions, Quoting, run_export},
    filter::{FilterOptions, FilterOutput, ParentRows, run_filter_with},
    index::RowIndex,
    jobs::{ExportProgress, FilterProgress, JobControl, PauseToken, RowsProgress, SortProgress},
    query,
    sample::sample_head,
    search::{Direction, SearchRequest, SearchStatus, parse_search, search_blocking},
    sort::{SortJob, SortKey, SortOptions, run_sort},
    source::Source,
    stats::{
        Scalar,
        profile::{ProfileColumn, ProfileOptions, run_profile},
    },
    types::{ColType, NullSet},
    view::View,
};
use tokio_util::sync::CancellationToken;

const ORDERS: &[u8] = b"sku,price,name\n\
ABC-1001,$30,  alice \n\
ABC-1002,$5,bob\n\
XYZ-7,$100,NULL\n\
ABC-1003,$7,carol\n";

struct Fx {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
}

fn fixture(content: &[u8]) -> Fx {
    let file = temp_file(content);
    let (src, report) = open_sniffed(file.path());
    let (index, _) = index_source(&src, report);
    Fx {
        _file: file,
        src,
        index,
    }
}

/// `edits` as `(field, chain)` pairs, for `src`'s encoding.
fn edits(src: &Source, chains: &[(usize, &str)]) -> Edits {
    let mut e = Edits::new(src.dialect().encoding, NullSet::default());
    for (field, chain) in chains {
        e.push(*field, parse_ops(chain).unwrap());
    }
    e
}

fn edited(fx: &Fx, chains: &[(usize, &str)]) -> Arc<Source> {
    Arc::new(fx.src.with_edits(edits(&fx.src, chains)))
}

fn columns(src: &Source, types: &[ColType]) -> Vec<ColumnMeta> {
    src.column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let mut c = ColumnMeta::new(n, i, false);
            c.set_inferred(types.get(i).copied().unwrap_or(ColType::Str));
            c
        })
        .collect()
}

/// Every displayed cell of `src`, row by row.
fn cells(src: &Source, index: &RowIndex) -> Vec<Vec<String>> {
    let mut cache = RowCache::new();
    let ids: Vec<u64> = (0..index.total_rows().unwrap()).collect();
    cache
        .get_window(&ids, src, index)
        .rows
        .into_iter()
        .map(|r| {
            let r = r.unwrap();
            (0..r.field_count())
                .map(|c| r.display(src, c).to_owned())
                .collect()
        })
        .collect()
}

#[test]
fn with_edits_shares_the_mapping_and_the_header() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(0, "drop 1")]);
    assert_eq!(src.bytes().as_ptr(), fx.src.bytes().as_ptr());
    assert_eq!(src.header(), fx.src.header());
    assert_eq!(src.data_start(), fx.src.data_start());
    assert!(src.edits().is_edited(0));
    assert!(fx.src.edits().is_empty(), "the original keeps its edits");
    // A dialect change rebuilds the columns: edits are dropped.
    let again = src.with_dialect(*src.dialect());
    assert!(again.edits().is_empty());
}

#[test]
fn display_and_copy_show_edited_values() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(0, "drop 1"), (1, "drop 1"), (2, "trim | title")]);
    let got = cells(&src, &fx.index);
    assert_eq!(
        got,
        [
            ["BC-1001", "30", "Alice"],
            ["BC-1002", "5", "Bob"],
            ["YZ-7", "100", "NULL"],
            ["BC-1003", "7", "Carol"],
        ]
    );
    // The clipboard reads `ParsedRow::value`, the same edited bytes.
    let mut cache = RowCache::new();
    let row = cache.get_window(&[0], &src, &fx.index).rows[0]
        .clone()
        .unwrap();
    let mut scratch = Vec::new();
    assert_eq!(row.value(&src, 2, &mut scratch), Some(&b"Alice"[..]));
    assert_eq!(row.value(&src, 9, &mut scratch), None);
    // Unedited source: raw values.
    assert_eq!(cells(&fx.src, &fx.index)[0][2], "  alice ");
}

#[test]
fn short_rows_keep_missing_cells() {
    let fx = fixture(b"a,b\n1,x\n2\n");
    let src = edited(&fx, &[(1, "prefix p")]);
    let got = cells(&src, &fx.index);
    assert_eq!(got[0], ["1", "px"]);
    assert_eq!(got[1], ["2"]);
}

fn filter_ids(fx: &Fx, src: &Arc<Source>, q: &str, types: &[ColType]) -> Vec<u64> {
    let cols = columns(src, types);
    let names: Vec<ColumnName> = cols.iter().map(|c| c.name.clone()).collect();
    let resolved = query::resolve(query::parse(q).unwrap(), &names).unwrap();
    let pred = query::compile_with_edits(
        &resolved,
        &cols,
        src.dialect(),
        &NullSet::default(),
        src.edits_arc(),
    )
    .unwrap();
    let rt = runtime();
    let out = FilterOutput::for_parent(&ParentRows::All, &std::env::temp_dir()).unwrap();
    let rows = Arc::clone(out.rows().unwrap());
    rt.block_on(run_filter_with(
        ParentRows::All,
        Arc::clone(src),
        Arc::clone(&fx.index),
        pred,
        out,
        Executor::with_handle(rt.handle().clone(), 2),
        CancellationToken::new(),
        PauseToken::new(),
        Arc::new(FilterProgress::default()),
        FilterOptions {
            prefilter: true,
            ..FilterOptions::default()
        },
    ))
    .unwrap();
    rows.with_bitmap(|b| b.iter().collect())
}

#[test]
fn filters_compare_edited_values() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(0, "drop 4"), (1, "drop 1")]);
    let types = [ColType::Str, ColType::I64, ColType::Str];
    // `$30` → 30: a numeric comparison only works after the edit.
    assert_eq!(filter_ids(&fx, &src, "price > 6", &types), [0, 2, 3]);
    // `1002` is in the raw bytes too, but `== "1002"` only after `drop 4`.
    assert_eq!(filter_ids(&fx, &src, "sku == \"1002\"", &types), [1]);
    // `starts "100"`: no raw record contains a field starting with it, so a
    // raw-bytes pre-filter would be wrong; the literal is dropped.
    assert_eq!(
        filter_ids(&fx, &src, "sku starts \"100\"", &types),
        [0, 1, 3]
    );
    let edited_literal = {
        let cols = columns(&src, &types);
        let names: Vec<ColumnName> = cols.iter().map(|c| c.name.clone()).collect();
        let r = query::resolve(query::parse("sku contains \"BC-1\"").unwrap(), &names).unwrap();
        query::compile_with_edits(
            &r,
            &cols,
            src.dialect(),
            &NullSet::default(),
            src.edits_arc(),
        )
        .unwrap()
    };
    assert_eq!(edited_literal.required_literal(), None);
    // Unedited columns keep it.
    let cols = columns(&src, &types);
    let names: Vec<ColumnName> = cols.iter().map(|c| c.name.clone()).collect();
    let r = query::resolve(query::parse("name contains \"carol\"").unwrap(), &names).unwrap();
    let p = query::compile_with_edits(
        &r,
        &cols,
        src.dialect(),
        &NullSet::default(),
        src.edits_arc(),
    )
    .unwrap();
    assert_eq!(p.required_literal(), Some(&b"carol"[..]));
}

#[test]
fn search_matches_edited_values() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(2, "trim | upper")]);
    let cols = columns(&src, &[]);
    let find = |src: &Source, q: &str| {
        let query = Arc::new(parse_search(q, &cols).unwrap());
        let req = SearchRequest::new(query, &cols, &[0, 1, 2], 0, 0, Direction::Forward);
        search_blocking(
            &req,
            &View::All,
            src,
            &fx.index,
            &CancellationToken::new(),
            &SearchStatus::default(),
        )
        .unwrap()
        .hit
        .map(|h| (h.row_id, h.col, h.byte_range_in_value))
    };
    // `CAROL` exists only after the edit: the raw-bytes fast path would miss it.
    assert_eq!(find(&src, "CAROL"), Some((3, 2, 0..5)));
    assert_eq!(find(&fx.src, "CAROL"), None);
    // The leading spaces were trimmed: the match range is in the edited value.
    assert_eq!(find(&src, "ALICE"), Some((0, 2, 0..5)));
    assert_eq!(find(&src, "re:^ALICE$"), Some((0, 2, 0..5)));
}

#[test]
fn sort_keys_use_edited_values() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(1, "drop 1")]);
    let tmp = tempfile::tempdir().unwrap();
    let rt = runtime();
    let sort = |src: &Arc<Source>, ty: ColType| {
        let job = SortJob {
            src: Arc::clone(src),
            index: Arc::clone(&fx.index),
            parent: View::All,
            keys: vec![SortKey::asc(1)],
            columns: columns(src, &[ColType::Str, ty, ColType::Str]),
            nulls: NullSet::default(),
            ram_cap: 64 << 20,
            tmp_dir: tmp.path().to_path_buf(),
            options: SortOptions::default(),
        };
        let exec = Executor::with_handle(rt.handle().clone(), 2);
        let list = rt
            .block_on(run_sort(
                job,
                &exec,
                &JobControl::default(),
                Arc::new(SortProgress::default()),
            ))
            .unwrap();
        list.read(0, list.len() as usize)
    };
    // 5, 7, 30, 100 once the `$` is gone.
    assert_eq!(sort(&src, ColType::I64), [1, 3, 0, 2]);
    // Unedited, `$…` does not parse as i64: every value is invalid and the
    // order is the file order (ties by row id).
    assert_eq!(sort(&fx.src, ColType::I64), [0, 1, 2, 3]);
}

#[test]
fn profile_uses_edited_values() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(1, "drop 1")]);
    let mut cols = columns(&src, &[ColType::Str, ColType::I64, ColType::Str]);
    let rt = runtime();
    let r = rt
        .block_on(run_profile(
            Arc::clone(&src),
            Arc::clone(&fx.index),
            vec![ProfileColumn::from_meta(1, &cols[1])],
            NullSet::default(),
            Executor::with_handle(rt.handle().clone(), 2),
            JobControl::new(CancellationToken::new()),
            Arc::new(RowsProgress::default()),
            ProfileOptions::default(),
        ))
        .unwrap();
    r.apply(&mut cols);
    let stats = cols[1].stats.as_ref().unwrap();
    let n = stats.numeric.as_ref().unwrap();
    assert_eq!(
        (n.min(), n.max()),
        (Some(Scalar::I64(5)), Some(Scalar::I64(100)))
    );
    assert_eq!(stats.nulls, 0);
}

#[test]
fn export_writes_edited_values() {
    let fx = fixture(ORDERS);
    let src = edited(&fx, &[(0, "s/^([A-Z]+)-(\\d+)$/$2-$1/"), (2, "trim")]);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.csv");
    let cols = columns(&src, &[]);
    let rt = runtime();
    let ctx = ExportContext {
        exec: Executor::with_handle(rt.handle().clone(), 2),
        ctl: JobControl::new(CancellationToken::new()),
        progress: Arc::new(ExportProgress::default()),
    };
    rt.block_on(run_export(
        ParentRows::All,
        Arc::clone(&src),
        Arc::clone(&fx.index),
        ExportColumn::select(&cols, &[0, 1, 2], false),
        ExportOptions::for_budget(b',', Quoting::Minimal, true, 32 << 20, 2, 16),
        target.clone(),
        ctx,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "sku,price,name\n1001-ABC,$30,alice\n1002-ABC,$5,bob\n7-XYZ,$100,NULL\n1003-ABC,$7,carol\n"
    );
}

#[test]
fn sample_reinfers_types_after_an_edit() {
    let fx = fixture(ORDERS);
    let rt = runtime();
    let exec = Executor::with_handle(rt.handle().clone(), 2);
    let nulls = NullSet::default();
    let raw = rt
        .block_on(sample_head(
            Arc::clone(&fx.src),
            &exec,
            nulls.clone(),
            CancellationToken::new(),
        ))
        .unwrap();
    assert_eq!(raw.per_column[1].inferred, ColType::Str);
    let e = edits(&fx.src, &[(1, "drop 1"), (2, "trim")]);
    let r = raw.with_edits(&e, &nulls, Encoding::Utf8);
    assert_eq!(r.per_column[1].inferred, ColType::I64);
    assert_eq!(r.per_column[1].values.get(0), Some(&b"30"[..]));
    // `  alice ` (8 wide) → `alice`.
    assert_eq!(r.per_column[2].stats.max_width, 5);
    assert_eq!(
        r.per_column[2].values.get(2),
        Some(&b"NULL"[..]),
        "nulls untouched"
    );
    // Unedited columns are unchanged, the raw sample too.
    assert_eq!(
        r.per_column[0].values.get(0),
        raw.per_column[0].values.get(0)
    );
    assert_eq!(raw.per_column[1].values.get(0), Some(&b"$30"[..]));
    assert_eq!((r.rows_sampled, r.phase), (raw.rows_sampled, raw.phase));
}

#[test]
fn windows_1252_values_are_edited_in_unicode() {
    // `café;crème` in Windows-1252.
    let fx = fixture(b"a;b\ncaf\xe9;cr\xe8me\n");
    assert_eq!(fx.src.dialect().encoding, Encoding::Windows1252);
    let src = edited(&fx, &[(0, "upper"), (1, "chop 2 | suffix ü")]);
    assert_eq!(cells(&src, &fx.index)[0], ["CAFÉ", "crèü"]);
    let mut cache = RowCache::new();
    let row = cache.get_window(&[0], &src, &fx.index).rows[0]
        .clone()
        .unwrap();
    let mut scratch = Vec::new();
    assert_eq!(row.value(&src, 0, &mut scratch), Some(&b"CAF\xc9"[..]));
    assert_eq!(row.value(&src, 1, &mut scratch), Some(&b"cr\xe8\xfc"[..]));
}
