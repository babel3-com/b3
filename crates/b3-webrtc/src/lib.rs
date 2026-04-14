//! WebRTC data channel abstraction for Babel3.
//!
//! Wraps `datachannel-rs` (libdatachannel bindings) with Babel3-specific
//! abstractions: framed message protocol, automatic 16KB chunking,
//! tokio channel bridge, and ICE/SDP signaling helpers.

pub mod channel;
pub mod peer;
pub mod signaling;

pub use channel::{ChannelMessage, ChannelSender, ChannelReceiver};
pub use peer::{B3Peer, PeerEvent, PeerConfig};
pub use signaling::{SignalingMessage, IceServer};

/// Suppress all datachannel logging by setting log level to NONE.
///
/// The `datachannel` crate's log callback accesses tracing TLS, which panics
/// during tokio thread destruction — corrupting the runtime's waker infrastructure
/// and silently killing broadcast receivers (the 4/5 restart delta freeze bug).
///
/// Setting log level to NONE with a null callback means the C library never
/// invokes ANY Rust callback. No TLS access, no panic, no corruption. We lose
/// libdatachannel's internal logging, but our own tracing at the b3-webrtc layer
/// provides sufficient observability.
///
/// Called from `B3Peer::new()` right after `RtcPeerConnection::new()` which
/// triggers the crate's `ensure_logging` Once. We immediately overwrite with NONE.
pub fn init_safe_logging() {
    unsafe {
        // RTC_LOG_NONE = 0: no callbacks, no TLS access, no panic.
        datachannel_sys::rtcInitLogger(0, None);
    }
}
