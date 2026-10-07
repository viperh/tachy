//! `Dialect` struct and the sniffer that detects it (spec §2, §2.1).
//!
//! [`sniff`] looks at a sample from the start of a file and returns a
//! [`SniffReport`]: the detected [`Dialect`], the dialect after the user's
//! [`DialectOverrides`] are applied, and the facts the indexer and the Detected
//! format dialog need (quoted newlines, whether the sample was cut).
//!
//! Detection runs in the order of spec §2.1: encoding, delimiter, quote char,
//! header, quoted newlines. Escape style and line ending come from the same
//! sample. The comment prefix is never detected: it only comes from
//! `--comment` (and the config, M6-03).

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use serde::{Deserialize, Serialize};

use crate::parse::{self, ParseOutcome, RecordParser, RecordRanges};

/// Default number of bytes the sniffer reads (spec §2.1, `sniff.sample_bytes`).
pub const DEFAULT_SAMPLE_BYTES: usize = 65_536;

/// How far past `sample_bytes` the sniffer extends the sample to reach the end
/// of the current line. A file that is one huge line cannot stall it.
pub const MAX_SAMPLE_EXTENSION: usize = 1 << 20;

/// Delimiter candidates, in tie-break order (spec §2, §2.1).
pub const DELIMITER_CANDIDATES: [u8; 6] = *b",\t|;: ";

/// Text encodings tachy reads (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encoding {
    /// UTF-8 without a byte-order mark.
    #[default]
    Utf8,
    /// UTF-8 starting with the `EF BB BF` byte-order mark.
    Utf8Bom,
    /// UTF-16 little endian, with the `FF FE` byte-order mark.
    Utf16Le,
    /// UTF-16 big endian, with the `FE FF` byte-order mark.
    Utf16Be,
    /// Windows-1252, which `encoding_rs` also uses for Latin-1 (WHATWG).
    Windows1252,
}

impl Encoding {
    /// The name shown in the status line and the Detected format dialog.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf8Bom => "utf-8 bom",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Windows1252 => "windows-1252",
        }
    }

    /// The matching `encoding_rs` encoding.
    pub fn to_encoding_rs(self) -> &'static encoding_rs::Encoding {
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom => encoding_rs::UTF_8,
            Encoding::Utf16Le => encoding_rs::UTF_16LE,
            Encoding::Utf16Be => encoding_rs::UTF_16BE,
            Encoding::Windows1252 => encoding_rs::WINDOWS_1252,
        }
    }

    /// True for the UTF-16 encodings, which are transcoded to a UTF-8 temp
    /// file before parsing (`spool::transcode_to_utf8`).
    pub fn is_utf16(self) -> bool {
        matches!(self, Encoding::Utf16Le | Encoding::Utf16Be)
    }

    /// The byte-order mark of this encoding, if it has one.
    pub fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8Bom => b"\xEF\xBB\xBF",
            Encoding::Utf16Le => b"\xFF\xFE",
            Encoding::Utf16Be => b"\xFE\xFF",
            Encoding::Utf8 | Encoding::Windows1252 => b"",
        }
    }

    /// Length of the byte-order mark at the start of `bytes` that belongs to
    /// this encoding. A UTF-8 BOM is also skipped when the encoding was forced
    /// to plain `Utf8`.
    pub fn bom_len_in(self, bytes: &[u8]) -> usize {
        let bom: &[u8] = match self {
            Encoding::Utf8 | Encoding::Utf8Bom => b"\xEF\xBB\xBF",
            other => other.bom(),
        };
        if !bom.is_empty() && bytes.starts_with(bom) {
            bom.len()
        } else {
            0
        }
    }
}

/// How a quote character inside a quoted field is escaped (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EscapeStyle {
    /// A doubled quote: `""`.
    #[default]
    Doubled,
    /// A backslash: `\"`. The byte after a backslash inside quotes is literal.
    Backslash,
}

/// Line endings seen in the sample. The parser accepts `\n` and `\r\n` in any
/// mix, so this is informational (status line and dialog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineEnding {
    /// Only `\n`.
    #[default]
    Lf,
    /// Only `\r\n`.
    CrLf,
    /// Both.
    Mixed,
}

/// The format of a delimited file (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dialect {
    /// Field delimiter: any single byte.
    pub delimiter: u8,
    /// Quote byte, or `None` when quoting is off.
    pub quote: Option<u8>,
    /// How quotes are escaped inside quoted fields.
    pub escape: EscapeStyle,
    /// Line endings seen in the sample.
    pub line_ending: LineEnding,
    /// Whether the first record is a header.
    pub header: bool,
    /// Text encoding.
    pub encoding: Encoding,
    /// Lines starting with this byte are skipped. Never auto-detected.
    pub comment: Option<u8>,
}

impl Default for Dialect {
    /// RFC 4180 style: `,`, `"`, doubled quotes, `\n`, header, UTF-8.
    fn default() -> Self {
        Dialect {
            delimiter: b',',
            quote: Some(b'"'),
            escape: EscapeStyle::Doubled,
            line_ending: LineEnding::Lf,
            header: true,
            encoding: Encoding::Utf8,
            comment: None,
        }
    }
}

impl Dialect {
    /// One-line summary for the status line (spec §11.5), items separated by
    /// two spaces: `delim ,  quote "  utf-8  header`.
    ///
    /// Invisible delimiters get names (`tab`, `space`), other control bytes
    /// print as `0x1f`. A backslash escape and a comment prefix are appended
    /// only when set (`escape \`, `comment #`), so the common case keeps the
    /// spec's exact form.
    pub fn summary(&self) -> String {
        let mut out = format!("delim {}", byte_name(self.delimiter));
        match self.quote {
            Some(q) => {
                out.push_str("  quote ");
                out.push_str(&byte_name(q));
            }
            None => out.push_str("  quote none"),
        }
        if self.escape == EscapeStyle::Backslash {
            out.push_str("  escape \\");
        }
        out.push_str("  ");
        out.push_str(self.encoding.name());
        out.push_str(if self.header {
            "  header"
        } else {
            "  no header"
        });
        if let Some(c) = self.comment {
            out.push_str("  comment ");
            out.push_str(&byte_name(c));
        }
        out
    }

    /// The dialect to use for a file transcoded from UTF-16 to UTF-8
    /// (`spool::transcode_to_utf8`): the same, with `encoding = Utf8`.
    pub fn transcoded(self) -> Dialect {
        Dialect {
            encoding: Encoding::Utf8,
            ..self
        }
    }
}

/// Readable name of a delimiter or quote byte.
fn byte_name(b: u8) -> String {
    match b {
        b'\t' => "tab".to_string(),
        b' ' => "space".to_string(),
        0x21..=0x7E => (b as char).to_string(),
        _ => format!("0x{b:02x}"),
    }
}

/// Dialect settings forced by the user. `None` means "auto-detect".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DialectOverrides {
    /// Field delimiter byte.
    pub delimiter: Option<u8>,
    /// Quote character. `Some(None)` means quoting is disabled.
    pub quote: Option<Option<u8>>,
    /// Quote escape style.
    pub escape: Option<EscapeStyle>,
    /// Whether the first row is a header.
    pub header: Option<bool>,
    /// Text encoding.
    pub encoding: Option<Encoding>,
    /// Lines starting with this byte are skipped.
    pub comment: Option<u8>,
}

impl DialectOverrides {
    /// True when no field is set.
    pub fn is_empty(&self) -> bool {
        *self == DialectOverrides::default()
    }

    /// `dialect` with every set override applied.
    pub fn apply(&self, dialect: Dialect) -> Dialect {
        Dialect {
            delimiter: self.delimiter.unwrap_or(dialect.delimiter),
            quote: self.quote.unwrap_or(dialect.quote),
            escape: self.escape.unwrap_or(dialect.escape),
            line_ending: dialect.line_ending,
            header: self.header.unwrap_or(dialect.header),
            encoding: self.encoding.unwrap_or(dialect.encoding),
            comment: self.comment.or(dialect.comment),
        }
    }
}

/// Result of [`sniff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniffReport {
    /// Pure detection, run without any override. The Detected format dialog
    /// reverts to `overrides.apply(detected)` on `Esc` (D3).
    pub detected: Dialect,
    /// The dialect to open with: detection where the overrides were used by
    /// the dependent steps, with the overrides applied.
    pub dialect: Dialect,
    /// A quoted field in the sample contains `\n` or `\r` (§2.1 step 5). It
    /// selects the indexer's slow path (§5.3).
    pub quoted_newlines: bool,
    /// Bytes of the file the sample covers, BOM included (before any
    /// transcoding).
    pub sample_len: usize,
    /// The sample ended in the middle of a record.
    pub sample_truncated: bool,
}

/// Detects the dialect of `bytes` (the whole file; only a sample is read).
///
/// `sample_bytes` is the sample size (spec default
/// [`DEFAULT_SAMPLE_BYTES`]). The sample is extended to the end of the current
/// line, by at most [`MAX_SAMPLE_EXTENSION`]. Overrides are applied before the
/// steps that depend on them: a forced encoding skips step 1, a forced
/// delimiter is used by steps 3–5 and a forced quote by steps 2, 4 and 5.
pub fn sniff(bytes: &[u8], sample_bytes: usize, overrides: &DialectOverrides) -> SniffReport {
    let pure = detect(bytes, sample_bytes, &DialectOverrides::default());
    let forced = if overrides.is_empty() {
        pure.clone()
    } else {
        detect(bytes, sample_bytes, overrides)
    };
    SniffReport {
        detected: pure.dialect,
        dialect: overrides.apply(forced.dialect),
        quoted_newlines: forced.quoted_newlines,
        sample_len: forced.sample_len,
        sample_truncated: forced.truncated,
    }
}

#[derive(Clone)]
struct Detection {
    dialect: Dialect,
    quoted_newlines: bool,
    sample_len: usize,
    truncated: bool,
}

fn detect(bytes: &[u8], sample_bytes: usize, overrides: &DialectOverrides) -> Detection {
    // Step 1: encoding. A BOM decides it; UTF-16 needs one.
    let bom_encoding = overrides.encoding.or_else(|| detect_bom(bytes));
    let bom_len = bom_encoding.map_or(0, |e| e.bom_len_in(bytes));
    let body = &bytes[bom_len..];
    let utf16 = bom_encoding.filter(|e| e.is_utf16());
    let (raw_sample, mut truncated) = sample_window(body, sample_bytes.max(1), utf16);
    let whole = raw_sample.len() == body.len();
    let encoding = bom_encoding.unwrap_or_else(|| detect_utf8(raw_sample));

    // UTF-16 is analysed as UTF-8 (the file is transcoded before parsing).
    let text: Cow<'_, [u8]> = if encoding.is_utf16() {
        let even = &raw_sample[..raw_sample.len() & !1];
        let (decoded, _) = encoding.to_encoding_rs().decode_without_bom_handling(even);
        Cow::Owned(decoded.into_owned().into_bytes())
    } else {
        Cow::Borrowed(raw_sample)
    };
    let text = &*text;
    let comment = overrides.comment;

    // Step 2: delimiter, splitting records with `"` (or the forced quote).
    let split_quote = overrides.quote.unwrap_or(Some(b'"'));
    let mut delimiter = overrides
        .delimiter
        .unwrap_or_else(|| best_delimiter(&delimiter_scores(text, split_quote, comment, whole)));

    // Step 3: quote char.
    let quote = match overrides.quote {
        Some(q) => q,
        None => {
            let (double, single) = quote_counts(text, delimiter);
            if single > double {
                if overrides.delimiter.is_none() {
                    delimiter =
                        best_delimiter(&delimiter_scores(text, Some(b'\''), comment, whole));
                }
                Some(b'\'')
            } else if double > 0 {
                Some(b'"')
            } else {
                None
            }
        }
    };

    let escape = overrides.escape.unwrap_or_else(|| match quote {
        Some(q) => detect_escape(text, q, delimiter),
        None => EscapeStyle::Doubled,
    });

    let mut dialect = Dialect {
        delimiter,
        quote,
        escape,
        line_ending: LineEnding::Lf,
        header: false,
        encoding,
        comment,
    };

    let records = sample_records(text, &dialect, whole);
    truncated |= records.cut_last;
    dialect.line_ending = records.line_ending;

    // Step 4: header.
    dialect.header = overrides
        .header
        .unwrap_or_else(|| detect_header(&records.rows, encoding));

    // Step 5: quoted newlines (from the same parse).
    Detection {
        dialect,
        quoted_newlines: records.quoted_newlines,
        sample_len: bom_len + raw_sample.len(),
        truncated,
    }
}

/// Encoding from a byte-order mark, if there is one.
pub fn detect_bom(bytes: &[u8]) -> Option<Encoding> {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        Some(Encoding::Utf8Bom)
    } else if bytes.starts_with(b"\xFF\xFE") {
        Some(Encoding::Utf16Le)
    } else if bytes.starts_with(b"\xFE\xFF") {
        Some(Encoding::Utf16Be)
    } else {
        None
    }
}

/// UTF-8 when the sample is valid UTF-8 (an incomplete sequence at the very
/// end is fine: the sample may cut a character), Windows-1252 otherwise.
pub fn detect_utf8(sample: &[u8]) -> Encoding {
    match std::str::from_utf8(sample) {
        Ok(_) => Encoding::Utf8,
        Err(e) if e.error_len().is_none() => Encoding::Utf8,
        Err(_) => Encoding::Windows1252,
    }
}

/// The sample: `sample_bytes`, extended to the end of the current line by at
/// most [`MAX_SAMPLE_EXTENSION`]. Also returns whether that cap was hit.
fn sample_window(body: &[u8], sample_bytes: usize, utf16: Option<Encoding>) -> (&[u8], bool) {
    if body.len() <= sample_bytes {
        return (body, false);
    }
    let cap = (sample_bytes + MAX_SAMPLE_EXTENSION).min(body.len());
    match utf16 {
        None => {
            if body[sample_bytes - 1] == b'\n' {
                return (&body[..sample_bytes], false);
            }
            match memchr::memchr(b'\n', &body[sample_bytes..cap]) {
                Some(i) => (&body[..sample_bytes + i + 1], false),
                None => (&body[..cap], cap < body.len()),
            }
        }
        Some(enc) => {
            let nl: [u8; 2] = if enc == Encoding::Utf16Le {
                [b'\n', 0]
            } else {
                [0, b'\n']
            };
            let mut i = (sample_bytes & !1).saturating_sub(2);
            while i + 1 < cap {
                if body[i..i + 2] == nl {
                    return (&body[..i + 2], false);
                }
                i += 2;
            }
            (&body[..cap & !1], cap < body.len())
        }
    }
}

/// Per-candidate delimiter score in [0, 1], in [`DELIMITER_CANDIDATES`] order.
///
/// Records are split on `\n` outside quotes. A quote byte toggles the quoted
/// state wherever it appears (good enough for a heuristic, and independent of
/// the candidate). Blank lines and comment lines are skipped. Unless `whole`
/// (the sample is the entire file), an unterminated last record is ignored.
pub fn delimiter_scores(
    text: &[u8],
    quote: Option<u8>,
    comment: Option<u8>,
    whole: bool,
) -> [f64; 6] {
    let mut counts: Vec<[u32; 6]> = Vec::new();
    let mut current = [0u32; 6];
    let mut in_quotes = false;
    let mut record_start = 0;
    let mut has_content = false;
    let is_comment = |line: &[u8]| comment.is_some() && line.first() == comment.as_ref();
    for (i, &b) in text.iter().enumerate() {
        if Some(b) == quote {
            in_quotes = !in_quotes;
            has_content = true;
            continue;
        }
        if in_quotes {
            continue;
        }
        if b == b'\n' {
            let line = &text[record_start..i];
            let blank = line.is_empty() || line == b"\r";
            if has_content && !blank && !is_comment(line) {
                counts.push(current);
            }
            current = [0; 6];
            record_start = i + 1;
            has_content = false;
            continue;
        }
        has_content = true;
        if let Some(k) = DELIMITER_CANDIDATES.iter().position(|&c| c == b) {
            current[k] += 1;
        }
    }
    let last = &text[record_start..];
    if has_content && whole && !in_quotes && !is_comment(last) {
        counts.push(current);
    }

    let mut scores = [0.0; 6];
    for (k, score) in scores.iter_mut().enumerate() {
        let column: Vec<u32> = counts.iter().map(|c| c[k]).collect();
        *score = consistency_score(&column);
    }
    scores
}

/// Score for one candidate: the fraction of records whose count equals the
/// modal count, or 0 when the modal count is 0. With fewer than 2 records the
/// score is 1 when the modal count is above 0.
pub fn consistency_score(counts: &[u32]) -> f64 {
    let mut freq: HashMap<u32, usize> = HashMap::new();
    for &c in counts {
        *freq.entry(c).or_default() += 1;
    }
    // Most frequent count; ties go to the larger count.
    let Some((&mode, &mode_freq)) = freq.iter().max_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)))
    else {
        return 0.0;
    };
    if mode == 0 {
        0.0
    } else if counts.len() < 2 {
        1.0
    } else {
        mode_freq as f64 / counts.len() as f64
    }
}

/// The highest score wins; ties go to the earlier candidate. All zero (a
/// single-column file) → `,`.
pub fn best_delimiter(scores: &[f64; 6]) -> u8 {
    let mut best = 0;
    for k in 1..scores.len() {
        if scores[k] > scores[best] {
            best = k;
        }
    }
    if scores[best] > 0.0 {
        DELIMITER_CANDIDATES[best]
    } else {
        b','
    }
}

/// Counts of `"` and `'` next to field boundaries: right after the delimiter
/// or at a line start, or right before the delimiter or at a line end.
/// Returns `(double, single)`.
pub fn quote_counts(text: &[u8], delimiter: u8) -> (usize, usize) {
    let mut double = 0;
    let mut single = 0;
    for (i, &b) in text.iter().enumerate() {
        if b != b'"' && b != b'\'' {
            continue;
        }
        let opens = i == 0 || text[i - 1] == delimiter || text[i - 1] == b'\n';
        let closes = match text.get(i + 1) {
            None => true,
            Some(&n) => n == delimiter || n == b'\n' || n == b'\r',
        };
        let n = usize::from(opens) + usize::from(closes);
        if b == b'"' {
            double += n;
        } else {
            single += n;
        }
    }
    (double, single)
}

/// `Backslash` only if `\<quote>` appears inside fields and a doubled quote
/// does not. `Doubled` otherwise.
fn detect_escape(text: &[u8], quote: u8, delimiter: u8) -> EscapeStyle {
    let boundary =
        |b: Option<&u8>| matches!(b, None | Some(b'\n' | b'\r')) || b == Some(&delimiter);
    let mut backslash = 0;
    let mut doubled = 0;
    for i in 0..text.len().saturating_sub(1) {
        if text[i] == b'\\' && text[i + 1] == quote && !boundary(text.get(i + 2)) {
            backslash += 1;
        }
        if text[i] == quote && text[i + 1] == quote {
            // `""` as a whole (empty) field is not an escape.
            let at_start = i == 0 || text[i - 1] == delimiter || text[i - 1] == b'\n';
            if !(at_start && boundary(text.get(i + 2))) {
                doubled += 1;
            }
        }
    }
    if backslash > 0 && doubled == 0 {
        EscapeStyle::Backslash
    } else {
        EscapeStyle::Doubled
    }
}

/// Records of the sample, parsed with the dialect so far.
struct SampleRecords {
    /// Unescaped field values of the first complete records.
    rows: Vec<Vec<Vec<u8>>>,
    quoted_newlines: bool,
    line_ending: LineEnding,
    /// The last record was cut by the end of the sample (and dropped).
    cut_last: bool,
}

/// At most this many sample rows are kept for header detection.
const MAX_SAMPLE_ROWS: usize = 1_000;

fn sample_records(text: &[u8], dialect: &Dialect, whole: bool) -> SampleRecords {
    let mut parser = RecordParser::new(dialect);
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut rows = Vec::new();
    let (mut crlf, mut lf) = (0usize, 0usize);
    let mut quoted_newlines = false;
    let mut cut_last = false;
    let mut pos = 0u64;
    loop {
        let (next, unterminated) = match parser.parse_at(text, pos, &mut rec) {
            ParseOutcome::Eof => break,
            ParseOutcome::Record { next } => (next, false),
            ParseOutcome::UnterminatedQuote { next } => (next, true),
        };
        let start = rec.start as usize;
        quoted_newlines |= rec.fields.iter().any(|f| {
            f.quoted
                && text[start + f.raw.start as usize..start + f.raw.end as usize]
                    .iter()
                    .any(|&b| b == b'\n' || b == b'\r')
        });
        let end = next as usize;
        let terminated = !unterminated && text[end - 1] == b'\n';
        if !whole && !terminated {
            cut_last = true;
            break;
        }
        if terminated {
            if end >= 2 && text[end - 2] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
        if rows.len() < MAX_SAMPLE_ROWS {
            let values = (0..rec.fields.len())
                .map(|i| parser.field_value(text, &rec, i, &mut scratch).to_vec())
                .collect();
            rows.push(values);
        }
        pos = next;
    }
    let line_ending = match (crlf, lf) {
        (0, _) => LineEnding::Lf,
        (_, 0) => LineEnding::CrLf,
        _ => LineEnding::Mixed,
    };
    SampleRecords {
        rows,
        quoted_newlines,
        line_ending,
        cut_last,
    }
}

/// §2.1 step 4: is the first row a header?
fn detect_header(rows: &[Vec<Vec<u8>>], encoding: Encoding) -> bool {
    let Some((first, rest)) = rows.split_first() else {
        return false;
    };
    let decode = |v: &[u8]| parse::decode_field(v, encoding).into_owned();
    let first: Vec<String> = first.iter().map(|v| decode(v)).collect();
    let rest: Vec<Vec<String>> = rest
        .iter()
        .map(|r| r.iter().map(|v| decode(v)).collect())
        .collect();

    // (a) Row 1's type signature differs from the other rows': a non-typed
    // row-1 value over a column whose other values are ≥ 90 % typed.
    for (c, value) in first.iter().enumerate() {
        let v = value.trim();
        if v.is_empty() || is_typed(v) {
            continue;
        }
        let others: Vec<&str> = rest
            .iter()
            .filter_map(|r| r.get(c))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if others.is_empty() {
            continue;
        }
        let typed = others.iter().filter(|s| is_typed(s)).count();
        if typed * 10 >= others.len() * 9 {
            return true;
        }
    }

    // (b) Unique, non-empty, identifier-like values, at least one of which
    // appears nowhere else in its column.
    let mut seen = HashSet::new();
    if !first
        .iter()
        .all(|v| is_identifier(v) && seen.insert(v.as_str()))
    {
        return false;
    }
    first
        .iter()
        .enumerate()
        .any(|(c, v)| !rest.iter().any(|r| r.get(c) == Some(v)))
}

/// `[A-Za-z_][A-Za-z0-9_ .\-]*`, at most 64 characters.
fn is_identifier(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b' ' | b'.' | b'-'))
}

/// "Typed value" classifier for header detection: number, date, datetime
/// or bool. Uses the M3-01 recognisers, plus a lenient number check that
/// also accepts thousands separators (a header test, not a type).
fn is_typed(s: &str) -> bool {
    let b = s.as_bytes();
    is_number(s)
        || is_date(s)
        || is_bool(s)
        || crate::types::parse_f64(b).is_some()
        || crate::types::parse_datetime(b).is_some()
}

fn is_number(s: &str) -> bool {
    let t = s.strip_prefix(['+', '-']).unwrap_or(s);
    let t: String = t.chars().filter(|&c| c != '_' && c != ',').collect();
    t.bytes().any(|b| b.is_ascii_digit())
        && t.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && t.parse::<f64>().is_ok()
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    b.len() >= 10
        && (
            // YYYY-MM-DD or YYYY/MM/DD, optionally followed by a time.
            (digits(0..4)
                && matches!(b[4], b'-' | b'/')
                && digits(5..7)
                && b[7] == b[4]
                && digits(8..10))
            // DD/MM/YYYY, MM/DD/YYYY, DD.MM.YYYY, DD-MM-YYYY
            || (digits(0..2)
                && matches!(b[2], b'/' | b'.' | b'-')
                && digits(3..5)
                && b[5] == b[2]
                && digits(6..10))
        )
}

fn is_bool(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no"
    )
}

/// The name of a column (spec §9.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnName {
    /// Shown in the header row: the name exactly as written (decoded).
    pub display: String,
    /// Used in queries: trimmed and unique within the file.
    pub query: String,
}

/// Column names from the first record (unescaped field values).
///
/// - Headerless files get `col1`, `col2`, … (1-based, like `$1` in queries).
/// - An empty (or all-whitespace) header name becomes `colN`, for both names.
/// - The display name is the header value decoded with the file's encoding,
///   untouched. The query name is the same value trimmed of surrounding
///   whitespace.
/// - **Duplicates:** the display name stays as written, but repeated query
///   names get a suffix: `name`, `name_2`, `name_3`. A suffixed name that is
///   already a column's own name is skipped (`a, a, a_2` → `a, a_3, a_2`).
pub fn column_names(first_record: &[Vec<u8>], header: bool, enc: Encoding) -> Vec<ColumnName> {
    let bases: Vec<(String, String)> = first_record
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let fallback = format!("col{}", i + 1);
            if !header {
                return (fallback.clone(), fallback);
            }
            let display = parse::decode_field(raw, enc).into_owned();
            let query = display.trim().to_string();
            if query.is_empty() {
                (fallback.clone(), fallback)
            } else {
                (display, query)
            }
        })
        .collect();
    let own_names: HashSet<&str> = bases.iter().map(|(_, q)| q.as_str()).collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::with_capacity(bases.len());
    for (display, query) in &bases {
        let n = seen.entry(query.as_str()).or_insert(0);
        *n += 1;
        let name = if *n == 1 && !used.contains(query) {
            query.clone()
        } else {
            let mut k = (*n).max(2);
            loop {
                let candidate = format!("{query}_{k}");
                if !own_names.contains(candidate.as_str()) && !used.contains(&candidate) {
                    break candidate;
                }
                k += 1;
            }
        };
        used.insert(name.clone());
        out.push(ColumnName {
            display: display.clone(),
            query: name,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn names(values: &[&str], header: bool) -> Vec<(String, String)> {
        let raw: Vec<Vec<u8>> = values.iter().map(|v| v.as_bytes().to_vec()).collect();
        column_names(&raw, header, Encoding::Utf8)
            .into_iter()
            .map(|c| (c.display, c.query))
            .collect()
    }

    fn pair(d: &str, q: &str) -> (String, String) {
        (d.to_string(), q.to_string())
    }

    #[test]
    fn summary_formats() {
        assert_eq!(
            Dialect::default().summary(),
            "delim ,  quote \"  utf-8  header"
        );
        let d = Dialect {
            delimiter: b'\t',
            quote: None,
            header: false,
            encoding: Encoding::Utf16Le,
            ..Dialect::default()
        };
        assert_eq!(d.summary(), "delim tab  quote none  utf-16le  no header");
        let d = Dialect {
            delimiter: b' ',
            quote: Some(b'\''),
            encoding: Encoding::Utf8Bom,
            ..Dialect::default()
        };
        assert_eq!(d.summary(), "delim space  quote '  utf-8 bom  header");
        let d = Dialect {
            delimiter: 0x1f,
            escape: EscapeStyle::Backslash,
            encoding: Encoding::Windows1252,
            comment: Some(b'#'),
            ..Dialect::default()
        };
        assert_eq!(
            d.summary(),
            "delim 0x1f  quote \"  escape \\  windows-1252  header  comment #"
        );
        let d = Dialect {
            encoding: Encoding::Utf16Be,
            ..Dialect::default()
        };
        assert!(d.summary().contains("utf-16be"));
    }

    #[test]
    fn column_names_headerless() {
        assert_eq!(
            names(&["1", "2", "3"], false),
            [
                pair("col1", "col1"),
                pair("col2", "col2"),
                pair("col3", "col3")
            ]
        );
    }

    #[test]
    fn column_names_empty_and_trimmed() {
        assert_eq!(
            names(&[" id ", "", "  ", "name"], true),
            [
                pair(" id ", "id"),
                pair("col2", "col2"),
                pair("col3", "col3"),
                pair("name", "name")
            ]
        );
    }

    #[test]
    fn column_names_duplicates() {
        assert_eq!(
            names(&["name", "name", "x", "name"], true),
            [
                pair("name", "name"),
                pair("name", "name_2"),
                pair("x", "x"),
                pair("name", "name_3")
            ]
        );
        assert_eq!(
            names(&["a", "a", "a_2"], true),
            [pair("a", "a"), pair("a", "a_3"), pair("a_2", "a_2")]
        );
        // Duplicates after trimming.
        assert_eq!(
            names(&["a", " a"], true),
            [pair("a", "a"), pair(" a", "a_2")]
        );
    }

    #[test]
    fn column_names_decode_windows_1252() {
        let raw = vec![b"caf\xE9".to_vec()];
        assert_eq!(
            column_names(&raw, true, Encoding::Windows1252)[0].display,
            "café"
        );
    }

    #[test]
    fn consistency_scores() {
        assert_eq!(consistency_score(&[2, 2, 2, 2]), 1.0);
        assert_eq!(consistency_score(&[2, 2, 2, 3]), 0.75);
        assert_eq!(consistency_score(&[0, 0, 1]), 0.0);
        assert_eq!(consistency_score(&[3]), 1.0);
        assert_eq!(consistency_score(&[0]), 0.0);
        assert_eq!(consistency_score(&[]), 0.0);
        // Tie in frequency: the larger count is the mode.
        assert_eq!(consistency_score(&[1, 2]), 0.5);
    }

    #[test]
    fn delimiter_tie_break_order() {
        // Every candidate perfectly consistent: `,` wins.
        let text = b"a,b\tc|d;e:f g\nh,i\tj|k;l:m n\n";
        let scores = delimiter_scores(text, Some(b'"'), None, true);
        assert!(scores.iter().all(|&s| s == 1.0), "{scores:?}");
        assert_eq!(best_delimiter(&scores), b',');
        let s = |t: &[u8]| best_delimiter(&delimiter_scores(t, Some(b'"'), None, true));
        assert_eq!(s(b"a\tb|c\nd\te|f\n"), b'\t');
        assert_eq!(s(b"a|b;c\nd|e;f\n"), b'|');
        assert_eq!(s(b"a;b:c\nd;e:f\n"), b';');
        assert_eq!(s(b"a:b c\nd:e f\n"), b':');
        assert_eq!(best_delimiter(&[0.0; 6]), b',');
    }

    #[test]
    fn delimiter_prefers_consistency() {
        let text = b"a,b;c\nd;e\nf,g,h;i\n";
        let scores = delimiter_scores(text, Some(b'"'), None, true);
        assert_eq!(scores[3], 1.0);
        assert!(scores[0] < 1.0);
        assert_eq!(best_delimiter(&scores), b';');
    }

    #[test]
    fn delimiter_respects_quotes() {
        // The quoted field hides two commas and a newline. `;` is consistent.
        let text = b"a;\"x,y,\nz\"\nb;c\nd;e\n";
        let scores = delimiter_scores(text, Some(b'"'), None, true);
        assert_eq!(scores[3], 1.0);
        assert_eq!(scores[0], 0.0);
    }

    #[test]
    fn delimiter_skips_comments_and_blank_lines() {
        let text = b"# a comment; with: stuff\n\na,b\nc,d\n";
        let scores = delimiter_scores(text, Some(b'"'), Some(b'#'), true);
        assert_eq!(scores[0], 1.0);
        assert_eq!(scores[3], 0.0);
    }

    #[test]
    fn delimiter_ignores_cut_last_record() {
        let text = b"a,b\nc,d\ne";
        assert_eq!(delimiter_scores(text, None, None, false)[0], 1.0);
        assert!(delimiter_scores(text, None, None, true)[0] < 1.0);
    }

    #[test]
    fn quote_counting() {
        assert_eq!(quote_counts(b"\"a\",'b'\n", b','), (2, 2));
        assert_eq!(quote_counts(b"'a','b'\n'c',d\n", b','), (0, 6));
        assert_eq!(quote_counts(b"don't,x\n", b','), (0, 0));
    }

    #[test]
    fn single_quote_wins_and_reruns_delimiter() {
        let text = b"'a;b'|c\n'd;e'|f\n'g;h'|i\n";
        let r = sniff(text, DEFAULT_SAMPLE_BYTES, &DialectOverrides::default());
        assert_eq!(r.dialect.quote, Some(b'\''));
        assert_eq!(r.dialect.delimiter, b'|');
    }

    #[test]
    fn escape_detection() {
        assert_eq!(
            detect_escape(b"\"a\\\"b\",c\n", b'"', b','),
            EscapeStyle::Backslash
        );
        assert_eq!(
            detect_escape(b"\"a\"\"b\",c\n", b'"', b','),
            EscapeStyle::Doubled
        );
        assert_eq!(
            detect_escape(b"\"\",\"a\\\"b\"\n", b'"', b','),
            EscapeStyle::Backslash
        );
        assert_eq!(
            detect_escape(b"plain,text\n", b'"', b','),
            EscapeStyle::Doubled
        );
    }

    #[test]
    fn header_detection_rules() {
        let rows = |t: &[&[&str]]| -> Vec<Vec<Vec<u8>>> {
            t.iter()
                .map(|r| r.iter().map(|v| v.as_bytes().to_vec()).collect())
                .collect()
        };
        let h = |t: &[&[&str]]| detect_header(&rows(t), Encoding::Utf8);
        // (a) type signature.
        assert!(h(&[&["price", "x"], &["1.5", "x"], &["2", "x"]]));
        assert!(h(&[&["when!", "x"], &["2024-01-02", "x"]]));
        // A numeric first row: no header.
        assert!(!h(&[&["1", "2"], &["3", "4"]]));
        // (b) identifiers that do not repeat.
        assert!(h(&[&["name", "city"], &["bob", "paris"]]));
        // Identifiers that repeat in every column: data.
        assert!(!h(&[&["a", "b"], &["a", "b"]]));
        // Not identifier-like and no type change.
        assert!(!h(&[&["hello world!", "x y"], &["a", "b"]]));
        // Not unique.
        assert!(!h(&[&["a", "a"], &["b", "c"]]));
        assert!(!h(&[]));
    }

    #[test]
    fn overrides_win() {
        let text = b"a,b\n1,2\n3,4\n";
        let o = DialectOverrides {
            delimiter: Some(b';'),
            quote: Some(Some(b'\'')),
            escape: Some(EscapeStyle::Backslash),
            header: Some(false),
            encoding: Some(Encoding::Windows1252),
            comment: Some(b'#'),
        };
        let r = sniff(text, DEFAULT_SAMPLE_BYTES, &o);
        assert_eq!(r.dialect.delimiter, b';');
        assert_eq!(r.dialect.quote, Some(b'\''));
        assert_eq!(r.dialect.escape, EscapeStyle::Backslash);
        assert!(!r.dialect.header);
        assert_eq!(r.dialect.encoding, Encoding::Windows1252);
        assert_eq!(r.dialect.comment, Some(b'#'));
        // Pure detection is kept separately.
        assert_eq!(r.detected.delimiter, b',');
        assert_eq!(r.detected.quote, None);
        assert!(r.detected.header);
        assert_eq!(r.detected.encoding, Encoding::Utf8);
        assert_eq!(r.detected.comment, None);
        assert_eq!(o.apply(r.detected), r.dialect);
    }

    #[test]
    fn forced_quote_is_used_for_splitting_and_newlines() {
        let text = b"a|b\n'x\ny'|2\n3|4\n";
        let o = DialectOverrides {
            quote: Some(Some(b'\'')),
            ..Default::default()
        };
        let r = sniff(text, DEFAULT_SAMPLE_BYTES, &o);
        assert_eq!(r.dialect.delimiter, b'|');
        assert!(r.quoted_newlines);
        let o = DialectOverrides {
            quote: Some(None),
            delimiter: Some(b'|'),
            ..Default::default()
        };
        assert!(!sniff(text, DEFAULT_SAMPLE_BYTES, &o).quoted_newlines);
    }

    #[test]
    fn forced_delimiter_drives_header_detection() {
        // Split on `;`, row 1 is `a,b` / `c` over numbers.
        let text = b"a,b;c\n1,5;2\n3,5;4\n";
        let o = DialectOverrides {
            delimiter: Some(b';'),
            ..Default::default()
        };
        let r = sniff(text, DEFAULT_SAMPLE_BYTES, &o);
        assert_eq!(r.dialect.delimiter, b';');
        assert!(r.dialect.header);
    }

    #[test]
    fn forced_encoding_skips_detection() {
        let o = DialectOverrides {
            encoding: Some(Encoding::Utf8),
            ..Default::default()
        };
        let r = sniff(b"caf\xE9,x\n1,2\n", DEFAULT_SAMPLE_BYTES, &o);
        assert_eq!(r.dialect.encoding, Encoding::Utf8);
        assert_eq!(r.detected.encoding, Encoding::Windows1252);
    }

    #[test]
    fn sample_extends_to_end_of_line() {
        let mut text = Vec::new();
        for i in 0..100 {
            text.extend_from_slice(format!("{i},value{i}\n").as_bytes());
        }
        let (s, cut) = sample_window(&text, 10, None);
        assert!(!cut);
        assert_eq!(*s.last().unwrap(), b'\n');
        let line = vec![b'x'; MAX_SAMPLE_EXTENSION + 100];
        let (s, cut) = sample_window(&line, 10, None);
        assert!(cut);
        assert_eq!(s.len(), 10 + MAX_SAMPLE_EXTENSION);
    }

    #[test]
    fn utf8_validity() {
        assert_eq!(detect_utf8("café".as_bytes()), Encoding::Utf8);
        // Cut in the middle of `é`.
        assert_eq!(detect_utf8(&"café".as_bytes()[..4]), Encoding::Utf8);
        assert_eq!(detect_utf8(b"caf\xE9,x"), Encoding::Windows1252);
    }
}
