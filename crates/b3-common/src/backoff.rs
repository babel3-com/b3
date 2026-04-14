//! Exponential backoff with failure tracking.
//!
//! Shared by any subsystem that retries on failure (pusher, puller, tunnel
//! registration, etc.). Encapsulates the pattern that was previously inline
//! in each subsystem's main loop.
//!
//! Usage:
//! ```ignore
//! let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
//! loop {
//!     match do_work().await {
//!         Ok(()) => backoff.reset(),
//!         Err(_) => {
//!             if backoff.should_log() { tracing::warn!("..."); }
//!             tokio::time::sleep(backoff.next_delay()).await;
//!         }
//!     }
//! }
//! ```

use std::time::Duration;

use crate::limits::FAILURE_LOG_INTERVAL;

/// Exponential backoff tracker with consecutive failure counting.
///
/// `next_delay()` doubles the delay on each call (capped at `max`).
/// `reset()` returns to the base delay and clears failure count.
/// `should_log()` returns true on the first failure and every Nth failure
/// to prevent log spam during prolonged outages.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    current: Duration,
    /// Total consecutive failures since last `reset()`.
    pub consecutive_failures: u32,
}

impl Backoff {
    /// Create a new backoff tracker.
    ///
    /// - `base`: initial delay after first failure
    /// - `max`: ceiling for exponential growth
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            current: base,
            consecutive_failures: 0,
        }
    }

    /// Record a failure and return the next delay to sleep.
    /// Doubles the delay each call, capped at `max`.
    pub fn next_delay(&mut self) -> Duration {
        self.consecutive_failures += 1;
        let delay = self.current;
        self.current = (self.current * 2).min(self.max);
        delay
    }

    /// Jump directly to max delay (e.g. for permanent/auth failures).
    pub fn next_delay_max(&mut self) -> Duration {
        self.consecutive_failures += 1;
        self.current = self.max;
        self.max
    }

    /// Peek at the current delay without incrementing the failure count.
    /// Use when you need the backoff interval for scheduling (e.g. pusher
    /// push interval) without recording a new failure.
    pub fn current_delay(&self) -> Duration {
        self.current
    }

    /// Reset to base delay and clear failure count (call on success).
    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.current = self.base;
    }

    /// True on the 1st failure and every `FAILURE_LOG_INTERVAL`th failure.
    /// Returns false when consecutive_failures is 0 (no failures yet).
    /// Use to avoid log spam during prolonged outages.
    pub fn should_log(&self) -> bool {
        self.consecutive_failures > 0
            && (self.consecutive_failures == 1
                || self.consecutive_failures % FAILURE_LOG_INTERVAL == 0)
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_growth_capped() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.consecutive_failures, 1);
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_secs(8));
        assert_eq!(b.next_delay(), Duration::from_secs(16));
        // Next would be 32, but capped at 30
        assert_eq!(b.next_delay(), Duration::from_secs(30));
        // Stays at cap
        assert_eq!(b.next_delay(), Duration::from_secs(30));
        assert_eq!(b.consecutive_failures, 7);
    }

    #[test]
    fn reset_returns_to_base() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        b.next_delay();
        b.next_delay();
        b.next_delay();
        assert_eq!(b.consecutive_failures, 3);

        b.reset();
        assert_eq!(b.consecutive_failures, 0);
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn next_delay_max_jumps_to_cap() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        assert_eq!(b.next_delay_max(), Duration::from_secs(30));
        assert_eq!(b.consecutive_failures, 1);
        // Stays at max
        assert_eq!(b.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn should_log_first_and_interval() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));

        // Before any failure — should_log is false (0 failures)
        assert!(!b.should_log());

        // First failure
        b.next_delay();
        assert!(b.should_log()); // failure #1

        // Failures 2 through 49 — should NOT log
        for _ in 2..FAILURE_LOG_INTERVAL {
            b.next_delay();
            assert!(!b.should_log());
        }

        // Failure #50 — should log
        b.next_delay();
        assert_eq!(b.consecutive_failures, FAILURE_LOG_INTERVAL);
        assert!(b.should_log());
    }

    #[test]
    fn current_delay_peeks_without_incrementing() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        // Before any failure, current_delay is base
        assert_eq!(b.current_delay(), Duration::from_secs(1));
        assert_eq!(b.consecutive_failures, 0);

        // After a failure, current_delay reflects the NEXT delay (doubled)
        b.next_delay(); // returns 1s, advances current to 2s
        assert_eq!(b.current_delay(), Duration::from_secs(2));
        assert_eq!(b.consecutive_failures, 1); // peek didn't increment

        // Multiple peeks don't change state
        assert_eq!(b.current_delay(), Duration::from_secs(2));
        assert_eq!(b.current_delay(), Duration::from_secs(2));
        assert_eq!(b.consecutive_failures, 1);
    }

    #[test]
    fn zero_base_delay() {
        let mut b = Backoff::new(Duration::ZERO, Duration::from_secs(1));
        // 0 * 2 = 0, stays at 0
        assert_eq!(b.next_delay(), Duration::ZERO);
        assert_eq!(b.next_delay(), Duration::ZERO);
    }
}
