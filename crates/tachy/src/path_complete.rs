//! Filesystem path completion for the `open ›` prompt (M1-08), reused by the
//! export path (M6-01).
//!
//! - Separators: `/` everywhere, and `\` on Windows ([`SEPARATORS`]). On
//!   Unix `\` is an ordinary file-name character, so it is not split on.
//! - A leading `~` / `~/` (`~\` on Windows) expands to `$HOME`; relative
//!   paths resolve against the process's current directory.
//! - One match completes fully (a separator appended for directories: the
//!   last one typed, else the platform's, [`default_separator`]). Several
//!   matches complete to their longest common prefix; a second `Tab` with no
//!   new input cycles through them ([`Cycle`]).
//! - Hidden entries (starting with `.`) are only offered when the typed name
//!   starts with `.`.
//!
//! [`list_candidates`] does blocking I/O: the UI runs it in `spawn_blocking`
//! with a timeout, so a slow network mount never blocks the UI.

use std::{
    io,
    path::{Path, PathBuf},
};

/// Matches listed on the prompt's second line before `+N more`.
pub const SHOWN_CANDIDATES: usize = 10;

/// The path separators the user may type on this platform.
pub const SEPARATORS: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };

/// Whether `c` separates path components on this platform.
pub fn is_separator(c: char) -> bool {
    SEPARATORS.contains(&c)
}

/// The separator appended to completed directories when the input has
/// none yet: `\` on Windows, `/` elsewhere.
pub fn default_separator() -> char {
    if cfg!(windows) { '\\' } else { '/' }
}

/// The separator completions of `input` append to directories: the last
/// separator typed, else [`default_separator`].
pub fn separator_for(input: &str) -> char {
    separator_in(input, SEPARATORS, default_separator())
}

/// The separator to append in `input`: the last one typed, else
/// `default` (`seps` are the separators that count).
fn separator_in(input: &str, seps: &[char], default: char) -> char {
    input
        .chars()
        .rev()
        .find(|c| seps.contains(c))
        .unwrap_or(default)
}

/// One directory entry that matches the typed name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub is_dir: bool,
}

impl Candidate {
    /// The name as completed: directories get a trailing `sep`.
    pub fn completed(&self, sep: char) -> String {
        if self.is_dir {
            format!("{}{sep}", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Splits the input into the directory part as typed (up to and including
/// the last separator) and the partial name after it.
pub fn split_input(input: &str) -> (&str, &str) {
    split_input_with(input, SEPARATORS)
}

/// [`split_input`] with an explicit separator set (tests use the Windows one
/// on any platform). A Windows drive (`C:`) without a separator is a
/// directory part too.
fn split_input_with<'a>(input: &'a str, seps: &[char]) -> (&'a str, &'a str) {
    match input.rfind(|c| seps.contains(&c)) {
        Some(i) => input.split_at(i + 1),
        None if seps.contains(&'\\') && is_drive(input) => input.split_at(2),
        None => ("", input),
    }
}

/// `C:…`: a drive letter and a colon.
fn is_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// The path `input` refers to: `~` expanded, relative to `cwd`.
pub fn resolve(input: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = match expand_tilde(input, home, SEPARATORS) {
        Some(p) => p,
        None => PathBuf::from(input),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// `~` or `~<sep>rest` as a path under `home`; `None` otherwise.
fn expand_tilde(input: &str, home: Option<&Path>, seps: &[char]) -> Option<PathBuf> {
    let home = home?;
    let rest = input.strip_prefix('~')?;
    if rest.is_empty() {
        return Some(home.to_path_buf());
    }
    let mut chars = rest.chars();
    let first = chars.next()?;
    seps.contains(&first).then(|| home.join(chars.as_str()))
}

/// The user's home directory, for `~`.
pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// Entries of the directory part of `input` whose name starts with the
/// partial name, sorted by name. Blocking.
pub fn list_candidates(input: &str, cwd: &Path, home: Option<&Path>) -> io::Result<Vec<Candidate>> {
    let (dir, partial) = split_input(input);
    let dir_path = if dir.is_empty() {
        cwd.to_path_buf()
    } else {
        resolve(dir, cwd, home)
    };
    let show_hidden = partial.starts_with('.');
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir_path)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(partial) || (name.starts_with('.') && !show_hidden) {
            continue;
        }
        // Follows symlinks, so a link to a directory completes with `/`.
        let is_dir = std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
        out.push(Candidate { name, is_dir });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// What one `Tab` does to the input, given the candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The new input.
    pub input: String,
    /// Set when there are several matches: the cycle for the next `Tab`.
    pub cycle: Option<Cycle>,
}

/// Completes `input` against `candidates` (from [`list_candidates`] for the
/// same input). `None` when nothing matches.
pub fn complete(input: &str, candidates: Vec<Candidate>) -> Option<Completion> {
    complete_with(input, candidates, SEPARATORS, default_separator())
}

/// [`complete`] with an explicit separator set and default separator.
fn complete_with(
    input: &str,
    candidates: Vec<Candidate>,
    seps: &[char],
    default: char,
) -> Option<Completion> {
    let (dir, _) = split_input_with(input, seps);
    let sep = separator_in(input, seps, default);
    // `~` alone is the home directory.
    if input == "~" {
        return Some(Completion {
            input: format!("~{sep}"),
            cycle: None,
        });
    }
    match candidates.len() {
        0 => None,
        1 => Some(Completion {
            input: format!("{dir}{}", candidates[0].completed(sep)),
            cycle: None,
        }),
        _ => {
            let prefix = common_prefix(candidates.iter().map(|c| c.name.as_str()));
            let input = format!("{dir}{prefix}");
            Some(Completion {
                cycle: Some(Cycle {
                    dir: dir.to_owned(),
                    sep,
                    matches: candidates,
                    next: 0,
                    last: input.clone(),
                }),
                input,
            })
        }
    }
}

/// Cycling through several matches with repeated `Tab`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    dir: String,
    /// Appended to directories.
    sep: char,
    pub matches: Vec<Candidate>,
    next: usize,
    /// The input this cycle last produced. Typing anything else ends the
    /// cycle.
    last: String,
}

impl Cycle {
    /// Whether a `Tab` on `input` continues this cycle (nothing was typed
    /// since the last completion).
    pub fn continues(&self, input: &str) -> bool {
        input == self.last
    }

    /// The next match, as the new input.
    pub fn advance(&mut self) -> String {
        let m = &self.matches[self.next % self.matches.len()];
        self.next = (self.next + 1) % self.matches.len();
        self.last = format!("{}{}", self.dir, m.completed(self.sep));
        self.last.clone()
    }
}

/// The longest common prefix of `names`, on char boundaries.
fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = names.next() else {
        return String::new();
    };
    let mut len = first.len();
    for name in names {
        len = first[..len]
            .char_indices()
            .zip(name.chars())
            .find(|((_, a), b)| a != b)
            .map_or(len.min(name.len()), |((i, _), _)| i);
        while !first.is_char_boundary(len) {
            len -= 1;
        }
    }
    first[..len].to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pretty_assertions::assert_eq;

    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("orders_2023.csv"), "").unwrap();
        fs::write(root.join("orders_2024.csv"), "").unwrap();
        fs::write(root.join("unique.tsv"), "").unwrap();
        fs::write(root.join(".hidden.csv"), "").unwrap();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data").join("inner.csv"), "").unwrap();
        dir
    }

    fn tab(input: &str, cwd: &Path, home: Option<&Path>) -> Option<Completion> {
        complete(input, list_candidates(input, cwd, home).unwrap())
    }

    #[test]
    fn unique_prefix_completes_fully() {
        let dir = tree();
        assert_eq!(tab("un", dir.path(), None).unwrap().input, "unique.tsv");
        // Directories get a slash; then their content completes.
        let sep = default_separator();
        assert_eq!(
            tab("da", dir.path(), None).unwrap().input,
            format!("data{sep}")
        );
        assert_eq!(
            tab("data/in", dir.path(), None).unwrap().input,
            "data/inner.csv"
        );
        assert_eq!(tab("zzz", dir.path(), None), None);
    }

    #[test]
    fn several_matches_complete_to_the_common_prefix_then_cycle() {
        let dir = tree();
        let c = tab("or", dir.path(), None).unwrap();
        assert_eq!(c.input, "orders_202");
        let mut cycle = c.cycle.unwrap();
        assert_eq!(cycle.matches.len(), 2);
        assert!(cycle.continues("orders_202"));
        assert_eq!(cycle.advance(), "orders_2023.csv");
        assert!(cycle.continues("orders_2023.csv"));
        assert_eq!(cycle.advance(), "orders_2024.csv");
        assert_eq!(cycle.advance(), "orders_2023.csv");
        // New input ends the cycle.
        assert!(!cycle.continues("orders_2023.cs"));
    }

    #[test]
    fn absolute_paths_and_tilde() {
        let dir = tree();
        let abs = format!("{}/un", dir.path().display());
        assert_eq!(
            tab(&abs, Path::new("/"), None).unwrap().input,
            format!("{}/unique.tsv", dir.path().display())
        );
        let home = Some(dir.path());
        assert_eq!(
            tab("~/un", Path::new("/"), home).unwrap().input,
            "~/unique.tsv"
        );
        assert_eq!(
            tab("~", Path::new("/"), home).unwrap().input,
            format!("~{}", default_separator())
        );
        assert_eq!(
            resolve("~/a.csv", Path::new("/x"), home),
            dir.path().join("a.csv")
        );
        assert_eq!(
            resolve("a.csv", Path::new("/x"), home),
            PathBuf::from("/x/a.csv")
        );
        assert_eq!(
            resolve("/a.csv", Path::new("/x"), home),
            PathBuf::from("/a.csv")
        );
    }

    #[test]
    fn hidden_entries_only_when_typed() {
        let dir = tree();
        let all = list_candidates("", dir.path(), None).unwrap();
        assert!(all.iter().all(|c| !c.name.starts_with('.')), "{all:?}");
        assert_eq!(all.len(), 4);
        assert_eq!(tab(".", dir.path(), None).unwrap().input, ".hidden.csv");
    }

    #[test]
    fn missing_directory_is_an_error() {
        let dir = tree();
        assert!(list_candidates("nope/x", dir.path(), None).is_err());
    }

    const WINDOWS: &[char] = &['/', '\\'];

    fn cands(list: &[(&str, bool)]) -> Vec<Candidate> {
        list.iter()
            .map(|(name, is_dir)| Candidate {
                name: (*name).to_owned(),
                is_dir: *is_dir,
            })
            .collect()
    }

    #[test]
    fn windows_separators_split_and_complete() {
        assert_eq!(split_input_with("data\\in", WINDOWS), ("data\\", "in"));
        assert_eq!(
            split_input_with("C:\\Users\\me/da", WINDOWS),
            ("C:\\Users\\me/", "da")
        );
        // A bare drive is a directory part.
        assert_eq!(split_input_with("C:da", WINDOWS), ("C:", "da"));
        assert_eq!(split_input_with("da", WINDOWS), ("", "da"));
        // Directories get the separator last typed, else the default.
        let c = complete_with("C:\\Us", cands(&[("Users", true)]), WINDOWS, '\\').unwrap();
        assert_eq!(c.input, "C:\\Users\\");
        let c = complete_with("C:/Us", cands(&[("Users", true)]), WINDOWS, '\\').unwrap();
        assert_eq!(c.input, "C:/Users/");
        let c = complete_with("da", cands(&[("data", true)]), WINDOWS, '\\').unwrap();
        assert_eq!(c.input, "data\\");
        assert_eq!(
            complete_with("~", vec![], WINDOWS, '\\').unwrap().input,
            "~\\"
        );
        // Cycling keeps the separator.
        let c = complete_with(
            "x\\o",
            cands(&[("one", true), ("other", true)]),
            WINDOWS,
            '\\',
        )
        .unwrap();
        assert_eq!(c.input, "x\\o");
        let mut cycle = c.cycle.unwrap();
        assert_eq!(cycle.advance(), "x\\one\\");
        assert_eq!(cycle.advance(), "x\\other\\");
    }

    #[test]
    fn tilde_expands_with_either_windows_separator() {
        let home = Path::new("/home/u");
        assert_eq!(
            expand_tilde("~\\a.csv", Some(home), WINDOWS),
            Some(home.join("a.csv"))
        );
        assert_eq!(
            expand_tilde("~/a.csv", Some(home), WINDOWS),
            Some(home.join("a.csv"))
        );
        assert_eq!(
            expand_tilde("~", Some(home), WINDOWS),
            Some(home.to_path_buf())
        );
        assert_eq!(expand_tilde("~x", Some(home), WINDOWS), None);
        assert_eq!(expand_tilde("~/a", None, WINDOWS), None);
        // On Unix `~\` is not the home directory: `\` is a name character.
        assert_eq!(expand_tilde("~\\a", Some(home), &['/']), None);
    }

    #[test]
    fn platform_separators() {
        assert!(is_separator('/'));
        assert_eq!(is_separator('\\'), cfg!(windows));
        assert_eq!(separator_for("a/b"), '/');
        assert_eq!(separator_for("ab"), default_separator());
        // On Unix a backslash stays part of the name.
        if !cfg!(windows) {
            assert_eq!(split_input("a\\b"), ("", "a\\b"));
            assert_eq!(default_separator(), '/');
        }
    }

    #[cfg(windows)]
    #[test]
    fn backslash_paths_complete_on_windows() {
        let dir = tree();
        assert_eq!(
            tab("data\\in", dir.path(), None).unwrap().input,
            "data\\inner.csv"
        );
        assert_eq!(tab("da", dir.path(), None).unwrap().input, "data\\");
        assert_eq!(
            resolve("data\\inner.csv", dir.path(), None),
            dir.path().join("data").join("inner.csv")
        );
    }

    #[test]
    fn common_prefixes() {
        assert_eq!(common_prefix(["abc", "abd"].into_iter()), "ab");
        assert_eq!(common_prefix(["abc", "ab"].into_iter()), "ab");
        assert_eq!(common_prefix(["x"].into_iter()), "x");
        assert_eq!(common_prefix(["日本", "日語"].into_iter()), "日");
        assert_eq!(common_prefix(std::iter::empty()), "");
    }
}
