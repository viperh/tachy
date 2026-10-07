//! M1-03: `parse_at` and `skip` do no heap allocation for records with up to
//! 32 fields. A dedicated test binary, so the counting global allocator sees
//! only this test.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

use tachy_core::{
    dialect::{Dialect, EscapeStyle},
    parse::{ParseOutcome, RecordParser, RecordRanges},
};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator and only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn parse_at_and_skip_do_not_allocate() {
    // Build the input before counting.
    let mut bytes = Vec::new();
    for i in 0..500 {
        let fields: Vec<String> = (0..32)
            .map(|c| match c % 4 {
                0 => format!("{i}"),
                1 => format!("\"q,{c}\""),
                2 => format!("\"d\"\"{c}\""),
                _ => format!("\"multi\nline {c}\""),
            })
            .collect();
        bytes.extend_from_slice(fields.join(",").as_bytes());
        bytes.extend_from_slice(if i % 2 == 0 { b"\r\n" } else { b"\n" });
        if i % 10 == 0 {
            bytes.extend_from_slice(b"\n# comment, \"\n");
        }
    }
    for escape in [EscapeStyle::Doubled, EscapeStyle::Backslash] {
        let d = Dialect {
            escape,
            comment: Some(b'#'),
            ..Dialect::default()
        };
        let mut p = RecordParser::new(&d);
        let mut rec = RecordRanges::default();

        let before = ALLOCATIONS.load(Ordering::SeqCst);
        let mut pos = 0;
        let mut rows = 0;
        let mut max_fields = 0;
        loop {
            match p.parse_at(&bytes, pos, &mut rec) {
                ParseOutcome::Eof => break,
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                    rows += 1;
                    max_fields = max_fields.max(rec.fields.len());
                    pos = next;
                }
            }
        }
        let skipped = p.skip(&bytes, 0, 400);
        let after = ALLOCATIONS.load(Ordering::SeqCst);

        assert_eq!(rows, 500);
        assert!(max_fields <= 32);
        assert!(matches!(
            skipped,
            tachy_core::parse::SkipOutcome::Skipped { .. }
        ));
        assert_eq!(after - before, 0, "{escape:?}: parse_at/skip allocated");
    }
}
