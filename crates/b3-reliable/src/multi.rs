//! MultiTransport — multi-sink routing over a single ReliableChannel.
//!
//! Manages multiple transport sinks (WS, RTC, etc.) with priority-based routing.
//! One ReliableChannel provides continuous sequence numbers across transport switches.
//! Used identically by both daemon and GPU sidecar.

use std::sync::{Arc, Mutex};

use crate::channel::{ReliableChannel, ReceivedMessage};
use crate::frame::Priority;
use crate::Config;

/// A named transport sink — wraps any send function.
struct TransportSink {
    name: String,
    priority: u8, // lower = preferred (0 = RTC, 1 = WS)
    send_fn: Box<dyn Fn(&[u8]) + Send + Sync>,
}

/// Multi-transport reliability channel.
///
/// One `ReliableChannel` inside, multiple transport sinks outside.
/// Frames are routed to the highest-priority (lowest number) active sink.
/// Adding/removing sinks is instant — the next `send()` uses the new best sink.
///
/// Both daemon and GPU sidecar use the same struct:
/// - Daemon: `add_transport("ws", 1, ws_send)` + `add_transport("rtc", 0, rtc_send)`
/// - GPU relay: same calls, same API, different send closures
pub struct MultiTransport {
    sinks: Arc<Mutex<Vec<TransportSink>>>,
    reliable: ReliableChannel,
}

impl MultiTransport {
    /// Create a new multi-transport channel.
    pub fn new(config: Config) -> Self {
        let sinks: Arc<Mutex<Vec<TransportSink>>> = Arc::new(Mutex::new(Vec::new()));
        let sinks_for_callback = sinks.clone();

        let reliable = ReliableChannel::new(config, move |data: &[u8]| {
            let guard = sinks_for_callback.lock().unwrap();
            // Route to the best (lowest priority number) sink.
            // Each MultiTransport has exactly two sinks: WS + RTC for ONE browser session.
            // Multiple browsers = multiple MultiTransport instances.
            if let Some(sink) = guard.first() {
                (sink.send_fn)(data);
            }
            // If no sinks, frame is silently dropped — it stays in send buffer for replay
        });

        Self { sinks, reliable }
    }

    /// Set session ID for log attribution on the underlying ReliableChannel.
    pub fn set_session_id(&mut self, id: u64) {
        self.reliable.set_session_id(id);
    }

    /// Register a transport sink. Can be called at any time (hot-add).
    /// Lower priority number = preferred. Typical: RTC=0, WS=1.
    /// If a sink with this name already exists, it is replaced.
    /// Flushes unacked frames through the new best transport so frames
    /// previously sent on a lower-priority path get retransmitted.
    pub fn add_transport(
        &mut self,
        name: &str,
        priority: u8,
        send_fn: impl Fn(&[u8]) + Send + Sync + 'static,
    ) {
        {
            let mut guard = self.sinks.lock().unwrap();
            // Remove existing with same name
            guard.retain(|s| s.name != name);
            guard.push(TransportSink {
                name: name.to_string(),
                priority,
                send_fn: Box::new(send_fn),
            });
            // Sort by priority (lowest first = preferred)
            guard.sort_by_key(|s| s.priority);
        }
        // Flush unacked frames through the (possibly new best) transport.
        // When RTC (priority 0) is added while WS (priority 1) was active,
        // in-flight WS frames may be lost in transit. Re-sending them via
        // the new transport ensures delivery.
        self.reliable.flush_unacked();
    }

    /// Remove a transport sink by name (disconnect/close).
    /// Immediately flushes unacked frames through the remaining sinks
    /// so frames queued for the dead transport are recovered.
    pub fn remove_transport(&mut self, name: &str) {
        {
            let mut guard = self.sinks.lock().unwrap();
            guard.retain(|s| s.name != name);
        }
        // Flush unacked frames through the new best transport.
        // Without this, frames sent to the dead sink sit in the buffer
        // until the remote side sends enough dup ACKs for fast retransmit.
        self.reliable.flush_unacked();
    }

    /// Send payload — routed through ReliableChannel to best active transport.
    pub fn send(&mut self, payload: &[u8], priority: Priority) {
        self.reliable.send(payload, priority);
    }

    /// Feed incoming data from ANY transport (ACK/Resume/Data).
    pub fn receive(&mut self, data: &[u8]) -> Vec<ReceivedMessage> {
        self.reliable.receive(data)
    }

    /// Periodic tick — sends ACKs via best transport.
    pub fn tick(&mut self) {
        self.reliable.tick();
    }

    /// Number of registered transport sinks.
    pub fn sink_count(&self) -> usize {
        self.sinks.lock().unwrap().len()
    }

    /// Names of registered transport sinks (e.g. ["ws"], ["rtc"], ["ws", "rtc"]).
    pub fn sink_names(&self) -> Vec<String> {
        self.sinks.lock().unwrap().iter().map(|s| s.name.clone()).collect()
    }

    /// Returns true if a transport sink with the given name is registered.
    pub fn has_transport(&self, name: &str) -> bool {
        self.sinks.lock().unwrap().iter().any(|s| s.name == name)
    }

    /// Send a keepalive ACK to ALL registered transports (not just the best).
    /// Call this every few seconds to prevent watchdogs on idle transports
    /// from killing connections that serve as fallback.
    pub fn keepalive_all(&mut self) {
        let ack = crate::frame::encode_ack(self.reliable.last_contiguous_received(), 0);
        let guard = self.sinks.lock().unwrap();
        for sink in guard.iter() {
            (sink.send_fn)(&ack);
        }
    }

    /// Send a resume frame via best transport (call after reconnect).
    pub fn send_resume(&mut self) {
        self.reliable.send_resume();
    }

    /// Reset the inner ReliableChannel to initial state. Call when the remote
    /// side is a fresh connection (browser hard refresh) with no prior seq state.
    pub fn reset(&mut self) {
        self.reliable.reset();
    }

    /// Which transport is currently active (highest priority with a sink)?
    pub fn active_transport(&self) -> Option<String> {
        let guard = self.sinks.lock().unwrap();
        guard.first().map(|s| s.name.clone())
    }

    /// Status of all registered transports, sorted by priority.
    pub fn transport_names(&self) -> Vec<(String, u8)> {
        let guard = self.sinks.lock().unwrap();
        guard.iter().map(|s| (s.name.clone(), s.priority)).collect()
    }

    /// Number of registered transport sinks.
    pub fn transport_count(&self) -> usize {
        self.sinks.lock().unwrap().len()
    }

    /// Number of unacked messages in send buffer.
    pub fn unacked_count(&self) -> usize {
        self.reliable.unacked_count()
    }

    /// Number of out-of-order messages buffered for reordering.
    pub fn reorder_count(&self) -> usize {
        self.reliable.reorder_count()
    }

    /// Last contiguous sequence received.
    pub fn last_contiguous_received(&self) -> u32 {
        self.reliable.last_contiguous_received()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use std::sync::{Arc, Mutex};

    /// Helper: captures bytes sent through a transport sink.
    fn capture_sink() -> (impl Fn(&[u8]) + Send + Sync + 'static, Arc<Mutex<Vec<Vec<u8>>>>) {
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let send_fn = move |data: &[u8]| {
            captured_clone.lock().unwrap().push(data.to_vec());
        };
        (send_fn, captured)
    }

    #[test]
    fn test_single_transport_send_receive() {
        let (ws_send, ws_captured) = capture_sink();
        let mut multi = MultiTransport::new(Config::default());
        multi.add_transport("ws", 1, ws_send);

        multi.send(b"hello", Priority::Critical);

        let wire = ws_captured.lock().unwrap();
        assert_eq!(wire.len(), 1);

        // Decode on receiver side
        let mut receiver = MultiTransport::new(Config::default());
        let (_, _) = capture_sink();
        // Receiver doesn't need a transport to receive — just feed data
        let msgs = receiver.receive(&wire[0]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload, b"hello");
        assert_eq!(msgs[0].seq, 1);
    }

    #[test]
    fn test_priority_routing() {
        let (ws_send, ws_captured) = capture_sink();
        let (rtc_send, rtc_captured) = capture_sink();

        let mut multi = MultiTransport::new(Config::default());
        multi.add_transport("ws", 1, ws_send);
        multi.add_transport("rtc", 0, rtc_send); // RTC preferred

        multi.send(b"goes to rtc", Priority::Critical);

        assert_eq!(rtc_captured.lock().unwrap().len(), 1, "RTC should get the message");
        assert_eq!(ws_captured.lock().unwrap().len(), 0, "WS should not get the message");
    }

    #[test]
    fn test_failover_on_remove() {
        let (ws_send, ws_captured) = capture_sink();
        let (rtc_send, rtc_captured) = capture_sink();

        let mut multi = MultiTransport::new(Config::default());
        multi.add_transport("ws", 1, ws_send);
        multi.add_transport("rtc", 0, rtc_send);

        // Send via RTC (preferred)
        multi.send(b"via rtc", Priority::Critical);
        assert_eq!(rtc_captured.lock().unwrap().len(), 1);

        // RTC drops — flush_unacked retransmits "via rtc" through WS
        multi.remove_transport("rtc");
        assert_eq!(ws_captured.lock().unwrap().len(), 1, "WS should get the flushed frame after RTC removal");

        // Next send also goes to WS
        multi.send(b"via ws", Priority::Critical);
        assert_eq!(ws_captured.lock().unwrap().len(), 2, "WS should get both flushed + new frame");
    }

    #[test]
    fn test_continuous_seq_across_transport_switch() {
        let (ws_send, ws_captured) = capture_sink();
        let (rtc_send, rtc_captured) = capture_sink();

        let mut multi = MultiTransport::new(Config::default());
        multi.add_transport("rtc", 0, rtc_send);

        // Send seq 1, 2 via RTC
        multi.send(b"one", Priority::Critical);
        multi.send(b"two", Priority::Critical);

        // Switch to WS — remove_transport flushes (no sinks), add_transport flushes via WS
        multi.remove_transport("rtc");
        multi.add_transport("ws", 1, ws_send);

        // Send seq 3 via WS
        multi.send(b"three", Priority::Critical);

        // Verify sequence continuity on receiver
        let mut receiver = MultiTransport::new(Config::default());

        let rtc_wire = rtc_captured.lock().unwrap();
        let ws_wire = ws_captured.lock().unwrap();

        // RTC got seq 1, 2 originally
        let msgs = receiver.receive(&rtc_wire[0]);
        assert_eq!(msgs[0].seq, 1);
        let msgs = receiver.receive(&rtc_wire[1]);
        assert_eq!(msgs[0].seq, 2);

        // WS got flushed seq 1,2 + new seq 3
        // Receiver already saw 1,2 from RTC — flush delivers them as dupes (ignored)
        // seq 3 is the new frame
        assert_eq!(ws_wire.len(), 3, "WS gets flushed 1,2 + new 3");
        let msgs = receiver.receive(&ws_wire[2]);
        assert_eq!(msgs[0].seq, 3); // Continuous!
    }

    #[test]
    fn test_resume_replays_via_new_transport() {
        let (rtc_send, _rtc_captured) = capture_sink();
        let (ws_send, ws_captured) = capture_sink();

        let mut sender = MultiTransport::new(Config::default());
        sender.add_transport("rtc", 0, rtc_send);

        // Send 3 messages via RTC
        sender.send(b"one", Priority::Critical);
        sender.send(b"two", Priority::BestEffort);
        sender.send(b"three", Priority::Critical);

        // RTC dies, switch to WS
        sender.remove_transport("rtc");
        // add_transport flushes 2 critical frames (seq 1, 3) via WS immediately
        sender.add_transport("ws", 1, ws_send);

        let flushed = ws_captured.lock().unwrap().len();
        assert_eq!(flushed, 2, "Flush sends 2 critical frames (seq 1, 3)");

        // Remote sends Resume with last_ack=1 (only received seq 1)
        // Resume replays seq 3 (critical, unacked) — seq 2 (best-effort) skipped
        let resume = frame::encode_resume(1);
        sender.receive(&resume);

        let total = ws_captured.lock().unwrap().len();
        assert_eq!(total, 3, "Flush (2) + resume replay (1) = 3 total");
    }

    #[test]
    fn test_no_sinks_silently_buffers() {
        let mut multi = MultiTransport::new(Config::default());

        // Send with no transports — should not panic
        multi.send(b"buffered", Priority::Critical);
        assert_eq!(multi.unacked_count(), 1);

        // add_transport flushes the buffered frame immediately
        let (ws_send, ws_captured) = capture_sink();
        multi.add_transport("ws", 1, ws_send);

        let ws_wire = ws_captured.lock().unwrap();
        assert_eq!(ws_wire.len(), 1, "Buffered message flushed when transport added");
    }

    #[test]
    fn test_replace_transport_same_name() {
        let (ws_send1, ws_captured1) = capture_sink();
        let (ws_send2, ws_captured2) = capture_sink();

        let mut multi = MultiTransport::new(Config::default());
        multi.add_transport("ws", 1, ws_send1);
        multi.send(b"first", Priority::Critical);
        assert_eq!(ws_captured1.lock().unwrap().len(), 1);

        // Replace WS with new connection — flushes "first" (unacked) via new WS
        multi.add_transport("ws", 1, ws_send2);
        assert_eq!(ws_captured2.lock().unwrap().len(), 1, "New WS gets flushed frame");

        multi.send(b"second", Priority::Critical);
        assert_eq!(ws_captured2.lock().unwrap().len(), 2, "New WS gets flushed + new");
        assert_eq!(ws_captured1.lock().unwrap().len(), 1, "Old WS doesn't get more");
    }

    #[test]
    fn test_active_transport_and_status() {
        let mut multi = MultiTransport::new(Config::default());

        assert_eq!(multi.active_transport(), None);
        assert_eq!(multi.transport_count(), 0);

        let (ws_send, _) = capture_sink();
        multi.add_transport("ws", 1, ws_send);
        assert_eq!(multi.active_transport(), Some("ws".to_string()));

        let (rtc_send, _) = capture_sink();
        multi.add_transport("rtc", 0, rtc_send);
        assert_eq!(multi.active_transport(), Some("rtc".to_string())); // RTC preferred
        assert_eq!(multi.transport_count(), 2);

        multi.remove_transport("rtc");
        assert_eq!(multi.active_transport(), Some("ws".to_string())); // Falls back to WS
    }

    #[test]
    fn test_ack_via_best_transport() {
        let (ws_send, ws_captured) = capture_sink();
        let mut multi = MultiTransport::new(Config::default());
        multi.add_transport("ws", 1, ws_send);

        // Feed a data frame to trigger ACK pending
        let data = frame::encode_data(1, 0, Priority::Critical, b"test");
        let msgs = multi.receive(&data);
        assert_eq!(msgs.len(), 1);

        // Tick sends ACK via WS
        multi.tick();
        let ws_wire = ws_captured.lock().unwrap();
        assert!(ws_wire.len() >= 1, "ACK should be sent via WS");

        // Verify it's an ACK frame
        let last = ws_wire.last().unwrap();
        assert!(frame::is_reliable(last));
        let decoded = frame::decode(last).unwrap();
        match decoded {
            frame::Frame::Ack { ack_seq, .. } => assert_eq!(ack_seq, 1),
            _ => panic!("Expected ACK frame"),
        }
    }
}
