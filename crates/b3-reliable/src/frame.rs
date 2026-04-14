//! Wire format: encode/decode reliable frames with CRC32 checksums.

use crc32fast::Hasher;

/// Magic bytes identifying a reliable frame.
pub const MAGIC: [u8; 2] = [0xB3, 0x01];
pub const MAGIC_ACK: [u8; 2] = [0xB3, 0x02];
pub const MAGIC_RESUME: [u8; 2] = [0xB3, 0x03];

/// Header size for data frames (magic + seq + ts + crc + pri + len).
pub const DATA_HEADER_SIZE: usize = 2 + 4 + 4 + 4 + 1 + 4; // 19 bytes
/// ACK frame size (magic + ack_seq + window).
pub const ACK_SIZE: usize = 2 + 4 + 2; // 8 bytes
/// Resume frame size (magic + last_ack_seq).
pub const RESUME_SIZE: usize = 2 + 4; // 6 bytes

/// Message priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Priority {
    /// Must deliver — replayed on reconnect.
    Critical = 0,
    /// Skip on replay — latest state wins.
    BestEffort = 1,
}

impl Priority {
    pub fn from_u8(v: u8) -> Self {
        if v == 0 { Priority::Critical } else { Priority::BestEffort }
    }
}

/// Frame type after parsing.
#[derive(Debug)]
pub enum FrameType {
    /// Returns true if bytes start with a reliable magic prefix.
    Legacy,
}

/// A decoded frame.
#[derive(Debug)]
pub enum Frame {
    /// Data frame with payload.
    Data {
        seq: u32,
        timestamp: u32,
        checksum: u32,
        priority: Priority,
        payload: Vec<u8>,
    },
    /// ACK frame.
    Ack {
        ack_seq: u32,
        window: u16,
    },
    /// Resume frame (reconnection handshake).
    Resume {
        last_ack_seq: u32,
    },
}

/// Check if bytes start with a reliable frame magic.
pub fn is_reliable(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xB3 && (data[1] >= 0x01 && data[1] <= 0x03)
}

/// Compute CRC32 of a byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Encode a data frame.
pub fn encode_data(seq: u32, timestamp: u32, priority: Priority, payload: &[u8]) -> Vec<u8> {
    let checksum = crc32(payload);
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(DATA_HEADER_SIZE + payload.len());
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&timestamp.to_le_bytes());
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf.push(priority as u8);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Encode an ACK frame.
pub fn encode_ack(ack_seq: u32, window: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ACK_SIZE);
    buf.extend_from_slice(&MAGIC_ACK);
    buf.extend_from_slice(&ack_seq.to_le_bytes());
    buf.extend_from_slice(&window.to_le_bytes());
    buf
}

/// Encode a resume frame.
pub fn encode_resume(last_ack_seq: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RESUME_SIZE);
    buf.extend_from_slice(&MAGIC_RESUME);
    buf.extend_from_slice(&last_ack_seq.to_le_bytes());
    buf
}

/// Decode a frame from bytes. Returns None if not a reliable frame or corrupt.
pub fn decode(data: &[u8]) -> Option<Frame> {
    if data.len() < 2 {
        return None;
    }
    match (data[0], data[1]) {
        (0xB3, 0x01) => decode_data(data),
        (0xB3, 0x02) => decode_ack(data),
        (0xB3, 0x03) => decode_resume(data),
        _ => None,
    }
}

fn decode_data(data: &[u8]) -> Option<Frame> {
    if data.len() < DATA_HEADER_SIZE {
        return None;
    }
    let seq = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let timestamp = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
    let checksum = u32::from_le_bytes([data[10], data[11], data[12], data[13]]);
    let priority = Priority::from_u8(data[14]);
    let payload_len = u32::from_le_bytes([data[15], data[16], data[17], data[18]]) as usize;

    if data.len() < DATA_HEADER_SIZE + payload_len {
        return None;
    }
    let payload = data[DATA_HEADER_SIZE..DATA_HEADER_SIZE + payload_len].to_vec();

    // Verify checksum
    let computed = crc32(&payload);
    if computed != checksum {
        return None; // Corrupt
    }

    Some(Frame::Data {
        seq,
        timestamp,
        checksum,
        priority,
        payload,
    })
}

fn decode_ack(data: &[u8]) -> Option<Frame> {
    if data.len() < ACK_SIZE {
        return None;
    }
    let ack_seq = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let window = u16::from_le_bytes([data[6], data[7]]);
    Some(Frame::Ack { ack_seq, window })
}

fn decode_resume(data: &[u8]) -> Option<Frame> {
    if data.len() < RESUME_SIZE {
        return None;
    }
    let last_ack_seq = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    Some(Frame::Resume { last_ack_seq })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_roundtrip() {
        let payload = b"hello world";
        let encoded = encode_data(42, 1000, Priority::Critical, payload);
        let frame = decode(&encoded).unwrap();
        match frame {
            Frame::Data { seq, timestamp, priority, payload: p, .. } => {
                assert_eq!(seq, 42);
                assert_eq!(timestamp, 1000);
                assert_eq!(priority, Priority::Critical);
                assert_eq!(p, b"hello world");
            }
            _ => panic!("expected Data frame"),
        }
    }

    #[test]
    fn test_ack_roundtrip() {
        let encoded = encode_ack(100, 500);
        let frame = decode(&encoded).unwrap();
        match frame {
            Frame::Ack { ack_seq, window } => {
                assert_eq!(ack_seq, 100);
                assert_eq!(window, 500);
            }
            _ => panic!("expected Ack frame"),
        }
    }

    #[test]
    fn test_resume_roundtrip() {
        let encoded = encode_resume(77);
        let frame = decode(&encoded).unwrap();
        match frame {
            Frame::Resume { last_ack_seq } => assert_eq!(last_ack_seq, 77),
            _ => panic!("expected Resume frame"),
        }
    }

    #[test]
    fn test_corrupt_checksum_rejected() {
        let mut encoded = encode_data(1, 0, Priority::Critical, b"test");
        // Flip a bit in the payload
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF;
        assert!(decode(&encoded).is_none());
    }

    #[test]
    fn test_is_reliable() {
        assert!(is_reliable(&[0xB3, 0x01, 0, 0]));
        assert!(is_reliable(&[0xB3, 0x02, 0, 0]));
        assert!(is_reliable(&[0xB3, 0x03, 0, 0]));
        assert!(!is_reliable(&[0x7B, 0x22])); // JSON "{"
        assert!(!is_reliable(&[0x01, 0x00])); // RTC binary
        assert!(!is_reliable(&[]));
    }

    #[test]
    fn test_legacy_passthrough() {
        // JSON message should not be detected as reliable
        let json = b"{\"type\":\"heartbeat\"}";
        assert!(!is_reliable(json));
        assert!(decode(json).is_none());
    }

    #[test]
    fn test_known_crc32() {
        // "hello" → CRC32 = 0x3610A686
        assert_eq!(crc32(b"hello"), 0x3610A686);
    }
}
