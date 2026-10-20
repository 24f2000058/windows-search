//! Resource governance for background indexing (ADR-014).
//!
//! Indexing must not compete with the user. The governor provides:
//! * **One heavy scan at a time** (full enumeration or traversal): volumes
//!   queue for a scan permit, so several disks are never thrashed at once and
//!   peak memory stays that of one scan.
//! * **Yielding to the user**: while the UI reports activity (keystrokes,
//!   searches), workers pause between batches. A pause is bounded, so
//!   indexing always makes progress, however busy the user is.
//! * **Thread setup**: a platform hook run on every worker thread (on Windows,
//!   background mode: low CPU, I/O and memory priority, plus EcoQoS).
//!
//! The governor only paces; it never drops work, so correctness does not
//! depend on it.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub type ThreadSetup = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GovernorOptions {
    /// Concurrent heavy scans across all volumes (at least 1).
    pub max_scans: usize,
    /// How long after the last user activity indexing stays paused.
    pub user_grace: Duration,
    /// Upper bound of a single pause, so indexing never starves.
    pub max_pause: Duration,
}

impl Default for GovernorOptions {
    fn default() -> Self {
        GovernorOptions { max_scans: 1, user_grace: Duration::from_millis(500), max_pause: Duration::from_millis(250) }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GovernorStats {
    pub scans_running: usize,
    pub scans_queued: usize,
    /// Total time workers spent paused for the user.
    pub paused: Duration,
}

pub struct Governor {
    opts: GovernorOptions,
    epoch: Instant,
    /// Milliseconds since `epoch` until which the user counts as active.
    active_until: AtomicU64,
    paused_us: AtomicU64,
    queued: AtomicUsize,
    running: Mutex<usize>,
    freed: Condvar,
    setup: Option<ThreadSetup>,
}

/// Held while a heavy scan runs; released on drop.
pub struct ScanPermit<'a>(&'a Governor);

impl Drop for ScanPermit<'_> {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

impl Governor {
    pub fn new(opts: GovernorOptions, setup: Option<ThreadSetup>) -> Arc<Self> {
        let opts = GovernorOptions { max_scans: opts.max_scans.max(1), ..opts };
        Arc::new(Governor { opts, epoch: Instant::now(), active_until: AtomicU64::new(0), paused_us: AtomicU64::new(0), queued: AtomicUsize::new(0), running: Mutex::new(0), freed: Condvar::new(), setup })
    }

    /// No limits; used by tests and tools.
    pub fn unthrottled() -> Arc<Self> {
        Self::new(GovernorOptions { max_scans: usize::MAX, user_grace: Duration::ZERO, max_pause: Duration::ZERO }, None)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Runs the platform hook on the calling (worker) thread.
    pub fn setup_thread(&self) {
        if let Some(f) = &self.setup {
            f();
        }
    }

    /// Called by the UI on input; cheap (one atomic store).
    pub fn note_user_activity(&self) {
        self.active_until.store(self.now_ms() + self.opts.user_grace.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn user_active(&self) -> bool {
        self.now_ms() < self.active_until.load(Ordering::Relaxed)
    }

    /// Called by workers between batches: waits while the user is active, up
    /// to `max_pause`, or until `stop` is set.
    pub fn pace(&self, stop: &AtomicBool) {
        let start = Instant::now();
        while self.user_active() && start.elapsed() < self.opts.max_pause && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10).min(self.opts.max_pause));
        }
        let us = start.elapsed().as_micros() as u64;
        if us >= 1000 {
            self.paused_us.fetch_add(us, Ordering::Relaxed);
        }
    }

    /// Waits for a heavy-scan permit. Returns None if `stop` was set while
    /// waiting. `on_queued` runs once if the caller has to wait.
    pub fn acquire_scan(&self, stop: &AtomicBool, on_queued: &mut dyn FnMut(bool)) -> Option<ScanPermit<'_>> {
        let mut running = self.running.lock().unwrap();
        let mut queued = false;
        while *running >= self.opts.max_scans {
            if !queued {
                queued = true;
                self.queued.fetch_add(1, Ordering::Relaxed);
                on_queued(true);
            }
            if stop.load(Ordering::Relaxed) {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                on_queued(false);
                return None;
            }
            running = self.freed.wait_timeout(running, Duration::from_millis(100)).unwrap().0;
        }
        if queued {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            on_queued(false);
        }
        *running += 1;
        Some(ScanPermit(self))
    }

    pub fn stats(&self) -> GovernorStats {
        GovernorStats { scans_running: *self.running.lock().unwrap(), scans_queued: self.queued.load(Ordering::Relaxed), paused: Duration::from_micros(self.paused_us.load(Ordering::Relaxed)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_are_serialized_and_queued_scans_can_be_stopped() {
        let g = Governor::new(GovernorOptions::default(), None);
        let stop = AtomicBool::new(false);
        let p = g.acquire_scan(&stop, &mut |_| panic!("first scan must not queue")).unwrap();
        let g2 = g.clone();
        let started = Arc::new(AtomicBool::new(false));
        let s2 = started.clone();
        let t = std::thread::spawn(move || {
            let stop = AtomicBool::new(false);
            let mut was_queued = false;
            let _p = g2.acquire_scan(&stop, &mut |q| was_queued |= q).unwrap();
            s2.store(true, Ordering::SeqCst);
            was_queued
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(!started.load(Ordering::SeqCst), "second scan waits for the first");
        assert_eq!(g.stats().scans_queued, 1);
        drop(p);
        assert!(t.join().unwrap(), "the waiting scan reported being queued");
        assert_eq!(g.stats(), GovernorStats { scans_running: 0, scans_queued: 0, paused: Duration::ZERO });

        let _p = g.acquire_scan(&stop, &mut |_| {}).unwrap();
        let stopped = AtomicBool::new(true);
        assert!(g.acquire_scan(&stopped, &mut |_| {}).is_none(), "stop wins over waiting");
        assert_eq!(g.stats().scans_queued, 0);
    }

    #[test]
    fn pauses_while_user_is_active_but_never_starves() {
        let g = Governor::new(GovernorOptions { max_scans: 1, user_grace: Duration::from_secs(10), max_pause: Duration::from_millis(60) }, None);
        let stop = AtomicBool::new(false);
        let t = Instant::now();
        g.pace(&stop);
        assert!(t.elapsed() < Duration::from_millis(20), "no pause without activity");
        g.note_user_activity();
        let t = Instant::now();
        g.pace(&stop);
        let e = t.elapsed();
        assert!(e >= Duration::from_millis(60) && e < Duration::from_secs(2), "bounded pause: {e:?}");
        assert!(g.stats().paused >= Duration::from_millis(60));
        stop.store(true, Ordering::Relaxed);
        let t = Instant::now();
        g.pace(&stop);
        assert!(t.elapsed() < Duration::from_millis(20), "stop ends a pause");
    }

    #[test]
    fn setup_hook_runs_on_request() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let g = Governor::new(GovernorOptions::default(), Some(Arc::new(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        })));
        g.setup_thread();
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
}
