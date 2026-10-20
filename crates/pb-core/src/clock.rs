//! Injectable clocks so timeouts, generators and sequences are testable
//! without sleeping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A source of monotonic time in nanoseconds.
pub trait Clock: Send + Sync {
    /// Monotonic nanoseconds since an arbitrary epoch (the clock's creation).
    fn now_ns(&self) -> u64;

    /// Wall-clock nanoseconds since the Unix epoch (used for file names and
    /// session headers).
    fn wall_ns(&self) -> u64;

    /// Blocks for `d`. A manual clock advances itself instead of sleeping.
    fn sleep(&self, d: Duration);
}

/// Shared clock handle.
pub type SharedClock = Arc<dyn Clock>;

/// Real time.
#[derive(Debug, Clone)]
pub struct SystemClock {
    start: Instant,
    wall_start_ns: u64,
}

impl SystemClock {
    /// Creates a clock whose monotonic epoch is now.
    pub fn new() -> Self {
        let wall_start_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self {
            start: Instant::now(),
            wall_start_ns,
        }
    }

    /// Convenience constructor for a shared handle.
    pub fn shared() -> SharedClock {
        Arc::new(Self::new())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
    fn wall_ns(&self) -> u64 {
        self.wall_start_ns + self.now_ns()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that only moves when told to. `sleep` advances it.
#[derive(Debug, Default, Clone)]
pub struct ManualClock {
    now: Arc<AtomicU64>,
}

impl ManualClock {
    /// Creates a manual clock at t = 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances time by `d`.
    pub fn advance(&self, d: Duration) {
        self.now.fetch_add(d.as_nanos() as u64, Ordering::SeqCst);
    }

    /// Sets the absolute time.
    pub fn set_ns(&self, ns: u64) {
        self.now.store(ns, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    fn wall_ns(&self) -> u64 {
        1_700_000_000_000_000_000 + self.now_ns()
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances_on_sleep() {
        let c = ManualClock::new();
        assert_eq!(c.now_ns(), 0);
        c.sleep(Duration::from_millis(5));
        assert_eq!(c.now_ns(), 5_000_000);
    }

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock::new();
        let a = c.now_ns();
        let b = c.now_ns();
        assert!(b >= a);
    }
}
