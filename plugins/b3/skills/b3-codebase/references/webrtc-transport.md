<!-- verified-against: b3-webrtc@0.1.0, b3-cli@0.1.1409 -->

# WebRTC Transport Reference

The `b3-webrtc` crate wraps `datachannel-rs` (libdatachannel C bindings) with Babel3-specific framing, chunking, and signaling. Used by the daemon to establish P2P data channels with browsers. Paired with `b3-reliable` for ordered, checksummed delivery.

---

## Architecture

```
Browser                           Daemon
┌───────────────┐                ┌────────────────────────┐
│ RTCPeerConn   │  SDP/ICE via   │ B3Peer                 │
│               │◄──── EC2 ─────►│  ├─ RtcPeerConnection   │
│ Data channels:│                │  ├─ ChannelSender/Recv  │
│  "control"    │◄──────────────►│  │   (tokio mpsc bridge)│
│  "terminal"   │◄──────────────►│  └─ init_safe_logging() │
│  "audio"      │◄──────────────►│                        │
└───────────────┘                └────────────────────────┘
```

---

## B3Peer — Main Entry Point

Creates a `RtcPeerConnection`, sets up ICE handling, and exposes an event-driven async interface.

### Creation

```rust
use b3_webrtc::{B3Peer, PeerConfig, IceServer};

let mut peer = B3Peer::new(PeerConfig {
    ice_servers: vec![IceServer {
        urls: vec!["turn:relay.babel3.com:3478".into()],
        username: Some("user".into()),
        credential: Some("pass".into()),
    }],
    force_relay: false,  // true = TURN only (testing)
})?;
```

**Note:** `B3Peer::new()` calls `init_safe_logging()` immediately after peer creation to prevent the TLS panic (see below).

### Creating Data Channels

```rust
let (sender, receiver, open_notify) = peer.create_channel("terminal")?;
open_notify.await?;  // Wait for channel to open
sender.send(ChannelMessage::Binary(data)).await?;
```

### Signaling Flow

```rust
// 1. Create offer
peer.create_offer()?;

// 2. Process events
loop {
    match peer.next_event().await {
        Some(PeerEvent::LocalDescription { sdp, sdp_type }) => {
            // Send SDP to remote via signaling server
        }
        Some(PeerEvent::LocalCandidate { candidate, mid }) => {
            // Send ICE candidate to remote (trickle ICE)
        }
        Some(PeerEvent::IncomingChannel { label, sender, receiver }) => {
            // Handle incoming data channel from remote
        }
        Some(PeerEvent::ConnectionStateChange(state)) => {
            // "connected", "disconnected", "failed", etc.
        }
        Some(PeerEvent::GatheringComplete) => {
            // All ICE candidates gathered
        }
        None => break,  // Peer closed
    }
}

// 3. Apply remote SDP
peer.set_remote_description(&sdp, "answer")?;

// 4. Add trickle ICE candidates
peer.add_remote_candidate(&candidate, &mid)?;
```

### Key Methods

| Method | Purpose |
|--------|---------|
| `new(config) → B3Peer` | Create peer, init safe logging |
| `create_channel(label) → (Sender, Receiver, open_notify)` | Create local data channel |
| `create_offer()` | Generate SDP offer |
| `set_remote_description(sdp, type)` | Apply remote SDP (offer/answer/pranswer/rollback) |
| `add_remote_candidate(candidate, mid)` | Trickle ICE |
| `next_event() → Option<PeerEvent>` | Receive next event (async) |
| `local_description() → Option<String>` | Get full SDP after gathering |

**ICE restart is not supported** by datachannel-rs/libdatachannel. Recovery path: drop `B3Peer` entirely and create a new one.

---

## Wire Format (Data Channels)

All data channel messages use framed encoding:

```
[type: u8][length: u32 LE][payload: length bytes]
```

### Type Tags

| Tag | Type | Description |
|-----|------|-------------|
| `0x01` | JSON | Text messages (control, RPC, events) |
| `0x02` | Binary | Audio chunks, PTY bytes |
| `0x03` | Ping | Keepalive |
| `0x04` | Pong | Keepalive response |
| `0x05` | Chunk | Fragment of message > 16KB |

### Auto-Chunking (16KB SCTP limit)

Messages > 16KB are split into chunks to work around Firefox SCTP fragmentation (Chrome doesn't reassemble Firefox's deprecated SCTP mechanism).

```
Max chunk size:    16,384 bytes (16KB)
Frame header:      5 bytes (type + length)
Chunk header:      9 bytes (seq: u32 + total: u32 + original_type: u8)
Chunk payload:     16,370 bytes
Max chunks:        256 per message
```

**ChunkAssembler** reassembles on the receiving side, keyed by chunk sequence.

### b3-reliable Frame Detection

`is_b3_reliable_frame(data)` checks for magic `0xB3` followed by `0x01/0x02/0x03`. When detected, data channel handlers pass frames through to the reliability layer without ChannelMessage decoding.

---

## ChannelSender / ChannelReceiver

Typed wrappers around tokio mpsc channels, bridging async Rust to the datachannel-rs callback model.

### ChannelSender (Clone-safe via Arc)

```rust
sender.send(ChannelMessage::Json(value)).await?;
sender.send_json(value).await?;
sender.send_binary(data).await?;
sender.ping().await?;
sender.send_raw(bytes).await?;  // Raw bytes, no ChannelMessage framing
```

**Zombie detection:** `alive` flag (AtomicBool) set to false on send errors. Checked by `send_raw()` for instant detection of dead channels.

### ChannelReceiver

```rust
while let Some(msg) = receiver.recv().await {
    match msg {
        ChannelMessage::Json(value) => { /* RPC, control */ }
        ChannelMessage::Binary(data) => { /* PTY, audio */ }
        ChannelMessage::Ping => { /* respond with pong */ }
        ChannelMessage::Pong => { /* keepalive confirmed */ }
    }
}
```

---

## Signaling Types

### SignalingMessage

```rust
pub enum SignalingMessage {
    Offer { session_id: String, sdp: String, target: SignalingTarget },
    Answer { session_id: String, sdp: String },
    Ice { session_id: String, candidate: String, mid: String },
}
```

Serialized with a `type` field (tag-based serde) for JSON transport.

### SignalingTarget

```rust
pub enum SignalingTarget {
    Daemon,      // Browser → daemon data channel
    GpuWorker,   // Browser → GPU sidecar data channel
}
```

### Signaling Flow

```
Browser → POST /api/agents/:id/webrtc/offer → EC2 → SSE → Daemon
Daemon → POST /api/agents/:id/webrtc/answer → EC2 → SSE → Browser
Browser ←→ Trickle ICE candidates ←→ Daemon (via EC2 relay)
```

EC2 blocks on the offer POST until the target answers (10s timeout).

---

## TURN Credentials

```rust
pub fn generate_turn_credentials(shared_secret: &str, ttl_secs: u64) -> (String, String)
```

RFC 7635 TURN REST API credentials:
- Username: `"{expiry_timestamp}:b3"`
- Credential: `base64(HMAC-SHA1(shared_secret, username))`
- Self-contained SHA-1 and HMAC implementation (no external crypto dependency)

### ICE Configuration

| Parameter | Default | Env Override |
|-----------|---------|-------------|
| STUN server | `stun:stun.l.google.com:19302` | `STUN_URL` |
| TURN host | Direct IP (not through CF proxy) | `TURN_HOST` |
| TURN secret | coturn static-auth-secret | `TURN_SECRET` |

**TURN credential encoding:** Colons in username/password are URL-encoded as `%3A` to prevent libjuice parsing errors at the wrong delimiter.

---

## The TLS Panic Fix

**The bug:** The `datachannel` crate's log callback accesses thread-local storage (TLS) via Rust's `tracing` crate. During tokio runtime shutdown, worker threads are destroyed. If libdatachannel fires a log callback during destruction, the TLS access panics. This corrupts tokio's waker infrastructure, silently killing broadcast receivers. Root cause of the "4/5 restart delta freeze" bug.

**The fix:** `init_safe_logging()` calls `rtcInitLogger(RTC_LOG_NONE, None)` via unsafe FFI immediately after peer creation. This overwrites the crate's default callback with nothing. No TLS access, no panic, no corruption.

**Trade-off:** Loses libdatachannel's internal logging. Our own tracing at the `b3-webrtc` layer provides sufficient observability.

**Location:** `lib.rs:init_safe_logging()`, called from `B3Peer::new()`.

---

## RPC Protocol (over data channels)

### RpcRequest

```json
{"id": "uuid", "method": "POST", "path": "/api/tts", "body": {...}}
```

### RpcResponse

```json
{"id": "uuid", "status": 200, "body": {...}}
```

### PushEvent (server-initiated)

```json
{"event": "tts_audio", "msg_id": "...", "chunk": 1, "total": 3}
```

---

## Three Data Channels Per Connection

| Channel | Ordering | Purpose |
|---------|----------|---------|
| `"control"` | ordered, reliable | JSON RPC, auth, control messages |
| `"terminal"` | ordered, reliable | Raw PTY bytes, bidirectional |
| `"audio"` | ordered, reliable | TTS WAV chunks |

---

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `datachannel` | 0.16 | libdatachannel bindings (built from source via `vendored` Cargo feature) |
| `datachannel-sys` | 0.23 | Direct FFI for `rtcInitLogger` (TLS panic fix) |
| `tokio` | 1 | Async runtime |
| `serde` / `serde_json` | 1 | Signaling message serialization |
| `tracing` | 0.1 | Structured logging |
| `anyhow` | 1 | Error handling |
