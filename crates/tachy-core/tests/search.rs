//! M4-05: incremental search.
//!
//! - `parse_search`: `col:`, `re:`, both, ambiguous prefix, empty.
//! - False positives: a needle across a delimiter, inside `""` escapes,
//!   across a record terminator; a true match across a quoted newline.
//! - Forward, backward and wrap on `All`, `Filtered` and `Ordered` views,
//!   checked against a brute-force reference over every cell.
//! - Hidden columns, Windows-1252 needles, cancellation, a growing index.

mod support;

use std::{sync::Arc, time::Duration};

use roaring::RoaringTreemap;
use support::{runtime, temp_source};
use tachy_core::{
    column::{ColumnMeta, ColumnName},
    dialect::{DEFAULT_SAMPLE_BYTES, Dialect, DialectOverrides, Encoding, sniff},
    exec::Executor,
    index::{IndexOptions, RowIndex, build_index},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    search::{
        Direction, SearchHit, SearchOutcome, SearchPattern, SearchQuery, SearchRequest,
        SearchStatus, match_column, parse_search, run_search, search_blocking,
    },
    source::Source,
    view::{FilterRows, OrderedKind, RowIdList, View},
};
use tokio_util::sync::CancellationToken;

fn cols(names: &[&str]) -> Vec<ColumnMeta> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            ColumnMeta::new(
                ColumnName {
                    display: (*n).to_owned(),
                    query: (*n).to_owned(),
                },
                i,
                false,
            )
        })
        .collect()
}

fn src_cols(src: &Source) -> Vec<ColumnMeta> {
    src.column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| ColumnMeta::new(n, i, false))
        .collect()
}

// ---------------------------------------------------------------------------
// parse_search
// ---------------------------------------------------------------------------

fn literal(q: &SearchQuery) -> &str {
    match &q.pattern {
        SearchPattern::Literal(s) => s,
        SearchPattern::Regex(_) => panic!("expected a literal"),
    }
}

fn regex(q: &SearchQuery) -> &str {
    match &q.pattern {
        SearchPattern::Regex(r) => r.as_str(),
        SearchPattern::Literal(_) => panic!("expected a regex"),
    }
}

#[test]
fn parse_plain_column_regex_and_both() {
    let c = cols(&["customer", "order id", "error_code", "Price"]);
    let q = parse_search("becker", &c).unwrap();
    assert_eq!((literal(&q), q.column), ("becker", None));

    let q = parse_search("customer: becker", &c).unwrap();
    assert_eq!((literal(&q), q.column), ("becker", Some(0)));
    let q = parse_search("customer:becker", &c).unwrap();
    assert_eq!((literal(&q), q.column), ("becker", Some(0)));
    // Go-to rules: case-insensitive, unique prefix, display names with spaces.
    let q = parse_search("CUSTOMER: x", &c).unwrap();
    assert_eq!(q.column, Some(0));
    let q = parse_search("cust: x", &c).unwrap();
    assert_eq!(q.column, Some(0));
    let q = parse_search("order id: 42", &c).unwrap();
    assert_eq!((literal(&q), q.column), ("42", Some(1)));
    let q = parse_search("price: 9", &c).unwrap();
    assert_eq!(q.column, Some(3));

    let q = parse_search("re:^ORD-\\d+$", &c).unwrap();
    assert_eq!((regex(&q), q.column), ("^ORD-\\d+$", None));
    let q = parse_search("customer: re:^bec", &c).unwrap();
    assert_eq!((regex(&q), q.column), ("^bec", Some(0)));
}

#[test]
fn parse_ambiguous_unknown_and_empty() {
    let c = cols(&["error_code", "error_text", "x"]);
    // No `error` column, and `error` is an ambiguous prefix: plain text.
    let q = parse_search("error: disk full", &c).unwrap();
    assert_eq!((literal(&q), q.column), ("error: disk full", None));
    let q = parse_search("nope: a", &c).unwrap();
    assert_eq!((literal(&q), q.column), ("nope: a", None));
    let q = parse_search(": a", &c).unwrap();
    assert_eq!(literal(&q), ": a");
    assert_eq!(parse_search("", &c).unwrap_err(), "empty search");
    assert_eq!(parse_search("   ", &c).unwrap_err(), "empty search");
    assert_eq!(parse_search("x:", &c).unwrap_err(), "empty search");
    assert_eq!(parse_search("re:", &c).unwrap_err(), "empty regex");
    assert!(
        parse_search("re:(", &c)
            .unwrap_err()
            .starts_with("invalid regex")
    );
    assert_eq!(
        match_column("error", &c).unwrap_err(),
        "ambiguous: error_code, error_text"
    );
    assert_eq!(match_column("zz", &c).unwrap_err(), "no such column \"zz\"");
}

#[test]
fn find_in_cell_for_highlighting() {
    let c = cols(&["a"]);
    let q = parse_search("ab", &c).unwrap();
    assert_eq!(q.find_in_cell("xabyab"), vec![1..3, 4..6]);
    let q = parse_search("re:\\d+", &c).unwrap();
    assert_eq!(q.find_in_cell("a12b3"), vec![1..3, 4..5]);
    assert!(q.searches_column(0, true) && !q.searches_column(0, false));
}

// ---------------------------------------------------------------------------
// Engine helpers
// ---------------------------------------------------------------------------

struct Fx {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    cols: Vec<ColumnMeta>,
}

fn fixture_with(content: &[u8], dialect: Option<Dialect>) -> Fx {
    let report = sniff(content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let dialect = dialect.unwrap_or(report.dialect);
    let (file, src) = temp_source(content, dialect);
    let rt = runtime();
    let index = Arc::new(RowIndex::with_stride(src.data_start(), 4));
    rt.block_on(build_index(
        Arc::clone(&src),
        Arc::clone(&index),
        report,
        Executor::with_handle(rt.handle().clone(), 2),
        CancellationToken::new(),
        IndexOptions {
            chunk_size: 512,
            ..IndexOptions::default()
        },
    ))
    .unwrap();
    let cols = src_cols(&src);
    Fx {
        _file: file,
        src,
        index,
        cols,
    }
}

fn fixture(content: &[u8]) -> Fx {
    fixture_with(content, None)
}

fn search(
    fx: &Fx,
    view: &View,
    q: &str,
    display: &[usize],
    pos: u64,
    col: usize,
    dir: Direction,
) -> SearchOutcome {
    let query = Arc::new(parse_search(q, &fx.cols).unwrap());
    let req = SearchRequest::new(query, &fx.cols, display, pos, col, dir);
    search_blocking(
        &req,
        view,
        &fx.src,
        &fx.index,
        &CancellationToken::new(),
        &SearchStatus::default(),
    )
    .unwrap()
}

fn at(o: &SearchOutcome) -> Option<(u64, usize)> {
    o.hit.as_ref().map(|h| (h.row_id, h.col))
}

const TRICKY: &[u8] = b"name,note\n\
a,b\n\
\"x\"\"y\",z\n\
\"multi\nline\",q\n\
\"has a,b inside\",w\n\
ab\n\
cd,e\n";

#[test]
fn false_positives_are_rejected() {
    let fx = fixture(TRICKY);
    let all = [0, 1];
    let fwd = Direction::Forward;
    // Row 0's raw bytes `a,b` span the delimiter: rejected; row 3 matches.
    let o = search(&fx, &View::All, "a,b", &all, 0, 0, fwd);
    assert_eq!(at(&o), Some((3, 0)));
    assert_eq!(o.hit.unwrap().byte_range_in_value, 4..7);
    // `x""y` is in the raw bytes, not in the value `x"y`.
    let o = search(&fx, &View::All, "x\"\"y", &all, 0, 0, fwd);
    assert_eq!(o.hit, None);
    let o = search(&fx, &View::All, "x\"y", &all, 0, 0, fwd);
    assert_eq!(at(&o), Some((1, 0)));
    // Across a record terminator (`ab\ncd`): rejected.
    let o = search(&fx, &View::All, "b\ncd", &all, 0, 0, fwd);
    assert_eq!(o.hit, None);
    // Across a quoted newline, inside one value: a real match.
    let o = search(&fx, &View::All, "multi\nline", &all, 0, 0, fwd);
    assert_eq!(at(&o), Some((2, 0)));
    // Across the closing quote and the delimiter: rejected.
    let o = search(&fx, &View::All, "line\",q", &all, 0, 0, fwd);
    assert_eq!(o.hit, None);
    // The same through the record path (a filtered view of every row).
    let every = View::Filtered {
        rows: Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter(0..6u64))),
        expr: "true".into(),
        columns: vec![],
    };
    let o = search(&fx, &every, "a,b", &all, 0, 0, fwd);
    assert_eq!((at(&o), o.hit.unwrap().pos), (Some((3, 0)), 3));
    assert_eq!(search(&fx, &every, "b\ncd", &all, 0, 0, fwd).hit, None);
}

#[test]
fn hidden_columns_and_col_prefix() {
    let fx = fixture(b"customer,city\nalice,becker town\nbecker,paris\nzed,becker\n");
    // `city` hidden: only `customer` is searched.
    let o = search(&fx, &View::All, "becker", &[0], 0, 0, Direction::Forward);
    assert_eq!(at(&o), Some((1, 0)));
    // Both visible: row 0's city comes first.
    let o = search(&fx, &View::All, "becker", &[0, 1], 0, 0, Direction::Forward);
    assert_eq!(at(&o), Some((0, 1)));
    // `col:` only matches that column.
    let o = search(
        &fx,
        &View::All,
        "customer: becker",
        &[0, 1],
        0,
        0,
        Direction::Forward,
    );
    assert_eq!(at(&o), Some((1, 0)));
    let o = search(
        &fx,
        &View::All,
        "city: re:^becker$",
        &[0, 1],
        0,
        0,
        Direction::Forward,
    );
    assert_eq!(at(&o), Some((2, 1)));
    // Display order decides the order of cells in a row.
    let o = search(&fx, &View::All, "e", &[1, 0], 0, 1, Direction::Forward);
    assert_eq!(at(&o), Some((0, 0)), "after city (slot 0) comes customer");
}

#[test]
fn regex_anchors_per_field() {
    let fx = fixture(b"id,ref\n1,ORD-12\n2,xORD-3\n3,ORD-77x\n4,ORD-5\n");
    let o = search(
        &fx,
        &View::All,
        "re:^ORD-\\d+$",
        &[0, 1],
        0,
        0,
        Direction::Forward,
    );
    assert_eq!(at(&o), Some((0, 1)));
    let o = search(
        &fx,
        &View::All,
        "re:^ORD-\\d+$",
        &[0, 1],
        0,
        1,
        Direction::Forward,
    );
    assert_eq!((at(&o), o.wrapped), (Some((3, 1)), false));
    let o = search(
        &fx,
        &View::All,
        "re:^ORD-\\d+$",
        &[0, 1],
        3,
        1,
        Direction::Forward,
    );
    assert_eq!((at(&o), o.wrapped), (Some((0, 1)), true));
    let o = search(
        &fx,
        &View::All,
        "re:^ORD-\\d+$",
        &[0, 1],
        3,
        1,
        Direction::Backward,
    );
    assert_eq!((at(&o), o.wrapped), (Some((0, 1)), false));
}

#[test]
fn wrap_and_only_the_cursor_cell() {
    let fx = fixture(b"a,b\nx,y\nfoo,z\nq,r\n");
    let o = search(&fx, &View::All, "foo", &[0, 1], 2, 0, Direction::Forward);
    assert_eq!((at(&o), o.wrapped), (Some((1, 0)), true));
    let o = search(&fx, &View::All, "foo", &[0, 1], 1, 0, Direction::Backward);
    assert_eq!((at(&o), o.wrapped), (Some((1, 0)), true));
    let o = search(
        &fx,
        &View::All,
        "nothing",
        &[0, 1],
        1,
        0,
        Direction::Forward,
    );
    assert_eq!(
        o,
        SearchOutcome {
            hit: None,
            wrapped: true
        }
    );
}

#[test]
fn windows_1252_needles() {
    let content = b"name,city\nRen\xe9,Z\xfcrich\nBob,Paris\n\x80uro,x\n";
    let d = Dialect {
        encoding: Encoding::Windows1252,
        ..Dialect::default()
    };
    let fx = fixture_with(content, Some(d));
    let o = search(&fx, &View::All, "Zürich", &[0, 1], 0, 1, Direction::Forward);
    assert_eq!(at(&o), Some((0, 1)), "wraps to row 0's city");
    let o = search(&fx, &View::All, "é", &[0, 1], 1, 0, Direction::Forward);
    assert_eq!(at(&o), Some((0, 0)));
    let o = search(&fx, &View::All, "€uro", &[0, 1], 0, 0, Direction::Forward);
    assert_eq!(at(&o), Some((2, 0)));
    let o = search(
        &fx,
        &View::All,
        "re:^Ren.$",
        &[0, 1],
        2,
        0,
        Direction::Forward,
    );
    assert_eq!(at(&o), Some((0, 0)));
    assert_eq!(o.hit.unwrap().byte_range_in_value, 0..5, "decoded text");
    // Not encodable in Windows-1252: no match.
    let o = search(&fx, &View::All, "日本", &[0, 1], 0, 0, Direction::Forward);
    assert_eq!(o.hit, None);
}

// ---------------------------------------------------------------------------
// Reference comparison
// ---------------------------------------------------------------------------

/// Every matching cell in view order: `(pos, slot, row, col)`.
fn reference_cells(
    fx: &Fx,
    view: &View,
    q: &SearchQuery,
    display: &[usize],
) -> Vec<(u64, usize, u64, usize)> {
    let bytes = fx.src.bytes();
    let mut p = RecordParser::new(fx.src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut out = Vec::new();
    let len = view.len(&fx.index);
    for pos in 0..len {
        let row = view.row_id_at(pos).unwrap();
        let off = fx.index.offset_of(row, &fx.src, &mut p).unwrap();
        assert!(matches!(
            p.parse_at(bytes, off, &mut rec),
            ParseOutcome::Record { .. } | ParseOutcome::UnterminatedQuote { .. }
        ));
        for (slot, &c) in display.iter().enumerate() {
            if q.column.is_some_and(|qc| qc != c) {
                continue;
            }
            let field = fx.cols[c].source_index;
            if field >= rec.fields.len() {
                continue;
            }
            let v = p.field_value(bytes, &rec, field, &mut scratch);
            let text = String::from_utf8_lossy(v);
            if !q.find_in_cell(&text).is_empty() {
                out.push((pos, slot, row, c));
            }
        }
    }
    out
}

fn expected(
    cells: &[(u64, usize, u64, usize)],
    pos: u64,
    slot: usize,
    dir: Direction,
) -> Option<(u64, usize, bool)> {
    let key = |c: &(u64, usize, u64, usize)| (c.0, c.1);
    match dir {
        Direction::Forward => cells
            .iter()
            .find(|c| key(c) > (pos, slot))
            .map(|c| (c.2, c.3, false))
            .or_else(|| cells.first().map(|c| (c.2, c.3, true))),
        Direction::Backward => cells
            .iter()
            .rev()
            .find(|c| key(c) < (pos, slot))
            .map(|c| (c.2, c.3, false))
            .or_else(|| cells.last().map(|c| (c.2, c.3, true))),
    }
}

fn generated(rows: usize) -> Vec<u8> {
    let mut out = b"id,customer,city,note\n".to_vec();
    let mut s = 7u64;
    let mut r = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for i in 0..rows {
        let customer = ["becker", "Becker", "smith", "beckerson", "o'neil"][(r() % 5) as usize];
        let city = match r() % 6 {
            0 => "\"bec,ker\"".to_owned(),
            1 => "\"say \"\"becker\"\"\"".to_owned(),
            2 => "\"line\nbecker\"".to_owned(),
            _ => format!("city{}", r() % 50),
        };
        if r() % 40 == 0 {
            out.extend_from_slice(b"\n");
        }
        out.extend_from_slice(format!("{i},{customer},{city},n{}\n", r() % 7).as_bytes());
    }
    out
}

#[test]
fn matches_reference_on_all_filtered_and_ordered_views() {
    let fx = fixture(&generated(600));
    let total = fx.index.total_rows().unwrap();
    let filtered = View::Filtered {
        rows: Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter(
            (0..total).filter(|r| r % 3 != 1),
        ))),
        expr: "x".into(),
        columns: vec![],
    };
    let dir = tempfile::tempdir().unwrap();
    let mut perm: Vec<u64> = (0..total).filter(|r| r % 5 != 0).collect();
    perm.reverse();
    perm.swap(3, 200);
    let ordered = View::Ordered {
        list: RowIdList::from_ids(dir.path(), &perm).unwrap(),
        kind: OrderedKind::Sorted { keys: vec![] },
    };
    let displays: [&[usize]; 3] = [&[0, 1, 2, 3], &[2, 1], &[3, 1, 2]];
    let queries = [
        "becker",
        "bec,ker",
        "\"becker\"",
        "line\nbecker",
        "customer: becker",
        "city: re:^city1",
        "re:^[Bb]ecker$",
        "n3",
        "zzz",
    ];
    for view in [View::All, filtered, ordered] {
        let len = view.len(&fx.index);
        for display in displays {
            for q in queries {
                let query = parse_search(q, &fx.cols).unwrap();
                let cells = reference_cells(&fx, &view, &query, display);
                let query = Arc::new(query);
                for pos in (0..len).step_by(37).chain([0, len - 1]) {
                    for slot in [0, display.len() - 1] {
                        for d in [Direction::Forward, Direction::Backward] {
                            let req = SearchRequest::new(
                                Arc::clone(&query),
                                &fx.cols,
                                display,
                                pos,
                                display[slot],
                                d,
                            );
                            let o = search_blocking(
                                &req,
                                &view,
                                &fx.src,
                                &fx.index,
                                &CancellationToken::new(),
                                &SearchStatus::default(),
                            )
                            .unwrap();
                            let want = expected(&cells, pos, slot, d);
                            let got = o.hit.as_ref().map(|h| (h.row_id, h.col, o.wrapped));
                            assert_eq!(
                                got,
                                want,
                                "{} q={q:?} display={display:?} pos={pos} slot={slot} {d:?}",
                                view.label()
                            );
                            if let Some(SearchHit { pos: p, row_id, .. }) = o.hit {
                                assert_eq!(view.row_id_at(p), Some(row_id));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn cancellation_and_growing_index() {
    let content = generated(20_000);
    let fx = fixture(&content);
    let query = Arc::new(parse_search("re:^n6$", &fx.cols).unwrap());
    let req = SearchRequest::new(query, &fx.cols, &[0, 1, 2, 3], 0, 0, Direction::Forward);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let r = search_blocking(
        &req,
        &View::All,
        &fx.src,
        &fx.index,
        &cancel,
        &SearchStatus::default(),
    );
    assert!(r.unwrap_err().is_cancelled());

    // A needle only in the last row, while the index is still building.
    let mut content = generated(20_000);
    content.extend_from_slice(b"20000,needle-at-the-end,x,y\n");
    let report = sniff(&content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let (_f, src) = temp_source(&content, report.dialect);
    let cols = src_cols(&src);
    let rt = runtime();
    for q in ["needle-at-the-end", "re:needle-at"] {
        let index = Arc::new(RowIndex::with_stride(src.data_start(), 8));
        let exec = Executor::with_handle(rt.handle().clone(), 2);
        let query = Arc::new(parse_search(q, &cols).unwrap());
        let req = SearchRequest::new(query, &cols, &[0, 1, 2, 3], 0, 0, Direction::Forward);
        let status = Arc::new(SearchStatus::default());
        let o = rt.block_on(async {
            let s = tokio::spawn(run_search(
                req,
                View::All,
                Arc::clone(&src),
                Arc::clone(&index),
                exec.clone(),
                CancellationToken::new(),
                Arc::clone(&status),
            ));
            tokio::time::sleep(Duration::from_millis(30)).await;
            build_index(
                Arc::clone(&src),
                Arc::clone(&index),
                report.clone(),
                exec,
                CancellationToken::new(),
                IndexOptions {
                    chunk_size: 4096,
                    batch: 1,
                    ..IndexOptions::default()
                },
            )
            .await
            .unwrap();
            s.await.unwrap().unwrap()
        });
        assert_eq!(at(&o), Some((20_000, 1)), "{q}");
        assert!(!o.wrapped);
        assert!(!status.waiting_for_index());
    }
}
