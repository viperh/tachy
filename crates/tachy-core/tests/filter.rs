//! M4-04: the parallel filter job.
//!
//! - Parallel == sequential, for many expressions on generated files, with
//!   and without the `memmem` pre-filter, with blank lines and quoted
//!   newlines, for `All`, `Filtered` and `Ordered` parents.
//! - In-order publishing when chunks complete out of order (injected).
//! - Following a growing index, cancellation and pause.

mod support;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use roaring::RoaringTreemap;
use support::{runtime, temp_source};
use tachy_core::{
    column::{ColumnMeta, ColumnName},
    dialect::{DEFAULT_SAMPLE_BYTES, Dialect, DialectOverrides, sniff},
    exec::Executor,
    filter::{FilterOptions, FilterOutput, ParentRows, run_filter_with},
    index::{IndexOptions, IndexPath, RowIndex, build_index},
    jobs::{FilterProgress, JobError, PauseToken},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    query::{self, EvalScratch, Predicate},
    source::Source,
    types::{ColType, NullSet},
    view::{FilterRows, RowIdList},
};
use tokio_util::sync::CancellationToken;

/// A small deterministic PRNG.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy)]
struct Gen {
    rows: usize,
    blank_lines: bool,
    quoted_newlines: bool,
    trailing_newline: bool,
    seed: u64,
}

fn generate(g: Gen) -> Vec<u8> {
    let mut rng = Rng(g.seed | 1);
    let mut out = b"id,name,country,price,qty,note\n".to_vec();
    for i in 0..g.rows {
        if g.blank_lines && rng.below(50) == 0 {
            out.extend_from_slice(b"\n");
        }
        let country = ["FR", "DE", "US", "JP", ""][rng.below(5) as usize];
        let note = match rng.below(8) {
            0 => "\"has, comma abc\"".to_owned(),
            1 => "\"say \"\"abc\"\" twice\"".to_owned(),
            2 if g.quoted_newlines => "\"multi\nline abc\"".to_owned(),
            3 => String::new(),
            _ => format!("plain{}", rng.below(1000)),
        };
        out.extend_from_slice(
            format!(
                "{i},user{},{country},{}.{:02},{},{note}",
                rng.below(5000),
                rng.below(200),
                rng.below(100),
                rng.below(100),
            )
            .as_bytes(),
        );
        if i + 1 < g.rows || g.trailing_newline {
            out.extend_from_slice(b"\n");
        }
    }
    out
}

fn columns(src: &Source) -> Vec<ColumnMeta> {
    let types = [
        ColType::I64,
        ColType::Str,
        ColType::Enum,
        ColType::F64,
        ColType::I64,
        ColType::Str,
    ];
    src.column_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let mut c = ColumnMeta::new(n, i, false);
            c.set_inferred(types[i]);
            c
        })
        .collect()
}

fn compile(src: &Source, q: &str) -> Predicate {
    let cols = columns(src);
    let names: Vec<ColumnName> = cols.iter().map(|c| c.name.clone()).collect();
    let ast = query::parse(q).unwrap();
    let resolved = query::resolve(ast, &names).unwrap();
    query::compile(&resolved, &cols, src.dialect(), &NullSet::default()).unwrap()
}

/// Random expressions; most have a required literal.
fn expressions(seed: u64, n: usize) -> Vec<String> {
    let mut rng = Rng(seed | 1);
    (0..n)
        .map(|_| match rng.below(10) {
            0 => format!("price > {}", rng.below(200)),
            1 => "country == \"DE\"".to_owned(),
            2 => format!("name contains \"user{}\"", rng.below(60)),
            3 => "note contains \"abc\"".to_owned(),
            4 => format!("name starts \"user{}\"", rng.below(40)),
            5 => format!("qty < {} && name contains \"er1\"", rng.below(100)),
            6 => "country == \"FR\" || price < 10".to_owned(),
            7 => format!("!(qty > {})", rng.below(100)),
            8 => "note ~ \"l.ne\"".to_owned(),
            _ => format!("name == \"user{}\"", rng.below(5000)),
        })
        .collect()
}

/// Sequential reference: every record parsed and evaluated in file order.
fn reference(src: &Source, p: &Predicate) -> Vec<u64> {
    let bytes = src.bytes();
    let mut parser = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = EvalScratch::new();
    let mut pos = src.data_start();
    let mut out = Vec::new();
    let mut row = 0;
    loop {
        match parser.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                if p.eval_record(bytes, &rec, &mut scratch) {
                    out.push(row);
                }
                row += 1;
                pos = next;
            }
            ParseOutcome::Eof => return out,
        }
    }
}

struct Fixture {
    _file: tempfile::NamedTempFile,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    path: IndexPath,
}

/// A file sniffed and fully indexed with a small stride (many checkpoints,
/// so small chunks still align).
fn fixture(g: Gen) -> Fixture {
    let content = generate(g);
    let report = sniff(&content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let (file, src) = temp_source(&content, report.dialect);
    let rt = runtime();
    let index = Arc::new(RowIndex::with_stride(src.data_start(), 16));
    let summary = rt
        .block_on(build_index(
            Arc::clone(&src),
            Arc::clone(&index),
            report,
            Executor::with_handle(rt.handle().clone(), 4),
            CancellationToken::new(),
            IndexOptions {
                chunk_size: 4096,
                ..IndexOptions::default()
            },
        ))
        .unwrap();
    Fixture {
        _file: file,
        src,
        index,
        path: summary.path_used,
    }
}

fn small_chunks(prefilter: bool) -> FilterOptions {
    FilterOptions {
        prefilter,
        chunk_bytes: 8 << 10,
        first_chunk_bytes: 1 << 10,
        batch_rows: 97,
        poll: Duration::from_millis(5),
        chunk_hook: None,
    }
}

fn run(
    f: &Fixture,
    parent: ParentRows,
    pred: &Predicate,
    threads: usize,
    opts: FilterOptions,
) -> (FilterOutput, Result<(), JobError>, Arc<FilterProgress>) {
    let rt = runtime();
    let dir = std::env::temp_dir();
    let out = FilterOutput::for_parent(&parent, &dir).unwrap();
    let handle = match &out {
        FilterOutput::Bitmap(rows) => FilterOutput::Bitmap(Arc::clone(rows)),
        FilterOutput::List(_) => unreachable!("list outputs go through run_list"),
    };
    let progress = Arc::new(FilterProgress::default());
    let r = rt.block_on(run_filter_with(
        parent,
        Arc::clone(&f.src),
        Arc::clone(&f.index),
        pred.clone(),
        out,
        Executor::with_handle(rt.handle().clone(), threads),
        CancellationToken::new(),
        PauseToken::new(),
        Arc::clone(&progress),
        opts,
    ));
    (handle, r, progress)
}

fn bitmap_ids(out: &FilterOutput) -> Vec<u64> {
    out.rows().unwrap().with_bitmap(|b| b.iter().collect())
}

fn run_list(f: &Fixture, list: Arc<RowIdList>, pred: &Predicate, threads: usize) -> Arc<RowIdList> {
    let rt = runtime();
    let parent = ParentRows::Ordered(list);
    let out = FilterOutput::for_parent(&parent, &std::env::temp_dir()).unwrap();
    let result = Arc::clone(out.list().unwrap());
    rt.block_on(run_filter_with(
        parent,
        Arc::clone(&f.src),
        Arc::clone(&f.index),
        pred.clone(),
        out,
        Executor::with_handle(rt.handle().clone(), threads),
        CancellationToken::new(),
        PauseToken::new(),
        Arc::new(FilterProgress::default()),
        small_chunks(false),
    ))
    .unwrap();
    assert!(!result.is_growing());
    result
}

const GENS: [Gen; 4] = [
    Gen {
        rows: 6000,
        blank_lines: false,
        quoted_newlines: false,
        trailing_newline: true,
        seed: 1,
    },
    Gen {
        rows: 5000,
        blank_lines: true,
        quoted_newlines: false,
        trailing_newline: false,
        seed: 2,
    },
    Gen {
        rows: 5000,
        blank_lines: false,
        quoted_newlines: true,
        trailing_newline: true,
        seed: 3,
    },
    Gen {
        rows: 4000,
        blank_lines: true,
        quoted_newlines: true,
        trailing_newline: false,
        seed: 4,
    },
];

#[test]
fn parallel_equals_sequential_all_parent() {
    for g in GENS {
        let f = fixture(g);
        for q in expressions(g.seed * 31, 12) {
            let p = compile(&f.src, &q);
            let want = reference(&f.src, &p);
            for prefilter in [false, true] {
                for threads in [1, 3, 8] {
                    let (out, r, progress) =
                        run(&f, ParentRows::All, &p, threads, small_chunks(prefilter));
                    r.unwrap();
                    assert_eq!(
                        bitmap_ids(&out),
                        want,
                        "{q} prefilter={prefilter} threads={threads} gen seed {}",
                        g.seed
                    );
                    assert!(!out.rows().unwrap().is_growing());
                    assert_eq!(progress.matches(), want.len() as u64);
                    assert_eq!(
                        progress.bytes_done(),
                        f.src.len() - f.src.data_start(),
                        "{q}"
                    );
                }
            }
        }
    }
}

#[test]
fn prefilter_is_used_on_fast_files_and_still_exact_with_default_chunks() {
    let f = fixture(GENS[0]);
    assert_eq!(f.path, IndexPath::Fast);
    for q in ["note contains \"abc\"", "name contains \"user42\""] {
        let p = compile(&f.src, q);
        assert!(p.required_literal().is_some());
        let want = reference(&f.src, &p);
        let (out, r, _) = run(
            &f,
            ParentRows::All,
            &p,
            4,
            FilterOptions {
                prefilter: true,
                ..FilterOptions::default()
            },
        );
        r.unwrap();
        assert_eq!(bitmap_ids(&out), want, "{q}");
    }
}

#[test]
fn refinement_filters_the_parent_only() {
    let f = fixture(GENS[2]);
    let parent_pred = compile(&f.src, "price > 50");
    let parent_ids = reference(&f.src, &parent_pred);
    let parent = Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter(
        parent_ids.iter().copied(),
    )));
    for q in expressions(77, 8) {
        let p = compile(&f.src, &q);
        let all: std::collections::HashSet<u64> = reference(&f.src, &p).into_iter().collect();
        let want: Vec<u64> = parent_ids
            .iter()
            .copied()
            .filter(|id| all.contains(id))
            .collect();
        for threads in [1, 8] {
            let (out, r, _) = run(
                &f,
                ParentRows::Filtered(Arc::clone(&parent)),
                &p,
                threads,
                small_chunks(true),
            );
            r.unwrap();
            let got = bitmap_ids(&out);
            assert_eq!(got, want, "{q}");
            assert!(got.len() <= parent_ids.len());
        }
    }
}

#[test]
fn ordered_parent_keeps_its_order() {
    let f = fixture(GENS[3]);
    let total = f.index.total_rows().unwrap();
    // A pseudo-random permutation of all rows.
    let mut perm: Vec<u64> = (0..total).collect();
    let mut rng = Rng(99);
    for i in (1..perm.len()).rev() {
        perm.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let dir = tempfile::tempdir().unwrap();
    let list = RowIdList::from_ids(dir.path(), &perm).unwrap();
    for q in expressions(5, 6) {
        let p = compile(&f.src, &q);
        let all: std::collections::HashSet<u64> = reference(&f.src, &p).into_iter().collect();
        let want: Vec<u64> = perm.iter().copied().filter(|id| all.contains(id)).collect();
        for threads in [1, 8] {
            let got = run_list(&f, Arc::clone(&list), &p, threads);
            assert_eq!(got.read(0, perm.len()), want, "{q}");
            assert_eq!(got.len(), want.len() as u64);
        }
    }
}

#[test]
fn publishes_in_order_when_chunks_finish_out_of_order() {
    let f = fixture(GENS[0]);
    let p = compile(&f.src, "price > 20");
    let want = reference(&f.src, &p);
    // Later chunks finish first: chunk i sleeps (40 - i) ms.
    let completions = Arc::new(Mutex::new(Vec::new()));
    let c2 = Arc::clone(&completions);
    let mut opts = small_chunks(false);
    opts.chunk_bytes = 16 << 10;
    opts.first_chunk_bytes = 16 << 10;
    opts.chunk_hook = Some(Arc::new(move |idx| {
        std::thread::sleep(Duration::from_millis(40u64.saturating_sub(idx as u64 * 3)));
        c2.lock().unwrap().push(idx);
    }));
    let rt = runtime();
    let rows = Arc::new(FilterRows::new_growing());
    let stop = Arc::new(AtomicBool::new(false));
    // Observer: every snapshot must be a prefix of the final result.
    let observer = {
        let (rows, stop, want) = (Arc::clone(&rows), Arc::clone(&stop), want.clone());
        std::thread::spawn(move || {
            let mut snapshots = 0;
            while !stop.load(Ordering::Acquire) {
                let snap: Vec<u64> = rows.with_bitmap(|b| b.iter().collect());
                assert_eq!(snap[..], want[..snap.len()], "a gap was published");
                assert!(rows.len() as usize <= want.len());
                snapshots += 1;
                std::thread::sleep(Duration::from_micros(200));
            }
            snapshots
        })
    };
    rt.block_on(run_filter_with(
        ParentRows::All,
        Arc::clone(&f.src),
        Arc::clone(&f.index),
        p,
        FilterOutput::Bitmap(Arc::clone(&rows)),
        Executor::with_handle(rt.handle().clone(), 6),
        CancellationToken::new(),
        PauseToken::new(),
        Arc::new(FilterProgress::default()),
        opts,
    ))
    .unwrap();
    stop.store(true, Ordering::Release);
    assert!(observer.join().unwrap() > 0);
    let order = completions.lock().unwrap().clone();
    assert!(
        order.windows(2).any(|w| w[0] > w[1]),
        "chunks completed in order: {order:?}"
    );
    assert_eq!(rows.with_bitmap(|b| b.iter().collect::<Vec<_>>()), want);
}

#[test]
fn follows_a_growing_index() {
    let g = Gen {
        rows: 40_000,
        blank_lines: true,
        quoted_newlines: false,
        trailing_newline: true,
        seed: 11,
    };
    let content = generate(g);
    let report = sniff(&content, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
    let (_file, src) = temp_source(&content, report.dialect);
    let p = compile(&src, "name contains \"user1\"");
    let want = reference(&src, &p);
    let rt = runtime();
    for _ in 0..3 {
        let index = Arc::new(RowIndex::with_stride(src.data_start(), 16));
        let exec = Executor::with_handle(rt.handle().clone(), 4);
        let rows = Arc::new(FilterRows::new_growing());
        let (got, summary) = rt.block_on(async {
            let filter = tokio::spawn(run_filter_with(
                ParentRows::All,
                Arc::clone(&src),
                Arc::clone(&index),
                p.clone(),
                FilterOutput::Bitmap(Arc::clone(&rows)),
                exec.clone(),
                CancellationToken::new(),
                PauseToken::new(),
                Arc::new(FilterProgress::default()),
                small_chunks(true),
            ));
            // Start indexing a little later, in small batches.
            tokio::time::sleep(Duration::from_millis(5)).await;
            let summary = build_index(
                Arc::clone(&src),
                Arc::clone(&index),
                report.clone(),
                exec,
                CancellationToken::new(),
                IndexOptions {
                    chunk_size: 2048,
                    batch: 2,
                    ..IndexOptions::default()
                },
            )
            .await
            .unwrap();
            (filter.await.unwrap(), summary)
        });
        got.unwrap();
        assert_eq!(summary.total_rows, 40_000);
        assert_eq!(rows.with_bitmap(|b| b.iter().collect::<Vec<_>>()), want);
        assert!(!rows.is_growing());
    }
}

#[test]
fn cancel_and_pause() {
    let f = fixture(GENS[1]);
    let p = compile(&f.src, "price > 1");
    let rt = runtime();
    // Paused, then cancelled without resuming: ends promptly as cancelled.
    let cancel = CancellationToken::new();
    let pause = PauseToken::new();
    pause.pause();
    let rows = Arc::new(FilterRows::new_growing());
    let started = std::time::Instant::now();
    let r = rt.block_on(async {
        let job = tokio::spawn(run_filter_with(
            ParentRows::All,
            Arc::clone(&f.src),
            Arc::clone(&f.index),
            p.clone(),
            FilterOutput::Bitmap(Arc::clone(&rows)),
            Executor::with_handle(rt.handle().clone(), 2),
            cancel.clone(),
            pause.clone(),
            Arc::new(FilterProgress::default()),
            small_chunks(false),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!job.is_finished(), "a paused job must not finish");
        assert!(rows.is_empty());
        cancel.cancel();
        job.await.unwrap()
    });
    assert!(r.unwrap_err().is_cancelled());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        rows.is_growing(),
        "a cancelled filter is never marked complete"
    );

    // Paused, then resumed: completes with the full result.
    let pause = PauseToken::new();
    pause.pause();
    let rows = Arc::new(FilterRows::new_growing());
    rt.block_on(async {
        let job = tokio::spawn(run_filter_with(
            ParentRows::All,
            Arc::clone(&f.src),
            Arc::clone(&f.index),
            p.clone(),
            FilterOutput::Bitmap(Arc::clone(&rows)),
            Executor::with_handle(rt.handle().clone(), 2),
            CancellationToken::new(),
            pause.clone(),
            Arc::new(FilterProgress::default()),
            small_chunks(false),
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        pause.resume();
        job.await.unwrap().unwrap();
    });
    assert_eq!(
        rows.with_bitmap(|b| b.iter().collect::<Vec<_>>()),
        reference(&f.src, &p)
    );
}

#[test]
fn zero_matches_and_empty_file() {
    let f = fixture(GENS[0]);
    let p = compile(&f.src, "price > 100000");
    let (out, r, _) = run(&f, ParentRows::All, &p, 4, small_chunks(true));
    r.unwrap();
    assert!(out.rows().unwrap().is_empty());
    assert!(!out.rows().unwrap().is_growing());

    let (_file, src) = temp_source(b"id,name,country,price,qty,note\n", Dialect::default());
    let index = Arc::new(RowIndex::for_source(&src));
    index.finish(0, src.len());
    let p = compile(&src, "price > 1");
    let rt = runtime();
    let rows = Arc::new(FilterRows::new_growing());
    rt.block_on(run_filter_with(
        ParentRows::All,
        Arc::clone(&src),
        index,
        p,
        FilterOutput::Bitmap(Arc::clone(&rows)),
        Executor::with_handle(rt.handle().clone(), 2),
        CancellationToken::new(),
        PauseToken::new(),
        Arc::new(FilterProgress::default()),
        FilterOptions::default(),
    ))
    .unwrap();
    assert!(rows.is_empty() && !rows.is_growing());
}
