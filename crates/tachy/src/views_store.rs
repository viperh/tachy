//! Saved views (M6-04, D2): named filters kept in `<config_dir>/views.json`.
//!
//! tachy owns `views.json` and rewrites it atomically; the user's config file
//! is never written. Format (version 1, plain JSON):
//!
//! ```json
//! { "version": 1,
//!   "views": [ { "name": "DE big orders", "file_glob": "orders_*.csv",
//!                "filter": "country == \"DE\" && price > 100" } ] }
//! ```
//!
//! The user config may also hold a read-only `views` array
//! ([`crate::config::ViewConfig`]). [`ViewsStore`] merges both sources: on a
//! name clash the `views.json` entry wins and the config entry is hidden;
//! config entries are labelled `(config)` in the palette.
//!
//! - A missing file is an empty store. A corrupt (or unreadable, or
//!   newer-version) file gives a [`ViewsWarning`]; the store starts empty and
//!   the bad file is renamed to `views.json.bad-<unix-ts>` before the next
//!   write, so it is never silently lost.
//! - Writes go to `views.json.tmp` in the same directory, are `sync_all`ed,
//!   then renamed over `views.json`. The directory is created if needed.
//! - `file_glob` is matched against the **file name** of the tab's source
//!   (not the full path), with `globset`: case-sensitive on Unix,
//!   case-insensitive on Windows. Stdin tabs have no name; they match with
//!   `""`, which only globs like `*` accept.
//!
//! The blocking functions do the I/O; the `*_async` wrappers run them in
//! `spawn_blocking` for use from the UI task.

use std::{
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;

use crate::{
    config::ViewConfig,
    toast::{Toast, ToastLevel},
};

/// The file name inside the config dir.
pub const VIEWS_FILE: &str = "views.json";
/// The only format version this build reads and writes.
pub const FORMAT_VERSION: u64 = 1;
/// Longest allowed view name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Where a view comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewSource {
    /// `views.json`: can be overwritten and deleted.
    Store,
    /// The user config's `views` array: read-only.
    Config,
}

/// A saved view, as the palette shows and applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedView {
    pub name: String,
    /// The glob as written (`*` for a config entry without `file_glob`).
    pub file_glob: String,
    /// The filter expression text, applied as if typed after `f`.
    pub filter: String,
    pub source: ViewSource,
}

impl SavedView {
    /// `view: <name>`, or `view: <name> (config)` for a config entry.
    #[cfg(test)]
    pub fn palette_label(&self) -> String {
        match self.source {
            ViewSource::Store => format!("view: {}", self.name),
            ViewSource::Config => format!("view: {} (config)", self.name),
        }
    }
}

/// A problem found while loading `views.json`. Shown as a warning toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewsWarning {
    pub file: PathBuf,
    pub message: String,
}

impl ViewsWarning {
    pub fn toast(&self) -> Toast {
        Toast::new(ToastLevel::Warning, self.to_string())
    }
}

impl fmt::Display for ViewsWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Toasts are one line: keep only the parser's first line.
        let first = self.message.lines().next().unwrap_or_default().trim_end();
        write!(
            f,
            "saved views ignored: {}: {first}; it will be kept as {}.bad-<time> on the next save",
            self.file.display(),
            VIEWS_FILE
        )
    }
}

/// Why a name typed at `save as ›` is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    ControlChar,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => write!(f, "the name is empty"),
            NameError::TooLong => write!(f, "the name is longer than {MAX_NAME_CHARS} characters"),
            NameError::ControlChar => write!(f, "the name contains a control character"),
        }
    }
}

/// Checks a view name: non-empty after trimming, at most
/// [`MAX_NAME_CHARS`] characters, no control characters. Returns the trimmed
/// name, which is what gets stored.
pub fn validate_name(name: &str) -> Result<&str, NameError> {
    let name = name.trim();
    if name.is_empty() {
        Err(NameError::Empty)
    } else if name.chars().count() > MAX_NAME_CHARS {
        Err(NameError::TooLong)
    } else if name.chars().any(char::is_control) {
        Err(NameError::ControlChar)
    } else {
        Ok(name)
    }
}

/// Compiles `glob` the way saved views match file names (case-insensitive on
/// Windows only). The error text is `globset`'s, for the inline error at
/// `glob ›`.
pub fn compile_glob(glob: &str) -> Result<GlobMatcher, String> {
    GlobBuilder::new(glob)
        .case_insensitive(cfg!(windows))
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| e.to_string())
}

/// Checks a glob typed at `glob ›`.
pub fn validate_glob(glob: &str) -> Result<(), String> {
    if glob.is_empty() {
        return Err("the glob is empty".to_string());
    }
    compile_glob(glob).map(|_| ())
}

/// The `glob ›` pre-fill: the tab's file name, with glob meta-characters
/// escaped so it matches exactly that name; `*` for stdin tabs (`None`).
pub fn default_glob(file_name: Option<&str>) -> String {
    match file_name {
        None => "*".to_string(),
        Some(name) => {
            let escaped = globset::escape(name);
            // On Unix `\` escapes in globset; elsewhere it is a literal.
            if cfg!(windows) {
                escaped
            } else {
                escaped.replace('\\', "\\\\")
            }
        }
    }
}

/// The file name a tab matches globs against: the last path component, or
/// `""` when there is none (stdin).
pub fn match_name(path: Option<&Path>) -> String {
    path.and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What a name means for the save flow: decides the `overwrite view "X"?
/// (y/n)` prompt and the "shadows the config entry" note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameStatus {
    /// Not used: just save.
    Free,
    /// Already in `views.json`: ask before overwriting.
    Stored,
    /// Only in the user config: saving shadows it (show a dim note).
    ConfigOnly,
}

/// What a successful save did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveReport {
    pub status: NameStatus,
    /// Where a corrupt `views.json` was moved to, if this write did that.
    pub backup: Option<PathBuf>,
}

/// A failed save or delete. The store is unchanged.
#[derive(Debug)]
pub enum ViewsError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// `delete view` on a name that only exists in the user config.
    ConfigOnly(String),
    /// `delete view` on an unknown name.
    NotFound(String),
    InvalidName(NameError),
    InvalidGlob(String),
}

impl fmt::Display for ViewsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewsError::Io { path, source } => {
                write!(f, "can't write {}: {source}", path.display())
            }
            ViewsError::ConfigOnly(name) => write!(
                f,
                "view \"{name}\" is defined in the config file; edit the config to remove it"
            ),
            ViewsError::NotFound(name) => write!(f, "no saved view \"{name}\""),
            ViewsError::InvalidName(e) => write!(f, "invalid view name: {e}"),
            ViewsError::InvalidGlob(e) => write!(f, "invalid glob: {e}"),
        }
    }
}

impl std::error::Error for ViewsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ViewsError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ViewsError + '_ {
    move |source| ViewsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// One entry with its compiled matcher (`None` = invalid glob, never matches).
#[derive(Clone, Debug)]
struct Entry {
    view: SavedView,
    matcher: Option<GlobMatcher>,
}

impl Entry {
    fn new(view: SavedView) -> Self {
        let matcher = match compile_glob(&view.file_glob) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(
                    "saved view \"{}\": invalid file_glob {:?}: {e}; it never matches",
                    view.name,
                    view.file_glob
                );
                None
            }
        };
        Self { view, matcher }
    }

    fn matches(&self, file_name: &str) -> bool {
        self.matcher.as_ref().is_some_and(|m| m.is_match(file_name))
    }
}

/// `views.json` on disk.
#[derive(Deserialize)]
struct FileV1 {
    version: u64,
    #[serde(default)]
    views: Vec<FileView>,
}

#[derive(Deserialize)]
struct FileView {
    name: String,
    file_glob: String,
    filter: String,
}

/// The saved views of `views.json` plus the read-only config entries.
///
/// Owned by the UI task. Mutations are all-or-nothing: the in-memory list
/// changes only after the file was written.
#[derive(Clone, Debug)]
pub struct ViewsStore {
    /// `<config_dir>/views.json`.
    path: PathBuf,
    /// `views.json` entries, in file order, unique names.
    stored: Vec<Entry>,
    /// Config entries, in config order, unique names.
    config: Vec<Entry>,
    /// The file on disk could not be used: move it aside before writing.
    needs_backup: bool,
}

impl ViewsStore {
    /// An empty store writing to `<config_dir>/views.json`, without touching
    /// the disk.
    pub fn empty(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join(VIEWS_FILE),
            stored: Vec::new(),
            config: Vec::new(),
            needs_backup: false,
        }
    }

    /// Loads `<config_dir>/views.json` (blocking) and merges `config_views`
    /// (the user config's `views`, e.g. `config.app.views`).
    pub fn load(config_dir: &Path, config_views: &[ViewConfig]) -> (Self, Option<ViewsWarning>) {
        let mut store = Self::empty(config_dir);
        store.set_config_views(config_views);
        let warning = match fs::read_to_string(&store.path) {
            Ok(text) => match parse(&text) {
                Ok(views) => {
                    store.stored = dedup(views.into_iter().map(Entry::new).collect());
                    None
                }
                Err(message) => Some(message),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => Some(e.to_string()),
        };
        let warning = warning.map(|message| {
            store.needs_backup = true;
            let w = ViewsWarning {
                file: store.path.clone(),
                message,
            };
            tracing::warn!("{w}");
            w
        });
        (store, warning)
    }

    /// Replaces the read-only config entries (e.g. after a config reload).
    pub fn set_config_views(&mut self, config_views: &[ViewConfig]) {
        self.config = dedup(
            config_views
                .iter()
                .map(|v| {
                    Entry::new(SavedView {
                        name: v.name.clone(),
                        file_glob: v.file_glob.clone().unwrap_or_else(|| "*".to_string()),
                        filter: v.filter.clone(),
                        source: ViewSource::Config,
                    })
                })
                .collect(),
        );
    }

    /// The `views.json` path.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every view of both sources (shadowed config entries left out), sorted
    /// by name.
    #[cfg(test)]
    pub fn all(&self) -> Vec<SavedView> {
        self.merged(|_| true)
    }

    /// The views whose `file_glob` matches `file_name` (see [`match_name`]),
    /// sorted by name. Config entries shadowed by a `views.json` entry of the
    /// same name are left out, even if only the config glob matches.
    pub fn matching_views(&self, file_name: &str) -> Vec<SavedView> {
        self.merged(|e| e.matches(file_name))
    }

    fn merged(&self, keep: impl Fn(&Entry) -> bool) -> Vec<SavedView> {
        let mut out: Vec<SavedView> = self
            .stored
            .iter()
            .chain(
                self.config
                    .iter()
                    .filter(|c| self.stored_index(&c.view.name).is_none()),
            )
            .filter(|e| keep(e))
            .map(|e| e.view.clone())
            .collect();
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// The names `delete view` accepts (`views.json` only), in file order.
    pub fn stored_names(&self) -> Vec<String> {
        self.stored.iter().map(|e| e.view.name.clone()).collect()
    }

    /// Whether `name` is free, in `views.json`, or only in the config.
    pub fn name_status(&self, name: &str) -> NameStatus {
        if self.stored_index(name).is_some() {
            NameStatus::Stored
        } else if self.config.iter().any(|e| e.view.name == name) {
            NameStatus::ConfigOnly
        } else {
            NameStatus::Free
        }
    }

    fn stored_index(&self, name: &str) -> Option<usize> {
        self.stored.iter().position(|e| e.view.name == name)
    }

    /// Adds a view to `views.json`, or overwrites the entry of the same name
    /// (the caller asks `overwrite view "X"? (y/n)` first, see
    /// [`Self::name_status`]). Validates the name and glob, then writes the
    /// file (blocking).
    pub fn upsert(
        &mut self,
        name: &str,
        file_glob: &str,
        filter: &str,
    ) -> Result<SaveReport, ViewsError> {
        let name = validate_name(name).map_err(ViewsError::InvalidName)?;
        validate_glob(file_glob).map_err(ViewsError::InvalidGlob)?;
        let status = self.name_status(name);
        let entry = Entry::new(SavedView {
            name: name.to_string(),
            file_glob: file_glob.to_string(),
            filter: filter.to_string(),
            source: ViewSource::Store,
        });
        let mut next = self.stored.clone();
        match self.stored_index(name) {
            Some(i) => next[i] = entry,
            None => next.push(entry),
        }
        let backup = self.write(&next)?;
        self.stored = next;
        Ok(SaveReport { status, backup })
    }

    /// Removes a `views.json` entry and writes the file (blocking).
    /// Config-only names give [`ViewsError::ConfigOnly`].
    pub fn delete(&mut self, name: &str) -> Result<SaveReport, ViewsError> {
        let Some(i) = self.stored_index(name) else {
            return Err(match self.name_status(name) {
                NameStatus::ConfigOnly => ViewsError::ConfigOnly(name.to_string()),
                _ => ViewsError::NotFound(name.to_string()),
            });
        };
        let mut next = self.stored.clone();
        next.remove(i);
        let backup = self.write(&next)?;
        self.stored = next;
        Ok(SaveReport {
            status: NameStatus::Stored,
            backup,
        })
    }

    /// Writes `entries` atomically, first moving a corrupt file aside.
    fn write(&mut self, entries: &[Entry]) -> Result<Option<PathBuf>, ViewsError> {
        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let mut backup = None;
        if self.needs_backup {
            if self.path.exists() {
                let to = backup_path(&self.path);
                fs::rename(&self.path, &to).map_err(io_err(&self.path))?;
                tracing::info!("moved corrupt {} to {}", self.path.display(), to.display());
                backup = Some(to);
            }
            self.needs_backup = false;
        }
        let tmp = dir.join(format!("{VIEWS_FILE}.tmp"));
        let text = render(entries.iter().map(|e| &e.view));
        let result = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.path)?;
            // Make the rename itself durable (best effort, Unix only).
            #[cfg(unix)]
            if let Ok(d) = fs::File::open(&dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        result.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            io_err(&self.path)(e)
        })?;
        Ok(backup)
    }
}

/// `views.json.bad-<unix-ts>`, with `-N` appended if that exists already.
fn backup_path(path: &Path) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = format!("{}.bad-{ts}", path.display());
    let mut candidate = PathBuf::from(&base);
    let mut n = 1;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

/// Parses `views.json`; the error is the message for the warning.
fn parse(text: &str) -> Result<Vec<SavedView>, String> {
    let file: FileV1 = json5::from_str(text).map_err(|e| e.to_string())?;
    if file.version != FORMAT_VERSION {
        return Err(format!(
            "unsupported version {} (expected {FORMAT_VERSION})",
            file.version
        ));
    }
    Ok(file
        .views
        .into_iter()
        .map(|v| SavedView {
            name: v.name,
            file_glob: v.file_glob,
            filter: v.filter,
            source: ViewSource::Store,
        })
        .collect())
}

/// Keeps the last entry of each name, at the position of the first.
fn dedup(entries: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(entries.len());
    for e in entries {
        match out.iter().position(|o| o.view.name == e.view.name) {
            Some(i) => out[i] = e,
            None => out.push(e),
        }
    }
    out
}

/// Renders the version-1 file as plain, pretty JSON.
fn render<'a>(views: impl Iterator<Item = &'a SavedView>) -> String {
    let mut s = format!("{{\n  \"version\": {FORMAT_VERSION},\n  \"views\": [");
    let mut first = true;
    for v in views {
        s.push_str(if first { "\n" } else { ",\n" });
        first = false;
        s.push_str("    { \"name\": ");
        push_json_str(&mut s, &v.name);
        s.push_str(", \"file_glob\": ");
        push_json_str(&mut s, &v.file_glob);
        s.push_str(", \"filter\": ");
        push_json_str(&mut s, &v.filter);
        s.push_str(" }");
    }
    s.push_str(if first { "]\n}\n" } else { "\n  ]\n}\n" });
    s
}

fn push_json_str(s: &mut String, v: &str) {
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            // U+2028/9 are valid JSON but not JS/JSON5 string characters.
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                s.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

fn join_error(path: &Path, e: tokio::task::JoinError) -> ViewsError {
    ViewsError::Io {
        path: path.to_path_buf(),
        source: io::Error::other(e),
    }
}

/// [`ViewsStore::load`] in `spawn_blocking`.
#[cfg(test)]
pub async fn load_async(
    config_dir: PathBuf,
    config_views: Vec<ViewConfig>,
) -> (ViewsStore, Option<ViewsWarning>) {
    let dir = config_dir.clone();
    match tokio::task::spawn_blocking(move || ViewsStore::load(&dir, &config_views)).await {
        Ok(r) => r,
        Err(e) => {
            let store = ViewsStore::empty(&config_dir);
            let w = ViewsWarning {
                file: store.path.clone(),
                message: e.to_string(),
            };
            (store, Some(w))
        }
    }
}

/// [`ViewsStore::upsert`] in `spawn_blocking`. Takes the store by value and
/// gives it back (updated on success, unchanged on error) for the UI task to
/// put back in place.
pub async fn upsert_async(
    mut store: ViewsStore,
    name: String,
    file_glob: String,
    filter: String,
) -> (ViewsStore, Result<SaveReport, ViewsError>) {
    let fallback = store.clone();
    tokio::task::spawn_blocking(move || {
        let r = store.upsert(&name, &file_glob, &filter);
        (store, r)
    })
    .await
    .unwrap_or_else(|e| {
        let err = join_error(&fallback.path, e);
        (fallback, Err(err))
    })
}

/// [`ViewsStore::delete`] in `spawn_blocking`; see [`upsert_async`].
pub async fn delete_async(
    mut store: ViewsStore,
    name: String,
) -> (ViewsStore, Result<SaveReport, ViewsError>) {
    let fallback = store.clone();
    tokio::task::spawn_blocking(move || {
        let r = store.delete(&name);
        (store, r)
    })
    .await
    .unwrap_or_else(|e| {
        let err = join_error(&fallback.path, e);
        (fallback, Err(err))
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn cfg_view(name: &str, glob: Option<&str>, filter: &str) -> ViewConfig {
        ViewConfig {
            name: name.into(),
            file_glob: glob.map(Into::into),
            filter: filter.into(),
        }
    }

    fn names(views: &[SavedView]) -> Vec<String> {
        views.iter().map(SavedView::palette_label).collect()
    }

    fn bad_files(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("views.json.bad-")
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn missing_file_is_empty_without_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, warning) = ViewsStore::load(tmp.path(), &[]);
        assert!(warning.is_none());
        assert!(store.all().is_empty());
        assert_eq!(store.path(), tmp.path().join("views.json"));
    }

    #[test]
    fn loads_spec_example() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(VIEWS_FILE),
            r#"{ "version": 1,
  "views": [ { "name": "DE big orders", "file_glob": "orders_*.csv", "filter": "country == \"DE\" && price > 100" } ] }"#,
        )
        .unwrap();
        let (store, warning) = ViewsStore::load(tmp.path(), &[]);
        assert!(warning.is_none());
        assert_eq!(
            store.all(),
            vec![SavedView {
                name: "DE big orders".into(),
                file_glob: "orders_*.csv".into(),
                filter: r#"country == "DE" && price > 100"#.into(),
                source: ViewSource::Store,
            }]
        );
    }

    #[test]
    fn save_creates_dir_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b/tachy");
        let (mut store, _) = ViewsStore::load(&dir, &[]);
        let filter = "name == \"a\\\\b\" && note == \"x\ny\u{1}\u{2028}\"";
        let report = store.upsert("  DE  ", "orders_*.csv", filter).unwrap();
        assert_eq!(report.status, NameStatus::Free);
        assert_eq!(report.backup, None);
        assert!(dir.join(VIEWS_FILE).is_file());
        assert!(!dir.join("views.json.tmp").exists());
        // Plain JSON a JSON5 reader accepts, and the values survive.
        let text = fs::read_to_string(dir.join(VIEWS_FILE)).unwrap();
        assert!(text.starts_with("{\n  \"version\": 1,"), "{text}");
        let (again, warning) = ViewsStore::load(&dir, &[]);
        assert!(warning.is_none());
        assert_eq!(again.all(), store.all());
        assert_eq!(again.all()[0].name, "DE");
        assert_eq!(again.all()[0].filter, filter);
    }

    #[test]
    fn empty_store_renders_valid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _) = ViewsStore::load(tmp.path(), &[]);
        store.upsert("a", "*", "x > 1").unwrap();
        store.delete("a").unwrap();
        let text = fs::read_to_string(store.path()).unwrap();
        assert_eq!(text, "{\n  \"version\": 1,\n  \"views\": []\n}\n");
        let (again, warning) = ViewsStore::load(tmp.path(), &[]);
        assert!(warning.is_none());
        assert!(again.all().is_empty());
    }

    #[test]
    fn save_replaces_existing_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _) = ViewsStore::load(tmp.path(), &[]);
        store.upsert("a", "*", "x > 1").unwrap();
        // A stale tmp file from a crash is simply overwritten.
        fs::write(tmp.path().join("views.json.tmp"), "garbage").unwrap();
        store.upsert("b", "*.csv", "y > 2").unwrap();
        assert!(!tmp.path().join("views.json.tmp").exists());
        let (again, _) = ViewsStore::load(tmp.path(), &[]);
        assert_eq!(again.stored_names(), vec!["a", "b"]);
    }

    #[test]
    fn overwrite_keeps_position_and_replaces_values() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _) = ViewsStore::load(tmp.path(), &[]);
        store.upsert("a", "*", "x > 1").unwrap();
        store.upsert("b", "*", "y > 1").unwrap();
        assert_eq!(store.name_status("a"), NameStatus::Stored);
        let report = store.upsert("a", "*.tsv", "x > 9").unwrap();
        assert_eq!(report.status, NameStatus::Stored);
        let (again, _) = ViewsStore::load(tmp.path(), &[]);
        assert_eq!(again.stored_names(), vec!["a", "b"]);
        let a = &again.all()[0];
        assert_eq!(
            (a.file_glob.as_str(), a.filter.as_str()),
            ("*.tsv", "x > 9")
        );
    }

    #[test]
    fn invalid_name_or_glob_is_rejected_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _) = ViewsStore::load(tmp.path(), &[]);
        assert!(matches!(
            store.upsert("   ", "*", "x"),
            Err(ViewsError::InvalidName(NameError::Empty))
        ));
        assert!(matches!(
            store.upsert("a", "orders_[.csv", "x"),
            Err(ViewsError::InvalidGlob(_))
        ));
        assert!(!store.path().exists());
        assert!(store.all().is_empty());
    }

    #[test]
    fn corrupt_file_warns_and_is_backed_up_on_next_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(VIEWS_FILE);
        fs::write(&path, "{ not json").unwrap();
        let (mut store, warning) = ViewsStore::load(tmp.path(), &[]);
        let warning = warning.expect("a warning");
        assert_eq!(warning.file, path);
        assert!(warning.to_string().contains("views.json.bad-"), "{warning}");
        assert!(store.all().is_empty());
        // Loading alone doesn't touch the file.
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
        assert!(bad_files(tmp.path()).is_empty());

        let report = store.upsert("a", "*", "x > 1").unwrap();
        let bad = bad_files(tmp.path());
        assert_eq!(bad.len(), 1);
        assert_eq!(report.backup.as_deref(), Some(bad[0].as_path()));
        assert_eq!(fs::read_to_string(&bad[0]).unwrap(), "{ not json");
        let (again, warning) = ViewsStore::load(tmp.path(), &[]);
        assert!(warning.is_none());
        assert_eq!(again.stored_names(), vec!["a"]);

        // Only the first write backs up.
        store.upsert("b", "*", "x > 2").unwrap();
        assert_eq!(bad_files(tmp.path()).len(), 1);
    }

    #[test]
    fn unknown_version_is_treated_as_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(VIEWS_FILE),
            r#"{ "version": 2, "views": [] }"#,
        )
        .unwrap();
        let (_, warning) = ViewsStore::load(tmp.path(), &[]);
        assert!(warning.unwrap().message.contains("unsupported version 2"));
    }

    #[test]
    fn backup_name_does_not_clobber_existing_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(VIEWS_FILE);
        let first = backup_path(&path);
        fs::write(&first, "old").unwrap();
        let second = backup_path(&path);
        assert_ne!(first, second);
        assert!(
            second
                .to_string_lossy()
                .starts_with(&*first.to_string_lossy())
        );
    }

    #[test]
    fn delete_removes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = [cfg_view("cfg", None, "z")];
        let (mut store, _) = ViewsStore::load(tmp.path(), &cfg);
        store.upsert("a", "*", "x").unwrap();
        store.upsert("b", "*", "y").unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.stored_names(), vec!["b"]);
        let (again, _) = ViewsStore::load(tmp.path(), &cfg);
        assert_eq!(again.stored_names(), vec!["b"]);
        assert!(matches!(store.delete("a"), Err(ViewsError::NotFound(n)) if n == "a"));
        let err = store.delete("cfg").unwrap_err();
        assert!(matches!(&err, ViewsError::ConfigOnly(n) if n == "cfg"));
        assert!(err.to_string().contains("edit the config"));
    }

    #[test]
    fn config_views_merge_and_are_shadowed() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = [
            cfg_view("DE", Some("orders_*.csv"), "country == \"DE\""),
            cfg_view("all", None, "x > 0"),
        ];
        let (mut store, _) = ViewsStore::load(tmp.path(), &cfg);
        assert_eq!(
            names(&store.all()),
            vec!["view: all (config)", "view: DE (config)"]
        );
        assert_eq!(store.all()[0].file_glob, "*");
        assert_eq!(store.name_status("DE"), NameStatus::ConfigOnly);

        let report = store.upsert("DE", "*.csv", "country == \"FR\"").unwrap();
        assert_eq!(report.status, NameStatus::ConfigOnly);
        assert_eq!(names(&store.all()), vec!["view: all (config)", "view: DE"]);
        // The store entry wins even where only the config glob would match.
        assert_eq!(
            names(&store.matching_views("orders_1.tsv")),
            vec!["view: all (config)"]
        );
        let de = store.matching_views("orders_1.csv");
        assert_eq!(de[1].filter, "country == \"FR\"");

        // Deleting the store entry brings the config one back.
        store.delete("DE").unwrap();
        assert_eq!(
            names(&store.all()),
            vec!["view: all (config)", "view: DE (config)"]
        );
    }

    #[test]
    fn duplicate_names_keep_the_last() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(VIEWS_FILE),
            r#"{"version":1,"views":[
              {"name":"a","file_glob":"*","filter":"1"},
              {"name":"b","file_glob":"*","filter":"2"},
              {"name":"a","file_glob":"*","filter":"3"}]}"#,
        )
        .unwrap();
        let (store, _) = ViewsStore::load(tmp.path(), &[]);
        assert_eq!(store.stored_names(), vec!["a", "b"]);
        assert_eq!(store.all()[0].filter, "3");
    }

    #[test]
    fn invalid_glob_in_file_never_matches() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(VIEWS_FILE),
            r#"{"version":1,"views":[{"name":"a","file_glob":"[","filter":"1"}]}"#,
        )
        .unwrap();
        let (store, warning) = ViewsStore::load(tmp.path(), &[]);
        assert!(warning.is_none());
        assert_eq!(store.stored_names(), vec!["a"]);
        assert!(store.matching_views("[").is_empty());
    }

    fn matches(glob: &str, name: &str) -> bool {
        compile_glob(glob).unwrap().is_match(name)
    }

    #[test]
    fn glob_star() {
        assert!(matches("orders_*.csv", "orders_2026-03.csv"));
        assert!(matches("orders_*.csv", "orders_.csv"));
        assert!(!matches("orders_*.csv", "orders_2026-03.tsv"));
        assert!(!matches("orders_*.csv", "old_orders_1.csv"));
        assert!(matches("*", "anything.csv"));
        // Stdin tabs match with "" and only `*`-like globs accept it.
        assert!(matches("*", ""));
        assert!(!matches("*.csv", ""));
    }

    #[test]
    fn glob_question_mark() {
        assert!(matches("log_?.csv", "log_1.csv"));
        assert!(!matches("log_?.csv", "log_12.csv"));
        assert!(!matches("log_?.csv", "log_.csv"));
    }

    #[test]
    fn glob_character_classes() {
        assert!(matches("data_[0-9].csv", "data_7.csv"));
        assert!(!matches("data_[0-9].csv", "data_x.csv"));
        assert!(matches("data_[!0-9].csv", "data_x.csv"));
        assert!(matches("*.{csv,tsv}", "a.tsv"));
        assert!(!matches("*.{csv,tsv}", "a.psv"));
    }

    #[test]
    fn glob_exact_name_and_case() {
        assert!(matches("orders.csv", "orders.csv"));
        assert!(!matches("orders.csv", "orders.csv.bak"));
        assert_eq!(matches("orders.csv", "ORDERS.CSV"), cfg!(windows));
    }

    #[test]
    fn default_glob_matches_exactly_that_name() {
        assert_eq!(default_glob(None), "*");
        assert_eq!(
            default_glob(Some("orders_2026-03.csv")),
            "orders_2026-03.csv"
        );
        for name in ["a[1].csv", "what?*.csv", "{x}.csv", "back\\slash.csv"] {
            let glob = default_glob(Some(name));
            validate_glob(&glob).unwrap();
            assert!(matches(&glob, name), "{glob} vs {name}");
            assert!(!matches(&glob, "other.csv"), "{glob}");
        }
    }

    #[test]
    fn match_name_uses_file_name_only() {
        assert_eq!(
            match_name(Some(Path::new("/data/x/orders_1.csv"))),
            "orders_1.csv"
        );
        assert_eq!(match_name(None), "");
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _) = ViewsStore::load(tmp.path(), &[]);
        store.upsert("a", "orders_*.csv", "x").unwrap();
        let name = match_name(Some(Path::new("/data/orders_dir/orders_1.csv")));
        assert_eq!(store.matching_views(&name).len(), 1);
        assert!(store.matching_views("/data/orders_1.csv").is_empty());
    }

    #[test]
    fn name_validation() {
        assert_eq!(validate_name(" DE "), Ok("DE"));
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name("a\tb"), Err(NameError::ControlChar));
        assert_eq!(validate_name(&"é".repeat(64)), Ok(&*"é".repeat(64)));
        assert_eq!(validate_name(&"x".repeat(65)), Err(NameError::TooLong));
    }

    #[test]
    fn warning_toast_is_one_line_warning() {
        let w = ViewsWarning {
            file: PathBuf::from("/c/views.json"),
            message: "line 1\nline 2".into(),
        };
        let toast = w.toast();
        assert_eq!(toast.level, ToastLevel::Warning);
        assert!(!toast.text.contains('\n'));
    }

    #[tokio::test]
    async fn async_wrappers() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = vec![cfg_view("c", None, "1")];
        let (store, warning) = load_async(tmp.path().to_path_buf(), cfg).await;
        assert!(warning.is_none());
        let (store, r) = upsert_async(store, "a".into(), "*".into(), "x".into()).await;
        r.unwrap();
        let (store, r) = upsert_async(store, "".into(), "*".into(), "x".into()).await;
        assert!(r.is_err());
        assert_eq!(store.stored_names(), vec!["a"]);
        let (store, r) = delete_async(store, "c".into()).await;
        assert!(matches!(r, Err(ViewsError::ConfigOnly(_))));
        let (store, r) = delete_async(store, "a".into()).await;
        r.unwrap();
        assert!(store.stored_names().is_empty());
    }
}
