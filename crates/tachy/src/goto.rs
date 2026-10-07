//! Input parsing of the `g` go-to dialog (spec §13, M2-03). Pure.
//!
//! - `1234`, `1_234`, `1,234`: a 1-based row of the active view.
//! - `50%`, `12.5%`: a position through the view. While the `All` view is
//!   still being indexed, the final row count isn't known, so a percentage
//!   means a **byte position** in the file (`App` resolves it); once the
//!   index is complete it means a row (`floor(P/100 × (len − 1))`).
//! - Anything else: a column name.

use tachy_core::column::ColumnMeta;

/// Matches listed in an "ambiguous" error.
const AMBIGUOUS_SHOWN: usize = 5;

/// What the user asked to go to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GotoTarget {
    /// 1-based view position.
    Row(u64),
    /// `0 ≤ P ≤ 100`.
    Percent(f64),
    /// Index into the tab's columns (its source column).
    Column(usize),
}

/// Parses the go-to input. Errors are shown under the input.
pub fn parse_goto(input: &str, columns: &[ColumnMeta]) -> Result<GotoTarget, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("type a row number, a percentage or a column name".to_owned());
    }
    if let Some(p) = s.strip_suffix('%') {
        let p = p.trim();
        if is_decimal(p) {
            let v: f64 = p.parse().map_err(|_| format!("not a percentage: {s}"))?;
            if !(0.0..=100.0).contains(&v) {
                return Err("percentages go from 0% to 100%".to_owned());
            }
            return Ok(GotoTarget::Percent(v));
        }
        if p.starts_with('-') && is_decimal(&p[1..]) {
            return Err("percentages go from 0% to 100%".to_owned());
        }
    }
    if let Some(digits) = row_digits(s) {
        let n: u64 = digits
            .parse()
            .map_err(|_| "row number too large".to_owned())?;
        if n == 0 {
            return Err("rows start at 1".to_owned());
        }
        return Ok(GotoTarget::Row(n));
    }
    if s.starts_with('-') && row_digits(&s[1..]).is_some() {
        return Err("rows start at 1".to_owned());
    }
    find_column(s, columns).map(GotoTarget::Column)
}

/// `12`, `12.5`, `.5`, `12.`: digits with at most one dot.
fn is_decimal(s: &str) -> bool {
    let mut digits = 0;
    let mut dots = 0;
    for c in s.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' => dots += 1,
            _ => return false,
        }
    }
    digits > 0 && dots <= 1
}

/// The digits of a row number with optional `_` or `,` separators, which
/// must sit between digits. `None` if `s` isn't one.
fn row_digits(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || !bytes[bytes.len() - 1].is_ascii_digit() {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for &b in bytes {
        match b {
            b'0'..=b'9' => {
                out.push(b as char);
                prev_sep = false;
            }
            b'_' | b',' if !prev_sep => prev_sep = true,
            _ => return None,
        }
    }
    Some(out)
}

/// Column lookup: exact display name, exact query name, case-insensitive,
/// then a unique case-insensitive prefix.
fn find_column(s: &str, columns: &[ColumnMeta]) -> Result<usize, String> {
    if let Some(i) = columns.iter().position(|c| c.name.display == s) {
        return Ok(i);
    }
    if let Some(i) = columns.iter().position(|c| c.name.query == s) {
        return Ok(i);
    }
    let lower = s.to_lowercase();
    let eq = |name: &str| name.to_lowercase() == lower;
    if let Some(i) = columns
        .iter()
        .position(|c| eq(&c.name.display) || eq(&c.name.query))
    {
        return Ok(i);
    }
    let prefixed: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.name.display.to_lowercase().starts_with(&lower)
                || c.name.query.to_lowercase().starts_with(&lower)
        })
        .map(|(i, _)| i)
        .collect();
    match prefixed.as_slice() {
        [] => Err(format!("no such column \"{s}\"")),
        [one] => Ok(*one),
        many => {
            let mut names: Vec<&str> = many
                .iter()
                .take(AMBIGUOUS_SHOWN)
                .map(|&i| columns[i].name.display.as_str())
                .collect();
            if many.len() > AMBIGUOUS_SHOWN {
                names.push("…");
            }
            Err(format!("ambiguous: {}", names.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tachy_core::column::ColumnName;

    use super::*;

    fn cols(names: &[&str]) -> Vec<ColumnMeta> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                ColumnMeta::new(
                    ColumnName {
                        display: (*n).to_owned(),
                        query: n.to_lowercase().replace(' ', "_"),
                    },
                    i,
                    false,
                )
            })
            .collect()
    }

    #[test]
    fn rows() {
        let c = cols(&["a"]);
        assert_eq!(parse_goto("1", &c), Ok(GotoTarget::Row(1)));
        assert_eq!(parse_goto(" 42 ", &c), Ok(GotoTarget::Row(42)));
        assert_eq!(parse_goto("1_000", &c), Ok(GotoTarget::Row(1000)));
        assert_eq!(parse_goto("1,000", &c), Ok(GotoTarget::Row(1000)));
        assert_eq!(parse_goto("1,000,000", &c), Ok(GotoTarget::Row(1_000_000)));
        assert_eq!(parse_goto("0", &c), Err("rows start at 1".into()));
        assert_eq!(parse_goto("-1", &c), Err("rows start at 1".into()));
        assert!(parse_goto("99999999999999999999999", &c).is_err());
    }

    #[test]
    fn percentages() {
        let c = cols(&["a"]);
        assert_eq!(parse_goto("50%", &c), Ok(GotoTarget::Percent(50.0)));
        assert_eq!(parse_goto("100%", &c), Ok(GotoTarget::Percent(100.0)));
        assert_eq!(parse_goto("0%", &c), Ok(GotoTarget::Percent(0.0)));
        assert_eq!(parse_goto("12.5%", &c), Ok(GotoTarget::Percent(12.5)));
        assert_eq!(
            parse_goto("101%", &c),
            Err("percentages go from 0% to 100%".into())
        );
        assert_eq!(
            parse_goto("-5%", &c),
            Err("percentages go from 0% to 100%".into())
        );
    }

    #[test]
    fn columns() {
        let c = cols(&["id", "Price", "price_eur", "Unit Price", "name"]);
        // Exact display name, then exact query name.
        assert_eq!(parse_goto("Price", &c), Ok(GotoTarget::Column(1)));
        assert_eq!(parse_goto("unit_price", &c), Ok(GotoTarget::Column(3)));
        // Case-insensitive.
        assert_eq!(parse_goto("NAME", &c), Ok(GotoTarget::Column(4)));
        assert_eq!(parse_goto("price", &c), Ok(GotoTarget::Column(1)));
        // Unique prefix.
        assert_eq!(parse_goto("na", &c), Ok(GotoTarget::Column(4)));
        assert_eq!(parse_goto("Un", &c), Ok(GotoTarget::Column(3)));
        // Ambiguous prefix.
        assert_eq!(
            parse_goto("pr", &c),
            Err("ambiguous: Price, price_eur".into())
        );
        assert_eq!(parse_goto("zzz", &c), Err("no such column \"zzz\"".into()));
        // A column whose name looks like a number is reachable by name only
        // when it isn't a valid row number.
        assert_eq!(parse_goto("1x", &c), Err("no such column \"1x\"".into()));
    }

    #[test]
    fn ambiguous_lists_at_most_five() {
        let c = cols(&["a1", "a2", "a3", "a4", "a5", "a6", "a7"]);
        assert_eq!(
            parse_goto("a", &c),
            Err("ambiguous: a1, a2, a3, a4, a5, …".into())
        );
    }

    #[test]
    fn separators_must_sit_between_digits() {
        let c = cols(&["x"]);
        assert!(parse_goto("1__000", &c).is_err());
        assert!(parse_goto("1,", &c).is_err());
        assert!(parse_goto(",1", &c).is_err());
    }
}
