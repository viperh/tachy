//! Command-line interface (spec §3).
//!
//! Options that the config file can also set (threads, memory, theme, ...)
//! have no clap default: an absent flag stays `None` so that
//! [`crate::settings::Settings::resolve`] can apply CLI > config > built-in.

use std::path::PathBuf;

use clap::{CommandFactory, Parser, ValueEnum, builder::PossibleValuesParser, error::ErrorKind};
use tachy_core::dialect::{Encoding, EscapeStyle};

use crate::{
    config::{get_config_dir, get_data_dir},
    theme::Theme,
};

/// One-line description, from the header of DOCUMENTATION.md.
const ABOUT: &str = "A terminal UI for viewing and processing very large delimited text files \
                     (CSV, TSV, PSV and any other DSV)";

/// `--help` footer (§16): the documented limitation of memory-mapping.
const LIMITATIONS: &str = "\
Limitations:
  Files are memory-mapped read-only. If an open file is truncated by another
  program, reading past its new end raises SIGBUS: tachy then restores the
  terminal, prints a message and exits with code 1. Appending or rewriting a
  file in place is detected within ~2 s; press R to reload it.";

#[derive(Parser, Debug)]
#[command(name = "tachy", author, version = version(), about = ABOUT, after_long_help = LIMITATIONS)]
pub struct Cli {
    /// Files to open; each opens in its own tab. Use - for stdin. Without
    /// files tachy starts empty: open one with Ctrl-o.
    #[arg(value_name = "FILE", num_args = 0.., verbatim_doc_comment)]
    pub files: Vec<PathBuf>,

    /// Field delimiter. Accepts a literal or: tab, comma, pipe, semicolon, space
    #[arg(short, long, value_name = "CHAR", value_parser = parse_delimiter)]
    pub delimiter: Option<u8>,

    /// Quote character, or "none"
    #[arg(short, long, value_name = "CHAR", value_parser = parse_quote)]
    pub quote: Option<QuoteArg>,

    /// Quote escape: double | backslash
    #[arg(
        long,
        value_name = "STYLE",
        hide_possible_values = true,
        ignore_case = true
    )]
    pub escape: Option<EscapeArg>,

    /// First row is a header (default: auto-detect)
    #[arg(long = "header", conflicts_with = "no_header")]
    header_flag: bool,

    /// First row is data, not a header (default: auto-detect)
    #[arg(long = "no-header")]
    no_header: bool,

    /// utf-8 | utf-16le | utf-16be | latin1 | windows-1252
    #[arg(
        long,
        value_name = "NAME",
        hide_possible_values = true,
        ignore_case = true
    )]
    pub encoding: Option<EncodingArg>,

    /// Skip lines starting with this character
    #[arg(long, value_name = "CHAR", value_parser = parse_comment)]
    pub comment: Option<u8>,

    /// Open with this filter applied (query language, §9)
    #[arg(short, long, value_name = "EXPR")]
    pub filter: Option<String>,

    /// Open sorted, e.g. "price:desc,ts"
    #[arg(short, long, value_name = "SPEC")]
    pub sort: Option<String>,

    /// Accept detected dialect without showing the dialog
    #[arg(short, long)]
    pub yes: bool,

    /// Worker threads (default: logical CPUs)
    #[arg(short = 'j', long, value_name = "N")]
    pub threads: Option<usize>,

    /// Memory budget, e.g. 2G, 512M (default: 2G)
    #[arg(short, long, value_name = "SIZE", value_parser = tachy_core::size::parse_size)]
    pub mem: Option<u64>,

    /// Directory for spill files (default: $TMPDIR)
    #[arg(long, value_name = "DIR")]
    pub tmp: Option<PathBuf>,

    /// Theme name (default: dark)
    #[arg(long, value_name = "NAME", value_parser = PossibleValuesParser::new(Theme::NAMES), hide_possible_values = true)]
    pub theme: Option<String>,
}

/// Value of `-q/--quote`: a quote byte, or `None` for "no quoting".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuoteArg(pub Option<u8>);

/// Value of `--escape`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EscapeArg {
    /// A doubled quote: `""`.
    Double,
    /// A backslash: `\"`.
    Backslash,
}

impl From<EscapeArg> for EscapeStyle {
    fn from(value: EscapeArg) -> Self {
        match value {
            EscapeArg::Double => EscapeStyle::Doubled,
            EscapeArg::Backslash => EscapeStyle::Backslash,
        }
    }
}

/// Value of `--encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EncodingArg {
    #[value(name = "utf-8", alias = "utf8")]
    Utf8,
    #[value(name = "utf-16le")]
    Utf16Le,
    #[value(name = "utf-16be")]
    Utf16Be,
    #[value(name = "latin1")]
    Latin1,
    #[value(name = "windows-1252")]
    Windows1252,
}

impl EncodingArg {
    /// The core encoding. `latin1` and `windows-1252` are the same encoding
    /// in the WHATWG standard that `encoding_rs` implements.
    pub fn encoding(self) -> Encoding {
        match self {
            EncodingArg::Utf8 => Encoding::Utf8,
            EncodingArg::Utf16Le => Encoding::Utf16Le,
            EncodingArg::Utf16Be => Encoding::Utf16Be,
            EncodingArg::Latin1 | EncodingArg::Windows1252 => Encoding::Windows1252,
        }
    }
}

impl Cli {
    /// Parses the process arguments and runs the checks clap cannot express.
    /// Exits with code 2 on any usage error.
    pub fn parse_and_validate() -> Self {
        let cli = Self::parse();
        if let Err(msg) = cli.validate() {
            Self::command()
                .error(ErrorKind::ArgumentConflict, msg)
                .exit();
        }
        cli
    }

    /// Checks done after parsing. Returns the usage error message.
    pub fn validate(&self) -> Result<(), String> {
        let stdin_count = self.files.iter().filter(|f| f.as_os_str() == "-").count();
        if stdin_count > 1 {
            return Err("stdin (-) can only be given once".to_string());
        }
        // stdin spooling needs crossterm's `/dev/tty` input (M1-08).
        if cfg!(windows) && stdin_count > 0 {
            return Err("stdin input is not supported on Windows".to_string());
        }
        Ok(())
    }

    /// `Some(true)` for `--header`, `Some(false)` for `--no-header`, `None`
    /// to auto-detect.
    pub fn header(&self) -> Option<bool> {
        match (self.header_flag, self.no_header) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }

    /// True when any dialect option was given explicitly, which skips the
    /// Detected format dialog (spec §2.1).
    #[cfg(test)]
    pub fn explicit_dialect(&self) -> bool {
        self.delimiter.is_some()
            || self.quote.is_some()
            || self.escape.is_some()
            || self.header().is_some()
            || self.encoding.is_some()
            || self.comment.is_some()
    }
}

/// Parses `-d`: one ASCII byte, a name (`tab`, `comma`, `pipe`, `semicolon`,
/// `space`) or the escape `\t`.
pub fn parse_delimiter(value: &str) -> Result<u8, String> {
    match value.to_ascii_lowercase().as_str() {
        "tab" | "\\t" => return Ok(b'\t'),
        "comma" => return Ok(b','),
        "pipe" => return Ok(b'|'),
        "semicolon" => return Ok(b';'),
        "space" => return Ok(b' '),
        _ => {}
    }
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_ascii() => Err("delimiter must be a single ASCII byte".into()),
        (Some('"'), None) => Err("the quote character \" cannot be the delimiter".into()),
        (Some('\n' | '\r'), None) => Err("a line break cannot be the delimiter".into()),
        (Some(c), None) => Ok(c as u8),
        (None, _) => Err("delimiter must be a single ASCII byte".into()),
        (Some(_), Some(_)) => Err("multi-byte delimiters are not supported".into()),
    }
}

/// Parses `-q`: `"`, `'` or `none` (case-insensitive).
pub fn parse_quote(value: &str) -> Result<QuoteArg, String> {
    match value {
        "\"" => Ok(QuoteArg(Some(b'"'))),
        "'" => Ok(QuoteArg(Some(b'\''))),
        v if v.eq_ignore_ascii_case("none") => Ok(QuoteArg(None)),
        _ => Err("quote must be \", ' or none".into()),
    }
}

/// Parses `--comment`: exactly one ASCII byte, not a line break.
pub fn parse_comment(value: &str) -> Result<u8, String> {
    match value.as_bytes() {
        [b'\n' | b'\r'] => Err("a line break cannot be the comment character".into()),
        [b] if b.is_ascii() => Ok(*b),
        _ => Err("comment must be a single ASCII byte".into()),
    }
}

/// `git describe` output, or the short commit hash when the repository has
/// no tags yet (vergen leaves `VERGEN_GIT_DESCRIBE` empty then).
fn git_describe() -> &'static str {
    let describe = env!("VERGEN_GIT_DESCRIBE");
    if describe.is_empty() {
        let sha = env!("VERGEN_GIT_SHA");
        &sha[..sha.len().min(7)]
    } else {
        describe
    }
}

pub fn version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let describe = git_describe();
    let build_date = env!("VERGEN_BUILD_DATE");
    let author = clap::crate_authors!();

    let config_dir_path = get_config_dir().display().to_string();
    let data_dir_path = get_data_dir().display().to_string();

    format!(
        "\
{version}-{describe} ({build_date})

Authors: {author}

Config directory: {config_dir_path}
Data directory: {data_dir_path}"
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("tachy").chain(args.iter().copied()))
    }

    #[test]
    fn command_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn delimiter_names_and_escapes() {
        assert_eq!(parse_delimiter("tab"), Ok(b'\t'));
        assert_eq!(parse_delimiter("TAB"), Ok(b'\t'));
        assert_eq!(parse_delimiter("\\t"), Ok(b'\t'));
        assert_eq!(parse_delimiter("comma"), Ok(b','));
        assert_eq!(parse_delimiter("Pipe"), Ok(b'|'));
        assert_eq!(parse_delimiter("semicolon"), Ok(b';'));
        assert_eq!(parse_delimiter("space"), Ok(b' '));
        assert_eq!(parse_delimiter(":"), Ok(b':'));
        assert_eq!(parse_delimiter("\t"), Ok(b'\t'));
    }

    #[test]
    fn delimiter_rejections() {
        assert_eq!(
            parse_delimiter("||"),
            Err("multi-byte delimiters are not supported".into())
        );
        assert_eq!(
            parse_delimiter("é"),
            Err("delimiter must be a single ASCII byte".into())
        );
        assert_eq!(
            parse_delimiter(""),
            Err("delimiter must be a single ASCII byte".into())
        );
        assert!(parse_delimiter("\"").is_err());
        assert!(parse_delimiter("\n").is_err());
    }

    #[test]
    fn delimiter_flag() {
        assert_eq!(
            parse(&["-d", "tab", "x.csv"]).unwrap().delimiter,
            Some(b'\t')
        );
        assert_eq!(
            parse(&["-d", "\\t", "x.csv"]).unwrap().delimiter,
            Some(b'\t')
        );
        assert!(parse(&["-d", "||", "x.csv"]).is_err());
    }

    #[test]
    fn quote_parsing() {
        assert_eq!(parse_quote("\""), Ok(QuoteArg(Some(b'"'))));
        assert_eq!(parse_quote("'"), Ok(QuoteArg(Some(b'\''))));
        assert_eq!(parse_quote("none"), Ok(QuoteArg(None)));
        assert_eq!(parse_quote("NONE"), Ok(QuoteArg(None)));
        assert!(parse_quote("`").is_err());
        assert!(parse_quote("").is_err());
    }

    #[test]
    fn encoding_parsing() {
        let enc = |name: &str| parse(&["--encoding", name, "x.csv"]).map(|c| c.encoding);
        assert_eq!(enc("utf-8").unwrap(), Some(EncodingArg::Utf8));
        assert_eq!(enc("UTF8").unwrap(), Some(EncodingArg::Utf8));
        assert_eq!(enc("utf-16le").unwrap(), Some(EncodingArg::Utf16Le));
        assert_eq!(enc("UTF-16BE").unwrap(), Some(EncodingArg::Utf16Be));
        assert_eq!(enc("Latin1").unwrap(), Some(EncodingArg::Latin1));
        assert_eq!(enc("windows-1252").unwrap(), Some(EncodingArg::Windows1252));
        assert!(enc("ebcdic").is_err());
        assert_eq!(
            EncodingArg::Latin1.encoding(),
            EncodingArg::Windows1252.encoding()
        );
        assert_eq!(EncodingArg::Utf16Le.encoding(), Encoding::Utf16Le);
    }

    #[test]
    fn escape_and_comment_parsing() {
        let cli = parse(&["--escape", "BACKSLASH", "--comment", "#", "x.csv"]).unwrap();
        assert_eq!(cli.escape, Some(EscapeArg::Backslash));
        assert_eq!(cli.comment, Some(b'#'));
        assert!(parse(&["--escape", "triple", "x.csv"]).is_err());
        assert!(parse(&["--comment", "##", "x.csv"]).is_err());
    }

    #[test]
    fn header_flags() {
        assert_eq!(parse(&["x.csv"]).unwrap().header(), None);
        assert_eq!(parse(&["--header", "x.csv"]).unwrap().header(), Some(true));
        assert_eq!(
            parse(&["--no-header", "x.csv"]).unwrap().header(),
            Some(false)
        );
        assert!(parse(&["--header", "--no-header", "x.csv"]).is_err());
    }

    #[test]
    fn explicit_dialect() {
        assert!(!parse(&["x.csv"]).unwrap().explicit_dialect());
        assert!(
            !parse(&["-y", "-f", "a > 1", "-j", "2", "x.csv"])
                .unwrap()
                .explicit_dialect()
        );
        for args in [
            &["-d", ",", "x.csv"][..],
            &["-q", "none", "x.csv"],
            &["--escape", "double", "x.csv"],
            &["--header", "x.csv"],
            &["--no-header", "x.csv"],
            &["--encoding", "utf-8", "x.csv"],
            &["--comment", "#", "x.csv"],
        ] {
            assert!(parse(args).unwrap().explicit_dialect(), "{args:?}");
        }
    }

    #[test]
    fn other_options() {
        let cli = parse(&[
            "-f",
            "price > 1",
            "-s",
            "price:desc",
            "-y",
            "-j",
            "0",
            "-m",
            "512M",
            "--tmp",
            "/tmp",
            "--theme",
            "dark",
            "a.csv",
            "-",
        ])
        .unwrap();
        assert_eq!(cli.filter.as_deref(), Some("price > 1"));
        assert_eq!(cli.sort.as_deref(), Some("price:desc"));
        assert!(cli.yes);
        assert_eq!(cli.threads, Some(0));
        assert_eq!(cli.mem, Some(536_870_912));
        assert_eq!(cli.tmp, Some(PathBuf::from("/tmp")));
        assert_eq!(cli.theme.as_deref(), Some("dark"));
        assert_eq!(cli.files, [PathBuf::from("a.csv"), PathBuf::from("-")]);
        assert_eq!(cli.validate(), Ok(()));
    }

    #[test]
    fn absent_options_are_none() {
        let cli = parse(&["x.csv"]).unwrap();
        assert_eq!(cli.threads, None);
        assert_eq!(cli.mem, None);
        assert_eq!(cli.tmp, None);
        assert_eq!(cli.theme, None);
        assert!(!cli.yes);
    }

    #[test]
    fn usage_errors() {
        // No file is fine: tachy starts empty.
        assert!(parse(&[]).unwrap().files.is_empty());
        assert!(parse(&["-m", "0", "x.csv"]).is_err());
        assert!(parse(&["--theme", "light", "x.csv"]).is_err());
        let cli = parse(&["-", "-"]).unwrap();
        assert_eq!(
            cli.validate(),
            Err("stdin (-) can only be given once".into())
        );
    }
}
