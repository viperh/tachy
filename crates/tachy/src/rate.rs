//! Throughput and ETA over a sliding window of progress samples (spec §12.3,
//! M1-07).
//!
//! The UI pushes one `(Instant, bytes)` sample per tick. Throughput is
//! measured between the oldest and the newest sample of the last
//! [`WINDOW`], so the numbers don't jitter from tick to tick. Nothing is
//! shown until the samples span [`MIN_SPAN`].

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Length of the sliding window.
pub const WINDOW: Duration = Duration::from_secs(2);
/// Rate and ETA are unknown until the samples span this long.
pub const MIN_SPAN: Duration = Duration::from_secs(1);

/// Progress samples of one running operation. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct RateWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl RateWindow {
    /// Records that `bytes` were done at `now`. Samples older than the
    /// window are dropped, except the newest of them, so the window always
    /// spans about [`WINDOW`].
    pub fn push(&mut self, now: Instant, bytes: u64) {
        if let Some(&(last, _)) = self.samples.back()
            && now < last
        {
            return;
        }
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2 && now.saturating_duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
    }

    /// Bytes per second over the window, or `None` until the samples span
    /// [`MIN_SPAN`].
    pub fn rate(&self) -> Option<f64> {
        let (&(t0, b0), &(t1, b1)) = (self.samples.front()?, self.samples.back()?);
        let span = t1.saturating_duration_since(t0);
        if span < MIN_SPAN {
            return None;
        }
        Some(b1.saturating_sub(b0) as f64 / span.as_secs_f64())
    }

    /// Time to do `remaining` more bytes at the current rate. `None` while
    /// the rate is unknown or zero.
    pub fn eta(&self, remaining: u64) -> Option<Duration> {
        let rate = self.rate()?;
        (rate > 0.0).then(|| Duration::from_secs_f64(remaining as f64 / rate))
    }

    /// Forgets every sample (the operation restarted).
    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_until_one_second_of_samples() {
        let t = Instant::now();
        let mut w = RateWindow::default();
        assert_eq!(w.rate(), None);
        w.push(t, 0);
        w.push(t + Duration::from_millis(500), 50);
        assert_eq!(w.rate(), None);
        assert_eq!(w.eta(100), None);
        w.push(t + Duration::from_secs(1), 100);
        assert_eq!(w.rate(), Some(100.0));
        assert_eq!(w.eta(1_000), Some(Duration::from_secs(10)));
    }

    #[test]
    fn rate_uses_only_the_last_two_seconds() {
        let t = Instant::now();
        let mut w = RateWindow::default();
        // 1,000 B/s for 10 s, then 100 B/s.
        let mut bytes = 0;
        for i in 0..=100 {
            w.push(t + Duration::from_millis(100 * i), bytes);
            bytes += 100;
        }
        assert!((w.rate().unwrap() - 1_000.0).abs() < 1e-6);
        for i in 101..=200 {
            bytes += 10;
            w.push(t + Duration::from_millis(100 * i), bytes);
        }
        let rate = w.rate().unwrap();
        assert!((rate - 100.0).abs() < 1e-6, "{rate}");
        // About 2 s worth of samples are kept.
        assert!(w.samples.len() <= 22, "{}", w.samples.len());
    }

    #[test]
    fn stalled_progress_has_no_eta() {
        let t = Instant::now();
        let mut w = RateWindow::default();
        w.push(t, 10);
        w.push(t + Duration::from_secs(2), 10);
        assert_eq!(w.rate(), Some(0.0));
        assert_eq!(w.eta(5), None);
        // Out-of-order samples are ignored.
        w.push(t, 0);
        assert_eq!(w.rate(), Some(0.0));
        w.clear();
        assert_eq!(w.rate(), None);
    }
}
