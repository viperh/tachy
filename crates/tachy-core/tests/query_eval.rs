//! M4-02: typed query compilation and evaluation (spec §9.2).
//!
//! - Evaluation tables: every operator × column type × {valid, unparseable,
//!   null, missing field}.
//! - Acceptance cases.
//! - Differential test against a naive reference evaluator that parses values
//!   with `str::parse`.

use proptest::prelude::*;
use tachy_core::{
    column::{ColumnMeta, ColumnName},
    dialect::{Dialect, Encoding},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    query::{self, EvalScratch, HighlightRule, Predicate, QueryError},
    types::{ColType, NullSet},
};

fn columns(spec: &[(&str, ColType)]) -> Vec<ColumnMeta> {
    spec.iter()
        .enumerate()
        .map(|(i, (n, t))| {
            let mut c = ColumnMeta::new(
                ColumnName {
                    display: (*n).to_owned(),
                    query: (*n).to_owned(),
                },
                i,
                false,
            );
            c.set_inferred(*t);
            c
        })
        .collect()
}

fn compile_with(q: &str, cols: &[ColumnMeta], d: &Dialect) -> Result<Predicate, QueryError> {
    let names: Vec<ColumnName> = cols.iter().map(|c| c.name.clone()).collect();
    let ast = query::parse(q)?;
    let resolved = query::resolve(ast, &names)?;
    query::compile(&resolved, cols, d, &NullSet::default())
}

fn compile(q: &str, cols: &[ColumnMeta]) -> Predicate {
    compile_with(q, cols, &Dialect::default()).unwrap_or_else(|e| panic!("{q}: {e}"))
}

/// Evaluates `p` on one CSV line (no header).
fn eval_line(p: &Predicate, line: &[u8]) -> bool {
    let mut parser = RecordParser::new(&Dialect::default());
    let mut rec = RecordRanges::default();
    let out = parser.parse_at(line, 0, &mut rec);
    assert!(matches!(out, ParseOutcome::Record { .. }), "{line:?}");
    p.eval_record(line, &rec, &mut EvalScratch::new())
}

// ---------------------------------------------------------------------------
// Evaluation tables
// ---------------------------------------------------------------------------

const OPS: [&str; 13] = [
    "==", "!=", "<", "<=", ">", ">=", "contains", "starts", "ends", "~", "in", "isnull", "notnull",
];

struct Table {
    ty: ColType,
    /// Right side for the six ordering operators.
    lit: &'static str,
    contains: &'static str,
    starts: &'static str,
    ends: &'static str,
    regex: &'static str,
    list: &'static str,
    /// `(field, expected results for OPS)`; `None` = the field is missing.
    rows: Vec<(Option<&'static str>, &'static str)>,
}

const NULL_ROW: &str = "0100000000010";

fn run_table(t: &Table) {
    let cols = columns(&[("k", ColType::Str), ("x", t.ty)]);
    let queries: Vec<String> = OPS
        .iter()
        .map(|op| match *op {
            "contains" => format!("x contains {}", t.contains),
            "starts" => format!("x starts {}", t.starts),
            "ends" => format!("x ends {}", t.ends),
            "~" => format!("x ~ {}", t.regex),
            "in" => format!("x in {}", t.list),
            "isnull" => "x is null".to_owned(),
            "notnull" => "x is not null".to_owned(),
            op => format!("x {op} {}", t.lit),
        })
        .collect();
    let preds: Vec<Predicate> = queries.iter().map(|q| compile(q, &cols)).collect();
    let mut rows = t.rows.clone();
    rows.push((Some(""), NULL_ROW));
    rows.push((Some("NULL"), NULL_ROW));
    rows.push((None, NULL_ROW));
    for (field, expected) in rows {
        let line = match field {
            Some(v) if v.contains(',') || v.contains('"') => {
                format!("k,\"{}\"\n", v.replace('"', "\"\""))
            }
            Some(v) => format!("k,{v}\n"),
            None => "k\n".to_owned(),
        };
        for ((q, p), want) in queries.iter().zip(&preds).zip(expected.chars()) {
            let got = eval_line(p, line.as_bytes());
            assert_eq!(
                got,
                want == '1',
                "type {} field {field:?}: `{q}`",
                t.ty.label()
            );
        }
    }
}

#[test]
fn table_i64() {
    run_table(&Table {
        ty: ColType::I64,
        lit: "100",
        contains: "\"10\"",
        starts: "\"10\"",
        ends: "\"00\"",
        regex: "\"^1\"",
        list: "[100, 7]",
        rows: vec![
            (Some("99"), "0111000000001"),
            (Some("100"), "1001011111101"),
            (Some("+100"), "1001011010101"),
            (Some("1000"), "0100111111001"),
            (Some("abc"), "0100000000001"),
            (Some("1,000"), "0100000011001"),
        ],
    });
}

#[test]
fn table_f64() {
    run_table(&Table {
        ty: ColType::F64,
        lit: "100",
        contains: "\"10\"",
        starts: "\"10\"",
        ends: "\"0\"",
        regex: "\"^1\"",
        list: "[100, 2.5]",
        rows: vec![
            (Some("99.5"), "0111000000001"),
            (Some("100.0"), "1001011111101"),
            (Some("1e3"), "0100110001001"),
            (Some("abc"), "0100000000001"),
            (Some("inf"), "0100000000001"),
        ],
    });
}

#[test]
fn table_date() {
    run_table(&Table {
        ty: ColType::Date,
        lit: "\"2026-03-01\"",
        contains: "\"-03-\"",
        starts: "\"2026-03\"",
        ends: "\"01\"",
        regex: "\"^2026\"",
        list: "[\"2026-03-01\", \"2020-01-01\"]",
        rows: vec![
            (Some("2026-02-28"), "0111000001001"),
            (Some("2026-03-01"), "1001011111101"),
            (Some("2026-03-02"), "0100111101001"),
            (Some("2026-02-30"), "0100000001001"),
        ],
    });
}

#[test]
fn table_datetime() {
    run_table(&Table {
        ty: ColType::DateTime,
        lit: "\"2026-03-01T12:00:00Z\"",
        contains: "\"T\"",
        starts: "\"2026\"",
        ends: "\"Z\"",
        regex: "\"^2026-03\"",
        list: "[\"2026-03-01T12:00:00Z\"]",
        rows: vec![
            (Some("2026-03-01T11:59:59Z"), "0111001111001"),
            (Some("2026-03-01 13:00:00+01:00"), "1001010101101"),
            (Some("2026-03-01T12:00:01Z"), "0100111111001"),
            (Some("2026-03-01T25:00"), "0100001101001"),
        ],
    });
}

#[test]
fn table_bool() {
    run_table(&Table {
        ty: ColType::Bool,
        lit: "true",
        contains: "\"e\"",
        starts: "\"y\"",
        ends: "\"e\"",
        regex: "\"(?i)^t\"",
        list: "[true]",
        rows: vec![
            (Some("no"), "0111000000001"),
            (Some("yes"), "1001011100101"),
            (Some("TRUE"), "1001010001101"),
            (Some("maybe"), "0100001010001"),
        ],
    });
}

#[test]
fn table_str_and_enum() {
    for ty in [ColType::Str, ColType::Enum] {
        run_table(&Table {
            ty,
            lit: "\"m\"",
            contains: "\"a\"",
            starts: "\"z\"",
            ends: "\"e\"",
            regex: "\"^[A-Z]\"",
            list: "[\"m\", \"apple\"]",
            rows: vec![
                (Some("apple"), "0111001010101"),
                (Some("m"), "1001010000101"),
                (Some("zebra"), "0100111100001"),
                (Some("Mango"), "0111001001001"),
            ],
        });
    }
}

// ---------------------------------------------------------------------------
// Acceptance cases
// ---------------------------------------------------------------------------

#[test]
fn f64_orders_numerically() {
    let cols = columns(&[("price", ColType::F64)]);
    let p = compile("price > 100", &cols);
    assert!(!eval_line(&p, b"99.5\n"));
    assert!(!eval_line(&p, b"100\n"));
    assert!(eval_line(&p, b"1e3\n"));
    assert!(eval_line(&p, b"100.5\n"));
}

#[test]
fn ne_is_true_for_null_and_unparseable() {
    let cols = columns(&[("status", ColType::Str), ("n", ColType::I64)]);
    let ne = compile("status != \"x\"", &cols);
    let eq = compile("status == \"x\"", &cols);
    assert!(eval_line(&ne, b"NULL,1\n"));
    assert!(!eval_line(&eq, b"NULL,1\n"));
    assert!(eval_line(&ne, b"y,1\n"));
    assert!(!eval_line(&ne, b"x,1\n"));
    let ne = compile("n != 5", &cols);
    let eq = compile("n == 5", &cols);
    for line in [&b"a,abc\n"[..], b"a,\n", b"a\n"] {
        assert!(eval_line(&ne, line), "{line:?}");
        assert!(!eval_line(&eq, line), "{line:?}");
    }
}

#[test]
fn case_insensitive_contains() {
    let cols = columns(&[("customer", ColType::Str), ("name", ColType::Str)]);
    let p = compile("customer contains \"becker\"i", &cols);
    assert!(eval_line(&p, b"Becker,x\n"));
    assert!(eval_line(&p, b"Mr BECKER,x\n"));
    assert!(!eval_line(&p, b"Beck,x\n"));
    let p = compile("name contains \"ü\"i", &cols);
    assert!(eval_line(&p, "a,MÜLLER\n".as_bytes()));
    assert!(eval_line(&p, "a,müller\n".as_bytes()));
    assert!(!eval_line(&p, b"a,MULLER\n"));
    let p = compile("name == \"müller\"i", &cols);
    assert!(eval_line(&p, "a,MÜLLER\n".as_bytes()));
    let p = compile("name starts \"MÜ\"i", &cols);
    assert!(eval_line(&p, "a,müller\n".as_bytes()));
    let p = compile("name ends \"LER\"i", &cols);
    assert!(eval_line(&p, "a,müller\n".as_bytes()));
    assert!(eval_line(&p, b"a,Miller\n"));
}

#[test]
fn invalid_regex_spans_the_literal() {
    let cols = columns(&[("a", ColType::Str)]);
    let q = "a ~ \"(unclosed\"";
    let err = compile_with(q, &cols, &Dialect::default()).unwrap_err();
    assert_eq!(&q[err.span.clone()], "\"(unclosed\"");
    assert!(err.message.starts_with("invalid regex"), "{}", err.message);
}

#[test]
fn large_i64_compares_exactly() {
    let cols = columns(&[("id", ColType::I64)]);
    let p = compile("id == 12345678901234567", &cols);
    assert!(eval_line(&p, b"12345678901234567\n"));
    // Same f64 value, different integer.
    assert!(!eval_line(&p, b"12345678901234568\n"));
    assert!(!eval_line(&p, b"12345678901234566\n"));
}

#[test]
fn literal_type_errors() {
    let cols = columns(&[
        ("price", ColType::F64),
        ("ts", ColType::Date),
        ("flag", ColType::Bool),
        ("n", ColType::I64),
    ]);
    let q = "price > \"abc\"";
    let err = compile_with(q, &cols, &Dialect::default()).unwrap_err();
    assert_eq!(err.message, "\"abc\" is not a valid f64 for column price");
    assert_eq!(&q[err.span], "\"abc\"");
    for q in [
        "ts > 5",
        "ts == \"2026-02-30\"",
        "flag == \"maybe\"",
        "n < true",
        "n in [1, \"x\"]",
        "price",
        "n < null",
    ] {
        assert!(compile_with(q, &cols, &Dialect::default()).is_err(), "{q}");
    }
    let err = compile_with("price", &cols, &Dialect::default()).unwrap_err();
    assert_eq!(err.message, "column price is not bool");
}

#[test]
fn typed_literals() {
    let cols = columns(&[
        ("n", ColType::I64),
        ("d", ColType::Date),
        ("t", ColType::DateTime),
        ("b", ColType::Bool),
    ]);
    // Decimal literal on an i64 column: both as f64.
    let p = compile("n > 1.5", &cols);
    assert!(eval_line(&p, b"2,,,\n"));
    assert!(!eval_line(&p, b"1,,,\n"));
    // A string literal that parses is accepted.
    assert!(eval_line(&compile("n == \"7\"", &cols), b"7,,,\n"));
    // datetime literal vs date column: the date part.
    let p = compile("d == \"2026-03-01T15:00:00Z\"", &cols);
    assert!(eval_line(&p, b",2026-03-01,,\n"));
    // date literal vs datetime column: midnight UTC.
    let p = compile("t >= \"2026-03-01\"", &cols);
    assert!(eval_line(&p, b",,2026-03-01T00:00:00Z,\n"));
    assert!(!eval_line(&p, b",,2026-02-28T23:59:59Z,\n"));
    assert!(eval_line(&p, b",,2026-03-01T00:30:00+00:30,\n"));
    assert!(!eval_line(&p, b",,2026-03-01T00:30:00+01:00,\n"));
    // bool literals and the recogniser.
    assert!(eval_line(&compile("b == \"yes\"", &cols), b",,,TRUE\n"));
    assert!(eval_line(&compile("b == 1", &cols), b",,,true\n"));
    assert!(eval_line(&compile("b", &cols), b",,,yes\n"));
    assert!(!eval_line(&compile("b", &cols), b",,,no\n"));
    assert!(!eval_line(&compile("b", &cols), b",,,\n"));
}

#[test]
fn nulls_and_in() {
    let cols = columns(&[("x", ColType::Str), ("n", ColType::I64)]);
    assert!(eval_line(&compile("x == null", &cols), b"NA,1\n"));
    assert!(!eval_line(&compile("x == null", &cols), b"a,1\n"));
    assert!(eval_line(&compile("x != null", &cols), b"a,1\n"));
    let p = compile("x in [\"a\", null]", &cols);
    assert!(eval_line(&p, b",1\n"));
    assert!(eval_line(&p, b"a,1\n"));
    assert!(!eval_line(&p, b"b,1\n"));
    // > 8 items: hash set; `i` items.
    let p = compile(
        "x in [\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\",\"Jj\"i]",
        &cols,
    );
    assert!(eval_line(&p, b"i,1\n"));
    assert!(eval_line(&p, b"JJ,1\n"));
    assert!(!eval_line(&p, b"I,1\n"));
    let p = compile("n in [1, 2.5]", &cols);
    assert!(eval_line(&p, b"a,1\n"));
    assert!(!eval_line(&p, b"a,2\n"));
}

#[test]
fn column_vs_column_and_literal_on_the_left() {
    let cols = columns(&[
        ("a", ColType::I64),
        ("b", ColType::I64),
        ("s", ColType::Str),
    ]);
    let p = compile("a < b", &cols);
    assert!(eval_line(&p, b"2,10,x\n"));
    assert!(!eval_line(&p, b"10,2,x\n"));
    assert!(!eval_line(&p, b"x,2,x\n"));
    assert!(!eval_line(&p, b",2,x\n"));
    assert!(eval_line(&compile("a != b", &cols), b",2,x\n"));
    // str vs i64: bytes.
    let p = compile("s < a", &cols);
    assert!(!eval_line(&p, b"10,0,2\n"));
    assert!(eval_line(&p, b"2,0,10\n"));
    // literal on the left.
    assert!(eval_line(&compile("100 > a", &cols), b"99,0,x\n"));
    assert!(!eval_line(&compile("100 > a", &cols), b"100,0,x\n"));
    let p = compile("\"hello world\" contains s", &cols);
    assert!(eval_line(&p, b"1,1,lo w\n"));
    assert!(!eval_line(&p, b"1,1,xyz\n"));
    let p = compile("\"HELLO\" starts s", &cols);
    assert!(eval_line(&p, b"1,1,HEL\n"));
    let p = compile("\"HELLO\"i starts s", &cols);
    assert!(eval_line(&p, b"1,1,hel\n"));
}

#[test]
fn constants_and_groups() {
    let cols = columns(&[("a", ColType::I64)]);
    assert!(eval_line(&compile("1 < 2", &cols), b"5\n"));
    assert!(!eval_line(&compile("\"b\" < \"a\"", &cols), b"5\n"));
    assert!(eval_line(
        &compile("\"abc\" contains \"b\" && a == 5", &cols),
        b"5\n"
    ));
    assert!(eval_line(&compile("(a > 1) == true", &cols), b"5\n"));
    assert!(eval_line(&compile("(a > 10) == false", &cols), b"5\n"));
    assert!(eval_line(&compile("(a > 10) != true", &cols), b"5\n"));
    assert!(eval_line(&compile("(a > 1) == (a > 2)", &cols), b"5\n"));
    assert!(!eval_line(&compile("(a > 1) != (a > 2)", &cols), b"5\n"));
    assert!(eval_line(
        &compile("!(a > 10) && (a < 3 || a == 5)", &cols),
        b"5\n"
    ));
    assert!(compile_with("(a > 1) < true", &cols, &Dialect::default()).is_err());
}

#[test]
fn quoted_fields_are_unescaped() {
    let cols = columns(&[("a", ColType::Str), ("b", ColType::I64)]);
    let p = compile("a == \"say \\\"hi\\\"\"", &cols);
    assert!(eval_line(&p, b"\"say \"\"hi\"\"\",1\n"));
    let p = compile("b == 5", &cols);
    assert!(eval_line(&p, b"x,\"5\"\n"));
}

#[test]
fn windows_1252_literals_are_transcoded() {
    let d = Dialect {
        encoding: Encoding::Windows1252,
        header: false,
        ..Dialect::default()
    };
    let cols = columns(&[("name", ColType::Str)]);
    let p = compile_with("name == \"Müller\"", &cols, &d).unwrap();
    let mut parser = RecordParser::new(&d);
    let mut rec = RecordRanges::default();
    let line = b"M\xfcller\n";
    parser.parse_at(line, 0, &mut rec);
    assert!(p.eval_record(line, &rec, &mut EvalScratch::new()));
    let p = compile_with("name contains \"ü\"i", &cols, &d).unwrap();
    let line = b"M\xdcLLER\n";
    parser.parse_at(line, 0, &mut rec);
    assert!(p.eval_record(line, &rec, &mut EvalScratch::new()));
    let p = compile_with("name ~ \"^Mü\"", &cols, &d).unwrap();
    let line = b"M\xfcller\n";
    parser.parse_at(line, 0, &mut rec);
    assert!(p.eval_record(line, &rec, &mut EvalScratch::new()));
    assert!(compile_with("name == \"日本\"", &cols, &d).is_err());
}

#[test]
fn columns_required_literal_and_highlights() {
    let cols = columns(&[
        ("country", ColType::Str),
        ("note", ColType::Str),
        ("price", ColType::F64),
    ]);
    let p = compile(
        "country == \"DE\" && note contains \"door\" && price > 5",
        &cols,
    );
    assert_eq!(p.columns(), &[0, 1, 2]);
    assert_eq!(p.fields(), &[0, 1, 2]);
    assert_eq!(p.required_literal(), Some(&b"door"[..]));
    let p = compile("country == \"Germany\" && note starts \"ab\"", &cols);
    assert_eq!(p.required_literal(), Some(&b"Germany"[..]));
    for q in [
        "price > 5",
        "note contains \"door\" || price > 5",
        "note contains \"door\"i",
        "note contains \"a\"\"b\"",
        "note contains \"x\\\"yz\"",
    ] {
        if let Ok(p) = compile_with(q, &cols, &Dialect::default()) {
            assert_eq!(p.required_literal(), None, "{q}");
        }
    }
    let p = compile("note contains \"ab\" && !(country == \"x\")", &cols);
    let rule = p.highlights(1).unwrap();
    assert_eq!(rule.find("xabyab"), vec![1..3, 4..6]);
    assert!(
        p.highlights(0).is_none(),
        "negated comparisons don't highlight"
    );
    let p = compile("note ~ \"[0-9]+\" && country == \"DE\"i", &cols);
    assert!(matches!(p.highlights(1), Some(HighlightRule::Regex(_))));
    assert_eq!(p.highlights(1).unwrap().find("a12b3"), vec![1..3, 4..5]);
    assert_eq!(p.highlights(0).unwrap().find("de"), vec![0..2]);
    let p = compile("note starts \"AB\"i || note ends \"z\"", &cols);
    let rules: Vec<_> = p.highlight_rules(1).collect();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].find("abc"), vec![0..2]);
    assert_eq!(rules[1].find("xyz"), vec![2..3]);
}

#[test]
fn predicate_is_send_sync_clone() {
    fn check<T: Send + Sync + Clone>() {}
    check::<Predicate>();
}

// ---------------------------------------------------------------------------
// Differential test
// ---------------------------------------------------------------------------

/// Columns of the random records, in field order.
const DIFF_COLS: [(&str, ColType); 5] = [
    ("s", ColType::Str),
    ("i", ColType::I64),
    ("f", ColType::F64),
    ("d", ColType::Date),
    ("b", ColType::Bool),
];

const S_VALUES: &[&str] = &[
    "apple", "Apple", "banana", "DE", "de", "", "NA", "Müller", "müller", "a,b", "x\"y", "ab",
];
const I_VALUES: &[&str] = &[
    "5",
    "-3",
    "+7",
    "007",
    "100",
    "-0",
    "9223372036854775807",
    "abc",
    "1.5",
    "",
    "NULL",
    "12",
];
const F_VALUES: &[&str] = &[
    "1.5", "-2", "1e3", ".5", "5.", "abc", "", "1,5", "100", "-0.0", "2.50", "N/A",
];
const D_VALUES: &[&str] = &[
    "2024-02-29",
    "2023-02-29",
    "2026-03-01",
    "1999-12-31",
    "x",
    "",
    "2026-13-01",
    "2026-03-02",
];
const B_VALUES: &[&str] = &["true", "FALSE", "yes", "0", "1", "maybe", "", "No"];

fn values_for(col: usize) -> &'static [&'static str] {
    [S_VALUES, I_VALUES, F_VALUES, D_VALUES, B_VALUES][col]
}

#[derive(Clone, Debug)]
enum Lit {
    Str(String, bool),
    Num(String),
    Bool(bool),
    Null,
}

impl Lit {
    fn render(&self) -> String {
        match self {
            Lit::Str(s, ci) => format!(
                "\"{}\"{}",
                s.replace('\\', "\\\\").replace('"', "\\\""),
                if *ci { "i" } else { "" }
            ),
            Lit::Num(t) => t.clone(),
            Lit::Bool(b) => b.to_string(),
            Lit::Null => "null".to_owned(),
        }
    }

    fn text(&self) -> String {
        match self {
            Lit::Str(s, _) => s.clone(),
            Lit::Num(t) => t.clone(),
            Lit::Bool(b) => b.to_string(),
            Lit::Null => "null".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
enum RExpr {
    Cmp(usize, &'static str, Lit),
    IsNull(usize, bool),
    In(usize, Vec<Lit>),
    And(Vec<RExpr>),
    Or(Vec<RExpr>),
    Not(Box<RExpr>),
}

impl RExpr {
    fn render(&self) -> String {
        match self {
            RExpr::Cmp(c, op, l) => format!("{} {op} {}", DIFF_COLS[*c].0, l.render()),
            RExpr::IsNull(c, neg) => {
                format!(
                    "{} is {}null",
                    DIFF_COLS[*c].0,
                    if *neg { "not " } else { "" }
                )
            }
            RExpr::In(c, list) => format!(
                "{} in [{}]",
                DIFF_COLS[*c].0,
                list.iter().map(Lit::render).collect::<Vec<_>>().join(", ")
            ),
            RExpr::And(v) => v
                .iter()
                .map(|e| format!("({})", e.render()))
                .collect::<Vec<_>>()
                .join(" && "),
            RExpr::Or(v) => v
                .iter()
                .map(|e| format!("({})", e.render()))
                .collect::<Vec<_>>()
                .join(" || "),
            RExpr::Not(e) => format!("!({})", e.render()),
        }
    }
}

const CMP_OPS: [&str; 9] = [
    "==", "!=", "<", "<=", ">", ">=", "contains", "starts", "ends",
];

fn lit_for(col: usize, op: &'static str) -> BoxedStrategy<Lit> {
    let string_op = matches!(op, "contains" | "starts" | "ends");
    let ci_ok = matches!(op, "==" | "!=" | "contains" | "starts" | "ends");
    let ty = DIFF_COLS[col].1;
    if string_op || ty == ColType::Str {
        let pool: Vec<&'static str> = values_for(col)
            .iter()
            .copied()
            .chain(["a", "pp", "ü", "Ü", "B"])
            .filter(|s| !s.is_empty())
            .collect();
        return (prop::sample::select(pool), any::<bool>())
            .prop_map(move |(s, ci)| Lit::Str(s.to_owned(), ci && ci_ok))
            .boxed();
    }
    match ty {
        ColType::I64 => prop::sample::select(vec!["5", "100", "-3", "0", "12", "1.5", "-2.5"])
            .prop_map(|t| Lit::Num(t.to_owned()))
            .boxed(),
        ColType::F64 => prop::sample::select(vec!["1.5", "100", "-2", "0", "2.5", "1000"])
            .prop_map(|t| Lit::Num(t.to_owned()))
            .boxed(),
        ColType::Date => prop::sample::select(vec!["2026-03-01", "2024-02-29", "2000-01-01"])
            .prop_map(|t| Lit::Str(t.to_owned(), false))
            .boxed(),
        _ => any::<bool>().prop_map(Lit::Bool).boxed(),
    }
}

fn leaf() -> impl Strategy<Value = RExpr> {
    let cmp = (0..DIFF_COLS.len(), prop::sample::select(CMP_OPS.to_vec()))
        .prop_flat_map(|(c, op)| lit_for(c, op).prop_map(move |l| RExpr::Cmp(c, op, l)));
    let is_null = (0..DIFF_COLS.len(), any::<bool>()).prop_map(|(c, n)| RExpr::IsNull(c, n));
    let in_list = (0..DIFF_COLS.len()).prop_flat_map(|c| {
        let item = match DIFF_COLS[c].1 {
            // Integer lists only: mixing decimals compares as f64 (lossy).
            ColType::I64 => prop::sample::select(vec!["5", "100", "-3", "0", "7"])
                .prop_map(|t| Lit::Num(t.to_owned()))
                .boxed(),
            _ => lit_for(c, "=="),
        };
        let null = Just(Lit::Null);
        prop::collection::vec(prop_oneof![9 => item, 1 => null], 1..12)
            .prop_map(move |l| RExpr::In(c, l))
    });
    prop_oneof![6 => cmp, 1 => is_null, 2 => in_list]
}

fn expr() -> impl Strategy<Value = RExpr> {
    leaf().prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 2..4).prop_map(RExpr::And),
            prop::collection::vec(inner.clone(), 2..4).prop_map(RExpr::Or),
            inner.prop_map(|e| RExpr::Not(Box::new(e))),
        ]
    })
}

/// A record: one value per column, truncated to `len` fields (≥ 1).
fn record() -> impl Strategy<Value = Vec<&'static str>> {
    (
        prop::sample::select(S_VALUES.to_vec()),
        prop::sample::select(I_VALUES.to_vec()),
        prop::sample::select(F_VALUES.to_vec()),
        prop::sample::select(D_VALUES.to_vec()),
        prop::sample::select(B_VALUES.to_vec()),
        1usize..=6,
    )
        .prop_map(|(s, i, f, d, b, len)| {
            let mut v = vec![s, i, f, d, b];
            v.truncate(len.min(5));
            v
        })
}

// ---- reference evaluator ----

fn ref_null(v: Option<&str>) -> bool {
    match v {
        None => true,
        Some(v) => ["", "NULL", "null", "NA", "N/A", "\\N"].contains(&v),
    }
}

fn ref_date(v: &str) -> Option<(i32, u32, u32)> {
    let parts: Vec<&str> = v.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    ((1..=12).contains(&m) && d >= 1 && d <= days[m as usize - 1]).then_some((y, m, d))
}

fn ref_bool(v: &str) -> Option<bool> {
    match v.to_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn ref_f64(v: &str) -> Option<f64> {
    // `str::parse` also accepts inf / nan, which the pools never contain.
    v.parse::<f64>().ok()
}

fn holds(op: &str, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        "==" => o == Equal,
        "!=" => o != Equal,
        "<" => o == Less,
        "<=" => o != Greater,
        ">" => o == Greater,
        ">=" => o != Less,
        _ => unreachable!(),
    }
}

/// Ordering of `field` vs `lit` for column `col`, or `None` if the field does
/// not parse.
fn ref_order(col: usize, v: &str, lit: &Lit) -> Option<std::cmp::Ordering> {
    let text = lit.text();
    match DIFF_COLS[col].1 {
        ColType::I64 => {
            let x: i64 = v.parse().ok()?;
            match text.parse::<i64>() {
                Ok(y) => Some(x.cmp(&y)),
                Err(_) => (x as f64).partial_cmp(&text.parse::<f64>().unwrap()),
            }
        }
        ColType::F64 => ref_f64(v)?.partial_cmp(&text.parse::<f64>().unwrap()),
        ColType::Date => Some(ref_date(v)?.cmp(&ref_date(&text).unwrap())),
        ColType::Bool => {
            let y = match lit {
                Lit::Bool(b) => *b,
                _ => ref_bool(&text).unwrap(),
            };
            Some(ref_bool(v)?.cmp(&y))
        }
        _ => Some(v.as_bytes().cmp(text.as_bytes())),
    }
}

fn ref_eval(e: &RExpr, rec: &[&str]) -> bool {
    let get = |c: usize| rec.get(c).copied();
    match e {
        RExpr::And(v) => v.iter().all(|e| ref_eval(e, rec)),
        RExpr::Or(v) => v.iter().any(|e| ref_eval(e, rec)),
        RExpr::Not(e) => !ref_eval(e, rec),
        RExpr::IsNull(c, neg) => ref_null(get(*c)) != *neg,
        RExpr::In(c, list) => {
            let v = get(*c);
            if ref_null(v) {
                return list.iter().any(|l| matches!(l, Lit::Null));
            }
            let v = v.unwrap();
            list.iter().any(|l| match l {
                Lit::Null => false,
                Lit::Str(s, true) => v.to_lowercase() == s.to_lowercase(),
                l => ref_order(*c, v, l) == Some(std::cmp::Ordering::Equal),
            })
        }
        RExpr::Cmp(c, op, lit) => {
            let v = get(*c);
            if ref_null(v) {
                return *op == "!=";
            }
            let v = v.unwrap();
            let ci = matches!(lit, Lit::Str(_, true));
            let fold = |s: &str| if ci { s.to_lowercase() } else { s.to_owned() };
            let (fv, ft) = (fold(v), fold(&lit.text()));
            match *op {
                "contains" => fv.contains(&ft),
                "starts" => fv.starts_with(&ft),
                "ends" => fv.ends_with(&ft),
                op if ci => (fv == ft) == (op == "=="),
                op => match ref_order(*c, v, lit) {
                    Some(o) => holds(op, o),
                    None => op == "!=",
                },
            }
        }
    }
}

fn csv_field(v: &str) -> String {
    if v.contains(',') || v.contains('"') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_owned()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn differential(e in expr(), records in prop::collection::vec(record(), 1..24)) {
        let cols = columns(&DIFF_COLS);
        let q = e.render();
        let p = compile_with(&q, &cols, &Dialect::default())
            .unwrap_or_else(|err| panic!("{q}: {err}"));
        let mut text = String::new();
        for r in &records {
            let line: Vec<String> = r.iter().map(|v| csv_field(v)).collect();
            text.push_str(&line.join(","));
            text.push('\n');
        }
        let bytes = text.as_bytes();
        let mut parser = RecordParser::new(&Dialect::default());
        let mut rec = RecordRanges::default();
        let mut scratch = EvalScratch::new();
        let mut pos = 0u64;
        for r in &records {
            // A record whose only field is empty is a blank line: not a row.
            if r.len() == 1 && r[0].is_empty() {
                pos += 1;
                continue;
            }
            let ParseOutcome::Record { next } = parser.parse_at(bytes, pos, &mut rec) else {
                panic!("expected a record");
            };
            pos = next;
            let got = p.eval_record(bytes, &rec, &mut scratch);
            let want = ref_eval(&e, r);
            prop_assert_eq!(got, want, "query `{}` on record {:?}", q, r);
        }
    }
}
