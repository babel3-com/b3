//! E2E encrypted channel wrapper for daemon ↔ browser communication.
//!
//! Wraps `ReliableChannel` with ECDH key exchange + AES-256-GCM encryption.
//! The encryption layer is transparent to `ReliableChannel` internals.
//!
//! # Protocol (daemon / server side)
//!
//! 1. Daemon registers with EC2, sends long-term public key in first frame.
//! 2. Browser fetches pubkey via `/api/daemon-key?agent_id=X` (auth-gated, no cache).
//! 3. Browser generates ephemeral keypair, computes ECDH shared secret.
//! 4. Browser connects WS, sends first plaintext frame:
//!    `{"type":"handshake","pubkey":"<base64 x25519 ephemeral pub>"}`
//! 5. Daemon computes ECDH → HKDF → derives two AES-256-GCM keys (one per direction).
//! 6. Daemon responds (plaintext): `{"type":"handshake_ok"}`
//! 7. All subsequent frames are encrypted wire format: `[12-byte nonce][ciphertext+tag]`
//!
//! # Key derivation
//!
//! - IKM: x25519 shared secret (32 bytes)
//! - Salt: `b"b3-daemon-proxy"` (fixed — acceptable given x25519 output entropy)
//! - Info: `b"b3-daemon-proxy-v1"`
//! - Output: 64 bytes
//!   - `[0..32]`  = browser send key  = daemon recv key
//!   - `[32..64]` = daemon send key   = browser recv key
//!
//! # Wire format (encrypted frames)
//!
//! ```text
//! [12-byte nonce (4 zero bytes | 8-byte u64 LE counter)][ciphertext + 16-byte AES-GCM tag]
//! ```
//!
//! Handshake frames are plaintext JSON — they arrive BEFORE the cipher is established.

use std::sync::{Arc, Mutex};

use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use aes_gcm::aead::Aead;
use base64::Engine as _;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::{Config, ReceivedMessage, ReliableChannel};
use crate::frame::Priority;

const HKDF_SALT: &[u8] = b"b3-daemon-proxy";
const HKDF_INFO: &[u8] = b"b3-daemon-proxy-v1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// State of the encrypted channel.
#[derive(Debug, PartialEq, Eq)]
enum EncryptedChannelState {
    Handshaking,
    Ready,
}

/// Shared send state — held inside the inner ReliableChannel's transport_tx closure.
struct SendState {
    cipher: Option<Aes256Gcm>,
    counter: u64,
}

/// E2E encrypted channel (daemon / server side).
///
/// Wraps `ReliableChannel`. The caller feeds raw bytes from the WS connection
/// into `receive()`. The channel handles handshake, key derivation, and
/// transparent encrypt/decrypt. Application code only sees plaintext via
/// `ReceivedMessage`.
///
/// # Usage
///
/// ```ignore
/// let ec = EncryptedChannel::new(config, long_term_secret, move |bytes| {
///     ws_sender.send(bytes.to_vec());
/// });
/// let pubkey = ec.public_key(); // register this with EC2
///
/// // On WS message received:
/// for msg in ec.receive(&bytes) {
///     // msg.payload is the decrypted application payload
/// }
///
/// // When READY:
/// ec.send(b"hello", Priority::Critical);
/// ec.tick(); // call periodically for ACK heartbeat
/// ```
pub struct EncryptedChannel {
    state: EncryptedChannelState,
    long_term_secret: StaticSecret,
    long_term_pub: PublicKey,
    inner: ReliableChannel,
    /// Raw send — used for plaintext handshake frames and passed into inner's transport_tx.
    raw_tx: Arc<dyn Fn(&[u8]) + Send + Sync>,
    /// Shared state between self and inner's transport_tx closure.
    send_state: Arc<Mutex<SendState>>,
    recv_cipher: Option<Aes256Gcm>,
    /// Optional version string included in the plaintext handshake_ok frame.
    version: Option<String>,
}

impl EncryptedChannel {
    /// Create a new `EncryptedChannel` in `Handshaking` state.
    ///
    /// `raw_tx` is the underlying WS send function — called for all outbound bytes
    /// (both plaintext handshake frames and encrypted reliable frames).
    pub fn new(
        config: Config,
        long_term_secret: StaticSecret,
        raw_tx: impl Fn(&[u8]) + Send + Sync + 'static,
    ) -> Self {
        let long_term_pub = PublicKey::from(&long_term_secret);
        let raw_tx: Arc<dyn Fn(&[u8]) + Send + Sync> = Arc::new(raw_tx);

        let send_state = Arc::new(Mutex::new(SendState { cipher: None, counter: 0 }));

        let send_state_inner = send_state.clone();
        let raw_tx_inner = raw_tx.clone();

        let inner = ReliableChannel::new(config, move |data: &[u8]| {
            let mut st = send_state_inner.lock().unwrap();
            if st.cipher.is_some() {
                st.counter += 1;
                let nonce = counter_nonce(st.counter);
                match st.cipher.as_ref().unwrap().encrypt(&nonce, data) {
                    Ok(ciphertext) => {
                        // Wire format: [12-byte nonce][ciphertext+tag]
                        let mut wire = Vec::with_capacity(NONCE_LEN + ciphertext.len());
                        wire.extend_from_slice(nonce.as_slice());
                        wire.extend_from_slice(&ciphertext);
                        raw_tx_inner(&wire);
                    }
                    Err(e) => {
                        tracing::error!("AES-GCM encrypt failed: {e}");
                    }
                }
            } else {
                // READY not yet established — drop and log.
                // In practice, send() checks is_ready() so this path should not occur.
                tracing::warn!("EncryptedChannel: dropping frame — cipher not ready");
            }
        });

        Self {
            state: EncryptedChannelState::Handshaking,
            long_term_secret,
            long_term_pub,
            inner,
            raw_tx,
            send_state,
            recv_cipher: None,
            version: None,
        }
    }

    /// Set the daemon version included in the plaintext handshake_ok frame.
    pub fn set_version(&mut self, v: &str) {
        self.version = Some(v.to_owned());
    }

    /// Daemon's long-term public key (base64). Register this with EC2 at startup.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        *self.long_term_pub.as_bytes()
    }

    /// Daemon's long-term public key as base64 string (for JSON registration).
    pub fn public_key_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.long_term_pub.as_bytes())
    }

    /// Feed bytes from the WS transport. Returns decoded application messages.
    ///
    /// In `Handshaking` state: looks for `{"type":"handshake","pubkey":"..."}`.
    ///   On success: derives keys, sends `{"type":"handshake_ok"}`, transitions to `Ready`.
    ///   On failure: logs error, stays in `Handshaking`.
    ///
    /// In `Ready` state: decrypts the wire-format frame, feeds to inner `ReliableChannel`.
    pub fn receive(&mut self, data: &[u8]) -> Vec<ReceivedMessage> {
        match self.state {
            EncryptedChannelState::Handshaking => {
                self.handle_handshake(data);
                Vec::new()
            }
            EncryptedChannelState::Ready => {
                match self.decrypt_frame(data) {
                    Some(plaintext) => self.inner.receive(&plaintext),
                    None => Vec::new(),
                }
            }
        }
    }

    /// Send an application payload. Panics if called before `Ready`.
    pub fn send(&mut self, payload: &[u8], priority: Priority) {
        assert!(self.is_ready(), "EncryptedChannel::send called before handshake complete");
        self.inner.send(payload, priority);
    }

    /// Returns true once the handshake is complete and application frames can flow.
    pub fn is_ready(&self) -> bool {
        self.state == EncryptedChannelState::Ready
    }

    /// Periodic tick — forward to inner channel for ACK heartbeat.
    pub fn tick(&mut self) {
        self.inner.tick();
    }

    /// Reset for a new browser session. Call when the previous browser disconnects.
    ///
    /// - Swaps the underlying transport closure (new raw_tx for the same WS connection).
    /// - Resets the inner `ReliableChannel` seq counters (`inner.reset()`).
    ///   Without this, the new browser's frames (seq=1..N) would be treated as
    ///   duplicates of the prior session and silently dropped.
    /// - Clears cipher state and returns to `Handshaking`.
    pub fn reset_for_reconnect(&mut self, raw_tx: impl Fn(&[u8]) + Send + Sync + 'static) {
        let raw_tx: Arc<dyn Fn(&[u8]) + Send + Sync> = Arc::new(raw_tx);

        let send_state = Arc::new(Mutex::new(SendState { cipher: None, counter: 0 }));

        let send_state_inner = send_state.clone();
        let raw_tx_inner = raw_tx.clone();

        self.inner.set_transport(move |data: &[u8]| {
            let mut st = send_state_inner.lock().unwrap();
            if st.cipher.is_some() {
                st.counter += 1;
                let nonce = counter_nonce(st.counter);
                match st.cipher.as_ref().unwrap().encrypt(&nonce, data) {
                    Ok(ciphertext) => {
                        let mut wire = Vec::with_capacity(NONCE_LEN + ciphertext.len());
                        wire.extend_from_slice(nonce.as_slice());
                        wire.extend_from_slice(&ciphertext);
                        raw_tx_inner(&wire);
                    }
                    Err(e) => tracing::error!("AES-GCM encrypt failed on reconnect: {e}"),
                }
            }
        });

        // Reset inner ReliableChannel seq state — new browser starts at seq=1.
        // Without this reset, frames seq=1..last_contiguous are treated as duplicates.
        self.inner.reset();

        self.send_state = send_state;
        self.raw_tx = raw_tx;
        self.recv_cipher = None;
        self.state = EncryptedChannelState::Handshaking;
    }

    // ── Private ──────────────────────────────────────────────────────

    fn handle_handshake(&mut self, data: &[u8]) {
        // Expect plaintext JSON: {"type":"handshake","pubkey":"<base64>"}
        let frame: serde_json::Value = match serde_json::from_slice(data) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("EncryptedChannel: handshake frame not valid JSON: {e}");
                return;
            }
        };

        if frame.get("type").and_then(|v| v.as_str()) != Some("handshake") {
            tracing::warn!("EncryptedChannel: expected handshake frame, got type={:?}",
                frame.get("type"));
            return;
        }

        let pubkey_b64 = match frame.get("pubkey").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                tracing::warn!("EncryptedChannel: handshake missing pubkey field");
                return;
            }
        };

        let pubkey_bytes = match base64::engine::general_purpose::STANDARD.decode(pubkey_b64) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("EncryptedChannel: invalid base64 pubkey: {e}");
                return;
            }
        };

        if pubkey_bytes.len() != 32 {
            tracing::warn!("EncryptedChannel: pubkey must be 32 bytes, got {}", pubkey_bytes.len());
            return;
        }

        let mut raw_bytes = [0u8; 32];
        raw_bytes.copy_from_slice(&pubkey_bytes);
        let browser_pub = PublicKey::from(raw_bytes);

        // ECDH: shared_secret = DH(daemon_long_term_priv, browser_ephemeral_pub)
        let shared = self.long_term_secret.diffie_hellman(&browser_pub);

        // HKDF-SHA256 → 64 bytes
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), shared.as_bytes());
        let mut okm = [0u8; 64];
        if hk.expand(HKDF_INFO, &mut okm).is_err() {
            tracing::error!("EncryptedChannel: HKDF expand failed");
            return;
        }

        // bytes [0..32]  = browser send key = daemon recv key
        // bytes [32..64] = daemon send key  = browser recv key
        let recv_key = Key::<Aes256Gcm>::from_slice(&okm[0..32]);
        let send_key = Key::<Aes256Gcm>::from_slice(&okm[32..64]);

        self.recv_cipher = Some(Aes256Gcm::new(recv_key));

        {
            let mut st = self.send_state.lock().unwrap();
            st.cipher = Some(Aes256Gcm::new(send_key));
            st.counter = 0;
        }

        okm.zeroize();

        self.state = EncryptedChannelState::Ready;

        // Send plaintext handshake_ok — last plaintext frame
        let ack = match &self.version {
            Some(v) => format!(r#"{{"type":"handshake_ok","version":"{v}"}}"#),
            None    => r#"{"type":"handshake_ok"}"#.to_owned(),
        };
        (self.raw_tx)(ack.as_bytes());

        tracing::info!("EncryptedChannel: handshake complete — READY");
    }

    fn decrypt_frame(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < NONCE_LEN + TAG_LEN {
            tracing::warn!("EncryptedChannel: frame too short to decrypt ({} bytes)", data.len());
            return None;
        }

        let nonce = Nonce::from_slice(&data[0..NONCE_LEN]);
        let ciphertext = &data[NONCE_LEN..];

        let cipher = self.recv_cipher.as_ref()?;
        match cipher.decrypt(nonce, ciphertext) {
            Ok(plaintext) => Some(plaintext),
            Err(e) => {
                tracing::warn!("EncryptedChannel: AES-GCM decrypt failed: {e}");
                None
            }
        }
    }
}

/// Build a 12-byte AES-GCM nonce from a u64 counter.
/// Layout: [0x00, 0x00, 0x00, 0x00, counter[0..8] LE]
fn counter_nonce(counter: u64) -> Nonce<aes_gcm::aead::consts::U12> {
    let mut bytes = [0u8; 12];
    bytes[4..12].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn make_channel_pair() -> (EncryptedChannel, EncryptedChannel) {
        let daemon_secret = StaticSecret::random_from_rng(OsRng);
        let browser_secret = StaticSecret::random_from_rng(OsRng);

        let daemon_pub = PublicKey::from(&daemon_secret);
        let browser_pub = PublicKey::from(&browser_secret);

        // Wire buffers
        let daemon_to_browser: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let browser_to_daemon: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        let d2b = daemon_to_browser.clone();
        let b2d = browser_to_daemon.clone();

        let daemon = EncryptedChannel::new(Config::default(), daemon_secret, move |data: &[u8]| {
            d2b.lock().unwrap().push(data.to_vec());
        });

        // Browser side: use a mirrored EncryptedChannel with the browser's long-term key
        // (In production the browser uses Web Crypto; here we simulate with a second Rust channel)
        let browser = EncryptedChannel::new(Config::default(), browser_secret, move |data: &[u8]| {
            b2d.lock().unwrap().push(data.to_vec());
        });

        let _ = (daemon_pub, browser_pub); // referenced for documentation
        (daemon, browser)
    }

    #[test]
    fn test_handshake_transitions_to_ready() {
        let daemon_secret = StaticSecret::random_from_rng(OsRng);
        let browser_secret = StaticSecret::random_from_rng(OsRng);

        let daemon_pub = PublicKey::from(&daemon_secret);
        let browser_pub = PublicKey::from(&browser_secret);

        let daemon_outbox: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let d_out = daemon_outbox.clone();

        let mut daemon = EncryptedChannel::new(Config::default(), daemon_secret, move |data: &[u8]| {
            d_out.lock().unwrap().push(data.to_vec());
        });

        assert!(!daemon.is_ready());

        // Simulate browser sending handshake
        let pubkey_b64 = base64::engine::general_purpose::STANDARD.encode(browser_pub.as_bytes());
        let handshake = format!(r#"{{"type":"handshake","pubkey":"{pubkey_b64}"}}"#);
        let msgs = daemon.receive(handshake.as_bytes());

        assert!(msgs.is_empty()); // handshake returns no app messages
        assert!(daemon.is_ready());

        // Daemon should have sent handshake_ok
        let outbox = daemon_outbox.lock().unwrap();
        assert_eq!(outbox.len(), 1);
        let ack: serde_json::Value = serde_json::from_slice(&outbox[0]).unwrap();
        assert_eq!(ack["type"], "handshake_ok");

        let _ = (daemon_pub, browser_pub);
    }

    #[test]
    fn test_encrypt_decrypt_round_trip() {
        // Perform handshake manually and verify encrypt/decrypt is symmetric
        let daemon_secret = StaticSecret::random_from_rng(OsRng);
        let browser_secret = StaticSecret::random_from_rng(OsRng);
        let daemon_pub = PublicKey::from(&daemon_secret);
        let browser_pub = PublicKey::from(&browser_secret);

        let daemon_outbox: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let browser_outbox: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        let d_out = daemon_outbox.clone();
        let b_out = browser_outbox.clone();

        let mut daemon = EncryptedChannel::new(Config::default(), daemon_secret, move |data: &[u8]| {
            d_out.lock().unwrap().push(data.to_vec());
        });

        // Simulate browser-side ECDH + handshake manually
        let shared = browser_secret.diffie_hellman(&daemon_pub);
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), shared.as_bytes());
        let mut okm = [0u8; 64];
        hk.expand(HKDF_INFO, &mut okm).unwrap();
        // Browser send key = daemon recv key = okm[0..32]
        // Daemon send key = browser recv key = okm[32..64]
        let browser_send_cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&okm[0..32]));
        let browser_recv_cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&okm[32..64]));

        // 1. Browser sends handshake
        let pubkey_b64 = base64::engine::general_purpose::STANDARD.encode(browser_pub.as_bytes());
        let handshake = format!(r#"{{"type":"handshake","pubkey":"{pubkey_b64}"}}"#);
        daemon.receive(handshake.as_bytes());
        assert!(daemon.is_ready());

        // 2. Daemon sends encrypted app message → should arrive in browser's recv
        daemon.send(b"hello from daemon", Priority::Critical);

        let daemon_frames = daemon_outbox.lock().unwrap().clone();
        // Frame 0 is handshake_ok (plaintext), frame 1 is the encrypted app message
        assert!(daemon_frames.len() >= 2);

        // Skip handshake_ok (index 0), decrypt the reliable frame (index 1+)
        let mut decrypted_msgs = Vec::new();
        for wire_frame in &daemon_frames[1..] {
            // wire_frame = [12-byte nonce][ciphertext+tag]
            assert!(wire_frame.len() > NONCE_LEN + TAG_LEN);
            let nonce = Nonce::from_slice(&wire_frame[0..NONCE_LEN]);
            let decrypted = browser_recv_cipher.decrypt(nonce, &wire_frame[NONCE_LEN..]).unwrap();
            decrypted_msgs.push(decrypted);
        }

        assert!(!decrypted_msgs.is_empty(), "Should have at least one encrypted frame");

        // 3. Browser sends encrypted message to daemon (simulate)
        let plaintext = b"hello from browser";
        let nonce = counter_nonce(1);
        let mut wire = Vec::new();
        wire.extend_from_slice(nonce.as_slice());
        let ct = browser_send_cipher.encrypt(&nonce, plaintext.as_slice()).unwrap();
        wire.extend_from_slice(&ct);

        // Daemon decrypts and feeds to inner reliable channel
        // (the payload will be decoded as a reliable frame — just check no panic)
        let _ = daemon.receive(&wire); // may return empty if not a valid reliable frame

        let _ = (b_out, browser_outbox);
    }

    #[test]
    fn test_invalid_handshake_stays_handshaking() {
        let daemon_secret = StaticSecret::random_from_rng(OsRng);
        let outbox: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let out = outbox.clone();

        let mut daemon = EncryptedChannel::new(Config::default(), daemon_secret, move |data: &[u8]| {
            out.lock().unwrap().push(data.to_vec());
        });

        // Garbage data
        daemon.receive(b"garbage not json");
        assert!(!daemon.is_ready());

        // Valid JSON but wrong type
        daemon.receive(b"{\"type\":\"submit\",\"pubkey\":\"abc\"}");
        assert!(!daemon.is_ready());

        // Missing pubkey
        daemon.receive(b"{\"type\":\"handshake\"}");
        assert!(!daemon.is_ready());

        assert!(outbox.lock().unwrap().is_empty()); // no frames sent
    }
}
