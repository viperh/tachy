//! `RowCache`: an LRU cache of parsed rows around the viewport (spec §6.3),
//! plus ragged-row normalisation (§6.4).
//!
//! The cache is owned by the UI-side `Tab` and is never shared with
//! background tasks, so it needs no locking. Scrolling only parses rows that
//! are not cached yet, and runs of consecutive missing rows cost one index
//! seek, not one per row.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use roaring::RoaringTreemap;
use smallvec::SmallVec;

use crate::{
    index::{RowIndex, seek},
    parse::{self, FieldRange, ParseOutcome, ParseWarning, Ragged, RecordParser, RecordRanges},
    source::Source,
};

/// One parsed row.
#[derive(Debug)]
pub struct ParsedRow {
    /// Absolute offset of the record's first byte.
    pub offset: u64,
    /// Record length in bytes, terminator included (saturates at 4 GiB).
    pub len: u32,
    /// Field ranges, relative to `offset`.
    pub fields: SmallVec<[FieldRange; 32]>,
    /// Decoded display strings, one slot per field, filled the first time a
    /// column is drawn (only visible columns are ever decoded). `OnceLock`
    /// keeps `Arc<ParsedRow>` `Send + Sync`.
    pub decoded: Vec<OnceLock<Box<str>>>,
    /// Field count compared to the column count `H`.
    pub ragged: Ragged,
    /// Set on the last row when the file ends inside a quoted field.
    pub warning: Option<ParseWarning>,
}

impl ParsedRow {
    fn from_record(rec: &RecordRanges, width: usize, warning: Option<ParseWarning>) -> Self {
        ParsedRow {
            offset: rec.start,
            len: u32::try_from(rec.end - rec.start).unwrap_or(u32::MAX),
            fields: rec.fields.clone(),
            decoded: (0..rec.fields.len()).map(|_| OnceLock::new()).collect(),
            ragged: Ragged::classify(rec.fields.len(), width),
            warning,
        }
    }

    /// Raw bytes of field `col` (quotes included), or `None` past the last
    /// field (a missing cell of a short row). Kept for export and clipboard.
    pub fn raw<'a>(&self, src: &'a Source, col: usize) -> Option<&'a [u8]> {
        let f = self.fields.get(col)?;
        let s = self.offset as usize;
        Some(&src.bytes()[s + f.raw.start as usize..s + f.raw.end as usize])
    }

    /// Unescaped value of field `col` (see `RecordParser::field_value`),
    /// after the source's column edits (`crate::edit`).
    pub fn value<'a>(
        &self,
        src: &'a Source,
        col: usize,
        scratch: &'a mut Vec<u8>,
    ) -> Option<&'a [u8]> {
        let raw = self.raw(src, col)?;
        let p = RecordParser::new(src.dialect());
        if !src.edits().is_edited(col) {
            return Some(p.unescape(raw, &self.fields[col], scratch));
        }
        let mut unescaped = Vec::new();
        let v = p.unescape(raw, &self.fields[col], &mut unescaped);
        src.edits().apply_into(col, v, scratch);
        Some(scratch)
    }

    /// Decoded display string of column `col`, decoded once and kept. A
    /// missing cell (short row) is `""`. Control characters are not escaped
    /// here: the UI runs `parse::display_segments` on the result.
    ///
    /// `src` must be the source the row was parsed from, and the row must be
    /// re-parsed (`RowCache::invalidate_all`) when its edits change.
    pub fn display(&self, src: &Source, col: usize) -> &str {
        let Some(slot) = self.decoded.get(col) else {
            return "";
        };
        slot.get_or_init(|| {
            let mut scratch = Vec::new();
            let value = self.value(src, col, &mut scratch).unwrap_or_default();
            parse::decode_field(value, src.dialect().encoding).into()
        })
    }

    /// Number of fields in the record.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// Estimated memory: the record bytes, the same again for decoded
    /// strings (filled lazily, so estimated up front), and per-field
    /// bookkeeping.
    pub fn approx_bytes(&self) -> u64 {
        2 * u64::from(self.len) + 40 * self.fields.len() as u64 + 128
    }
}

/// Counters for tests and debug logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Requested rows found in the cache.
    pub hits: u64,
    /// Requested rows that were not.
    pub misses: u64,
    /// Rows parsed (`parse_at` calls that produced a row).
    pub parsed_rows: u64,
    /// Index seeks (`locate` + skip), one per run of consecutive misses.
    pub seeks: u64,
    /// Estimated bytes held now.
    pub bytes: u64,
}

/// Result of [`RowCache::get_window`].
#[derive(Debug, Clone, Default)]
pub struct Window {
    /// One entry per requested row id, in the same order. `None` when the
    /// row is past EOF or not reachable yet (beyond the speculative range).
    pub rows: Vec<Option<Arc<ParsedRow>>>,
    /// The most fields any row parsed so far had. When it exceeds the column
    /// count, the tab appends `_extra1`, `_extra2`, … columns (§6.4).
    pub max_fields_seen: usize,
    /// Ragged rows seen by the cache so far (deduplicated). Shown as
    /// `≥ N ragged` until the index is complete; then the indexer's exact
    /// count replaces it.
    pub ragged_seen: u64,
}

const NIL: usize = usize::MAX;

#[derive(Debug)]
struct Entry {
    key: u64,
    row: Arc<ParsedRow>,
    bytes: u64,
    prev: usize,
    next: usize,
}

/// LRU of parsed rows keyed by row id. See the module docs.
///
/// Hand-rolled: a `HashMap` from row id into a slab of entries that form an
/// intrusive doubly linked list (most recent at the head).
#[derive(Debug)]
pub struct RowCache {
    map: HashMap<u64, usize>,
    slab: Vec<Entry>,
    free: Vec<usize>,
    head: usize,
    tail: usize,
    max_rows: usize,
    max_bytes: u64,
    stats: CacheStats,
    max_fields_seen: usize,
    ragged_seen: RoaringTreemap,
}

impl Default for RowCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RowCache {
    /// Default capacity in rows (§6.3).
    pub const DEFAULT_ROWS: usize = 4_096;
    /// Default byte cap: 64 MiB, half of the UI's 128 MiB budget (§10.4).
    pub const DEFAULT_BYTES: u64 = 64 << 20;

    /// A cache with the default limits.
    pub fn new() -> Self {
        Self::with_limits(Self::DEFAULT_ROWS, Self::DEFAULT_BYTES)
    }

    /// A cache holding at most `max_rows` rows and about `max_bytes` bytes.
    pub fn with_limits(max_rows: usize, max_bytes: u64) -> Self {
        RowCache {
            map: HashMap::new(),
            slab: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            max_rows: max_rows.max(1),
            max_bytes,
            stats: CacheStats::default(),
            max_fields_seen: 0,
            ragged_seen: RoaringTreemap::new(),
        }
    }

    /// Number of cached rows.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Counters.
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// The most fields any parsed row had.
    pub fn max_fields_seen(&self) -> usize {
        self.max_fields_seen
    }

    /// Ragged rows seen so far (deduplicated by row id).
    pub fn ragged_seen(&self) -> u64 {
        self.ragged_seen.len()
    }

    fn unlink(&mut self, i: usize) {
        let (prev, next) = (self.slab[i].prev, self.slab[i].next);
        if prev == NIL {
            self.head = next;
        } else {
            self.slab[prev].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slab[next].prev = prev;
        }
    }

    fn push_front(&mut self, i: usize) {
        self.slab[i].prev = NIL;
        self.slab[i].next = self.head;
        if self.head != NIL {
            self.slab[self.head].prev = i;
        }
        self.head = i;
        if self.tail == NIL {
            self.tail = i;
        }
    }

    /// The cached row, marked as most recently used.
    pub fn get(&mut self, row_id: u64) -> Option<Arc<ParsedRow>> {
        let &i = self.map.get(&row_id)?;
        self.unlink(i);
        self.push_front(i);
        Some(Arc::clone(&self.slab[i].row))
    }

    /// Inserts (or replaces) a row, then evicts least recently used rows
    /// until both limits hold.
    pub fn insert(&mut self, row_id: u64, row: Arc<ParsedRow>) {
        self.max_fields_seen = self.max_fields_seen.max(row.field_count());
        if row.ragged.is_ragged() {
            self.ragged_seen.insert(row_id);
        }
        let bytes = row.approx_bytes();
        if let Some(&i) = self.map.get(&row_id) {
            self.stats.bytes = self.stats.bytes - self.slab[i].bytes + bytes;
            self.slab[i].row = row;
            self.slab[i].bytes = bytes;
            self.unlink(i);
            self.push_front(i);
        } else {
            let entry = Entry {
                key: row_id,
                row,
                bytes,
                prev: NIL,
                next: NIL,
            };
            let i = match self.free.pop() {
                Some(i) => {
                    self.slab[i] = entry;
                    i
                }
                None => {
                    self.slab.push(entry);
                    self.slab.len() - 1
                }
            };
            self.map.insert(row_id, i);
            self.push_front(i);
            self.stats.bytes += bytes;
        }
        while self.map.len() > self.max_rows
            || (self.stats.bytes > self.max_bytes && self.map.len() > 1)
        {
            self.evict_lru();
        }
    }

    fn evict_lru(&mut self) {
        let i = self.tail;
        if i == NIL {
            return;
        }
        self.unlink(i);
        let key = self.slab[i].key;
        self.map.remove(&key);
        self.stats.bytes -= self.slab[i].bytes;
        // Drop the row now; the slot is reused later.
        self.slab[i].row = Arc::new(ParsedRow {
            offset: 0,
            len: 0,
            fields: SmallVec::new(),
            decoded: Vec::new(),
            ragged: Ragged::Exact,
            warning: None,
        });
        self.free.push(i);
    }

    /// Empties the cache. Called on dialect change (§6.3) and reload
    /// (M7-03). Also resets `max_fields_seen` and the ragged count.
    pub fn invalidate_all(&mut self) {
        self.map.clear();
        self.slab.clear();
        self.free.clear();
        self.head = NIL;
        self.tail = NIL;
        self.stats.bytes = 0;
        self.max_fields_seen = 0;
        self.ragged_seen.clear();
    }

    /// Resolves the rows of the visible window in one call.
    ///
    /// `rows` are row ids in view order. Cached rows are reused. For each run
    /// of **consecutive** missing row ids the cache seeks once
    /// (`RowIndex::locate_speculative`, so the first screen renders before
    /// the indexer has published anything) and parses forward sequentially.
    pub fn get_window(&mut self, rows: &[u64], src: &Source, index: &RowIndex) -> Window {
        let mut out: Vec<Option<Arc<ParsedRow>>> = Vec::with_capacity(rows.len());
        let mut parser = RecordParser::new(src.dialect());
        let mut rec = RecordRanges::default();
        let bytes = src.bytes();
        let width = src.width();
        let mut i = 0;
        while i < rows.len() {
            if let Some(row) = self.get(rows[i]) {
                self.stats.hits += 1;
                out.push(Some(row));
                i += 1;
                continue;
            }
            // A run of consecutive, missing row ids.
            let first = rows[i];
            let mut n = 1;
            while i + n < rows.len()
                && rows[i + n] == first + n as u64
                && !self.map.contains_key(&rows[i + n])
            {
                n += 1;
            }
            self.stats.misses += n as u64;
            self.stats.seeks += 1;
            let mut pos = index
                .locate_speculative(first)
                .and_then(|(offset, skip)| seek(bytes, &mut parser, offset, skip));
            for k in 0..n {
                let parsed = pos.and_then(|p| {
                    let (next, warning) = match parser.parse_at(bytes, p, &mut rec) {
                        ParseOutcome::Eof => return None,
                        ParseOutcome::Record { next } => (next, None),
                        ParseOutcome::UnterminatedQuote { next } => {
                            (next, Some(ParseWarning::UnterminatedQuote))
                        }
                    };
                    self.stats.parsed_rows += 1;
                    Some((next, Arc::new(ParsedRow::from_record(&rec, width, warning))))
                });
                match parsed {
                    Some((next, row)) => {
                        pos = Some(next);
                        self.insert(first + k as u64, Arc::clone(&row));
                        out.push(Some(row));
                    }
                    None => {
                        pos = None;
                        out.push(None);
                    }
                }
            }
            i += n;
        }
        Window {
            rows: out,
            max_fields_seen: self.max_fields_seen,
            ragged_seen: self.ragged_seen(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{dialect::Dialect, parse::extra_column_name};

    fn row(len: u32) -> Arc<ParsedRow> {
        Arc::new(ParsedRow {
            offset: 0,
            len,
            fields: SmallVec::new(),
            decoded: Vec::new(),
            ragged: Ragged::Exact,
            warning: None,
        })
    }

    fn source(content: &[u8]) -> (tempfile::NamedTempFile, Source) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let s = Source::open(f.path(), None)
            .unwrap()
            .with_dialect(Dialect::default());
        (f, s)
    }

    fn numbered(rows: usize) -> Vec<u8> {
        let mut out = b"a,b,c,d,e\n".to_vec();
        for i in 0..rows {
            out.extend_from_slice(format!("{i},b{i},c,d,e\n").as_bytes());
        }
        out
    }

    #[test]
    fn lru_order_and_capacity() {
        let mut c = RowCache::with_limits(3, u64::MAX);
        for k in 0..3 {
            c.insert(k, row(1));
        }
        assert!(c.get(0).is_some()); // 0 is now most recent; 1 is LRU
        c.insert(3, row(1));
        assert_eq!(c.len(), 3);
        assert!(c.get(1).is_none());
        assert!(c.get(0).is_some() && c.get(2).is_some() && c.get(3).is_some());
        c.insert(4, row(1)); // evicts 0 (LRU after the gets above)
        assert!(c.get(0).is_none());
    }

    #[test]
    fn default_capacity_is_respected() {
        let mut c = RowCache::new();
        for k in 0..5_000 {
            c.insert(k, row(10));
        }
        assert_eq!(c.len(), RowCache::DEFAULT_ROWS);
        assert!(c.get(5_000 - 4_096 - 1).is_none());
        assert!(c.get(5_000 - 4_096).is_some());
    }

    #[test]
    fn byte_cap_evicts_below_row_capacity() {
        let mut c = RowCache::new();
        // ~2 MiB each: the 64 MiB cap holds about 31.
        for k in 0..100 {
            c.insert(k, row(1 << 20));
        }
        assert!(c.len() < 40, "{}", c.len());
        assert!(c.stats().bytes <= RowCache::DEFAULT_BYTES);
        assert!(c.get(99).is_some());
        assert!(c.get(0).is_none());
    }

    #[test]
    fn invalidate_all_empties() {
        let mut c = RowCache::new();
        c.insert(1, row(1));
        c.invalidate_all();
        assert!(c.is_empty());
        assert!(c.get(1).is_none());
        assert_eq!(c.stats().bytes, 0);
        c.insert(2, row(1));
        assert!(c.get(2).is_some());
    }

    #[test]
    fn scrolling_parses_one_new_row() {
        let (_f, src) = source(&numbered(500));
        let index = RowIndex::for_source(&src);
        let mut c = RowCache::new();
        let w = c.get_window(&(0..40).collect::<Vec<_>>(), &src, &index);
        assert!(w.rows.iter().all(Option::is_some));
        assert_eq!(c.stats().parsed_rows, 40);
        let w = c.get_window(&(1..41).collect::<Vec<_>>(), &src, &index);
        assert_eq!(c.stats().parsed_rows, 41);
        assert_eq!(w.rows[39].as_ref().unwrap().display(&src, 0), "40");
    }

    #[test]
    fn consecutive_missing_rows_seek_once() {
        let (_f, src) = source(&numbered(3000));
        // Index fully built by hand.
        let index = RowIndex::for_source(&src);
        let p = RecordParser::new(src.dialect());
        let mut cps = Vec::new();
        let mut pos = src.data_start();
        for r in 0..3000u64 {
            let (start, next, _) = p.next_record(src.bytes(), pos).unwrap();
            if r > 0 && r % 1024 == 0 {
                cps.push(start);
            }
            pos = next;
        }
        index.push_checkpoints(&cps);
        index.finish(3000, src.len());
        let mut c = RowCache::new();
        let rows: Vec<u64> = (2000..2050).collect();
        let w = c.get_window(&rows, &src, &index);
        assert_eq!(c.stats().seeks, 1);
        assert_eq!(c.stats().parsed_rows, 50);
        for (k, r) in w.rows.iter().enumerate() {
            assert_eq!(r.as_ref().unwrap().display(&src, 0), (2000 + k).to_string());
        }
        // Non-consecutive ids: one seek per run.
        let w = c.get_window(&[10, 11, 500, 2999, 3000], &src, &index);
        assert_eq!(c.stats().seeks, 4);
        assert!(w.rows[4].is_none()); // past EOF
        assert_eq!(w.rows[3].as_ref().unwrap().display(&src, 1), "b2999");
    }

    #[test]
    fn first_screen_before_any_checkpoint() {
        let (_f, src) = source(&numbered(10_000));
        let index = RowIndex::for_source(&src);
        assert_eq!(index.published_checkpoints(), 1);
        let mut c = RowCache::new();
        let w = c.get_window(&(0..60).collect::<Vec<_>>(), &src, &index);
        assert!(w.rows.iter().all(Option::is_some));
        assert_eq!(w.rows[59].as_ref().unwrap().display(&src, 0), "59");
        // Beyond the speculative range: not reachable yet.
        let w = c.get_window(&[5_000], &src, &index);
        assert!(w.rows[0].is_none());
    }

    #[test]
    fn ragged_rows() {
        let (_f, src) = source(b"a,b,c,d,e\n1,2\n1,2,3,4,5,6,7\n1,2,3,4,5\n");
        let index = RowIndex::for_source(&src);
        let mut c = RowCache::new();
        let w = c.get_window(&[0, 1, 2], &src, &index);
        let short = w.rows[0].as_ref().unwrap();
        assert_eq!(short.ragged, Ragged::Short(3));
        assert_eq!(short.display(&src, 1), "2");
        assert_eq!(short.display(&src, 2), "");
        assert_eq!(short.display(&src, 4), "");
        assert!(short.raw(&src, 3).is_none());
        let long = w.rows[1].as_ref().unwrap();
        assert_eq!(long.ragged, Ragged::Long(2));
        assert_eq!(long.display(&src, 6), "7");
        assert_eq!(w.rows[2].as_ref().unwrap().ragged, Ragged::Exact);
        assert_eq!(w.max_fields_seen, 7);
        assert_eq!(w.ragged_seen, 2);
        let extra: Vec<String> = (1..=w.max_fields_seen - src.width())
            .map(extra_column_name)
            .collect();
        assert_eq!(extra, ["_extra1", "_extra2"]);
        // Seeing the same rows again does not count them twice.
        c.invalidate_all();
        c.get_window(&[0, 1], &src, &index);
        c.get_window(&[0, 1], &src, &index);
        assert_eq!(c.ragged_seen(), 2);
    }

    #[test]
    fn unterminated_quote_warning() {
        let (_f, src) = source(b"a,b\n1,\"open\n");
        let index = RowIndex::for_source(&src);
        let mut c = RowCache::new();
        let w = c.get_window(&[0], &src, &index);
        let r = w.rows[0].as_ref().unwrap();
        assert_eq!(r.warning, Some(ParseWarning::UnterminatedQuote));
        assert_eq!(r.display(&src, 1), "open\n");
    }
}
