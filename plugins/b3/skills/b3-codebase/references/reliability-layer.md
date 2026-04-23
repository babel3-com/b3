<!-- verified-against: b3-reliable@0.1.0, b3-cli@0.1.1409 -->

# Reliability Layer Reference

The `b3-reliable` crate provides Mosh-inspired ordered delivery with CRC32 checksums over any transport. It sits between the application and the wire — WebSocket, WebRTC, or anything that can send bytes. The browser counterpart is `browser/js/reliable.js` with an identical wire format.

---

## Architecture

Three layers, each optional:

```
┌─────────────────────────────────────────────────────┐
│ SessionManager          (per-browser isolation)      │
│  └─ Session                                          │
│      ├─ MultiTransport  (priority routing + failover)│
│      │   ├─ WebRTC sink   (priority 0, preferred)    │
│      │   └─ WebSocket sink (priority 1, fallback)    │
│      ├─ ReliableChannel (seq + CRC + ACK + retransmit│
│      │   ├─ send buffer   (VecDeque, max 1000)       │
│      │   ├─ reorder buffer (BTreeMap)                │
│      │   └─ chunk assembly (Vec<u8>)                 │
│      ├─ Bridge task     (backend → browser async)    │
│      └─ sent_offset     (delta computation)          │
└─────────────────────────────────────────────────────┘
```

**ReliableChannel** works standalone over any single transport. **MultiTransport** adds named sinks with priority routing. **SessionManager** adds per-browser isolation with reconnect persistence. Use whichever layer fits.

---

## Wire Format

All frames start with magic byte `0xB3`. Three frame types:

### Data Frame (0xB3 0x01) — 19-byte header + payload

```
Offset  Type      Size  Field
0-1     [u8; 2]   2    Magic: [0xB3, 0x01]
2-5     u32 LE    4    Sequence number (starts at 1; 0 = "nothing received")
6-9     u32 LE    4    Timestamp (ms since channel creation)
10-13   u32 LE    4    CRC32 checksum (payload only, excludes chunk tag)
14      u8        1    Priority (0=Critical, 1=BestEffort)
15-18   u32 LE    4    Payload length
19+     [u8]      var  Payload (first byte is chunk tag)
```

### ACK Frame (0xB3 0x02) — 8 bytes

```
Offset  Type      Size  Field
0-1     [u8; 2]   2    Magic: [0xB3, 0x02]
2-5     u32 LE    4    Cumulative ACK (last contiguous seq received)
6-7     u16 LE    2    Receive window (buffered message count)
```

### Resume Frame (0xB3 0x03) — 6 bytes

```
Offset  Type      Size  Field
0-1     [u8; 2]   2    Magic: [0xB3, 0x03]
2-5     u32 LE    4    Last contiguous seq received
```

### CRC32

- ISO 3309 polynomial via `crc32fast` crate
- Computed over the full payload passed to `encode_data()` — this **includes** the chunk tag byte (first byte of payload). Implementers in other languages must include the chunk tag in their CRC computation or they'll get mismatches on chunked frames.
- Corrupt frames silently dropped on decode
- Known value: `crc32(b"hello") == 0x3610A686`

---

## Auto-Chunking

Payloads exceeding `max_frame_payload` (default 32 KB) are split automatically. Each chunk gets its own sequence number, CRC32, and reliable delivery.

### Chunk Tags (first byte of payload)

| Tag | Meaning | Behavior |
|-----|---------|----------|
| `0x00` | Standalone | Deliver immediately, no reassembly |
| `0x01` | First chunk | Clear assembly buffer, begin accumulation |
| `0x02` | Middle chunk | Append to assembly buffer |
| `0x03` | Last chunk | Append and deliver complete payload |

Send-side chunking: `channel.rs:83-107`. Receive-side reassembly: `channel.rs:203-238` via `process_chunk()` using stateful `chunk_assembly: Vec<u8>` buffer.

---

## Priority System

```rust
pub enum Priority {
    Critical = 0,     // Terminal deltas, TTS audio — replayed on Resume
    BestEffort = 1,   // LED animations, notifications — skipped on replay
}
```

**Buffer eviction order:** When send buffer exceeds `max_send_buffer`:
1. Drop BestEffort frames first
2. Drop oldest Critical frames (with warning — retransmit now impossible)

**Resume behavior:** Only Critical frames are replayed after reconnect. BestEffort gaps are accepted.

---

## ReliableChannel API

### Creation

```rust
use b3_reliable::{ReliableChannel, Config, frame::Priority};

let channel = ReliableChannel::new(Config::default(), move |bytes: &[u8]| {
    my_transport.send(bytes);  // your WS, TCP, UDP, etc.
});
```

### Sending

```rust
// Auto-chunks if payload > 32KB
channel.send(b"hello world", Priority::Critical);
```

### Receiving

```rust
let messages = channel.receive(&incoming_bytes);
for msg in messages {
    // Delivered in order, deduplicated, integrity-checked
    handle(msg.payload, msg.seq, msg.priority);
}
```

### Reconnection

```rust
// After transport reconnects:
channel.set_transport(move |bytes| new_transport.send(bytes));
channel.send_resume();  // Sends Resume frame with last_contiguous_received()

// When Resume arrives from remote:
channel.handle_resume(last_ack_seq);  // Replays Critical frames after last_ack_seq
```

### Periodic Maintenance

```rust
channel.tick();  // Call every ~200ms — sends pending ACKs
```

### Key Methods

| Method | Purpose |
|--------|---------|
| `send(payload, priority)` | Send with auto-chunking |
| `receive(data) → Vec<ReceivedMessage>` | Decode, reorder, deliver |
| `send_resume()` | Send Resume frame after reconnect |
| `handle_resume(last_ack_seq)` | Replay Critical frames for remote |
| `set_transport(send_fn)` | Swap transport on reconnect |
| `tick()` | Send pending ACKs (call every ~200ms) |
| `flush_unacked()` | Retransmit all Critical frames (transport failover) |
| `reset()` | Clear all state (hard refresh) |
| `unacked_count()` | Number of unACKed frames in send buffer |
| `last_contiguous_received()` | For Resume handshake |

---

## MultiTransport API

Holds multiple named transport sinks with priority-based routing and automatic failover.

### Setup

```rust
use b3_reliable::{MultiTransport, Config, frame::Priority};

let mut multi = MultiTransport::new(Config::default());

// Add transports (lower priority number = preferred)
multi.add_transport("rtc", 0, move |bytes| rtc_send(bytes));
multi.add_transport("ws", 1, move |bytes| ws_send(bytes));
```

### Routing

- `send()` routes to the lowest-priority-number active sink (RTC preferred over WS)
- If the preferred transport dies, call `remove_transport("rtc")` — traffic falls to WS
- When RTC reconnects, `add_transport("rtc", 0, ...)` — it steals priority back
- `flush_unacked()` called automatically on add/remove to recover in-flight frames

### Keepalive

```rust
multi.keepalive_all();  // Send ACK to ALL transports (call every ~5s)
                        // Prevents watchdog kills on idle fallback transports
```

### Key Methods

| Method | Purpose |
|--------|---------|
| `add_transport(name, priority, send_fn)` | Hot-add sink, auto-sorts, flushes unacked |
| `remove_transport(name)` | Remove sink, flushes unacked to remaining |
| `send(payload, priority)` | Route to best sink |
| `receive(data) → Vec<ReceivedMessage>` | Delegate to ReliableChannel |
| `tick()` | ACK coalescing (every 200ms) |
| `keepalive_all()` | ACK to all transports (every 5s) |
| `active_transport() → Option<String>` | Current best sink name |
| `transport_names() → Vec<(String, u8)>` | All sinks with priorities |

---

## SessionManager API

Maps browser session IDs to isolated `Session` instances. Each session has its own `MultiTransport`, `ReliableChannel`, bridge task, and delta offset.

### Key Methods

| Method | Purpose |
|--------|---------|
| `create(config) → (Session, backend_rx)` | New session with auto-assigned ID |
| `create_with_id(id, config)` | Explicit ID (e.g., browser client_id) |
| `get(id) → Option<Session>` | Lookup by ID |
| `most_recent() → Option<Session>` | Fallback for old browsers without client_id |
| `reconnect_or_create(client_id, config)` | Reuse existing or create new — preserves send buffer on reconnect |
| `remove(id)` | Destroy session, abort bridge and event tasks |
| `count()` | Active session count |

### Session Fields

| Field | Type | Purpose |
|-------|------|---------|
| `id` | `u64` | Browser session identifier |
| `multi` | `Arc<Mutex<MultiTransport>>` | Per-session transport routing |
| `sent_offset` | `Arc<Mutex<u64>>` | Delta computation for terminal output |
| `rtc_active` | `Arc<AtomicBool>` | Whether WebRTC is connected |
| `backend_tx` | `Arc<Mutex<Sender<OutboundMessage>>>` | Feed data into bridge |

---

## Bridge Task

`backend_to_browser()` is the async loop that moves data from the daemon backend to the browser via MultiTransport.

```rust
pub async fn backend_to_browser(
    multi: Arc<Mutex<MultiTransport>>,
    mut backend_rx: Receiver<OutboundMessage>,
)
```

**Timers:**
- Every 200ms: `multi.tick()` (send pending ACKs). Note: this is the bridge's tick cadence, distinct from `ack_interval_ms` (default 100ms) which is the ACK coalescing window inside ReliableChannel. The bridge ticks at 200ms; the channel considers an ACK "pending" after 100ms of received data.
- Every 5s: `multi.keepalive_all()` (prevent idle transport kills)
- On message: `multi.send(payload, priority)` (route via best sink)

---

## Reconnection Scenarios

### Normal reconnect (frame replay)

Browser sends `Resume(last_ack=50)`. Daemon has seq 51-100 in send buffer. Daemon replays all Critical frames after seq 50. BestEffort gaps are skipped.

### Daemon restart (seq mismatch)

Daemon restarts at `next_seq=1`. Browser sends `Resume(last_ack=3217)`. Daemon detects `last_ack >= next_seq`, jumps `next_seq = 3218`, resets both directions.

### Browser hard refresh

New ReliableChannel at browser starts at seq 0. Daemon receives `Resume(last_ack=0)`, resets inbound `last_contiguous = 0` to accept seq=1 frames.

---

## Fast Retransmit

TCP-style fast retransmit: 3 duplicate ACKs of the same sequence number trigger immediate retransmit of all unACKed Critical frames, without waiting for a timeout. Location: `channel.rs:240-273`.

---

## Configuration

```rust
b3_reliable::Config {
    max_send_buffer: 1000,     // Max unacked frames before eviction
    ack_interval_ms: 100,      // ACK coalescing interval
    max_frame_payload: 32768,  // 32KB — triggers auto-chunking above this
}
```

---

## Browser Counterpart

`browser/js/reliable.js` implements the identical wire format and protocol in vanilla JavaScript. Same magic bytes (`0xB3`), same CRC32 polynomial (ISO 3309), same frame layout, same chunk tags. The two sides interoperate directly.

Heartbeat timeout: 15s (browser kills zombie transports that stop sending).
