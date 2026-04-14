//! Daemon ↔ client wire protocol over Unix socket.
//!
//! Length-prefixed messages with a 1-byte type tag:
//!
//!   [len: u32 LE] [type: u8] [payload: len-1 bytes]
//!
//! Message types:
//!   Client → Daemon:
//!     0x01  Input     — raw bytes to write to PTY stdin
//!     0x02  Resize    — [rows: u16 LE] [cols: u16 LE]
//!     0x03  Detach    — client disconnecting (no payload)
//!     0x04  Stop      — request daemon shutdown (no payload)
//!     0x05  Status    — request status info (no payload)
//!
//!   Daemon → Client:
//!     0x81  Output    — raw bytes from PTY stdout
//!     0x82  Exited    — PTY child exited [exit_code: i32 LE]
//!     0x83  StatusResp — JSON status info
//!     0x84  Welcome   — [rows: u16 LE] [cols: u16 LE] — sent on attach

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Client → Daemon
pub const MSG_INPUT: u8 = 0x01;
pub const MSG_RESIZE: u8 = 0x02;
pub const MSG_DETACH: u8 = 0x03;
pub const MSG_STOP: u8 = 0x04;
pub const MSG_STATUS: u8 = 0x05;

// Daemon → Client
pub const MSG_OUTPUT: u8 = 0x81;
pub const MSG_EXITED: u8 = 0x82;
pub const MSG_STATUS_RESP: u8 = 0x83;
pub const MSG_WELCOME: u8 = 0x84;

/// A parsed message from the wire.
#[derive(Debug, Clone)]
pub enum Message {
    // Client → Daemon
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Detach,
    Stop,
    Status,

    // Daemon → Client
    Output(Vec<u8>),
    Exited(i32),
    StatusResp(String),
    Welcome { rows: u16, cols: u16 },
}

/// Read one message from an async reader.
pub async fn read_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> io::Result<Message> {
    // Read length (u32 LE)
    let len = reader.read_u32_le().await?;
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty message"));
    }
    if len > 4 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }

    // Read type tag
    let tag = reader.read_u8().await?;
    let payload_len = (len - 1) as usize;

    // Read payload
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload).await?;
    }

    match tag {
        MSG_INPUT => Ok(Message::Input(payload)),
        MSG_RESIZE => {
            if payload.len() != 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad resize"));
            }
            let rows = u16::from_le_bytes([payload[0], payload[1]]);
            let cols = u16::from_le_bytes([payload[2], payload[3]]);
            Ok(Message::Resize { rows, cols })
        }
        MSG_DETACH => Ok(Message::Detach),
        MSG_STOP => Ok(Message::Stop),
        MSG_STATUS => Ok(Message::Status),
        MSG_OUTPUT => Ok(Message::Output(payload)),
        MSG_EXITED => {
            if payload.len() != 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad exited"));
            }
            let code = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            Ok(Message::Exited(code))
        }
        MSG_STATUS_RESP => {
            let s = String::from_utf8(payload)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))?;
            Ok(Message::StatusResp(s))
        }
        MSG_WELCOME => {
            if payload.len() != 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad welcome"));
            }
            let rows = u16::from_le_bytes([payload[0], payload[1]]);
            let cols = u16::from_le_bytes([payload[2], payload[3]]);
            Ok(Message::Welcome { rows, cols })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown message type: {tag:#x}"),
        )),
    }
}

/// Write one message to an async writer.
pub async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> io::Result<()> {
    let (tag, payload): (u8, Vec<u8>) = match msg {
        Message::Input(data) => (MSG_INPUT, data.clone()),
        Message::Resize { rows, cols } => {
            let mut p = Vec::with_capacity(4);
            p.extend_from_slice(&rows.to_le_bytes());
            p.extend_from_slice(&cols.to_le_bytes());
            (MSG_RESIZE, p)
        }
        Message::Detach => (MSG_DETACH, vec![]),
        Message::Stop => (MSG_STOP, vec![]),
        Message::Status => (MSG_STATUS, vec![]),
        Message::Output(data) => (MSG_OUTPUT, data.clone()),
        Message::Exited(code) => (MSG_EXITED, code.to_le_bytes().to_vec()),
        Message::StatusResp(s) => (MSG_STATUS_RESP, s.as_bytes().to_vec()),
        Message::Welcome { rows, cols } => {
            let mut p = Vec::with_capacity(4);
            p.extend_from_slice(&rows.to_le_bytes());
            p.extend_from_slice(&cols.to_le_bytes());
            (MSG_WELCOME, p)
        }
    };

    let len = (1 + payload.len()) as u32;
    writer.write_u32_le(len).await?;
    writer.write_u8(tag).await?;
    if !payload.is_empty() {
        writer.write_all(&payload).await?;
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_roundtrip_input() {
        let msg = Message::Input(b"hello world".to_vec());
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::Input(data) => assert_eq!(data, b"hello world"),
            other => panic!("Expected Input, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_roundtrip_resize() {
        let msg = Message::Resize { rows: 40, cols: 120 };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::Resize { rows, cols } => {
                assert_eq!(rows, 40);
                assert_eq!(cols, 120);
            }
            other => panic!("Expected Resize, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_roundtrip_output() {
        let msg = Message::Output(b"\x1b[32mgreen text\x1b[0m".to_vec());
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::Output(data) => assert_eq!(data, b"\x1b[32mgreen text\x1b[0m"),
            other => panic!("Expected Output, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_roundtrip_welcome() {
        let msg = Message::Welcome { rows: 24, cols: 80 };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::Welcome { rows, cols } => {
                assert_eq!(rows, 24);
                assert_eq!(cols, 80);
            }
            other => panic!("Expected Welcome, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_roundtrip_detach() {
        let msg = Message::Detach;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        assert!(matches!(decoded, Message::Detach));
    }

    #[tokio::test]
    async fn test_roundtrip_stop() {
        let msg = Message::Stop;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        assert!(matches!(decoded, Message::Stop));
    }

    #[tokio::test]
    async fn test_roundtrip_exited() {
        let msg = Message::Exited(42);
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::Exited(code) => assert_eq!(code, 42),
            other => panic!("Expected Exited, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_roundtrip_status_resp() {
        let msg = Message::StatusResp(r#"{"running":true}"#.to_string());
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::StatusResp(s) => assert_eq!(s, r#"{"running":true}"#),
            other => panic!("Expected StatusResp, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_multiple_messages_in_stream() {
        let messages = vec![
            Message::Input(b"key1".to_vec()),
            Message::Output(b"output1".to_vec()),
            Message::Resize { rows: 50, cols: 100 },
            Message::Detach,
        ];

        let mut buf = Vec::new();
        for msg in &messages {
            write_message(&mut buf, msg).await.unwrap();
        }

        let mut cursor = io::Cursor::new(buf);
        let m1 = read_message(&mut cursor).await.unwrap();
        assert!(matches!(m1, Message::Input(_)));
        let m2 = read_message(&mut cursor).await.unwrap();
        assert!(matches!(m2, Message::Output(_)));
        let m3 = read_message(&mut cursor).await.unwrap();
        assert!(matches!(m3, Message::Resize { .. }));
        let m4 = read_message(&mut cursor).await.unwrap();
        assert!(matches!(m4, Message::Detach));
    }

    #[tokio::test]
    async fn test_empty_input() {
        let msg = Message::Input(vec![]);
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = io::Cursor::new(buf);
        let decoded = read_message(&mut cursor).await.unwrap();
        match decoded {
            Message::Input(data) => assert!(data.is_empty()),
            other => panic!("Expected Input, got: {other:?}"),
        }
    }
}
