//! Query lexer / parser / resolver / highlighter tests (M4-01, spec §9, §19).

use proptest::prelude::*;
use tachy_core::{
    column::ColumnName,
    query::{
        CmpOp, ColumnRef, Expr, Literal, Operand, QueryError, ResolvedExpr, ResolvedOperand,
        TokenClass, highlight, lexer, parse, resolve,
    },
};

// --- helpers (spans are zeroed; trees are compared with `without_spans`) ---

fn col(name: &str) -> Operand {
    Operand::Column(ColumnRef::Name {
        name: name.into(),
        span: 0..0,
    })
}

fn idx(n: usize) -> Operand {
    Operand::Column(ColumnRef::Index { n, span: 0..0 })
}

fn s(v: &str) -> Literal {
    Literal::Str {
        value: v.into(),
        ci: false,
        span: 0..0,
    }
}

fn si(v: &str) -> Literal {
    Literal::Str {
        value: v.into(),
        ci: true,
        span: 0..0,
    }
}

fn num(t: &str) -> Literal {
    Literal::Num {
        text: t.into(),
        value: t.parse().unwrap(),
        span: 0..0,
    }
}

fn lit(l: Literal) -> Operand {
    Operand::Literal(l)
}

fn cmp(lhs: Operand, op: CmpOp, rhs: Operand) -> Expr {
    Expr::Cmp {
        lhs,
        op,
        rhs,
        span: 0..0,
    }
}

fn p(input: &str) -> Expr {
    parse(input)
        .unwrap_or_else(|e| panic!("{input:?}: {e:?}"))
        .without_spans()
}

fn perr(input: &str) -> QueryError {
    parse(input).expect_err(input)
}

fn named(display: &str, query: &str) -> ColumnName {
    ColumnName {
        display: display.to_owned(),
        query: query.to_owned(),
    }
}

fn cols(names: &[&str]) -> Vec<ColumnName> {
    names.iter().map(|n| named(n, n)).collect()
}

// --- §9.3 examples ---

#[test]
fn spec_examples_parse_to_expected_ast() {
    let cases: Vec<(&str, Expr)> = vec![
        (
            r#"country == "DE" && price > 100 && status != "refunded""#,
            Expr::And(vec![
                cmp(col("country"), CmpOp::Eq, lit(s("DE"))),
                cmp(col("price"), CmpOp::Gt, lit(num("100"))),
                cmp(col("status"), CmpOp::Ne, lit(s("refunded"))),
            ]),
        ),
        (
            r#"ts >= "2026-03-01" and ts < "2026-04-01""#,
            Expr::And(vec![
                cmp(col("ts"), CmpOp::Ge, lit(s("2026-03-01"))),
                cmp(col("ts"), CmpOp::Lt, lit(s("2026-04-01"))),
            ]),
        ),
        (
            r#"status in ["paid", "shipped"]"#,
            Expr::In {
                operand: col("status"),
                list: vec![s("paid"), s("shipped")],
                span: 0..0,
            },
        ),
        (
            r#"`order id` ~ "^ORD-88""#,
            cmp(col("order id"), CmpOp::Match, lit(s("^ORD-88"))),
        ),
        (
            r#"notes is not null && notes contains "door"i"#,
            Expr::And(vec![
                Expr::IsNull {
                    operand: col("notes"),
                    negated: true,
                    span: 0..0,
                },
                cmp(col("notes"), CmpOp::Contains, lit(si("door"))),
            ]),
        ),
        (
            r#"$3 == "oslo-3""#,
            cmp(idx(3), CmpOp::Eq, lit(s("oslo-3"))),
        ),
    ];
    for (input, want) in cases {
        assert_eq!(p(input), want, "{input}");
    }
}

#[test]
fn and_binds_tighter_than_or() {
    assert_eq!(
        p(r#"country == "DE" && price > 100 || x == 1"#),
        Expr::Or(vec![
            Expr::And(vec![
                cmp(col("country"), CmpOp::Eq, lit(s("DE"))),
                cmp(col("price"), CmpOp::Gt, lit(num("100"))),
            ]),
            cmp(col("x"), CmpOp::Eq, lit(num("1"))),
        ])
    );
    assert_eq!(
        p("a || b && c"),
        Expr::Or(vec![
            Expr::Truthy(col("a")),
            Expr::And(vec![Expr::Truthy(col("b")), Expr::Truthy(col("c"))]),
        ])
    );
}

#[test]
fn keyword_aliases() {
    assert_eq!(
        p("not a == 1 and b is not null"),
        Expr::And(vec![
            Expr::Not(Box::new(cmp(col("a"), CmpOp::Eq, lit(num("1")))), 0..0),
            Expr::IsNull {
                operand: col("b"),
                negated: true,
                span: 0..0,
            },
        ])
    );
    assert_eq!(p("a or b"), p("a || b"));
    assert_eq!(p("!a"), p("not a"));
    assert_eq!(p("not not a"), p("!!a"));
}

#[test]
fn parentheses_group_and_compare() {
    assert_eq!(
        p("(a || b) && c"),
        Expr::And(vec![
            Expr::Or(vec![Expr::Truthy(col("a")), Expr::Truthy(col("b"))]),
            Expr::Truthy(col("c")),
        ])
    );
    assert_eq!(
        p("(x > 1) == true"),
        cmp(
            Operand::Group(Box::new(cmp(col("x"), CmpOp::Gt, lit(num("1"))))),
            CmpOp::Eq,
            lit(Literal::Bool(true, 0..0)),
        )
    );
    assert_eq!(p("((a))"), Expr::Truthy(col("a")));
}

#[test]
fn literals_and_operators() {
    assert_eq!(
        p("a == -1.5e-3"),
        cmp(col("a"), CmpOp::Eq, lit(num("-1.5e-3")))
    );
    assert_eq!(
        p("a != null"),
        cmp(col("a"), CmpOp::Ne, lit(Literal::Null(0..0)))
    );
    assert_eq!(
        p("a in [1, \"x\"i, true, null]"),
        Expr::In {
            operand: col("a"),
            list: vec![
                num("1"),
                si("x"),
                Literal::Bool(true, 0..0),
                Literal::Null(0..0)
            ],
            span: 0..0,
        }
    );
    for (text, op) in [
        ("==", CmpOp::Eq),
        ("!=", CmpOp::Ne),
        ("<", CmpOp::Lt),
        ("<=", CmpOp::Le),
        (">", CmpOp::Gt),
        (">=", CmpOp::Ge),
        ("contains", CmpOp::Contains),
        ("starts", CmpOp::Starts),
        ("ends", CmpOp::Ends),
    ] {
        assert_eq!(
            p(&format!("a {text} \"v\"")),
            cmp(col("a"), op, lit(s("v")))
        );
    }
    // Number text is kept exactly.
    let Expr::Cmp {
        rhs: Operand::Literal(Literal::Num { text, .. }),
        ..
    } = p("id == 12345678901234567")
    else {
        panic!()
    };
    assert_eq!(text, "12345678901234567");
}

#[test]
fn spans_are_byte_offsets() {
    let e = parse("  `ö x` == \"é\" && !b").unwrap();
    let Expr::And(v) = &e else { panic!() };
    let Expr::Cmp { lhs, rhs, span, .. } = &v[0] else {
        panic!()
    };
    assert_eq!(lhs.span(), 2..8);
    assert_eq!(rhs.span(), 12..16);
    assert_eq!(*span, 2..16);
    let Expr::Not(_, nspan) = &v[1] else { panic!() };
    assert_eq!(*nspan, 20..22);
}

// --- case-insensitive flag ---

#[test]
fn ci_flag_rules() {
    let e = perr(r#"price < "5"i"#);
    assert_eq!(e.message, "case-insensitive flag not supported with <");
    assert_eq!(e.span, 8..12);
    assert!(parse(r#"name == "x"i"#).is_ok());
    for op in ["!=", "contains", "starts", "ends"] {
        assert!(parse(&format!("name {op} \"x\"i")).is_ok(), "{op}");
    }
    for op in ["<=", ">", ">="] {
        assert!(parse(&format!("name {op} \"x\"i")).is_err(), "{op}");
    }
    assert!(parse(r#"name in ["a"i, "b"]"#).is_ok());
    let e = perr(r#"name ~ "x"i"#);
    assert!(e.message.contains("(?i)"), "{}", e.message);
    assert_eq!(e.span, 7..11);
    // The lhs is checked too.
    assert_eq!(perr(r#""x"i < name"#).span, 0..4);
}

// --- error positions, one per error kind ---

#[test]
fn error_positions() {
    let cases: &[(&str, std::ops::Range<usize>, &str)] = &[
        ("", 0..0, "empty filter"),
        ("   ", 0..0, "empty filter"),
        ("a == \"unterminated", 5..6, "unterminated string"),
        ("a == \"x\\t\"", 7..9, "invalid escape"),
        ("`order id == 1", 0..1, "unterminated column name"),
        ("`` == 1", 0..2, "empty column name"),
        ("$0 == 1", 0..2, "column numbers start at $1"),
        ("$ == 1", 0..1, "expected a column number"),
        ("a == - 1", 5..6, "'-' must be followed by a digit"),
        ("a = 1", 2..3, "use =="),
        ("a @ 1", 2..3, "unexpected character '@'"),
        ("a < b < c", 6..7, "can't be chained"),
        ("a == 1 in [1]", 7..9, "can't be chained"),
        ("a ~ b", 4..5, "right side of ~"),
        ("a ~ 1", 4..5, "right side of ~"),
        ("a == 1 b", 7..8, "unexpected input"),
        ("a == 1 ) && b", 7..13, "unexpected input"),
        ("a ==", 4..4, "expected a column or a value"),
        ("a &&", 4..4, "expected an expression after"),
        ("a || ", 5..5, "expected an expression after"),
        ("(a == 1", 0..1, "unclosed ("),
        ("(", 0..1, "unclosed ("),
        ("(a b)", 3..4, "expected )"),
        ("a in 1", 5..6, "expected [ after in"),
        ("a in [", 5..6, "unclosed ["),
        ("a in [1", 5..6, "unclosed ["),
        ("a in []", 6..7, "expected a literal"),
        ("a in [b]", 6..7, "expected a literal"),
        ("a in [1 2]", 8..9, "expected , or ]"),
        ("a is", 4..4, "expected null after is"),
        ("a is not 1", 9..10, "expected null after is not"),
        ("contains == 1", 0..8, "is a keyword"),
        ("== 1", 0..2, "expected a column or a value"),
        (")", 0..1, "expected a column or a value"),
    ];
    for (input, span, msg) in cases {
        let e = perr(input);
        assert_eq!(e.span, *span, "{input:?}: {e:?}");
        assert!(e.message.contains(msg), "{input:?}: {:?}", e.message);
        // Every span is a valid slice of the input.
        assert!(input.get(e.span.clone()).is_some());
    }
}

// --- resolution ---

#[test]
fn resolve_backticked_column() {
    let columns = cols(&["id", "order id", "price"]);
    let r = resolve(parse(r#"`order id` ~ "^ORD-88""#).unwrap(), &columns).unwrap();
    let ResolvedExpr::Cmp { lhs, .. } = r else {
        panic!()
    };
    assert_eq!(
        lhs,
        ResolvedOperand::Column {
            index: 1,
            span: 0..10
        }
    );
}

#[test]
fn resolve_unknown_column_with_suggestion() {
    let columns = cols(&["id", "price", "country"]);
    let e = resolve(parse("prce > 1").unwrap(), &columns).unwrap_err();
    assert_eq!(e.span, 0..4);
    assert_eq!(
        e.message,
        r#"unknown column "prce" — did you mean "price"?"#
    );
    let e = resolve(parse("x == 1 && zzzzzz > 1").unwrap(), &columns).unwrap_err();
    assert_eq!(e.span, 0..1);
    assert_eq!(e.message, r#"unknown column "x" — did you mean "id"?"#);
    let e = resolve(parse("zzzzzz > 1").unwrap(), &columns).unwrap_err();
    assert_eq!(e.message, r#"unknown column "zzzzzz""#);
    // No case folding for lookups; the suggestion is case-insensitive.
    let e = resolve(parse("PRICE > 1").unwrap(), &columns).unwrap_err();
    assert_eq!(
        e.message,
        r#"unknown column "PRICE" — did you mean "price"?"#
    );
}

#[test]
fn resolve_query_name_then_display_name() {
    let columns = vec![
        named("name", "name"),
        named("name", "name_2"),
        named(" total ", "total"),
    ];
    let index_of = |q: &str| match resolve(parse(q).unwrap(), &columns).unwrap() {
        ResolvedExpr::Truthy(ResolvedOperand::Column { index, .. }) => index,
        other => panic!("{other:?}"),
    };
    assert_eq!(index_of("name"), 0);
    assert_eq!(index_of("name_2"), 1);
    assert_eq!(index_of("total"), 2);
    assert_eq!(index_of("` total `"), 2);
}

#[test]
fn resolve_column_numbers() {
    let columns = cols(&["a"; 13]);
    let r = resolve(parse("$13 == 1").unwrap(), &columns).unwrap();
    assert!(matches!(
        r,
        ResolvedExpr::Cmp {
            lhs: ResolvedOperand::Column { index: 12, .. },
            ..
        }
    ));
    let e = resolve(parse("x == 1 || $14 == 1").unwrap(), &cols(&["x"; 13])).unwrap_err();
    assert_eq!(e.message, "$14 is out of range (13 columns)");
    assert_eq!(e.span, 10..13);
    let e = resolve(parse("$2").unwrap(), &cols(&["x"])).unwrap_err();
    assert_eq!(e.message, "$2 is out of range (1 column)");
}

#[test]
fn resolve_keeps_shape() {
    let columns = cols(&["a", "b"]);
    let r = resolve(
        parse(r#"!(a == 1) || (b > 2) == true && b in [1] && a is null"#).unwrap(),
        &columns,
    )
    .unwrap();
    let ResolvedExpr::Or(v) = r else { panic!() };
    assert!(matches!(v[0], ResolvedExpr::Not(..)));
    let ResolvedExpr::And(w) = &v[1] else {
        panic!()
    };
    assert!(matches!(
        w[0],
        ResolvedExpr::Cmp {
            lhs: ResolvedOperand::Group(_),
            ..
        }
    ));
    assert!(matches!(w[1], ResolvedExpr::In { .. }));
    assert!(matches!(w[2], ResolvedExpr::IsNull { negated: false, .. }));
}

// --- highlighting ---

#[test]
fn highlight_classes() {
    let columns = cols(&["price", "name"]);
    let h = highlight(
        r#"price > 10 && nmae == "x"i || $3 is null"#,
        Some(&columns),
    );
    let classes: Vec<TokenClass> = h.iter().map(|(_, c)| *c).collect();
    assert_eq!(
        classes,
        vec![
            TokenClass::Column,
            TokenClass::Operator,
            TokenClass::Number,
            TokenClass::Operator,
            TokenClass::UnknownColumn,
            TokenClass::Operator,
            TokenClass::String,
            TokenClass::Operator,
            TokenClass::UnknownColumn,
            TokenClass::Keyword,
            TokenClass::Keyword,
        ]
    );
    assert_eq!(h[0].0, 0..5);
    // Without a column list every name is a plain column.
    assert!(
        highlight("nmae", None)
            .iter()
            .all(|(_, c)| *c == TokenClass::Column)
    );
}

#[test]
fn highlight_incomplete_input() {
    let h = highlight(r#"a == "unfinished"#, None);
    assert_eq!(h.last().unwrap(), &(5..16, TokenClass::String));
    let h = highlight("a = 1 # `x", None);
    let classes: Vec<TokenClass> = h.iter().map(|(_, c)| *c).collect();
    assert_eq!(
        classes,
        vec![
            TokenClass::Column,
            TokenClass::Error,
            TokenClass::Number,
            TokenClass::Error,
            TokenClass::Error,
        ]
    );
}

// --- round trips ---

#[test]
fn spec_examples_round_trip() {
    for input in [
        r#"country == "DE" && price > 100 && status != "refunded""#,
        r#"ts >= "2026-03-01" and ts < "2026-04-01""#,
        r#"status in ["paid", "shipped"]"#,
        r#"`order id` ~ "^ORD-88""#,
        r#"notes is not null && notes contains "door"i"#,
        r#"$3 == "oslo-3""#,
        r#"country == "DE" && price > 100 || x == 1"#,
        r#"not a == 1 and b is not null"#,
        r#"!(a || b) && (c && d || e) || ((f))"#,
        r#"(x > 1) == true && `and` == "a \"quoted\" \\ value""#,
        r#"a || (b || c)"#,
        r#"(a && b) && c"#,
    ] {
        let ast = parse(input).unwrap();
        let printed = ast.to_string();
        let reparsed = parse(&printed).unwrap_or_else(|e| panic!("{printed:?}: {e:?}"));
        assert_eq!(reparsed.without_spans(), ast.without_spans(), "{printed}");
    }
    assert_eq!(
        parse("a  and  b or not c").unwrap().to_string(),
        "a && b || !c"
    );
}

fn arb_name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z_][a-z0-9_]{0,6}",
        "[a-zA-Z0-9 _.é-]{1,8}",
        Just("and".to_owned()),
        Just("contains".to_owned()),
    ]
}

fn arb_literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        ("[ -~é\"\\\\]{0,8}", any::<bool>()).prop_map(|(v, ci)| Literal::Str {
            value: v,
            ci,
            span: 0..0
        }),
        "-?[0-9]{1,5}(\\.[0-9]{1,3})?([eE][+-]?[0-9]{1,2})?".prop_map(|t| Literal::Num {
            value: t.parse().unwrap(),
            text: t,
            span: 0..0
        }),
        any::<bool>().prop_map(|b| Literal::Bool(b, 0..0)),
        Just(Literal::Null(0..0)),
    ]
}

fn arb_column() -> impl Strategy<Value = Operand> {
    prop_oneof![
        arb_name().prop_map(|name| Operand::Column(ColumnRef::Name { name, span: 0..0 })),
        (1usize..100).prop_map(|n| Operand::Column(ColumnRef::Index { n, span: 0..0 })),
    ]
}

/// Strips the `i` flag where the operator does not allow it.
fn fix_ci(o: Operand, op: CmpOp) -> Operand {
    match o {
        Operand::Literal(Literal::Str { value, ci, span }) => Operand::Literal(Literal::Str {
            value,
            ci: ci && op.allows_ci(),
            span,
        }),
        o => o,
    }
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    let ops = prop_oneof![
        Just(CmpOp::Eq),
        Just(CmpOp::Ne),
        Just(CmpOp::Lt),
        Just(CmpOp::Le),
        Just(CmpOp::Gt),
        Just(CmpOp::Ge),
        Just(CmpOp::Match),
        Just(CmpOp::Contains),
        Just(CmpOp::Starts),
        Just(CmpOp::Ends),
    ];
    let simple_operand = prop_oneof![arb_column(), arb_literal().prop_map(Operand::Literal)];
    let leaf = prop_oneof![
        (
            simple_operand.clone(),
            ops.clone(),
            simple_operand.clone(),
            "[a-z^$.*]{0,5}"
        )
            .prop_map(|(l, op, r, re)| {
                let r = if op == CmpOp::Match {
                    Operand::Literal(Literal::Str {
                        value: re,
                        ci: false,
                        span: 0..0,
                    })
                } else {
                    fix_ci(r, op)
                };
                Expr::Cmp {
                    lhs: fix_ci(l, op),
                    op,
                    rhs: r,
                    span: 0..0,
                }
            }),
        (
            simple_operand.clone(),
            prop::collection::vec(arb_literal(), 1..4)
        )
            .prop_map(|(operand, list)| Expr::In {
                operand,
                list,
                span: 0..0
            }),
        (simple_operand.clone(), any::<bool>()).prop_map(|(operand, negated)| Expr::IsNull {
            operand,
            negated,
            span: 0..0
        }),
        simple_operand.prop_map(Expr::Truthy),
    ];
    leaf.prop_recursive(4, 32, 4, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 2..4).prop_map(Expr::Or),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Expr::And),
            inner.clone().prop_map(|e| Expr::Not(Box::new(e), 0..0)),
            (inner, ops.clone(), arb_literal()).prop_map(|(e, op, r)| {
                let rhs = if op == CmpOp::Match {
                    Operand::Literal(Literal::Str {
                        value: "x".into(),
                        ci: false,
                        span: 0..0,
                    })
                } else {
                    fix_ci(Operand::Literal(r), op)
                };
                Expr::Cmp {
                    lhs: Operand::Group(Box::new(e)),
                    op,
                    rhs,
                    span: 0..0,
                }
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn generated_asts_round_trip(ast in arb_expr()) {
        let printed = ast.to_string();
        let reparsed = parse(&printed)
            .map_err(|e| TestCaseError::fail(format!("{printed:?}: {e:?}")))?;
        prop_assert_eq!(reparsed.without_spans(), ast, "{}", printed);
    }

    #[test]
    fn arbitrary_strings_never_panic(input in "\\PC{0,40}") {
        let _ = lexer::lex(&input);
        let _ = parse(&input);
        let columns = cols(&["a", "b"]);
        for (span, _) in highlight(&input, Some(&columns)) {
            prop_assert!(input.get(span).is_some());
        }
        if let Err(e) = parse(&input) {
            prop_assert!(input.get(e.span).is_some());
        }
    }

    #[test]
    fn query_like_strings_never_panic(
        input in "[a-z$`\"\\\\ ()\\[\\],!=<>~&|0-9.eé-]{0,40}"
    ) {
        let _ = parse(&input);
        let toks = lexer::lex_lenient(&input);
        let mut prev_end = 0;
        for t in &toks {
            prop_assert!(t.span.start >= prev_end && t.span.end > t.span.start);
            prop_assert!(input.get(t.span.clone()).is_some());
            prev_end = t.span.end;
        }
        if let Ok(ast) = parse(&input) {
            let _ = resolve(ast, &cols(&["a", "e"]));
        }
    }

    #[test]
    fn arbitrary_bytes_lossily_decoded_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..40)) {
        // Invalid UTF-8 (e.g. a multibyte char cut by a paste) reaches the bar
        // as replacement characters next to multibyte text.
        let input = String::from_utf8_lossy(&bytes).into_owned();
        let _ = parse(&input);
        for (span, _) in highlight(&input, None) {
            prop_assert!(input.get(span).is_some());
        }
    }
}
