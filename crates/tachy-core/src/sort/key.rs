//! Sort-key encoding (spec §8.4, M5-03 step 1).
//!
//! Each row becomes a 24-byte [`SortRecord`]: a 16-byte **prefix**
//! (`[flag][15 key bytes]`) compared with `memcmp`, then the row id.
//!
//! - `flag`: `0` = a value, `1` = null or unparseable. It is never inverted,
//!   so nulls sort **last in both directions**.
//! - `i64` → `(x as u64) ^ (1 << 63)`, big-endian. `datetime` likewise.
//! - `f64` → the order-preserving bit transform (sign set: flip all bits;
//!   otherwise flip the sign bit), big-endian; `-0.0` is normalised to `0.0`.
//! - `date` → `(d as u32) ^ (1 << 31)`, big-endian. `bool` → `0` / `1`.
//! - `str` / `enum` → the first **14** bytes of the value, zero-padded, then
//!   one **length byte**: the value's length when it is at most 14 bytes,
//!   `15` when it is longer (truncated). With `ci` the value is lowercased
//!   first (ASCII fast path; Unicode `to_lowercase` of the decoded value
//!   otherwise, so prefix order always agrees with the full comparison).
//! - Descending: the 15 key bytes are bitwise-NOTed (not the flag, not the
//!   row id).
//!
//! **Deviation from M5-03** (15 raw key bytes): the length byte makes equal
//! prefixes mean "equal values" for every string of at most 14 bytes, even
//! with NUL bytes or trailing-zero ambiguity (`"ab"` vs `"ab\0"`). Only two
//! truncated strings with the same 14-byte start need the full values from
//! the mmap. Without it every tie of a short string (an `enum` column: almost
//! all comparisons) would read the file twice.
//!
//! **Multi-key sorts**: the record holds the first key only. Ties on the
//! first key are resolved by reading the remaining keys from the mmap. This
//! keeps records at 24 bytes, which is what the disk estimate (§8.4,
//! 24 B/row) assumes.

use std::cmp::Ordering;

use crate::{
    dialect::Encoding,
    parse::decode_field,
    query::eval::{lower_into, norm_f64},
    types::{ColType, NullSet, Value, parse_value},
};

/// Bytes of a record on disk: prefix + little-endian row id.
pub const RECORD_BYTES: usize = 24;
/// Prefix bytes: flag + 15 key bytes.
pub const PREFIX_BYTES: usize = 16;
/// String bytes kept in the prefix.
pub const STRING_PREFIX: usize = 14;
/// Length byte value of a truncated string.
const TRUNCATED: u8 = 15;

/// One row of the sort: the encoded first key and the row id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortRecord {
    /// `[flag][15 key bytes]`.
    pub prefix: [u8; PREFIX_BYTES],
    /// The row id (written little-endian on disk).
    pub row_id: u64,
}

impl SortRecord {
    /// The on-disk form: prefix, then the row id little-endian.
    pub fn to_bytes(&self) -> [u8; RECORD_BYTES] {
        let mut out = [0u8; RECORD_BYTES];
        out[..PREFIX_BYTES].copy_from_slice(&self.prefix);
        out[PREFIX_BYTES..].copy_from_slice(&self.row_id.to_le_bytes());
        out
    }

    /// Inverse of [`SortRecord::to_bytes`].
    pub fn from_bytes(b: &[u8; RECORD_BYTES]) -> SortRecord {
        let mut prefix = [0u8; PREFIX_BYTES];
        prefix.copy_from_slice(&b[..PREFIX_BYTES]);
        let mut id = [0u8; 8];
        id.copy_from_slice(&b[PREFIX_BYTES..]);
        SortRecord {
            prefix,
            row_id: u64::from_le_bytes(id),
        }
    }

    /// Null or unparseable first key.
    pub fn is_null(&self) -> bool {
        self.prefix[0] == 1
    }
}

/// How one key column is compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeySpec {
    /// Field position (`ColumnMeta::source_index`).
    pub field: usize,
    /// Effective column type.
    pub ty: ColType,
    /// Descending.
    pub descending: bool,
    /// Case-folded string comparison.
    pub ci: bool,
}

impl KeySpec {
    /// Whether the record prefix of this key can be truncated (strings).
    pub fn is_string(&self) -> bool {
        matches!(self.ty, ColType::Str | ColType::Enum)
    }

    /// Whether `prefix` (encoded with this spec) holds a truncated string, so
    /// equal prefixes don't mean equal values.
    pub fn is_truncated(&self, prefix: &[u8; PREFIX_BYTES]) -> bool {
        if !self.is_string() || prefix[0] != 0 {
            return false;
        }
        let len = prefix[PREFIX_BYTES - 1];
        let len = if self.descending { !len } else { len };
        len == TRUNCATED
    }
}

/// Encodes the first key of a row. `v` is the unescaped value, `None` for a
/// missing field. `text` is scratch for Unicode case folding.
pub fn encode_key(
    spec: &KeySpec,
    v: Option<&[u8]>,
    nulls: &NullSet,
    enc: Encoding,
    text: &mut String,
) -> [u8; PREFIX_BYTES] {
    let mut out = [0u8; PREFIX_BYTES];
    let parsed = match v {
        Some(v) => parse_value(spec.ty, v, nulls),
        None => Value::Null,
    };
    let key = &mut out[1..];
    match parsed {
        Value::Null | Value::Invalid => {
            out[0] = 1;
            return out;
        }
        Value::Bool(b) => key[0] = u8::from(b),
        Value::I64(x) => key[..8].copy_from_slice(&((x as u64) ^ (1 << 63)).to_be_bytes()),
        Value::DateTime(x) => key[..8].copy_from_slice(&((x as u64) ^ (1 << 63)).to_be_bytes()),
        Value::F64(x) => {
            let bits = norm_f64(x).to_bits();
            let t = if bits >> 63 == 1 {
                !bits
            } else {
                bits ^ (1 << 63)
            };
            key[..8].copy_from_slice(&t.to_be_bytes());
        }
        Value::Date(d) => key[..4].copy_from_slice(&((d as u32) ^ (1 << 31)).to_be_bytes()),
        Value::Bytes(b) => {
            let folded: &[u8] = if spec.ci {
                fold(b, enc, text);
                text.as_bytes()
            } else {
                b
            };
            let n = folded.len().min(STRING_PREFIX);
            key[..n].copy_from_slice(&folded[..n]);
            key[STRING_PREFIX] = length_byte(folded.len());
        }
    }
    if spec.descending {
        for b in key.iter_mut() {
            *b = !*b;
        }
    }
    out
}

fn length_byte(len: usize) -> u8 {
    if len <= STRING_PREFIX {
        len as u8
    } else {
        TRUNCATED
    }
}

/// Compares the values of one key read from two rows (`None` = missing
/// field), with the full values: typed, nulls and unparseable values last
/// in both directions, `descending` reversing the order of values.
pub fn compare_values(
    spec: &KeySpec,
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    nulls: &NullSet,
    enc: Encoding,
    text_a: &mut String,
    text_b: &mut String,
) -> Ordering {
    let pa = a.map_or(Value::Null, |v| parse_value(spec.ty, v, nulls));
    let pb = b.map_or(Value::Null, |v| parse_value(spec.ty, v, nulls));
    let null_a = matches!(pa, Value::Null | Value::Invalid);
    let null_b = matches!(pb, Value::Null | Value::Invalid);
    let o = match (null_a, null_b) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => match (pa, pb) {
            (Value::Bool(x), Value::Bool(y)) => x.cmp(&y),
            (Value::I64(x), Value::I64(y)) => x.cmp(&y),
            (Value::DateTime(x), Value::DateTime(y)) => x.cmp(&y),
            (Value::Date(x), Value::Date(y)) => x.cmp(&y),
            (Value::F64(x), Value::F64(y)) => norm_f64(x).total_cmp(&norm_f64(y)),
            (Value::Bytes(x), Value::Bytes(y)) if spec.ci => {
                fold(x, enc, text_a);
                fold(y, enc, text_b);
                text_a.as_bytes().cmp(text_b.as_bytes())
            }
            (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
            _ => Ordering::Equal,
        },
    };
    if spec.descending { o.reverse() } else { o }
}

/// Case folding consistent with [`encode_key`]: ASCII lowercase for ASCII
/// values, Unicode lowercase of the decoded value otherwise.
fn fold(v: &[u8], enc: Encoding, out: &mut String) {
    if v.is_ascii() {
        out.clear();
        out.push_str(&decode_field(v, enc));
        out.make_ascii_lowercase();
    } else {
        lower_into(v, enc, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(ty: ColType, descending: bool, ci: bool) -> KeySpec {
        KeySpec {
            field: 0,
            ty,
            descending,
            ci,
        }
    }

    fn enc(s: &KeySpec, v: &str) -> [u8; PREFIX_BYTES] {
        encode_key(
            s,
            Some(v.as_bytes()),
            &NullSet::default(),
            Encoding::Utf8,
            &mut String::new(),
        )
    }

    /// Asserts that the encodings of `values` (given in ascending order) are
    /// strictly ascending, and strictly descending with `descending`.
    fn ordered(ty: ColType, values: &[&str]) {
        for desc in [false, true] {
            let s = spec(ty, desc, false);
            let keys: Vec<_> = values.iter().map(|v| enc(&s, v)).collect();
            for (w, v) in keys.windows(2).zip(values.windows(2)) {
                if desc {
                    assert!(w[0] > w[1], "{ty:?} desc {v:?}");
                } else {
                    assert!(w[0] < w[1], "{ty:?} asc {v:?}");
                }
            }
        }
    }

    #[test]
    fn integers() {
        ordered(
            ColType::I64,
            &[
                "-9223372036854775808",
                "-100",
                "-1",
                "0",
                "1",
                "2",
                "10",
                "9223372036854775807",
            ],
        );
        assert_eq!(
            enc(&spec(ColType::I64, false, false), "-0"),
            enc(&spec(ColType::I64, false, false), "0")
        );
    }

    #[test]
    fn floats() {
        ordered(
            ColType::F64,
            &[
                "-1e308", "-1.5", "-1e-310", "0", "4.9e-324", "1e-310", "1", "2", "10", "1e308",
            ],
        );
        let s = spec(ColType::F64, false, false);
        assert_eq!(enc(&s, "-0.0"), enc(&s, "0"));
        assert_eq!(enc(&s, "nan")[0], 1, "unparseable");
    }

    #[test]
    fn dates_and_times() {
        ordered(
            ColType::Date,
            &["1600-01-01", "1969-12-31", "1970-01-01", "2026-03-01"],
        );
        ordered(
            ColType::DateTime,
            &[
                "1969-12-31T23:59:59Z",
                "1970-01-01T00:00:00Z",
                "2026-03-01 12:00:00+01:00",
                "2026-03-01T12:00:00Z",
            ],
        );
        ordered(ColType::Bool, &["false", "true"]);
    }

    #[test]
    fn strings() {
        ordered(
            ColType::Str,
            &[
                "A",
                "B",
                "a",
                "ab",
                "ab\0",
                "abc",
                "abcdefghijklm",
                "abcdefghijklmn",
                "abcdefghijklmnX",
                "b",
            ],
        );
        let s = spec(ColType::Str, false, false);
        let t = |v: &str| s.is_truncated(&enc(&s, v));
        assert!(!t("abcdefghijklmn"));
        assert!(t("abcdefghijklmno"));
        let d = spec(ColType::Str, true, false);
        assert!(d.is_truncated(&enc(&d, "abcdefghijklmno")));
        assert!(!d.is_truncated(&enc(&d, "abc")));
    }

    #[test]
    fn nulls_last_in_both_directions() {
        for desc in [false, true] {
            let s = spec(ColType::I64, desc, false);
            let null = enc(&s, "");
            let bad = enc(&s, "x");
            assert_eq!(null, bad);
            for v in ["-9223372036854775808", "0", "9223372036854775807"] {
                assert!(enc(&s, v) < null, "{v} desc={desc}");
            }
        }
    }

    #[test]
    fn case_folding() {
        let s = spec(ColType::Str, false, true);
        let k: Vec<_> = ["apple", "Banana", "cherry"]
            .iter()
            .map(|v| enc(&s, v))
            .collect();
        assert!(k[0] < k[1] && k[1] < k[2]);
        assert_eq!(enc(&s, "ABC"), enc(&s, "abc"));
        assert_eq!(enc(&s, "MÜLLER"), enc(&s, "müller"));
        let (mut a, mut b) = (String::new(), String::new());
        let n = NullSet::default();
        assert_eq!(
            compare_values(
                &s,
                Some("MÜLLER".as_bytes()),
                Some("müller".as_bytes()),
                &n,
                Encoding::Utf8,
                &mut a,
                &mut b
            ),
            Ordering::Equal
        );
    }

    #[test]
    fn record_round_trip() {
        let r = SortRecord {
            prefix: [7; PREFIX_BYTES],
            row_id: 0x0102_0304_0506_0708,
        };
        let b = r.to_bytes();
        assert_eq!(&b[16..], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(SortRecord::from_bytes(&b), r);
        assert_eq!(std::mem::size_of::<SortRecord>(), RECORD_BYTES);
    }
}
