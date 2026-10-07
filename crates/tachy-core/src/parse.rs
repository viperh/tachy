//! Record parser: splits records into field byte ranges and decodes fields
//! for display (spec §6.1, §6.2).
//!
//! # Implementation choice
//!
//! csv-core reports field ends in its *output* buffer, not raw input
//! positions, so this module uses a small hand-written state machine that
//! mirrors csv-core's quoting rules for raw ranges and values (the second
//! option of M1-03). It works directly on the memory map, jumps between
//! interesting bytes with `memchr` and never allocates. csv-core stays the
//! oracle: a property test (`tests/parse_props.rs`) checks that both agree on
//! records and field values.
//!
//! # Record rules (shared with the indexer, M2-02)
//!
//! These rules define what a row is. The parallel indexer (`index::scan`)
//! implements exactly the same rules, so row ids from the index and from the
//! parser always agree.
//!
//! - A record ends at `\n` outside quotes. A `\r` right before that `\n` (or
//!   right before EOF) belongs to the terminator, so mixed `\n` / `\r\n`
//!   files never leave a stray `\r` in the last field.
//! - **A lone `\r` is data**, not a line ending. csv-core's CRLF terminator
//!   accepts it, but spec §2 only lists `\n` and `\r\n`, and the indexer splits
//!   chunks at `\n`; treating it as data keeps both in agreement.
//! - **Blank lines are not rows**: an empty line (`\n` or `\r\n` at a record
//!   start) is skipped, like csv-core does.
//! - **Comment lines are not rows**: when `dialect.comment` is set, a line
//!   whose first byte (at a record start, outside quotes) is the comment byte
//!   is skipped up to and including its `\n`.
//! - A quote opens a quoted field only at the **start of a field** (record
//!   start or right after a delimiter). Elsewhere it is a literal byte, as in
//!   csv-core.
//! - Inside quotes: with [`EscapeStyle::Doubled`], `""` is a literal quote;
//!   with [`EscapeStyle::Backslash`], the byte after `\` is literal. Any other
//!   quote closes the field.
//! - Bytes between a closing quote and the next delimiter or terminator are
//!   appended to the value (csv-core's lenient behaviour): `"a"b` → `ab`.
//! - A file that ends inside a quoted field: the last record ends at EOF and
//!   the parser reports [`ParseOutcome::UnterminatedQuote`].

use std::{borrow::Cow, ops::Range};

use smallvec::SmallVec;

use crate::dialect::{Dialect, Encoding, EscapeStyle};

/// Byte range of one field inside its record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldRange {
    /// Raw bytes of the field relative to the record start, **including** the
    /// surrounding quotes of a quoted field. The record terminator is never
    /// included.
    pub raw: Range<u32>,
    /// The field starts with the quote character.
    pub quoted: bool,
    /// The value differs from the raw bytes inside the quotes: it contains a
    /// doubled quote or backslash escape, bytes after the closing quote, or
    /// the closing quote is missing (EOF). [`field_bytes`] then unescapes it.
    pub has_escapes: bool,
}

/// One record: absolute bounds plus field ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordRanges {
    /// Absolute offset of the record's first byte.
    pub start: u64,
    /// Absolute offset just past the terminator (EOF for the last record).
    pub end: u64,
    /// Fields, in order. Inline (no allocation) up to 32 fields.
    pub fields: SmallVec<[FieldRange; 32]>,
}

impl RecordRanges {
    /// Raw bytes of field `i` (quotes included), or `None` if out of range.
    pub fn raw_field<'a>(&self, bytes: &'a [u8], i: usize) -> Option<&'a [u8]> {
        let f = self.fields.get(i)?;
        let s = self.start as usize;
        Some(&bytes[s + f.raw.start as usize..s + f.raw.end as usize])
    }
}

/// Result of [`RecordParser::parse_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// A record was parsed; the next one starts at or after `next`.
    Record {
        /// Offset just past this record's terminator.
        next: u64,
    },
    /// No record at or after the offset (only blank or comment lines left).
    Eof,
    /// The record ran to EOF inside a quoted field (§16). It is still a row.
    UnterminatedQuote {
        /// Always the file length.
        next: u64,
    },
}

/// Result of [`RecordParser::skip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipOutcome {
    /// All `n` records were skipped. `next` is just past the last skipped
    /// record's terminator: the same offset `n` calls to `parse_at` reach.
    Skipped {
        /// Offset to continue from.
        next: u64,
    },
    /// EOF came first, after `skipped` records.
    Eof {
        /// Records skipped before EOF.
        skipped: u64,
    },
}

/// Warning attached to a parsed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseWarning {
    /// The row ran to EOF inside a quoted field.
    UnterminatedQuote,
}

/// How a row's field count compares to the column count `H` (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ragged {
    /// Exactly `H` fields.
    #[default]
    Exact,
    /// Fewer fields: this many cells are missing and render empty (null).
    Short(u32),
    /// More fields: this many extra fields go to `_extra1`, `_extra2`, …
    Long(u32),
}

impl Ragged {
    /// Classifies a row with `fields` fields against `width` columns.
    pub fn classify(fields: usize, width: usize) -> Ragged {
        use std::cmp::Ordering;
        let diff = |d: usize| u32::try_from(d).unwrap_or(u32::MAX);
        match fields.cmp(&width) {
            Ordering::Equal => Ragged::Exact,
            Ordering::Less => Ragged::Short(diff(width - fields)),
            Ordering::Greater => Ragged::Long(diff(fields - width)),
        }
    }

    /// True for `Short` and `Long`.
    pub fn is_ragged(self) -> bool {
        self != Ragged::Exact
    }
}

/// Name of the `n`-th synthetic column (1-based) for extra fields: `_extra1`.
pub fn extra_column_name(n: usize) -> String {
    format!("_extra{n}")
}

/// Reusable record parser for one dialect. Cheap to build, never allocates.
#[derive(Debug, Clone)]
pub struct RecordParser {
    delimiter: u8,
    quote: Option<u8>,
    backslash: bool,
    comment: Option<u8>,
}

impl RecordParser {
    /// A parser for `d`.
    pub fn new(d: &Dialect) -> Self {
        RecordParser {
            delimiter: d.delimiter,
            quote: d.quote,
            backslash: d.escape == EscapeStyle::Backslash,
            comment: d.comment,
        }
    }

    /// Skips blank and comment lines at `pos`. Returns the first byte of the
    /// next record, or `bytes.len()`.
    pub fn skip_ignorable(&self, bytes: &[u8], mut pos: usize) -> usize {
        while pos < bytes.len() {
            match bytes[pos] {
                b'\n' => pos += 1,
                b'\r' if bytes.get(pos + 1) == Some(&b'\n') => pos += 2,
                b if Some(b) == self.comment => {
                    pos = memchr::memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |i| pos + i + 1);
                }
                _ => break,
            }
        }
        pos
    }

    /// Parses the record at or after `offset` (blank and comment lines are
    /// skipped first) into `out`.
    pub fn parse_at(&mut self, bytes: &[u8], offset: u64, out: &mut RecordRanges) -> ParseOutcome {
        out.fields.clear();
        let len = bytes.len();
        let start = self.skip_ignorable(bytes, (offset as usize).min(len));
        out.start = start as u64;
        if start >= len {
            out.end = len as u64;
            return ParseOutcome::Eof;
        }
        let rel = |p: usize| u32::try_from(p - start).unwrap_or(u32::MAX);
        let mut pos = start;
        loop {
            let field_start = pos;
            let mut quoted = false;
            let mut escapes = false;
            if let Some(q) = self.quote
                && pos < len
                && bytes[pos] == q
            {
                quoted = true;
                match self.close_quote(bytes, pos + 1, &mut escapes) {
                    Some(after) => pos = after,
                    None => {
                        out.fields.push(FieldRange {
                            raw: rel(field_start)..rel(len),
                            quoted,
                            has_escapes: true,
                        });
                        out.end = len as u64;
                        return ParseOutcome::UnterminatedQuote { next: len as u64 };
                    }
                }
                // Bytes after the closing quote, other than a delimiter or a
                // terminator, are appended to the value.
                if pos < len
                    && bytes[pos] != self.delimiter
                    && bytes[pos] != b'\n'
                    && !(bytes[pos] == b'\r' && matches!(bytes.get(pos + 1), None | Some(b'\n')))
                {
                    escapes = true;
                }
            }
            match memchr::memchr2(self.delimiter, b'\n', &bytes[pos..]) {
                Some(i) if bytes[pos + i] == self.delimiter => {
                    out.fields.push(FieldRange {
                        raw: rel(field_start)..rel(pos + i),
                        quoted,
                        has_escapes: escapes,
                    });
                    pos += i + 1;
                }
                found => {
                    let (end, next) = match found {
                        Some(i) => (pos + i, pos + i + 1),
                        None => (len, len),
                    };
                    let field_end = if end > pos && bytes[end - 1] == b'\r' {
                        end - 1
                    } else {
                        end
                    };
                    out.fields.push(FieldRange {
                        raw: rel(field_start)..rel(field_end),
                        quoted,
                        has_escapes: escapes,
                    });
                    out.end = next as u64;
                    return ParseOutcome::Record { next: next as u64 };
                }
            }
        }
    }

    /// From just after an opening quote, finds the closing quote. Returns the
    /// offset just past it, or `None` at EOF (unterminated). Sets `escapes`
    /// when an escape sequence was seen.
    #[inline]
    fn close_quote(&self, bytes: &[u8], mut pos: usize, escapes: &mut bool) -> Option<usize> {
        let q = self.quote?;
        loop {
            let hit = if self.backslash {
                memchr::memchr2(q, b'\\', &bytes[pos.min(bytes.len())..])?
            } else {
                memchr::memchr(q, &bytes[pos.min(bytes.len())..])?
            };
            let i = pos + hit;
            if bytes[i] != q {
                // Backslash: the next byte is literal.
                *escapes = true;
                if i + 1 >= bytes.len() {
                    return None;
                }
                pos = i + 2;
            } else if !self.backslash && bytes.get(i + 1) == Some(&q) {
                *escapes = true;
                pos = i + 2;
            } else {
                return Some(i + 1);
            }
        }
    }

    /// Finds the end of the record starting at `start` (a record start, not a
    /// blank or comment line). Returns `(next, unterminated)`.
    #[inline]
    fn record_end(&self, bytes: &[u8], start: usize) -> (usize, bool) {
        let len = bytes.len();
        let Some(q) = self.quote else {
            return (
                memchr::memchr(b'\n', &bytes[start..]).map_or(len, |i| start + i + 1),
                false,
            );
        };
        let mut pos = start;
        let mut scratch = false;
        loop {
            match memchr::memchr2(b'\n', q, &bytes[pos..]) {
                None => return (len, false),
                Some(i) if bytes[pos + i] == b'\n' => return (pos + i + 1, false),
                Some(i) => {
                    let at = pos + i;
                    if at == start || bytes[at - 1] == self.delimiter {
                        match self.close_quote(bytes, at + 1, &mut scratch) {
                            Some(after) => pos = after,
                            None => return (len, true),
                        }
                    } else {
                        pos = at + 1;
                    }
                }
            }
        }
    }

    /// Bounds of the record at or after `offset` (blank and comment lines
    /// skipped), without field ranges: `(start, next, unterminated)`, or
    /// `None` at EOF.
    pub fn next_record(&self, bytes: &[u8], offset: u64) -> Option<(u64, u64, bool)> {
        let start = self.skip_ignorable(bytes, (offset as usize).min(bytes.len()));
        if start >= bytes.len() {
            return None;
        }
        let (next, unterminated) = self.record_end(bytes, start);
        Some((start as u64, next as u64, unterminated))
    }

    /// Skips `n` records from `offset` without building field ranges (§5.2
    /// forward scan). Lands on the same offset as `n` calls to `parse_at`,
    /// with the same blank-line and comment rules.
    pub fn skip(&mut self, bytes: &[u8], offset: u64, n: u64) -> SkipOutcome {
        let mut pos = (offset as usize).min(bytes.len());
        for done in 0..n {
            let start = self.skip_ignorable(bytes, pos);
            if start >= bytes.len() {
                return SkipOutcome::Eof { skipped: done };
            }
            pos = self.record_end(bytes, start).0;
        }
        SkipOutcome::Skipped { next: pos as u64 }
    }

    /// Value of field `i` of `rec`: the raw slice for an unquoted field, the
    /// slice inside the quotes for a quoted field without escapes, or the
    /// unescaped value written into `scratch`. Empty if `i` is out of range.
    pub fn field_value<'a>(
        &self,
        bytes: &'a [u8],
        rec: &RecordRanges,
        i: usize,
        scratch: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        match (rec.fields.get(i), rec.raw_field(bytes, i)) {
            (Some(f), Some(raw)) => self.unescape(raw, f, scratch),
            _ => &[],
        }
    }

    /// Value of a field given its raw bytes (see [`RecordParser::field_value`]).
    pub fn unescape<'a>(
        &self,
        raw: &'a [u8],
        f: &FieldRange,
        scratch: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        match self.quote {
            Some(q) if f.quoted => field_bytes(raw, f, q, self.backslash, scratch),
            _ => raw,
        }
    }
}

/// Value of a field from its raw bytes. See [`RecordParser::field_value`].
pub fn field_bytes<'a>(
    raw: &'a [u8],
    f: &FieldRange,
    quote: u8,
    backslash: bool,
    scratch: &'a mut Vec<u8>,
) -> &'a [u8] {
    if !f.quoted {
        return raw;
    }
    if !f.has_escapes && raw.len() >= 2 {
        return &raw[1..raw.len() - 1];
    }
    // Mirrors csv-core's states inside a quoted field.
    #[derive(PartialEq)]
    enum St {
        Quoted,
        Escape,
        QuoteSeen,
        Unquoted,
    }
    scratch.clear();
    let mut st = St::Quoted;
    for &b in raw.iter().skip(1) {
        st = match st {
            St::Quoted if b == quote => St::QuoteSeen,
            St::Quoted if backslash && b == b'\\' => St::Escape,
            St::Quoted | St::Escape => {
                scratch.push(b);
                St::Quoted
            }
            St::QuoteSeen if !backslash && b == quote => {
                scratch.push(b);
                St::Quoted
            }
            St::QuoteSeen | St::Unquoted => {
                scratch.push(b);
                St::Unquoted
            }
        };
    }
    scratch
}

/// Decodes a field for display or query evaluation (§6.2).
///
/// - UTF-8 (with or without BOM): lossy, invalid sequences become `�`.
/// - Windows-1252: transcoded with `encoding_rs`.
/// - UTF-16 never reaches the parser (files are transcoded first); it is
///   treated as UTF-8.
pub fn decode_field(raw: &[u8], enc: Encoding) -> Cow<'_, str> {
    match enc {
        Encoding::Windows1252 => encoding_rs::WINDOWS_1252.decode_without_bom_handling(raw).0,
        _ => String::from_utf8_lossy(raw),
    }
}

/// A piece of a display string: plain text, or an escaped control character
/// that the UI draws with `theme.control_char()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment<'a> {
    /// Text to draw. Never contains a control character.
    pub text: Cow<'a, str>,
    /// True for escape sequences such as `\t` or `\x1b`.
    pub escaped: bool,
}

/// The escape sequence for `c`, if it must not reach the terminator raw.
fn escape_char(c: char) -> Option<Cow<'static, str>> {
    Some(match c {
        '\t' => Cow::Borrowed("\\t"),
        '\r' => Cow::Borrowed("\\r"),
        '\n' => Cow::Borrowed("\\n"),
        '\x1b' => Cow::Borrowed("\\x1b"),
        '\0'..='\x1f' | '\x7f' | '\u{80}'..='\u{9f}' => Cow::Owned(format!("\\x{:02x}", c as u32)),
        // Bidi embeddings, overrides and isolates can reorder terminal output.
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
            Cow::Owned(format!("\\u{{{:x}}}", c as u32))
        }
        _ => return None,
    })
}

/// Splits `s` into plain and escaped segments (§6.2).
///
/// `\t`, `\r`, `\n` and ESC become `\t`, `\r`, `\n`, `\x1b`; other C0
/// controls, DEL and C1 controls become `\xHH`; bidi overrides U+202A–U+202E
/// and U+2066–U+2069 become `\u{202e}`. Guarantee: no control character is
/// left in any segment.
pub fn display_segments(s: &str) -> SmallVec<[Segment<'_>; 4]> {
    let mut out: SmallVec<[Segment<'_>; 4]> = SmallVec::new();
    let mut plain_start = 0;
    for (i, c) in s.char_indices() {
        let Some(esc) = escape_char(c) else {
            continue;
        };
        if plain_start < i {
            out.push(Segment {
                text: Cow::Borrowed(&s[plain_start..i]),
                escaped: false,
            });
        }
        match out.last_mut() {
            Some(last) if last.escaped && plain_start == i => last.text.to_mut().push_str(&esc),
            _ => out.push(Segment {
                text: esc,
                escaped: true,
            }),
        }
        plain_start = i + c.len_utf8();
    }
    if plain_start < s.len() || out.is_empty() {
        out.push(Segment {
            text: Cow::Borrowed(&s[plain_start..]),
            escaped: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn dialect(escape: EscapeStyle, comment: Option<u8>) -> Dialect {
        Dialect {
            escape,
            comment,
            ..Dialect::default()
        }
    }

    /// All records of `bytes` as unescaped string values.
    fn parse_all(d: &Dialect, bytes: &[u8]) -> Vec<Vec<String>> {
        let mut p = RecordParser::new(d);
        let mut rec = RecordRanges::default();
        let mut scratch = Vec::new();
        let mut pos = 0;
        let mut rows = Vec::new();
        loop {
            let next = match p.parse_at(bytes, pos, &mut rec) {
                ParseOutcome::Eof => break,
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
            };
            rows.push(
                (0..rec.fields.len())
                    .map(|i| {
                        String::from_utf8_lossy(p.field_value(bytes, &rec, i, &mut scratch))
                            .into_owned()
                    })
                    .collect(),
            );
            pos = next;
        }
        rows
    }

    #[test]
    fn quoted_fields_and_raw_ranges() {
        let bytes = b"a,\"b,c\",\"d\"\"e\",\"f\ng\"\n";
        let d = Dialect::default();
        let mut p = RecordParser::new(&d);
        let mut rec = RecordRanges::default();
        assert_eq!(
            p.parse_at(bytes, 0, &mut rec),
            ParseOutcome::Record {
                next: bytes.len() as u64
            }
        );
        assert_eq!(rec.fields.len(), 4);
        let mut scratch = Vec::new();
        let values: Vec<Vec<u8>> = (0..4)
            .map(|i| p.field_value(bytes, &rec, i, &mut scratch).to_vec())
            .collect();
        assert_eq!(values, [&b"a"[..], b"b,c", b"d\"e", b"f\ng"]);
        let raws: Vec<&[u8]> = (0..4).map(|i| rec.raw_field(bytes, i).unwrap()).collect();
        assert_eq!(raws, [&b"a"[..], b"\"b,c\"", b"\"d\"\"e\"", b"\"f\ng\""]);
        assert!(!rec.fields[0].quoted && rec.fields[1].quoted);
        assert!(!rec.fields[1].has_escapes && rec.fields[2].has_escapes);
    }

    #[test]
    fn backslash_escape() {
        let d = dialect(EscapeStyle::Backslash, None);
        assert_eq!(parse_all(&d, b"\"d\\\"e\",x\n"), [["d\"e", "x"]]);
        // An escaped newline stays inside the field.
        assert_eq!(parse_all(&d, b"\"a\\\nb\"\nc\n"), [vec!["a\nb"], vec!["c"]]);
    }

    #[test]
    fn mixed_line_endings_strip_cr() {
        let d = Dialect::default();
        assert_eq!(
            parse_all(&d, b"a,b\r\nc,d\ne,f\r\n\"g\",\"h\"\r\n"),
            [["a", "b"], ["c", "d"], ["e", "f"], ["g", "h"]]
        );
        // A `\r` at EOF is a terminator too; a lone `\r` is data.
        assert_eq!(parse_all(&d, b"a,b\r"), [["a", "b"]]);
        assert_eq!(parse_all(&d, b"a\rb,c\n"), [["a\rb", "c"]]);
    }

    #[test]
    fn unterminated_quote_at_eof() {
        let d = Dialect::default();
        let bytes = b"a,b\nc,\"d\ne";
        let mut p = RecordParser::new(&d);
        let mut rec = RecordRanges::default();
        assert_eq!(
            p.parse_at(bytes, 0, &mut rec),
            ParseOutcome::Record { next: 4 }
        );
        assert_eq!(
            p.parse_at(bytes, 4, &mut rec),
            ParseOutcome::UnterminatedQuote {
                next: bytes.len() as u64
            }
        );
        assert_eq!(rec.end, bytes.len() as u64);
        let mut scratch = Vec::new();
        assert_eq!(p.field_value(bytes, &rec, 1, &mut scratch), b"d\ne");
        assert_eq!(
            p.parse_at(bytes, bytes.len() as u64, &mut rec),
            ParseOutcome::Eof
        );
    }

    #[test]
    fn blank_and_comment_lines_are_not_rows() {
        let d = dialect(EscapeStyle::Doubled, Some(b'#'));
        let bytes = b"\n# c,\"\na,b\n\r\n\n#x\nc,d\n#end";
        assert_eq!(parse_all(&d, bytes), [["a", "b"], ["c", "d"]]);
        // Without a comment prefix, `#` lines are data.
        assert_eq!(parse_all(&Dialect::default(), b"#x\n").len(), 1);
    }

    #[test]
    fn skip_matches_parse_at() {
        let d = dialect(EscapeStyle::Doubled, Some(b'#'));
        let bytes = b"a,b\n\n#c\n\"x\ny\",z\r\n\r\n#\"\nq\n\"unterminated\n";
        let mut p = RecordParser::new(&d);
        let mut rec = RecordRanges::default();
        let mut offsets = vec![0u64];
        let mut pos = 0;
        loop {
            match p.parse_at(bytes, pos, &mut rec) {
                ParseOutcome::Eof => break,
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                    offsets.push(next);
                    pos = next;
                }
            }
        }
        assert_eq!(offsets.len(), 5);
        for (n, &want) in offsets.iter().enumerate() {
            assert_eq!(
                p.skip(bytes, 0, n as u64),
                SkipOutcome::Skipped { next: want },
                "n = {n}"
            );
        }
        assert_eq!(p.skip(bytes, 0, 5), SkipOutcome::Eof { skipped: 4 });
    }

    #[test]
    fn quotes_only_open_at_field_start() {
        let d = Dialect::default();
        assert_eq!(
            parse_all(&d, b"5'10\",x\ny,z\n"),
            [["5'10\"", "x"], ["y", "z"]]
        );
        assert_eq!(parse_all(&d, b"\"a\"b,c\n"), [["ab", "c"]]);
        assert_eq!(parse_all(&d, b"a,\n"), [["a", ""]]);
        assert_eq!(parse_all(&d, b"a,"), [["a", ""]]);
    }

    #[test]
    fn no_quoting() {
        let d = Dialect {
            quote: None,
            ..Dialect::default()
        };
        assert_eq!(parse_all(&d, b"\"a,b\"\n"), [["\"a", "b\""]]);
    }

    #[test]
    fn segments_escape_control_chars() {
        let segs = display_segments("a\x1b[31mb");
        assert!(segs.iter().all(|s| !s.text.contains('\x1b')));
        assert_eq!(
            segs.iter()
                .map(|s| (s.text.as_ref(), s.escaped))
                .collect::<Vec<_>>(),
            [("a", false), ("\\x1b", true), ("[31mb", false)]
        );
        let segs = display_segments("x\t\r\n\u{0}\u{7f}\u{85}\u{202e}y");
        assert_eq!(
            segs.iter()
                .map(|s| (s.text.as_ref(), s.escaped))
                .collect::<Vec<_>>(),
            [
                ("x", false),
                ("\\t\\r\\n\\x00\\x7f\\x85\\u{202e}", true),
                ("y", false)
            ]
        );
        assert_eq!(display_segments("").len(), 1);
        assert_eq!(display_segments("plain")[0].text, "plain");
    }

    #[test]
    fn invalid_utf8_displays_replacement() {
        let bytes = b"ok,a\xFFb\n";
        let d = Dialect::default();
        let mut p = RecordParser::new(&d);
        let mut rec = RecordRanges::default();
        p.parse_at(bytes, 0, &mut rec);
        let raw = rec.raw_field(bytes, 1).unwrap();
        assert_eq!(raw, b"a\xFFb");
        assert_eq!(decode_field(raw, Encoding::Utf8), "a\u{FFFD}b");
        assert_eq!(decode_field(b"caf\xE9", Encoding::Windows1252), "café");
    }

    #[test]
    fn ragged_classification() {
        assert_eq!(Ragged::classify(5, 5), Ragged::Exact);
        assert_eq!(Ragged::classify(2, 5), Ragged::Short(3));
        assert_eq!(Ragged::classify(7, 5), Ragged::Long(2));
        assert!(!Ragged::Exact.is_ragged());
        assert_eq!(extra_column_name(1), "_extra1");
    }
}
