//! M4-03: views (`All`, `Filtered`, `Ordered`).

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use roaring::RoaringTreemap;
use tachy_core::view::{FilterRows, OrderedKind, RowIdList, View};

/// Acceptance: rendering a 50-row window of a 100M-row filtered view takes
/// < 1 ms (one `select`, then iteration).
#[test]
fn window_of_a_100m_row_filtered_view_is_fast() {
    // Every third row of 300M: 100M rows over ~4,600 containers.
    let bitmap = RoaringTreemap::from_sorted_iter((0..100_000_000u64).map(|i| i * 3)).unwrap();
    let v = View::Filtered {
        rows: Arc::new(FilterRows::from_bitmap(bitmap)),
        expr: "x".into(),
        columns: vec![],
    };
    let mut best = Duration::MAX;
    for first in [0, 12_345_678, 50_000_000, 99_999_950] {
        let t = Instant::now();
        let ids = v.row_ids(first, 50);
        best = best.min(t.elapsed());
        assert_eq!(ids.len(), 50);
        assert_eq!(ids[0], first * 3);
        assert_eq!(ids[49], (first + 49) * 3);
    }
    let t = Instant::now();
    let ids = v.row_ids(77_777_777, 50);
    let elapsed = t.elapsed();
    assert_eq!(ids[0], 77_777_777 * 3);
    assert!(elapsed < Duration::from_millis(1), "took {elapsed:?}");
    assert_eq!(v.position_of(77_777_777 * 3), Some(77_777_777));
    assert_eq!(v.position_of(77_777_777 * 3 + 1), None);
    eprintln!("50-row window: {elapsed:?} (best {best:?})");
}

/// A reader sees a growing `RowIdList` consistently while the writer appends.
#[test]
fn growing_list_read_while_written() {
    let dir = tempfile::tempdir().unwrap();
    let (list, mut w) = RowIdList::create_in(dir.path()).unwrap();
    let view = View::Ordered {
        list: Arc::clone(&list),
        kind: OrderedKind::Sorted { keys: vec![] },
    };
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut checks = 0u64;
            while !done.load(Ordering::Acquire) {
                let len = list.len();
                if len > 0 {
                    let first = len.saturating_sub(100);
                    let ids = view.row_ids(first, 100);
                    assert_eq!(ids.len() as u64, len - first);
                    for (i, id) in ids.iter().enumerate() {
                        assert_eq!(*id, (first + i as u64) * 2);
                    }
                    checks += 1;
                }
            }
            checks
        })
    };
    for i in 0..1_000_000u64 {
        w.push(i * 2).unwrap();
    }
    let list = w.finish().unwrap();
    done.store(true, Ordering::Release);
    reader.join().unwrap();
    assert_eq!(list.len(), 1_000_000);
    assert_eq!(list.get(999_999), Some(1_999_998));
}
