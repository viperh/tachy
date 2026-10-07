//! Query AST (spec §9.1) and its pretty printer.
//!
//! [`Expr`] is what [`parse`](super::parse) returns: column references are
//! still names or `$N` indices. [`resolve`](super::resolve) turns it into a
//! [`ResolvedExpr`] of the same shape with column indices.
//!
//! `Display` prints a canonical form (`&&`, `||`, `!`, minimal parentheses)
//! that parses back to the same tree, spans aside.

use std::{fmt, ops::Range};

use super::lexer::{Keyword, is_ident};

/// A byte range into the query text.
pub type Span = Range<usize>;

/// A boolean expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// `a || b || …`, at least two terms.
    Or(Vec<Expr>),
    /// `a && b && …`, at least two terms.
    And(Vec<Expr>),
    /// `!expr`; the span runs from `!` to the end of the operand.
    Not(Box<Expr>, Span),
    /// `lhs op rhs`.
    Cmp {
        /// Left operand.
        lhs: Operand,
        /// Operator.
        op: CmpOp,
        /// Right operand.
        rhs: Operand,
        /// From the start of `lhs` to the end of `rhs`.
        span: Span,
    },
    /// `operand in [lit, …]`.
    In {
        /// Tested operand.
        operand: Operand,
        /// The literal list (at least one).
        list: Vec<Literal>,
        /// From the operand to the closing `]`.
        span: Span,
    },
    /// `operand is null` / `operand is not null`.
    IsNull {
        /// Tested operand.
        operand: Operand,
        /// `true` for `is not null`.
        negated: bool,
        /// From the operand to `null`.
        span: Span,
    },
    /// A bare operand, e.g. a `bool` column.
    Truthy(Operand),
}

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    /// A column reference.
    Column(ColumnRef),
    /// A literal.
    Literal(Literal),
    /// A parenthesised expression used as a comparison operand, e.g.
    /// `(a > 1) == true`. A parenthesised expression on its own is not
    /// wrapped: `(a || b) && c` parses as `And([Or([a, b]), c])`.
    Group(Box<Expr>),
}

/// A column reference before resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnRef {
    /// By name (identifier or backticked).
    Name {
        /// The name, without backticks.
        name: String,
        /// The source span, including backticks.
        span: Span,
    },
    /// By position: `$1` is the first column.
    Index {
        /// 1-based column number.
        n: usize,
        /// The span of `$N`.
        span: Span,
    },
}

/// A literal value.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// `"..."`, unescaped, with the `i` (case-insensitive) flag.
    Str {
        /// The unescaped text.
        value: String,
        /// Whether the `i` suffix was given.
        ci: bool,
        /// Span including the quotes and flag.
        span: Span,
    },
    /// A number. `text` is kept so `i64` columns compare exactly.
    Num {
        /// The literal as written.
        text: String,
        /// Its `f64` value.
        value: f64,
        /// The span.
        span: Span,
    },
    /// `true` / `false`.
    Bool(bool, Span),
    /// `null`.
    Null(Span),
}

/// A comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `~` (regex match)
    Match,
    /// `contains`
    Contains,
    /// `starts`
    Starts,
    /// `ends`
    Ends,
}

impl CmpOp {
    /// The operator as written in a query.
    pub const fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Match => "~",
            CmpOp::Contains => "contains",
            CmpOp::Starts => "starts",
            CmpOp::Ends => "ends",
        }
    }

    /// Whether a case-insensitive (`"…"i`) string is allowed with this
    /// operator.
    pub const fn allows_ci(self) -> bool {
        matches!(
            self,
            CmpOp::Eq | CmpOp::Ne | CmpOp::Contains | CmpOp::Starts | CmpOp::Ends
        )
    }
}

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Literal {
    /// The literal's span.
    pub fn span(&self) -> Span {
        match self {
            Literal::Str { span, .. } | Literal::Num { span, .. } => span.clone(),
            Literal::Bool(_, span) | Literal::Null(span) => span.clone(),
        }
    }

    /// Whether this is a string with the `i` flag.
    pub fn is_ci(&self) -> bool {
        matches!(self, Literal::Str { ci: true, .. })
    }
}

impl ColumnRef {
    /// The reference's span.
    pub fn span(&self) -> Span {
        match self {
            ColumnRef::Name { span, .. } | ColumnRef::Index { span, .. } => span.clone(),
        }
    }
}

impl Operand {
    /// The operand's span. For a group this excludes the parentheses.
    pub fn span(&self) -> Span {
        match self {
            Operand::Column(c) => c.span(),
            Operand::Literal(l) => l.span(),
            Operand::Group(e) => e.span(),
        }
    }
}

impl Expr {
    /// The expression's span (parentheses around groups excluded).
    pub fn span(&self) -> Span {
        match self {
            Expr::Or(v) | Expr::And(v) => {
                let start = v.first().map_or(0, |e| e.span().start);
                let end = v.last().map_or(0, |e| e.span().end);
                start..end
            }
            Expr::Not(_, span)
            | Expr::Cmp { span, .. }
            | Expr::In { span, .. }
            | Expr::IsNull { span, .. } => span.clone(),
            Expr::Truthy(o) => o.span(),
        }
    }

    /// A copy with every span set to `0..0`, for comparing trees regardless of
    /// formatting.
    pub fn without_spans(&self) -> Expr {
        match self {
            Expr::Or(v) => Expr::Or(v.iter().map(Expr::without_spans).collect()),
            Expr::And(v) => Expr::And(v.iter().map(Expr::without_spans).collect()),
            Expr::Not(e, _) => Expr::Not(Box::new(e.without_spans()), 0..0),
            Expr::Cmp { lhs, op, rhs, .. } => Expr::Cmp {
                lhs: lhs.without_spans(),
                op: *op,
                rhs: rhs.without_spans(),
                span: 0..0,
            },
            Expr::In { operand, list, .. } => Expr::In {
                operand: operand.without_spans(),
                list: list.iter().map(Literal::without_spans).collect(),
                span: 0..0,
            },
            Expr::IsNull {
                operand, negated, ..
            } => Expr::IsNull {
                operand: operand.without_spans(),
                negated: *negated,
                span: 0..0,
            },
            Expr::Truthy(o) => Expr::Truthy(o.without_spans()),
        }
    }
}

impl Operand {
    /// See [`Expr::without_spans`].
    pub fn without_spans(&self) -> Operand {
        match self {
            Operand::Column(ColumnRef::Name { name, .. }) => Operand::Column(ColumnRef::Name {
                name: name.clone(),
                span: 0..0,
            }),
            Operand::Column(ColumnRef::Index { n, .. }) => {
                Operand::Column(ColumnRef::Index { n: *n, span: 0..0 })
            }
            Operand::Literal(l) => Operand::Literal(l.without_spans()),
            Operand::Group(e) => Operand::Group(Box::new(e.without_spans())),
        }
    }
}

impl Literal {
    /// See [`Expr::without_spans`].
    pub fn without_spans(&self) -> Literal {
        match self {
            Literal::Str { value, ci, .. } => Literal::Str {
                value: value.clone(),
                ci: *ci,
                span: 0..0,
            },
            Literal::Num { text, value, .. } => Literal::Num {
                text: text.clone(),
                value: *value,
                span: 0..0,
            },
            Literal::Bool(b, _) => Literal::Bool(*b, 0..0),
            Literal::Null(_) => Literal::Null(0..0),
        }
    }
}

// ---------------------------------------------------------------------------
// Pretty printer
// ---------------------------------------------------------------------------

/// Writes `s` as a string literal body with `\"` and `\\` escapes.
fn write_str_lit(f: &mut fmt::Formatter<'_>, s: &str, ci: bool) -> fmt::Result {
    f.write_str("\"")?;
    for c in s.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            c => write!(f, "{c}")?,
        }
    }
    f.write_str("\"")?;
    if ci {
        f.write_str("i")?;
    }
    Ok(())
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Str { value, ci, .. } => write_str_lit(f, value, *ci),
            Literal::Num { text, .. } => f.write_str(text),
            Literal::Bool(b, _) => write!(f, "{b}"),
            Literal::Null(_) => f.write_str("null"),
        }
    }
}

impl fmt::Display for ColumnRef {
    /// A bare identifier when possible, otherwise backticked.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnRef::Name { name, .. } => {
                if is_ident(name) && Keyword::from_ident(name).is_none() {
                    f.write_str(name)
                } else {
                    write!(f, "`{name}`")
                }
            }
            ColumnRef::Index { n, .. } => write!(f, "${n}"),
        }
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Column(c) => c.fmt(f),
            Operand::Literal(l) => l.fmt(f),
            Operand::Group(e) => write!(f, "({e})"),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Or(v) => {
                for (i, e) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" || ")?;
                    }
                    if matches!(e, Expr::Or(_)) {
                        write!(f, "({e})")?;
                    } else {
                        e.fmt(f)?;
                    }
                }
                Ok(())
            }
            Expr::And(v) => {
                for (i, e) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" && ")?;
                    }
                    if matches!(e, Expr::Or(_) | Expr::And(_)) {
                        write!(f, "({e})")?;
                    } else {
                        e.fmt(f)?;
                    }
                }
                Ok(())
            }
            Expr::Not(e, _) => {
                if matches!(**e, Expr::Or(_) | Expr::And(_)) {
                    write!(f, "!({e})")
                } else {
                    write!(f, "!{e}")
                }
            }
            Expr::Cmp { lhs, op, rhs, .. } => write!(f, "{lhs} {op} {rhs}"),
            Expr::In { operand, list, .. } => {
                write!(f, "{operand} in [")?;
                for (i, l) in list.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    l.fmt(f)?;
                }
                f.write_str("]")
            }
            Expr::IsNull {
                operand, negated, ..
            } => {
                if *negated {
                    write!(f, "{operand} is not null")
                } else {
                    write!(f, "{operand} is null")
                }
            }
            Expr::Truthy(o) => o.fmt(f),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved form
// ---------------------------------------------------------------------------

/// [`Expr`] after name resolution: columns are 0-based indices.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedExpr {
    /// See [`Expr::Or`].
    Or(Vec<ResolvedExpr>),
    /// See [`Expr::And`].
    And(Vec<ResolvedExpr>),
    /// See [`Expr::Not`].
    Not(Box<ResolvedExpr>, Span),
    /// See [`Expr::Cmp`].
    Cmp {
        /// Left operand.
        lhs: ResolvedOperand,
        /// Operator.
        op: CmpOp,
        /// Right operand.
        rhs: ResolvedOperand,
        /// Source span.
        span: Span,
    },
    /// See [`Expr::In`].
    In {
        /// Tested operand.
        operand: ResolvedOperand,
        /// Literal list.
        list: Vec<Literal>,
        /// Source span.
        span: Span,
    },
    /// See [`Expr::IsNull`].
    IsNull {
        /// Tested operand.
        operand: ResolvedOperand,
        /// `true` for `is not null`.
        negated: bool,
        /// Source span.
        span: Span,
    },
    /// See [`Expr::Truthy`].
    Truthy(ResolvedOperand),
}

/// [`Operand`] after name resolution.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedOperand {
    /// A column by 0-based index into the tab's columns.
    Column {
        /// 0-based column index.
        index: usize,
        /// Source span of the reference.
        span: Span,
    },
    /// A literal.
    Literal(Literal),
    /// A parenthesised expression.
    Group(Box<ResolvedExpr>),
}
