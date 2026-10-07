//! `gen`: deterministic test-data generator for benches and manual testing
//! (spec §19, task M7-05).
//!
//! ```text
//! cargo run --release -p tachy-core --features gen --bin gen -- --rows 10M --cols 12 --out /tmp/x.csv
//! ```
//!
//! Columns cycle through `id` (sequential i64), `price` (f64), `date`, `ts`
//! (datetime), `country` (30 codes), `status` (enum), `customer` (names) and
//! `notes` (free text with delimiters, quotes and, with `--quoted-newlines`,
//! line breaks). Column 9 onwards repeat the mix with a `_2`, `_3`, …
//! suffix.
//!
//! The output depends only on the flags: the same seed gives the same bytes.
//! The PRNG is an inline xoshiro256** seeded through SplitMix64, so there is
//! no `rand` dependency and the stream never changes under us.
//!
//! Speed: rows are assembled in reused buffers (no per-field allocation) and
//! written through an 8 MiB `BufWriter`.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, ValueEnum};
use tachy_core::size::parse_count;

/// Generates a deterministic delimited test file.
#[derive(Debug, Parser)]
#[command(name = "gen", version, about)]
struct Args {
    /// Number of data rows (`10M` = 10,000,000, `1k` = 1,000).
    #[arg(long, value_parser = parse_rows)]
    rows: u64,

    /// Number of columns.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    cols: u32,

    /// Output file (default: stdout).
    #[arg(long, short)]
    out: Option<PathBuf>,

    /// PRNG seed; the same seed and flags give byte-identical output.
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Field delimiter: one ASCII character, or `tab` / `\t`.
    #[arg(long, default_value = ",", value_parser = parse_delimiter)]
    delimiter: u8,

    /// Fraction of `notes` fields containing a quoted line break (0..=1).
    #[arg(long, default_value_t = 0.0, value_parser = parse_ratio)]
    quoted_newlines: f64,

    /// Fraction of rows with one field too many or too few (0..=1).
    #[arg(long, default_value_t = 0.0, value_parser = parse_ratio)]
    ragged: f64,

    /// Fraction of fields (all but `id`) left empty (0..=1).
    #[arg(long, default_value_t = 0.0, value_parser = parse_ratio)]
    nulls: f64,

    /// Write a header row (default).
    #[arg(long, overrides_with = "no_header")]
    header: bool,

    /// Do not write a header row.
    #[arg(long, overrides_with = "header")]
    no_header: bool,

    /// End lines with `\r\n` instead of `\n`.
    #[arg(long)]
    crlf: bool,

    /// Output encoding of the non-ASCII text (names, notes).
    #[arg(long, value_enum, default_value_t = Encoding::Utf8)]
    encoding: Encoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Encoding {
    #[value(name = "utf-8", alias = "utf8")]
    Utf8,
    #[value(name = "windows-1252", alias = "cp1252", alias = "latin1")]
    Windows1252,
}

fn parse_rows(s: &str) -> Result<u64, String> {
    parse_count(s).map_err(|e| e.to_string())
}

fn parse_ratio(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("'{s}' is not a number"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("{v} is not between 0 and 1"))
    }
}

fn parse_delimiter(s: &str) -> Result<u8, String> {
    let b = match s {
        "tab" | "\\t" | "\t" => b'\t',
        _ if s.len() == 1 => s.as_bytes()[0],
        _ => return Err(format!("'{s}' is not a single ASCII character")),
    };
    if !b.is_ascii() || matches!(b, b'"' | b'\r' | b'\n') || b.is_ascii_alphanumeric() {
        return Err(format!(
            "'{s}' cannot be a delimiter (letters, digits, quotes and line breaks are not allowed)"
        ));
    }
    Ok(b)
}

// ---------------------------------------------------------------------------
// PRNG: xoshiro256** (Blackman & Vigna), seeded with SplitMix64.
// ---------------------------------------------------------------------------

struct Xoshiro256 {
    s: [u64; 4],
}

impl Xoshiro256 {
    fn new(seed: u64) -> Self {
        let mut sm = seed;
        let mut next = || {
            sm = sm.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = sm;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Xoshiro256 {
            s: [next(), next(), next(), next()],
        }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform in `0..n` (Lemire's multiply-shift, bias negligible here).
    #[inline]
    fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// True with probability `threshold / 2^64` (see [`threshold`]).
    #[inline]
    fn chance(&mut self, threshold: u64) -> bool {
        threshold == u64::MAX || self.next_u64() < threshold
    }
}

/// A ratio in `0..=1` as a `u64` threshold for [`Xoshiro256::chance`].
fn threshold(ratio: f64) -> u64 {
    if ratio >= 1.0 {
        u64::MAX
    } else {
        (ratio * 18_446_744_073_709_551_616.0) as u64
    }
}

// ---------------------------------------------------------------------------
// Value pools
// ---------------------------------------------------------------------------

const KINDS: [&str; 8] = [
    "id", "price", "date", "ts", "country", "status", "customer", "notes",
];

const COUNTRIES: [&str; 30] = [
    "DE", "FR", "US", "GB", "JP", "CN", "IN", "BR", "CA", "AU", "IT", "ES", "NL", "SE", "NO", "DK",
    "FI", "PL", "RO", "PT", "AT", "CH", "BE", "IE", "MX", "AR", "ZA", "KR", "NZ", "SG",
];

const STATUSES: [&str; 6] = [
    "paid",
    "shipped",
    "refunded",
    "pending",
    "cancelled",
    "delivered",
];

const FIRST_NAMES: [&str; 16] = [
    "Anna", "Bruno", "Chloé", "Dmitri", "Élodie", "Farid", "Greta", "Hiro", "Inés", "Jürgen",
    "Kofi", "Léa", "Mateo", "Noémie", "Oskar", "Zoë",
];

const LAST_NAMES: [&str; 16] = [
    "Müller",
    "García",
    "Dubois",
    "Smith",
    "Tanaka",
    "Rossi",
    "Nowak",
    "Popescu",
    "Søndergaard",
    "O'Brien",
    "Núñez",
    "Kowalski",
    "Larsen",
    "Moreau",
    "Schäfer",
    "Silva",
];

const WORDS: [&str; 32] = [
    "order",
    "late",
    "gift",
    "wrap",
    "café",
    "naïve",
    "résumé",
    "fragile",
    "please",
    "call",
    "before",
    "delivery",
    "leave",
    "at",
    "door",
    "back",
    "office",
    "invoice",
    "requested",
    "customer",
    "reported",
    "damage",
    "refund",
    "issued",
    "express",
    "address",
    "changed",
    "déjà",
    "vu",
    "über",
    "fast",
    "thanks",
];

/// Encodes every pool string once, so rows only copy bytes.
struct Pools {
    first: Vec<Vec<u8>>,
    last: Vec<Vec<u8>>,
    words: Vec<Vec<u8>>,
    /// Some name contains the delimiter or a quote: names must be scanned.
    names_need_scan: bool,
    /// `word_has_delimiter[i]`: `words[i]` contains the delimiter.
    word_has_delimiter: Vec<bool>,
}

impl Pools {
    fn new(enc: Encoding, delimiter: u8) -> Self {
        let encode = |list: &[&str]| -> Vec<Vec<u8>> {
            list.iter()
                .map(|s| match enc {
                    Encoding::Utf8 => s.as_bytes().to_vec(),
                    Encoding::Windows1252 => {
                        let (bytes, _, unmappable) = encoding_rs::WINDOWS_1252.encode(s);
                        assert!(!unmappable, "{s} is not in windows-1252");
                        bytes.into_owned()
                    }
                })
                .collect()
        };
        let first = encode(&FIRST_NAMES);
        let last = encode(&LAST_NAMES);
        let words = encode(&WORDS);
        let has = |s: &[u8], b: u8| s.contains(&b);
        Pools {
            names_need_scan: delimiter == b' '
                || first
                    .iter()
                    .chain(&last)
                    .any(|s| has(s, delimiter) || has(s, b'"')),
            word_has_delimiter: words.iter().map(|w| has(w, delimiter)).collect(),
            first,
            last,
            words,
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers (no allocation)
// ---------------------------------------------------------------------------

#[inline]
fn push_u64(buf: &mut Vec<u8>, mut n: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

#[inline]
fn push_2d(buf: &mut Vec<u8>, n: u64) {
    buf.extend_from_slice(&[b'0' + (n / 10) as u8, b'0' + (n % 10) as u8]);
}

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

struct Generator {
    rng: Xoshiro256,
    pools: Pools,
    cols: usize,
    delimiter: u8,
    eol: &'static [u8],
    quoted_newline: u64,
    ragged: u64,
    nulls: u64,
    /// `scan[kind]`: values of this column kind may contain the delimiter,
    /// so they are built in `field` and quoted if needed. The other kinds
    /// go straight into `row`.
    scan: [bool; KINDS.len()],
    field: Vec<u8>,
    row: Vec<u8>,
}

impl Generator {
    fn new(args: &Args) -> Self {
        // ids, prices, dates, timestamps, country codes and statuses use only
        // ASCII letters, digits, `-`, `.` and `:`; delimiters are never
        // letters or digits.
        let plain_safe = !matches!(args.delimiter, b'-' | b'.' | b':');
        let pools = Pools::new(args.encoding, args.delimiter);
        let mut scan = [false; KINDS.len()];
        scan[..6].fill(!plain_safe);
        scan[6] = pools.names_need_scan;
        // scan[7] (notes) stays false: notes quote themselves.
        Generator {
            rng: Xoshiro256::new(args.seed),
            pools,
            cols: args.cols as usize,
            delimiter: args.delimiter,
            eol: if args.crlf { b"\r\n" } else { b"\n" },
            quoted_newline: threshold(args.quoted_newlines),
            ragged: threshold(args.ragged),
            nulls: threshold(args.nulls),
            scan,
            field: Vec::with_capacity(256),
            row: Vec::with_capacity(4096),
        }
    }

    fn header(&mut self) -> &[u8] {
        self.row.clear();
        for c in 0..self.cols {
            if c > 0 {
                self.row.push(self.delimiter);
            }
            self.row
                .extend_from_slice(KINDS[c % KINDS.len()].as_bytes());
            if c >= KINDS.len() {
                self.row.push(b'_');
                push_u64(&mut self.row, (c / KINDS.len() + 1) as u64);
            }
        }
        self.row.extend_from_slice(self.eol);
        &self.row
    }

    /// Appends `self.field` to the row, quoted if it needs to be.
    #[inline]
    fn flush_field(&mut self) {
        // `\r` only ever appears right before `\n`.
        if memchr::memchr3(self.delimiter, b'"', b'\n', &self.field).is_some() {
            self.row.push(b'"');
            let mut rest = &self.field[..];
            while let Some(i) = memchr::memchr(b'"', rest) {
                self.row.extend_from_slice(&rest[..=i]);
                self.row.push(b'"');
                rest = &rest[i + 1..];
            }
            self.row.extend_from_slice(rest);
            self.row.push(b'"');
        } else {
            self.row.extend_from_slice(&self.field);
        }
    }

    fn row(&mut self, id: u64) -> &[u8] {
        self.row.clear();
        let mut fields = self.cols;
        if self.ragged != 0 && self.rng.chance(self.ragged) {
            fields = if fields > 1 && self.rng.below(2) == 0 {
                fields - 1
            } else {
                fields + 1
            };
        }
        for c in 0..fields {
            if c > 0 {
                self.row.push(self.delimiter);
            }
            let kind = c % KINDS.len();
            if kind != 0 && self.nulls != 0 && self.rng.chance(self.nulls) {
                continue;
            }
            if self.scan[kind] {
                let mut field = std::mem::take(&mut self.field);
                field.clear();
                self.value(kind, id, &mut field);
                self.field = field;
                self.flush_field();
            } else {
                let mut row = std::mem::take(&mut self.row);
                self.value(kind, id, &mut row);
                self.row = row;
            }
        }
        self.row.extend_from_slice(self.eol);
        &self.row
    }

    /// Appends one value of column kind `kind` to `f`.
    #[inline]
    fn value(&mut self, kind: usize, id: u64, f: &mut Vec<u8>) {
        let rng = &mut self.rng;
        match kind {
            // id
            0 => push_u64(f, id),
            // price: 0.00 ..= 9999.99, a few negatives (refunds)
            1 => {
                let r = rng.next_u64();
                if r & 0xFF == 0 {
                    f.push(b'-');
                }
                let cents = (r >> 8) % 1_000_000;
                push_u64(f, cents / 100);
                f.push(b'.');
                push_2d(f, cents % 100);
            }
            // date: 2000-01-01 ..= 2029-12-28
            2 => {
                let r = rng.next_u64();
                push_u64(f, 2000 + r % 30);
                f.push(b'-');
                push_2d(f, 1 + (r >> 8) % 12);
                f.push(b'-');
                push_2d(f, 1 + (r >> 16) % 28);
            }
            // ts
            3 => {
                let r = rng.next_u64();
                push_u64(f, 2000 + r % 30);
                f.push(b'-');
                push_2d(f, 1 + (r >> 8) % 12);
                f.push(b'-');
                push_2d(f, 1 + (r >> 16) % 28);
                f.push(b'T');
                push_2d(f, (r >> 24) % 24);
                f.push(b':');
                push_2d(f, (r >> 32) % 60);
                f.push(b':');
                push_2d(f, (r >> 40) % 60);
            }
            4 => f.extend_from_slice(COUNTRIES[rng.below(30) as usize].as_bytes()),
            5 => f.extend_from_slice(STATUSES[rng.below(6) as usize].as_bytes()),
            // customer: "First Last"
            6 => {
                let r = rng.next_u64();
                f.extend_from_slice(&self.pools.first[(r % 16) as usize]);
                f.push(b' ');
                f.extend_from_slice(&self.pools.last[((r >> 8) % 16) as usize]);
            }
            // notes: 2..=7 words, some commas and quoted words, optional
            // line break. Written straight into the row: whether the field
            // needs quotes is known from the draws before any byte is
            // written.
            _ => {
                let r = rng.next_u64();
                let words = 2 + r % 6;
                let newline_at = if self.quoted_newline != 0 && rng.chance(self.quoted_newline) {
                    1 + (r >> 8) % (words - 1)
                } else {
                    u64::MAX
                };
                // 12 bits per word: 5 for the word, 4 for quoting it, 3 for a comma.
                let low = rng.next_u64();
                let high = if words > 5 { rng.next_u64() } else { 0 };
                let bits = |w: u64| {
                    if w < 5 {
                        low >> (12 * w)
                    } else {
                        high >> (12 * (w - 5))
                    }
                };
                let pools = &self.pools;
                let comma = |b: u64| (b >> 9) & 7 == 0;
                let quote = |b: u64| (b >> 5) & 15 == 0;
                let needs_quotes = newline_at != u64::MAX
                    || self.delimiter == b' '
                    || (0..words).any(|w| {
                        let b = bits(w);
                        quote(b)
                            || (comma(b) && self.delimiter == b',')
                            || pools.word_has_delimiter[(b & 31) as usize]
                    });
                if needs_quotes {
                    f.push(b'"');
                }
                for w in 0..words {
                    let b = bits(w);
                    if w == newline_at {
                        f.extend_from_slice(self.eol);
                    } else if w > 0 {
                        f.push(b' ');
                    }
                    let word = &pools.words[(b & 31) as usize];
                    if quote(b) {
                        // Only ever inside a quoted field: doubled.
                        f.extend_from_slice(b"\"\"");
                        f.extend_from_slice(word);
                        f.extend_from_slice(b"\"\"");
                    } else {
                        f.extend_from_slice(word);
                    }
                    if comma(b) {
                        f.push(b',');
                    }
                }
                if needs_quotes {
                    f.push(b'"');
                }
            }
        }
    }
}

fn run(args: &Args) -> io::Result<()> {
    let sink: Box<dyn Write> = match &args.out {
        Some(path) => Box::new(File::create(path)?),
        None => Box::new(io::stdout().lock()),
    };
    let mut w = BufWriter::with_capacity(8 << 20, sink);
    let mut g = Generator::new(args);
    if !args.no_header {
        w.write_all(g.header())?;
    }
    for id in 1..=args.rows {
        w.write_all(g.row(id))?;
    }
    w.flush()
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        // `gen | head` closes the pipe early; that is not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gen: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses flags; `--rows 2k` unless `extra` sets `--rows`.
    fn args(extra: &[&str]) -> Args {
        let mut v = vec!["gen"];
        if !extra.contains(&"--rows") {
            v.extend_from_slice(&["--rows", "2k"]);
        }
        v.extend_from_slice(extra);
        Args::try_parse_from(v).unwrap()
    }

    fn generate(args: &Args) -> Vec<u8> {
        let mut g = Generator::new(args);
        let mut out = Vec::new();
        if !args.no_header {
            out.extend_from_slice(g.header());
        }
        for id in 1..=args.rows {
            out.extend_from_slice(g.row(id));
        }
        out
    }

    #[test]
    fn xoshiro_reference_vector() {
        // xoshiro256** with state [1, 2, 3, 4]: the reference implementation's
        // first outputs.
        let mut r = Xoshiro256 { s: [1, 2, 3, 4] };
        assert_eq!(r.next_u64(), 11520);
        assert_eq!(r.next_u64(), 0);
        assert_eq!(r.next_u64(), 1509978240);
        assert_eq!(r.next_u64(), 1215971899390074240);
    }

    #[test]
    fn deterministic_per_seed() {
        let a = generate(&args(&["--cols", "12", "--quoted-newlines", "0.1"]));
        let b = generate(&args(&["--cols", "12", "--quoted-newlines", "0.1"]));
        assert_eq!(a, b);
        let c = generate(&args(&[
            "--cols",
            "12",
            "--quoted-newlines",
            "0.1",
            "--seed",
            "7",
        ]));
        assert_ne!(a, c);
    }

    #[test]
    fn header_and_columns() {
        let out = generate(&args(&["--cols", "10", "--rows", "3"]));
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "id,price,date,ts,country,status,customer,notes,id_2,price_2"
        );
        assert_eq!(lines.len(), 4);
        assert!(lines[1].starts_with("1,"));
        assert!(lines[3].starts_with("3,"));
        let no_header = generate(&args(&["--rows", "3", "--no-header"]));
        assert!(no_header.starts_with(b"1,"));
    }

    #[test]
    fn flags_parse() {
        let a = args(&[
            "--rows",
            "10M",
            "--delimiter",
            "tab",
            "--crlf",
            "--encoding",
            "windows-1252",
        ]);
        assert_eq!(a.rows, 10_000_000);
        assert_eq!(a.delimiter, b'\t');
        assert!(a.crlf);
        assert_eq!(a.encoding, Encoding::Windows1252);
        assert!(Args::try_parse_from(["gen", "--rows", "1", "--nulls", "1.5"]).is_err());
        assert!(Args::try_parse_from(["gen", "--rows", "1", "--delimiter", "ab"]).is_err());
        assert!(Args::try_parse_from(["gen", "--rows", "1", "--delimiter", "\""]).is_err());
        assert!(Args::try_parse_from(["gen"]).is_err(), "--rows is required");
        let a = args(&["--no-header", "--header"]);
        assert!(!a.no_header, "the last of --header/--no-header wins");
    }

    #[test]
    fn crlf_and_quoted_newlines() {
        let out = generate(&args(&["--crlf", "--quoted-newlines", "1"]));
        // Every line break is CRLF.
        for (i, &b) in out.iter().enumerate() {
            if b == b'\n' {
                assert_eq!(out[i - 1], b'\r');
            }
        }
        // One quoted break in every notes field, plus the record ends.
        let breaks = out.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(breaks, 1 + 2 * 2000);
    }

    #[test]
    fn nulls_and_ragged() {
        let all_null = generate(&args(&["--rows", "5", "--nulls", "1", "--no-header"]));
        assert_eq!(
            String::from_utf8(all_null).unwrap().lines().next().unwrap(),
            "1,,,,,,,"
        );
        let ragged = generate(&args(&["--rows", "200", "--ragged", "1", "--no-header"]));
        let text = String::from_utf8(ragged).unwrap();
        // Without notes (which may contain commas) the field count is exact.
        let a = args(&[
            "--rows",
            "200",
            "--ragged",
            "1",
            "--no-header",
            "--cols",
            "6",
        ]);
        let six = String::from_utf8(generate(&a)).unwrap();
        for line in six.lines() {
            let n = line.split(',').count();
            assert!(n == 5 || n == 7, "{line}");
        }
        assert!(!text.is_empty());
    }

    #[test]
    fn windows_1252_encodes_accents() {
        let a = args(&["--encoding", "windows-1252", "--cols", "8"]);
        let out = generate(&a);
        assert!(
            std::str::from_utf8(&out).is_err(),
            "contains 0xE9-style bytes"
        );
        let (text, _, had_errors) = encoding_rs::WINDOWS_1252.decode(&out);
        assert!(!had_errors);
        assert!(text.contains('é') || text.contains('ü') || text.contains('ö'));
        let utf8 = generate(&args(&["--cols", "8"]));
        assert!(std::str::from_utf8(&utf8).is_ok());
    }
}
