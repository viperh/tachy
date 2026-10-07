//! Query lexer: turns filter text into tokens with byte positions (spec §9.1).
//!
//! Every token carries a byte span into the input; the UI converts spans to
//! display columns.
//!
//! Token kinds:
//! - identifiers `[A-Za-z_][A-Za-z0-9_]*`; **keywords are recognised from
//!   identifiers and are case-sensitive, lowercase only**: `contains`,
//!   `starts`, `ends`, `in`, `is`, `not`, `null`, `true`, `false`, `and`, `or`.
//!   `And` or `NULL` are therefore ordinary identifiers. A column whose name
//!   is a keyword must be backticked or referenced by `$N`;
//! - backticked names `` `order id` `` (any characters but a backtick);
//! - column numbers `$1`, `$2`, … (`$0` and a bare `$` are errors);
//! - strings `"..."` with the escapes `\"` and `\\` only, and an optional `i`
//!   flag directly after the closing quote;
//! - numbers `-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?`, keeping their text;
//! - operators `==` `!=` `<` `<=` `>` `>=` `~` `&&` `||` `!` `(` `)` `[` `]` `,`.
//!
//! [`lex`] stops at the first error. [`lex_lenient`] never fails: it is used to
//! highlight incomplete input while the user types.

use super::{QueryError, ast::Span};

/// A token and its byte span.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// What the token is.
    pub kind: TokenKind,
    /// Byte range in the input.
    pub span: Span,
}

/// Token kinds.
#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    /// A bare column name.
    Ident(String),
    /// A backticked column name, without the backticks.
    QuotedIdent(String),
    /// `$N`, 1-based.
    ColIndex(usize),
    /// A string literal.
    Str {
        /// Unescaped contents.
        value: String,
        /// The `i` flag.
        ci: bool,
        /// Lenient mode only: the closing quote is missing and the token runs
        /// to the end of the input.
        unterminated: bool,
    },
    /// A number literal.
    Number {
        /// The text as written.
        text: String,
        /// Its value.
        value: f64,
    },
    /// An operator or punctuation.
    Op(Op),
    /// A keyword.
    Keyword(Keyword),
    /// Lenient mode only: input that does not lex, with the error message.
    Error(String),
}

/// Operators and punctuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
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
    /// `~`
    Tilde,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `!`
    Bang,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
}

impl Op {
    /// The operator as written.
    pub const fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "==",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Tilde => "~",
            Op::AndAnd => "&&",
            Op::OrOr => "||",
            Op::Bang => "!",
            Op::LParen => "(",
            Op::RParen => ")",
            Op::LBracket => "[",
            Op::RBracket => "]",
            Op::Comma => ",",
        }
    }
}

/// Keywords (case-sensitive, lowercase).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyword {
    /// `contains`
    Contains,
    /// `starts`
    Starts,
    /// `ends`
    Ends,
    /// `in`
    In,
    /// `is`
    Is,
    /// `not` (≡ `!`, except in `is not null`)
    Not,
    /// `null`
    Null,
    /// `true`
    True,
    /// `false`
    False,
    /// `and` (≡ `&&`)
    And,
    /// `or` (≡ `||`)
    Or,
}

impl Keyword {
    /// Every keyword.
    pub const ALL: [Keyword; 11] = [
        Keyword::Contains,
        Keyword::Starts,
        Keyword::Ends,
        Keyword::In,
        Keyword::Is,
        Keyword::Not,
        Keyword::Null,
        Keyword::True,
        Keyword::False,
        Keyword::And,
        Keyword::Or,
    ];

    /// The keyword as written.
    pub const fn as_str(self) -> &'static str {
        match self {
            Keyword::Contains => "contains",
            Keyword::Starts => "starts",
            Keyword::Ends => "ends",
            Keyword::In => "in",
            Keyword::Is => "is",
            Keyword::Not => "not",
            Keyword::Null => "null",
            Keyword::True => "true",
            Keyword::False => "false",
            Keyword::And => "and",
            Keyword::Or => "or",
        }
    }

    /// The keyword spelled exactly `s`, if any.
    pub fn from_ident(s: &str) -> Option<Keyword> {
        Keyword::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `s` is a bare identifier (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn is_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && is_ident_start(b[0]) && b[1..].iter().all(|&c| is_ident_continue(c))
}

/// Lexes `input`, stopping at the first error.
pub fn lex(input: &str) -> Result<Vec<Token>, QueryError> {
    let mut lx = Lexer::new(input, false);
    lx.run()?;
    Ok(lx.tokens)
}

/// Lexes `input` without ever failing: bad input becomes
/// [`TokenKind::Error`] tokens and lexing continues after them. An
/// unterminated string becomes a [`TokenKind::Str`] token that runs to the end
/// of the input, flagged `unterminated`. A string with an invalid escape
/// becomes one `Error` token. Spans never overlap and are in input order.
pub fn lex_lenient(input: &str) -> Vec<Token> {
    let mut lx = Lexer::new(input, true);
    // In lenient mode `run` never returns an error.
    let _ = lx.run();
    lx.tokens
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    lenient: bool,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, lenient: bool) -> Lexer<'a> {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            lenient,
            tokens: Vec::new(),
        }
    }

    fn push(&mut self, kind: TokenKind, span: Span) {
        self.tokens.push(Token { kind, span });
    }

    /// Reports an error. In lenient mode, records an `Error` token for
    /// `consume` (which must start at or after the current token start), moves
    /// past it and returns `Ok`.
    fn error(&mut self, message: String, span: Span, consume: Span) -> Result<(), QueryError> {
        if self.lenient {
            self.pos = consume.end.max(self.pos);
            self.push(TokenKind::Error(message), consume);
            Ok(())
        } else {
            Err(QueryError { message, span })
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn run(&mut self) -> Result<(), QueryError> {
        while let Some(c) = self.peek_char() {
            let start = self.pos;
            if c.is_whitespace() {
                self.pos += c.len_utf8();
                continue;
            }
            let b = self.bytes[start];
            let next = self.bytes.get(start + 1).copied();
            let two = |op: Op| (op, 2usize);
            let op = match (b, next) {
                (b'=', Some(b'=')) => Some(two(Op::Eq)),
                (b'!', Some(b'=')) => Some(two(Op::Ne)),
                (b'<', Some(b'=')) => Some(two(Op::Le)),
                (b'>', Some(b'=')) => Some(two(Op::Ge)),
                (b'&', Some(b'&')) => Some(two(Op::AndAnd)),
                (b'|', Some(b'|')) => Some(two(Op::OrOr)),
                (b'<', _) => Some((Op::Lt, 1)),
                (b'>', _) => Some((Op::Gt, 1)),
                (b'~', _) => Some((Op::Tilde, 1)),
                (b'!', _) => Some((Op::Bang, 1)),
                (b'(', _) => Some((Op::LParen, 1)),
                (b')', _) => Some((Op::RParen, 1)),
                (b'[', _) => Some((Op::LBracket, 1)),
                (b']', _) => Some((Op::RBracket, 1)),
                (b',', _) => Some((Op::Comma, 1)),
                _ => None,
            };
            if let Some((op, len)) = op {
                self.pos += len;
                self.push(TokenKind::Op(op), start..start + len);
                continue;
            }
            match b {
                b'"' => self.string()?,
                b'`' => self.quoted_ident()?,
                b'$' => self.col_index()?,
                b'-' | b'0'..=b'9' => self.number()?,
                b if is_ident_start(b) => self.ident(),
                _ => {
                    let end = start + c.len_utf8();
                    let message = match c {
                        '=' => "unexpected '='; use == to compare".to_owned(),
                        '&' => "unexpected '&'; use && (or and)".to_owned(),
                        '|' => "unexpected '|'; use || (or or)".to_owned(),
                        c => format!("unexpected character '{c}'"),
                    };
                    self.error(message, start..end, start..end)?;
                }
            }
        }
        Ok(())
    }

    fn ident(&mut self) {
        let start = self.pos;
        while self.pos < self.bytes.len() && is_ident_continue(self.bytes[self.pos]) {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        let kind = match Keyword::from_ident(text) {
            Some(k) => TokenKind::Keyword(k),
            None => TokenKind::Ident(text.to_owned()),
        };
        self.push(kind, start..self.pos);
    }

    fn quoted_ident(&mut self) -> Result<(), QueryError> {
        let start = self.pos;
        let Some(len) = self.src[start + 1..].find('`') else {
            let end = self.src.len();
            return self.error(
                "unterminated column name; add a closing `".to_owned(),
                start..start + 1,
                start..end,
            );
        };
        let end = start + 1 + len + 1;
        if len == 0 {
            return self.error("empty column name".to_owned(), start..end, start..end);
        }
        self.pos = end;
        let name = self.src[start + 1..end - 1].to_owned();
        self.push(TokenKind::QuotedIdent(name), start..end);
        Ok(())
    }

    fn col_index(&mut self) -> Result<(), QueryError> {
        let start = self.pos;
        let mut end = start + 1;
        while end < self.bytes.len() && self.bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start + 1 {
            return self.error(
                "expected a column number after $ (e.g. $1)".to_owned(),
                start..end,
                start..end,
            );
        }
        match self.src[start + 1..end].parse::<usize>() {
            Ok(0) => self.error(
                "column numbers start at $1".to_owned(),
                start..end,
                start..end,
            ),
            Ok(n) => {
                self.pos = end;
                self.push(TokenKind::ColIndex(n), start..end);
                Ok(())
            }
            Err(_) => self.error(
                "column number is too large".to_owned(),
                start..end,
                start..end,
            ),
        }
    }

    fn digits_from(&self, mut i: usize) -> usize {
        while i < self.bytes.len() && self.bytes[i].is_ascii_digit() {
            i += 1;
        }
        i
    }

    fn number(&mut self) -> Result<(), QueryError> {
        let start = self.pos;
        let mut i = start;
        if self.bytes[i] == b'-' {
            i += 1;
            if !self.bytes.get(i).is_some_and(u8::is_ascii_digit) {
                return self.error(
                    "'-' must be followed by a digit (it is only valid in a number)".to_owned(),
                    start..start + 1,
                    start..start + 1,
                );
            }
        }
        i = self.digits_from(i);
        if self.bytes.get(i) == Some(&b'.') && self.bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
        {
            i = self.digits_from(i + 1);
        }
        if matches!(self.bytes.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(self.bytes.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if self.bytes.get(j).is_some_and(u8::is_ascii_digit) {
                i = self.digits_from(j);
            }
        }
        // A number glued to letters, digits or dots (`12abc`, `1.`, `1e`) is
        // one bad token rather than a number followed by something else.
        if self
            .bytes
            .get(i)
            .is_some_and(|&b| is_ident_continue(b) || b == b'.')
        {
            let mut end = i;
            while end < self.bytes.len()
                && (is_ident_continue(self.bytes[end]) || self.bytes[end] == b'.')
            {
                end += 1;
            }
            let text = &self.src[start..end];
            return self.error(format!("invalid number '{text}'"), start..end, start..end);
        }
        let text = &self.src[start..i];
        // The grammar is a subset of what f64::from_str accepts.
        let value = text.parse::<f64>().unwrap_or(f64::NAN);
        self.pos = i;
        self.push(
            TokenKind::Number {
                text: text.to_owned(),
                value,
            },
            start..i,
        );
        Ok(())
    }

    fn string(&mut self) -> Result<(), QueryError> {
        let start = self.pos;
        let mut i = start + 1;
        let mut value = String::new();
        let mut bad_escape: Option<(String, Span)> = None;
        loop {
            let Some(c) = self.src[i..].chars().next() else {
                // Unterminated.
                if !self.lenient {
                    return Err(QueryError {
                        message: "unterminated string; add a closing \"".to_owned(),
                        span: start..start + 1,
                    });
                }
                let end = self.src.len();
                self.pos = end;
                match bad_escape {
                    Some((msg, _)) => self.push(TokenKind::Error(msg), start..end),
                    None => self.push(
                        TokenKind::Str {
                            value,
                            ci: false,
                            unterminated: true,
                        },
                        start..end,
                    ),
                }
                return Ok(());
            };
            match c {
                '"' => {
                    i += 1;
                    break;
                }
                '\\' => match self.src[i + 1..].chars().next() {
                    Some(e @ ('"' | '\\')) => {
                        value.push(e);
                        i += 2;
                    }
                    // A trailing backslash: the string is unterminated.
                    None => i += 1,
                    Some(e) => {
                        let span = i..i + 1 + e.len_utf8();
                        let msg = format!("invalid escape \\{e}; only \\\" and \\\\ are allowed");
                        if !self.lenient {
                            return Err(QueryError { message: msg, span });
                        }
                        bad_escape.get_or_insert((msg, span));
                        i += 1 + e.len_utf8();
                    }
                },
                c => {
                    value.push(c);
                    i += c.len_utf8();
                }
            }
        }
        // `i` flag: directly after the quote and not the start of a word
        // (`"x"in [...]` is `"x" in [...]`).
        let mut ci = false;
        if self.bytes.get(i) == Some(&b'i')
            && !self.bytes.get(i + 1).is_some_and(|&b| is_ident_continue(b))
        {
            ci = true;
            i += 1;
        }
        self.pos = i;
        match bad_escape {
            Some((msg, _)) => self.push(TokenKind::Error(msg), start..i),
            None => self.push(
                TokenKind::Str {
                    value,
                    ci,
                    unterminated: false,
                },
                start..i,
            ),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(input: &str) -> Vec<TokenKind> {
        lex(input).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn err(input: &str) -> (String, Span) {
        let e = lex(input).unwrap_err();
        (e.message, e.span)
    }

    #[test]
    fn tokens_and_spans() {
        let toks = lex("price >= -1.5e3 && `order id` != \"a\\\"b\"i || $3").unwrap();
        let spans: Vec<Span> = toks.iter().map(|t| t.span.clone()).collect();
        assert_eq!(
            spans,
            vec![
                0..5,
                6..8,
                9..15,
                16..18,
                19..29,
                30..32,
                33..40,
                41..43,
                44..46
            ]
        );
        assert_eq!(toks[0].kind, TokenKind::Ident("price".into()));
        assert_eq!(
            toks[2].kind,
            TokenKind::Number {
                text: "-1.5e3".into(),
                value: -1500.0
            }
        );
        assert_eq!(toks[4].kind, TokenKind::QuotedIdent("order id".into()));
        assert_eq!(
            toks[6].kind,
            TokenKind::Str {
                value: "a\"b".into(),
                ci: true,
                unterminated: false
            }
        );
        assert_eq!(toks[8].kind, TokenKind::ColIndex(3));
    }

    #[test]
    fn keywords_are_lowercase_only() {
        assert_eq!(
            kinds("and And AND not_x"),
            vec![
                TokenKind::Keyword(Keyword::And),
                TokenKind::Ident("And".into()),
                TokenKind::Ident("AND".into()),
                TokenKind::Ident("not_x".into()),
            ]
        );
    }

    #[test]
    fn number_keeps_text() {
        assert_eq!(
            kinds("12345678901234567"),
            vec![TokenKind::Number {
                text: "12345678901234567".into(),
                value: 12_345_678_901_234_567.0
            }]
        );
    }

    #[test]
    fn i_flag_only_directly_after_quote() {
        assert_eq!(
            kinds("\"x\" i"),
            vec![
                TokenKind::Str {
                    value: "x".into(),
                    ci: false,
                    unterminated: false
                },
                TokenKind::Ident("i".into()),
            ]
        );
        assert_eq!(
            kinds("\"x\"in"),
            vec![
                TokenKind::Str {
                    value: "x".into(),
                    ci: false,
                    unterminated: false
                },
                TokenKind::Keyword(Keyword::In),
            ]
        );
    }

    #[test]
    fn error_positions() {
        assert_eq!(err("a == \"abc").1, 5..6);
        assert_eq!(err("a == \"ab\\").1, 5..6);
        assert_eq!(err("a == \"a\\nb\"").1, 7..9);
        assert_eq!(err("`order id").1, 0..1);
        assert_eq!(err("`` == 1").1, 0..2);
        assert_eq!(err("$0 == 1").1, 0..2);
        assert_eq!(err("$ == 1").1, 0..1);
        assert_eq!(err("$99999999999999999999999").1, 0..24);
        assert_eq!(err("a - 1").1, 2..3);
        assert_eq!(err("a = 1").1, 2..3);
        assert_eq!(err("a & b").1, 2..3);
        assert_eq!(err("a | b").1, 2..3);
        assert_eq!(err("a == 1.").1, 5..7);
        assert_eq!(err("a == 12ab").1, 5..9);
        assert_eq!(err("prix == é").1, 8..10);
        assert_eq!(err("a # b").0, "unexpected character '#'");
    }

    #[test]
    fn lenient_keeps_going() {
        let toks = lex_lenient("a = \"x\\q\" && b == \"open");
        let kinds: Vec<&TokenKind> = toks.iter().map(|t| &t.kind).collect();
        assert!(matches!(kinds[1], TokenKind::Error(_)));
        assert!(matches!(kinds[2], TokenKind::Error(_)));
        assert_eq!(toks[2].span, 4..9);
        assert_eq!(*kinds[3], TokenKind::Op(Op::AndAnd));
        assert_eq!(
            *kinds.last().unwrap(),
            &TokenKind::Str {
                value: "open".into(),
                ci: false,
                unterminated: true
            }
        );
        assert_eq!(toks.last().unwrap().span, 18..23);
    }
}
