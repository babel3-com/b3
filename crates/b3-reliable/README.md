# b3-reliable

Mosh-like reliability layer for bidirectional communication over unreliable transports. The reason your session survives a tunnel flap.

Built for Babel3's cellular voice coding sessions, where connection flaps are common (every ~10-20 min on LTE). Without this layer, every connection drop loses all in-flight terminal deltas.

## What It Does

- **Sequence numbers + CRC32** — detect corruption, reorder out-of-sequence frames
- **Send buffer** (configurable, default 1000 frames) — retransmit on loss
- **ACK/Resume protocol** — receiver sends periodic ACKs; on reconnect, sends a Resume frame with last-received sequence number and sender replays everything after it
- **Fast retransmit** — 3 duplicate ACKs trigger immediate replay without waiting for timeout
- **Priority tagging** — Critical frames (terminal deltas, TTS audio) survive buffer pressure; BestEffort frames (LED animations, notifications) get evicted first
- **Auto-chunking** — payloads larger than 32KB are automatically split into tagged chunks (`0x01` first, `0x02` middle, `0x03` last), reassembled on receive via `process_chunk()` in `channel.rs`
- **Multi-transport routing** — `MultiTransport` holds multiple named sinks (e.g., WebSocket + WebRTC) with priority-based routing and automatic failover

## Wire Format

You don't need to understand the wire format to use this crate — `ReliableChannel` handles encoding and decoding. This section is for implementers writing compatible endpoints in other languages.

All frames start with magic byte `0xB3`:

```
Data:   [0xB3][0x01][seq:u32LE][ts:u32LE][crc:u32LE][pri:u8][len:u32LE][payload]
ACK:    [0xB3][0x02][ack_seq:u32LE][window:u16LE]
Resume: [0xB3][0x03][last_ack_seq:u32LE]
```

- **Data** (19-byte header + payload): Sequence number, millisecond timestamp, CRC32 of payload, priority byte, length-prefixed payload.
- **ACK** (8 bytes): Cumulative acknowledgement up to `ack_seq`. `window` is a hint for flow control.
- **Resume** (6 bytes): Sent on reconnect. Sender replays all frames after `last_ack_seq`.

## Layering

`ReliableChannel` works standalone over any single transport. `MultiTransport` adds named sinks with priority routing and automatic failover. `SessionManager` adds per-browser isolation with reconnect persistence. Use whichever layer fits your needs — they compose but don't require each other.

## Architecture

```
┌─────────────────────────────────────────────────┐
│ SessionManager                                  │
│  └─ Session (per browser)                       │
│      ├─ MultiTransport                          │
│      │   ├─ WebSocket sink (priority 1)         │
│      │   └─ WebRTC sink (priority 0, preferred) │
│      ├─ ReliableChannel                         │
│      │   ├─ send buffer (VecDeque, max 1000)    │
│      │   ├─ reorder buffer (BTreeMap)           │
│      │   └─ chunk reassembly                    │
│      ├─ Bridge task (backend → browser)         │
│      └─ sent_offset (delta computation)         │
└─────────────────────────────────────────────────┘
```

- **MultiTransport** routes `send()` to the lowest-priority-number active sink. Hot-add/remove transports at runtime. On transport switch, unacked frames flush to the new transport.
- **ReliableChannel** wraps the `MultiTransport` with sequence tracking, ACK processing, and retransmit logic.
- **SessionManager** maps browser session IDs to `Session` instances. `reconnect_or_create()` preserves session state across transport reconnects. `ensure_bridge_alive()` restarts dead bridge tasks.

## Standalone Usage

`ReliableChannel` wraps any transport that can send bytes:

```rust
use b3_reliable::{ReliableChannel, Config, frame::Priority};

// Create a channel with your transport's send function
let channel = ReliableChannel::new(Config::default(), move |bytes: &[u8]| {
    my_transport.send(bytes);  // your WebSocket, TCP, UDP, etc.
});

// Send — automatically adds seq, CRC32, priority
channel.send(b"hello world", Priority::Critical);

// When bytes arrive from the remote side:
let messages = channel.receive(&incoming_bytes);
for msg in messages {
    // Delivered in order, deduplicated, integrity-checked
    handle(msg.payload);
}

// After a reconnect — replay unacked frames:
channel.send_resume();
```

No `MultiTransport` or `SessionManager` needed. Those are higher-level abstractions you add when you need multi-transport failover or per-browser isolation.

## Configuration

```rust
b3_reliable::Config {
    max_send_buffer: 1000,     // Max unacked frames retained
    ack_interval_ms: 100,      // ACK send frequency
    max_frame_payload: 32768,  // 32KB, auto-chunk above this
}
```

## Browser Counterpart

`browser/js/reliable.js` implements the identical wire format and protocol in vanilla JavaScript. Same magic bytes, same CRC32 polynomial (ISO 3309), same frame layout. The two sides interoperate directly.

## Dependencies

- `crc32fast` — CRC32 checksums
- `tokio` — async runtime (sync, time, macros)
- `tracing` — structured logging

## License

[Apache-2.0](../../LICENSE)
