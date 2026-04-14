//! SDP and ICE signaling types for WebRTC negotiation.
//!
//! These types are exchanged via HTTP through the EC2 signaling server.
//! Browser POSTs offer → EC2 relays via SSE → daemon/GPU answers.

use serde::{Deserialize, Serialize};

/// ICE server configuration (STUN or TURN).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Signaling message exchanged between peers via EC2.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    /// SDP offer from browser to daemon/GPU.
    #[serde(rename = "offer")]
    Offer {
        session_id: String,
        sdp: String,
        target: SignalingTarget,
    },

    /// SDP answer from daemon/GPU back to browser.
    #[serde(rename = "answer")]
    Answer {
        session_id: String,
        sdp: String,
    },

    /// ICE candidate (trickle ICE, both directions).
    #[serde(rename = "ice")]
    Ice {
        session_id: String,
        candidate: String,
        mid: String,
    },
}

/// Which peer should handle the offer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SignalingTarget {
    Daemon,
    GpuWorker,
}

/// Request body for POST /api/agents/:id/webrtc/offer.
#[derive(Debug, Serialize, Deserialize)]
pub struct OfferRequest {
    pub sdp: String,
    pub target: SignalingTarget,
}

/// Response body from POST /api/agents/:id/webrtc/offer.
/// EC2 blocks until the target answers (10s timeout).
#[derive(Debug, Serialize, Deserialize)]
pub struct OfferResponse {
    pub sdp: String,
    pub session_id: String,
}

/// Request body for POST /api/agents/:id/webrtc/ice.
#[derive(Debug, Serialize, Deserialize)]
pub struct IceRequest {
    pub session_id: String,
    pub candidate: String,
    pub mid: String,
    pub target: SignalingTarget,
}

/// Response body from GET /api/webrtc/turn-credentials.
#[derive(Debug, Serialize, Deserialize)]
pub struct TurnCredentials {
    pub ice_servers: Vec<IceServer>,
    /// Seconds until credentials expire.
    pub ttl: u64,
}

/// Generate time-limited TURN credentials using the TURN REST API (RFC 7635).
///
/// `shared_secret` is the coturn static-auth-secret.
/// Returns (username, credential) valid for `ttl_secs`.
pub fn generate_turn_credentials(shared_secret: &str, ttl_secs: u64) -> (String, String) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + ttl_secs;

    // TURN REST API: username = "expiry:random"
    let username = format!("{expiry}:b3");

    // credential = base64(HMAC-SHA1(shared_secret, username))
    let credential = hmac_sha1_base64(shared_secret.as_bytes(), username.as_bytes());

    (username, credential)
}

/// HMAC-SHA1 → base64 (for TURN REST API credential generation).
fn hmac_sha1_base64(key: &[u8], data: &[u8]) -> String {
    // RFC 2104 HMAC-SHA1 implementation.
    use std::io::Write;

    const BLOCK_SIZE: usize = 64;
    const HASH_SIZE: usize = 20;

    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        // Hash key if > block size.
        let hash = sha1(key);
        key_block[..HASH_SIZE].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    // Inner hash: SHA1((key XOR ipad) || data)
    let mut inner = Vec::with_capacity(BLOCK_SIZE + data.len());
    for &b in &key_block {
        inner.push(b ^ 0x36);
    }
    inner.write_all(data).unwrap();
    let inner_hash = sha1(&inner);

    // Outer hash: SHA1((key XOR opad) || inner_hash)
    let mut outer = Vec::with_capacity(BLOCK_SIZE + HASH_SIZE);
    for &b in &key_block {
        outer.push(b ^ 0x5c);
    }
    outer.extend_from_slice(&inner_hash);
    let result = sha1(&outer);

    base64_encode(&result)
}

/// Minimal SHA-1 (RFC 3174). Only used for HMAC-SHA1 in TURN credential generation.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;

    // Pad message: append 0x80, then zeros, then 64-bit big-endian length.
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);

        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut result = [0u8; 20];
    result[0..4].copy_from_slice(&h0.to_be_bytes());
    result[4..8].copy_from_slice(&h1.to_be_bytes());
    result[8..12].copy_from_slice(&h2.to_be_bytes());
    result[12..16].copy_from_slice(&h3.to_be_bytes());
    result[16..20].copy_from_slice(&h4.to_be_bytes());
    result
}

/// Base64 encode (standard alphabet, with padding).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_credentials_format() {
        let (username, credential) = generate_turn_credentials("test-secret", 3600);
        assert!(username.contains(':'));
        assert!(!credential.is_empty());
        // Credential should be base64 (ends with = padding or alphanumeric).
        assert!(credential.chars().all(|c| c.is_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn signaling_message_roundtrip() {
        let msg = SignalingMessage::Offer {
            session_id: "s1".into(),
            sdp: "v=0\r\n".into(),
            target: SignalingTarget::Daemon,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalingMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalingMessage::Offer { session_id, sdp, target } => {
                assert_eq!(session_id, "s1");
                assert_eq!(sdp, "v=0\r\n");
                assert_eq!(target, SignalingTarget::Daemon);
            }
            _ => panic!("wrong variant"),
        }
    }
}
