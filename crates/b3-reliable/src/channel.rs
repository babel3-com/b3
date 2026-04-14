//! ReliableChannel — send buffer, ACK tracking, ordered delivery, replay.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;
use tracing::{debug, warn};

use crate::frame::{self, Frame, Priority};
use crate::Config;

/// A buffered message awaiting ACK.
struct BufferedMessage {
    seq: u32,
    priority: Priority,
    encoded: Vec<u8>, // pre-encoded frame bytes (ready to resend)
}

/// Received message ready for delivery.
#[derive(Debug)]
pub struct ReceivedMessage {
    pub payload: Vec<u8>,
    pub seq: u32,
    pub priority: Priority,
}

/// Transport-agnostic reliable channel.
///
/// Wraps any `send(bytes)` function. Adds sequence numbers, checksums,
/// ACK tracking, and replay on reconnect.
pub struct ReliableChannel {
    config: Config,
    start: Instant,
    /// Session ID for log attribution. Set via `set_session_id()` after creation.
    session_id: u64,

    // ── Sending ──
    next_seq: u32,
    send_buffer: VecDeque<BufferedMessage>,
    last_ack_received: u32,
    transport_tx: Box<dyn Fn(&[u8]) + Send + Sync>,

    // ── Receiving ──
    /// Highest contiguous seq received (all seqs ≤ this have been delivered).
    last_contiguous: u32,
    /// Out-of-order messages waiting for gaps to fill.
    reorder_buffer: BTreeMap<u32, ReceivedMessage>,
    /// Tracks whether an ACK is pending (dirty flag).
    ack_pending: bool,
    /// Chunk reassembly buffer — accumulates chunks (tag 0x01-0x03) until last arrives.
    chunk_assembly: Vec<u8>,
    /// Count of duplicate ACKs received (same ack_seq). After 3, retransmit.
    duplicate_ack_count: u32,
}

impl ReliableChannel {
    /// Create a new channel with the given transport send function.
    pub fn new(config: Config, transport_tx: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        Self {
            config,
            start: Instant::now(),
            session_id: 0,
            next_seq: 1, // seq 0 is reserved (means "nothing received yet")
            send_buffer: VecDeque::new(),
            last_ack_received: 0,
            transport_tx: Box::new(transport_tx),
            last_contiguous: 0,
            reorder_buffer: BTreeMap::new(),
            ack_pending: false,
            chunk_assembly: Vec::new(),
            duplicate_ack_count: 0,
        }
    }

    /// Set the session ID for log attribution.
    pub fn set_session_id(&mut self, id: u64) {
        self.session_id = id;
    }

    /// Current timestamp in ms since channel creation.
    fn timestamp(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Send a message with the given priority.
    /// If payload exceeds max_frame_payload, automatically chunks it.
    /// Each chunk is a separate reliable frame with a 1-byte chunk header:
    ///   0x00 = standalone (not chunked), 0x01 = first, 0x02 = middle, 0x03 = last.
    pub fn send(&mut self, payload: &[u8], priority: Priority) {
        let max = self.config.max_frame_payload;
        if payload.len() <= max {
            // Standalone — prepend 0x00 header
            let mut framed = Vec::with_capacity(1 + payload.len());
            framed.push(0x00);
            framed.extend_from_slice(payload);
            self.send_one(&framed, priority);
        } else {
            // Chunk the payload
            let chunks: Vec<&[u8]> = payload.chunks(max).collect();
            let last_idx = chunks.len() - 1;
            for (i, chunk) in chunks.iter().enumerate() {
                let tag = if i == 0 { 0x01 } else if i == last_idx { 0x03 } else { 0x02 };
                let mut framed = Vec::with_capacity(1 + chunk.len());
                framed.push(tag);
                framed.extend_from_slice(chunk);
                self.send_one(&framed, priority);
            }
        }
    }

    /// Send a single frame (internal — used by send() and chunking).
    fn send_one(&mut self, payload: &[u8], priority: Priority) {
        let seq = self.next_seq;
        self.next_seq += 1;

        let encoded = frame::encode_data(seq, self.timestamp(), priority, payload);

        self.send_buffer.push_back(BufferedMessage {
            seq,
            priority,
            encoded: encoded.clone(),
        });

        // Enforce buffer limit — drop oldest best-effort first
        while self.send_buffer.len() > self.config.max_send_buffer {
            if let Some(front) = self.send_buffer.front() {
                if front.priority == Priority::BestEffort || front.seq <= self.last_ack_received {
                    let evicted_seq = front.seq;
                    let evicted_pri = front.priority;
                    self.send_buffer.pop_front();
                    if evicted_pri == Priority::Critical {
                        warn!(session_id = self.session_id, evicted_seq, last_ack = self.last_ack_received, buffer_len = self.config.max_send_buffer,
                              "Evicted CRITICAL frame from send buffer — retransmit impossible for this seq");
                    }
                } else {
                    break;
                }
            }
        }

        (self.transport_tx)(&encoded);
    }

    /// Feed incoming bytes from transport. Returns decoded, in-order messages.
    pub fn receive(&mut self, data: &[u8]) -> Vec<ReceivedMessage> {
        let frame = match frame::decode(data) {
            Some(f) => f,
            None => return Vec::new(), // Not a reliable frame or corrupt
        };

        match frame {
            Frame::Data { seq, priority, payload, .. } => {
                self.receive_data(seq, priority, payload)
            }
            Frame::Ack { ack_seq, .. } => {
                self.receive_ack(ack_seq);
                Vec::new()
            }
            Frame::Resume { last_ack_seq } => {
                self.handle_resume(last_ack_seq);
                Vec::new()
            }
        }
    }

    fn receive_data(&mut self, seq: u32, priority: Priority, payload: Vec<u8>) -> Vec<ReceivedMessage> {
        self.ack_pending = true;

        // Duplicate check — already delivered
        if seq <= self.last_contiguous {
            return Vec::new();
        }

        // Next expected?
        if seq == self.last_contiguous + 1 {
            self.last_contiguous = seq;
            let mut delivered = self.process_chunk(payload, seq, priority);

            // Drain any buffered messages that are now contiguous
            while let Some(next) = self.reorder_buffer.remove(&(self.last_contiguous + 1)) {
                self.last_contiguous = next.seq;
                delivered.extend(self.process_chunk(next.payload, next.seq, next.priority));
            }

            return delivered;
        }

        // Out of order — buffer for later (raw payload with chunk tag intact)
        let was_new = !self.reorder_buffer.contains_key(&seq);
        self.reorder_buffer.entry(seq).or_insert(ReceivedMessage {
            payload,
            seq,
            priority,
        });
        if was_new && self.reorder_buffer.len() % 100 == 0 {
            let missing_start = self.last_contiguous + 1;
            warn!(session_id = self.session_id, seq, last_contiguous = self.last_contiguous, missing_start,
                  reorder_len = self.reorder_buffer.len(),
                  "Reorder buffer growing — waiting for seq {missing_start}");
        }

        Vec::new()
    }

    /// Process a chunk-tagged payload. Returns 0 or 1 assembled messages.
    /// Tag: 0x00=standalone, 0x01=first, 0x02=middle, 0x03=last.
    fn process_chunk(&mut self, payload: Vec<u8>, seq: u32, priority: Priority) -> Vec<ReceivedMessage> {
        if payload.is_empty() {
            return Vec::new();
        }
        let tag = payload[0];
        let data = &payload[1..];
        match tag {
            0x00 => {
                // Standalone — deliver immediately
                vec![ReceivedMessage { payload: data.to_vec(), seq, priority }]
            }
            0x01 => {
                // First chunk — start new assembly
                self.chunk_assembly.clear();
                self.chunk_assembly.extend_from_slice(data);
                Vec::new()
            }
            0x02 => {
                // Middle chunk — append
                self.chunk_assembly.extend_from_slice(data);
                Vec::new()
            }
            0x03 => {
                // Last chunk — assemble and deliver
                self.chunk_assembly.extend_from_slice(data);
                let assembled = std::mem::take(&mut self.chunk_assembly);
                vec![ReceivedMessage { payload: assembled, seq, priority }]
            }
            _ => {
                // Unknown tag — treat as standalone (backward compat with old senders)
                vec![ReceivedMessage { payload: payload, seq, priority }]
            }
        }
    }

    fn receive_ack(&mut self, ack_seq: u32) {
        let was_new = ack_seq > self.last_ack_received;
        if was_new {
            self.last_ack_received = ack_seq;
            // Remove acked messages from send buffer
            while let Some(front) = self.send_buffer.front() {
                if front.seq <= ack_seq {
                    self.send_buffer.pop_front();
                } else {
                    break;
                }
            }
            self.duplicate_ack_count = 0;
        } else if ack_seq == self.last_ack_received && !self.send_buffer.is_empty() {
            // Duplicate ACK — remote side is stuck at this seq.
            // After 3 duplicate ACKs, retransmit unacked critical frames.
            // This is TCP-style fast retransmit: 3 dup ACKs = lost frame.
            self.duplicate_ack_count += 1;
            if self.duplicate_ack_count >= 3 {
                self.duplicate_ack_count = 0;
                let mut retransmit_count = 0u32;
                let first_unacked = self.send_buffer.front().map(|m| m.seq);
                for m in &self.send_buffer {
                    if m.seq > ack_seq && m.priority == Priority::Critical {
                        (self.transport_tx)(&m.encoded);
                        retransmit_count += 1;
                    }
                }
                debug!(session_id = self.session_id, ack_seq, retransmit_count, ?first_unacked,
                       buffer_len = self.send_buffer.len(),
                       "Fast retransmit triggered by 3 dup ACKs");
            }
        }
    }

    fn handle_resume(&mut self, last_ack_seq: u32) {
        debug!(session_id = self.session_id, last_ack_seq, buffer_len = self.send_buffer.len(), next_seq = self.next_seq,
               "Resume received — replaying unacked critical frames");
        self.last_ack_received = last_ack_seq;

        // Case 1: After daemon/relay restart, next_seq resets to 1 but the browser
        // remembers its last_ack (e.g. 3217). New frames at seq=1 would be rejected
        // as duplicates. Jump next_seq past the browser's last_ack.
        // Case 2: Browser is fresh (new page load → new ReliableChannel, last_ack=0)
        // but the relay/daemon reused a session with next_seq > 1 from a previous
        // connection. The browser expects seq=1 but the relay sends seq=50+. All
        // frames pile up in the browser's reorder buffer → deadlock.
        if last_ack_seq >= self.next_seq {
            warn!(session_id = self.session_id, last_ack_seq, next_seq = self.next_seq,
                  last_contiguous = self.last_contiguous,
                  "Resume seq ahead of send seq — daemon restarted. Resetting both directions.");
            self.next_seq = last_ack_seq + 1;
            self.send_buffer.clear();
            // Also reset inbound — the browser restarted, its outbound seq will start
            // from 1 again. We must accept seq=1 frames, not reject them as duplicates.
            self.last_contiguous = 0;
            self.reorder_buffer.clear();
        } else if last_ack_seq < self.next_seq
            && self.send_buffer.front().map_or(false, |m| m.seq > last_ack_seq + 1)
        {
            // Browser is behind and the frames it needs (seq last_ack+1...) are
            // no longer in the send buffer. Can't replay → reset to browser's state.
            // map_or(false): empty buffer is NOT a gap — nothing to replay is fine.
            // A clean reconnect after reset() arrives as Resume(0) with next_seq=1
            // and empty buffer; that's a valid fresh session, not an unreplayable gap.
            // Preserve Critical frames — re-encode them at new seq numbers so they
            // survive the reset. GPU results (TTS audio, transcriptions) are Critical
            // and must not be lost on reconnect.
            let critical_payloads: Vec<(Vec<u8>, Priority)> = self.send_buffer.iter()
                .filter(|m| m.priority == Priority::Critical)
                .map(|m| {
                    // Extract the original payload from the encoded frame
                    // (skip the 19-byte header to get the payload)
                    let payload = if m.encoded.len() > crate::frame::DATA_HEADER_SIZE {
                        m.encoded[crate::frame::DATA_HEADER_SIZE..].to_vec()
                    } else {
                        m.encoded.clone()
                    };
                    (payload, m.priority)
                })
                .collect();
            let critical_count = critical_payloads.len();

            warn!(session_id = self.session_id, last_ack_seq, next_seq = self.next_seq,
                  oldest_buffered = self.send_buffer.front().map(|m| m.seq),
                  critical_preserved = critical_count,
                  "Resume from stale client — unreplayable gap. Resetting outbound channel.");
            self.next_seq = last_ack_seq + 1;
            self.last_contiguous = last_ack_seq;
            self.send_buffer.clear();
            self.reorder_buffer.clear();

            // Re-send preserved Critical frames at new seq numbers
            for (payload, priority) in critical_payloads {
                self.send_one(&payload, priority);
            }
        }

        let to_replay: Vec<Vec<u8>> = self.send_buffer
            .iter()
            .filter(|m| m.seq > last_ack_seq && m.priority == Priority::Critical)
            .map(|m| m.encoded.clone())
            .collect();

        for encoded in to_replay {
            (self.transport_tx)(&encoded);
        }

        // Clean up acked messages
        while let Some(front) = self.send_buffer.front() {
            if front.seq <= last_ack_seq {
                self.send_buffer.pop_front();
            } else {
                break;
            }
        }
    }

    /// Swap transport (reconnection). Call this when the underlying connection changes.
    pub fn set_transport(&mut self, transport_tx: impl Fn(&[u8]) + Send + Sync + 'static) {
        self.transport_tx = Box::new(transport_tx);
    }

    /// Reset the channel to initial state. Call when the remote side is a fresh
    /// connection (e.g. browser hard refresh) that doesn't have any prior seq state.
    /// Clears send buffer, reorder buffer, and resets seq counters to start fresh.
    pub fn reset(&mut self) {
        let prev_seq = self.next_seq;
        let prev_buf = self.send_buffer.len();
        let prev_reorder = self.reorder_buffer.len();
        self.next_seq = 1;
        self.send_buffer.clear();
        self.last_ack_received = 0;
        self.last_contiguous = 0;
        self.reorder_buffer.clear();
        self.ack_pending = false;
        self.chunk_assembly.clear();
        self.duplicate_ack_count = 0;
        debug!(session_id = self.session_id, prev_seq, prev_buf, prev_reorder, "ReliableChannel reset");
    }

    /// Send a resume frame to the remote side (call after reconnect).
    pub fn send_resume(&mut self) {
        let resume = frame::encode_resume(self.last_contiguous);
        (self.transport_tx)(&resume);
    }

    /// Get the last contiguous sequence received (for reconnection handshake).
    pub fn last_contiguous_received(&self) -> u32 {
        self.last_contiguous
    }

    /// Periodic tick — sends ACK if any data was received since last tick.
    pub fn tick(&mut self) {
        if self.ack_pending {
            let window = (self.config.max_send_buffer - self.reorder_buffer.len()) as u16;
            let ack = frame::encode_ack(self.last_contiguous, window);
            (self.transport_tx)(&ack);
            self.ack_pending = false;
        }
    }

    /// Re-send all unacked critical frames via the current transport.
    /// Call after a transport failover (e.g. RTC removed, WS now active)
    /// to immediately push buffered frames through the new path.
    pub fn flush_unacked(&mut self) {
        let mut count = 0u32;
        for m in &self.send_buffer {
            if m.seq > self.last_ack_received && m.priority == Priority::Critical {
                (self.transport_tx)(&m.encoded);
                count += 1;
            }
        }
        if count > 0 {
            debug!(session_id = self.session_id, count, buffer_len = self.send_buffer.len(),
                   "Flushed unacked critical frames after transport change");
        }
    }

    /// Number of messages in the send buffer awaiting ACK.
    pub fn unacked_count(&self) -> usize {
        self.send_buffer.len()
    }

    /// Number of out-of-order messages buffered for reordering.
    pub fn reorder_count(&self) -> usize {
        self.reorder_buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn mock_channel_pair() -> (ReliableChannel, ReliableChannel, Arc<Mutex<Vec<Vec<u8>>>>, Arc<Mutex<Vec<Vec<u8>>>>) {
        let a_to_b: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let b_to_a: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        let a_to_b_clone = a_to_b.clone();
        let b_to_a_clone = b_to_a.clone();

        let a = ReliableChannel::new(Config::default(), move |data: &[u8]| {
            a_to_b_clone.lock().unwrap().push(data.to_vec());
        });
        let b = ReliableChannel::new(Config::default(), move |data: &[u8]| {
            b_to_a_clone.lock().unwrap().push(data.to_vec());
        });

        (a, b, a_to_b, b_to_a)
    }

    #[test]
    fn test_send_receive_in_order() {
        let (mut a, mut b, a_to_b, _) = mock_channel_pair();

        a.send(b"hello", Priority::Critical);
        a.send(b"world", Priority::Critical);

        let wire: Vec<_> = a_to_b.lock().unwrap().clone();
        assert_eq!(wire.len(), 2);

        let msgs1 = b.receive(&wire[0]);
        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].payload, b"hello");
        assert_eq!(msgs1[0].seq, 1);

        let msgs2 = b.receive(&wire[1]);
        assert_eq!(msgs2.len(), 1);
        assert_eq!(msgs2[0].payload, b"world");
        assert_eq!(msgs2[0].seq, 2);
    }

    #[test]
    fn test_out_of_order_reordering() {
        let (mut a, mut b, a_to_b, _) = mock_channel_pair();

        a.send(b"one", Priority::Critical);
        a.send(b"two", Priority::Critical);
        a.send(b"three", Priority::Critical);

        let wire: Vec<_> = a_to_b.lock().unwrap().clone();

        // Deliver out of order: 3, 1, 2
        let msgs = b.receive(&wire[2]); // seq 3
        assert_eq!(msgs.len(), 0); // buffered, waiting for 1

        let msgs = b.receive(&wire[0]); // seq 1
        assert_eq!(msgs.len(), 1); // delivers 1
        assert_eq!(msgs[0].payload, b"one");

        let msgs = b.receive(&wire[1]); // seq 2
        assert_eq!(msgs.len(), 2); // delivers 2 AND 3 (gap filled)
        assert_eq!(msgs[0].payload, b"two");
        assert_eq!(msgs[1].payload, b"three");
    }

    #[test]
    fn test_duplicate_rejected() {
        let (mut a, mut b, a_to_b, _) = mock_channel_pair();

        a.send(b"hello", Priority::Critical);
        let wire: Vec<_> = a_to_b.lock().unwrap().clone();

        let msgs1 = b.receive(&wire[0]);
        assert_eq!(msgs1.len(), 1);

        // Send same frame again
        let msgs2 = b.receive(&wire[0]);
        assert_eq!(msgs2.len(), 0); // duplicate, rejected
    }

    #[test]
    fn test_ack_clears_send_buffer() {
        let (mut a, mut b, a_to_b, b_to_a) = mock_channel_pair();

        a.send(b"one", Priority::Critical);
        a.send(b"two", Priority::Critical);
        assert_eq!(a.unacked_count(), 2);

        // B receives and ticks (sends ACK)
        let wire: Vec<_> = a_to_b.lock().unwrap().clone();
        b.receive(&wire[0]);
        b.receive(&wire[1]);
        b.tick();

        // A processes ACK
        let acks: Vec<_> = b_to_a.lock().unwrap().clone();
        assert_eq!(acks.len(), 1);
        a.receive(&acks[0]);
        assert_eq!(a.unacked_count(), 0);
    }

    #[test]
    fn test_resume_replays_critical() {
        let (mut a, mut b, a_to_b, _) = mock_channel_pair();

        a.send(b"one", Priority::Critical);
        a.send(b"two", Priority::BestEffort);
        a.send(b"three", Priority::Critical);

        // Simulate: B received seq 1 then disconnected
        let wire: Vec<_> = a_to_b.lock().unwrap().clone();
        b.receive(&wire[0]);

        // Clear the wire, swap transport
        a_to_b.lock().unwrap().clear();
        let a_to_b2 = a_to_b.clone();
        a.set_transport(move |data: &[u8]| {
            a_to_b2.lock().unwrap().push(data.to_vec());
        });

        // B sends resume with last_ack=1
        a.receive(&frame::encode_resume(1));

        // A should replay seq 3 (critical) but NOT seq 2 (best-effort)
        let replayed: Vec<_> = a_to_b.lock().unwrap().clone();
        assert_eq!(replayed.len(), 1); // only one critical message replayed
        let msg = b.receive(&replayed[0]);
        assert_eq!(msg.len(), 0); // seq 3 is out of order (waiting for 2)
        // But seq 2 was best-effort, so it's skipped. The gap means 3 stays buffered.
        // In practice, the receiver would need to handle this gap.
        // For now, this test verifies only critical messages are replayed.
    }

    #[test]
    fn test_legacy_passthrough() {
        let (_, mut b, _, _) = mock_channel_pair();

        // JSON message — not a reliable frame
        let json = b"{\"type\":\"heartbeat\"}";
        let msgs = b.receive(json);
        assert_eq!(msgs.len(), 0); // not processed as reliable
    }

    #[test]
    fn test_retransmit_on_duplicate_ack() {
        let (mut a, mut b, a_to_b, b_to_a) = mock_channel_pair();

        // A sends 3 critical messages
        a.send(b"one", Priority::Critical);
        a.send(b"two", Priority::Critical);
        a.send(b"three", Priority::Critical);
        assert_eq!(a.unacked_count(), 3);

        let wire: Vec<_> = a_to_b.lock().unwrap().clone();
        assert_eq!(wire.len(), 3);

        // B receives only message 1 (simulating frame 2 lost in transit)
        b.receive(&wire[0]);
        assert_eq!(b.last_contiguous_received(), 1);

        // Send 3 duplicate ACK(1) frames directly to A
        let ack_frame = frame::encode_ack(1, 1000);
        a_to_b.lock().unwrap().clear();

        a.receive(&ack_frame); // new ack (1 > 0), clears seq 1, resets dup count
        a.receive(&ack_frame); // dup 1
        a.receive(&ack_frame); // dup 2
        a.receive(&ack_frame); // dup 3 → triggers retransmit

        // A should have retransmitted frames 2 and 3
        let retransmitted = a_to_b.lock().unwrap().clone();
        assert!(retransmitted.len() >= 2, "Should retransmit 2 critical frames, got {}", retransmitted.len());

        // B receives the retransmitted frames
        for frame in &retransmitted {
            b.receive(frame);
        }

        // B should now have all 3 messages
        assert_eq!(b.last_contiguous_received(), 3);
    }

    #[test]
    fn test_no_retransmit_on_new_ack() {
        let (mut a, mut b, a_to_b, b_to_a) = mock_channel_pair();

        // A sends 2 messages, B receives both
        a.send(b"one", Priority::Critical);
        a.send(b"two", Priority::Critical);

        let wire: Vec<_> = a_to_b.lock().unwrap().clone();
        b.receive(&wire[0]);
        b.receive(&wire[1]);

        // B ACKs up to 2
        b.tick();
        let acks = b_to_a.lock().unwrap().clone();
        a_to_b.lock().unwrap().clear();
        a.receive(&acks[0]);

        // All acked, no retransmit needed
        let retransmitted = a_to_b.lock().unwrap().clone();
        assert_eq!(retransmitted.len(), 0, "No retransmit when all acked");
    }

    #[test]
    fn test_retransmit_only_critical() {
        let (mut a, mut b, a_to_b, _b_to_a) = mock_channel_pair();

        // A sends 1 critical + 1 best-effort + 1 critical
        a.send(b"critical1", Priority::Critical);
        a.send(b"besteffort", Priority::BestEffort);
        a.send(b"critical2", Priority::Critical);

        let wire: Vec<_> = a_to_b.lock().unwrap().clone();

        // B receives only message 1
        b.receive(&wire[0]);

        // Send 3 duplicate ACK(1) directly
        let ack_frame = frame::encode_ack(1, 1000);
        a_to_b.lock().unwrap().clear();

        a.receive(&ack_frame); // new ack (1 > 0), clears seq 1
        a.receive(&ack_frame); // dup 1
        a.receive(&ack_frame); // dup 2
        a.receive(&ack_frame); // dup 3 → retransmit

        // Should retransmit only critical frames (not best-effort)
        let retransmitted = a_to_b.lock().unwrap().clone();
        let mut critical_count = 0;
        for f in &retransmitted {
            if let Some(decoded) = frame::decode(f) {
                if let frame::Frame::Data { priority, .. } = decoded {
                    if priority == Priority::Critical {
                        critical_count += 1;
                    } else {
                        panic!("Retransmitted a BestEffort frame");
                    }
                }
            }
        }
        assert!(critical_count >= 1, "Should retransmit at least 1 critical frame, got {}", critical_count);
    }
}
