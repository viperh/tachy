//! Query evaluation: a typed, compiled predicate over field ranges (spec §9.2).
//!
//! [`compile`](super::compile::compile) turns a [`ResolvedExpr`](super::ResolvedExpr)
//! into a [`Predicate`]. A predicate runs directly on a record's raw bytes and
//! its [`FieldRange`]s (§6.1: values stay `&[u8]`). Literals were parsed into
//! each column's type once, at compile time, so evaluation only parses the
//! fields it needs.
//!
//! # Semantics (§9.2)
//!
//! - A **null** field (per [`NullSet`]) or a **missing** field (short ragged
//!   row) makes every comparison `false`, except `!=`, which is `true`.
//!   `is null` / `is not null` test exactly that.
//! - A field that does not parse as the column's type (**unparseable**)
//!   follows the same rule: `false`, except `!=`.
//! - `contains`, `starts`, `ends` and `~` work on the bytes of every type.
//!
//! # Allocation
//!
//! [`Predicate::eval`] does not allocate once its [`EvalScratch`] buffers
//! have grown to fit: unescaped values and lowercased copies go into the
//! scratch. Each blocking worker owns one scratch.

use std::{cmp::Ordering, collections::HashSet, ops::Range, sync::Arc};

use memchr::memmem::Finder;

use super::ast::CmpOp;
use crate::{
    dialect::Encoding,
    parse::{FieldRange, RecordRanges, decode_field, field_bytes},
    types::{
        ColType, NullSet, Value, parse_bool, parse_date, parse_datetime, parse_f64, parse_i64,
        parse_value,
    },
};

/// Per-worker scratch buffers for [`Predicate::eval`].
#[derive(Debug, Default)]
pub struct EvalScratch {
    a: Vec<u8>,
    b: Vec<u8>,
    lower: Vec<u8>,
    text: String,
}

impl EvalScratch {
    /// Empty buffers; they grow on first use and are then reused.
    pub fn new() -> EvalScratch {
        EvalScratch::default()
    }
}

/// How the UI finds the matching part of a displayed cell (M4-04).
///
/// Needles are the query's literals as written (UTF-8), because cells are
/// matched after decoding.
#[derive(Clone, Debug)]
pub enum HighlightRule {
    /// `contains`: every occurrence of the needle.
    Substring {
        /// The literal.
        needle: String,
        /// The `i` flag.
        ci: bool,
    },
    /// `starts`: the needle at the start of the cell.
    Prefix {
        /// The literal.
        needle: String,
        /// The `i` flag.
        ci: bool,
    },
    /// `ends`: the needle at the end of the cell.
    Suffix {
        /// The literal.
        needle: String,
        /// The `i` flag.
        ci: bool,
    },
    /// `==` on a string column: the whole cell when it equals the needle.
    Whole {
        /// The literal.
        needle: String,
        /// The `i` flag.
        ci: bool,
    },
    /// `~`: every regex match.
    Regex(regex::Regex),
}

impl HighlightRule {
    /// Byte ranges of `cell` to highlight, in order and non-overlapping.
    ///
    /// Case-insensitive rules compare the Unicode lowercase of both sides; a
    /// match is reported only when lowercasing keeps the cell's byte length
    /// (always true for ASCII cells), so offsets stay valid.
    #[allow(clippy::single_range_in_vec_init)]
    pub fn find(&self, cell: &str) -> Vec<Range<usize>> {
        let fold = |s: &str, ci: bool| if ci { s.to_lowercase() } else { s.to_owned() };
        match self {
            HighlightRule::Regex(re) => re.find_iter(cell).map(|m| m.range()).collect(),
            HighlightRule::Substring { needle, ci }
            | HighlightRule::Prefix { needle, ci }
            | HighlightRule::Suffix { needle, ci }
            | HighlightRule::Whole { needle, ci } => {
                if needle.is_empty() {
                    return Vec::new();
                }
                let hay = fold(cell, *ci);
                let needle = fold(needle, *ci);
                if hay.len() != cell.len() {
                    return Vec::new();
                }
                match self {
                    HighlightRule::Substring { .. } => hay
                        .match_indices(needle.as_str())
                        .map(|(i, m)| i..i + m.len())
                        .collect(),
                    HighlightRule::Prefix { .. } if hay.starts_with(&needle) => {
                        vec![0..needle.len()]
                    }
                    HighlightRule::Suffix { .. } if hay.ends_with(&needle) => {
                        vec![hay.len() - needle.len()..hay.len()]
                    }
                    HighlightRule::Whole { .. } if hay == needle => vec![0..hay.len()],
                    _ => Vec::new(),
                }
            }
        }
    }
}

/// A compiled filter. Cheap to clone (`Arc` inside), `Send + Sync`.
#[derive(Clone, Debug)]
pub struct Predicate {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) root: Node,
    pub(crate) ctx: Ctx,
    /// Referenced column indices (into the tab's columns), sorted, unique.
    pub(crate) columns: Vec<usize>,
    /// Referenced field indices (`source_index`), sorted, unique.
    pub(crate) fields: Vec<usize>,
    pub(crate) required_literal: Option<Box<[u8]>>,
    /// `(column index, rule)`.
    pub(crate) highlights: Vec<(usize, HighlightRule)>,
}

/// What evaluation needs to know about the file.
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    pub(crate) quote: Option<u8>,
    pub(crate) backslash: bool,
    pub(crate) nulls: NullSet,
    pub(crate) encoding: Encoding,
}

impl Predicate {
    /// Evaluates the predicate on one record: `rec` holds the record's bytes
    /// (from its first byte), `fields` its field ranges relative to `rec`.
    pub fn eval(&self, rec: &[u8], fields: &[FieldRange], scratch: &mut EvalScratch) -> bool {
        eval_node(&self.inner.root, &self.inner.ctx, rec, fields, scratch)
    }

    /// [`Predicate::eval`] on a record parsed with `RecordParser::parse_at`
    /// over the whole file `bytes`.
    pub fn eval_record(&self, bytes: &[u8], rec: &RecordRanges, scratch: &mut EvalScratch) -> bool {
        let start = (rec.start as usize).min(bytes.len());
        let end = (rec.end as usize).clamp(start, bytes.len());
        self.eval(&bytes[start..end], &rec.fields, scratch)
    }

    /// The referenced columns (indices into the tab's columns), sorted and
    /// unique: for header highlighting (§12.4).
    pub fn columns(&self) -> &[usize] {
        &self.inner.columns
    }

    /// The referenced fields (`ColumnMeta::source_index`), sorted and unique.
    /// A filter job can stop parsing a record after the last one.
    pub fn fields(&self) -> &[usize] {
        &self.inner.fields
    }

    /// A literal that every matching record contains verbatim in its raw
    /// bytes: from a case-sensitive `contains`, `starts` or `==` (on a string
    /// column) with a literal of at least 3 bytes, at the top level or in a
    /// top-level `&&` (the longest one is returned). The filter job (M4-04)
    /// can `memmem` a chunk for it and only parse records around hits.
    /// Encoded in the file's encoding.
    ///
    /// Caveat: a malformed quoted field with bytes after its closing quote
    /// (`"ab"cd`, value `abcd`) can contain the literal in its value but not
    /// in its raw bytes. Literals containing the quote byte (or a backslash
    /// with backslash escapes) are never returned.
    pub fn required_literal(&self) -> Option<&[u8]> {
        self.inner.required_literal.as_deref()
    }

    /// How to highlight matches in a cell of column `col` (M4-04): the first
    /// rule for that column. Only non-negated `contains`, `starts`, `ends`,
    /// `~` and string `==` comparisons produce rules.
    pub fn highlights(&self, col: usize) -> Option<HighlightRule> {
        self.highlight_rules(col).next().cloned()
    }

    /// Every highlight rule for column `col`.
    pub fn highlight_rules(&self, col: usize) -> impl Iterator<Item = &HighlightRule> {
        self.inner
            .highlights
            .iter()
            .filter(move |(c, _)| *c == col)
            .map(|(_, r)| r)
    }
}

// ---------------------------------------------------------------------------
// Compiled tree
// ---------------------------------------------------------------------------

// `Field` (the hot leaf) is kept inline on purpose: boxing it would add a
// pointer chase per evaluated comparison.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Node {
    Const(bool),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    /// `field is null` (`negated`: `is not null`). Missing fields are null.
    IsNull {
        field: usize,
        negated: bool,
    },
    /// A test on one field. `on_null` is the result for a null or missing field.
    Field {
        field: usize,
        test: Test,
        on_null: bool,
    },
    /// Column vs column.
    Pair {
        lhs: usize,
        rhs: usize,
        op: CmpOp,
        mode: PairMode,
    },
    /// `(expr) == (expr)` (`true` in the last slot: `!=`).
    Same(Box<Node>, Box<Node>, bool),
}

/// How column-vs-column comparisons compare.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PairMode {
    Bytes,
    Typed(ColType),
}

/// A case-insensitive literal.
#[derive(Debug)]
pub(crate) struct CiLit {
    /// The literal is ASCII: ASCII folding is exact for ASCII fields.
    pub(crate) ascii: bool,
    /// ASCII-lowercased literal bytes (meaningful when `ascii`).
    pub(crate) ascii_lower: Box<[u8]>,
    pub(crate) finder: Finder<'static>,
    /// Unicode lowercase of the literal.
    pub(crate) unicode_lower: String,
}

impl CiLit {
    pub(crate) fn new(text: &str) -> CiLit {
        let ascii_lower: Box<[u8]> = text.as_bytes().to_ascii_lowercase().into();
        CiLit {
            ascii: text.is_ascii(),
            finder: Finder::new(&ascii_lower[..]).into_owned(),
            ascii_lower,
            unicode_lower: text.to_lowercase(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Test {
    I64 {
        op: CmpOp,
        rhs: i64,
    },
    /// An `i64` column against a decimal literal: both as `f64`.
    I64AsF64 {
        op: CmpOp,
        rhs: f64,
    },
    F64 {
        op: CmpOp,
        rhs: f64,
    },
    Date {
        op: CmpOp,
        rhs: i32,
    },
    DateTime {
        op: CmpOp,
        rhs: i64,
    },
    Bool {
        op: CmpOp,
        rhs: bool,
    },
    Bytes {
        op: CmpOp,
        rhs: Box<[u8]>,
    },
    /// `==` (`negate`: `!=`) with the `i` flag.
    EqCi {
        lit: CiLit,
        negate: bool,
    },
    Contains(Finder<'static>),
    ContainsCi(CiLit),
    Starts(Box<[u8]>),
    StartsCi(CiLit),
    Ends(Box<[u8]>),
    EndsCi(CiLit),
    /// `literal contains|starts|ends field`.
    Rev {
        op: CmpOp,
        lit: Box<[u8]>,
        ci: Option<CiLit>,
    },
    /// `~`. `decode`: match the decoded (UTF-8) value, for non-UTF-8 files.
    Regex {
        re: regex::bytes::Regex,
        decode: bool,
    },
    In(InSet),
}

#[derive(Debug)]
pub(crate) enum ByteSet {
    Small(Vec<Box<[u8]>>),
    Hash(HashSet<Box<[u8]>>),
}

impl ByteSet {
    /// A linear list up to 8 items, a hash set above.
    pub(crate) fn new(items: Vec<Box<[u8]>>) -> ByteSet {
        if items.len() <= 8 {
            ByteSet::Small(items)
        } else {
            ByteSet::Hash(items.into_iter().collect())
        }
    }

    fn contains(&self, v: &[u8]) -> bool {
        match self {
            ByteSet::Small(items) => items.iter().any(|i| **i == *v),
            ByteSet::Hash(set) => set.contains(v),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            ByteSet::Small(items) => items.is_empty(),
            ByteSet::Hash(set) => set.is_empty(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum InSet {
    /// String columns: exact items, plus Unicode-lowercased `i` items.
    Bytes {
        exact: ByteSet,
        ci: ByteSet,
    },
    I64(Vec<i64>),
    I64AsF64(Vec<f64>),
    F64(Vec<f64>),
    Date(Vec<i32>),
    DateTime(Vec<i64>),
    Bool {
        t: bool,
        f: bool,
    },
}

/// Whether `op` holds for the ordering of the left value relative to the
/// right one.
#[inline]
pub(crate) fn holds(op: CmpOp, o: Ordering) -> bool {
    match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::Ne => o != Ordering::Equal,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Le => o != Ordering::Greater,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Ge => o != Ordering::Less,
        CmpOp::Match | CmpOp::Contains | CmpOp::Starts | CmpOp::Ends => false,
    }
}

/// `-0.0` → `0.0`, so equal values compare equal under `total_cmp`.
#[inline]
pub(crate) fn norm_f64(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

#[inline]
fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// The value of field `i`, unescaped into `buf` if needed. `None` when the
/// record has no such field.
#[inline]
fn field<'a>(
    ctx: &Ctx,
    rec: &'a [u8],
    fields: &[FieldRange],
    i: usize,
    buf: &'a mut Vec<u8>,
) -> Option<&'a [u8]> {
    let f = fields.get(i)?;
    let raw = rec.get(f.raw.start as usize..f.raw.end as usize)?;
    Some(match ctx.quote {
        Some(q) if f.quoted => field_bytes(raw, f, q, ctx.backslash, buf),
        _ => raw,
    })
}

/// Unicode lowercase of `v` (decoded with `enc`) into `out`.
pub(crate) fn lower_into(v: &[u8], enc: Encoding, out: &mut String) {
    out.clear();
    for c in decode_field(v, enc).chars() {
        out.extend(c.to_lowercase());
    }
}

fn eval_node(
    node: &Node,
    ctx: &Ctx,
    rec: &[u8],
    fields: &[FieldRange],
    s: &mut EvalScratch,
) -> bool {
    match node {
        Node::Const(b) => *b,
        Node::And(v) => v.iter().all(|n| eval_node(n, ctx, rec, fields, s)),
        Node::Or(v) => v.iter().any(|n| eval_node(n, ctx, rec, fields, s)),
        Node::Not(n) => !eval_node(n, ctx, rec, fields, s),
        Node::Same(a, b, negate) => {
            (eval_node(a, ctx, rec, fields, s) == eval_node(b, ctx, rec, fields, s)) != *negate
        }
        Node::IsNull { field: i, negated } => {
            let null = field(ctx, rec, fields, *i, &mut s.a).is_none_or(|v| ctx.nulls.is_null(v));
            null != *negated
        }
        Node::Field {
            field: i,
            test,
            on_null,
        } => {
            let EvalScratch { a, lower, text, .. } = s;
            match field(ctx, rec, fields, *i, a) {
                Some(v) if !ctx.nulls.is_null(v) => test.eval(v, ctx.encoding, lower, text),
                _ => *on_null,
            }
        }
        Node::Pair { lhs, rhs, op, mode } => {
            let EvalScratch { a, b, .. } = s;
            let on_null = *op == CmpOp::Ne;
            let (Some(x), Some(y)) = (
                field(ctx, rec, fields, *lhs, a),
                field(ctx, rec, fields, *rhs, b),
            ) else {
                return on_null;
            };
            if ctx.nulls.is_null(x) || ctx.nulls.is_null(y) {
                return on_null;
            }
            eval_pair(x, y, *op, *mode, &ctx.nulls)
        }
    }
}

/// Column vs column on two non-null values.
fn eval_pair(x: &[u8], y: &[u8], op: CmpOp, mode: PairMode, nulls: &NullSet) -> bool {
    match op {
        CmpOp::Contains => memchr::memmem::find(x, y).is_some(),
        CmpOp::Starts => x.starts_with(y),
        CmpOp::Ends => x.ends_with(y),
        CmpOp::Match => false,
        _ => match mode {
            PairMode::Bytes => holds(op, x.cmp(y)),
            PairMode::Typed(t) => {
                match cmp_values(&parse_value(t, x, nulls), &parse_value(t, y, nulls)) {
                    Some(o) => holds(op, o),
                    None => op == CmpOp::Ne,
                }
            }
        },
    }
}

/// Orders two values of the same type; `None` when either is null or invalid.
fn cmp_values(a: &Value, b: &Value) -> Option<Ordering> {
    Some(match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::I64(x), Value::I64(y)) => x.cmp(y),
        (Value::F64(x), Value::F64(y)) => cmp_f64(*x, *y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        _ => return None,
    })
}

impl Test {
    /// Evaluates on a non-null value.
    #[inline]
    fn eval(&self, v: &[u8], enc: Encoding, lower: &mut Vec<u8>, text: &mut String) -> bool {
        match self {
            Test::I64 { op, rhs } => match parse_i64(v) {
                Some(x) => holds(*op, x.cmp(rhs)),
                None => *op == CmpOp::Ne,
            },
            Test::I64AsF64 { op, rhs } => match parse_i64(v) {
                Some(x) => holds(*op, cmp_f64(x as f64, *rhs)),
                None => *op == CmpOp::Ne,
            },
            Test::F64 { op, rhs } => match parse_f64(v) {
                Some(x) => holds(*op, cmp_f64(x, *rhs)),
                None => *op == CmpOp::Ne,
            },
            Test::Date { op, rhs } => match parse_date(v) {
                Some(x) => holds(*op, x.cmp(rhs)),
                None => *op == CmpOp::Ne,
            },
            Test::DateTime { op, rhs } => match parse_datetime(v) {
                Some(x) => holds(*op, x.cmp(rhs)),
                None => *op == CmpOp::Ne,
            },
            Test::Bool { op, rhs } => match parse_bool(v) {
                Some(x) => holds(*op, x.cmp(rhs)),
                None => *op == CmpOp::Ne,
            },
            Test::Bytes { op, rhs } => holds(*op, v.cmp(rhs)),
            Test::EqCi { lit, negate } => {
                let eq = if lit.ascii && v.is_ascii() {
                    v.eq_ignore_ascii_case(&lit.ascii_lower)
                } else {
                    lower_into(v, enc, text);
                    *text == lit.unicode_lower
                };
                eq != *negate
            }
            Test::Contains(f) => f.find(v).is_some(),
            Test::ContainsCi(lit) => {
                if lit.ascii && v.is_ascii() {
                    lower.clear();
                    lower.extend(v.iter().map(u8::to_ascii_lowercase));
                    lit.finder.find(lower).is_some()
                } else {
                    lower_into(v, enc, text);
                    text.contains(lit.unicode_lower.as_str())
                }
            }
            Test::Starts(lit) => v.starts_with(lit),
            Test::StartsCi(lit) => {
                if lit.ascii && v.is_ascii() {
                    let n = lit.ascii_lower.len();
                    v.len() >= n && v[..n].eq_ignore_ascii_case(&lit.ascii_lower)
                } else {
                    lower_into(v, enc, text);
                    text.starts_with(lit.unicode_lower.as_str())
                }
            }
            Test::Ends(lit) => v.ends_with(lit),
            Test::EndsCi(lit) => {
                if lit.ascii && v.is_ascii() {
                    let n = lit.ascii_lower.len();
                    v.len() >= n && v[v.len() - n..].eq_ignore_ascii_case(&lit.ascii_lower)
                } else {
                    lower_into(v, enc, text);
                    text.ends_with(lit.unicode_lower.as_str())
                }
            }
            Test::Rev { op, lit, ci } => match ci {
                None => match op {
                    CmpOp::Contains => memchr::memmem::find(lit, v).is_some(),
                    CmpOp::Starts => lit.starts_with(v),
                    _ => lit.ends_with(v),
                },
                Some(ci) => {
                    lower_into(v, enc, text);
                    let hay = ci.unicode_lower.as_str();
                    match op {
                        CmpOp::Contains => hay.contains(text.as_str()),
                        CmpOp::Starts => hay.starts_with(text.as_str()),
                        _ => hay.ends_with(text.as_str()),
                    }
                }
            },
            Test::Regex { re, decode } => {
                if *decode && !v.is_ascii() {
                    text.clear();
                    text.push_str(&decode_field(v, enc));
                    re.is_match(text.as_bytes())
                } else {
                    re.is_match(v)
                }
            }
            Test::In(set) => set.contains(v, enc, text),
        }
    }
}

impl InSet {
    fn contains(&self, v: &[u8], enc: Encoding, text: &mut String) -> bool {
        match self {
            InSet::Bytes { exact, ci } => {
                exact.contains(v)
                    || (!ci.is_empty() && {
                        lower_into(v, enc, text);
                        ci.contains(text.as_bytes())
                    })
            }
            InSet::I64(list) => parse_i64(v).is_some_and(|x| list.binary_search(&x).is_ok()),
            InSet::I64AsF64(list) => parse_i64(v).is_some_and(|x| {
                list.binary_search_by(|y| y.total_cmp(&norm_f64(x as f64)))
                    .is_ok()
            }),
            InSet::F64(list) => parse_f64(v)
                .is_some_and(|x| list.binary_search_by(|y| y.total_cmp(&norm_f64(x))).is_ok()),
            InSet::Date(list) => parse_date(v).is_some_and(|x| list.binary_search(&x).is_ok()),
            InSet::DateTime(list) => {
                parse_datetime(v).is_some_and(|x| list.binary_search(&x).is_ok())
            }
            InSet::Bool { t, f } => parse_bool(v).is_some_and(|x| if x { *t } else { *f }),
        }
    }
}

/// A rough evaluation cost, for ordering `&&` / `||` children (cheap first):
/// null checks < numeric < byte compares < memmem < `in` < regex.
pub(crate) fn cost(node: &Node) -> u32 {
    match node {
        Node::Const(_) => 0,
        Node::IsNull { .. } => 1,
        Node::Field { test, .. } => match test {
            Test::I64 { .. }
            | Test::I64AsF64 { .. }
            | Test::Date { .. }
            | Test::DateTime { .. }
            | Test::Bool { .. } => 2,
            Test::F64 { .. } | Test::Bytes { .. } | Test::Starts(_) | Test::Ends(_) => 3,
            Test::EqCi { .. } | Test::StartsCi(_) | Test::EndsCi(_) => 4,
            Test::Contains(_) | Test::Rev { .. } => 5,
            Test::ContainsCi(_) => 6,
            Test::In(_) => 7,
            Test::Regex { .. } => 20,
        },
        Node::Pair { .. } => 6,
        Node::Not(n) => cost(n),
        Node::And(v) | Node::Or(v) => v.iter().map(cost).sum(),
        Node::Same(a, b, _) => cost(a) + cost(b),
    }
}
