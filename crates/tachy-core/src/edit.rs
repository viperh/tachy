//! Column edits: per-column chains of text transforms applied to field values
//! on the fly (`edit <col>: drop 1 | upper`).
//!
//! The file is never written (§1). An [`Edits`] set is attached to a
//! [`Source`](crate::source::Source) with `Source::with_edits`, which shares
//! the mapping, like a dialect change. Every consumer of field values reads
//! through [`Edits::apply`]: display, filter evaluation, search, sort keys,
//! duplicate keys, the profile job and export. A running job keeps the
//! `Source` it started with, so it never sees an edit change mid-scan.
//!
//! # Semantics
//!
//! - Ops run left to right on the **decoded** value (UTF-8, or Windows-1252
//!   decoded to Unicode); the result is encoded back to the source encoding.
//!   For Windows-1252 a character with no Windows-1252 byte becomes `?`.
//! - Lengths and positions count **grapheme clusters**, so `drop 1` removes
//!   one user-perceived character (`é` written as `e` + U+0301 included).
//! - Null values (`settings.null_values`) and missing cells of short rows
//!   are left alone: an edit never turns a null into a value.
//! - Invalid UTF-8 in an edited column is replaced by `�` (unedited columns
//!   keep their raw bytes, §16).
//!
//! # Syntax ([`parse_ops`])
//!
//! `op ("|" op)*`, where `op` is one of:
//!
//! | Op | Effect |
//! |---|---|
//! | `drop N` | remove the first `N` characters |
//! | `chop N` | remove the last `N` characters |
//! | `take N` | keep the first `N` characters |
//! | `slice A:B` | keep characters `A..B`; negative counts from the end, either side may be empty (`slice -3:`) |
//! | `trim [S]`, `ltrim [S]`, `rtrim [S]` | strip whitespace, or the characters of `S`, from both / the left / the right end |
//! | `upper`, `lower`, `title` | change case |
//! | `lpad W [C]`, `rpad W [C]` | pad on the left / right to `W` characters with `C` (default a space) |
//! | `prefix S`, `suffix S` | add text |
//! | `replace S T` | replace every occurrence of `S` by `T` (literal) |
//! | `s/RE/REP/[gi]` | regex replace: first match, or all with `g`; `i` ignores case; `$1`, `${name}` in `REP`. Any punctuation may replace `/` |
//!
//! Strings are a bare word or `"quoted"` (with `\"`, `\\`, `\t`, `\n`).

use std::{fmt, ops::Range, sync::OnceLock};

use regex::{Regex, RegexBuilder};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    dialect::Encoding, parse::decode_field, query::compile::REGEX_SIZE_LIMIT, types::NullSet,
};

/// Which end(s) of a value an op works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The start.
    Left,
    /// The end.
    Right,
    /// Both ends.
    Both,
}

/// Case conversions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// `UPPER`.
    Upper,
    /// `lower`.
    Lower,
    /// `Title Case`: the first letter of each word upper, the rest lower.
    Title,
}

/// One transform. See the module docs for the syntax.
#[derive(Clone, Debug)]
pub enum EditOp {
    /// Keep characters `start..end` (Python slice semantics, `None` = open).
    Slice {
        /// First kept character; negative counts from the end.
        start: Option<i64>,
        /// One past the last kept character; negative counts from the end.
        end: Option<i64>,
        /// How it was written (`drop`, `chop`, `take` or `slice`), for display.
        form: SliceForm,
    },
    /// Strip characters from one or both ends.
    Trim {
        /// Which end(s).
        side: Side,
        /// The characters to strip; `None` = Unicode whitespace.
        chars: Option<String>,
    },
    /// Change case.
    Case(Case),
    /// Pad to a width.
    Pad {
        /// `Left` pads before the value (right-aligns it).
        side: Side,
        /// Target width in characters.
        width: usize,
        /// The fill character (one grapheme).
        fill: String,
    },
    /// Add text before (`Left`) or after (`Right`) the value.
    Affix {
        /// Where.
        side: Side,
        /// The text.
        text: String,
    },
    /// Replace every occurrence of a literal.
    Replace {
        /// The text to find (never empty).
        from: String,
        /// Its replacement.
        to: String,
    },
    /// Regex replace.
    Regex {
        /// The compiled pattern.
        re: Regex,
        /// The pattern as written (without the `i` flag).
        pattern: String,
        /// The replacement (`$1`, `${name}`).
        rep: String,
        /// Replace every match (`g`).
        all: bool,
        /// Case-insensitive (`i`).
        ci: bool,
    },
}

/// How a [`EditOp::Slice`] was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceForm {
    /// `drop N` = `slice N:`.
    Drop,
    /// `chop N` = `slice :-N`.
    Chop,
    /// `take N` = `slice :N`.
    Take,
    /// `slice A:B`.
    Slice,
}

impl EditOp {
    /// Applies the op to `s`.
    pub fn apply(&self, s: &str) -> String {
        match self {
            EditOp::Slice { start, end, .. } => slice(s, *start, *end).to_owned(),
            EditOp::Trim { side, chars } => {
                let t = match chars {
                    None => match side {
                        Side::Left => s.trim_start(),
                        Side::Right => s.trim_end(),
                        Side::Both => s.trim(),
                    },
                    Some(set) => {
                        let f = |c: char| set.contains(c);
                        match side {
                            Side::Left => s.trim_start_matches(f),
                            Side::Right => s.trim_end_matches(f),
                            Side::Both => s.trim_matches(f),
                        }
                    }
                };
                t.to_owned()
            }
            EditOp::Case(Case::Upper) => s.to_uppercase(),
            EditOp::Case(Case::Lower) => s.to_lowercase(),
            EditOp::Case(Case::Title) => title_case(s),
            EditOp::Pad { side, width, fill } => {
                let n = char_count(s);
                if n >= *width {
                    return s.to_owned();
                }
                let pad = fill.repeat(width - n);
                match side {
                    Side::Left => pad + s,
                    _ => s.to_owned() + &pad,
                }
            }
            EditOp::Affix { side, text } => match side {
                Side::Left => format!("{text}{s}"),
                _ => format!("{s}{text}"),
            },
            EditOp::Replace { from, to } => s.replace(from.as_str(), to),
            EditOp::Regex { re, rep, all, .. } => {
                if *all {
                    re.replace_all(s, rep.as_str()).into_owned()
                } else {
                    re.replace(s, rep.as_str()).into_owned()
                }
            }
        }
    }
}

impl fmt::Display for EditOp {
    /// The canonical text, which [`parse_ops`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditOp::Slice { start, end, form } => match (form, start, end) {
                (SliceForm::Drop, Some(n), None) => write!(f, "drop {n}"),
                (SliceForm::Chop, None, Some(n)) => write!(f, "chop {}", -n),
                (SliceForm::Take, None, Some(n)) => write!(f, "take {n}"),
                _ => {
                    f.write_str("slice ")?;
                    if let Some(a) = start {
                        write!(f, "{a}")?;
                    }
                    f.write_str(":")?;
                    if let Some(b) = end {
                        write!(f, "{b}")?;
                    }
                    Ok(())
                }
            },
            EditOp::Trim { side, chars } => {
                f.write_str(match side {
                    Side::Left => "ltrim",
                    Side::Right => "rtrim",
                    Side::Both => "trim",
                })?;
                match chars {
                    Some(c) => write!(f, " {}", Quoted(c)),
                    None => Ok(()),
                }
            }
            EditOp::Case(c) => f.write_str(match c {
                Case::Upper => "upper",
                Case::Lower => "lower",
                Case::Title => "title",
            }),
            EditOp::Pad { side, width, fill } => {
                let name = if *side == Side::Left { "lpad" } else { "rpad" };
                if fill == " " {
                    write!(f, "{name} {width}")
                } else {
                    write!(f, "{name} {width} {}", Quoted(fill))
                }
            }
            EditOp::Affix { side, text } => {
                let name = if *side == Side::Left {
                    "prefix"
                } else {
                    "suffix"
                };
                write!(f, "{name} {}", Quoted(text))
            }
            EditOp::Replace { from, to } => {
                write!(f, "replace {} {}", Quoted(from), Quoted(to))
            }
            EditOp::Regex {
                pattern,
                rep,
                all,
                ci,
                ..
            } => {
                let d = ['/', '#', '|', '!', '@', '%', ',', ';', '~']
                    .into_iter()
                    .find(|d| !pattern.contains(*d) && !rep.contains(*d))
                    .unwrap_or('/');
                write!(f, "s{d}{pattern}{d}{rep}{d}")?;
                if *all {
                    f.write_str("g")?;
                }
                if *ci {
                    f.write_str("i")?;
                }
                Ok(())
            }
        }
    }
}

/// Writes a string argument: bare when that reads back the same, quoted
/// otherwise.
struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        let bare = !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| !c.is_whitespace() && !matches!(c, '|' | '"' | '\\'));
        if bare {
            return f.write_str(s);
        }
        f.write_str("\"")?;
        for c in s.chars() {
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\t' => f.write_str("\\t")?,
                '\n' => f.write_str("\\n")?,
                c => write!(f, "{c}")?,
            }
        }
        f.write_str("\"")
    }
}

/// Number of grapheme clusters.
fn char_count(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.graphemes(true).count()
    }
}

/// `s[start..end]` in grapheme clusters, Python slice semantics.
fn slice(s: &str, start: Option<i64>, end: Option<i64>) -> &str {
    let ascii = s.is_ascii();
    let bounds: Vec<usize> = if ascii {
        Vec::new()
    } else {
        s.grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(s.len()))
            .collect()
    };
    let n = if ascii { s.len() } else { bounds.len() - 1 } as i64;
    let resolve = |i: i64| if i < 0 { (n + i).max(0) } else { i.min(n) } as usize;
    let a = start.map_or(0, resolve);
    let b = end.map_or(n as usize, resolve);
    if a >= b {
        return "";
    }
    if ascii {
        &s[a..b]
    } else {
        &s[bounds[a]..bounds[b]]
    }
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if at_start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            at_start = false;
        } else {
            out.push(c);
            // An apostrophe inside a word (`o'neil` → `O'neil`) keeps it
            // going.
            at_start = c != '\'';
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The edit set
// ---------------------------------------------------------------------------

/// The edits of one source: an op chain per field position (`source_index`).
/// See the module docs.
#[derive(Clone, Debug, Default)]
pub struct Edits {
    enc: Encoding,
    nulls: NullSet,
    /// By field position; an empty chain is no edit.
    cols: Vec<Vec<EditOp>>,
}

impl Edits {
    /// No edits, for a source in `enc` with null spellings `nulls`.
    pub fn new(enc: Encoding, nulls: NullSet) -> Edits {
        Edits {
            enc,
            nulls,
            cols: Vec::new(),
        }
    }

    /// Whether no field is edited.
    pub fn is_empty(&self) -> bool {
        self.cols.iter().all(Vec::is_empty)
    }

    /// Whether `field` has an edit.
    #[inline]
    pub fn is_edited(&self, field: usize) -> bool {
        self.cols.get(field).is_some_and(|c| !c.is_empty())
    }

    /// The op chain of `field` (empty when unedited).
    pub fn ops(&self, field: usize) -> &[EditOp] {
        self.cols.get(field).map_or(&[], Vec::as_slice)
    }

    /// The edited field positions, ascending.
    pub fn fields(&self) -> impl Iterator<Item = usize> + '_ {
        self.cols
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_empty())
            .map(|(i, _)| i)
    }

    /// The chain of `field` as text (`drop 1 | upper`), `None` when unedited.
    pub fn describe(&self, field: usize) -> Option<String> {
        let ops = self.ops(field);
        if ops.is_empty() {
            return None;
        }
        Some(
            ops.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" | "),
        )
    }

    /// Appends `ops` to the chain of `field`.
    pub fn push(&mut self, field: usize, ops: impl IntoIterator<Item = EditOp>) {
        if self.cols.len() <= field {
            self.cols.resize_with(field + 1, Vec::new);
        }
        self.cols[field].extend(ops);
    }

    /// Removes the last op of `field`. `false` when it had none.
    pub fn undo(&mut self, field: usize) -> bool {
        self.cols.get_mut(field).and_then(Vec::pop).is_some()
    }

    /// Removes every op of `field`. `false` when it had none.
    pub fn reset(&mut self, field: usize) -> bool {
        match self.cols.get_mut(field) {
            Some(c) if !c.is_empty() => {
                c.clear();
                true
            }
            _ => false,
        }
    }

    /// Removes every edit.
    pub fn clear(&mut self) {
        self.cols.clear();
    }

    /// Keeps only the edits of fields `< width` (after a reload).
    pub fn truncate(&mut self, width: usize) {
        self.cols.truncate(width);
    }

    /// The value of `field` after its edits: `value` itself when the field
    /// is unedited or the value is null, otherwise the edited bytes, written
    /// to `out`.
    #[inline]
    pub fn apply<'a>(&self, field: usize, value: &'a [u8], out: &'a mut Vec<u8>) -> &'a [u8] {
        match self.cols.get(field) {
            Some(ops) if !ops.is_empty() && !self.nulls.is_null(value) => {
                self.apply_ops(ops, value, out);
                out
            }
            _ => value,
        }
    }

    /// Like [`Edits::apply`], but always writes the result to `out` (for
    /// callers whose `value` borrows a temporary buffer).
    pub fn apply_into(&self, field: usize, value: &[u8], out: &mut Vec<u8>) {
        match self.cols.get(field) {
            Some(ops) if !ops.is_empty() && !self.nulls.is_null(value) => {
                self.apply_ops(ops, value, out);
            }
            _ => {
                out.clear();
                out.extend_from_slice(value);
            }
        }
    }

    /// Like [`Edits::apply`] for an optional value: `None` (a missing cell)
    /// stays `None`.
    #[inline]
    pub fn apply_opt<'a>(
        &self,
        field: usize,
        value: Option<&'a [u8]>,
        out: &'a mut Vec<u8>,
    ) -> Option<&'a [u8]> {
        value.map(|v| self.apply(field, v, out))
    }

    #[cold]
    fn apply_ops(&self, ops: &[EditOp], value: &[u8], out: &mut Vec<u8>) {
        let mut s = decode_field(value, self.enc).into_owned();
        for op in ops {
            s = op.apply(&s);
        }
        out.clear();
        match self.enc {
            Encoding::Windows1252 => out.extend(s.chars().map(encode_1252)),
            _ => out.extend_from_slice(s.as_bytes()),
        }
    }
}

/// The Windows-1252 byte of `c`, or `?`.
fn encode_1252(c: char) -> u8 {
    static TABLE: OnceLock<Vec<(char, u8)>> = OnceLock::new();
    if (c as u32) < 0x80 {
        return c as u8;
    }
    let table = TABLE.get_or_init(|| {
        let mut t: Vec<(char, u8)> = (0x80..=0xFFu8)
            .filter_map(|b| {
                let byte = [b];
                let (s, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&byte);
                let c = s.chars().next()?;
                (c != '\u{FFFD}').then_some((c, b))
            })
            .collect();
        t.sort_unstable();
        t
    });
    table
        .binary_search_by(|(k, _)| k.cmp(&c))
        .map_or(b'?', |i| table[i].1)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A syntax error in an op chain, with its byte span in the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditError {
    /// What went wrong.
    pub message: String,
    /// Where, in bytes of the input.
    pub span: Range<usize>,
}

impl EditError {
    fn new(message: impl Into<String>, span: Range<usize>) -> EditError {
        EditError {
            message: message.into(),
            span,
        }
    }
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The op names, for completion.
pub const OP_NAMES: &[&str] = &[
    "drop", "chop", "take", "slice", "trim", "ltrim", "rtrim", "upper", "lower", "title", "lpad",
    "rpad", "prefix", "suffix", "replace", "s/",
];

/// Parses `op ("|" op)*` (see the module docs). Never empty on success.
pub fn parse_ops(input: &str) -> Result<Vec<EditOp>, EditError> {
    let mut p = Parser { input, pos: 0 };
    let mut ops = Vec::new();
    loop {
        p.skip_ws();
        if p.at_end() {
            let at = input.len()..input.len();
            return Err(EditError::new(
                if ops.is_empty() {
                    "expected an edit, e.g. drop 1"
                } else {
                    "expected an edit after |"
                },
                at,
            ));
        }
        ops.push(p.op()?);
        p.skip_ws();
        match p.peek() {
            None => return Ok(ops),
            Some('|') => p.pos += 1,
            Some(_) => {
                let start = p.pos;
                let word = p.word();
                let end = start + word.len().max(1);
                return Err(EditError::new(
                    format!("expected | or the end, found \"{}\"", &input[start..end]),
                    start..end,
                ));
            }
        }
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn skip_ws(&mut self) {
        let r = self.rest();
        self.pos += r.len() - r.trim_start().len();
    }

    /// The run of characters up to whitespace or `|` (not consumed).
    fn word(&self) -> &str {
        let r = self.rest();
        let end = r
            .find(|c: char| c.is_whitespace() || c == '|')
            .unwrap_or(r.len());
        &r[..end]
    }

    fn op(&mut self) -> Result<EditOp, EditError> {
        let start = self.pos;
        let mut chars = self.rest().chars();
        if chars.next() == Some('s')
            && let Some(d) = chars.next()
            && d.is_ascii_punctuation()
        {
            return self.regex(d);
        }
        let name = self.word().to_owned();
        let span = start..start + name.len();
        self.pos = span.end;
        let op = match name.as_str() {
            "drop" => {
                let n = self.count()?;
                EditOp::Slice {
                    start: Some(n),
                    end: None,
                    form: SliceForm::Drop,
                }
            }
            "chop" => {
                let n = self.count()?;
                EditOp::Slice {
                    start: None,
                    end: Some(-n),
                    form: SliceForm::Chop,
                }
            }
            "take" => {
                let n = self.count()?;
                EditOp::Slice {
                    start: None,
                    end: Some(n),
                    form: SliceForm::Take,
                }
            }
            "slice" => self.slice()?,
            "trim" | "ltrim" | "rtrim" => EditOp::Trim {
                side: match name.as_str() {
                    "ltrim" => Side::Left,
                    "rtrim" => Side::Right,
                    _ => Side::Both,
                },
                chars: self.opt_string()?.filter(|s| !s.is_empty()),
            },
            "upper" => EditOp::Case(Case::Upper),
            "lower" => EditOp::Case(Case::Lower),
            "title" => EditOp::Case(Case::Title),
            "lpad" | "rpad" => {
                let width = self.count()? as usize;
                let fill_at = self.next_arg_start();
                let fill = self.opt_string()?.unwrap_or_else(|| " ".to_owned());
                if fill.graphemes(true).count() != 1 {
                    return Err(EditError::new(
                        "the fill must be one character",
                        fill_at..self.pos,
                    ));
                }
                EditOp::Pad {
                    side: if name == "lpad" {
                        Side::Left
                    } else {
                        Side::Right
                    },
                    width,
                    fill,
                }
            }
            "prefix" | "suffix" => EditOp::Affix {
                side: if name == "prefix" {
                    Side::Left
                } else {
                    Side::Right
                },
                text: self.string("text")?,
            },
            "replace" => {
                let at = self.next_arg_start();
                let from = self.string("the text to replace")?;
                if from.is_empty() {
                    return Err(EditError::new("the text to replace is empty", at..self.pos));
                }
                let to = self.string("the replacement")?;
                EditOp::Replace { from, to }
            }
            "" => return Err(EditError::new("expected an edit", span)),
            other => {
                return Err(EditError::new(
                    format!(
                        "unknown edit \"{other}\" (drop, chop, take, slice, trim, upper, lower, \
                         title, lpad, rpad, prefix, suffix, replace, s/re/rep/)"
                    ),
                    span,
                ));
            }
        };
        Ok(op)
    }

    fn next_arg_start(&mut self) -> usize {
        self.skip_ws();
        self.pos
    }

    /// A non-negative integer argument.
    fn count(&mut self) -> Result<i64, EditError> {
        let start = self.next_arg_start();
        let w = self.word();
        let span = start..start + w.len();
        let n = w.parse::<i64>().ok().filter(|n| *n >= 0).ok_or_else(|| {
            EditError::new(
                if w.is_empty() {
                    "expected a number".to_owned()
                } else {
                    format!("expected a number, found \"{w}\"")
                },
                if w.is_empty() {
                    start..start
                } else {
                    span.clone()
                },
            )
        })?;
        self.pos = span.end;
        Ok(n)
    }

    /// `A:B`, either side optional, each a possibly negative integer.
    fn slice(&mut self) -> Result<EditOp, EditError> {
        let start = self.next_arg_start();
        let w = self.word().to_owned();
        let span = start..start + w.len();
        let err = || EditError::new("expected a range like 1:, :-1 or 2:5", span.clone());
        let (a, b) = w.split_once(':').ok_or_else(err)?;
        let num = |s: &str| -> Result<Option<i64>, EditError> {
            if s.is_empty() {
                Ok(None)
            } else {
                s.parse().map(Some).map_err(|_| err())
            }
        };
        let (start, end) = (num(a)?, num(b)?);
        self.pos = span.end;
        Ok(EditOp::Slice {
            start,
            end,
            form: SliceForm::Slice,
        })
    }

    /// A required string argument.
    fn string(&mut self, what: &str) -> Result<String, EditError> {
        let at = self.next_arg_start();
        self.opt_string()?
            .ok_or_else(|| EditError::new(format!("expected {what}"), at..at))
    }

    /// A bare word or a quoted string; `None` at `|` or the end.
    fn opt_string(&mut self) -> Result<Option<String>, EditError> {
        let start = self.next_arg_start();
        match self.peek() {
            None | Some('|') => Ok(None),
            Some('"') => {
                let mut out = String::new();
                let mut it = self.rest().char_indices().skip(1);
                while let Some((i, c)) = it.next() {
                    match c {
                        '"' => {
                            self.pos += i + 1;
                            return Ok(Some(out));
                        }
                        '\\' => match it.next() {
                            Some((_, 'n')) => out.push('\n'),
                            Some((_, 't')) => out.push('\t'),
                            Some((_, c)) => out.push(c),
                            None => break,
                        },
                        c => out.push(c),
                    }
                }
                Err(EditError::new(
                    "unterminated string",
                    start..self.input.len(),
                ))
            }
            Some(_) => {
                let w = self.word().to_owned();
                self.pos += w.len();
                Ok(Some(w))
            }
        }
    }

    /// `s<d>pattern<d>replacement<d>[flags]`, at `s`.
    fn regex(&mut self, d: char) -> Result<EditOp, EditError> {
        let start = self.pos;
        self.pos += 1 + d.len_utf8();
        let pat_start = self.pos;
        let pattern = self.delimited(d).ok_or_else(|| {
            EditError::new(
                format!("unterminated regex: expected s{d}pattern{d}replacement{d}"),
                start..self.input.len(),
            )
        })?;
        let pat_end = self.pos - d.len_utf8();
        let rep = self.delimited(d).ok_or_else(|| {
            EditError::new(
                format!("unterminated regex: expected s{d}pattern{d}replacement{d}"),
                start..self.input.len(),
            )
        })?;
        let (mut all, mut ci) = (false, false);
        while let Some(c) = self.peek() {
            match c {
                'g' => all = true,
                'i' => ci = true,
                c if c.is_whitespace() || c == '|' => break,
                c => {
                    let at = self.pos;
                    return Err(EditError::new(
                        format!("unknown regex flag '{c}' (g, i)"),
                        at..at + c.len_utf8(),
                    ));
                }
            }
            self.pos += 1;
        }
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(ci)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| {
                let msg = match e {
                    regex::Error::Syntax(s) => s
                        .lines()
                        .last()
                        .unwrap_or("invalid regex")
                        .trim_start_matches("error: ")
                        .to_owned(),
                    other => other.to_string(),
                };
                EditError::new(format!("regex: {msg}"), pat_start..pat_end)
            })?;
        Ok(EditOp::Regex {
            re,
            pattern,
            rep,
            all,
            ci,
        })
    }

    /// Text up to the next unescaped `d`, which is consumed. `\d` stands for
    /// `d`; other backslashes are kept for the regex.
    fn delimited(&mut self, d: char) -> Option<String> {
        let mut out = String::new();
        let mut it = self.rest().char_indices().peekable();
        while let Some((i, c)) = it.next() {
            if c == '\\' && it.peek().is_some_and(|(_, n)| *n == d) {
                it.next();
                out.push(d);
            } else if c == '\\' {
                out.push('\\');
                if let Some((_, n)) = it.next() {
                    out.push(n);
                }
            } else if c == d {
                self.pos += i + d.len_utf8();
                return Some(out);
            } else {
                out.push(c);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chain: &str, v: &str) -> String {
        let ops = parse_ops(chain).unwrap();
        let mut e = Edits::new(Encoding::Utf8, NullSet::none());
        e.push(0, ops);
        let mut out = Vec::new();
        String::from_utf8(e.apply(0, v.as_bytes(), &mut out).to_vec()).unwrap()
    }

    #[test]
    fn slicing() {
        assert_eq!(run("drop 1", "ABC-1001"), "BC-1001");
        assert_eq!(run("drop 10", "abc"), "");
        assert_eq!(run("chop 2", "abcd"), "ab");
        assert_eq!(run("take 2", "abcd"), "ab");
        assert_eq!(run("slice -3:", "abcdef"), "def");
        assert_eq!(run("slice 1:-1", "[x]"), "x");
        assert_eq!(run("slice 3:1", "abcdef"), "");
        // Graphemes, not bytes or code points.
        assert_eq!(run("drop 1", "e\u{301}clair"), "clair");
        assert_eq!(run("chop 1", "naïve🇩🇪"), "naïve");
    }

    #[test]
    fn trim_case_pad_affix() {
        assert_eq!(run("trim", "  a b \t"), "a b");
        assert_eq!(run("ltrim 0", "00120"), "120");
        assert_eq!(run("rtrim \".-\"", "a.b.-."), "a.b");
        assert_eq!(run("upper", "straße"), "STRASSE");
        assert_eq!(run("lower", "ÀB"), "àb");
        assert_eq!(run("title", "o'neil mc-DONALD"), "O'neil Mc-Donald");
        assert_eq!(run("lpad 5 0", "42"), "00042");
        assert_eq!(run("rpad 4", "ab"), "ab  ");
        assert_eq!(run("lpad 2 0", "12345"), "12345");
        assert_eq!(run("prefix \"ID \"", "7"), "ID 7");
        assert_eq!(run("suffix %", "7"), "7%");
        assert_eq!(run("replace , .", "1,5"), "1.5");
    }

    #[test]
    fn regex_replace() {
        assert_eq!(run("s/(\\w+)-(\\d+)/$2-$1/", "abc-12"), "12-abc");
        assert_eq!(run("s/a/x/", "aaa"), "xaa");
        assert_eq!(run("s/a/x/g", "aaa"), "xxx");
        assert_eq!(run("s/A/x/gi", "aAa"), "xxx");
        // Alternation and a custom delimiter.
        assert_eq!(run("s#a|b#-#g", "abc"), "--c");
        assert_eq!(run("s/\\//_/g", "a/b"), "a_b");
    }

    #[test]
    fn chains_run_left_to_right() {
        assert_eq!(run("trim | drop 1 | upper", "  $abc "), "ABC");
        assert_eq!(run("s/x|y/z/g | upper", "xay"), "ZAZ");
    }

    #[test]
    fn display_round_trips() {
        for chain in [
            "drop 1 | chop 2 | take 3 | slice -3: | slice 1:-1 | slice :",
            "trim | ltrim \"0 \" | rtrim x",
            "upper | lower | title",
            "lpad 5 0 | rpad 3",
            "prefix \"a b\" | suffix \"\\\"q\\\"\"",
            "replace \"|\" \"\"",
            "s/a|b/$1/gi | s#/#_#",
        ] {
            let ops = parse_ops(chain).unwrap();
            let mut e = Edits::default();
            e.push(0, ops);
            let text = e.describe(0).unwrap();
            let again = parse_ops(&text).unwrap();
            let mut e2 = Edits::default();
            e2.push(0, again);
            assert_eq!(e2.describe(0).unwrap(), text, "{chain}");
        }
    }

    #[test]
    fn errors_have_spans() {
        let e = parse_ops("drop x").unwrap_err();
        assert_eq!(e.span, 5..6);
        let e = parse_ops("frob").unwrap_err();
        assert_eq!(e.span, 0..4);
        assert!(e.message.contains("unknown edit"));
        let e = parse_ops("upper |").unwrap_err();
        assert_eq!(e.message, "expected an edit after |");
        let e = parse_ops("s/(/x/").unwrap_err();
        assert_eq!(e.span, 2..3);
        let e = parse_ops("s/a/b").unwrap_err();
        assert!(e.message.contains("unterminated"));
        let e = parse_ops("upper lower").unwrap_err();
        assert_eq!(e.span, 6..11);
        let e = parse_ops("lpad 3 ab").unwrap_err();
        assert!(e.message.contains("one character"));
        assert!(parse_ops("").is_err());
    }

    #[test]
    fn nulls_and_unedited_fields_pass_through() {
        let mut e = Edits::new(Encoding::Utf8, NullSet::default());
        e.push(1, parse_ops("prefix x").unwrap());
        let mut out = Vec::new();
        assert_eq!(e.apply(0, b"a", &mut out), b"a");
        assert_eq!(e.apply(1, b"NULL", &mut out), b"NULL");
        assert_eq!(e.apply(1, b"", &mut out), b"");
        assert_eq!(e.apply(1, b"a", &mut out), b"xa");
        assert_eq!(e.apply_opt(1, None, &mut out), None);
        // `apply_into` always fills `out`, edited or not.
        e.apply_into(0, b"raw", &mut out);
        assert_eq!(out, b"raw");
        e.apply_into(1, b"a", &mut out);
        assert_eq!(out, b"xa");
        e.apply_into(1, b"NULL", &mut out);
        assert_eq!(out, b"NULL");
        assert!(e.is_edited(1) && !e.is_edited(0) && !e.is_edited(9));
        assert_eq!(e.fields().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn undo_and_reset() {
        let mut e = Edits::default();
        e.push(2, parse_ops("drop 1 | upper").unwrap());
        assert!(e.undo(2));
        assert_eq!(e.describe(2).as_deref(), Some("drop 1"));
        assert!(e.reset(2));
        assert!(e.is_empty());
        assert!(!e.reset(2));
        assert!(!e.undo(5));
    }

    #[test]
    fn windows_1252_round_trip() {
        let mut e = Edits::new(Encoding::Windows1252, NullSet::none());
        e.push(0, parse_ops("upper | suffix € | suffix 中").unwrap());
        let mut out = Vec::new();
        // "café" in Windows-1252.
        let v = e.apply(0, b"caf\xE9", &mut out);
        assert_eq!(v, b"CAF\xC9\x80?");
    }
}
