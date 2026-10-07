//! One-line, self-dismissing messages shown above the status line (spec §16,
//! M1-09).
//!
//! Background tasks raise a toast with `msg_tx.send(Msg::Toast(..))`; UI code
//! calls [`ToastQueue::push`] directly. The queue never reads the clock: every
//! call that needs the time takes `now`, so tests inject `Instant`s.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use tachy_core::source::SourceError;

/// How long each toast stays on screen once it reaches the front (§16).
pub const TOAST_DURATION: Duration = Duration::from_secs(5);

/// Severity of a [`Toast`]; picks its colour (`Theme::toast`) and log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Error,
    Warning,
    /// `exported 1,234 rows to …`, `copied cell` (normal text colour).
    Info,
}

/// A user-visible message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub level: ToastLevel,
    /// When the toast reached the front of the queue; `None` while it waits.
    pub shown_at: Option<Instant>,
}

impl Toast {
    pub fn new(level: ToastLevel, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level,
            shown_at: None,
        }
    }

    /// An error toast for a file that could not be opened (§16).
    pub fn open_error(err: &SourceError) -> Self {
        Self::new(ToastLevel::Error, open_error_text(err))
    }

    fn same_message(&self, other: &Toast) -> bool {
        self.level == other.level && self.text == other.text
    }

    fn log(&self) {
        match self.level {
            ToastLevel::Error => tracing::error!(toast = %self.text),
            ToastLevel::Warning => tracing::warn!(toast = %self.text),
            ToastLevel::Info => tracing::info!(toast = %self.text),
        }
    }
}

/// The toast text for a [`SourceError`]. The path is shown as given on the
/// command line (the error carries the path it was opened with).
pub fn open_error_text(err: &SourceError) -> String {
    let (path, reason) = match err {
        SourceError::NotFound(path) => (path, "file not found".to_owned()),
        SourceError::PermissionDenied(path) => (path, "permission denied".to_owned()),
        SourceError::IsDirectory(path) => (path, "is a directory".to_owned()),
        SourceError::NotRegular(path) => (path, "not a regular file (use - for pipes)".to_owned()),
        SourceError::Io { path, source } => (path, source.to_string()),
    };
    format!("cannot open {}: {reason}", path.display())
}

/// Toasts waiting to be shown. The front one is displayed for
/// [`TOAST_DURATION`], counted from when it reached the front.
#[derive(Debug, Clone, Default)]
pub struct ToastQueue {
    queue: VecDeque<Toast>,
}

impl ToastQueue {
    /// Queues `toast` and logs it at its level.
    ///
    /// Pushing the same text and level as the toast on screen resets its timer
    /// instead of queueing a copy; a copy of a toast that is already waiting
    /// is dropped. Either way a repeated failing action can't flood the queue.
    pub fn push(&mut self, mut toast: Toast, now: Instant) {
        toast.log();
        if let Some(front) = self.queue.front_mut()
            && front.same_message(&toast)
        {
            front.shown_at = Some(now);
            return;
        }
        if self.queue.iter().any(|t| t.same_message(&toast)) {
            return;
        }
        toast.shown_at = self.queue.is_empty().then_some(now);
        self.queue.push_back(toast);
    }

    /// Drops the front toast once its time is up and starts the next one's.
    /// Returns whether anything changed on screen.
    pub fn tick(&mut self, now: Instant) -> bool {
        let expired = self
            .queue
            .front()
            .and_then(|t| t.shown_at)
            .is_some_and(|shown| now.saturating_duration_since(shown) >= TOAST_DURATION);
        if expired {
            self.dismiss(now);
        }
        expired
    }

    /// Removes the toast on screen (`Esc`, M1-06 `Dismiss`). Returns whether
    /// there was one.
    pub fn dismiss(&mut self, now: Instant) -> bool {
        let dismissed = self.queue.pop_front().is_some();
        if let Some(next) = self.queue.front_mut() {
            next.shown_at = Some(now);
        }
        dismissed
    }

    /// The toast on screen, if any.
    pub fn front(&self) -> Option<&Toast> {
        self.queue.front()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Toasts waiting behind the one on screen: the `(+N)` count.
    pub fn waiting(&self) -> usize {
        self.queue.len().saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    use pretty_assertions::assert_eq;

    use super::*;

    fn secs(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    fn err(text: &str) -> Toast {
        Toast::new(ToastLevel::Error, text)
    }

    fn front_text(q: &ToastQueue) -> Option<&str> {
        q.front().map(|t| t.text.as_str())
    }

    #[test]
    fn a_toast_expires_after_five_seconds() {
        let t0 = Instant::now();
        let mut q = ToastQueue::default();
        q.push(err("a"), t0);
        assert_eq!(q.front().unwrap().shown_at, Some(t0));
        assert!(!q.tick(secs(t0, 4.9)));
        assert_eq!(front_text(&q), Some("a"));
        assert!(q.tick(secs(t0, 5.0)));
        assert!(q.is_empty());
        assert!(!q.tick(secs(t0, 6.0)));
    }

    #[test]
    fn queued_toasts_each_get_their_full_time() {
        let t0 = Instant::now();
        let mut q = ToastQueue::default();
        q.push(err("a"), t0);
        q.push(err("b"), secs(t0, 1.0));
        assert_eq!(q.waiting(), 1);
        // `b` waits without a timer.
        assert_eq!(q.queue[1].shown_at, None);

        assert!(q.tick(secs(t0, 5.1)));
        assert_eq!(front_text(&q), Some("b"));
        assert_eq!(q.waiting(), 0);
        assert_eq!(q.front().unwrap().shown_at, Some(secs(t0, 5.1)));
        assert!(!q.tick(secs(t0, 10.0)));
        assert!(q.tick(secs(t0, 10.1)));
        assert!(q.is_empty());
    }

    #[test]
    fn dismiss_shows_the_next_one_with_a_fresh_timer() {
        let t0 = Instant::now();
        let mut q = ToastQueue::default();
        q.push(err("a"), t0);
        q.push(err("b"), t0);
        assert!(q.dismiss(secs(t0, 2.0)));
        assert_eq!(front_text(&q), Some("b"));
        assert!(!q.tick(secs(t0, 6.9)));
        assert!(q.tick(secs(t0, 7.0)));
        assert!(!q.dismiss(secs(t0, 8.0)));
    }

    #[test]
    fn duplicate_of_the_shown_toast_resets_its_timer() {
        let t0 = Instant::now();
        let mut q = ToastQueue::default();
        q.push(err("a"), t0);
        q.push(err("a"), secs(t0, 4.0));
        assert_eq!(q.len(), 1);
        assert!(!q.tick(secs(t0, 8.9)));
        assert!(q.tick(secs(t0, 9.0)));
    }

    #[test]
    fn same_text_with_another_level_is_not_a_duplicate() {
        let t0 = Instant::now();
        let mut q = ToastQueue::default();
        q.push(err("a"), t0);
        q.push(Toast::new(ToastLevel::Info, "a"), t0);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn duplicate_of_a_waiting_toast_is_dropped() {
        let t0 = Instant::now();
        let mut q = ToastQueue::default();
        q.push(err("a"), t0);
        q.push(err("b"), t0);
        q.push(err("b"), t0);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn open_error_texts() {
        let p = || PathBuf::from("data/x.csv");
        let cases = [
            (
                SourceError::NotFound(p()),
                "cannot open data/x.csv: file not found",
            ),
            (
                SourceError::PermissionDenied(p()),
                "cannot open data/x.csv: permission denied",
            ),
            (
                SourceError::IsDirectory(p()),
                "cannot open data/x.csv: is a directory",
            ),
            (
                SourceError::NotRegular(p()),
                "cannot open data/x.csv: not a regular file (use - for pipes)",
            ),
            (
                SourceError::Io {
                    path: p(),
                    source: io::Error::other("disk on fire"),
                },
                "cannot open data/x.csv: disk on fire",
            ),
        ];
        for (err, text) in cases {
            let toast = Toast::open_error(&err);
            assert_eq!(toast.text, text);
            assert_eq!(toast.level, ToastLevel::Error);
        }
    }
}
