//! Column type inference, null detection and value parsing (spec §7.1).
//!
//! Everything here is pure: it works on unescaped field bytes and never
//! touches the file. The recognisers are shared by display (alignment, the type
//! label), query evaluation, sort-key extraction and statistics, so they never
//! allocate.
//!
//! - [`ColType`]: the seven column types and their labels.
//! - [`NullSet`]: the configured null spellings (`settings.null_values`, §15).
//! - Recognisers: [`parse_bool`], [`parse_i64`], [`parse_f64`], [`parse_date`],
//!   [`parse_datetime`].
//! - [`Value`] and [`parse_value`]: a field parsed according to a column type.
//! - Hand-written calendar math: [`days_from_civil`], [`civil_from_days`],
//!   [`format_date`], [`format_datetime`].
//! - Inference: [`infer`] and the incremental [`TypeInference`].
//!
//! The sample itself is collected by [`crate::sample`]; the UI wiring
//! (`Msg::SampleReady`) lives in the `tachy` crate.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};

/// The display and comparison type of a column (spec §7.1).
///
/// Types affect display (alignment, the header label) and query comparisons
/// only, never the stored data.
///
/// The variant order is the inference precedence: when several types match a
/// column, the earliest one wins.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ColType {
    /// `true/false`, `yes/no`, `0/1`, ASCII case-insensitive.
    Bool,
    /// A signed 64-bit integer.
    I64,
    /// A decimal or scientific-notation number.
    F64,
    /// An ISO 8601 `YYYY-MM-DD` date.
    Date,
    /// An ISO 8601 date and time, normalised to UTC.
    DateTime,
    /// A string column with few distinct values.
    Enum,
    /// Anything else.
    Str,
}

impl ColType {
    /// Every type, in inference precedence order.
    pub const ALL: [ColType; 7] = [
        ColType::Bool,
        ColType::I64,
        ColType::F64,
        ColType::Date,
        ColType::DateTime,
        ColType::Enum,
        ColType::Str,
    ];

    /// The label shown on header line 2: `bool`, `i64`, `f64`, `date`,
    /// `datetime`, `enum` or `str`.
    pub const fn label(self) -> &'static str {
        match self {
            ColType::Bool => "bool",
            ColType::I64 => "i64",
            ColType::F64 => "f64",
            ColType::Date => "date",
            ColType::DateTime => "datetime",
            ColType::Enum => "enum",
            ColType::Str => "str",
        }
    }

    /// Parses a label as written by [`ColType::label`] (used by `set type`).
    /// The match is exact (lowercase).
    pub fn from_label(label: &str) -> Option<ColType> {
        ColType::ALL.into_iter().find(|t| t.label() == label)
    }

    /// `true` for `i64` and `f64`.
    pub const fn is_numeric(self) -> bool {
        matches!(self, ColType::I64 | ColType::F64)
    }

    /// `true` for `date` and `datetime`.
    pub const fn is_temporal(self) -> bool {
        matches!(self, ColType::Date | ColType::DateTime)
    }
}

impl fmt::Display for ColType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// Nulls
// ---------------------------------------------------------------------------

/// The null spellings used when `settings.null_values` is not set (§7.1, §15).
pub const DEFAULT_NULL_VALUES: [&str; 6] = ["", "NULL", "null", "NA", "N/A", "\\N"];

/// A precomputed set of null spellings (spec §7.1, `null_values` in §15).
///
/// A field is null when its **unescaped** bytes equal one of the spellings
/// exactly (case-sensitive). [`NullSet::is_null`] runs in every filter, sort
/// and profile, so it neither allocates nor hashes: the empty spelling is a
/// flag, and the others are a small sorted list that is only searched when the
/// value is no longer than the longest spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NullSet {
    empty: bool,
    values: Vec<Box<[u8]>>,
    max_len: usize,
}

impl NullSet {
    /// Builds a set from the given spellings. Duplicates are ignored.
    pub fn new<I, S>(values: I) -> NullSet
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        let mut empty = false;
        let mut list: Vec<Box<[u8]>> = Vec::new();
        for v in values {
            let v = v.as_ref();
            if v.is_empty() {
                empty = true;
            } else {
                list.push(v.into());
            }
        }
        list.sort();
        list.dedup();
        let max_len = list.iter().map(|v| v.len()).max().unwrap_or(0);
        NullSet {
            empty,
            values: list,
            max_len,
        }
    }

    /// A set that treats nothing as null.
    pub fn none() -> NullSet {
        NullSet::new(std::iter::empty::<&[u8]>())
    }

    /// Whether `v` (unescaped field bytes) is a null.
    #[inline]
    pub fn is_null(&self, v: &[u8]) -> bool {
        if v.is_empty() {
            return self.empty;
        }
        if v.len() > self.max_len {
            return false;
        }
        self.values.binary_search_by(|x| (**x).cmp(v)).is_ok()
    }
}

impl Default for NullSet {
    /// The §7.1 defaults: empty, `NULL`, `null`, `NA`, `N/A`, `\N`.
    fn default() -> NullSet {
        NullSet::new(DEFAULT_NULL_VALUES)
    }
}

// ---------------------------------------------------------------------------
// Recognisers
// ---------------------------------------------------------------------------

/// Recognises `true/false`, `yes/no` and `0/1`, ASCII case-insensitive.
pub fn parse_bool(v: &[u8]) -> Option<bool> {
    match v.len() {
        1 => match v[0] {
            b'1' => Some(true),
            b'0' => Some(false),
            _ => None,
        },
        2 if v.eq_ignore_ascii_case(b"no") => Some(false),
        3 if v.eq_ignore_ascii_case(b"yes") => Some(true),
        4 if v.eq_ignore_ascii_case(b"true") => Some(true),
        5 if v.eq_ignore_ascii_case(b"false") => Some(false),
        _ => None,
    }
}

/// Recognises `[+-]?[0-9]+` that fits in an `i64`.
///
/// Leading zeros are allowed (`007`). Thousands separators are not (`1,234`
/// is not an integer), and neither is surrounding whitespace.
pub fn parse_i64(v: &[u8]) -> Option<i64> {
    let (neg, digits) = match v.first()? {
        b'-' => (true, &v[1..]),
        b'+' => (false, &v[1..]),
        _ => (false, v),
    };
    if digits.is_empty() {
        return None;
    }
    // Accumulate negatively so that i64::MIN parses.
    let mut acc: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_sub(i64::from(b - b'0'))?;
    }
    if neg { Some(acc) } else { acc.checked_neg() }
}

/// Recognises `[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?`.
///
/// `inf`, `nan` and hexadecimal are rejected. A value too large for `f64`
/// (`1e400`) matches the pattern and parses to infinity.
pub fn parse_f64(v: &[u8]) -> Option<f64> {
    if !is_f64_syntax(v) {
        return None;
    }
    // The syntax check guarantees ASCII.
    std::str::from_utf8(v).ok()?.parse::<f64>().ok()
}

fn is_f64_syntax(v: &[u8]) -> bool {
    let mut i = 0;
    if matches!(v.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while i < v.len() && v[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < v.len() && v[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < v.len() && v[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < v.len() && matches!(v[i], b'e' | b'E') {
        i += 1;
        if i < v.len() && matches!(v[i], b'+' | b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < v.len() && v[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == v.len()
}

/// Parses a field of ASCII digits of the expected length.
fn digits(v: &[u8], len: usize) -> Option<u32> {
    if v.len() != len {
        return None;
    }
    let mut acc = 0u32;
    for &b in v {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + u32::from(b - b'0');
    }
    Some(acc)
}

/// Whether `year` is a Gregorian leap year.
pub const fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in `month` (1–12) of `year`. Returns 0 for an invalid month.
pub const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Recognises an ISO 8601 `YYYY-MM-DD` date and returns days since
/// 1970-01-01.
///
/// The month must be 1–12 and the day valid for that month, including leap
/// years (`2024-02-29` is valid, `1900-02-29` and `2026-02-30` are not).
pub fn parse_date(v: &[u8]) -> Option<i32> {
    if v.len() != 10 || v[4] != b'-' || v[7] != b'-' {
        return None;
    }
    let year = i64::from(digits(&v[0..4], 4)?);
    let month = digits(&v[5..7], 2)?;
    let day = digits(&v[8..10], 2)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    // Years 0000–9999 always fit in an i32 day count.
    Some(days_from_civil(year, month, day) as i32)
}

/// Microseconds per day.
const US_PER_DAY: i64 = 86_400_000_000;

/// Recognises an ISO 8601 date-time and returns microseconds since the Unix
/// epoch, normalised to UTC.
///
/// Accepted form: a [`parse_date`] date, then `T` or a space, then
/// `HH:MM[:SS[.f{1,9}]]`, then an optional zone `Z`, `±HH:MM` or `±HHMM`.
/// Hours are 0–23, minutes and seconds 0–59 (no leap seconds). Fractions
/// beyond microseconds are truncated.
///
/// **Values without a zone are treated as UTC.**
pub fn parse_datetime(v: &[u8]) -> Option<i64> {
    if v.len() < 16 {
        return None;
    }
    let days = i64::from(parse_date(&v[..10])?);
    if !matches!(v[10], b'T' | b' ') || v[13] != b':' {
        return None;
    }
    let hour = digits(&v[11..13], 2)?;
    let minute = digits(&v[14..16], 2)?;
    if hour > 23 || minute > 59 {
        return None;
    }
    let mut i = 16;
    let mut second = 0;
    let mut micros = 0i64;
    if v.get(i) == Some(&b':') {
        second = digits(v.get(i + 1..i + 3)?, 2)?;
        if second > 59 {
            return None;
        }
        i += 3;
        if v.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while i < v.len() && v[i].is_ascii_digit() {
                i += 1;
            }
            let n = i - start;
            if !(1..=9).contains(&n) {
                return None;
            }
            for k in 0..6 {
                let d = if k < n {
                    i64::from(v[start + k] - b'0')
                } else {
                    0
                };
                micros = micros * 10 + d;
            }
        }
    }
    let offset_minutes: i64 = match &v[i..] {
        [] | [b'Z'] => 0,
        [sign @ (b'+' | b'-'), rest @ ..] => {
            let (h, m) = match rest {
                [h1, h2, b':', m1, m2] | [h1, h2, m1, m2] => {
                    (digits(&[*h1, *h2], 2)?, digits(&[*m1, *m2], 2)?)
                }
                _ => return None,
            };
            if h > 23 || m > 59 {
                return None;
            }
            let total = i64::from(h * 60 + m);
            if *sign == b'-' { -total } else { total }
        }
        _ => return None,
    };
    let secs = i64::from(hour * 3600 + minute * 60 + second) - offset_minutes * 60;
    Some(days * US_PER_DAY + secs * 1_000_000 + micros)
}

// ---------------------------------------------------------------------------
// Calendar math (Howard Hinnant's algorithms, proleptic Gregorian calendar)
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// `month` is 1–12 and `day` 1–31; the caller validates them.
pub const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = ((month + 9) % 12) as i64; // March = 0
    let doy = (153 * mp + 2) / 5 + day as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The `(year, month, day)` of a day count since 1970-01-01. Inverse of
/// [`days_from_civil`].
pub const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Formats days since 1970-01-01 as `YYYY-MM-DD`.
///
/// Years outside 0000–9999 (which [`parse_date`] never produces) are printed
/// with a sign or extra digits.
pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(i64::from(days));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats microseconds since the epoch as `YYYY-MM-DDTHH:MM:SS[.ffffff]Z`
/// (UTC). The fraction is omitted when zero and trimmed of trailing zeros.
pub fn format_datetime(micros: i64) -> String {
    let days = micros.div_euclid(US_PER_DAY);
    let rem = micros.rem_euclid(US_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    let secs = rem / 1_000_000;
    let frac = rem % 1_000_000;
    let (h, mi, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut out = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    if frac != 0 {
        let f = format!("{frac:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A field parsed according to a column type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value<'a> {
    /// The field is one of the [`NullSet`] spellings.
    Null,
    /// A `bool` column value.
    Bool(bool),
    /// An `i64` column value.
    I64(i64),
    /// An `f64` column value.
    F64(f64),
    /// A `date` column value: days since 1970-01-01.
    Date(i32),
    /// A `datetime` column value: microseconds since the epoch, UTC.
    DateTime(i64),
    /// An `enum` or `str` column value: the raw bytes.
    Bytes(&'a [u8]),
    /// The field does not parse as the column's type.
    Invalid,
}

/// Parses `v` (unescaped field bytes) as type `t`. Nulls are checked first.
pub fn parse_value<'a>(t: ColType, v: &'a [u8], nulls: &NullSet) -> Value<'a> {
    if nulls.is_null(v) {
        return Value::Null;
    }
    let parsed = match t {
        ColType::Bool => parse_bool(v).map(Value::Bool),
        ColType::I64 => parse_i64(v).map(Value::I64),
        ColType::F64 => parse_f64(v).map(Value::F64),
        ColType::Date => parse_date(v).map(Value::Date),
        ColType::DateTime => parse_datetime(v).map(Value::DateTime),
        ColType::Enum | ColType::Str => Some(Value::Bytes(v)),
    };
    parsed.unwrap_or(Value::Invalid)
}

// ---------------------------------------------------------------------------
// Inference
// ---------------------------------------------------------------------------

/// Most distinct values an `enum` column may have in the sample (§7.1).
pub const ENUM_MAX_DISTINCT: usize = 64;

/// Incremental type inference over one column of a sample (spec §7.1).
///
/// Rules:
/// - A type matches when it parses **at least 99 %** of the **non-null**
///   values. Nulls (per [`NullSet`]) are excluded from the calculation.
/// - When several types match, the precedence is `bool` > `i64` > `f64` >
///   `date` > `datetime` > `enum` > `str`. A column holding only `0` and `1`
///   is therefore `bool` (§7.1 lists `0/1` under bool); `set type` overrides
///   it. `i64` values also match `f64`, and `i64` wins. `date` values do not
///   match `datetime`, which needs a time.
/// - `enum` needs at least 2 distinct values, at most 64 **and** at most 50 %
///   of the non-null sample size. The 50 % guard is not in §7.1; without it a
///   10-row file would make every column an enum.
/// - A column with no non-null values is `str`.
#[derive(Clone, Debug, Default)]
pub struct TypeInference {
    non_null: u64,
    bools: u64,
    ints: u64,
    floats: u64,
    dates: u64,
    datetimes: u64,
    /// Distinct non-null values, capped at `ENUM_MAX_DISTINCT + 1` entries.
    distinct: HashSet<Box<[u8]>>,
}

impl TypeInference {
    /// An empty accumulator.
    pub fn new() -> TypeInference {
        TypeInference::default()
    }

    /// Adds one field (unescaped bytes).
    pub fn push(&mut self, v: &[u8], nulls: &NullSet) {
        if nulls.is_null(v) {
            return;
        }
        self.non_null += 1;
        self.bools += u64::from(parse_bool(v).is_some());
        self.ints += u64::from(parse_i64(v).is_some());
        self.floats += u64::from(parse_f64(v).is_some());
        self.dates += u64::from(parse_date(v).is_some());
        self.datetimes += u64::from(parse_datetime(v).is_some());
        if self.distinct.len() <= ENUM_MAX_DISTINCT && !self.distinct.contains(v) {
            self.distinct.insert(v.into());
        }
    }

    /// Number of non-null values seen.
    pub fn non_null(&self) -> u64 {
        self.non_null
    }

    /// The inferred type for the values pushed so far.
    pub fn infer(&self) -> ColType {
        let n = self.non_null;
        if n == 0 {
            return ColType::Str;
        }
        let matches = |count: u64| u128::from(count) * 100 >= u128::from(n) * 99;
        if matches(self.bools) {
            ColType::Bool
        } else if matches(self.ints) {
            ColType::I64
        } else if matches(self.floats) {
            ColType::F64
        } else if matches(self.dates) {
            ColType::Date
        } else if matches(self.datetimes) {
            ColType::DateTime
        } else {
            let d = self.distinct.len();
            if (2..=ENUM_MAX_DISTINCT).contains(&d) && (d as u64) * 2 <= n {
                ColType::Enum
            } else {
                ColType::Str
            }
        }
    }
}

/// Infers the type of one sample column (see [`TypeInference`] for the rules).
pub fn infer<'a, I>(values: I, nulls: &NullSet) -> ColType
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut acc = TypeInference::new();
    for v in values {
        acc.push(v, nulls);
    }
    acc.infer()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip() {
        for t in ColType::ALL {
            assert_eq!(ColType::from_label(t.label()), Some(t));
            assert_eq!(t.to_string(), t.label());
        }
        assert_eq!(ColType::from_label("I64"), None);
        assert_eq!(ColType::from_label("string"), None);
    }

    #[test]
    fn null_set() {
        let n = NullSet::default();
        for v in DEFAULT_NULL_VALUES {
            assert!(n.is_null(v.as_bytes()), "{v:?}");
        }
        for v in ["Null", "nul", "NULLS", "n/a", " ", "x"] {
            assert!(!n.is_null(v.as_bytes()), "{v:?}");
        }
        let custom = NullSet::new(["-"]);
        assert!(custom.is_null(b"-"));
        assert!(!custom.is_null(b""));
        assert!(!NullSet::none().is_null(b""));
    }

    #[test]
    fn bools() {
        let cases: &[(&str, Option<bool>)] = &[
            ("true", Some(true)),
            ("TRUE", Some(true)),
            ("True", Some(true)),
            ("false", Some(false)),
            ("FaLsE", Some(false)),
            ("yes", Some(true)),
            ("YES", Some(true)),
            ("no", Some(false)),
            ("No", Some(false)),
            ("1", Some(true)),
            ("0", Some(false)),
            ("2", None),
            ("01", None),
            ("t", None),
            ("y", None),
            ("", None),
            (" true", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_bool(input.as_bytes()), *want, "{input:?}");
        }
    }

    #[test]
    fn integers() {
        let cases: &[(&str, Option<i64>)] = &[
            ("0", Some(0)),
            ("-0", Some(0)),
            ("+5", Some(5)),
            ("-17", Some(-17)),
            ("007", Some(7)),
            ("9223372036854775807", Some(i64::MAX)),
            ("-9223372036854775808", Some(i64::MIN)),
            ("9223372036854775808", None),
            ("-9223372036854775809", None),
            ("1,234", None),
            ("1_000", None),
            ("1.0", None),
            ("1e3", None),
            ("", None),
            ("+", None),
            ("-", None),
            (" 1", None),
            ("1 ", None),
            ("0x10", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_i64(input.as_bytes()), *want, "{input:?}");
        }
    }

    #[test]
    fn floats() {
        let cases: &[(&str, Option<f64>)] = &[
            ("0", Some(0.0)),
            ("1.5", Some(1.5)),
            ("-2.25", Some(-2.25)),
            ("+5", Some(5.0)),
            ("1e10", Some(1e10)),
            ("1E-3", Some(1e-3)),
            ("2.5e+2", Some(250.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("-.5", Some(-0.5)),
            ("9223372036854775808", Some(9_223_372_036_854_775_808.0)),
            (".", None),
            ("e5", None),
            ("1e", None),
            ("1e+", None),
            ("inf", None),
            ("-inf", None),
            ("NaN", None),
            ("nan", None),
            ("infinity", None),
            ("0x1p3", None),
            ("1,5", None),
            ("1.2.3", None),
            ("", None),
            ("-", None),
            (" 1.0", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_f64(input.as_bytes()), *want, "{input:?}");
        }
        assert!(parse_f64(b"-0").unwrap().is_sign_negative());
    }

    #[test]
    fn i64_max_plus_one_is_f64_not_i64() {
        let v = b"9223372036854775808";
        assert_eq!(parse_i64(v), None);
        assert!(parse_f64(v).is_some());
        assert_eq!(infer([&v[..]], &NullSet::default()), ColType::F64);
    }

    #[test]
    fn dates() {
        let cases: &[(&str, Option<i32>)] = &[
            ("1970-01-01", Some(0)),
            ("1970-01-02", Some(1)),
            ("1969-12-31", Some(-1)),
            ("2000-03-01", Some(11_017)),
            ("2024-02-29", Some(19_782)),
            ("2000-02-29", Some(11_016)),
            ("0000-01-01", Some(-719_528)),
            ("9999-12-31", Some(2_932_896)),
            ("2026-02-30", None),
            ("2023-02-29", None),
            ("1900-02-29", None),
            ("2026-04-31", None),
            ("2026-13-01", None),
            ("2026-00-10", None),
            ("2026-01-00", None),
            ("2026-1-01", None),
            ("26-01-01", None),
            ("2026/01/01", None),
            ("2026-01-01T00:00", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_date(input.as_bytes()), *want, "{input:?}");
        }
    }

    #[test]
    fn datetimes() {
        let day = US_PER_DAY;
        let h = 3_600_000_000i64;
        let m = 60_000_000i64;
        let cases: &[(&str, Option<i64>)] = &[
            ("1970-01-01T00:00", Some(0)),
            ("1970-01-01 00:00", Some(0)),
            ("1970-01-01T00:00:00Z", Some(0)),
            ("1970-01-02T01:02:03", Some(day + h + 2 * m + 3_000_000)),
            ("1970-01-01T00:00:00.5", Some(500_000)),
            ("1970-01-01T00:00:00.123456", Some(123_456)),
            ("1970-01-01T00:00:00.123456789", Some(123_456)),
            ("1970-01-01T00:00:00.000001Z", Some(1)),
            ("1970-01-01T05:30+05:30", Some(0)),
            ("1970-01-01T05:30:00+05:30", Some(0)),
            ("1969-12-31T16:00:00-0800", Some(0)),
            ("1969-12-31T16:00-08:00", Some(0)),
            ("1970-01-01T00:00:00+0000", Some(0)),
            ("2024-02-29T23:59:59Z", Some(19_782 * day + 86_399_000_000)),
            ("1970-01-01T24:00", None),
            ("1970-01-01T23:60", None),
            ("1970-01-01T23:59:60", None),
            ("1970-01-01T00", None),
            ("1970-01-01T00:00:00.", None),
            ("1970-01-01T00:00:00.1234567890", None),
            ("1970-01-01T00:00.5", None),
            ("1970-01-01T00:00:00+05", None),
            ("1970-01-01T00:00:00+5:30", None),
            ("1970-01-01T00:00:00+24:00", None),
            ("1970-01-01T00:00:00 Z", None),
            ("1970-01-01T00:00:00z", None),
            ("1970-01-01X00:00", None),
            ("1970-01-01", None),
            ("2026-02-30T00:00", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_datetime(input.as_bytes()), *want, "{input:?}");
        }
    }

    #[test]
    fn civil_round_trip_over_twenty_thousand_years() {
        let start = days_from_civil(-10_000, 1, 1);
        let end = days_from_civil(10_000, 12, 31);
        let (mut y, mut m, mut d) = (-10_000i64, 1u32, 1u32);
        for days in start..=end {
            assert_eq!(civil_from_days(days), (y, m, d), "day {days}");
            assert_eq!(days_from_civil(y, m, d), days);
            d += 1;
            if d > days_in_month(y, m) {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
        }
        assert_eq!((y, m, d), (10_001, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(19_782), "2024-02-29");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_datetime(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_datetime(-1), "1969-12-31T23:59:59.999999Z");
        assert_eq!(format_datetime(500_000), "1970-01-01T00:00:00.5Z");
        for s in [
            "2024-02-29T23:59:59Z",
            "1999-12-31T00:00:01.25Z",
            "0001-01-01T00:00:00Z",
        ] {
            let us = parse_datetime(s.as_bytes()).unwrap();
            assert_eq!(format_datetime(us), s);
        }
        for s in ["2024-02-29", "0000-03-01", "9999-12-31"] {
            assert_eq!(format_date(parse_date(s.as_bytes()).unwrap()), s);
        }
    }

    #[test]
    fn parse_value_by_type() {
        let n = NullSet::default();
        assert_eq!(parse_value(ColType::I64, b"NA", &n), Value::Null);
        assert_eq!(parse_value(ColType::Str, b"", &n), Value::Null);
        assert_eq!(parse_value(ColType::Bool, b"Yes", &n), Value::Bool(true));
        assert_eq!(parse_value(ColType::I64, b"-3", &n), Value::I64(-3));
        assert_eq!(parse_value(ColType::I64, b"1.5", &n), Value::Invalid);
        assert_eq!(parse_value(ColType::F64, b"1.5", &n), Value::F64(1.5));
        assert_eq!(
            parse_value(ColType::Date, b"1970-01-02", &n),
            Value::Date(1)
        );
        assert_eq!(
            parse_value(ColType::DateTime, b"1970-01-01T00:00:01Z", &n),
            Value::DateTime(1_000_000)
        );
        assert_eq!(parse_value(ColType::Enum, b"x", &n), Value::Bytes(b"x"));
        assert_eq!(
            parse_value(ColType::Str, b"null!", &n),
            Value::Bytes(b"null!")
        );
    }
}
