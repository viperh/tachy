//! `Executor`: bounds parallel CPU work by running chunk tasks on tokio's
//! blocking pool behind a semaphore with `--threads` permits (README §A2).
//!
//! There is one `Executor` per app, built from `Settings.threads` and passed
//! to every job (indexing, filter, sort, profile, export, search). So
//! `--threads` means "max concurrent blocking work items", not the size of a
//! thread pool.

use std::{num::NonZeroUsize, sync::Arc};

use tokio::{runtime::Handle, sync::Semaphore};

/// Runs blocking closures on the tokio blocking pool, at most `threads` at a
/// time. Cheap to clone; clones share the permits.
#[derive(Debug, Clone)]
pub struct Executor {
    handle: Handle,
    permits: Arc<Semaphore>,
    threads: usize,
}

impl Executor {
    /// An executor on the current tokio runtime with `threads` permits
    /// (0 → `std::thread::available_parallelism()`).
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime. Use [`Executor::with_handle`] there.
    pub fn new(threads: usize) -> Self {
        Self::with_handle(Handle::current(), threads)
    }

    /// An executor on the runtime behind `handle`.
    pub fn with_handle(handle: Handle, threads: usize) -> Self {
        let threads = if threads == 0 {
            std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
        } else {
            threads
        };
        Executor {
            handle,
            permits: Arc::new(Semaphore::new(threads)),
            threads,
        }
    }

    /// The number of permits: how many closures may run at once.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Acquires a permit, runs `f` on the blocking pool and releases the
    /// permit when `f` returns.
    ///
    /// A panic in `f` is resumed in the caller.
    pub async fn run<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("the executor's semaphore is never closed");
        let task = self.handle.spawn_blocking(move || {
            let _permit = permit;
            f()
        });
        match task.await {
            Ok(value) => value,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => panic!("blocking task failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounds_concurrency() {
        let exec = Executor::new(2);
        assert_eq!(exec.threads(), 2);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..8 {
            let exec = exec.clone();
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                exec.run(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                    i * 2
                })
                .await
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        assert_eq!(results, (0..8).map(|i| i * 2).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn zero_means_available_parallelism() {
        let exec = Executor::new(0);
        assert!(exec.threads() >= 1);
    }

    #[tokio::test]
    async fn panics_propagate() {
        let exec = Executor::new(1);
        let r = tokio::spawn(async move { exec.run(|| panic!("boom")).await }).await;
        assert!(r.unwrap_err().is_panic());
    }
}
