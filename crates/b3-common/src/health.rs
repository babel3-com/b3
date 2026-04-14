//! Subsystem health registry — shared between CLI daemon and server.
//!
//! Each subsystem (pusher, puller, tunnel, websocket, etc.) registers a handle
//! at startup. The handle is used to report success/failure from the subsystem's
//! main loop. The registry aggregates all reports into a single health snapshot.
//!
//! The `/health` endpoint reads the registry and returns 200 (all healthy) or
//! 503 (any subsystem unhealthy) with a detailed JSON report.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::Serialize;

/// Health snapshot for a single subsystem.
#[derive(Debug, Clone, Serialize)]
pub struct SubsystemHealth {
    pub name: &'static str,
    pub healthy: bool,
    pub last_success: Option<u64>,  // seconds ago (serialization-friendly)
    pub last_failure: Option<u64>,  // seconds ago
    pub consecutive_failures: u32,
    pub detail: String,
}

/// Internal state for a subsystem (not serialized directly).
#[derive(Debug)]
struct SubsystemState {
    name: &'static str,
    healthy: bool,
    last_success: Option<Instant>,
    last_failure: Option<Instant>,
    consecutive_failures: u32,
    detail: String,
}

impl SubsystemState {
    fn snapshot(&self) -> SubsystemHealth {
        let now = Instant::now();
        SubsystemHealth {
            name: self.name,
            healthy: self.healthy,
            last_success: self.last_success.map(|t| now.duration_since(t).as_secs()),
            last_failure: self.last_failure.map(|t| now.duration_since(t).as_secs()),
            consecutive_failures: self.consecutive_failures,
            detail: self.detail.clone(),
        }
    }
}

/// Central registry that holds all subsystem health states.
#[derive(Clone)]
pub struct HealthRegistry {
    subsystems: Arc<RwLock<Vec<SubsystemState>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            subsystems: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Register a new subsystem. Returns a handle the subsystem uses to report health.
    pub fn register(&self, name: &'static str) -> SubsystemHandle {
        let mut subs = self.subsystems.write().expect("health registry poisoned");
        let index = subs.len();
        subs.push(SubsystemState {
            name,
            healthy: true, // optimistic: healthy until proven otherwise
            last_success: None,
            last_failure: None,
            consecutive_failures: 0,
            detail: "registered, no activity yet".to_string(),
        });
        SubsystemHandle {
            registry: self.subsystems.clone(),
            index,
        }
    }

    /// Snapshot all subsystem health states.
    pub fn report(&self) -> Vec<SubsystemHealth> {
        let subs = self.subsystems.read().expect("health registry poisoned");
        subs.iter().map(|s| s.snapshot()).collect()
    }

    /// True if all registered subsystems are healthy.
    /// An empty registry is vacuously healthy.
    pub fn is_healthy(&self) -> bool {
        let subs = self.subsystems.read().expect("health registry poisoned");
        subs.iter().all(|s| s.healthy)
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for a single subsystem to report its health.
/// Subsystems call `record_success()` and `record_failure()` from their main loops.
#[derive(Clone)]
pub struct SubsystemHandle {
    registry: Arc<RwLock<Vec<SubsystemState>>>,
    index: usize,
}

impl SubsystemHandle {
    /// Record a successful operation. Resets consecutive failure count.
    pub fn record_success(&self) {
        let mut subs = self.registry.write().expect("health registry poisoned");
        if let Some(state) = subs.get_mut(self.index) {
            state.healthy = true;
            state.last_success = Some(Instant::now());
            state.consecutive_failures = 0;
            state.detail = "ok".to_string();
        }
    }

    /// Record a failed operation. Increments consecutive failure count.
    /// Marks unhealthy after 3 consecutive failures.
    pub fn record_failure(&self, detail: &str) {
        let mut subs = self.registry.write().expect("health registry poisoned");
        if let Some(state) = subs.get_mut(self.index) {
            state.last_failure = Some(Instant::now());
            state.consecutive_failures += 1;
            state.detail = detail.to_string();
            // Unhealthy after 3 consecutive failures — allow transient blips
            if state.consecutive_failures >= 3 {
                state.healthy = false;
            }
        }
    }

    /// Set a descriptive detail string without changing health status.
    pub fn set_detail(&self, detail: &str) {
        let mut subs = self.registry.write().expect("health registry poisoned");
        if let Some(state) = subs.get_mut(self.index) {
            state.detail = detail.to_string();
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_registration_and_report() {
        let registry = HealthRegistry::new();
        let handle = registry.register("test-subsystem");

        let report = registry.report();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].name, "test-subsystem");
        assert!(report[0].healthy);
        assert!(registry.is_healthy());

        handle.record_success();
        let report = registry.report();
        assert_eq!(report[0].detail, "ok");
        assert!(report[0].last_success.is_some());
    }

    #[test]
    fn failure_threshold() {
        let registry = HealthRegistry::new();
        let handle = registry.register("flaky");

        // 1 failure: still healthy
        handle.record_failure("timeout");
        assert!(registry.is_healthy());

        // 2 failures: still healthy
        handle.record_failure("timeout");
        assert!(registry.is_healthy());

        // 3 failures: now unhealthy
        handle.record_failure("timeout");
        assert!(!registry.is_healthy());

        let report = registry.report();
        assert_eq!(report[0].consecutive_failures, 3);
        assert!(!report[0].healthy);
    }

    #[test]
    fn recovery_after_failure() {
        let registry = HealthRegistry::new();
        let handle = registry.register("recoverable");

        // Fail 5 times
        for _ in 0..5 {
            handle.record_failure("down");
        }
        assert!(!registry.is_healthy());
        assert_eq!(registry.report()[0].consecutive_failures, 5);

        // Single success recovers
        handle.record_success();
        assert!(registry.is_healthy());
        assert_eq!(registry.report()[0].consecutive_failures, 0);
    }

    #[test]
    fn concurrent_failure_reporting() {
        let registry = HealthRegistry::new();
        let handle_a = registry.register("subsystem-a");
        let handle_b = registry.register("subsystem-b");

        // Both report failures concurrently
        let ha = handle_a.clone();
        let hb = handle_b.clone();

        let threads: Vec<_> = (0..100).map(|i| {
            let ha = ha.clone();
            let hb = hb.clone();
            std::thread::spawn(move || {
                if i % 2 == 0 {
                    ha.record_failure("fail-a");
                } else {
                    hb.record_failure("fail-b");
                }
            })
        }).collect();

        for t in threads {
            t.join().unwrap();
        }

        // Both should have recorded failures, no data races
        let report = registry.report();
        assert_eq!(report.len(), 2);
        assert!(report[0].consecutive_failures > 0);
        assert!(report[1].consecutive_failures > 0);
    }

    #[test]
    fn all_subsystems_fail_simultaneously() {
        let registry = HealthRegistry::new();
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let name: &'static str = Box::leak(format!("sub-{i}").into_boxed_str());
                registry.register(name)
            })
            .collect();

        // All report 3+ failures
        for h in &handles {
            for _ in 0..3 {
                h.record_failure("catastrophe");
            }
        }

        assert!(!registry.is_healthy());
        let report = registry.report();
        assert_eq!(report.len(), 5);
        for sub in &report {
            assert!(!sub.healthy);
            assert_eq!(sub.consecutive_failures, 3);
        }
    }

    #[test]
    fn empty_registry_is_healthy() {
        let registry = HealthRegistry::new();
        assert!(registry.is_healthy());
        assert!(registry.report().is_empty());
    }

    #[test]
    fn set_detail_does_not_change_health() {
        let registry = HealthRegistry::new();
        let handle = registry.register("detail-test");

        handle.set_detail("warming up");
        assert!(registry.is_healthy());
        assert_eq!(registry.report()[0].detail, "warming up");

        // Fail to unhealthy
        for _ in 0..3 {
            handle.record_failure("err");
        }
        assert!(!registry.is_healthy());

        // set_detail doesn't flip back to healthy
        handle.set_detail("still broken but with context");
        assert!(!registry.is_healthy());
        assert_eq!(registry.report()[0].detail, "still broken but with context");
    }
}
