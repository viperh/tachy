//! Query parser: tokens → AST, with error positions (spec §9.1).
//!
//! Recursive descent over the grammar:
//!
//! ```text
//! expr     := or_expr
//! or_expr  := and_expr ( ("||"|"or") and_expr )*
//! and_expr := not_expr ( ("&&"|"and") not_expr )*
//! not_expr := ("!"|"not") not_expr | cmp
//! cmp      := operand ( cmp_op operand )?
//!           | operand "in" "[" literal ("," literal)* "]"
//!           | operand "is" "null" | operand "is" "not" "null"
//! cmp_op   := "==" | "!=" | "<" | "<=" | ">" | ">=" | "~" | "contains" | "starts" | "ends"
//! operand  := column | literal | "(" expr ")"
//! literal  := string | number | "true" | "false" | "null"
//! ```
//!
//! Precedence, loosest first: `||`, `&&`, `!`, comparison. `!` applies to a
//! whole comparison (`!a == 1` is `!(a == 1)`), as the grammar says.
//! Comparisons do not chain: `a < b < c` is an error at the second operator.

use super::{
    QueryError,
    ast::{CmpOp, ColumnRef, Expr, Literal, Operand, Span},
    lexer::{Keyword, Op, Token, TokenKind, lex},
};

/// Parses a filter expression.
pub fn parse(input: &str) -> Result<Expr, QueryError> {
    let tokens = lex(input)?;
    if tokens.is_empty() {
        return Err(QueryError::new("empty filter", 0..0));
    }
    let mut p = Parser {
        tokens: &tokens,
        pos: 0,
        len: input.len(),
        last_end: 0,
    };
    let expr = p.or_expr()?;
    if let Some(t) = p.peek() {
        let span = t.span.start..input.len();
        return Err(QueryError::new(
            format!("unexpected input after the expression: {}", describe(t)),
            span,
        ));
    }
    Ok(expr)
}

struct Parser<'t> {
    tokens: &'t [Token],
    pos: usize,
    len: usize,
    /// End of the last consumed token.
    last_end: usize,
}

fn describe(t: &Token) -> String {
    match &t.kind {
        TokenKind::Ident(n) => format!("column {n}"),
        TokenKind::QuotedIdent(n) => format!("column `{n}`"),
        TokenKind::ColIndex(n) => format!("${n}"),
        TokenKind::Str { .. } => "a string".to_owned(),
        TokenKind::Number { text, .. } => format!("number {text}"),
        TokenKind::Op(op) => format!("'{}'", op.as_str()),
        TokenKind::Keyword(k) => format!("'{}'", k.as_str()),
        TokenKind::Error(m) => m.clone(),
    }
}

fn cmp_op(kind: &TokenKind) -> Option<CmpOp> {
    Some(match kind {
        TokenKind::Op(Op::Eq) => CmpOp::Eq,
        TokenKind::Op(Op::Ne) => CmpOp::Ne,
        TokenKind::Op(Op::Lt) => CmpOp::Lt,
        TokenKind::Op(Op::Le) => CmpOp::Le,
        TokenKind::Op(Op::Gt) => CmpOp::Gt,
        TokenKind::Op(Op::Ge) => CmpOp::Ge,
        TokenKind::Op(Op::Tilde) => CmpOp::Match,
        TokenKind::Keyword(Keyword::Contains) => CmpOp::Contains,
        TokenKind::Keyword(Keyword::Starts) => CmpOp::Starts,
        TokenKind::Keyword(Keyword::Ends) => CmpOp::Ends,
        _ => return None,
    })
}

fn is_comparison_start(kind: &TokenKind) -> bool {
    cmp_op(kind).is_some() || matches!(kind, TokenKind::Keyword(Keyword::In | Keyword::Is))
}

fn literal_of(t: &Token) -> Option<Literal> {
    let span = t.span.clone();
    Some(match &t.kind {
        TokenKind::Str { value, ci, .. } => Literal::Str {
            value: value.clone(),
            ci: *ci,
            span,
        },
        TokenKind::Number { text, value } => Literal::Num {
            text: text.clone(),
            value: *value,
            span,
        },
        TokenKind::Keyword(Keyword::True) => Literal::Bool(true, span),
        TokenKind::Keyword(Keyword::False) => Literal::Bool(false, span),
        TokenKind::Keyword(Keyword::Null) => Literal::Null(span),
        _ => return None,
    })
}

impl<'t> Parser<'t> {
    fn peek(&self) -> Option<&'t Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&'t Token> {
        let t = self.tokens.get(self.pos)?;
        self.pos += 1;
        self.last_end = t.span.end;
        Some(t)
    }

    fn peek_is(&self, f: impl Fn(&TokenKind) -> bool) -> bool {
        self.peek().is_some_and(|t| f(&t.kind))
    }

    fn eof(&self) -> Span {
        self.len..self.len
    }

    fn or_expr(&mut self) -> Result<Expr, QueryError> {
        let mut terms = vec![self.and_expr()?];
        while self
            .peek_is(|k| matches!(k, TokenKind::Op(Op::OrOr) | TokenKind::Keyword(Keyword::Or)))
        {
            let op = self.bump().expect("peeked");
            if self.peek().is_none() {
                return Err(QueryError::new(
                    format!("expected an expression after {}", describe(op)),
                    self.eof(),
                ));
            }
            terms.push(self.and_expr()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("one term")
        } else {
            Expr::Or(terms)
        })
    }

    fn and_expr(&mut self) -> Result<Expr, QueryError> {
        let mut terms = vec![self.not_expr()?];
        while self.peek_is(|k| {
            matches!(
                k,
                TokenKind::Op(Op::AndAnd) | TokenKind::Keyword(Keyword::And)
            )
        }) {
            let op = self.bump().expect("peeked");
            if self.peek().is_none() {
                return Err(QueryError::new(
                    format!("expected an expression after {}", describe(op)),
                    self.eof(),
                ));
            }
            terms.push(self.not_expr()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("one term")
        } else {
            Expr::And(terms)
        })
    }

    fn not_expr(&mut self) -> Result<Expr, QueryError> {
        if let Some(t) = self.peek()
            && matches!(
                t.kind,
                TokenKind::Op(Op::Bang) | TokenKind::Keyword(Keyword::Not)
            )
        {
            self.bump();
            let inner = self.not_expr()?;
            return Ok(Expr::Not(Box::new(inner), t.span.start..self.last_end));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Expr, QueryError> {
        let start = self.peek().map_or(self.len, |t| t.span.start);
        let lhs = self.operand()?;
        let Some(t) = self.peek() else {
            return Ok(truthy(lhs));
        };
        let expr = if let Some(op) = cmp_op(&t.kind) {
            self.bump();
            let rhs = self.operand()?;
            check_ci(&lhs, op)?;
            if op == CmpOp::Match && !matches!(rhs, Operand::Literal(Literal::Str { .. })) {
                return Err(QueryError::new(
                    "the right side of ~ must be a string (the regex)",
                    rhs.span(),
                ));
            }
            check_ci(&rhs, op)?;
            Expr::Cmp {
                lhs,
                op,
                rhs,
                span: start..self.last_end,
            }
        } else if t.kind == TokenKind::Keyword(Keyword::In) {
            self.bump();
            let list = self.literal_list()?;
            Expr::In {
                operand: lhs,
                list,
                span: start..self.last_end,
            }
        } else if t.kind == TokenKind::Keyword(Keyword::Is) {
            self.bump();
            let negated = self.peek_is(|k| *k == TokenKind::Keyword(Keyword::Not));
            if negated {
                self.bump();
            }
            match self.bump() {
                Some(t) if t.kind == TokenKind::Keyword(Keyword::Null) => {}
                other => {
                    let what = if negated { "is not" } else { "is" };
                    let span = other.map_or(self.eof(), |t| t.span.clone());
                    return Err(QueryError::new(format!("expected null after {what}"), span));
                }
            }
            Expr::IsNull {
                operand: lhs,
                negated,
                span: start..self.last_end,
            }
        } else {
            return Ok(truthy(lhs));
        };
        if let Some(t) = self.peek()
            && is_comparison_start(&t.kind)
        {
            return Err(QueryError::new(
                "comparisons can't be chained; combine them with &&",
                t.span.clone(),
            ));
        }
        Ok(expr)
    }

    fn literal_list(&mut self) -> Result<Vec<Literal>, QueryError> {
        let open = match self.bump() {
            Some(t) if t.kind == TokenKind::Op(Op::LBracket) => t.span.clone(),
            other => {
                let span = other.map_or(self.eof(), |t| t.span.clone());
                return Err(QueryError::new("expected [ after in", span));
            }
        };
        let mut list = Vec::new();
        loop {
            match self.bump() {
                Some(t) => match literal_of(t) {
                    Some(l) => list.push(l),
                    None => {
                        return Err(QueryError::new(
                            format!("expected a literal in the list, found {}", describe(t)),
                            t.span.clone(),
                        ));
                    }
                },
                None => return Err(QueryError::new("unclosed [", open)),
            }
            match self.bump() {
                Some(t) if t.kind == TokenKind::Op(Op::Comma) => {}
                Some(t) if t.kind == TokenKind::Op(Op::RBracket) => return Ok(list),
                Some(t) => {
                    return Err(QueryError::new(
                        format!("expected , or ] but found {}", describe(t)),
                        t.span.clone(),
                    ));
                }
                None => return Err(QueryError::new("unclosed [", open)),
            }
        }
    }

    fn operand(&mut self) -> Result<Operand, QueryError> {
        let Some(t) = self.bump() else {
            return Err(QueryError::new("expected a column or a value", self.eof()));
        };
        if let Some(l) = literal_of(t) {
            return Ok(Operand::Literal(l));
        }
        let span = t.span.clone();
        match &t.kind {
            TokenKind::Ident(name) | TokenKind::QuotedIdent(name) => {
                Ok(Operand::Column(ColumnRef::Name {
                    name: name.clone(),
                    span,
                }))
            }
            TokenKind::ColIndex(n) => Ok(Operand::Column(ColumnRef::Index { n: *n, span })),
            TokenKind::Op(Op::LParen) => {
                if self.peek().is_none() {
                    return Err(QueryError::new("unclosed (", span));
                }
                let inner = self.or_expr()?;
                match self.bump() {
                    Some(t) if t.kind == TokenKind::Op(Op::RParen) => {
                        Ok(Operand::Group(Box::new(inner)))
                    }
                    Some(t) => Err(QueryError::new(
                        format!("expected ) but found {}", describe(t)),
                        t.span.clone(),
                    )),
                    None => Err(QueryError::new("unclosed (", span)),
                }
            }
            TokenKind::Keyword(k) => Err(QueryError::new(
                format!(
                    "'{0}' is a keyword; write `{0}` to use it as a column name",
                    k.as_str()
                ),
                span,
            )),
            _ => Err(QueryError::new(
                format!("expected a column or a value, found {}", describe(t)),
                span,
            )),
        }
    }
}

/// A bare operand. A parenthesised expression on its own is returned as is.
fn truthy(o: Operand) -> Expr {
    match o {
        Operand::Group(e) => *e,
        o => Expr::Truthy(o),
    }
}

fn check_ci(o: &Operand, op: CmpOp) -> Result<(), QueryError> {
    if let Operand::Literal(l) = o
        && l.is_ci()
        && !op.allows_ci()
    {
        let mut message = format!("case-insensitive flag not supported with {op}");
        if op == CmpOp::Match {
            message.push_str("; use (?i) in the regex instead");
        }
        return Err(QueryError::new(message, l.span()));
    }
    Ok(())
}
