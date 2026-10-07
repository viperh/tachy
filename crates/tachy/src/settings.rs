//! Effective settings: command-line flags merged over the configuration and
//! the built-in defaults (spec §3, §15).

use std::{num::NonZeroUsize, path::PathBuf, thread};

use color_eyre::eyre::eyre;
use tachy_core::dialect::DialectOverrides;

use crate::{cli::Cli, config::Config};

/// Default memory budget: 2 GiB (spec §1, §3).
pub const DEFAULT_MEMORY: u64 = 2 << 30;
/// Default theme (spec §3).
pub const DEFAULT_THEME: &str = "dark";
/// Default number of frozen columns (spec §15).
pub const DEFAULT_FREEZE: usize = 1;
/// Default maximum column width in cells (spec §15).
pub const DEFAULT_MAX_COLUMN_WIDTH: u16 = 40;
/// Default values treated as null (spec §15).
pub const DEFAULT_NULL_VALUES: &[&str] = &["", "NULL", "null", "NA", "N/A", "\\N"];
/// Default sniff sample size: 64 KiB (spec §2.1).
pub const DEFAULT_SNIFF_SAMPLE_BYTES: usize = 64 * 1024;

/// Everything the app needs to know about how it was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Files to open, in order. `-` means stdin.
    pub files: Vec<PathBuf>,
    /// Dialect options given on the command line.
    pub dialect_overrides: DialectOverrides,
    /// Filter to apply on open (validated in M4-04).
    pub filter: Option<String>,
    /// Sort spec to apply on open (parsed in M5-03).
    pub sort: Option<String>,
    /// Accept the detected dialect without the dialog.
    pub yes: bool,
    /// Maximum number of concurrent blocking work items (README §A2). Never 0.
    pub threads: usize,
    /// Memory budget in bytes.
    pub memory: u64,
    /// Directory for spill and spool files.
    pub tmp_dir: PathBuf,
    /// Theme name.
    pub theme: String,
    /// Show the key-hint line.
    pub hints: bool,
    /// Show the inspector panel.
    pub inspector: bool,
    /// Number of frozen leading columns.
    pub freeze: usize,
    /// Maximum column width in cells.
    pub max_column_width: u16,
    /// Field values treated as null.
    pub null_values: Vec<String>,
    /// Bytes read by the dialect sniffer.
    pub sniff_sample_bytes: usize,
    /// Show the Detected format dialog.
    pub sniff_confirm: bool,
}

impl Settings {
    /// Merges `cli` over `cfg`: **CLI > user config > built-in default**
    /// (spec §15). The built-in defaults are already in `cfg.app` (they come
    /// from `.config/config.json`, see [`Config`]); the config has already
    /// validated its values.
    ///
    /// `--yes` forces `sniff_confirm = false`. Fails (a runtime error, exit
    /// code 1) when `--tmp` is not an existing directory.
    pub fn resolve(cli: &Cli, cfg: &Config) -> color_eyre::Result<Settings> {
        let app = &cfg.app;
        let tmp_dir = match &cli.tmp {
            Some(dir) => {
                if !dir.exists() {
                    return Err(eyre!("--tmp {}: directory does not exist", dir.display()));
                }
                if !dir.is_dir() {
                    return Err(eyre!("--tmp {}: not a directory", dir.display()));
                }
                dir.clone()
            }
            None => app.tmp_dir.clone().unwrap_or_else(std::env::temp_dir),
        };

        let threads = match cli.threads.unwrap_or(app.threads) {
            0 => logical_cpus(),
            n => n,
        };

        Ok(Settings {
            files: cli.files.clone(),
            dialect_overrides: DialectOverrides {
                delimiter: cli.delimiter,
                quote: cli.quote.map(|q| q.0),
                escape: cli.escape.map(Into::into),
                header: cli.header(),
                encoding: cli.encoding.map(|e| e.encoding()),
                comment: cli.comment,
            },
            filter: cli.filter.clone(),
            sort: cli.sort.clone(),
            yes: cli.yes,
            threads,
            memory: cli.mem.unwrap_or(app.memory),
            tmp_dir,
            theme: cli.theme.clone().unwrap_or_else(|| app.theme.clone()),
            hints: app.hints,
            inspector: app.inspector,
            freeze: app.freeze,
            max_column_width: app.max_column_width,
            null_values: app.null_values.clone(),
            sniff_sample_bytes: app.sniff.sample_bytes,
            sniff_confirm: app.sniff.confirm && !cli.yes,
        })
    }
}

/// Number of logical CPUs, or 1 if it cannot be determined.
fn logical_cpus() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use tachy_core::dialect::{Encoding, EscapeStyle};

    use super::*;
    use crate::config::{AppSettings, SniffSettings};

    fn resolve(args: &[&str]) -> color_eyre::Result<Settings> {
        let cli = Cli::try_parse_from(std::iter::once("tachy").chain(args.iter().copied()))?;
        Settings::resolve(&cli, &Config::default())
    }

    #[test]
    fn defaults() {
        let s = resolve(&["x.csv"]).unwrap();
        assert_eq!(s.files, [PathBuf::from("x.csv")]);
        assert_eq!(s.dialect_overrides, DialectOverrides::default());
        assert_eq!(s.threads, logical_cpus());
        assert_eq!(s.memory, 2 * 1024 * 1024 * 1024);
        assert_eq!(s.tmp_dir, std::env::temp_dir());
        assert_eq!(s.theme, "dark");
        assert!(s.hints && s.inspector && s.sniff_confirm && !s.yes);
        assert_eq!(s.freeze, 1);
        assert_eq!(s.max_column_width, 40);
        assert_eq!(s.null_values, ["", "NULL", "null", "NA", "N/A", "\\N"]);
        assert_eq!(s.sniff_sample_bytes, 65_536);
    }

    #[test]
    fn cli_flags_override_defaults() {
        let tmp = std::env::temp_dir();
        let s = resolve(&[
            "-d",
            "pipe",
            "-q",
            "none",
            "--escape",
            "backslash",
            "--no-header",
            "--encoding",
            "latin1",
            "--comment",
            "#",
            "-j",
            "3",
            "-m",
            "512M",
            "--tmp",
            tmp.to_str().unwrap(),
            "-y",
            "x.csv",
        ])
        .unwrap();
        assert_eq!(
            s.dialect_overrides,
            DialectOverrides {
                delimiter: Some(b'|'),
                quote: Some(None),
                escape: Some(EscapeStyle::Backslash),
                header: Some(false),
                encoding: Some(Encoding::Windows1252),
                comment: Some(b'#'),
            }
        );
        assert_eq!(s.threads, 3);
        assert_eq!(s.memory, 536_870_912);
        assert_eq!(s.tmp_dir, tmp);
        assert!(s.yes);
    }

    fn resolve_with(args: &[&str], cfg: &Config) -> Settings {
        let cli =
            Cli::try_parse_from(std::iter::once("tachy").chain(args.iter().copied())).unwrap();
        Settings::resolve(&cli, cfg).unwrap()
    }

    /// A config whose every §15 value differs from the built-in default.
    fn custom_config(tmp: &std::path::Path) -> Config {
        Config {
            app: AppSettings {
                theme: "custom".into(),
                threads: 8,
                memory: 1 << 30,
                tmp_dir: Some(tmp.to_path_buf()),
                hints: false,
                inspector: false,
                freeze: 3,
                max_column_width: 80,
                null_values: vec!["-".into()],
                sniff: SniffSettings {
                    sample_bytes: 4096,
                    confirm: false,
                },
                views: vec![],
            },
            ..Config::default()
        }
    }

    /// Config > built-in default, for every key.
    #[test]
    fn config_overrides_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let s = resolve_with(&["x.csv"], &custom_config(tmp.path()));
        assert_eq!(s.theme, "custom");
        assert_eq!(s.threads, 8);
        assert_eq!(s.memory, 1 << 30);
        assert_eq!(s.tmp_dir, tmp.path());
        assert!(!s.hints && !s.inspector && !s.sniff_confirm);
        assert_eq!(s.freeze, 3);
        assert_eq!(s.max_column_width, 80);
        assert_eq!(s.null_values, ["-"]);
        assert_eq!(s.sniff_sample_bytes, 4096);
    }

    /// CLI > config, for every overridable key.
    #[test]
    fn cli_overrides_config() {
        let cfg_tmp = tempfile::tempdir().unwrap();
        let cli_tmp = tempfile::tempdir().unwrap();
        let cfg = custom_config(cfg_tmp.path());
        let s = resolve_with(
            &[
                "-j",
                "4",
                "-m",
                "512M",
                "--tmp",
                cli_tmp.path().to_str().unwrap(),
                "--theme",
                "dark",
                "x.csv",
            ],
            &cfg,
        );
        assert_eq!(s.threads, 4);
        assert_eq!(s.memory, 512 << 20);
        assert_eq!(s.tmp_dir, cli_tmp.path());
        assert_eq!(s.theme, "dark");
        // `-j 0` means logical CPUs even when the config says 8.
        assert_eq!(
            resolve_with(&["-j", "0", "x.csv"], &cfg).threads,
            logical_cpus()
        );
    }

    #[test]
    fn yes_forces_sniff_confirm_off() {
        let mut cfg = Config::default();
        assert!(resolve_with(&["x.csv"], &cfg).sniff_confirm);
        assert!(!resolve_with(&["-y", "x.csv"], &cfg).sniff_confirm);
        cfg.app.sniff.confirm = false;
        assert!(!resolve_with(&["x.csv"], &cfg).sniff_confirm);
    }

    #[test]
    fn config_threads_zero_means_logical_cpus() {
        let cfg = Config::default();
        assert_eq!(cfg.app.threads, 0);
        assert_eq!(resolve_with(&["x.csv"], &cfg).threads, logical_cpus());
    }

    #[test]
    fn zero_threads_means_logical_cpus() {
        assert_eq!(
            resolve(&["-j", "0", "x.csv"]).unwrap().threads,
            logical_cpus()
        );
    }

    #[test]
    fn tmp_must_be_an_existing_directory() {
        let err = resolve(&["--tmp", "/definitely/not/here", "x.csv"]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        let file = std::env::current_exe().unwrap();
        let err = resolve(&["--tmp", file.to_str().unwrap(), "x.csv"]).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }
}
