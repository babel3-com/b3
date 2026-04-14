//! b3-reliable — Mosh-like reliability layer for bidirectional communication.
//!
//! Transport-agnostic: wraps any send(bytes) / onmessage(bytes) pair.
//! Each message gets: sequence number, timestamp, CRC32 checksum, priority.
//! Receiver sends periodic ACKs. Sender retains unacked messages for replay.
//!
//! Wire format:
//!   Data:   [0xB3][0x01][seq:u32LE][ts:u32LE][crc:u32LE][pri:u8][len:u32LE][payload]
//!   ACK:    [0xB3][0x02][ack_seq:u32LE][window:u16LE]
//!   Resume: [0xB3][0x03][last_ack_seq:u32LE]

pub mod frame;
pub mod bridge;
pub mod session;
pub mod crypto;
mod channel;
mod multi;

pub use frame::{Frame, FrameType, Priority, MAGIC, is_reliable};
pub use channel::{ReliableChannel, ReceivedMessage};
pub use multi::MultiTransport;
pub use bridge::{OutboundMessage, BridgeCounters, backend_to_browser};
pub use session::{Session, SessionManager};
pub use crypto::EncryptedChannel;

/// Configuration for a ReliableChannel.
#[derive(Debug, Clone)]
pub struct Config {
    /// Max unacked messages in send buffer (default: 1000)
    pub max_send_buffer: usize,
    /// How often to send ACKs in milliseconds (default: 100)
    pub ack_interval_ms: u64,
    /// Max payload size per frame in bytes (default: 32768 = 32KB).
    /// Payloads larger than this are split into chunks automatically.
    /// Each chunk is a separate reliable frame. Receiver reassembles.
    /// Chunk framing: first byte of payload = 0x00 standalone, 0x01 first, 0x02 middle, 0x03 last.
    pub max_frame_payload: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_send_buffer: 1000,
            ack_interval_ms: 100,
            max_frame_payload: 32 * 1024, // 32KB
        }
    }
}
