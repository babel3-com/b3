//! SessionManager — per-client session management for b3-reliable.
//!
//! Each browser window gets its own Session with isolated:
//! - MultiTransport (own seq counter, own send buffer)
//! - Delta offset tracking
//! - RTC active state
//! - Bridge task (backend_to_browser)
//!
//! Used by both the daemon and GPU relay.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{Mutex, RwLock, mpsc};

use crate::{Config, MultiTransport, bridge::OutboundMessage};

/// A client session — owns a MultiTransport, delta offset, and bridge.
/// Each browser window gets its own Session with isolated seq numbers.
pub struct Session {
    /// Unique session identifier (matches daemon client_id).
    pub id: u64,

    /// This session's own MultiTransport. Owns its own seq counter and
    /// send buffer. WS and RTC sinks are registered here.
    pub multi: Arc<Mutex<MultiTransport>>,

    /// This session's delta offset — how far into the SessionStore buffer
    /// this browser has received. Only used by daemon (GPU relay doesn't
    /// track offsets).
    pub sent_offset: Arc<Mutex<u64>>,

    /// Whether RTC is active for THIS session.
    pub rtc_active: Arc<AtomicBool>,

    /// Sender for the per-session backend channel. The event consumer
    /// sends OutboundMessages here; the bridge reads them.
    /// Wrapped in Arc<Mutex> so it can be replaced when the bridge restarts.
    pub backend_tx: Arc<Mutex<mpsc::Sender<OutboundMessage>>>,

    /// Handle for the bridge task. Aborted on session destroy.
    bridge_task: Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Optional event consumer task (daemon sets this; GPU relay doesn't).
    event_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Session {
    /// Create a new session with its own MultiTransport and bridge.
    /// The bridge reads from `backend_rx` and sends via the MultiTransport.
    pub fn new(id: u64, config: Config) -> (Self, mpsc::Receiver<OutboundMessage>) {
        let mut mt = MultiTransport::new(config);
        mt.set_session_id(id);
        let multi = Arc::new(Mutex::new(mt));
        let (backend_tx, backend_rx) = mpsc::channel::<OutboundMessage>(256);

        let session = Self {
            id,
            multi,
            sent_offset: Arc::new(Mutex::new(0)),
            rtc_active: Arc::new(AtomicBool::new(false)),
            backend_tx: Arc::new(Mutex::new(backend_tx)),
            bridge_task: Mutex::new(None),
            event_task: Mutex::new(None),
        };

        (session, backend_rx)
    }

    /// Start the bridge task (reads OutboundMessages, sends via MultiTransport).
    /// Call this after creating the session and before registering sinks.
    /// Pass `counters` for pipeline telemetry (daemon passes real counters,
    /// GPU relay passes None).
    pub async fn start_bridge(
        &self,
        backend_rx: mpsc::Receiver<OutboundMessage>,
        counters: Option<crate::BridgeCounters>,
    ) {
        let multi = self.multi.clone();
        let handle = tokio::spawn(async move {
            crate::backend_to_browser(multi, backend_rx, counters).await;
        });
        *self.bridge_task.lock().await = Some(handle);
    }

    /// Unconditionally restart the bridge with a fresh channel.
    /// Don't check is_finished() — a bridge that's "alive" but stuck on a dead
    /// channel is functionally dead. Same lesson as every other fix today:
    /// stop guessing, act decisively.
    /// Pass `counters` for pipeline telemetry (daemon passes real counters,
    /// GPU relay passes None).
    pub async fn restart_bridge(&self, counters: Option<crate::BridgeCounters>) {
        let mut guard = self.bridge_task.lock().await;
        // Abort old bridge (may be stuck on a blocked write)
        if let Some(h) = guard.take() {
            h.abort();
        }
        let (tx, rx) = mpsc::channel::<OutboundMessage>(256);
        let multi = self.multi.clone();
        let handle = tokio::spawn(async move {
            crate::backend_to_browser(multi, rx, counters).await;
        });
        *guard = Some(handle);
        *self.backend_tx.lock().await = tx;
        tracing::info!("[Session {}] Bridge restarted (unconditional on reconnect)", self.id);
    }

    /// Set the event consumer task handle (daemon only).
    /// Aborts the previous consumer to prevent orphaned tasks that
    /// multiply pipeline load on every WebSocket reconnect.
    pub async fn set_event_task(&self, handle: tokio::task::JoinHandle<()>) {
        let mut guard = self.event_task.lock().await;
        if let Some(old) = guard.take() {
            tracing::info!("[Session {}] Aborting orphaned event consumer on reconnect", self.id);
            old.abort();
        }
        *guard = Some(handle);
    }

    /// Abort all tasks and clean up.
    pub async fn destroy(&self) {
        if let Some(h) = self.bridge_task.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.event_task.lock().await.take() {
            h.abort();
        }
    }
}

/// Manages multiple client sessions. Shared state for the server.
/// Used by both daemon and GPU relay.
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<u64, Arc<Session>>>>,
    next_id: AtomicU64,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    /// Allocate the next session ID.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Create a new session with its own MultiTransport and bridge.
    /// Returns the session and the backend_rx for feeding OutboundMessages.
    pub async fn create(&self, config: Config) -> (Arc<Session>, mpsc::Receiver<OutboundMessage>) {
        let id = self.next_id();
        let (session, backend_rx) = Session::new(id, config);
        let session = Arc::new(session);
        self.sessions.write().await.insert(id, session.clone());
        (session, backend_rx)
    }

    /// Create a session with a specific ID (e.g., daemon's client_id).
    pub async fn create_with_id(&self, id: u64, config: Config) -> (Arc<Session>, mpsc::Receiver<OutboundMessage>) {
        let (session, backend_rx) = Session::new(id, config);
        let session = Arc::new(session);
        self.sessions.write().await.insert(id, session.clone());
        (session, backend_rx)
    }

    /// Look up a session by id.
    pub async fn get(&self, id: u64) -> Option<Arc<Session>> {
        self.sessions.read().await.get(&id).cloned()
    }

    /// Get the most recent session (fallback for old browsers without client_id).
    pub async fn most_recent(&self) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        sessions.values().max_by_key(|s| s.id).cloned()
    }

    /// Snapshot of all current sessions. Used by stale session reaper.
    pub async fn all_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.read().await.values().cloned().collect()
    }

    /// Remove a session and destroy its tasks.
    pub async fn remove(&self, id: u64) {
        if let Some(session) = self.sessions.write().await.remove(&id) {
            session.destroy().await;
        }
    }

    /// Reconnect to an existing session or create a new one.
    ///
    /// This is the standard way to handle a browser WS reconnect after a tunnel
    /// drop. If a session with `client_id` exists, it's reused — the send buffer
    /// retains unacked frames which get replayed via Resume. If `client_id` is
    /// None, falls back to the most recent session. If no session exists, creates
    /// a new one.
    ///
    /// The caller must:
    /// 1. Remove the old WS transport: `session.multi.lock().await.remove_transport("ws")`
    /// 2. Register the new WS transport: `session.multi.lock().await.add_transport("ws", ...)`
    ///
    /// Returns (session, needs_bridge) — if needs_bridge is true, the caller must
    /// call `session.start_bridge(backend_rx)` with the returned receiver.
    pub async fn reconnect_or_create(
        &self,
        client_id: Option<u64>,
        config: Config,
    ) -> (Arc<Session>, Option<mpsc::Receiver<OutboundMessage>>) {
        // Try to find existing session
        let existing = if let Some(cid) = client_id {
            self.get(cid).await
        } else {
            self.most_recent().await
        };

        if let Some(session) = existing {
            // Reuse: send buffer preserved, Resume will replay unacked frames
            (session, None)
        } else {
            // New session — use provided client_id if any, otherwise auto-increment
            let (session, backend_rx) = if let Some(cid) = client_id {
                self.create_with_id(cid, config).await
            } else {
                self.create(config).await
            };
            (session, Some(backend_rx))
        }
    }

    /// Number of active sessions.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// List all session IDs.
    pub async fn ids(&self) -> Vec<u64> {
        self.sessions.read().await.keys().copied().collect()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Priority;

    #[tokio::test]
    async fn test_session_manager_create_and_get() {
        let mgr = SessionManager::new();
        assert_eq!(mgr.count().await, 0);

        let (session, _rx) = mgr.create(Config::default()).await;
        assert_eq!(session.id, 1);
        assert_eq!(mgr.count().await, 1);

        let found = mgr.get(1).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, 1);

        assert!(mgr.get(999).await.is_none());
    }

    #[tokio::test]
    async fn test_session_manager_create_with_id() {
        let mgr = SessionManager::new();
        let (session, _rx) = mgr.create_with_id(42, Config::default()).await;
        assert_eq!(session.id, 42);
        assert!(mgr.get(42).await.is_some());
    }

    #[tokio::test]
    async fn test_session_manager_remove() {
        let mgr = SessionManager::new();
        let (session, _rx) = mgr.create(Config::default()).await;
        let id = session.id;
        assert_eq!(mgr.count().await, 1);

        mgr.remove(id).await;
        assert_eq!(mgr.count().await, 0);
        assert!(mgr.get(id).await.is_none());
    }

    #[tokio::test]
    async fn test_session_manager_most_recent() {
        let mgr = SessionManager::new();
        assert!(mgr.most_recent().await.is_none());

        let (_s1, _rx1) = mgr.create(Config::default()).await;
        let (s2, _rx2) = mgr.create(Config::default()).await;

        let recent = mgr.most_recent().await.unwrap();
        assert_eq!(recent.id, s2.id);
    }

    #[tokio::test]
    async fn test_session_isolation() {
        // Two sessions should have independent seq counters
        let mgr = SessionManager::new();
        let (s1, rx1) = mgr.create(Config::default()).await;
        let (s2, rx2) = mgr.create(Config::default()).await;

        // Start bridges
        s1.start_bridge(rx1, None).await;
        s2.start_bridge(rx2, None).await;

        // Send through session 1
        s1.multi.lock().await.send(b"hello from s1", Priority::Critical);
        s1.multi.lock().await.send(b"second from s1", Priority::Critical);

        // Send through session 2
        s2.multi.lock().await.send(b"hello from s2", Priority::Critical);

        // Sessions have independent seq counters
        // s1 sent 2 messages, s2 sent 1 — they don't interfere
        // (We can't directly check next_seq on MultiTransport, but we can
        // verify the send buffers are independent via unacked_count)
        assert_eq!(s1.multi.lock().await.unacked_count(), 2);
        assert_eq!(s2.multi.lock().await.unacked_count(), 1);
    }

    #[tokio::test]
    async fn test_session_reset_isolation() {
        // Resetting one session should not affect another
        let mgr = SessionManager::new();
        let (s1, rx1) = mgr.create(Config::default()).await;
        let (s2, rx2) = mgr.create(Config::default()).await;

        s1.start_bridge(rx1, None).await;
        s2.start_bridge(rx2, None).await;

        // Send frames on both
        s1.multi.lock().await.send(b"s1 data", Priority::Critical);
        s2.multi.lock().await.send(b"s2 data", Priority::Critical);

        assert_eq!(s1.multi.lock().await.unacked_count(), 1);
        assert_eq!(s2.multi.lock().await.unacked_count(), 1);

        // Reset session 1
        s1.multi.lock().await.reset();

        // Session 1 is reset (send buffer cleared)
        assert_eq!(s1.multi.lock().await.unacked_count(), 0);
        // Session 2 is unaffected
        assert_eq!(s2.multi.lock().await.unacked_count(), 1);
    }

    #[tokio::test]
    async fn test_session_offset_isolation() {
        let mgr = SessionManager::new();
        let (s1, _rx1) = mgr.create(Config::default()).await;
        let (s2, _rx2) = mgr.create(Config::default()).await;

        // Set different offsets
        *s1.sent_offset.lock().await = 1000;
        *s2.sent_offset.lock().await = 5000;

        // They're independent
        assert_eq!(*s1.sent_offset.lock().await, 1000);
        assert_eq!(*s2.sent_offset.lock().await, 5000);

        // Changing one doesn't affect the other
        *s1.sent_offset.lock().await = 2000;
        assert_eq!(*s2.sent_offset.lock().await, 5000);
    }

    #[tokio::test]
    async fn test_session_rtc_active_isolation() {
        let mgr = SessionManager::new();
        let (s1, _rx1) = mgr.create(Config::default()).await;
        let (s2, _rx2) = mgr.create(Config::default()).await;

        assert!(!s1.rtc_active.load(Ordering::Relaxed));
        assert!(!s2.rtc_active.load(Ordering::Relaxed));

        // Enable RTC on session 1 only
        s1.rtc_active.store(true, Ordering::Relaxed);

        assert!(s1.rtc_active.load(Ordering::Relaxed));
        assert!(!s2.rtc_active.load(Ordering::Relaxed)); // unaffected
    }

    #[tokio::test]
    async fn test_session_destroy() {
        let mgr = SessionManager::new();
        let (session, rx) = mgr.create(Config::default()).await;
        session.start_bridge(rx, None).await;

        // Bridge task should be running
        assert!(session.bridge_task.lock().await.is_some());

        // Destroy
        session.destroy().await;

        // Tasks aborted
        assert!(session.bridge_task.lock().await.is_none());
        assert!(session.event_task.lock().await.is_none());
    }

    #[tokio::test]
    async fn test_session_ids() {
        let mgr = SessionManager::new();
        let (_s1, _rx1) = mgr.create(Config::default()).await;
        let (_s2, _rx2) = mgr.create(Config::default()).await;
        let (_s3, _rx3) = mgr.create(Config::default()).await;

        let mut ids = mgr.ids().await;
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_backend_tx_sends_to_bridge() {
        let mgr = SessionManager::new();
        let (session, rx) = mgr.create(Config::default()).await;
        session.start_bridge(rx, None).await;

        // Register a mock sink that captures frames
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        session.multi.lock().await.add_transport("test", 1, move |data: &[u8]| {
            captured_clone.lock().unwrap().push(data.to_vec());
        });

        // Send via backend_tx — should flow through bridge → MultiTransport → sink
        session.backend_tx.lock().await.send(OutboundMessage {
            payload: b"test payload".to_vec(),
            priority: Priority::Critical,
        }).await.unwrap();

        // Give the bridge task time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let frames = captured.lock().unwrap();
        assert!(!frames.is_empty(), "Bridge should have forwarded the frame to the sink");
    }
}
