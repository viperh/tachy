//! Session history of the filter and search bars (M4-04, M4-05): `↑`/`↓`.
//!
//! Newest first, no consecutive duplicates, at most [`MAX_ENTRIES`], not
//! persisted. Filter and search keep separate histories. Stepping into the
//! history saves the unsent text as a draft, restored when stepping past the
//! newest entry.

use std::collections::VecDeque;

/// Entries kept per history.
pub const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct QueryHistory {
    /// Newest first.
    entries: VecDeque<String>,
    /// The entry shown, while browsing.
    pos: Option<usize>,
    /// The text that was in the bar before browsing.
    draft: String,
}

impl QueryHistory {
    /// Adds a submitted text and stops browsing. Blank texts and repeats of
    /// the newest entry are not added.
    pub fn push(&mut self, text: &str) {
        self.reset();
        if text.trim().is_empty() || self.entries.front().is_some_and(|e| e == text) {
            return;
        }
        self.entries.push_front(text.to_owned());
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Stops browsing (the bar was opened or closed). The draft is dropped.
    pub fn reset(&mut self) {
        self.pos = None;
        self.draft.clear();
    }

    /// `↑`: the next older entry, or `None` at the oldest (the text stays).
    /// `current` is the bar's text, saved as the draft on the first step.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        let next = self.pos.map_or(0, |p| p + 1);
        let entry = self.entries.get(next)?.clone();
        if self.pos.is_none() {
            self.draft = current.to_owned();
        }
        self.pos = Some(next);
        Some(entry)
    }

    /// `↓`: the next newer entry, then the draft; `None` when not browsing.
    pub fn next(&mut self) -> Option<String> {
        match self.pos? {
            0 => {
                self.pos = None;
                Some(std::mem::take(&mut self.draft))
            }
            p => {
                self.pos = Some(p - 1);
                self.entries.get(p - 1).cloned()
            }
        }
    }

    #[cfg(test)]
    pub fn entries(&self) -> Vec<&str> {
        self.entries.iter().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn newest_first_without_consecutive_duplicates() {
        let mut h = QueryHistory::default();
        for t in ["a", "b", "b", "a", "  "] {
            h.push(t);
        }
        assert_eq!(h.entries(), ["a", "b", "a"]);
    }

    #[test]
    fn keeps_at_most_100_entries() {
        let mut h = QueryHistory::default();
        for i in 0..150 {
            h.push(&i.to_string());
        }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        assert_eq!(h.entries()[0], "149");
        assert_eq!(h.entries()[99], "50");
    }

    #[test]
    fn browsing_keeps_the_draft() {
        let mut h = QueryHistory::default();
        h.push("old");
        h.push("new");
        assert_eq!(h.next(), None, "not browsing");
        assert_eq!(h.prev("typing").as_deref(), Some("new"));
        assert_eq!(h.prev("new").as_deref(), Some("old"));
        assert_eq!(h.prev("old"), None, "oldest reached");
        assert_eq!(h.next().as_deref(), Some("new"));
        assert_eq!(h.next().as_deref(), Some("typing"));
        assert_eq!(h.next(), None);
        // A new browse saves a new draft.
        assert_eq!(h.prev("other").as_deref(), Some("new"));
        h.push("third");
        assert_eq!(h.next(), None, "push stops browsing");
        assert_eq!(h.prev("").as_deref(), Some("third"));
    }

    #[test]
    fn empty_history() {
        let mut h = QueryHistory::default();
        assert_eq!(h.prev("x"), None);
        assert_eq!(h.next(), None);
    }
}
