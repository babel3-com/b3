//! Framed message protocol over WebRTC data channels.
//!
//! All data channels use a consistent wire format:
//!   [type: u8][length: u32 LE][payload: `length` bytes]
//!
//! Messages larger than 16KB are automatically chunked and reassembled
//! (Firefox SCTP fragmentation issue with Chromium).

use std::sync::Arc;
use serde::{Deserialize, Serialize};

/// Maximum safe message size across all browsers.
/// Firefox fragments using a deprecated SCTP mechanism that Chromium
/// doesn't reassemble — so we chunk at 16KB ourselves.
const MAX_CHUNK_SIZE: usize = 16 * 1024;

/// Overhead per chunk: seq(4) + total(4) + original_type(1) = 9 bytes.
const CHUNK_HEADER_SIZE: usize = 9;

/// Frame header: type(1) + length(4) = 5 bytes.
const FRAME_HEADER_SIZE: usize = 5;

/// Payload capacity per chunk after both frame header and chunk header.
const CHUNK_PAYLOAD_SIZE: usize = MAX_CHUNK_SIZE - FRAME_HEADER_SIZE - CHUNK_HEADER_SIZE;

/// Wire type tags.
const TYPE_JSON: u8 = 0x01;
const TYPE_BINARY: u8 = 0x02;
const TYPE_PING: u8 = 0x03;
const TYPE_PONG: u8 = 0x04;
const TYPE_CHUNK: u8 = 0x05;

/// A framed message exchanged over a data channel.
#[derive(Debug, Clone)]
pub enum ChannelMessage {
    /// JSON text message (terminal control, RPC, events).
    Json(serde_json::Value),
    /// Binary data (audio chunks, PTY bytes).
    Binary(Vec<u8>),
    /// Keepalive ping.
    Ping,
    /// Keepalive pong.
    Pong,
}

impl ChannelMessage {
    /// Encode to wire format. Returns one or more frames (chunked if needed).
    pub fn encode(&self) -> Vec<Vec<u8>> {
        let (type_tag, payload) = match self {
            ChannelMessage::Json(v) => {
                (TYPE_JSON, serde_json::to_vec(v).unwrap_or_default())
            }
            ChannelMessage::Binary(data) => (TYPE_BINARY, data.clone()),
            ChannelMessage::Ping => return vec![Self::encode_simple(TYPE_PING)],
            ChannelMessage::Pong => return vec![Self::encode_simple(TYPE_PONG)],
        };

        let total_size = 1 + 4 + payload.len(); // type + length + payload
        if total_size <= MAX_CHUNK_SIZE {
            // Fits in a single frame.
            let mut frame = Vec::with_capacity(total_size);
            frame.push(type_tag);
            frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            frame.extend_from_slice(&payload);
            return vec![frame];
        }

        // Chunk the payload.
        let total_chunks =
            (payload.len() + CHUNK_PAYLOAD_SIZE - 1) / CHUNK_PAYLOAD_SIZE;
        let mut frames = Vec::with_capacity(total_chunks);

        for (seq, chunk_data) in payload.chunks(CHUNK_PAYLOAD_SIZE).enumerate() {
            let frame_len = CHUNK_HEADER_SIZE + chunk_data.len();
            let mut frame = Vec::with_capacity(1 + 4 + frame_len);
            frame.push(TYPE_CHUNK);
            frame.extend_from_slice(&(frame_len as u32).to_le_bytes());
            // Chunk header: seq, total, original type.
            frame.extend_from_slice(&(seq as u32).to_le_bytes());
            frame.extend_from_slice(&(total_chunks as u32).to_le_bytes());
            frame.push(type_tag);
            frame.extend_from_slice(chunk_data);
            frames.push(frame);
        }

        frames
    }

    fn encode_simple(type_tag: u8) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5);
        frame.push(type_tag);
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame
    }

    /// Decode a single wire frame. Returns `None` if this is a chunk that
    /// needs more pieces — feed it to a [`ChunkAssembler`] instead.
    pub fn decode(data: &[u8]) -> Result<DecodeResult, DecodeError> {
        if data.len() < 5 {
            return Err(DecodeError::TooShort);
        }

        let type_tag = data[0];
        let length = u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize;

        if data.len() < 5 + length {
            return Err(DecodeError::Truncated {
                expected: 5 + length,
                got: data.len(),
            });
        }

        let payload = &data[5..5 + length];

        match type_tag {
            TYPE_JSON => {
                let value = serde_json::from_slice(payload)
                    .map_err(|e| DecodeError::InvalidJson(e.to_string()))?;
                Ok(DecodeResult::Complete(ChannelMessage::Json(value)))
            }
            TYPE_BINARY => {
                Ok(DecodeResult::Complete(ChannelMessage::Binary(
                    payload.to_vec(),
                )))
            }
            TYPE_PING => Ok(DecodeResult::Complete(ChannelMessage::Ping)),
            TYPE_PONG => Ok(DecodeResult::Complete(ChannelMessage::Pong)),
            TYPE_CHUNK => {
                if payload.len() < CHUNK_HEADER_SIZE {
                    return Err(DecodeError::InvalidChunk);
                }
                let seq =
                    u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
                        as usize;
                let total = u32::from_le_bytes([
                    payload[4], payload[5], payload[6], payload[7],
                ]) as usize;
                if total > MAX_CHUNKS {
                    return Err(DecodeError::InvalidChunk);
                }
                let original_type = payload[8];
                let chunk_data = payload[9..].to_vec();
                Ok(DecodeResult::Chunk {
                    seq,
                    total,
                    original_type,
                    data: chunk_data,
                })
            }
            _ => Err(DecodeError::UnknownType(type_tag)),
        }
    }
}

/// Result of decoding a wire frame.
#[derive(Debug)]
pub enum DecodeResult {
    /// A complete, non-chunked message.
    Complete(ChannelMessage),
    /// A chunk that needs assembly.
    Chunk {
        seq: usize,
        total: usize,
        original_type: u8,
        data: Vec<u8>,
    },
}

/// Errors during decode.
#[derive(Debug)]
pub enum DecodeError {
    TooShort,
    Truncated { expected: usize, got: usize },
    InvalidJson(String),
    InvalidChunk,
    UnknownType(u8),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "frame too short (< 5 bytes)"),
            DecodeError::Truncated { expected, got } => {
                write!(f, "frame truncated: expected {expected}, got {got}")
            }
            DecodeError::InvalidJson(e) => write!(f, "invalid JSON: {e}"),
            DecodeError::InvalidChunk => write!(f, "invalid chunk header"),
            DecodeError::UnknownType(t) => write!(f, "unknown type tag: 0x{t:02x}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Maximum number of chunks allowed per chunked message.
const MAX_CHUNKS: usize = 256;

/// Reassembles chunked messages.
pub struct ChunkAssembler {
    chunks: Vec<Option<Vec<u8>>>,
    original_type: u8,
    total: usize,
    received: usize,
}

impl ChunkAssembler {
    /// Start assembling a chunked message.
    /// Returns `None` if `total` exceeds `MAX_CHUNKS` (256).
    pub fn new(total: usize, original_type: u8) -> Self {
        let capped = total.min(MAX_CHUNKS);
        Self {
            chunks: vec![None; capped],
            original_type,
            total: capped,
            received: 0,
        }
    }

    /// Feed a chunk. Returns the assembled message when all chunks are received.
    pub fn feed(&mut self, seq: usize, data: Vec<u8>) -> Option<ChannelMessage> {
        if seq >= self.total {
            return None;
        }
        if self.chunks[seq].is_none() {
            self.received += 1;
        }
        self.chunks[seq] = Some(data);

        if self.received < self.total {
            return None;
        }

        // All chunks received — reassemble.
        let mut payload = Vec::new();
        for chunk in &self.chunks {
            if let Some(data) = chunk {
                payload.extend_from_slice(data);
            }
        }

        match self.original_type {
            TYPE_JSON => serde_json::from_slice(&payload)
                .ok()
                .map(ChannelMessage::Json),
            TYPE_BINARY => Some(ChannelMessage::Binary(payload)),
            _ => None,
        }
    }
}

/// Sender half — wraps a datachannel send function for ergonomic use.
/// Clone-safe via Arc.
#[derive(Clone, Debug)]
pub struct ChannelSender {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Set to false by the send loop when dc.send() fails.
    /// Checked by send_raw() before touching the mpsc — instant zombie detection.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl ChannelSender {
    pub fn new(sender: tokio::sync::mpsc::Sender<Vec<u8>>, alive: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { sender, alive }
    }

    /// Send a framed message, chunking if needed.
    pub async fn send(&self, msg: ChannelMessage) -> anyhow::Result<()> {
        for frame in msg.encode() {
            self.sender
                .send(frame)
                .await
                .map_err(|_| anyhow::anyhow!("data channel closed"))?;
        }
        Ok(())
    }

    /// Send raw JSON value.
    pub async fn send_json(&self, value: serde_json::Value) -> anyhow::Result<()> {
        self.send(ChannelMessage::Json(value)).await
    }

    /// Send binary data (audio chunks, PTY bytes).
    pub async fn send_binary(&self, data: Vec<u8>) -> anyhow::Result<()> {
        self.send(ChannelMessage::Binary(data)).await
    }

    /// Send a ping.
    pub async fn ping(&self) -> anyhow::Result<()> {
        self.send(ChannelMessage::Ping).await
    }

    /// Send raw bytes without ChannelMessage framing.
    /// Use this when the payload is already framed (e.g. b3-reliable frames).
    pub async fn send_raw(&self, data: Vec<u8>) -> anyhow::Result<()> {
        if !self.alive.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("data channel dead");
        }
        self.sender
            .send(data)
            .await
            .map_err(|_| anyhow::anyhow!("data channel closed"))
    }
}

/// Check if bytes look like a b3-reliable frame (magic 0xB3, 0x01/0x02/0x03).
pub fn is_b3_reliable_frame(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xB3 && (data[1] == 0x01 || data[1] == 0x02 || data[1] == 0x03)
}

/// Receiver half — delivers decoded ChannelMessages.
#[derive(Debug)]
pub struct ChannelReceiver {
    receiver: tokio::sync::mpsc::Receiver<ChannelMessage>,
}

impl ChannelReceiver {
    pub fn new(receiver: tokio::sync::mpsc::Receiver<ChannelMessage>) -> Self {
        Self { receiver }
    }

    /// Receive the next message (blocks until available or channel closes).
    pub async fn recv(&mut self) -> Option<ChannelMessage> {
        self.receiver.recv().await
    }
}

/// Serializable RPC request over control channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: String,
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

/// Serializable RPC response over control channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

/// Server-push event (no request id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushEvent {
    pub event: String,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let msg = ChannelMessage::Json(serde_json::json!({"hello": "world"}));
        let frames = msg.encode();
        assert_eq!(frames.len(), 1);
        match ChannelMessage::decode(&frames[0]).unwrap() {
            DecodeResult::Complete(ChannelMessage::Json(v)) => {
                assert_eq!(v["hello"], "world");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_binary() {
        let data = vec![0u8; 100];
        let msg = ChannelMessage::Binary(data.clone());
        let frames = msg.encode();
        assert_eq!(frames.len(), 1);
        match ChannelMessage::decode(&frames[0]).unwrap() {
            DecodeResult::Complete(ChannelMessage::Binary(d)) => {
                assert_eq!(d, data);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_ping_pong() {
        for msg in [ChannelMessage::Ping, ChannelMessage::Pong] {
            let frames = msg.encode();
            assert_eq!(frames.len(), 1);
            let decoded = ChannelMessage::decode(&frames[0]).unwrap();
            match (&msg, decoded) {
                (ChannelMessage::Ping, DecodeResult::Complete(ChannelMessage::Ping)) => {}
                (ChannelMessage::Pong, DecodeResult::Complete(ChannelMessage::Pong)) => {}
                _ => panic!("mismatch"),
            }
        }
    }

    #[test]
    fn chunking_large_binary() {
        // 40KB binary — should produce 3 chunks (16KB payload each).
        let data = vec![42u8; 40 * 1024];
        let msg = ChannelMessage::Binary(data.clone());
        let frames = msg.encode();
        assert!(frames.len() > 1, "should be chunked");

        let mut assembler: Option<ChunkAssembler> = None;
        let mut result = None;

        for frame in &frames {
            match ChannelMessage::decode(frame).unwrap() {
                DecodeResult::Chunk {
                    seq,
                    total,
                    original_type,
                    data: chunk_data,
                } => {
                    let asm = assembler
                        .get_or_insert_with(|| ChunkAssembler::new(total, original_type));
                    if let Some(msg) = asm.feed(seq, chunk_data) {
                        result = Some(msg);
                    }
                }
                _ => panic!("expected chunk"),
            }
        }

        match result.unwrap() {
            ChannelMessage::Binary(d) => assert_eq!(d, data),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
