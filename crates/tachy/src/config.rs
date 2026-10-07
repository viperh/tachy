//! Configuration (spec §15, README §A4, M6-03).
//!
//! The built-in defaults are `.config/config.json` (JSON5), embedded in the
//! binary. User files (`config.json5`, `config.json`, `config.yaml`,
//! `config.toml`, `config.ini` in [`get_config_dir`]) are layered on top:
//! **user config > built-in default**. The command line wins over both, in
//! [`Settings::resolve`](crate::settings::Settings::resolve).
//!
//! Loading never fails and never panics. Every problem becomes a
//! [`ConfigWarning`], shown as a toast once the TUI is up:
//! - a file that does not parse is ignored as a whole (error toast with the
//!   file and the parser's position);
//! - a bad value (wrong type, out of range, unknown theme) falls back to the
//!   default for that key only;
//! - a bad key binding (invalid chord, unknown context or action) is skipped;
//! - an unknown top-level key is reported with a "did you mean" suggestion.
//!
//! Key bindings from the user are added on top of the defaults: a user entry
//! for a chord replaces the default action of that chord, and the value
//! `"None"` removes the default binding.

use std::{
    collections::HashMap,
    env, fmt,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use config::{ConfigError, FileFormat, Map, Value};
use directories::ProjectDirs;
use ratatui::style::{Color, Modifier, Style};
use serde::{
    Deserialize,
    de::{DeserializeOwned, Deserializer, IntoDeserializer},
};
use tachy_core::size::parse_size;
use tracing::{debug, warn};

use crate::{
    action::Action,
    keymap::Keymap,
    keymap::parse_key_chord,
    mode::{KeyContext, Mode},
    settings::{
        DEFAULT_FREEZE, DEFAULT_MAX_COLUMN_WIDTH, DEFAULT_MEMORY, DEFAULT_NULL_VALUES,
        DEFAULT_SNIFF_SAMPLE_BYTES, DEFAULT_THEME,
    },
    theme::Theme,
    toast::{Toast, ToastLevel},
};

/// The default config, baked into the binary at compile time. User config
/// files found in [`get_config_dir`] are layered on top of it.
const CONFIG: &str = include_str!("../../../.config/config.json");

/// Reverse-domain qualifier and organisation used to locate the per-user
/// config and data directories. Change these when you rename the project.
const APP_QUALIFIER: &str = "me";
const APP_ORGANIZATION: &str = "viperh";

/// User config files, in the order they are layered (later wins).
const USER_CONFIG_FILES: [(&str, FileFormat); 5] = [
    ("config.json5", FileFormat::Json5),
    ("config.json", FileFormat::Json),
    ("config.yaml", FileFormat::Yaml),
    ("config.toml", FileFormat::Toml),
    ("config.ini", FileFormat::Ini),
];

/// A key-binding value that removes the default binding of that chord.
pub const UNBIND: &str = "None";

/// Every top-level key the config understands. Anything else is reported.
const KNOWN_KEYS: &[&str] = &[
    "theme",
    "threads",
    "memory",
    "tmp_dir",
    "hints",
    "inspector",
    "freeze",
    "max_column_width",
    "null_values",
    "sniff",
    "keybindings",
    "views",
    "styles",
    "data_dir",
    "config_dir",
];
/// The keys of the `sniff` table.
const KNOWN_SNIFF_KEYS: &[&str] = &["sample_bytes", "confirm"];

/// Allowed `max_column_width` (cells).
pub const MAX_COLUMN_WIDTH_RANGE: RangeInclusive<u64> = 4..=1000;
/// Allowed `sniff.sample_bytes`: 4 KiB to 16 MiB.
pub const SNIFF_SAMPLE_BYTES_RANGE: RangeInclusive<u64> = 4 << 10..=16 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppConfig {
    #[allow(dead_code)] // Kept from the template; the log path is computed in `logging`.
    pub data_dir: PathBuf,
    #[allow(dead_code)] // Kept from the template; M6-04 writes `views.json` here.
    pub config_dir: PathBuf,
}

/// The §15 settings. Every key is optional in the user file; a missing or
/// invalid key keeps the built-in default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    /// `theme`, overridden by `--theme`.
    pub theme: String,
    /// `threads` (0 = logical CPUs), overridden by `-j`.
    pub threads: usize,
    /// `memory` in bytes (`"2G"` or an integer), overridden by `-m`.
    pub memory: u64,
    /// `tmp_dir`; `None` = the system temp dir. Overridden by `--tmp`.
    pub tmp_dir: Option<PathBuf>,
    /// `hints`: show the key-hint line.
    pub hints: bool,
    /// `inspector`: show the inspector panel.
    pub inspector: bool,
    /// `freeze`: frozen leading columns.
    pub freeze: usize,
    /// `max_column_width` in cells, within [`MAX_COLUMN_WIDTH_RANGE`].
    pub max_column_width: u16,
    /// `null_values`: field values treated as null.
    pub null_values: Vec<String>,
    /// `sniff.*`.
    pub sniff: SniffSettings,
    /// `views`: read-only saved views (M6-04).
    pub views: Vec<ViewConfig>,
}

impl Default for AppSettings {
    /// The built-in defaults. `.config/config.json` holds the same values
    /// (checked by a test).
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            threads: 0,
            memory: DEFAULT_MEMORY,
            tmp_dir: None,
            hints: true,
            inspector: true,
            freeze: DEFAULT_FREEZE,
            max_column_width: DEFAULT_MAX_COLUMN_WIDTH,
            null_values: DEFAULT_NULL_VALUES.iter().map(|s| s.to_string()).collect(),
            sniff: SniffSettings::default(),
            views: Vec::new(),
        }
    }
}

/// The `sniff` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SniffSettings {
    /// `sniff.sample_bytes`, within [`SNIFF_SAMPLE_BYTES_RANGE`].
    pub sample_bytes: usize,
    /// `sniff.confirm`: show the Detected format dialog. `--yes` turns it off.
    pub confirm: bool,
}

impl Default for SniffSettings {
    fn default() -> Self {
        Self {
            sample_bytes: DEFAULT_SNIFF_SAMPLE_BYTES,
            confirm: true,
        }
    }
}

/// One entry of the read-only `views` array (M6-04).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ViewConfig {
    pub name: String,
    /// Files the view applies to; `None` = every file.
    #[serde(default)]
    pub file_glob: Option<String>,
    pub filter: String,
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    #[allow(dead_code)] // The template's data/config dirs; see `AppConfig`.
    pub config: AppConfig,
    /// The §15 settings.
    pub app: AppSettings,
    pub keybindings: KeyBindings,
    #[allow(dead_code)] // The template's styles: loaded, unused.
    pub styles: Styles,
    /// Problems found by [`Config::new`]. `App::new` takes them (leaving this
    /// empty) and shows them as toasts ([`warning_toasts`]).
    pub warnings: Vec<ConfigWarning>,
}

/// Upper-cased crate name, used as the prefix for the `*_DATA`, `*_CONFIG`
/// and `*_LOG_LEVEL` environment variables (see `.envrc`).
pub static PROJECT_NAME: LazyLock<String> =
    LazyLock::new(|| env!("CARGO_CRATE_NAME").to_uppercase().to_string());
pub static DATA_FOLDER: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    env::var(format!("{}_DATA", PROJECT_NAME.clone()))
        .ok()
        .map(PathBuf::from)
});
pub static CONFIG_FOLDER: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    env::var(format!("{}_CONFIG", PROJECT_NAME.clone()))
        .ok()
        .map(PathBuf::from)
});

/// A problem found while loading the config. None of them stops tachy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigWarning {
    /// A user config file could not be read or parsed; it was ignored.
    Parse { file: PathBuf, message: String },
    /// A key binding was skipped.
    Binding {
        context: String,
        key: String,
        reason: String,
    },
    /// A key the config does not know (a typo, usually).
    UnknownKey {
        key: String,
        suggestion: Option<String>,
    },
    /// A value of the wrong type or out of range; the default is used.
    InvalidValue { key: String, message: String },
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigWarning::Parse { file, message } => {
                write!(f, "config error: {}: {message}", file.display())
            }
            ConfigWarning::Binding {
                context,
                key,
                reason,
            } => write!(
                f,
                "config: keybindings.{context} \"{key}\": {reason}; ignored"
            ),
            ConfigWarning::UnknownKey { key, suggestion } => {
                write!(f, "config: unknown key \"{key}\"")?;
                if let Some(s) = suggestion {
                    write!(f, " (did you mean \"{s}\"?)")?;
                }
                Ok(())
            }
            ConfigWarning::InvalidValue { key, message } => {
                write!(f, "config: {key}: {message}; using the default")
            }
        }
    }
}

/// The toasts for `warnings`: one per warning, except that all binding
/// warnings share one toast (`config: 3 key bindings ignored — see log`).
/// Parse errors are error toasts, the rest are warnings. Toasts are one line,
/// so only the first line of a multi-line parser message is shown.
pub fn warning_toasts(warnings: &[ConfigWarning]) -> Vec<Toast> {
    let mut toasts = Vec::new();
    let mut bindings = 0;
    for w in warnings {
        match w {
            ConfigWarning::Binding { .. } => bindings += 1,
            ConfigWarning::Parse { .. } => {
                let text = w.to_string();
                let first = text.lines().next().unwrap_or_default().trim_end();
                toasts.push(Toast::new(ToastLevel::Error, first));
            }
            _ => toasts.push(Toast::new(ToastLevel::Warning, w.to_string())),
        }
    }
    if bindings > 0 {
        let noun = if bindings == 1 { "binding" } else { "bindings" };
        toasts.push(Toast::new(
            ToastLevel::Warning,
            format!("config: {bindings} key {noun} ignored — see log"),
        ));
    }
    toasts
}

impl Config {
    /// The built-in defaults only (`.config/config.json`), without any user
    /// file. Tests use it so they don't depend on the user's config.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn embedded() -> Self {
        let (cfg, warnings) = Self::build(&defaults_source(), None, AppConfig::default());
        debug_assert!(warnings.is_empty(), "embedded config: {warnings:?}");
        cfg
    }

    /// Loads the defaults and the user files in [`get_config_dir`]. The
    /// warnings are kept in [`Config::warnings`] for the app to show.
    pub fn new() -> Self {
        let (mut cfg, warnings) = Self::load_from(&get_config_dir());
        cfg.warnings = warnings;
        cfg
    }

    /// Loads the defaults and the user files in `config_dir`. Never fails:
    /// see the module docs. The returned config's `warnings` is empty.
    pub fn load_from(config_dir: &Path) -> (Self, Vec<ConfigWarning>) {
        let mut warnings = Vec::new();
        let user = load_user_files(config_dir, &mut warnings);
        let app_config = AppConfig {
            data_dir: get_data_dir(),
            config_dir: config_dir.to_path_buf(),
        };
        let (cfg, more) = Self::build(&defaults_source(), user.as_ref(), app_config);
        warnings.extend(more);
        for w in &warnings {
            match w {
                // Shown as one summary toast, so each one is logged here.
                ConfigWarning::Binding { .. } => warn!("{w}"),
                // Logged at their level when pushed as toasts.
                _ => debug!("{w}"),
            }
        }
        (cfg, warnings)
    }

    /// Defaults, then the user layer on top.
    fn build(
        defaults: &config::Config,
        user: Option<&config::Config>,
        mut app_config: AppConfig,
    ) -> (Self, Vec<ConfigWarning>) {
        let mut w = Vec::new();
        let mut app = AppSettings::default();
        let mut keybindings = Keymap::default();
        app.apply(defaults, &mut w);
        apply_keybindings(&mut keybindings, defaults, &mut w);
        let mut styles = get::<Styles>(defaults, "styles", &mut w).unwrap_or_default();

        if let Some(user) = user {
            check_unknown_keys(user, &mut w);
            app.apply(user, &mut w);
            apply_keybindings(&mut keybindings, user, &mut w);
            if let Some(user_styles) = get::<Styles>(user, "styles", &mut w) {
                for (mode, entries) in user_styles.0 {
                    styles.0.entry(mode).or_default().extend(entries);
                }
            }
            if let Some(dir) = get::<String>(user, "data_dir", &mut w) {
                app_config.data_dir = PathBuf::from(dir);
            }
            if let Some(dir) = get::<String>(user, "config_dir", &mut w) {
                app_config.config_dir = PathBuf::from(dir);
            }
        }

        let cfg = Config {
            config: app_config,
            app,
            keybindings,
            styles,
            warnings: Vec::new(),
        };
        (cfg, w)
    }
}

/// The embedded defaults as a `config` source.
fn defaults_source() -> config::Config {
    config::Config::builder()
        .add_source(config::File::from_str(CONFIG, FileFormat::Json5))
        .build()
        .expect("the embedded config is valid")
}

/// Builds the user layer from the files in `dir`. Each file is parsed on its
/// own, so one broken file is reported with its name and skipped while the
/// others still apply. `None` when there is no usable user file.
fn load_user_files(dir: &Path, warnings: &mut Vec<ConfigWarning>) -> Option<config::Config> {
    let mut builder = config::Config::builder();
    let mut loaded = false;
    for (name, format) in USER_CONFIG_FILES {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let source = config::File::from(path.clone()).format(format);
        match config::Config::builder().add_source(source).build() {
            Ok(layer) => {
                builder = builder.add_source(layer);
                loaded = true;
            }
            Err(e) => warnings.push(ConfigWarning::Parse {
                file: path,
                message: parse_message(&e),
            }),
        }
    }
    if !loaded {
        // Having no user config is the normal case.
        debug!(dir = %dir.display(), "no user configuration file");
        return None;
    }
    match builder.build() {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            warnings.push(ConfigWarning::Parse {
                file: dir.to_path_buf(),
                message: parse_message(&e),
            });
            None
        }
    }
}

/// The parser's own message (with its line and column), without the
/// `in <path>` suffix the `config` crate adds: the path is shown separately.
fn parse_message(e: &ConfigError) -> String {
    match e {
        ConfigError::FileParse { cause, .. } => cause.to_string(),
        other => other.to_string(),
    }
}

/// Reads `key` from `src`. Missing → `None`. Present but invalid → `None` and
/// an [`ConfigWarning::InvalidValue`].
fn get<T: DeserializeOwned>(
    src: &config::Config,
    key: &str,
    w: &mut Vec<ConfigWarning>,
) -> Option<T> {
    match src.get::<T>(key) {
        Ok(v) => Some(v),
        Err(ConfigError::NotFound(_)) => None,
        Err(e) => {
            invalid(w, key, e.to_string());
            None
        }
    }
}

fn invalid(w: &mut Vec<ConfigWarning>, key: &str, message: impl Into<String>) {
    w.push(ConfigWarning::InvalidValue {
        key: key.to_owned(),
        message: message.into(),
    });
}

/// Reads an integer and checks it against `range`.
fn get_in_range(
    src: &config::Config,
    key: &str,
    range: RangeInclusive<u64>,
    w: &mut Vec<ConfigWarning>,
) -> Option<u64> {
    let v = get::<u64>(src, key, w)?;
    if range.contains(&v) {
        Some(v)
    } else {
        invalid(
            w,
            key,
            format!("{v} is out of range ({}–{})", range.start(), range.end()),
        );
        None
    }
}

impl AppSettings {
    /// Overrides every §15 key present in `src`, validating each one.
    fn apply(&mut self, src: &config::Config, w: &mut Vec<ConfigWarning>) {
        if let Some(theme) = get::<String>(src, "theme", w) {
            if Theme::NAMES.contains(&theme.as_str()) {
                self.theme = theme;
            } else {
                invalid(
                    w,
                    "theme",
                    format!(
                        "unknown theme \"{theme}\" (available: {})",
                        Theme::NAMES.join(", ")
                    ),
                );
            }
        }
        if let Some(threads) = get::<usize>(src, "threads", w) {
            self.threads = threads;
        }
        // A size string (`"2G"`) or a plain number of bytes; the `config`
        // crate turns numbers into strings on request.
        if let Some(memory) = get::<String>(src, "memory", w) {
            match parse_size(&memory) {
                Ok(bytes) => self.memory = bytes,
                Err(e) => invalid(w, "memory", e.to_string()),
            }
        }
        if let Some(dir) = get::<String>(src, "tmp_dir", w) {
            let dir = PathBuf::from(dir);
            if dir.is_dir() {
                self.tmp_dir = Some(dir);
            } else {
                invalid(
                    w,
                    "tmp_dir",
                    format!("{} is not an existing directory", dir.display()),
                );
            }
        }
        if let Some(hints) = get::<bool>(src, "hints", w) {
            self.hints = hints;
        }
        if let Some(inspector) = get::<bool>(src, "inspector", w) {
            self.inspector = inspector;
        }
        if let Some(freeze) = get::<usize>(src, "freeze", w) {
            self.freeze = freeze;
        }
        if let Some(width) = get_in_range(src, "max_column_width", MAX_COLUMN_WIDTH_RANGE, w) {
            self.max_column_width = width as u16;
        }
        if let Some(nulls) = get::<Vec<String>>(src, "null_values", w) {
            self.null_values = nulls;
        }
        if let Some(bytes) = get_in_range(src, "sniff.sample_bytes", SNIFF_SAMPLE_BYTES_RANGE, w) {
            self.sniff.sample_bytes = bytes as usize;
        }
        if let Some(confirm) = get::<bool>(src, "sniff.confirm", w) {
            self.sniff.confirm = confirm;
        }
        if let Some(views) = get::<Vec<ViewConfig>>(src, "views", w) {
            self.views = views;
        }
    }
}

/// Applies the `keybindings` table of `src` on top of `keymap`. Bad entries
/// are skipped with a [`ConfigWarning::Binding`].
fn apply_keybindings(keymap: &mut Keymap, src: &config::Config, w: &mut Vec<ConfigWarning>) {
    let Some(contexts) = get::<Map<String, Value>>(src, "keybindings", w) else {
        return;
    };
    let mut contexts: Vec<_> = contexts.into_iter().collect();
    contexts.sort_by(|a, b| a.0.cmp(&b.0));
    for (context_name, entries) in contexts {
        let entries = match entries.into_table() {
            Ok(entries) => entries,
            Err(e) => {
                w.push(ConfigWarning::Binding {
                    context: context_name,
                    key: String::new(),
                    reason: format!("expected a table of bindings ({e})"),
                });
                continue;
            }
        };
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let Some(context) = parse_context(&context_name) else {
            for (key, _) in entries {
                w.push(ConfigWarning::Binding {
                    context: context_name.clone(),
                    key,
                    reason: "unknown key context".to_owned(),
                });
            }
            continue;
        };
        let map = keymap.0.entry(context).or_default();
        for (key, value) in entries {
            let mut skip = |reason: String| {
                w.push(ConfigWarning::Binding {
                    context: context_name.clone(),
                    key: key.clone(),
                    reason,
                })
            };
            let action = match value.into_string() {
                Ok(action) => action,
                Err(e) => {
                    skip(format!("expected an action name ({e})"));
                    continue;
                }
            };
            let chord = match parse_key_chord(&key) {
                Ok(chord) => chord,
                Err(e) => {
                    skip(e);
                    continue;
                }
            };
            if action == UNBIND {
                map.remove(&chord);
                continue;
            }
            match parse_action(&action) {
                Some(action) => {
                    map.insert(chord, action);
                }
                None => skip(format!("unknown action \"{action}\"")),
            }
        }
    }
}

/// A [`KeyContext`] by its config name (`"Normal"`, `"JobsDrawer"`, ...).
fn parse_context(name: &str) -> Option<KeyContext> {
    let de: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
        name.into_deserializer();
    KeyContext::deserialize(de).ok()
}

/// A bindable [`Action`] by its variant name. Internal variants (`Tick`,
/// `Render`, ...) are not bindable.
fn parse_action(name: &str) -> Option<Action> {
    Action::all()
        .iter()
        .find(|a| a.to_string() == name)
        .cloned()
}

/// Reports unknown top-level keys and unknown keys of the `sniff` table.
fn check_unknown_keys(user: &config::Config, w: &mut Vec<ConfigWarning>) {
    let Ok(top) = user.clone().try_deserialize::<Map<String, Value>>() else {
        return;
    };
    let mut keys: Vec<_> = top.keys().collect();
    keys.sort();
    for key in keys {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            w.push(ConfigWarning::UnknownKey {
                key: key.clone(),
                suggestion: closest(key, KNOWN_KEYS).map(str::to_owned),
            });
        }
    }
    if let Some(Ok(sniff)) = top.get("sniff").map(|v| v.clone().into_table()) {
        let mut keys: Vec<_> = sniff.keys().collect();
        keys.sort();
        for key in keys {
            if !KNOWN_SNIFF_KEYS.contains(&key.as_str()) {
                w.push(ConfigWarning::UnknownKey {
                    key: format!("sniff.{key}"),
                    suggestion: closest(key, KNOWN_SNIFF_KEYS).map(|s| format!("sniff.{s}")),
                });
            }
        }
    }
}

/// The known key closest to `key`, if it is a plausible typo (edit distance
/// at most 2, or 1 for short keys).
fn closest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let max = if key.chars().count() <= 4 { 1 } else { 2 };
    known
        .iter()
        .map(|k| (edit_distance(key, k), *k))
        .filter(|(d, _)| *d <= max)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Levenshtein distance over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

pub fn get_data_dir() -> PathBuf {
    if let Some(s) = DATA_FOLDER.clone() {
        s
    } else if let Some(proj_dirs) = project_directory() {
        proj_dirs.data_local_dir().to_path_buf()
    } else {
        PathBuf::from(".").join(".data")
    }
}

pub fn get_config_dir() -> PathBuf {
    if let Some(s) = CONFIG_FOLDER.clone() {
        s
    } else if let Some(proj_dirs) = project_directory() {
        proj_dirs.config_local_dir().to_path_buf()
    } else {
        PathBuf::from(".").join(".config")
    }
}

fn project_directory() -> Option<ProjectDirs> {
    ProjectDirs::from(APP_QUALIFIER, APP_ORGANIZATION, env!("CARGO_PKG_NAME"))
}

/// Key bindings per [`KeyContext`] (M1-06). See [`crate::keymap`].
pub type KeyBindings = Keymap;

#[derive(Clone, Debug, Default)]
pub struct Styles(pub HashMap<Mode, HashMap<String, Style>>);

impl<'de> Deserialize<'de> for Styles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let parsed_map = HashMap::<Mode, HashMap<String, String>>::deserialize(deserializer)?;

        let styles = parsed_map
            .into_iter()
            .map(|(mode, inner_map)| {
                let converted_inner_map = inner_map
                    .into_iter()
                    .map(|(str, style)| (str, parse_style(&style)))
                    .collect();
                (mode, converted_inner_map)
            })
            .collect();

        Ok(Styles(styles))
    }
}

pub fn parse_style(line: &str) -> Style {
    let (foreground, background) =
        line.split_at(line.to_lowercase().find("on ").unwrap_or(line.len()));
    let foreground = process_color_string(foreground);
    let background = process_color_string(&background.replace("on ", ""));

    let mut style = Style::default();
    if let Some(fg) = parse_color(&foreground.0) {
        style = style.fg(fg);
    }
    if let Some(bg) = parse_color(&background.0) {
        style = style.bg(bg);
    }
    style = style.add_modifier(foreground.1 | background.1);
    style
}

fn process_color_string(color_str: &str) -> (String, Modifier) {
    let color = color_str
        .replace("grey", "gray")
        .replace("bright ", "")
        .replace("bold ", "")
        .replace("underline ", "")
        .replace("inverse ", "");

    let mut modifiers = Modifier::empty();
    if color_str.contains("underline") {
        modifiers |= Modifier::UNDERLINED;
    }
    if color_str.contains("bold") {
        modifiers |= Modifier::BOLD;
    }
    if color_str.contains("inverse") {
        modifiers |= Modifier::REVERSED;
    }

    (color, modifiers)
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim_start();
    let s = s.trim_end();
    if s.contains("bright color") {
        let s = s.trim_start_matches("bright ");
        let c = s
            .trim_start_matches("color")
            .parse::<u8>()
            .unwrap_or_default();
        Some(Color::Indexed(c.wrapping_shl(8)))
    } else if s.contains("color") {
        let c = s
            .trim_start_matches("color")
            .parse::<u8>()
            .unwrap_or_default();
        Some(Color::Indexed(c))
    } else if s.contains("gray") {
        let c = 232
            + s.trim_start_matches("gray")
                .parse::<u8>()
                .unwrap_or_default();
        Some(Color::Indexed(c))
    } else if s.contains("rgb") {
        let red = (s.as_bytes()[3] as char).to_digit(10).unwrap_or_default() as u8;
        let green = (s.as_bytes()[4] as char).to_digit(10).unwrap_or_default() as u8;
        let blue = (s.as_bytes()[5] as char).to_digit(10).unwrap_or_default() as u8;
        let c = 16 + red * 36 + green * 6 + blue;
        Some(Color::Indexed(c))
    } else if s == "bold black" {
        Some(Color::Indexed(8))
    } else if s == "bold red" {
        Some(Color::Indexed(9))
    } else if s == "bold green" {
        Some(Color::Indexed(10))
    } else if s == "bold yellow" {
        Some(Color::Indexed(11))
    } else if s == "bold blue" {
        Some(Color::Indexed(12))
    } else if s == "bold magenta" {
        Some(Color::Indexed(13))
    } else if s == "bold cyan" {
        Some(Color::Indexed(14))
    } else if s == "bold white" {
        Some(Color::Indexed(15))
    } else if s == "black" {
        Some(Color::Indexed(0))
    } else if s == "red" {
        Some(Color::Indexed(1))
    } else if s == "green" {
        Some(Color::Indexed(2))
    } else if s == "yellow" {
        Some(Color::Indexed(3))
    } else if s == "blue" {
        Some(Color::Indexed(4))
    } else if s == "magenta" {
        Some(Color::Indexed(5))
    } else if s == "cyan" {
        Some(Color::Indexed(6))
    } else if s == "white" {
        Some(Color::Indexed(7))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_parse_style_default() {
        let style = parse_style("");
        assert_eq!(style, Style::default());
    }

    #[test]
    fn test_parse_style_foreground() {
        let style = parse_style("red");
        assert_eq!(style.fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn test_parse_style_background() {
        let style = parse_style("on blue");
        assert_eq!(style.bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn test_parse_style_modifiers() {
        let style = parse_style("underline red on blue");
        assert_eq!(style.fg, Some(Color::Indexed(1)));
        assert_eq!(style.bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn test_process_color_string() {
        let (color, modifiers) = process_color_string("underline bold inverse gray");
        assert_eq!(color, "gray");
        assert!(modifiers.contains(Modifier::UNDERLINED));
        assert!(modifiers.contains(Modifier::BOLD));
        assert!(modifiers.contains(Modifier::REVERSED));
    }

    #[test]
    fn test_parse_color_rgb() {
        let color = parse_color("rgb123");
        let expected = 16 + 36 + 2 * 6 + 3;
        assert_eq!(color, Some(Color::Indexed(expected)));
    }

    #[test]
    fn test_parse_color_unknown() {
        let color = parse_color("unknown");
        assert_eq!(color, None);
    }

    fn resolve(c: &Config, context: KeyContext, key: &str) -> Option<Action> {
        c.keybindings
            .resolve(context, parse_key_chord(key).unwrap())
            .cloned()
    }

    /// Writes `files` into a fresh directory and loads it.
    fn load(files: &[(&str, &str)]) -> (Config, Vec<ConfigWarning>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        Config::load_from(dir.path())
    }

    /// `tmp_dir` of the §15 example: `/var/tmp` exists on Unix only.
    fn example_tmp_dir() -> String {
        if cfg!(unix) {
            "/var/tmp".to_owned()
        } else {
            std::env::temp_dir()
                .display()
                .to_string()
                .replace('\\', "/")
        }
    }

    #[test]
    fn test_config() {
        let (c, warnings) = load(&[]);
        assert_eq!(warnings, []);
        assert_eq!(
            resolve(&c, KeyContext::Normal, "<ctrl-q>"),
            Some(Action::Quit)
        );
        assert_eq!(
            resolve(&c, KeyContext::Normal, "<ctrl-c>"),
            Some(Action::Quit)
        );
    }

    #[test]
    fn no_user_file_is_identical_to_the_defaults() {
        let (c, warnings) = load(&[]);
        assert_eq!(warnings, []);
        let embedded = Config::embedded();
        assert_eq!(c.app, embedded.app);
        assert_eq!(c.keybindings, embedded.keybindings);
        assert_eq!(c.app, AppSettings::default());
    }

    /// `.config/config.json` and `AppSettings::default()` agree.
    #[test]
    fn embedded_matches_built_in_defaults() {
        let mut w = Vec::new();
        let defaults = defaults_source();
        let mut app = AppSettings {
            theme: "x".into(),
            threads: 99,
            memory: 1,
            tmp_dir: None,
            hints: false,
            inspector: false,
            freeze: 9,
            max_column_width: 9,
            null_values: vec![],
            sniff: SniffSettings {
                sample_bytes: 1,
                confirm: false,
            },
            views: vec![ViewConfig {
                name: "x".into(),
                file_glob: None,
                filter: "x".into(),
            }],
        };
        app.apply(&defaults, &mut w);
        assert_eq!(w, []);
        assert_eq!(app, AppSettings::default());
    }

    const EXAMPLE_JSON5: &str = r#"// ~/.config/tachy/config.json5
{
  theme: "dark", threads: 0, memory: "2G", tmp_dir: "TMP",
  hints: true, inspector: true, freeze: 1, max_column_width: 40,
  null_values: ["", "NULL", "null", "NA", "N/A", "\\N"],
  sniff: { sample_bytes: 65536, confirm: true },
  keybindings: { Normal: { "<ctrl-f>": "Filter", "<Q>": "Quit" } },
  views: [ { name: "DE big orders", file_glob: "orders_*.csv", filter: 'country == "DE" && price > 100' } ],
}
"#;

    const EXAMPLE_TOML: &str = r#"
theme = "dark"
threads = 0
memory = "2G"
tmp_dir = "TMP"
hints = true
inspector = true
freeze = 1
max_column_width = 40
null_values = ["", "NULL", "null", "NA", "N/A", '\N']

[sniff]
sample_bytes = 65536
confirm = true

[keybindings.Normal]
"<ctrl-f>" = "Filter"
"<Q>" = "Quit"

[[views]]
name = "DE big orders"
file_glob = "orders_*.csv"
filter = 'country == "DE" && price > 100'
"#;

    fn check_example(c: &Config, warnings: &[ConfigWarning]) {
        assert_eq!(warnings, []);
        let expected = AppSettings {
            tmp_dir: Some(PathBuf::from(example_tmp_dir())),
            views: vec![ViewConfig {
                name: "DE big orders".into(),
                file_glob: Some("orders_*.csv".into()),
                filter: r#"country == "DE" && price > 100"#.into(),
            }],
            ..AppSettings::default()
        };
        assert_eq!(c.app, expected);
        assert_eq!(c.app.null_values[5], "\\N");
        assert_eq!(
            resolve(c, KeyContext::Normal, "ctrl-f"),
            Some(Action::Filter)
        );
        assert_eq!(resolve(c, KeyContext::Normal, "Q"), Some(Action::Quit));
        // Defaults are still there.
        assert_eq!(resolve(c, KeyContext::Normal, "f"), Some(Action::Filter));
        assert_eq!(resolve(c, KeyContext::Normal, "q"), Some(Action::Quit));
    }

    #[test]
    fn spec_example_loads_as_json5() {
        let text = EXAMPLE_JSON5.replace("TMP", &example_tmp_dir());
        let (c, warnings) = load(&[("config.json5", &text)]);
        check_example(&c, &warnings);
    }

    #[test]
    fn spec_example_loads_as_toml() {
        let text = EXAMPLE_TOML.replace("TMP", &example_tmp_dir());
        let (c, warnings) = load(&[("config.toml", &text)]);
        check_example(&c, &warnings);
    }

    #[test]
    fn user_values_override_defaults() {
        let (c, warnings) = load(&[(
            "config.json5",
            r#"{ threads: 8, memory: 536870912, hints: false, inspector: false,
                 freeze: 0, max_column_width: 1000, null_values: ["-"],
                 sniff: { sample_bytes: 4096, confirm: false } }"#,
        )]);
        assert_eq!(warnings, []);
        assert_eq!(c.app.threads, 8);
        assert_eq!(c.app.memory, 512 << 20);
        assert!(!c.app.hints && !c.app.inspector && !c.app.sniff.confirm);
        assert_eq!(c.app.freeze, 0);
        assert_eq!(c.app.max_column_width, 1000);
        assert_eq!(c.app.null_values, ["-"]);
        assert_eq!(c.app.sniff.sample_bytes, 4096);
        // Untouched keys keep their defaults.
        assert_eq!(c.app.theme, "dark");
        assert_eq!(c.app.tmp_dir, None);
    }

    #[test]
    fn none_unbinds_a_default_key() {
        let (c, warnings) = load(&[(
            "config.json5",
            r#"{ keybindings: { Normal: { "f": "None", "<ctrl-f>": "Filter" },
                               JobsDrawer: { "K": "None" } } }"#,
        )]);
        assert_eq!(warnings, []);
        assert_eq!(resolve(&c, KeyContext::Normal, "f"), None);
        assert_eq!(
            resolve(&c, KeyContext::Normal, "ctrl-f"),
            Some(Action::Filter)
        );
        assert_eq!(resolve(&c, KeyContext::JobsDrawer, "K"), None);
        // Other bindings of the context stay.
        assert_eq!(
            resolve(&c, KeyContext::JobsDrawer, "p"),
            Some(Action::PauseJob)
        );
    }

    #[test]
    fn a_user_binding_replaces_the_default_action_of_its_chord() {
        let (c, warnings) = load(&[(
            "config.json5",
            r#"{ keybindings: { Normal: { "q": "Help" } } }"#,
        )]);
        assert_eq!(warnings, []);
        assert_eq!(resolve(&c, KeyContext::Normal, "q"), Some(Action::Help));
        assert_eq!(
            resolve(&c, KeyContext::Normal, "ctrl-q"),
            Some(Action::Quit)
        );
    }

    #[test]
    fn bad_bindings_are_skipped_with_warnings() {
        let (c, warnings) = load(&[(
            "config.json5",
            r#"{ keybindings: {
                Normal: {
                    "<ctrl-foo>": "Quit",
                    "<g><g>": "FirstRow",
                    "x": "NoSuchAction",
                    "t": "Tick",
                    "z": 3,
                    "v": ["Quit"],
                    "ctrl-g": "LastRow",
                },
                Nowhere: { "a": "Quit", "b": "Help" },
            } }"#,
        )]);
        let reasons: Vec<(String, String, String)> = warnings
            .iter()
            .map(|w| match w {
                ConfigWarning::Binding {
                    context,
                    key,
                    reason,
                } => (context.clone(), key.clone(), reason.clone()),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(reasons.len(), 8, "{reasons:#?}");
        let has = |ctx: &str, key: &str, part: &str| {
            assert!(
                reasons
                    .iter()
                    .any(|(c, k, r)| c == ctx && k == key && r.contains(part)),
                "{ctx} {key} {part}: {reasons:#?}"
            )
        };
        has("Normal", "<ctrl-foo>", "unknown key");
        has("Normal", "<g><g>", "multi-key");
        has("Normal", "x", "unknown action \"NoSuchAction\"");
        has("Normal", "t", "unknown action \"Tick\"");
        has("Normal", "z", "unknown action \"3\"");
        has("Normal", "v", "expected an action name");
        has("Nowhere", "a", "unknown key context");
        has("Nowhere", "b", "unknown key context");
        // The good entry still applies; the defaults are intact.
        assert_eq!(
            resolve(&c, KeyContext::Normal, "ctrl-g"),
            Some(Action::LastRow)
        );
        assert_eq!(resolve(&c, KeyContext::Normal, "x"), Some(Action::PopView));
        assert_eq!(resolve(&c, KeyContext::Normal, "t"), None);

        let toasts = warning_toasts(&warnings);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].level, ToastLevel::Warning);
        assert_eq!(toasts[0].text, "config: 8 key bindings ignored — see log");
    }

    #[test]
    fn unknown_keys_get_suggestions() {
        let (c, warnings) = load(&[(
            "config.json5",
            r#"{ hint: false, colour: "red", sniff: { sample_byte: 4096, confirm: false } }"#,
        )]);
        assert_eq!(
            warnings,
            [
                ConfigWarning::UnknownKey {
                    key: "colour".into(),
                    suggestion: None
                },
                ConfigWarning::UnknownKey {
                    key: "hint".into(),
                    suggestion: Some("hints".into())
                },
                ConfigWarning::UnknownKey {
                    key: "sniff.sample_byte".into(),
                    suggestion: Some("sniff.sample_bytes".into())
                },
            ]
        );
        assert_eq!(
            warnings[1].to_string(),
            r#"config: unknown key "hint" (did you mean "hints"?)"#
        );
        // Known keys next to unknown ones still apply.
        assert!(!c.app.sniff.confirm);
        assert!(c.app.hints);
    }

    #[test]
    fn invalid_values_fall_back_to_the_default() {
        let (c, warnings) = load(&[(
            "config.json5",
            r#"{ theme: "light", threads: "many", memory: "lots", max_column_width: 2,
                 sniff: { sample_bytes: 100000000 }, tmp_dir: "/definitely/not/here",
                 hints: "maybe", freeze: 3 }"#,
        )]);
        let keys: Vec<&str> = warnings
            .iter()
            .map(|w| match w {
                ConfigWarning::InvalidValue { key, .. } => key.as_str(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                "theme",
                "threads",
                "memory",
                "tmp_dir",
                "hints",
                "max_column_width",
                "sniff.sample_bytes"
            ]
        );
        assert!(warnings[0].to_string().contains("unknown theme \"light\""));
        assert!(warnings[5].to_string().contains("out of range (4–1000)"));
        let expected = AppSettings {
            freeze: 3,
            ..AppSettings::default()
        };
        assert_eq!(c.app, expected);
        // One toast per value warning.
        assert_eq!(warning_toasts(&warnings).len(), 7);
    }

    #[test]
    fn a_malformed_file_is_ignored_with_an_error() {
        let (c, warnings) = load(&[("config.json5", "{ threads: 4,\n  hints: [true,\n}")]);
        assert_eq!(c.app, AppSettings::default());
        assert_eq!(c.keybindings, Config::embedded().keybindings);
        let [ConfigWarning::Parse { file, message }] = warnings.as_slice() else {
            panic!("{warnings:?}");
        };
        assert!(file.ends_with("config.json5"), "{file:?}");
        assert!(!message.is_empty());
        let toasts = warning_toasts(&warnings);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].level, ToastLevel::Error);
        assert!(
            toasts[0].text.starts_with("config error: ")
                && toasts[0].text.contains("config.json5: "),
            "{}",
            toasts[0].text
        );
        assert!(!toasts[0].text.contains('\n'));
    }

    #[test]
    fn a_malformed_toml_file_reports_line_and_column() {
        let (c, warnings) = load(&[
            ("config.toml", "threads = 4\nhints = = true\n"),
            // Another, valid file still applies.
            ("config.json5", "{ freeze: 2 }"),
        ]);
        assert_eq!(c.app.freeze, 2);
        assert_eq!(c.app.threads, 0);
        let [ConfigWarning::Parse { file, message }] = warnings.as_slice() else {
            panic!("{warnings:?}");
        };
        assert!(file.ends_with("config.toml"));
        assert!(message.contains("line 2"), "{message}");
        let toast = &warning_toasts(&warnings)[0];
        assert!(toast.text.contains("line 2"), "{}", toast.text);
    }

    #[test]
    fn edit_distance_and_suggestions() {
        assert_eq!(edit_distance("hint", "hints"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(closest("thraeds", KNOWN_KEYS), Some("threads"));
        assert_eq!(closest("xyz", KNOWN_KEYS), None);
    }

    #[test]
    fn every_spec_key_is_bound() {
        use KeyContext::*;
        let c = Config::embedded();
        let cases: &[(KeyContext, &str, Action)] = &[
            (Normal, "h", Action::MoveLeft),
            (Normal, "<left>", Action::MoveLeft),
            (Normal, "j", Action::MoveDown),
            (Normal, "<down>", Action::MoveDown),
            (Normal, "k", Action::MoveUp),
            (Normal, "<up>", Action::MoveUp),
            (Normal, "l", Action::MoveRight),
            (Normal, "<right>", Action::MoveRight),
            (Normal, "<ctrl-d>", Action::HalfPageDown),
            (Normal, "<ctrl-u>", Action::HalfPageUp),
            (Normal, "<pgdn>", Action::PageDown),
            (Normal, "<pgup>", Action::PageUp),
            (Normal, "<home>", Action::FirstRow),
            (Normal, "G", Action::LastRow),
            (Normal, "0", Action::FirstCol),
            (Normal, "$", Action::LastCol),
            (Normal, "w", Action::NextCol),
            (Normal, "b", Action::PrevCol),
            (Normal, "g", Action::Goto),
            (Normal, "1", Action::Tab1),
            (Normal, "2", Action::Tab2),
            (Normal, "3", Action::Tab3),
            (Normal, "4", Action::Tab4),
            (Normal, "5", Action::Tab5),
            (Normal, "6", Action::Tab6),
            (Normal, "7", Action::Tab7),
            (Normal, "8", Action::Tab8),
            (Normal, "9", Action::Tab9),
            (Normal, "<ctrl-o>", Action::OpenFile),
            (Normal, "<ctrl-w>", Action::CloseTab),
            (Normal, "/", Action::Search),
            (Normal, "n", Action::SearchNext),
            (Normal, "N", Action::SearchPrev),
            (Normal, "f", Action::Filter),
            (Normal, "F", Action::RefineFilter),
            (Normal, "x", Action::PopView),
            (Normal, "s", Action::SortAsc),
            (Normal, "S", Action::SortDesc),
            (Normal, "c", Action::ColumnChooser),
            (Normal, "<lt>", Action::ShrinkCol),
            (Normal, "<gt>", Action::GrowCol),
            (Normal, "=", Action::AutofitCol),
            (Normal, "i", Action::ToggleInspector),
            (Normal, "<tab>", Action::FocusNext),
            (Normal, "<enter>", Action::JumpToSource),
            (Normal, "y", Action::CopyCell),
            (Normal, "Y", Action::CopyRow),
            (Normal, "e", Action::Export),
            (Normal, "J", Action::ToggleJobs),
            (Normal, ":", Action::CommandPalette),
            (Normal, "?", Action::Help),
            (Normal, "q", Action::Quit),
            (Normal, "<ctrl-c>", Action::Quit),
            (Normal, "<ctrl-q>", Action::Quit),
            (Normal, "R", Action::Reload),
            (Normal, "<ctrl-z>", Action::Suspend),
            (Normal, "<esc>", Action::Dismiss),
            (Filter, "<enter>", Action::Submit),
            (Filter, "<esc>", Action::Cancel),
            (Filter, "<up>", Action::HistoryPrev),
            (Filter, "<down>", Action::HistoryNext),
            (Filter, "<tab>", Action::Complete),
            (Filter, "<ctrl-s>", Action::SaveView),
            (Search, "<enter>", Action::Submit),
            (Search, "<esc>", Action::Cancel),
            (Search, "<up>", Action::HistoryPrev),
            (Search, "<down>", Action::HistoryNext),
            (Search, "<tab>", Action::Complete),
            (Command, "<enter>", Action::Submit),
            (Command, "<esc>", Action::Cancel),
            (Command, "<up>", Action::SelectPrev),
            (Command, "<down>", Action::SelectNext),
            (Command, "<tab>", Action::Complete),
            (Prompt, "<enter>", Action::Submit),
            (Prompt, "<esc>", Action::Cancel),
            (Prompt, "<tab>", Action::Complete),
            (JobsDrawer, "j", Action::SelectNext),
            (JobsDrawer, "<down>", Action::SelectNext),
            (JobsDrawer, "k", Action::SelectPrev),
            (JobsDrawer, "<up>", Action::SelectPrev),
            (JobsDrawer, "p", Action::PauseJob),
            (JobsDrawer, "K", Action::KillJob),
            (JobsDrawer, "d", Action::DismissJob),
            (JobsDrawer, "<tab>", Action::FocusNext),
            (JobsDrawer, "<esc>", Action::Cancel),
            (Inspector, "j", Action::SelectNext),
            (Inspector, "k", Action::SelectPrev),
            (Inspector, "<enter>", Action::OpenValue),
            (Inspector, "<tab>", Action::FocusNext),
            (ColumnChooser, "j", Action::SelectNext),
            (ColumnChooser, "k", Action::SelectPrev),
            (Help, "?", Action::Help),
            (Help, "<esc>", Action::Cancel),
        ];
        for (context, key, action) in cases {
            assert_eq!(
                resolve(&c, *context, key).as_ref(),
                Some(action),
                "{context:?} {key}"
            );
        }
        // Text-input contexts leave letters to the input.
        assert_eq!(resolve(&c, Filter, "j"), None);
        assert_eq!(resolve(&c, Prompt, "q"), None);
    }
}
