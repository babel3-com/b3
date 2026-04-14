# b3-webrtc

WebRTC data channel abstraction for Babel3. Wraps [`datachannel-rs`](https://github.com/niclas-AEA/datachannel-rs) (libdatachannel C bindings) with Babel3-specific framing, chunking, and signaling.

Used by the daemon to establish P2P data channels with browsers. Paired with `b3-reliable` for ordered, checksummed delivery.

> **War story included.** If you've ever debugged a silent runtime corruption caused by a C library calling back into Rust during thread shutdown — read the [TLS Panic Fix](#the-tls-panic-fix) section. It took days to find and two lines to fix.

## What It Does

- **Framed message protocol** — length-prefixed binary messages over raw data channels
- **Automatic 16KB chunking** — splits large messages to fit WebRTC SCTP constraints, reassembles on receive
- **Tokio channel bridge** — `ChannelSender` / `ChannelReceiver` backed by tokio mpsc channels, so data channel I/O integrates with async Rust
- **ICE/SDP signaling helpers** — `SignalingMessage` enum for offer/answer/candidate exchange
- **Safe logging** — suppresses libdatachannel's C++ log callback to prevent TLS panics during tokio thread destruction

## Components

### `B3Peer`

The main entry point. Creates a `RtcPeerConnection`, sets up ICE handling, and exposes a stream of `PeerEvent`s (connection state changes, data channel open/close, signaling messages).

```rust
let peer = B3Peer::new(PeerConfig {
    ice_servers: vec![IceServer { url: "turn:relay.babel3.com:3478".into(), .. }],
}).await?;
```

### `ChannelSender` / `ChannelReceiver`

Typed wrappers around tokio channels connected to a WebRTC data channel. Send and receive `ChannelMessage` (binary or text).

### `SignalingMessage`

```rust
enum SignalingMessage {
    Offer(String),
    Answer(String),
    Candidate { candidate: String, mid: String },
}
```

Exchange these via your signaling path (Babel3 uses the EC2 relay's SSE stream).

## The TLS Panic Fix

The `datachannel` crate's log callback accesses thread-local storage (TLS) via Rust's `tracing` crate. During tokio runtime shutdown, worker threads are destroyed — and if libdatachannel fires a log callback during destruction, the TLS access panics. This corrupts the runtime's waker infrastructure, silently killing broadcast receivers. This was the root cause of the "4/5 restart delta freeze" bug.

The fix: `init_safe_logging()` calls `rtcInitLogger(RTC_LOG_NONE, None)` immediately after peer creation, overwriting the crate's default callback with nothing. No TLS access, no panic, no corruption. We lose libdatachannel's internal logging but our own tracing at the `b3-webrtc` layer provides sufficient observability.

## Dependencies

- `datachannel` v0.16 (builds libdatachannel from source via `vendored` Cargo feature) — WebRTC bindings
- `datachannel-sys` v0.23 — direct FFI for `rtcInitLogger`
- `tokio` — async runtime
- `serde` / `serde_json` — signaling message serialization
- `tracing` — structured logging
- `anyhow` — error handling

## License

[Apache-2.0](../../LICENSE)
