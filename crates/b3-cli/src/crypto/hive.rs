//! End-to-end encrypted hive messaging.
//!
//! Uses X25519 Diffie-Hellman for key agreement and ChaCha20-Poly1305 for
//! authenticated encryption. Reuses the agent's existing WireGuard keypair.
//!
//! Two modes:
//! - **Pairwise (DM):** ECDH between sender and recipient static keys.
//!   Optional ephemeral keys for forward secrecy.
//! - **Room (group):** Symmetric room key distributed via pairwise DH channels.
//!
//! See docs/marketing-strategy/e2e-encrypted-hive-design.md for full protocol.

use base64::Engine as _;
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

/// HKDF salt shared across all hive encryption.
const HKDF_SALT: &[u8] = b"b3-hive-v1";

/// Wire format for an encrypted message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedEnvelope {
    pub encrypted: bool,
    pub version: u8,
    pub key_type: KeyType,
    /// Only present for ephemeral key mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_pub: Option<String>,
    /// 12-byte nonce, base64-encoded.
    pub nonce: String,
    /// Ciphertext, base64-encoded.
    pub content: String,
}

/// Key exchange mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    Static,
    Ephemeral,
}

/// Encrypted room key for one member.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedRoomKey {
    pub nonce: String,
    pub encrypted_key: String,
}

// ── Key Derivation ──────────────────────────────────────────────────

/// Derive an encryption key from a DH shared secret.
///
/// Uses HKDF-SHA256 with domain-separated `info` to prevent key reuse
/// across different contexts (DM static, DM ephemeral, room key distribution).
fn derive_key(shared_secret: &[u8; 32], info: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), shared_secret);
    let mut key = [0u8; 32];
    hk.expand(info.as_bytes(), &mut key)
        .expect("32-byte output is always valid for HKDF-SHA256");
    key
}

/// Compute domain-separated info string for a DM.
/// Sorts agent IDs lexicographically so both sides derive the same key.
fn dm_info(a_id: &str, b_id: &str, ephemeral: bool) -> String {
    let (first, second) = if a_id <= b_id {
        (a_id, b_id)
    } else {
        (b_id, a_id)
    };
    if ephemeral {
        format!("dm-ephemeral:{first}:{second}")
    } else {
        format!("dm:{first}:{second}")
    }
}

// ── Pairwise (Direct Message) Encryption ────────────────────────────

/// Encrypt a direct message using static key DH.
///
/// `aad` authenticates metadata (from_agent, target, timestamp) without
/// encrypting it, so EC2 can still route but tampering is detected.
pub fn encrypt_dm(
    plaintext: &[u8],
    my_key: &StaticSecret,
    their_pub: &PublicKey,
    my_id: &str,
    their_id: &str,
    aad: &[u8],
) -> EncryptedEnvelope {
    let shared = my_key.diffie_hellman(their_pub);
    let info = dm_info(my_id, their_id, false);
    let enc_key = derive_key(shared.as_bytes(), &info);
    let (nonce_bytes, ciphertext) = encrypt_aead(&enc_key, plaintext, aad);

    EncryptedEnvelope {
        encrypted: true,
        version: 1,
        key_type: KeyType::Static,
        ephemeral_pub: None,
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        content: base64::engine::general_purpose::STANDARD.encode(ciphertext),
    }
}

/// Encrypt a direct message with an ephemeral key for forward secrecy.
///
/// Returns the envelope AND the ephemeral public key (already inside the
/// envelope). The caller MUST NOT retain the ephemeral private key.
pub fn encrypt_dm_ephemeral(
    plaintext: &[u8],
    their_pub: &PublicKey,
    my_id: &str,
    their_id: &str,
    aad: &[u8],
) -> EncryptedEnvelope {
    use rand::rngs::OsRng;

    let ephemeral_secret = StaticSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);

    let shared = ephemeral_secret.diffie_hellman(their_pub);
    let info = dm_info(my_id, their_id, true);
    let enc_key = derive_key(shared.as_bytes(), &info);
    let (nonce_bytes, ciphertext) = encrypt_aead(&enc_key, plaintext, aad);

    // ephemeral_secret is dropped here — forward secrecy achieved.
    EncryptedEnvelope {
        encrypted: true,
        version: 1,
        key_type: KeyType::Ephemeral,
        ephemeral_pub: Some(
            base64::engine::general_purpose::STANDARD.encode(ephemeral_public.as_bytes()),
        ),
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        content: base64::engine::general_purpose::STANDARD.encode(ciphertext),
    }
}

/// Decrypt a direct message envelope.
///
/// Handles both static and ephemeral key types.
pub fn decrypt_dm(
    envelope: &EncryptedEnvelope,
    my_key: &StaticSecret,
    sender_pub: &PublicKey,
    my_id: &str,
    sender_id: &str,
    aad: &[u8],
) -> Result<Vec<u8>, DecryptError> {
    let b64 = base64::engine::general_purpose::STANDARD;

    let nonce_bytes = b64.decode(&envelope.nonce).map_err(|_| DecryptError::InvalidNonce)?;
    let ciphertext = b64.decode(&envelope.content).map_err(|_| DecryptError::InvalidContent)?;

    let enc_key = match envelope.key_type {
        KeyType::Static => {
            let shared = my_key.diffie_hellman(sender_pub);
            let info = dm_info(my_id, sender_id, false);
            derive_key(shared.as_bytes(), &info)
        }
        KeyType::Ephemeral => {
            let eph_pub_b64 = envelope
                .ephemeral_pub
                .as_ref()
                .ok_or(DecryptError::MissingEphemeralKey)?;
            let eph_pub_bytes: [u8; 32] = b64
                .decode(eph_pub_b64)
                .map_err(|_| DecryptError::InvalidEphemeralKey)?
                .try_into()
                .map_err(|_| DecryptError::InvalidEphemeralKey)?;
            let eph_pub = PublicKey::from(eph_pub_bytes);
            let shared = my_key.diffie_hellman(&eph_pub);
            let info = dm_info(my_id, sender_id, true);
            derive_key(shared.as_bytes(), &info)
        }
    };

    decrypt_aead(&enc_key, &nonce_bytes, &ciphertext, aad)
}

// ── Room (Group) Encryption ─────────────────────────────────────────

/// Generate a random 256-bit symmetric room key.
pub fn generate_room_key() -> [u8; 32] {
    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

/// Encrypt a room key for a specific member using pairwise DH.
///
/// The `room_id` is included in the HKDF info string for domain separation,
/// ensuring distinct encryption keys per room even between the same agent pair.
pub fn encrypt_room_key(
    room_key: &[u8; 32],
    my_key: &StaticSecret,
    member_pub: &PublicKey,
    room_id: &str,
) -> EncryptedRoomKey {
    let shared = my_key.diffie_hellman(member_pub);
    let enc_key = derive_key(shared.as_bytes(), &format!("room-key:{room_id}"));
    let (nonce_bytes, ciphertext) = encrypt_aead(&enc_key, room_key, &[]);

    let b64 = base64::engine::general_purpose::STANDARD;
    EncryptedRoomKey {
        nonce: b64.encode(nonce_bytes),
        encrypted_key: b64.encode(ciphertext),
    }
}

/// Decrypt a room key received from the room creator.
///
/// # Caller Responsibility
///
/// The caller MUST verify that the message containing this encrypted room key
/// actually came from the room creator. This function authenticates the
/// cryptographic binding (DH with `creator_pub`) but cannot verify message
/// authorship — that's the transport layer's job. If an attacker can inject
/// a key_distribution message claiming to be the creator, they could distribute
/// a room key they control.
///
/// Verify `from_agent` matches the room's creator stored in room metadata
/// (`~/.b3/room-keys/{room_id}.meta`) before calling this function.
pub fn decrypt_room_key(
    encrypted: &EncryptedRoomKey,
    my_key: &StaticSecret,
    creator_pub: &PublicKey,
    room_id: &str,
) -> Result<[u8; 32], DecryptError> {
    let b64 = base64::engine::general_purpose::STANDARD;

    let nonce_bytes = b64.decode(&encrypted.nonce).map_err(|_| DecryptError::InvalidNonce)?;
    let ciphertext = b64
        .decode(&encrypted.encrypted_key)
        .map_err(|_| DecryptError::InvalidContent)?;

    let shared = my_key.diffie_hellman(creator_pub);
    let enc_key = derive_key(shared.as_bytes(), &format!("room-key:{room_id}"));

    let plaintext = decrypt_aead(&enc_key, &nonce_bytes, &ciphertext, &[])?;

    plaintext
        .try_into()
        .map_err(|_| DecryptError::InvalidRoomKey)
}

/// Encrypt a message for a room using the symmetric room key.
pub fn encrypt_room_message(plaintext: &[u8], room_key: &[u8; 32]) -> EncryptedEnvelope {
    let (nonce_bytes, ciphertext) = encrypt_aead(room_key, plaintext, &[]);

    let b64 = base64::engine::general_purpose::STANDARD;
    EncryptedEnvelope {
        encrypted: true,
        version: 1,
        key_type: KeyType::Static, // room messages use symmetric key, not DH
        ephemeral_pub: None,
        nonce: b64.encode(nonce_bytes),
        content: b64.encode(ciphertext),
    }
}

/// Decrypt a room message using the symmetric room key.
pub fn decrypt_room_message(
    envelope: &EncryptedEnvelope,
    room_key: &[u8; 32],
) -> Result<Vec<u8>, DecryptError> {
    let b64 = base64::engine::general_purpose::STANDARD;

    let nonce_bytes = b64.decode(&envelope.nonce).map_err(|_| DecryptError::InvalidNonce)?;
    let ciphertext = b64.decode(&envelope.content).map_err(|_| DecryptError::InvalidContent)?;

    decrypt_aead(room_key, &nonce_bytes, &ciphertext, &[])
}

// ── AEAD Helpers ────────────────────────────────────────────────────

/// Encrypt with ChaCha20-Poly1305, returning (nonce, ciphertext).
fn encrypt_aead(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> ([u8; 12], Vec<u8>) {
    use rand::RngCore;

    let cipher = ChaCha20Poly1305::new(key.into());
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("ChaCha20Poly1305 encryption should not fail");

    (nonce_bytes, ciphertext)
}

/// Decrypt with ChaCha20-Poly1305.
fn decrypt_aead(
    key: &[u8; 32],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, DecryptError> {
    let nonce: &[u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| DecryptError::InvalidNonce)?;

    let cipher = ChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| DecryptError::DecryptionFailed)
}

// ── Errors ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum DecryptError {
    #[error("invalid nonce")]
    InvalidNonce,
    #[error("invalid ciphertext content")]
    InvalidContent,
    #[error("missing ephemeral public key")]
    MissingEphemeralKey,
    #[error("invalid ephemeral public key")]
    InvalidEphemeralKey,
    #[error("decryption failed (wrong key, tampered data, or invalid AAD)")]
    DecryptionFailed,
    #[error("decrypted room key has wrong length")]
    InvalidRoomKey,
}

// ── TOFU Key Pinning ────────────────────────────────────────────────

/// Store a public key using Trust-On-First-Use (TOFU).
///
/// Returns Ok(true) if this is a new key (first contact),
/// Ok(false) if the key matches the cached one,
/// Err with the old key if there's a mismatch (potential MITM).
pub fn pin_public_key(agent_name: &str, public_key: &PublicKey) -> Result<bool, PublicKey> {
    let b64 = base64::engine::general_purpose::STANDARD;

    let keys_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".b3")
        .join("known-keys");
    let _ = std::fs::create_dir_all(&keys_dir);
    let key_file = keys_dir.join(format!("{agent_name}.pub"));

    if key_file.exists() {
        if let Ok(cached_b64) = std::fs::read_to_string(&key_file) {
            if let Ok(cached_bytes) = b64.decode(cached_b64.trim()) {
                if let Ok(cached_arr) = <[u8; 32]>::try_from(cached_bytes.as_slice()) {
                    let cached_pub = PublicKey::from(cached_arr);
                    if cached_pub.as_bytes() == public_key.as_bytes() {
                        return Ok(false); // Key matches
                    } else {
                        return Err(cached_pub); // MISMATCH
                    }
                }
            }
        }
    }

    // First contact — store the key
    let _ = std::fs::write(&key_file, b64.encode(public_key.as_bytes()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(true)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn test_keypair() -> (StaticSecret, PublicKey) {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        (secret, public)
    }

    #[test]
    fn dm_static_roundtrip() {
        let (a_secret, a_public) = test_keypair();
        let (b_secret, b_public) = test_keypair();
        let plaintext = b"Hello from agent A!";
        let aad = b"a:b:1234567890";

        let envelope = encrypt_dm(plaintext, &a_secret, &b_public, "a", "b", aad);
        assert!(envelope.encrypted);
        assert_eq!(envelope.key_type, KeyType::Static);
        assert!(envelope.ephemeral_pub.is_none());

        let decrypted = decrypt_dm(&envelope, &b_secret, &a_public, "b", "a", aad).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn dm_static_wrong_key_fails() {
        let (a_secret, _a_public) = test_keypair();
        let (_b_secret, b_public) = test_keypair();
        let (c_secret, _c_public) = test_keypair();
        let aad = b"a:b:1234567890";

        let envelope = encrypt_dm(b"secret", &a_secret, &b_public, "a", "b", aad);

        // C tries to decrypt — should fail
        let result = decrypt_dm(&envelope, &c_secret, &_a_public, "c", "a", aad);
        assert!(result.is_err());
    }

    #[test]
    fn dm_static_wrong_aad_fails() {
        let (a_secret, a_public) = test_keypair();
        let (b_secret, b_public) = test_keypair();
        let aad = b"a:b:1234567890";
        let wrong_aad = b"a:b:9999999999";

        let envelope = encrypt_dm(b"secret", &a_secret, &b_public, "a", "b", aad);
        let result = decrypt_dm(&envelope, &b_secret, &a_public, "b", "a", wrong_aad);
        assert!(result.is_err());
    }

    #[test]
    fn dm_ephemeral_roundtrip() {
        let (b_secret, b_public) = test_keypair();
        let plaintext = b"Forward secret message";
        let aad = b"a:b:1234567890";

        let envelope = encrypt_dm_ephemeral(plaintext, &b_public, "a", "b", aad);
        assert_eq!(envelope.key_type, KeyType::Ephemeral);
        assert!(envelope.ephemeral_pub.is_some());

        // NOTE: For ephemeral mode, `sender_pub` is NOT used — the ephemeral
        // public key from the envelope is used instead. We pass a dummy key here
        // deliberately. If sender verification is added later, this test should
        // be updated to pass the actual sender's static public key.
        let dummy_pub = PublicKey::from([0u8; 32]);
        let decrypted = decrypt_dm(&envelope, &b_secret, &dummy_pub, "b", "a", aad).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn room_key_roundtrip() {
        let (creator_secret, creator_public) = test_keypair();
        let (member_secret, member_public) = test_keypair();

        let room_key = generate_room_key();
        let encrypted = encrypt_room_key(&room_key, &creator_secret, &member_public, "test-room-123");
        let decrypted = decrypt_room_key(&encrypted, &member_secret, &creator_public, "test-room-123").unwrap();

        assert_eq!(room_key, decrypted);
    }

    #[test]
    fn room_message_roundtrip() {
        let room_key = generate_room_key();
        let plaintext = b"Room message for everyone";

        let envelope = encrypt_room_message(plaintext, &room_key);
        let decrypted = decrypt_room_message(&envelope, &room_key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn room_message_wrong_key_fails() {
        let room_key = generate_room_key();
        let wrong_key = generate_room_key();

        let envelope = encrypt_room_message(b"secret", &room_key);
        let result = decrypt_room_message(&envelope, &wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn dm_info_is_order_independent() {
        assert_eq!(dm_info("alice", "bob", false), dm_info("bob", "alice", false));
        assert_eq!(dm_info("alice", "bob", true), dm_info("bob", "alice", true));
    }

    #[test]
    fn dm_info_ephemeral_differs_from_static() {
        assert_ne!(dm_info("alice", "bob", false), dm_info("alice", "bob", true));
    }
}
