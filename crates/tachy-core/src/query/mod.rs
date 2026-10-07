//! The filter query language: lexer, parser and evaluator (spec §9).
//!
//! Public API:
//! - [`parse`]: text → [`Expr`], or the first [`QueryError`] with its byte span.
//! - [`resolve`]: [`Expr`] → [`ResolvedExpr`], replacing column names and `$N`
//!   with column indices.
//! - [`compile()`]: [`ResolvedExpr`] → [`Predicate`], typed per column (§9.2);
//!   [`Predicate::eval`] runs it on a record's field ranges.
//! - [`highlight`]: token classes for syntax highlighting, which never fails.
//! - [`Expr`]'s `Display` is a pretty printer whose output parses back to the
//!   same tree.

pub mod ast;
pub mod compile;
pub mod eval;
pub mod lexer;
pub mod parser;

use std::ops::Range;

use thiserror::Error;

pub use ast::{CmpOp, ColumnRef, Expr, Literal, Operand, ResolvedExpr, ResolvedOperand, Span};
pub use compile::compile;
pub use eval::{EvalScratch, HighlightRule, Predicate};
pub use parser::parse;

use crate::column::ColumnName;
use lexer::{TokenKind, lex_lenient};

/// A query error with the byte span to underline.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct QueryError {
    /// What went wrong.
    pub message: String,
    /// Byte range in the query text.
    pub span: Range<usize>,
}

impl QueryError {
    /// A new error.
    pub fn new(message: impl Into<String>, span: Range<usize>) -> QueryError {
        QueryError {
            message: message.into(),
            span,
        }
    }
}

// ---------------------------------------------------------------------------
// Name resolution (§9.2)
// ---------------------------------------------------------------------------

/// Finds a column by its exact query name, then its exact display name. No
/// case folding.
fn lookup(name: &str, columns: &[ColumnName]) -> Option<usize> {
    columns
        .iter()
        .position(|c| c.query == name)
        .or_else(|| columns.iter().position(|c| c.display == name))
}

/// Levenshtein distance over chars.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest query name within Levenshtein distance 2, compared
/// case-insensitively. Ties go to the earlier column.
fn suggest<'c>(name: &str, columns: &'c [ColumnName]) -> Option<&'c str> {
    let lower = name.to_lowercase();
    columns
        .iter()
        .map(|c| {
            (
                levenshtein(&lower, &c.query.to_lowercase()),
                c.query.as_str(),
            )
        })
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n)
}

fn resolve_column(c: ColumnRef, columns: &[ColumnName]) -> Result<ResolvedOperand, QueryError> {
    match c {
        ColumnRef::Name { name, span } => match lookup(&name, columns) {
            Some(index) => Ok(ResolvedOperand::Column { index, span }),
            None => {
                let message = match suggest(&name, columns) {
                    Some(s) => format!("unknown column \"{name}\" — did you mean \"{s}\"?"),
                    None => format!("unknown column \"{name}\""),
                };
                Err(QueryError::new(message, span))
            }
        },
        ColumnRef::Index { n, span } => {
            if n >= 1 && n <= columns.len() {
                Ok(ResolvedOperand::Column { index: n - 1, span })
            } else {
                let count = columns.len();
                let unit = if count == 1 { "column" } else { "columns" };
                Err(QueryError::new(
                    format!("${n} is out of range ({count} {unit})"),
                    span,
                ))
            }
        }
    }
}

fn resolve_operand(o: Operand, columns: &[ColumnName]) -> Result<ResolvedOperand, QueryError> {
    match o {
        Operand::Column(c) => resolve_column(c, columns),
        Operand::Literal(l) => Ok(ResolvedOperand::Literal(l)),
        Operand::Group(e) => Ok(ResolvedOperand::Group(Box::new(resolve(*e, columns)?))),
    }
}

/// Replaces every column reference with its 0-based index in `columns`
/// (§9.2).
///
/// Names match the exact query name first (M1-02 dedup names), then the exact
/// display name; there is no case folding. An unknown name is an error
/// spanning the name, with the closest name (Levenshtein ≤ 2,
/// case-insensitive) suggested. `$N` past the last column is an error
/// spanning it.
pub fn resolve(ast: Expr, columns: &[ColumnName]) -> Result<ResolvedExpr, QueryError> {
    Ok(match ast {
        Expr::Or(v) => ResolvedExpr::Or(
            v.into_iter()
                .map(|e| resolve(e, columns))
                .collect::<Result<_, _>>()?,
        ),
        Expr::And(v) => ResolvedExpr::And(
            v.into_iter()
                .map(|e| resolve(e, columns))
                .collect::<Result<_, _>>()?,
        ),
        Expr::Not(e, span) => ResolvedExpr::Not(Box::new(resolve(*e, columns)?), span),
        Expr::Cmp { lhs, op, rhs, span } => ResolvedExpr::Cmp {
            lhs: resolve_operand(lhs, columns)?,
            op,
            rhs: resolve_operand(rhs, columns)?,
            span,
        },
        Expr::In {
            operand,
            list,
            span,
        } => ResolvedExpr::In {
            operand: resolve_operand(operand, columns)?,
            list,
            span,
        },
        Expr::IsNull {
            operand,
            negated,
            span,
        } => ResolvedExpr::IsNull {
            operand: resolve_operand(operand, columns)?,
            negated,
            span,
        },
        Expr::Truthy(o) => ResolvedExpr::Truthy(resolve_operand(o, columns)?),
    })
}

// ---------------------------------------------------------------------------
// Highlighting (§9.4)
// ---------------------------------------------------------------------------

/// How a span of the filter bar is drawn (§9.4; the UI maps classes to theme
/// colours in M4-04).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenClass {
    /// Operators and punctuation (teal).
    Operator,
    /// Keywords, including `true`, `false` and `null` (teal).
    Keyword,
    /// String literals, unterminated ones included (green).
    String,
    /// Number literals (purple).
    Number,
    /// A known column, or any column when no list is given (default fg).
    Column,
    /// A column not in the list, or `$N` out of range (underlined coral).
    UnknownColumn,
    /// Input that does not lex (underlined coral).
    Error,
}

/// Classifies `input` for syntax highlighting. Never fails and never panics;
/// incomplete input is classified as far as possible (see
/// [`lexer::lex_lenient`]). Spans are byte ranges on char boundaries, in order
/// and non-overlapping; whitespace is not covered.
///
/// With `columns`, names and `$N` that do not resolve are
/// [`TokenClass::UnknownColumn`].
pub fn highlight(input: &str, columns: Option<&[ColumnName]>) -> Vec<(Range<usize>, TokenClass)> {
    lex_lenient(input)
        .into_iter()
        .map(|t| {
            let class = match &t.kind {
                TokenKind::Op(_) => TokenClass::Operator,
                TokenKind::Keyword(_) => TokenClass::Keyword,
                TokenKind::Str { .. } => TokenClass::String,
                TokenKind::Number { .. } => TokenClass::Number,
                TokenKind::Error(_) => TokenClass::Error,
                TokenKind::Ident(name) | TokenKind::QuotedIdent(name) => match columns {
                    Some(cols) if lookup(name, cols).is_none() => TokenClass::UnknownColumn,
                    _ => TokenClass::Column,
                },
                TokenKind::ColIndex(n) => match columns {
                    Some(cols) if *n > cols.len() => TokenClass::UnknownColumn,
                    _ => TokenClass::Column,
                },
            };
            (t.span, class)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("prce", "price"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn suggestion_is_case_insensitive() {
        let cols = ["Price", "country"].map(|n| ColumnName {
            display: n.to_owned(),
            query: n.to_owned(),
        });
        assert_eq!(suggest("PRCE", &cols), Some("Price"));
        assert_eq!(suggest("xyz", &cols), None);
    }
}
