//! Chunk scanners of the parallel indexer (spec §5.3).
//!
//! # Record rules
//!
//! The scanner implements exactly the record rules of [`crate::parse`]: a
//! record ends at `\n` outside quotes, blank lines and comment lines are not
//! records, a quote opens a quoted field only at the start of a field, `""`
//! (doubled style) or `\x` (backslash style) inside quotes are literal. A lone
//! `\r` is data for both, so chunks can always be split after a `\n`.
//!
//! # Chunk boundaries and start states
//!
//! The planner (`build.rs`) moves every chunk boundary to just after a `\n`.
//! The parser state right after a `\n` can only be one of two states, which
//! is why the speculative slow path needs exactly two runs per chunk, for
//! both escape styles:
//!
//! - **`Outside`** (at a record start): a `\n` outside quotes always ends the
//!   record (or a blank line, or a comment line), so every outside state
//!   (start of record, start of field, unquoted field, after a closing
//!   quote, comment) becomes "record start".
//! - **`Inside`** (in a quoted field): a `\n` inside quotes is copied and the
//!   field stays quoted. The other quoted states need a specific previous
//!   byte: "quote seen, maybe doubled" needs a quote, "after backslash"
//!   needs a `\`. Neither can precede a boundary whose previous byte is
//!   `\n`. (A `\n` right after a backslash is the escaped byte, which leaves
//!   the scanner back in `Inside`.)
//!
//! Without the alignment, a chunk could start in any of the parser's states
//! (`Inside`, quote-seen, after-backslash, unquoted field, field start, …),
//! and a quote's meaning would depend on bytes before the chunk.
//!
//! # Fast and slow path
//!
//! The same scanner serves both. The fast path runs each chunk once,
//! assuming `Outside`; the stitcher checks that the previous chunk really
//! ended `Outside` (the generalisation of the spec's "even quote parity").
//! The slow path runs each chunk under both states and the stitcher picks
//! the run that matches. The scanner jumps between interesting bytes with
//! `memchr` (`\n`, the quote, and inside quotes the quote or `\`).
//!
//! # Ragged counting
//!
//! Delimiters outside quotes are counted per record and compared to the
//! column count `H`. A record cut by a chunk boundary carries its count to
//! the next chunk (`head` / `tail`). With counting on, quote-free spans are
//! scanned with 64-byte compare masks (SSE2 on x86_64) that give newlines and
//! delimiters in one pass; without it, the scanner jumps from `\n` to `\n`
//! with `memchr`.
//!
//! Measured with `benches/index.rs` (1 GiB, ~100-byte rows with quoted
//! fields, warm page cache, 12-core machine; GiB/s):
//!
//! | threads | fast, counting | fast, no counting | cost | slow, counting |
//! |---|---|---|---|---|
//! | 1  | 4.1  | 5.4  | 24 % | 2.4 |
//! | 8  | 14.1 | 17.3 | 18 % | 8.1 |
//! | 12 | 15.1 | 17.8 | 15 % | 8.6 |
//!
//! **Decision:** counting costs more than the 15 % M2-02 allows on paper, but
//! with it on, the fast path still runs at 4–5× the §17 target (3 GB/s) and
//! the slow path at 8× (1 GB/s), and only counting in the indexer gives the
//! exact `ragged_rows` both paths must report. So counting stays on for both
//! paths; `IndexOptions::count_ragged = false` turns it off.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio_util::sync::CancellationToken;

use crate::dialect::{Dialect, EscapeStyle};

/// Scanners check for cancellation (and report progress) after every block
/// of this many bytes (§4.3).
pub const CHECK_BYTES: usize = 64 * 1024;

/// Parser state at a chunk start. See the module docs for why two suffice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChunkState {
    /// At a record start, outside quotes.
    Outside,
    /// Inside a quoted field.
    Inside,
}

/// The scan was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// What a scanner needs from the dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanConfig {
    /// Field delimiter.
    pub delimiter: u8,
    /// Quote byte, if quoting is on.
    pub quote: Option<u8>,
    /// Backslash escapes inside quotes.
    pub backslash: bool,
    /// Comment prefix.
    pub comment: Option<u8>,
    /// Column count `H` for ragged counting, or `None` to skip it.
    pub width: Option<u64>,
}

impl ScanConfig {
    /// The config for `dialect`, counting ragged rows against `width`.
    pub fn new(dialect: &Dialect, width: Option<u64>) -> Self {
        ScanConfig {
            delimiter: dialect.delimiter,
            quote: dialect.quote,
            backslash: dialect.escape == EscapeStyle::Backslash,
            comment: dialect.comment,
            width,
        }
    }
}

/// Record starts of a chunk, relative to the chunk start. 4 bytes each,
/// unless the chunk is over 4 GiB (one giant line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Starts {
    /// Offsets of a chunk under 4 GiB.
    Narrow(Vec<u32>),
    /// Offsets of a larger chunk.
    Wide(Vec<u64>),
}

impl Starts {
    fn for_len(len: usize) -> Self {
        if u32::try_from(len).is_ok() {
            Starts::Narrow(Vec::new())
        } else {
            Starts::Wide(Vec::new())
        }
    }

    #[inline]
    fn push(&mut self, rel: usize) {
        match self {
            Starts::Narrow(v) => v.push(rel as u32),
            Starts::Wide(v) => v.push(rel as u64),
        }
    }

    /// Number of record starts.
    pub fn len(&self) -> usize {
        match self {
            Starts::Narrow(v) => v.len(),
            Starts::Wide(v) => v.len(),
        }
    }

    /// True when no record starts in the chunk.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Relative offset of start `i`.
    pub fn get(&self, i: usize) -> Option<u64> {
        match self {
            Starts::Narrow(v) => v.get(i).map(|&x| u64::from(x)),
            Starts::Wide(v) => v.get(i).copied(),
        }
    }

    /// All relative offsets.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len()).filter_map(|i| self.get(i))
    }
}

/// The part of a record that began before the chunk (`Inside` start only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// Delimiters outside quotes in this chunk's part of the record.
    pub delims: u64,
    /// The record ended in this chunk.
    pub closed: bool,
}

/// Result of scanning one chunk under one start state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkScan {
    /// Absolute chunk start.
    pub start: u64,
    /// Absolute chunk end (exclusive).
    pub end: u64,
    /// The state assumed at `start`.
    pub start_state: ChunkState,
    /// The state at `end`.
    pub end_state: ChunkState,
    /// Starts of the records that begin in this chunk.
    pub starts: Starts,
    /// The record carried in from the previous chunk (`Inside` start).
    pub head: Option<Head>,
    /// Delimiters of the record that began in this chunk and is still open
    /// at its end (`end_state == Inside`).
    pub tail_delims: Option<u64>,
    /// Ragged records that began and ended in this chunk.
    pub ragged: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    RecordStart,
    Comment,
    InRecord,
    Quoted,
}

/// Bits `0..k` set (all bits for `k >= 64`).
#[inline]
fn below(k: u32) -> u64 {
    if k >= 64 { u64::MAX } else { (1u64 << k) - 1 }
}

/// Bit masks of the bytes of `w` (at most 64) equal to `a` and to `b`.
#[inline]
fn eq_masks(w: &[u8], a: u8, b: u8) -> (u64, u64) {
    debug_assert!(w.len() <= 64);
    #[cfg(target_arch = "x86_64")]
    if w.len() == 64 {
        use std::arch::x86_64::{
            __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
        };
        let (mut ma, mut mb) = (0u64, 0u64);
        // SAFETY: SSE2 is part of the x86_64 baseline, and every unaligned
        // 16-byte load reads inside `w`, which is 64 bytes long.
        unsafe {
            let va = _mm_set1_epi8(a as i8);
            let vb = _mm_set1_epi8(b as i8);
            for k in 0..4 {
                let v = _mm_loadu_si128(w.as_ptr().add(16 * k).cast::<__m128i>());
                ma |= u64::from(_mm_movemask_epi8(_mm_cmpeq_epi8(v, va)) as u16) << (16 * k);
                mb |= u64::from(_mm_movemask_epi8(_mm_cmpeq_epi8(v, vb)) as u16) << (16 * k);
            }
        }
        return (ma, mb);
    }
    let (mut ma, mut mb) = (0u64, 0u64);
    for (i, &x) in w.iter().enumerate() {
        ma |= u64::from(x == a) << i;
        mb |= u64::from(x == b) << i;
    }
    (ma, mb)
}

/// Scans `bytes[start..end]` assuming `state` at `start`.
///
/// `end` must be just after a `\n`, or the end of the file. Checks `cancel`
/// before every block of [`CHECK_BYTES`] and adds to `progress` after it, so
/// a cancelled scan does at most one more block of work.
///
/// With `quote == None` only the `Outside` state exists; an `Inside` start is
/// scanned as `Outside`.
pub fn scan_chunk(
    bytes: &[u8],
    start: u64,
    end: u64,
    state: ChunkState,
    cfg: &ScanConfig,
    cancel: &CancellationToken,
    progress: Option<&AtomicU64>,
) -> Result<ChunkScan, Cancelled> {
    let (start, end) = (start as usize, end as usize);
    debug_assert!(end <= bytes.len());
    debug_assert!(end == bytes.len() || bytes[end - 1] == b'\n');
    let len = bytes.len();
    let d = cfg.delimiter;
    let counting = cfg.width.is_some();
    let width = cfg.width.unwrap_or(0);

    let state = if cfg.quote.is_none() {
        ChunkState::Outside
    } else {
        state
    };
    let mut st = match state {
        ChunkState::Outside => St::RecordStart,
        ChunkState::Inside => St::Quoted,
    };
    let mut carried = state == ChunkState::Inside;
    let mut head = None;
    let mut ragged = 0u64;
    let mut starts = Starts::for_len(end - start);
    let mut rec_start = usize::MAX;
    let mut delims = 0u64;
    // Cache of the next quote: searched [nq_from, nq_to), found `nq`.
    let (mut nq_from, mut nq_to, mut nq) = (usize::MAX, 0usize, None::<usize>);

    let mut pos = start;
    let mut block_start = start;
    loop {
        if cancel.is_cancelled() {
            return Err(Cancelled);
        }
        let block_end = (block_start + CHECK_BYTES).min(end);
        while pos < block_end {
            match st {
                St::RecordStart => {
                    let b = bytes[pos];
                    if b == b'\n' {
                        pos += 1;
                    } else if b == b'\r' && bytes.get(pos + 1) == Some(&b'\n') {
                        pos += 2;
                    } else if Some(b) == cfg.comment {
                        st = St::Comment;
                    } else {
                        starts.push(pos - start);
                        rec_start = pos;
                        delims = 0;
                        st = St::InRecord;
                    }
                }
                St::Comment => match memchr::memchr(b'\n', &bytes[pos..block_end]) {
                    Some(i) => {
                        pos += i + 1;
                        st = St::RecordStart;
                    }
                    None => pos = block_end,
                },
                St::InRecord if counting => {
                    // Quote-free span ahead: newlines and delimiters from
                    // 64-byte compare masks, one record end at a time.
                    let span_end = match cfg.quote {
                        Some(q) => {
                            let valid = nq_from <= pos
                                && match nq {
                                    Some(p) => p >= pos,
                                    None => block_end <= nq_to,
                                };
                            if !valid {
                                nq_from = pos;
                                nq_to = block_end;
                                nq = memchr::memchr(q, &bytes[pos..block_end]).map(|i| pos + i);
                            }
                            nq.unwrap_or(block_end)
                        }
                        None => block_end,
                    };
                    if span_end == pos {
                        // At a quote.
                        if pos == rec_start || bytes[pos - 1] == d {
                            st = St::Quoted;
                        }
                        pos += 1;
                        continue;
                    }
                    let w_end = (pos + 64).min(span_end);
                    let w_start = pos;
                    let (mut nl, dm) = eq_masks(&bytes[w_start..w_end], b'\n', d);
                    let mut cur = 0u32;
                    loop {
                        if nl == 0 {
                            delims += u64::from((dm & !below(cur)).count_ones());
                            pos = w_end;
                            break;
                        }
                        let j = nl.trailing_zeros();
                        nl &= nl - 1;
                        delims += u64::from((dm & below(j) & !below(cur)).count_ones());
                        if carried {
                            head = Some(Head {
                                delims,
                                closed: true,
                            });
                            carried = false;
                        } else if delims + 1 != width {
                            ragged += 1;
                        }
                        // The next record starts in this window: keep using
                        // its masks unless the start needs the slow rules
                        // (blank line, comment).
                        let next = w_start + j as usize + 1;
                        let simple = next < w_end
                            && !matches!(bytes[next], b'\n' | b'\r')
                            && Some(bytes[next]) != cfg.comment;
                        if !simple {
                            pos = next;
                            st = St::RecordStart;
                            break;
                        }
                        starts.push(next - start);
                        rec_start = next;
                        delims = 0;
                        cur = j + 1;
                    }
                }
                St::InRecord => {
                    let nl = memchr::memchr(b'\n', &bytes[pos..block_end]).map(|i| pos + i);
                    let seg_end = nl.unwrap_or(block_end);
                    let quote_at = match cfg.quote {
                        Some(q) => {
                            let valid = nq_from <= pos
                                && match nq {
                                    Some(p) => p >= pos,
                                    None => seg_end <= nq_to,
                                };
                            if !valid {
                                nq_from = pos;
                                nq_to = block_end;
                                nq = memchr::memchr(q, &bytes[pos..block_end]).map(|i| pos + i);
                            }
                            nq.filter(|&p| p < seg_end)
                        }
                        None => None,
                    };
                    if let Some(qi) = quote_at {
                        if qi == rec_start || bytes[qi - 1] == d {
                            st = St::Quoted;
                        }
                        pos = qi + 1;
                    } else {
                        match nl {
                            Some(n) => {
                                if carried {
                                    head = Some(Head {
                                        delims,
                                        closed: true,
                                    });
                                    carried = false;
                                }
                                pos = n + 1;
                                st = St::RecordStart;
                            }
                            None => pos = block_end,
                        }
                    }
                }
                St::Quoted => {
                    let Some(q) = cfg.quote else {
                        unreachable!("the quoted state needs a quote byte")
                    };
                    let window = &bytes[pos..block_end];
                    let hit = if cfg.backslash {
                        memchr::memchr2(q, b'\\', window)
                    } else {
                        memchr::memchr(q, window)
                    };
                    match hit {
                        None => pos = block_end,
                        Some(i) => {
                            let i = pos + i;
                            if bytes[i] != q {
                                // Backslash: skip the escaped byte.
                                pos = (i + 2).min(end);
                            } else if !cfg.backslash && bytes.get(i + 1) == Some(&q) {
                                pos = i + 2;
                            } else {
                                st = St::InRecord;
                                pos = i + 1;
                            }
                        }
                    }
                }
            }
        }
        if let Some(p) = progress {
            p.fetch_add((block_end - block_start) as u64, Ordering::Relaxed);
        }
        if block_end >= end {
            break;
        }
        block_start = block_end;
    }

    let mut tail_delims = None;
    let end_state = match st {
        St::Quoted => {
            if carried {
                head = Some(Head {
                    delims,
                    closed: false,
                });
            } else {
                tail_delims = Some(delims);
            }
            ChunkState::Inside
        }
        St::InRecord => {
            // Only at EOF: the last record has no trailing newline.
            debug_assert_eq!(end, len);
            if carried {
                head = Some(Head {
                    delims,
                    closed: true,
                });
            } else if counting && delims + 1 != width {
                ragged += 1;
            }
            ChunkState::Outside
        }
        St::RecordStart | St::Comment => ChunkState::Outside,
    };
    if carried && head.is_none() {
        // An `Inside` chunk that never left the quoted field.
        head = Some(Head {
            delims,
            closed: false,
        });
    }
    Ok(ChunkScan {
        start: start as u64,
        end: end as u64,
        start_state: state,
        end_state,
        starts,
        head,
        tail_delims,
        ragged,
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn cfg(width: Option<u64>) -> ScanConfig {
        ScanConfig::new(&Dialect::default(), width)
    }

    fn scan(bytes: &[u8], state: ChunkState, width: Option<u64>) -> ChunkScan {
        scan_chunk(
            bytes,
            0,
            bytes.len() as u64,
            state,
            &cfg(width),
            &CancellationToken::new(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn masks() {
        let w: Vec<u8> = (0..64u8)
            .map(|i| if i % 3 == 0 { b',' } else { b'\n' })
            .collect();
        let (nl, d) = eq_masks(&w, b'\n', b',');
        assert_eq!(d.count_ones(), 22);
        assert_eq!(nl, !d);
        assert_eq!(eq_masks(&w[..10], b'\n', b','), (nl & 0x3FF, d & 0x3FF));
        assert_eq!(eq_masks(b"", b'\n', b','), (0, 0));
    }

    #[test]
    fn outside_scan() {
        let bytes = b"a,b\n\n\"x\ny\",z\nc\n";
        let r = scan(bytes, ChunkState::Outside, Some(2));
        assert_eq!(r.starts.iter().collect::<Vec<_>>(), [0, 5, 13]);
        assert_eq!(r.end_state, ChunkState::Outside);
        assert_eq!(r.ragged, 1); // `c`
        assert_eq!(r.head, None);
        assert_eq!(r.tail_delims, None);
    }

    #[test]
    fn inside_scan() {
        // Starts inside `"..`, closes, then one more record that stays open.
        let bytes = b"tail\",b,c\nd,\"open\n";
        let r = scan(bytes, ChunkState::Inside, Some(3));
        assert_eq!(r.starts.iter().collect::<Vec<_>>(), [10]);
        assert_eq!(
            r.head,
            Some(Head {
                delims: 2,
                closed: true
            })
        );
        assert_eq!(r.tail_delims, Some(1));
        assert_eq!(r.end_state, ChunkState::Inside);
        // The same bytes from outside: the quote after `tail` is literal.
        let r = scan(bytes, ChunkState::Outside, Some(3));
        assert_eq!(r.starts.iter().collect::<Vec<_>>(), [0, 10]);
        assert_eq!(r.end_state, ChunkState::Inside);
    }

    #[test]
    fn whole_chunk_inside() {
        let r = scan(b"still quoted\nmore\n", ChunkState::Inside, Some(1));
        assert!(r.starts.is_empty());
        assert_eq!(
            r.head,
            Some(Head {
                delims: 0,
                closed: false
            })
        );
        assert_eq!(r.end_state, ChunkState::Inside);
    }

    #[test]
    fn cancelled_scan_stops_within_one_block() {
        let bytes = vec![b'x'; 10 * CHECK_BYTES];
        let cancel = CancellationToken::new();
        cancel.cancel();
        let progress = AtomicU64::new(0);
        let r = scan_chunk(
            &bytes,
            0,
            bytes.len() as u64,
            ChunkState::Outside,
            &cfg(None),
            &cancel,
            Some(&progress),
        );
        assert_eq!(r, Err(Cancelled));
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }
}
