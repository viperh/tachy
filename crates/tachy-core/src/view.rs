//! Views: an ordered selection of row ids over a source (spec §4.2, §8.1;
//! M4-03).
//!
//! All table rendering goes through a [`View`], which maps a **view
//! position** to a **row id** (the 0-based index of a record in the file,
//! header excluded). Views stack (the UI's `ViewStack`), so filters and sorts
//! combine in any order.
//!
//! - [`View::All`]: identity, position `p` → row id `p`.
//! - [`View::Filtered`]: a [`FilterRows`] bitmap of row ids, in file order.
//! - [`View::Ordered`]: a [`RowIdList`] permutation file (a sort, or a filter
//!   over a sorted view, which keeps the parent's order, D10).
//!
//! Lengths are lower bounds while a view is growing (a running filter, or
//! the index while it builds).

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use memmap2::Mmap;
use roaring::RoaringTreemap;
use tempfile::NamedTempFile;

use crate::{
    dupes::{DupeMode, DupeSpec},
    index::RowIndex,
    sort::SortKey,
};

/// What an [`View::Ordered`] view holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrderedKind {
    /// The result of a sort (M5-03).
    Sorted {
        /// The sort keys, in priority order.
        keys: Vec<SortKey>,
    },
    /// A filter over a sorted view, in the sorted parent's order (D10).
    FilteredSorted {
        /// The filter text.
        expr: String,
        /// Columns referenced by the filter (header highlighting).
        columns: Vec<usize>,
        /// The parent's sort keys.
        keys: Vec<SortKey>,
    },
    /// `dupes` / `dedupe` over a sorted view, in the parent's order.
    Dupes {
        /// The key and mode.
        spec: DupeSpec,
        /// The parent's sort keys.
        keys: Vec<SortKey>,
    },
}

/// A view over a source: view position → row id. Cheap to clone.
#[derive(Clone, Debug)]
pub enum View {
    /// Every row, in file order.
    All,
    /// The rows matching a filter, in file order (§8.2).
    Filtered {
        /// The matching row ids.
        rows: Arc<FilterRows>,
        /// The filter text.
        expr: String,
        /// Columns referenced by the filter (header highlighting).
        columns: Vec<usize>,
    },
    /// The rows selected by `dupes` / `dedupe` (`crate::dupes`), in file
    /// order.
    Dupes {
        /// The selected row ids.
        rows: Arc<FilterRows>,
        /// The key and mode.
        spec: DupeSpec,
    },
    /// Rows in the order of a permutation file.
    Ordered {
        /// The row ids, in view order.
        list: Arc<RowIdList>,
        /// What produced it.
        kind: OrderedKind,
    },
}

impl View {
    /// Number of positions: `index.indexed_rows()` for `All`, the published
    /// length otherwise. A lower bound while [`View::is_growing`].
    pub fn len(&self, index: &RowIndex) -> u64 {
        match self {
            View::All => index.indexed_rows(),
            View::Filtered { rows, .. } | View::Dupes { rows, .. } => rows.len(),
            View::Ordered { list, .. } => list.len(),
        }
    }

    /// Whether the view has no rows (yet).
    pub fn is_empty(&self, index: &RowIndex) -> bool {
        self.len(index) == 0
    }

    /// Whether rows may still be added: the index is building (`All`) or the
    /// producing job is running.
    pub fn is_growing(&self, index: &RowIndex) -> bool {
        match self {
            View::All => !index.is_complete(),
            View::Filtered { rows, .. } | View::Dupes { rows, .. } => rows.is_growing(),
            View::Ordered { list, .. } => list.is_growing(),
        }
    }

    /// The row id at view position `pos`, or `None` past the end. For `All`
    /// this is the identity without bounds checking (clamp with
    /// [`View::len`]).
    pub fn row_id_at(&self, pos: u64) -> Option<u64> {
        match self {
            View::All => Some(pos),
            View::Filtered { rows, .. } | View::Dupes { rows, .. } => rows.row_id_at(pos),
            View::Ordered { list, .. } => list.get(pos),
        }
    }

    /// Row ids of positions `first .. first + count`, fewer at the end. For
    /// rendering: `Filtered` does one `select`, then iterates; `Ordered` reads
    /// one slice of the mapping. For `All` it is the identity without bounds
    /// checking (clamp `count` with [`View::len`]).
    pub fn row_ids(&self, first: u64, count: usize) -> Vec<u64> {
        match self {
            View::All => (first..first.saturating_add(count as u64)).collect(),
            View::Filtered { rows, .. } | View::Dupes { rows, .. } => rows.row_ids(first, count),
            View::Ordered { list, .. } => list.read(first, count),
        }
    }

    /// The view position of `row_id`: identity for `All`, its rank for
    /// `Filtered`. `None` for `Ordered`, which has no inverse index (a linear
    /// scan of 400M ids is too slow; D1 restores positions from saved cursor
    /// state instead), and for rows not in the view.
    pub fn position_of(&self, row_id: u64) -> Option<u64> {
        match self {
            View::All => Some(row_id),
            View::Filtered { rows, .. } | View::Dupes { rows, .. } => rows.position_of(row_id),
            View::Ordered { .. } => None,
        }
    }

    /// The top-bar label (§8.1): `all rows`, `filtered` or `sorted`. A filter
    /// over a sorted view is `filtered` (D10). `dupes` views are
    /// `duplicates`, `dedupe` views `deduplicated`, over sorted views too.
    pub fn label(&self) -> &'static str {
        if let Some(spec) = self.dupe_spec() {
            return match spec.mode {
                DupeMode::Show => "duplicates",
                DupeMode::Remove => "deduplicated",
            };
        }
        match self {
            View::All => "all rows",
            View::Filtered { .. }
            | View::Ordered {
                kind: OrderedKind::FilteredSorted { .. },
                ..
            } => "filtered",
            View::Ordered {
                kind: OrderedKind::Sorted { .. },
                ..
            } => "sorted",
            View::Dupes { .. }
            | View::Ordered {
                kind: OrderedKind::Dupes { .. },
                ..
            } => unreachable!("handled above"),
        }
    }

    /// The `dupes` / `dedupe` spec of a duplicates view.
    pub fn dupe_spec(&self) -> Option<&DupeSpec> {
        match self {
            View::Dupes { spec, .. }
            | View::Ordered {
                kind: OrderedKind::Dupes { spec, .. },
                ..
            } => Some(spec),
            _ => None,
        }
    }

    /// Sort keys for the header `▲`/`▼` indicators (M1-05); empty when the
    /// view is not sorted.
    pub fn sort_keys(&self) -> &[SortKey] {
        match self {
            View::Ordered {
                kind:
                    OrderedKind::Sorted { keys }
                    | OrderedKind::FilteredSorted { keys, .. }
                    | OrderedKind::Dupes { keys, .. },
                ..
            } => keys,
            _ => &[],
        }
    }

    /// Columns referenced by the view's filter (header highlighting, §12.4),
    /// or a duplicates view's key columns; empty for `All` and plain sorts.
    pub fn filter_columns(&self) -> &[usize] {
        if let Some(spec) = self.dupe_spec() {
            return &spec.columns;
        }
        match self {
            View::Filtered { columns, .. }
            | View::Ordered {
                kind: OrderedKind::FilteredSorted { columns, .. },
                ..
            } => columns,
            _ => &[],
        }
    }

    /// The filter text, if any.
    pub fn filter_expr(&self) -> Option<&str> {
        match self {
            View::Filtered { expr, .. }
            | View::Ordered {
                kind: OrderedKind::FilteredSorted { expr, .. },
                ..
            } => Some(expr),
            _ => None,
        }
    }

    /// Whether this is [`View::All`].
    pub fn is_all(&self) -> bool {
        matches!(self, View::All)
    }
}

// ---------------------------------------------------------------------------
// FilterRows
// ---------------------------------------------------------------------------

/// The row ids of a filtered view: a `RoaringTreemap` in file order, filled
/// while the filter job runs (§8.2 "results are live").
///
/// The writer (the filter job) inserts under the write lock and then
/// publishes `len`; readers take the read lock only for the duration of one
/// call.
#[derive(Debug, Default)]
pub struct FilterRows {
    bitmap: RwLock<RoaringTreemap>,
    len: AtomicU64,
    growing: AtomicBool,
}

impl FilterRows {
    /// An empty set that is still growing.
    pub fn new_growing() -> FilterRows {
        FilterRows {
            growing: AtomicBool::new(true),
            ..FilterRows::default()
        }
    }

    /// A finished set.
    pub fn from_bitmap(bitmap: RoaringTreemap) -> FilterRows {
        FilterRows {
            len: AtomicU64::new(bitmap.len()),
            bitmap: RwLock::new(bitmap),
            growing: AtomicBool::new(false),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, RoaringTreemap> {
        self.bitmap.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds row ids (writer side) and publishes the new length.
    pub fn insert_many(&self, ids: impl IntoIterator<Item = u64>) {
        let mut b = self.bitmap.write().unwrap_or_else(|e| e.into_inner());
        b.extend(ids);
        self.len.store(b.len(), Ordering::Release);
    }

    /// Unions a chunk's bitmap in (writer side) and publishes the new length.
    pub fn union_with(&self, other: &RoaringTreemap) {
        let mut b = self.bitmap.write().unwrap_or_else(|e| e.into_inner());
        *b |= other;
        self.len.store(b.len(), Ordering::Release);
    }

    /// Marks the set complete.
    pub fn finish(&self) {
        self.growing.store(false, Ordering::Release);
    }

    /// Published row count (a lower bound while growing).
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// No rows (yet).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the filter is still running.
    pub fn is_growing(&self) -> bool {
        self.growing.load(Ordering::Acquire)
    }

    /// Whether `row_id` matched.
    pub fn contains(&self, row_id: u64) -> bool {
        self.read().contains(row_id)
    }

    /// The row id at position `pos` (`select`).
    pub fn row_id_at(&self, pos: u64) -> Option<u64> {
        self.read().select(pos)
    }

    /// Row ids of positions `first .. first + count`: one `select`, then
    /// iteration from that value.
    pub fn row_ids(&self, first: u64, count: usize) -> Vec<u64> {
        let b = self.read();
        let Some(start) = b.select(first) else {
            return Vec::new();
        };
        let mut it = b.iter();
        it.advance_to(start);
        it.take(count).collect()
    }

    /// Position of `row_id` (`rank − 1`), if it matched.
    pub fn position_of(&self, row_id: u64) -> Option<u64> {
        let b = self.read();
        b.contains(row_id).then(|| b.rank(row_id) - 1)
    }

    /// Runs `f` on the bitmap under the read lock (e.g. to iterate the rows
    /// of a refinement filter or a sort).
    pub fn with_bitmap<R>(&self, f: impl FnOnce(&RoaringTreemap) -> R) -> R {
        f(&self.read())
    }
}

// ---------------------------------------------------------------------------
// RowIdList
// ---------------------------------------------------------------------------

/// Row ids published per block by [`RowIdWriter::push`].
pub const WRITE_BLOCK_IDS: u64 = 64 * 1024;

/// A permutation file: a flat array of `u64` row ids, **little-endian**,
/// memory-mapped for reading. Deleted when dropped (`NamedTempFile`), i.e.
/// when its view is popped, its tab closed or tachy exits.
///
/// It can grow while being written: a [`RowIdWriter`] appends to the file and
/// publishes `len` with `Release` after each block; readers re-map when they
/// need bytes past the current mapping (at most once per call).
#[derive(Debug)]
pub struct RowIdList {
    map: RwLock<Option<Mmap>>,
    len: AtomicU64,
    growing: AtomicBool,
    file: NamedTempFile,
}

impl RowIdList {
    /// Creates an empty, growing list in `dir` (`tachy-perm-*` file) and the
    /// writer that fills it.
    pub fn create_in(dir: &Path) -> io::Result<(Arc<RowIdList>, RowIdWriter)> {
        let file = tempfile::Builder::new()
            .prefix("tachy-perm-")
            .suffix(".bin")
            .tempfile_in(dir)?;
        let out = file.as_file().try_clone()?;
        let list = Arc::new(RowIdList {
            map: RwLock::new(None),
            len: AtomicU64::new(0),
            growing: AtomicBool::new(true),
            file,
        });
        let writer = RowIdWriter {
            list: Arc::clone(&list),
            out: BufWriter::with_capacity(1 << 20, out),
            written: 0,
        };
        Ok((list, writer))
    }

    /// A finished list holding `ids`, in `dir`.
    pub fn from_ids(dir: &Path, ids: &[u64]) -> io::Result<Arc<RowIdList>> {
        let (_, mut w) = RowIdList::create_in(dir)?;
        w.extend(ids)?;
        w.finish()
    }

    /// Published number of ids (a lower bound while growing).
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// No ids (yet).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the writer is still running.
    pub fn is_growing(&self) -> bool {
        self.growing.load(Ordering::Acquire)
    }

    /// Path of the backing file.
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// The id at position `pos`.
    pub fn get(&self, pos: u64) -> Option<u64> {
        self.read(pos, 1).first().copied()
    }

    /// Ids of positions `first .. first + count`, fewer at the end.
    pub fn read(&self, first: u64, count: usize) -> Vec<u64> {
        let len = self.len();
        if first >= len || count == 0 {
            return Vec::new();
        }
        let end = first.saturating_add(count as u64).min(len);
        let need = (end * 8) as usize;
        {
            let map = self.map.read().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = map.as_ref()
                && m.len() >= need
            {
                return decode(&m[(first * 8) as usize..need]);
            }
        }
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        if map.as_ref().is_none_or(|m| m.len() < need) {
            // SAFETY: the file is a private temp file. The only writer appends
            // (never truncates or rewrites published bytes) and the mapping
            // is only read below the published length.
            match unsafe { Mmap::map(self.file.as_file()) } {
                Ok(m) => *map = Some(m),
                Err(e) => {
                    tracing::warn!("mapping {}: {e}", self.path().display());
                    return Vec::new();
                }
            }
        }
        match map.as_ref() {
            Some(m) if m.len() >= need => decode(&m[(first * 8) as usize..need]),
            _ => Vec::new(),
        }
    }
}

fn decode(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect()
}

/// Appends row ids to a [`RowIdList`]. Publishes every
/// [`WRITE_BLOCK_IDS`] ids, on [`RowIdWriter::publish`] and on
/// [`RowIdWriter::finish`]. Dropping the writer without `finish` leaves the
/// list marked growing (a cancelled job drops the list too).
#[derive(Debug)]
pub struct RowIdWriter {
    list: Arc<RowIdList>,
    out: BufWriter<File>,
    written: u64,
}

impl RowIdWriter {
    /// Appends one id.
    pub fn push(&mut self, id: u64) -> io::Result<()> {
        self.out.write_all(&id.to_le_bytes())?;
        self.written += 1;
        if self.written.is_multiple_of(WRITE_BLOCK_IDS) {
            self.publish()?;
        }
        Ok(())
    }

    /// Appends ids.
    pub fn extend(&mut self, ids: &[u64]) -> io::Result<()> {
        for &id in ids {
            self.push(id)?;
        }
        Ok(())
    }

    /// Ids written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Flushes and publishes the ids written so far.
    pub fn publish(&mut self) -> io::Result<()> {
        self.out.flush()?;
        self.list.len.store(self.written, Ordering::Release);
        Ok(())
    }

    /// Publishes everything and marks the list complete.
    pub fn finish(mut self) -> io::Result<Arc<RowIdList>> {
        self.publish()?;
        self.list.growing.store(false, Ordering::Release);
        Ok(Arc::clone(&self.list))
    }

    /// The list being written.
    pub fn list(&self) -> &Arc<RowIdList> {
        &self.list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_is_identity() {
        let index = RowIndex::new(0);
        index.finish(10, 100);
        let v = View::All;
        assert_eq!(v.len(&index), 10);
        assert_eq!(v.row_id_at(3), Some(3));
        assert_eq!(v.row_ids(8, 2), vec![8, 9]);
        assert_eq!(v.position_of(7), Some(7));
        assert_eq!(v.label(), "all rows");
        assert!(v.sort_keys().is_empty() && v.filter_columns().is_empty());
        assert!(!v.is_growing(&index));
    }

    #[test]
    fn filtered_rows() {
        let rows = Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter([
            2u64,
            5,
            9,
            1 << 40,
        ])));
        let v = View::Filtered {
            rows,
            expr: "a > 1".into(),
            columns: vec![0],
        };
        let index = RowIndex::new(0);
        assert_eq!(v.len(&index), 4);
        assert_eq!(v.row_id_at(0), Some(2));
        assert_eq!(v.row_id_at(3), Some(1 << 40));
        assert_eq!(v.row_id_at(4), None);
        assert_eq!(v.row_ids(1, 10), vec![5, 9, 1 << 40]);
        assert_eq!(v.row_ids(4, 10), Vec::<u64>::new());
        assert_eq!(v.position_of(9), Some(2));
        assert_eq!(v.position_of(3), None);
        assert_eq!(v.label(), "filtered");
        assert_eq!(v.filter_columns(), &[0]);
        assert_eq!(v.filter_expr(), Some("a > 1"));
    }

    #[test]
    fn empty_and_growing_filter() {
        let rows = Arc::new(FilterRows::new_growing());
        let v = View::Filtered {
            rows: Arc::clone(&rows),
            expr: String::new(),
            columns: vec![],
        };
        let index = RowIndex::new(0);
        assert!(v.is_empty(&index) && v.is_growing(&index));
        assert_eq!(v.row_ids(0, 5), Vec::<u64>::new());
        rows.insert_many([10, 11]);
        rows.union_with(&RoaringTreemap::from_iter([3u64]));
        assert_eq!(v.row_ids(0, 5), vec![3, 10, 11]);
        rows.finish();
        assert!(!v.is_growing(&index));
    }

    #[test]
    fn ordered_list_grows() {
        let dir = tempfile::tempdir().unwrap();
        let (list, mut w) = RowIdList::create_in(dir.path()).unwrap();
        let v = View::Ordered {
            list: Arc::clone(&list),
            kind: OrderedKind::Sorted {
                keys: vec![SortKey {
                    column: 1,
                    descending: true,
                    ci: false,
                }],
            },
        };
        let index = RowIndex::new(0);
        assert!(v.is_empty(&index));
        assert_eq!(v.row_id_at(0), None);
        w.extend(&[9, 4, 7]).unwrap();
        assert_eq!(v.len(&index), 0, "not published yet");
        w.publish().unwrap();
        assert_eq!(v.row_ids(0, 10), vec![9, 4, 7]);
        // Grows past the current mapping: re-mapped on demand.
        let more: Vec<u64> = (100..100 + 2 * WRITE_BLOCK_IDS).collect();
        w.extend(&more).unwrap();
        assert!(v.len(&index) >= WRITE_BLOCK_IDS);
        assert!(v.is_growing(&index));
        let list = w.finish().unwrap();
        assert!(!v.is_growing(&index));
        assert_eq!(list.len(), 3 + 2 * WRITE_BLOCK_IDS);
        assert_eq!(
            v.row_id_at(3 + 2 * WRITE_BLOCK_IDS - 1),
            Some(99 + 2 * WRITE_BLOCK_IDS)
        );
        assert_eq!(v.row_ids(2, 3), vec![7, 100, 101]);
        assert_eq!(v.position_of(9), None);
        assert_eq!(v.label(), "sorted");
        assert_eq!(v.sort_keys().len(), 1);
        // Little-endian on disk.
        let bytes = std::fs::read(list.path()).unwrap();
        assert_eq!(&bytes[..8], &9u64.to_le_bytes());
    }

    #[test]
    fn filtered_sorted_is_labelled_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let list = RowIdList::from_ids(dir.path(), &[5, 1]).unwrap();
        let v = View::Ordered {
            list,
            kind: OrderedKind::FilteredSorted {
                expr: "x".into(),
                columns: vec![2],
                keys: vec![],
            },
        };
        assert_eq!(v.label(), "filtered");
        assert_eq!(v.filter_columns(), &[2]);
        assert_eq!(v.row_ids(0, 5), vec![5, 1]);
    }

    #[test]
    fn dupes_views() {
        let rows = Arc::new(FilterRows::from_bitmap(RoaringTreemap::from_iter([
            1u64, 4, 6,
        ])));
        let spec = DupeSpec {
            columns: vec![2, 0],
            mode: DupeMode::Show,
        };
        let v = View::Dupes {
            rows,
            spec: spec.clone(),
        };
        let index = RowIndex::new(0);
        assert_eq!(v.len(&index), 3);
        assert_eq!(v.row_ids(1, 5), vec![4, 6]);
        assert_eq!(v.row_id_at(0), Some(1));
        assert_eq!(v.position_of(6), Some(2));
        assert_eq!(v.label(), "duplicates");
        assert_eq!(v.filter_columns(), &[2, 0], "key columns are highlighted");
        assert_eq!(v.filter_expr(), None);
        assert_eq!(v.dupe_spec(), Some(&spec));
        assert!(v.sort_keys().is_empty() && !v.is_growing(&index));

        // Over a sorted parent: the parent's keys and order stay.
        let dir = tempfile::tempdir().unwrap();
        let list = RowIdList::from_ids(dir.path(), &[6, 1]).unwrap();
        let keys = vec![SortKey::desc(1)];
        let v = View::Ordered {
            list,
            kind: OrderedKind::Dupes {
                spec: DupeSpec {
                    columns: vec![],
                    mode: DupeMode::Remove,
                },
                keys: keys.clone(),
            },
        };
        assert_eq!(v.label(), "deduplicated");
        assert_eq!(v.sort_keys(), keys.as_slice());
        assert!(v.filter_columns().is_empty());
        assert_eq!(v.row_ids(0, 5), vec![6, 1]);
        assert!(View::All.dupe_spec().is_none());
    }

    #[test]
    fn dropping_the_list_deletes_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let list = RowIdList::from_ids(dir.path(), &[1, 2, 3]).unwrap();
        let path = list.path().to_path_buf();
        assert!(path.exists());
        drop(list);
        assert!(!path.exists());
    }
}
