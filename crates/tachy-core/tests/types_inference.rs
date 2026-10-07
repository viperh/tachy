//! Type inference on synthetic sample columns (M3-01, spec §7.1).

use tachy_core::{
    column::{ColumnMeta, ColumnName},
    types::{ColType, NullSet, infer, parse_date, parse_i64},
};

fn infer_strs(values: &[String]) -> ColType {
    infer(values.iter().map(|v| v.as_bytes()), &NullSet::default())
}

fn strs(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

/// `n` values from `good(i)`, with `bad` of them replaced by garbage.
fn column(n: usize, bad: usize, good: impl Fn(usize) -> String) -> Vec<String> {
    (0..n)
        .map(|i| {
            if i < bad {
                format!("garbage-{i}")
            } else {
                good(i)
            }
        })
        .collect()
}

#[test]
fn each_type_on_fixture_columns() {
    let cases: Vec<(Vec<String>, ColType)> = vec![
        (
            strs(&["true", "False", "YES", "no", "1", "0"]),
            ColType::Bool,
        ),
        (strs(&["0", "1", "1", "0"]), ColType::Bool),
        (
            strs(&["-5", "+12", "007", "9223372036854775807"]),
            ColType::I64,
        ),
        (strs(&["1.5", "2", "-3e4", ".5", "5."]), ColType::F64),
        (strs(&["1", "2", "9223372036854775808"]), ColType::F64),
        (
            strs(&["2024-02-29", "1999-12-31", "2026-01-01"]),
            ColType::Date,
        ),
        (
            strs(&[
                "2026-03-01T10:00:00Z",
                "2026-03-01 10:00",
                "2026-03-01T10:00:00.123+05:30",
                "2026-03-01T10:00:00-0800",
            ]),
            ColType::DateTime,
        ),
        (
            (0..100)
                .map(|i| ["paid", "shipped", "refunded"][i % 3].to_owned())
                .collect(),
            ColType::Enum,
        ),
        (
            (0..100).map(|i| format!("customer {i}")).collect(),
            ColType::Str,
        ),
        // A mix of dates and datetimes matches neither at 99 %.
        (
            (0..100)
                .map(|i| {
                    let date = format!("2026-{:02}-{:02}", i % 12 + 1, i % 28 + 1);
                    if i % 2 == 0 {
                        date
                    } else {
                        format!("{date}T00:00")
                    }
                })
                .collect(),
            ColType::Str,
        ),
    ];
    for (values, want) in cases {
        assert_eq!(infer_strs(&values), want, "{values:?}");
    }
}

#[test]
fn garbage_tolerance() {
    // 0.5 % garbage: still numeric.
    assert_eq!(
        infer_strs(&column(1000, 5, |i| i.to_string())),
        ColType::I64
    );
    assert_eq!(
        infer_strs(&column(1000, 5, |i| format!("{i}.25"))),
        ColType::F64
    );
    // 2 % garbage: str.
    assert_eq!(
        infer_strs(&column(1000, 20, |i| i.to_string())),
        ColType::Str
    );
    assert_eq!(
        infer_strs(&column(1000, 20, |i| format!("{i}.25"))),
        ColType::Str
    );
}

#[test]
fn match_rate_boundaries() {
    let ints = |i: usize| (i * 7).to_string();
    // 100 %, 99 % (exactly at the threshold) and 98 %.
    assert_eq!(infer_strs(&column(10_000, 0, ints)), ColType::I64);
    assert_eq!(infer_strs(&column(10_000, 100, ints)), ColType::I64);
    assert_eq!(infer_strs(&column(10_000, 101, ints)), ColType::Str);
    assert_eq!(infer_strs(&column(10_000, 200, ints)), ColType::Str);
    // 84 distinct dates, so a failed match falls through to str, not enum.
    let dates = |i: usize| format!("2026-{:02}-{:02}", i % 12 + 1, i % 28 + 1);
    assert_eq!(infer_strs(&column(200, 2, dates)), ColType::Date);
    assert_eq!(infer_strs(&column(200, 4, dates)), ColType::Str);
}

#[test]
fn thousands_separators_are_not_integers() {
    assert_eq!(parse_i64(b"1,234"), None);
    assert_eq!(
        infer_strs(&(0..100).map(|i| format!("{i},234")).collect::<Vec<_>>()),
        ColType::Str
    );
}

#[test]
fn date_validation() {
    assert!(parse_date(b"2026-02-30").is_none());
    assert!(parse_date(b"2024-02-29").is_some());
}

#[test]
fn nulls_are_excluded() {
    // 50 nulls + 50 integers: 100 % of the non-null values match.
    let mut values: Vec<String> = (0..50).map(|i| i.to_string()).collect();
    for i in 0..50 {
        values.push(["", "NULL", "null", "NA", "N/A", "\\N"][i % 6].to_owned());
    }
    assert_eq!(infer_strs(&values), ColType::I64);
    // All null → str.
    assert_eq!(infer_strs(&strs(&["", "NA", "NULL"])), ColType::Str);
    // Custom null list.
    let custom = NullSet::new(["-"]);
    assert_eq!(
        infer(["1", "-", "2", "-"].map(str::as_bytes), &custom),
        ColType::I64
    );
}

#[test]
fn enum_guard_on_small_files() {
    // 10 rows, 10 distinct strings: str, not enum.
    let values: Vec<String> = (0..10).map(|i| format!("city {i}")).collect();
    assert_eq!(infer_strs(&values), ColType::Str);
    // 10 rows, 5 distinct (50 %): enum.
    let values: Vec<String> = (0..10).map(|i| format!("city {}", i % 5)).collect();
    assert_eq!(infer_strs(&values), ColType::Enum);
    // One distinct value is not an enum.
    assert_eq!(infer_strs(&strs(&["x"; 10])), ColType::Str);
    // 64 distinct is an enum, 65 is not.
    let values: Vec<String> = (0..1000).map(|i| format!("v{}", i % 64)).collect();
    assert_eq!(infer_strs(&values), ColType::Enum);
    let values: Vec<String> = (0..1000).map(|i| format!("v{}", i % 65)).collect();
    assert_eq!(infer_strs(&values), ColType::Str);
}

#[test]
fn override_survives_phase_two_reinference() {
    let mut meta = ColumnMeta::new(
        ColumnName {
            display: "flag".into(),
            query: "flag".into(),
        },
        0,
        false,
    );
    // Phase 1.
    meta.set_inferred(infer_strs(&strs(&["0", "1", "1"])));
    assert_eq!(meta.ty(), ColType::Bool);
    // `set type flag i64`.
    meta.type_override = Some(ColType::I64);
    // Phase 2 sees more rows and infers again.
    meta.set_inferred(infer_strs(&strs(&["0", "1", "2", "3"])));
    assert_eq!(meta.inferred, ColType::I64);
    meta.set_inferred(ColType::Str);
    assert_eq!(meta.ty(), ColType::I64);
}
