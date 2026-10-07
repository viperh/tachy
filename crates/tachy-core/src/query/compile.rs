//! Typed compilation: [`ResolvedExpr`] → [`Predicate`] (spec §9.2, M4-02).
//!
//! Literals are converted once, here, into the type of the column they are
//! compared with. The rules:
//!
//! | Column type | Literal accepted as |
//! |---|---|
//! | `i64` | an integer (exact `i64`), or a decimal → both sides compared as `f64` |
//! | `f64` | any number |
//! | `date` | a date, or a datetime → its (UTC) date part |
//! | `datetime` | a datetime, or a date → midnight UTC |
//! | `bool` | `true`/`false`, or the M3-01 recogniser (`"yes"`, `1`, …) |
//! | `str` / `enum` | anything, compared as bytes (a number keeps its text) |
//!
//! A string literal is accepted for typed columns when its text parses
//! (`ts >= "2026-03-01"`, `price > "100"`). A literal that does not convert is
//! a compile error spanning the literal.
//!
//! - **Column vs column** (`a < b`) compares with the **left** column's type,
//!   parsing the right value with the same type. When the types differ and
//!   either is `str`/`enum`, both are compared as bytes.
//! - **Literal on the left** (`100 < price`) is flipped (`price > 100`). For
//!   `contains`/`starts`/`ends` the literal is the haystack.
//! - **Literal vs literal** is constant-folded.
//! - `== null` / `!= null` mean `is null` / `is not null`; other operators
//!   with `null` are errors.
//! - `x in [..., null]` also matches null fields.
//! - A parenthesised expression can be compared with `true`, `false` or
//!   another parenthesised expression, with `==` / `!=`.
//! - Non-UTF-8 sources (Windows-1252): literals are transcoded at compile time
//!   so comparisons stay byte-wise; `i` matching and `~` decode the field.
//! - `&&` / `||` children are reordered by a cost estimate (null checks <
//!   numeric < byte compares < memmem < `in` < regex). Evaluation has no side
//!   effects, so this never changes a result.

use std::{borrow::Cow, cmp::Ordering, sync::Arc};

use memchr::memmem::Finder;

use super::{
    QueryError,
    ast::{CmpOp, Literal, ResolvedExpr, ResolvedOperand},
    eval::{
        ByteSet, CiLit, Ctx, HighlightRule, InSet, Inner, Node, PairMode, Predicate, Test, cost,
        holds, norm_f64,
    },
};
use crate::{
    column::ColumnMeta,
    dialect::{Dialect, Encoding, EscapeStyle},
    types::{ColType, NullSet, parse_bool, parse_date, parse_datetime, parse_f64, parse_i64},
};

/// Microseconds per day.
const US_PER_DAY: i64 = 86_400_000_000;

/// Regex size limit (§9.2: compiled once per query).
pub const REGEX_SIZE_LIMIT: usize = 10 << 20;

/// Compiles a resolved query against the tab's columns (their effective
/// types and `source_index`), the file's dialect and null spellings.
pub fn compile(
    expr: &ResolvedExpr,
    cols: &[ColumnMeta],
    dialect: &Dialect,
    nulls: &NullSet,
) -> Result<Predicate, QueryError> {
    let mut c = Compiler {
        cols,
        enc: dialect.encoding,
        columns: Vec::new(),
        highlights: Vec::new(),
    };
    let root = c.expr(expr, true)?;
    let required_literal = c.required_literal(expr, dialect);
    let mut columns = c.columns;
    columns.sort_unstable();
    columns.dedup();
    let mut fields: Vec<usize> = columns.iter().map(|&i| cols[i].source_index).collect();
    fields.sort_unstable();
    fields.dedup();
    Ok(Predicate {
        inner: Arc::new(Inner {
            root,
            ctx: Ctx {
                quote: dialect.quote,
                backslash: dialect.escape == EscapeStyle::Backslash,
                nulls: nulls.clone(),
                encoding: dialect.encoding,
            },
            columns,
            fields,
            required_literal,
            highlights: c.highlights,
        }),
    })
}

struct Compiler<'a> {
    cols: &'a [ColumnMeta],
    enc: Encoding,
    columns: Vec<usize>,
    highlights: Vec<(usize, HighlightRule)>,
}

/// The text of a non-null literal as compared byte-wise.
fn lit_text(l: &Literal) -> Cow<'_, str> {
    match l {
        Literal::Str { value, .. } => Cow::Borrowed(value),
        Literal::Num { text, .. } => Cow::Borrowed(text),
        Literal::Bool(b, _) => Cow::Borrowed(if *b { "true" } else { "false" }),
        Literal::Null(_) => Cow::Borrowed("null"),
    }
}

/// `a op b` → `b op' a`.
fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        other => other,
    }
}

fn is_string(t: ColType) -> bool {
    matches!(t, ColType::Str | ColType::Enum)
}

fn not(n: Node) -> Node {
    match n {
        Node::Const(b) => Node::Const(!b),
        Node::Not(inner) => *inner,
        n => Node::Not(Box::new(n)),
    }
}

/// Builds an `&&` (`and`) or `||` node: folds constants, flattens, orders
/// children by cost.
fn junction(and: bool, children: Vec<Node>) -> Node {
    let mut out = Vec::with_capacity(children.len());
    for c in children {
        match c {
            Node::Const(b) if b == and => {}
            Node::Const(b) => return Node::Const(b),
            Node::And(v) if and => out.extend(v),
            Node::Or(v) if !and => out.extend(v),
            c => out.push(c),
        }
    }
    out.sort_by_key(cost);
    match out.len() {
        0 => Node::Const(and),
        1 => out.pop().expect("one child"),
        _ if and => Node::And(out),
        _ => Node::Or(out),
    }
}

impl Compiler<'_> {
    fn col(&mut self, index: usize) -> (&ColumnMeta, usize) {
        self.columns.push(index);
        let c = &self.cols[index];
        (c, c.source_index)
    }

    /// Encodes literal text in the file's encoding.
    fn encode(&self, text: &str, l: &Literal) -> Result<Box<[u8]>, QueryError> {
        match self.enc {
            Encoding::Windows1252 => {
                let (bytes, _, errors) = encoding_rs::WINDOWS_1252.encode(text);
                if errors {
                    return Err(QueryError::new(
                        format!("{l} can't be represented in windows-1252"),
                        l.span(),
                    ));
                }
                Ok(bytes.into_owned().into_boxed_slice())
            }
            _ => Ok(text.as_bytes().into()),
        }
    }

    fn expr(&mut self, e: &ResolvedExpr, positive: bool) -> Result<Node, QueryError> {
        Ok(match e {
            ResolvedExpr::And(v) => junction(
                true,
                v.iter()
                    .map(|e| self.expr(e, positive))
                    .collect::<Result<_, _>>()?,
            ),
            ResolvedExpr::Or(v) => junction(
                false,
                v.iter()
                    .map(|e| self.expr(e, positive))
                    .collect::<Result<_, _>>()?,
            ),
            ResolvedExpr::Not(e, _) => not(self.expr(e, !positive)?),
            ResolvedExpr::IsNull {
                operand, negated, ..
            } => match operand {
                ResolvedOperand::Column { index, .. } => {
                    let (_, field) = self.col(*index);
                    Node::IsNull {
                        field,
                        negated: *negated,
                    }
                }
                ResolvedOperand::Literal(l) => {
                    Node::Const(matches!(l, Literal::Null(_)) != *negated)
                }
                ResolvedOperand::Group(_) => Node::Const(*negated),
            },
            ResolvedExpr::Truthy(o) => match o {
                ResolvedOperand::Column { index, .. } => {
                    let (c, field) = self.col(*index);
                    if c.ty() != ColType::Bool {
                        return Err(QueryError::new(
                            format!("column {} is not bool", c.name.query),
                            o_span(o),
                        ));
                    }
                    Node::Field {
                        field,
                        test: Test::Bool {
                            op: CmpOp::Eq,
                            rhs: true,
                        },
                        on_null: false,
                    }
                }
                ResolvedOperand::Literal(Literal::Bool(b, _)) => Node::Const(*b),
                ResolvedOperand::Literal(l) => {
                    return Err(QueryError::new(format!("{l} is not a bool"), l.span()));
                }
                ResolvedOperand::Group(e) => self.expr(e, positive)?,
            },
            ResolvedExpr::In { operand, list, .. } => match operand {
                ResolvedOperand::Column { index, .. } => self.in_list(*index, list)?,
                ResolvedOperand::Literal(l) => {
                    let mut hit = false;
                    for item in list {
                        hit |= fold(l, CmpOp::Eq, item)?;
                    }
                    Node::Const(hit)
                }
                ResolvedOperand::Group(e) => {
                    return Err(QueryError::new(
                        "an expression can't be used with in",
                        e_span(e),
                    ));
                }
            },
            ResolvedExpr::Cmp { lhs, op, rhs, span } => self.cmp(lhs, *op, rhs, span, positive)?,
        })
    }

    fn cmp(
        &mut self,
        lhs: &ResolvedOperand,
        op: CmpOp,
        rhs: &ResolvedOperand,
        span: &std::ops::Range<usize>,
        positive: bool,
    ) -> Result<Node, QueryError> {
        use ResolvedOperand as O;
        match (lhs, rhs) {
            (O::Column { index, .. }, O::Literal(l)) => {
                self.col_lit(*index, op, l, false, positive)
            }
            (O::Literal(l), O::Column { index, .. }) => {
                if op == CmpOp::Match {
                    return Err(QueryError::new(
                        "the right side of ~ must be a string (the regex)",
                        o_span(rhs),
                    ));
                }
                self.col_lit(*index, flip(op), l, true, positive)
            }
            (O::Literal(a), O::Literal(b)) => Ok(Node::Const(fold(a, op, b)?)),
            (O::Column { index: a, .. }, O::Column { index: b, .. }) => {
                let (ca, fa) = self.col(*a);
                let ta = ca.ty();
                let (cb, fb) = self.col(*b);
                let tb = cb.ty();
                let mode = if is_string(ta) || is_string(tb) {
                    PairMode::Bytes
                } else {
                    PairMode::Typed(ta)
                };
                Ok(Node::Pair {
                    lhs: fa,
                    rhs: fb,
                    op,
                    mode,
                })
            }
            (O::Group(_), _) | (_, O::Group(_)) => self.group_cmp(lhs, op, rhs, span, positive),
        }
    }

    fn group_cmp(
        &mut self,
        lhs: &ResolvedOperand,
        op: CmpOp,
        rhs: &ResolvedOperand,
        span: &std::ops::Range<usize>,
        positive: bool,
    ) -> Result<Node, QueryError> {
        use ResolvedOperand as O;
        let err = || {
            QueryError::new(
                "a parenthesised expression can only be compared with == or != to true, false \
                 or another parenthesised expression",
                span.clone(),
            )
        };
        let negate = match op {
            CmpOp::Eq => false,
            CmpOp::Ne => true,
            _ => return Err(err()),
        };
        match (lhs, rhs) {
            (O::Group(a), O::Group(b)) => Ok(Node::Same(
                Box::new(self.expr(a, positive)?),
                Box::new(self.expr(b, positive)?),
                negate,
            )),
            (O::Group(e), O::Literal(Literal::Bool(b, _)))
            | (O::Literal(Literal::Bool(b, _)), O::Group(e)) => {
                let wanted = *b != negate;
                let n = self.expr(e, positive == wanted)?;
                Ok(if wanted { n } else { not(n) })
            }
            _ => Err(err()),
        }
    }

    fn type_error(&self, l: &Literal, ty: ColType, col: &ColumnMeta) -> QueryError {
        QueryError::new(
            format!(
                "{l} is not a valid {} for column {}",
                ty.label(),
                col.name.query
            ),
            l.span(),
        )
    }

    /// `column op literal` (`reversed`: it was written `literal op column`).
    fn col_lit(
        &mut self,
        index: usize,
        op: CmpOp,
        l: &Literal,
        reversed: bool,
        positive: bool,
    ) -> Result<Node, QueryError> {
        let (c, field) = self.col(index);
        let c = c.clone();
        let ty = c.ty();
        if let Literal::Null(span) = l {
            return match op {
                CmpOp::Eq | CmpOp::Ne => Ok(Node::IsNull {
                    field,
                    negated: op == CmpOp::Ne,
                }),
                _ => Err(QueryError::new(
                    format!("null can't be used with {op}; use is null / is not null"),
                    span.clone(),
                )),
            };
        }
        let text = lit_text(l);
        let ci = l.is_ci();
        let on_null = op == CmpOp::Ne;
        let test = match op {
            CmpOp::Match => {
                let re = regex::bytes::RegexBuilder::new(&text)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|e| QueryError::new(format!("invalid regex: {e}"), l.span()))?;
                if positive && let Ok(re) = regex::Regex::new(&text) {
                    self.highlights.push((index, HighlightRule::Regex(re)));
                }
                Test::Regex {
                    re,
                    decode: self.enc == Encoding::Windows1252,
                }
            }
            CmpOp::Contains | CmpOp::Starts | CmpOp::Ends => {
                let bytes = self.encode(&text, l)?;
                if reversed {
                    Test::Rev {
                        op,
                        lit: bytes,
                        ci: ci.then(|| CiLit::new(&text)),
                    }
                } else {
                    if positive {
                        let needle = text.clone().into_owned();
                        let rule = match op {
                            CmpOp::Contains => HighlightRule::Substring { needle, ci },
                            CmpOp::Starts => HighlightRule::Prefix { needle, ci },
                            _ => HighlightRule::Suffix { needle, ci },
                        };
                        self.highlights.push((index, rule));
                    }
                    match (op, ci) {
                        (CmpOp::Contains, false) => {
                            Test::Contains(Finder::new(&bytes[..]).into_owned())
                        }
                        (CmpOp::Contains, true) => Test::ContainsCi(CiLit::new(&text)),
                        (CmpOp::Starts, false) => Test::Starts(bytes),
                        (CmpOp::Starts, true) => Test::StartsCi(CiLit::new(&text)),
                        (_, false) => Test::Ends(bytes),
                        (_, true) => Test::EndsCi(CiLit::new(&text)),
                    }
                }
            }
            _ => match ty {
                ColType::Str | ColType::Enum => {
                    if positive && op == CmpOp::Eq {
                        self.highlights.push((
                            index,
                            HighlightRule::Whole {
                                needle: text.clone().into_owned(),
                                ci,
                            },
                        ));
                    }
                    if ci {
                        Test::EqCi {
                            lit: CiLit::new(&text),
                            negate: op == CmpOp::Ne,
                        }
                    } else {
                        Test::Bytes {
                            op,
                            rhs: self.encode(&text, l)?,
                        }
                    }
                }
                ColType::Bool => {
                    let rhs = match l {
                        Literal::Bool(b, _) => *b,
                        _ => {
                            parse_bool(text.as_bytes()).ok_or_else(|| self.type_error(l, ty, &c))?
                        }
                    };
                    Test::Bool { op, rhs }
                }
                ColType::I64 => match self.i64_lit(l, &text, &c)? {
                    Ok(rhs) => Test::I64 { op, rhs },
                    Err(rhs) => Test::I64AsF64 { op, rhs },
                },
                ColType::F64 => Test::F64 {
                    op,
                    rhs: self.f64_lit(l, &text, ty, &c)?,
                },
                ColType::Date => Test::Date {
                    op,
                    rhs: self.date_lit(l, &text, &c)?,
                },
                ColType::DateTime => Test::DateTime {
                    op,
                    rhs: self.datetime_lit(l, &text, &c)?,
                },
            },
        };
        Ok(Node::Field {
            field,
            test,
            on_null,
        })
    }

    /// `Ok(i64)` for an exact integer, `Err(f64)` for a decimal.
    fn i64_lit(
        &self,
        l: &Literal,
        text: &str,
        c: &ColumnMeta,
    ) -> Result<Result<i64, f64>, QueryError> {
        if matches!(l, Literal::Bool(..)) {
            return Err(self.type_error(l, ColType::I64, c));
        }
        if let Some(x) = parse_i64(text.as_bytes()) {
            return Ok(Ok(x));
        }
        match parse_f64(text.as_bytes()) {
            Some(x) => Ok(Err(norm_f64(x))),
            None => Err(self.type_error(l, ColType::I64, c)),
        }
    }

    fn f64_lit(
        &self,
        l: &Literal,
        text: &str,
        ty: ColType,
        c: &ColumnMeta,
    ) -> Result<f64, QueryError> {
        if matches!(l, Literal::Bool(..)) {
            return Err(self.type_error(l, ty, c));
        }
        parse_f64(text.as_bytes())
            .map(norm_f64)
            .ok_or_else(|| self.type_error(l, ty, c))
    }

    fn date_lit(&self, l: &Literal, text: &str, c: &ColumnMeta) -> Result<i32, QueryError> {
        if let Literal::Str { .. } = l {
            if let Some(d) = parse_date(text.as_bytes()) {
                return Ok(d);
            }
            if let Some(t) = parse_datetime(text.as_bytes()) {
                return Ok(t.div_euclid(US_PER_DAY) as i32);
            }
        }
        Err(self.type_error(l, ColType::Date, c))
    }

    fn datetime_lit(&self, l: &Literal, text: &str, c: &ColumnMeta) -> Result<i64, QueryError> {
        if let Literal::Str { .. } = l {
            if let Some(t) = parse_datetime(text.as_bytes()) {
                return Ok(t);
            }
            if let Some(d) = parse_date(text.as_bytes()) {
                return Ok(i64::from(d) * US_PER_DAY);
            }
        }
        Err(self.type_error(l, ColType::DateTime, c))
    }

    fn in_list(&mut self, index: usize, list: &[Literal]) -> Result<Node, QueryError> {
        let (c, field) = self.col(index);
        let c = c.clone();
        let ty = c.ty();
        let has_null = list.iter().any(|l| matches!(l, Literal::Null(_)));
        let items: Vec<&Literal> = list
            .iter()
            .filter(|l| !matches!(l, Literal::Null(_)))
            .collect();
        let set = match ty {
            ColType::Str | ColType::Enum => {
                let mut exact = Vec::new();
                let mut ci = Vec::new();
                for l in items {
                    let text = lit_text(l);
                    if l.is_ci() {
                        ci.push(text.to_lowercase().into_bytes().into_boxed_slice());
                    } else {
                        exact.push(self.encode(&text, l)?);
                    }
                }
                InSet::Bytes {
                    exact: ByteSet::new(exact),
                    ci: ByteSet::new(ci),
                }
            }
            ColType::Bool => {
                let (mut t, mut f) = (false, false);
                for l in items {
                    let b = match l {
                        Literal::Bool(b, _) => *b,
                        _ => parse_bool(lit_text(l).as_bytes())
                            .ok_or_else(|| self.type_error(l, ty, &c))?,
                    };
                    if b {
                        t = true;
                    } else {
                        f = true;
                    }
                }
                InSet::Bool { t, f }
            }
            ColType::I64 => {
                let mut ints = Vec::new();
                let mut floats = Vec::new();
                for l in items {
                    match self.i64_lit(l, &lit_text(l), &c)? {
                        Ok(x) => ints.push(x),
                        Err(x) => floats.push(x),
                    }
                }
                if floats.is_empty() {
                    ints.sort_unstable();
                    InSet::I64(ints)
                } else {
                    floats.extend(ints.iter().map(|&x| norm_f64(x as f64)));
                    floats.sort_by(f64::total_cmp);
                    InSet::I64AsF64(floats)
                }
            }
            ColType::F64 => {
                let mut v = items
                    .into_iter()
                    .map(|l| self.f64_lit(l, &lit_text(l), ty, &c))
                    .collect::<Result<Vec<_>, _>>()?;
                v.sort_by(f64::total_cmp);
                InSet::F64(v)
            }
            ColType::Date => {
                let mut v = items
                    .into_iter()
                    .map(|l| self.date_lit(l, &lit_text(l), &c))
                    .collect::<Result<Vec<_>, _>>()?;
                v.sort_unstable();
                InSet::Date(v)
            }
            ColType::DateTime => {
                let mut v = items
                    .into_iter()
                    .map(|l| self.datetime_lit(l, &lit_text(l), &c))
                    .collect::<Result<Vec<_>, _>>()?;
                v.sort_unstable();
                InSet::DateTime(v)
            }
        };
        Ok(Node::Field {
            field,
            test: Test::In(set),
            on_null: has_null,
        })
    }

    /// See [`Predicate::required_literal`].
    fn required_literal(&self, e: &ResolvedExpr, dialect: &Dialect) -> Option<Box<[u8]>> {
        let terms: &[ResolvedExpr] = match e {
            ResolvedExpr::And(v) => v,
            other => std::slice::from_ref(other),
        };
        let backslash = dialect.escape == EscapeStyle::Backslash;
        terms
            .iter()
            .filter_map(|t| {
                let ResolvedExpr::Cmp {
                    lhs: ResolvedOperand::Column { index, .. },
                    op,
                    rhs:
                        ResolvedOperand::Literal(
                            l @ (Literal::Str { ci: false, .. } | Literal::Num { .. }),
                        ),
                    ..
                } = t
                else {
                    return None;
                };
                let ok = match op {
                    CmpOp::Contains | CmpOp::Starts => true,
                    CmpOp::Eq => is_string(self.cols[*index].ty()),
                    _ => false,
                };
                if !ok {
                    return None;
                }
                let bytes = self.encode(&lit_text(l), l).ok()?;
                let unsafe_byte = |b: &u8| Some(*b) == dialect.quote || (backslash && *b == b'\\');
                (bytes.len() >= 3 && !bytes.iter().any(unsafe_byte)).then_some(bytes)
            })
            .max_by_key(|b| b.len())
    }
}

fn o_span(o: &ResolvedOperand) -> std::ops::Range<usize> {
    match o {
        ResolvedOperand::Column { span, .. } => span.clone(),
        ResolvedOperand::Literal(l) => l.span(),
        ResolvedOperand::Group(e) => e_span(e),
    }
}

fn e_span(e: &ResolvedExpr) -> std::ops::Range<usize> {
    match e {
        ResolvedExpr::Or(v) | ResolvedExpr::And(v) => {
            let start = v.first().map_or(0, |e| e_span(e).start);
            let end = v.last().map_or(0, |e| e_span(e).end);
            start..end
        }
        ResolvedExpr::Not(_, span)
        | ResolvedExpr::Cmp { span, .. }
        | ResolvedExpr::In { span, .. }
        | ResolvedExpr::IsNull { span, .. } => span.clone(),
        ResolvedExpr::Truthy(o) => o_span(o),
    }
}

/// Constant-folds `a op b` for two literals: numbers numerically (exactly
/// when both are integers), booleans as booleans, anything else byte-wise on
/// the text (`i`: Unicode lowercase). `null == null` is true; `null != x` is
/// true; any other comparison with `null` is false.
fn fold(a: &Literal, op: CmpOp, b: &Literal) -> Result<bool, QueryError> {
    if matches!(a, Literal::Null(_)) || matches!(b, Literal::Null(_)) {
        let both = matches!(a, Literal::Null(_)) && matches!(b, Literal::Null(_));
        return Ok(match op {
            CmpOp::Eq => both,
            CmpOp::Ne => !both,
            _ => false,
        });
    }
    let ci = a.is_ci() || b.is_ci();
    let fold_case = |s: Cow<'_, str>| if ci { s.to_lowercase() } else { s.into_owned() };
    let (x, y) = (fold_case(lit_text(a)), fold_case(lit_text(b)));
    Ok(match op {
        CmpOp::Match => regex::RegexBuilder::new(&y)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| QueryError::new(format!("invalid regex: {e}"), b.span()))?
            .is_match(&x),
        CmpOp::Contains => x.contains(y.as_str()),
        CmpOp::Starts => x.starts_with(y.as_str()),
        CmpOp::Ends => x.ends_with(y.as_str()),
        _ => {
            let o = match (a, b) {
                (
                    Literal::Num {
                        text: t1,
                        value: v1,
                        ..
                    },
                    Literal::Num {
                        text: t2,
                        value: v2,
                        ..
                    },
                ) => match (parse_i64(t1.as_bytes()), parse_i64(t2.as_bytes())) {
                    (Some(p), Some(q)) => p.cmp(&q),
                    _ => v1.partial_cmp(v2).unwrap_or(Ordering::Equal),
                },
                (Literal::Bool(p, _), Literal::Bool(q, _)) => p.cmp(q),
                _ => x.as_bytes().cmp(y.as_bytes()),
            };
            holds(op, o)
        }
    })
}
