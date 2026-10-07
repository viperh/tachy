//! Parsing and formatting of human-readable sizes and counts.
//!
//! - [`parse_size`] reads byte sizes in binary units (`2G` = 2^31 bytes), as
//!   used by `--mem` (spec §3) and the `memory` config key (spec §15).
//! - [`format_size`], [`format_count`], [`format_count_compact`],
//!   [`format_rate`] and [`format_eta`] format numbers for display, in one
//!   style shared by every screen (M1-07).
//! - [`parse_count`] reads decimal counts (`10M` = 10,000,000), used by the
//!   test-data generator.
//!
//! These live in the core, without any `clap` dependency, so every front end
//! and tool can reuse them.

use thiserror::Error;

/// Why a size or count string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseSizeError {
    /// The input was empty or only whitespace.
    #[error("empty value")]
    Empty,
    /// The input started with a minus sign.
    #[error("must not be negative")]
    Negative,
    /// A size of zero bytes was given.
    #[error("size must be greater than zero")]
    Zero,
    /// A fractional number was given without a unit suffix.
    #[error("decimals need a unit suffix, e.g. 1.5G")]
    DecimalWithoutSuffix,
    /// The value does not fit in 64 bits.
    #[error("value is too large")]
    Overflow,
    /// The input is not a number followed by an optional known suffix.
    #[error("invalid value '{0}', expected a number with an optional suffix such as 512M or 2G")]
    Invalid(String),
}

/// Parses a byte size such as `2G`, `512M`, `64k`, `1T`, `2GiB` or `100`.
///
/// - Suffixes `K`, `M`, `G`, `T` are binary (1K = 1024) and case-insensitive,
///   optionally followed by `B` or `iB`. A bare `B` means bytes.
/// - A decimal is allowed with a unit suffix (`1.5G`) and is rounded down to
///   whole bytes.
/// - Zero, negative values, overflow and decimals without a suffix are
///   rejected.
pub fn parse_size(input: &str) -> Result<u64, ParseSizeError> {
    let value = parse_scaled(input, |suffix| {
        let unit = match suffix {
            "b" => return Some(1),
            other => other
                .strip_suffix("ib")
                .or_else(|| other.strip_suffix('b'))
                .unwrap_or(other),
        };
        let power = match unit {
            "k" => 1,
            "m" => 2,
            "g" => 3,
            "t" => 4,
            _ => return None,
        };
        Some(1u64 << (10 * power))
    })?;
    if value == 0 {
        return Err(ParseSizeError::Zero);
    }
    Ok(value)
}

/// Parses a decimal count such as `10M` (10,000,000), `1k` (1,000) or `250`.
///
/// Suffixes `K`, `M`, `G`, `T` are powers of 1000 and case-insensitive.
/// Decimals are allowed with a suffix (`1.5k` = 1,500) and rounded down.
/// Zero is accepted.
pub fn parse_count(input: &str) -> Result<u64, ParseSizeError> {
    parse_scaled(input, |suffix| match suffix {
        "k" => Some(1_000),
        "m" => Some(1_000_000),
        "g" => Some(1_000_000_000),
        "t" => Some(1_000_000_000_000),
        _ => None,
    })
}

/// Formats a byte count for display (spec §11.5, M1-07): binary units with
/// decimal-style labels and one decimal place, `512 B`, `4.2 KB`, `4.2 MB`,
/// `4.2 GB`, `1.1 TB` (1 KB = 1,024 B). Rounded to the nearest tenth.
///
/// One style everywhere: the status line, the jobs drawer and toasts.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    // The largest unit not above `bytes`.
    let mut power = 1;
    while power < UNITS.len() && bytes >= 1u64 << (10 * (power + 1)) {
        power += 1;
    }
    let tenths = |power: usize| {
        let unit = 1u128 << (10 * power);
        ((u128::from(bytes) * 10 + unit / 2) / unit) as u64
    };
    let mut t = tenths(power);
    // `1023.96 KB` rounds to `1024.0 KB`: show `1.0 MB` instead.
    if t >= 10_240 && power < UNITS.len() {
        power += 1;
        t = tenths(power);
    }
    format!("{}.{} {}", t / 10, t % 10, UNITS[power - 1])
}

/// Formats a count with `,` thousands separators: `412,000,113`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Compact count for tight places (jobs drawer, `sample 20k rows`): `999`,
/// `1k`, `1.2k`, `20k`, `12.3M`, `412M`, `1.5G`, `2T`.
///
/// One decimal below 100 of the unit, none from 100 on; a `.0` is dropped.
/// Rounded down, so it never overstates.
pub fn format_count_compact(n: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000_000_000_000, "T"),
        (1_000_000_000, "G"),
        (1_000_000, "M"),
        (1_000, "k"),
    ];
    for (scale, unit) in UNITS {
        if n >= scale {
            let tenths = (u128::from(n) * 10 / u128::from(scale)) as u64;
            return if tenths >= 1000 || tenths.is_multiple_of(10) {
                format!("{}{unit}", tenths / 10)
            } else {
                format!("{}.{}{unit}", tenths / 10, tenths % 10)
            };
        }
    }
    n.to_string()
}

/// Formats a throughput in bytes per second: `3.4 GB/s`.
pub fn format_rate(bytes_per_sec: f64) -> String {
    let rate = if bytes_per_sec.is_finite() && bytes_per_sec > 0.0 {
        bytes_per_sec as u64
    } else {
        0
    };
    format!("{}/s", format_size(rate))
}

/// Formats a remaining time: `~23 s left`, `~4 min left`, `~1 h 12 min left`.
/// `None` (not enough samples yet, or no progress) is `—`.
///
/// Seconds are shown below one minute, minutes (rounded) below one hour.
pub fn format_eta(remaining: Option<std::time::Duration>) -> String {
    let Some(remaining) = remaining else {
        return "—".to_owned();
    };
    let secs = remaining.as_secs_f64().round() as u64;
    if secs < 60 {
        return format!("~{} s left", secs.max(1));
    }
    let minutes = (secs + 30) / 60;
    if minutes < 60 {
        return format!("~{minutes} min left");
    }
    match (minutes / 60, minutes % 60) {
        (h, 0) => format!("~{h} h left"),
        (h, m) => format!("~{h} h {m} min left"),
    }
}

/// Shared parser: `<digits>[.<digits>]<suffix>` where `multiplier` maps the
/// lower-cased suffix (possibly empty) to a factor. An empty suffix is 1.
fn parse_scaled(
    input: &str,
    multiplier: impl Fn(&str) -> Option<u64>,
) -> Result<u64, ParseSizeError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseSizeError::Empty);
    }
    if trimmed.starts_with('-') {
        return Err(ParseSizeError::Negative);
    }
    let invalid = || ParseSizeError::Invalid(trimmed.to_string());

    let number_end = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(number_end);
    let suffix = suffix.trim_start().to_ascii_lowercase();

    let (int_part, frac_part) = match number.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (number, None),
    };
    if int_part.is_empty() && frac_part.is_none_or(str::is_empty) {
        return Err(invalid());
    }
    if frac_part.is_some_and(|f| f.contains('.')) {
        return Err(invalid());
    }

    let factor = if suffix.is_empty() {
        1
    } else {
        multiplier(&suffix).ok_or_else(invalid)?
    };
    if frac_part.is_some() && factor == 1 {
        return Err(ParseSizeError::DecimalWithoutSuffix);
    }

    let int: u128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| ParseSizeError::Overflow)?
    };
    let mut total = int
        .checked_mul(u128::from(factor))
        .ok_or(ParseSizeError::Overflow)?;

    if let Some(frac) = frac_part.filter(|f| !f.is_empty()) {
        // Only the leading digits can matter for a 64-bit factor; extra ones
        // would only overflow the scale without changing the rounded result.
        let frac = &frac[..frac.len().min(20)];
        let numerator: u128 = frac.parse().map_err(|_| invalid())?;
        let scale = 10u128.pow(frac.len() as u32);
        total += numerator * u128::from(factor) / scale;
    }

    u64::try_from(total).map_err(|_| ParseSizeError::Overflow)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn parses_binary_sizes() {
        assert_eq!(parse_size("2G"), Ok(2 * GIB));
        assert_eq!(parse_size("2g"), Ok(2 * GIB));
        assert_eq!(parse_size("2GiB"), Ok(2 * GIB));
        assert_eq!(parse_size("2gb"), Ok(2 * GIB));
        assert_eq!(parse_size("512M"), Ok(536_870_912));
        assert_eq!(parse_size("64k"), Ok(65_536));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        assert_eq!(parse_size("100"), Ok(100));
        assert_eq!(parse_size("100B"), Ok(100));
    }

    #[test]
    fn decimals_need_a_suffix_and_round_down() {
        assert_eq!(parse_size("1.5G"), Ok(1_610_612_736));
        assert_eq!(parse_size("0.5k"), Ok(512));
        assert_eq!(parse_size("1.0001K"), Ok(1024));
        assert_eq!(parse_size("1.5"), Err(ParseSizeError::DecimalWithoutSuffix));
    }

    #[test]
    fn rejects_bad_sizes() {
        assert_eq!(parse_size("0"), Err(ParseSizeError::Zero));
        assert_eq!(parse_size("0G"), Err(ParseSizeError::Zero));
        assert_eq!(parse_size("-1"), Err(ParseSizeError::Negative));
        assert_eq!(parse_size("99999999999T"), Err(ParseSizeError::Overflow));
        assert_eq!(
            parse_size("99999999999999999999999999999999999999999"),
            Err(ParseSizeError::Overflow)
        );
        assert_eq!(parse_size(""), Err(ParseSizeError::Empty));
        assert_eq!(parse_size("  "), Err(ParseSizeError::Empty));
        assert!(matches!(parse_size("abc"), Err(ParseSizeError::Invalid(_))));
        assert!(matches!(parse_size("2X"), Err(ParseSizeError::Invalid(_))));
        assert!(matches!(
            parse_size("1.2.3G"),
            Err(ParseSizeError::Invalid(_))
        ));
        assert!(matches!(parse_size(".G"), Err(ParseSizeError::Invalid(_))));
    }

    #[test]
    fn parses_decimal_counts() {
        assert_eq!(parse_count("10M"), Ok(10_000_000));
        assert_eq!(parse_count("1k"), Ok(1_000));
        assert_eq!(parse_count("1.5K"), Ok(1_500));
        assert_eq!(parse_count("250"), Ok(250));
        assert_eq!(parse_count("0"), Ok(0));
        assert!(matches!(
            parse_count("1KiB"),
            Err(ParseSizeError::Invalid(_))
        ));
        assert_eq!(parse_count("-5"), Err(ParseSizeError::Negative));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(4300), "4.2 KB");
        assert_eq!(format_size(4_404_019), "4.2 MB");
        assert_eq!(format_size(536_870_912), "512.0 MB");
        assert_eq!(format_size(1_610_612_736), "1.5 GB");
        assert_eq!(format_size(2 * GIB), "2.0 GB");
        assert_eq!(format_size(48 * GIB + GIB / 5), "48.2 GB");
        assert_eq!(format_size((1 << 40) + (1 << 37)), "1.1 TB");
        // Rounding up to the next unit shows the next unit.
        assert_eq!(format_size(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(format_size(u64::MAX), "16384.0 PB");
    }

    #[test]
    fn formats_counts() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(128_004_551), "128,004,551");
        assert_eq!(format_count(412_000_113), "412,000,113");
        assert_eq!(format_count(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn formats_compact_counts() {
        assert_eq!(format_count_compact(0), "0");
        assert_eq!(format_count_compact(999), "999");
        assert_eq!(format_count_compact(1_000), "1k");
        assert_eq!(format_count_compact(1_234), "1.2k");
        assert_eq!(format_count_compact(10_000), "10k");
        assert_eq!(format_count_compact(20_000), "20k");
        assert_eq!(format_count_compact(12_345_678), "12.3M");
        assert_eq!(format_count_compact(412_000_113), "412M");
        assert_eq!(format_count_compact(1_500_000_000), "1.5G");
    }

    #[test]
    fn formats_rates() {
        assert_eq!(format_rate(3.4 * GIB as f64), "3.4 GB/s");
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s");
        assert_eq!(format_rate(512.0), "512 B/s");
    }

    #[test]
    fn formats_etas() {
        use std::time::Duration;
        assert_eq!(format_eta(None), "—");
        assert_eq!(format_eta(Some(Duration::from_millis(200))), "~1 s left");
        assert_eq!(format_eta(Some(Duration::from_secs(23))), "~23 s left");
        assert_eq!(format_eta(Some(Duration::from_secs(59))), "~59 s left");
        assert_eq!(format_eta(Some(Duration::from_secs(60))), "~1 min left");
        assert_eq!(
            format_eta(Some(Duration::from_secs(4 * 60 + 10))),
            "~4 min left"
        );
        assert_eq!(format_eta(Some(Duration::from_secs(3599))), "~1 h left");
        assert_eq!(
            format_eta(Some(Duration::from_secs(72 * 60))),
            "~1 h 12 min left"
        );
        assert_eq!(format_eta(Some(Duration::from_secs(2 * 3600))), "~2 h left");
    }
}
