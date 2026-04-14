//! Integration layer between crypto/hive.rs primitives and the daemon.
//!
//! Handles public key fetching, TOFU pinning, room key storage, and the
//! encrypt/decrypt workflows used by the hive proxy (outbound) and puller
//! (inbound). The crypto module (crypto/hive.rs) provides pure functions;
//! this module provides the stateful glue.
//!
//! ## v1 Known Limitations (Chen Wei + Vera review, 2026-03-13)
//!
//! - **AAD without timestamp:** DM AAD is `sender:target` (pair-bound). Prevents
//!   cross-pair replay. Within-pair replay still possible — v2 should add a
//!   sequence number or timestamp.
//! - **Per-message key fetch:** `decrypt_dm_if_encrypted` may HTTP-fetch the
//!   sender's public key for each message. TOFU file cache mitigates after
//!   first contact. v2: batch key fetch before decryption loop.
//! - **Room key plaintext on disk:** `~/.b3/room-keys/` files are base64
//!   with 0o600 perms (same posture as SSH keys). v2: encrypt at rest with
//!   WireGuard private key derivative.

use base64::Engine as _;
use serde_json::Value;
use std::path::PathBuf;
use x25519_dalek::PublicKey;

use super::hive::{
    self, EncryptedEnvelope, EncryptedRoomKey,
};
use crate::config::Config;

// ── Public Key Fetching + TOFU ────────────────────────────────────

/// Fetch an agent's public key from EC2 and apply TOFU pinning.
///
/// Returns the public key on success, or an error string suitable for
/// logging or returning to the MCP caller.
/// Fetched identity: public key + stable agent_id for HKDF derivation.
pub struct AgentIdentity {
    pub public_key: PublicKey,
    pub agent_id: String,
}

pub async fn fetch_and_pin_public_key(
    api_url: &str,
    api_key: &str,
    agent_name: &str,
) -> Result<AgentIdentity, String> {
    let url = format!("{}/api/agents/{}/public-key", api_url, agent_name);
    let client = crate::http::build_client(std::time::Duration::from_secs(10))
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .map_err(|e| format!("Failed to fetch public key for {agent_name}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "EC2 returned {} for {agent_name}'s public key",
            resp.status()
        ));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid JSON in public key response: {e}"))?;

    let key_b64 = body
        .get("wg_public_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing wg_public_key in response".to_string())?;

    // agent_id is stable across renames — used for HKDF derivation
    let agent_id = body
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or(agent_name) // fallback to name for old servers
        .to_string();

    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| format!("Invalid base64 in public key: {e}"))?
        .try_into()
        .map_err(|_| "Public key is not 32 bytes".to_string())?;

    let public_key = PublicKey::from(key_bytes);

    // TOFU: pin the key. On mismatch, warn but don't block (v1 behavior).
    match hive::pin_public_key(agent_name, &public_key) {
        Ok(true) => {
            tracing::info!("TOFU: pinned new public key for {agent_name}");
        }
        Ok(false) => {
            // Key matches cached — normal case
        }
        Err(old_key) => {
            let b64 = base64::engine::general_purpose::STANDARD;
            tracing::warn!(
                "⚠️ [HIVE-SECURITY] Agent {agent_name}'s public key has CHANGED. \
                 Previous: {}. New: {}. This could indicate key rotation or MITM.",
                b64.encode(old_key.as_bytes()),
                b64.encode(public_key.as_bytes()),
            );
            // v1: warn and proceed. v2: could reject or prompt user.
        }
    }

    Ok(AgentIdentity { public_key, agent_id })
}

// ── DM Encryption / Decryption ────────────────────────────────────

/// Encrypt a direct message for the given target agent.
///
/// Fetches the target's public key from EC2 (with TOFU), encrypts the
/// plaintext, and returns the serialized EncryptedEnvelope as a JSON string.
pub async fn encrypt_dm_for_target(
    api_url: &str,
    api_key: &str,
    my_name: &str,
    my_agent_id: &str,
    target_name: &str,
    plaintext: &str,
) -> Result<String, String> {
    let my_key = Config::load_wg_private_key()
        .ok_or_else(|| "WireGuard private key not found — run `b3 setup` first".to_string())?;

    let their = fetch_and_pin_public_key(api_url, api_key, target_name).await?;

    // Use stable agent_ids for HKDF derivation — survives renames.
    // AAD uses agent_ids too for consistency.
    let aad = format!("{my_agent_id}:{}", their.agent_id);

    let envelope = hive::encrypt_dm(
        plaintext.as_bytes(),
        &my_key,
        &their.public_key,
        my_agent_id,
        &their.agent_id,
        aad.as_bytes(),
    );

    serde_json::to_string(&envelope)
        .map_err(|e| format!("Failed to serialize envelope: {e}"))
}

/// Encrypt a direct message with forward secrecy (ephemeral DH keypair).
///
/// The ephemeral private key is generated, used once, then dropped.
/// Neither sender nor server can decrypt after this function returns.
pub async fn encrypt_dm_ephemeral_for_target(
    api_url: &str,
    api_key: &str,
    my_name: &str,
    my_agent_id: &str,
    target_name: &str,
    plaintext: &str,
) -> Result<String, String> {
    let their = fetch_and_pin_public_key(api_url, api_key, target_name).await?;

    let aad = format!("{my_agent_id}:{}", their.agent_id);

    let envelope = hive::encrypt_dm_ephemeral(
        plaintext.as_bytes(),
        &their.public_key,
        my_agent_id,
        &their.agent_id,
        aad.as_bytes(),
    );

    serde_json::to_string(&envelope)
        .map_err(|e| format!("Failed to serialize envelope: {e}"))
}

/// Decrypt an incoming DM envelope.
///
/// `sender_name` is used for TOFU lookup and key derivation.
/// `raw_text` is the message content field which may be JSON (encrypted) or plaintext.
pub async fn decrypt_dm_if_encrypted(
    api_url: &str,
    api_key: &str,
    my_name: &str,
    my_agent_id: &str,
    sender_name: &str,
    raw_text: &str,
) -> Result<String, String> {
    // Try to parse as EncryptedEnvelope
    let envelope: EncryptedEnvelope = match serde_json::from_str(raw_text) {
        Ok(env) => env,
        Err(_) => {
            // Not JSON or not an envelope — treat as plaintext (backward compat)
            tracing::debug!("Hive DM from {sender_name}: plaintext (not an encrypted envelope)");
            return Ok(raw_text.to_string());
        }
    };

    if !envelope.encrypted {
        // Explicitly marked as unencrypted
        tracing::debug!("Hive DM from {sender_name}: plaintext (encrypted=false)");
        return Ok(raw_text.to_string());
    }

    tracing::info!("Hive DM from {sender_name}: encrypted envelope detected, decrypting...");

    let my_key = Config::load_wg_private_key()
        .ok_or_else(|| "WireGuard private key not found".to_string())?;

    let sender = fetch_and_pin_public_key(api_url, api_key, sender_name).await?;

    // Use stable agent_ids for HKDF and AAD — matches what the sender used.
    let aad = format!("{}:{my_agent_id}", sender.agent_id);

    hive::decrypt_dm(&envelope, &my_key, &sender.public_key, my_agent_id, &sender.agent_id, aad.as_bytes())
        .map(|bytes| {
            let plaintext = String::from_utf8_lossy(&bytes).to_string();
            tracing::info!("Hive DM from {sender_name}: decrypted OK ({} bytes)", plaintext.len());
            plaintext
        })
        .map_err(|e| format!("Decryption failed for message from {sender_name}: {e}"))
}

// ── Room Key Storage ──────────────────────────────────────────────

/// Directory for room key files.
fn room_keys_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".b3")
        .join("room-keys")
}

/// Store a room key to disk.
///
/// `expires_at` is an optional RFC 3339 timestamp. If set, the key will be
/// refused for encryption after that time, and the cleanup loop will delete it.
pub fn store_room_key(
    room_id: &str,
    room_key: &[u8; 32],
    creator: &str,
    expires_at: Option<&str>,
) -> Result<(), String> {
    store_room_key_with_members(room_id, room_key, creator, expires_at, &[])
}

/// Store a room key with an optional member list.
pub fn store_room_key_with_members(
    room_id: &str,
    room_key: &[u8; 32],
    creator: &str,
    expires_at: Option<&str>,
    members: &[String],
) -> Result<(), String> {
    let dir = room_keys_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create room-keys dir: {e}"))?;

    let b64 = base64::engine::general_purpose::STANDARD;

    // Store the key
    let key_path = dir.join(format!("{room_id}.key"));
    std::fs::write(&key_path, b64.encode(room_key))
        .map_err(|e| format!("Failed to write room key: {e}"))?;

    // Store metadata
    let meta_path = dir.join(format!("{room_id}.meta"));
    let mut meta = serde_json::json!({ "creator": creator });
    if let Some(exp) = expires_at {
        meta["expires_at"] = serde_json::Value::String(exp.to_string());
    }
    if !members.is_empty() {
        meta["members"] = serde_json::json!(members);
    }
    std::fs::write(&meta_path, meta.to_string())
        .map_err(|e| format!("Failed to write room meta: {e}"))?;

    // Permissions: owner-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::set_permissions(&meta_path, std::fs::Permissions::from_mode(0o600));
    }

    tracing::info!("Stored room key for {room_id} (creator: {creator}, expires_at: {expires_at:?})");
    Ok(())
}

/// Load a room key from disk.
pub fn load_room_key(room_id: &str) -> Option<[u8; 32]> {
    let key_path = room_keys_dir().join(format!("{room_id}.key"));
    let b64_str = std::fs::read_to_string(&key_path).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_str.trim())
        .ok()?;
    bytes.try_into().ok()
}

/// Room metadata: creator and optional expiration.
pub struct RoomMeta {
    pub creator: String,
    pub expires_at: Option<String>,
    pub members: Vec<String>,
}

/// Load room metadata (creator name, expiration).
pub fn load_room_meta_full(room_id: &str) -> Option<RoomMeta> {
    let meta_path = room_keys_dir().join(format!("{room_id}.meta"));
    let meta_str = std::fs::read_to_string(&meta_path).ok()?;
    let meta: Value = serde_json::from_str(&meta_str).ok()?;
    let creator = meta.get("creator").and_then(|v| v.as_str())?.to_string();
    let expires_at = meta.get("expires_at").and_then(|v| v.as_str()).map(String::from);
    let members = meta.get("members")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Some(RoomMeta { creator, expires_at, members })
}

/// Load room metadata — returns just the creator name (backward compat).
pub fn load_room_meta(room_id: &str) -> Option<String> {
    load_room_meta_full(room_id).map(|m| m.creator)
}

/// Store room members in the existing meta file.
pub fn store_room_members(room_id: &str, members: &[String]) {
    let meta_path = room_keys_dir().join(format!("{room_id}.meta"));
    let mut meta = if let Ok(s) = std::fs::read_to_string(&meta_path) {
        serde_json::from_str::<serde_json::Value>(&s).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    meta["members"] = serde_json::json!(members);
    let _ = std::fs::write(&meta_path, meta.to_string());
}

/// Load room members from stored metadata.
pub fn load_room_members(room_id: &str) -> Vec<String> {
    load_room_meta_full(room_id).map(|m| m.members).unwrap_or_default()
}

/// Delete a room key (on leave, revoke, or expiration).
pub fn delete_room_key(room_id: &str) {
    let dir = room_keys_dir();
    let _ = std::fs::remove_file(dir.join(format!("{room_id}.key")));
    let _ = std::fs::remove_file(dir.join(format!("{room_id}.meta")));
    tracing::info!("Deleted room key for {room_id}");
}

// ── Room Encryption / Decryption ──────────────────────────────────

/// Encrypt a message for a room using the locally stored room key.
///
/// Refuses to encrypt if the room key has expired.
pub fn encrypt_room_message(room_id: &str, plaintext: &str) -> Result<String, String> {
    // Check expiration before encrypting
    if let Some(meta) = load_room_meta_full(room_id) {
        if let Some(ref exp_str) = meta.expires_at {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(exp_str) {
                if chrono::Utc::now() > exp {
                    delete_room_key(room_id);
                    return Err(format!(
                        "Room key for {room_id} expired at {exp_str} — key deleted"
                    ));
                }
            }
        }
    }

    let room_key = load_room_key(room_id)
        .ok_or_else(|| format!("No room key found for {room_id}"))?;

    let envelope = hive::encrypt_room_message(plaintext.as_bytes(), &room_key);
    serde_json::to_string(&envelope)
        .map_err(|e| format!("Failed to serialize room envelope: {e}"))
}

/// Decrypt a room message using the locally stored room key.
pub fn decrypt_room_message_if_encrypted(room_id: &str, raw_text: &str) -> String {
    // Try to parse as EncryptedEnvelope
    let envelope: EncryptedEnvelope = match serde_json::from_str::<EncryptedEnvelope>(raw_text) {
        Ok(env) if env.encrypted => env,
        _ => return raw_text.to_string(), // plaintext fallback
    };

    let room_key = match load_room_key(room_id) {
        Some(k) => k,
        None => {
            tracing::warn!("No room key for {room_id} — cannot decrypt");
            return format!("[encrypted — missing room key for {room_id}]");
        }
    };

    match hive::decrypt_room_message(&envelope, &room_key) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(e) => {
            tracing::error!("Room message decryption failed for {room_id}: {e}");
            format!("[decryption failed: {e}]")
        }
    }
}

// ── Room Key Distribution ─────────────────────────────────────────

/// Generate a room key and encrypt it for each member.
///
/// Returns the key distribution message payload as a JSON string.
/// The caller should send this as a room message so members can receive
/// their encrypted copy of the room key.
///
/// Payload format:
/// ```json
/// {
///   "msg_type": "key_distribution",
///   "room_id": "...",
///   "keys": {
///     "member_a": { "nonce": "...", "encrypted_key": "..." },
///     "member_b": { "nonce": "...", "encrypted_key": "..." }
///   }
/// }
/// ```
pub async fn generate_and_distribute_room_key(
    api_url: &str,
    api_key: &str,
    room_id: &str,
    my_name: &str,
    members: &[String],
    expires_at: Option<&str>,
) -> Result<String, String> {
    let my_key = Config::load_wg_private_key()
        .ok_or_else(|| "WireGuard private key not found".to_string())?;

    let room_key = hive::generate_room_key();

    // Encrypt room key for each member.
    // Keys are inserted with lowercase names so lookup in puller.rs is case-insensitive:
    // agents may have mixed-case email prefixes, but the map key is always lowercase.
    let mut key_distribution: serde_json::Map<String, Value> = serde_json::Map::new();
    for member in members {
        if member.to_lowercase() == my_name {
            continue; // We already have the key
        }
        let member_identity = fetch_and_pin_public_key(api_url, api_key, member).await?;
        let encrypted = hive::encrypt_room_key(&room_key, &my_key, &member_identity.public_key, room_id);
        key_distribution.insert(
            member.to_lowercase(),
            serde_json::to_value(&encrypted)
                .map_err(|e| format!("Failed to serialize encrypted room key: {e}"))?,
        );
    }

    // Store our own copy
    store_room_key(room_id, &room_key, my_name, expires_at)?;

    // Build the key distribution message payload
    let mut payload = serde_json::json!({
        "msg_type": "key_distribution",
        "room_id": room_id,
        "keys": Value::Object(key_distribution),
    });
    if let Some(exp) = expires_at {
        payload["expires_at"] = serde_json::Value::String(exp.to_string());
    }

    serde_json::to_string(&payload)
        .map_err(|e| format!("Failed to serialize key distribution: {e}"))
}

/// Receive and decrypt a room key from a key_distribution message.
pub async fn receive_room_key(
    api_url: &str,
    api_key: &str,
    room_id: &str,
    creator_name: &str,
    encrypted_key_json: &Value,
    expires_at: Option<&str>,
) -> Result<(), String> {
    let my_key = Config::load_wg_private_key()
        .ok_or_else(|| "WireGuard private key not found".to_string())?;

    let creator = fetch_and_pin_public_key(api_url, api_key, creator_name).await?;

    let encrypted: EncryptedRoomKey = serde_json::from_value(encrypted_key_json.clone())
        .map_err(|e| format!("Invalid encrypted room key format: {e}"))?;

    let room_key = hive::decrypt_room_key(&encrypted, &my_key, &creator.public_key, room_id)
        .map_err(|e| format!("Failed to decrypt room key: {e}"))?;

    store_room_key(room_id, &room_key, creator_name, expires_at)?;
    tracing::info!("Received and stored room key for {room_id} from {creator_name} (expires_at: {expires_at:?})");
    Ok(())
}

// ── Expired Key Cleanup ──────────────────────────────────────────

/// Scan all room keys and delete any that have expired.
///
/// Call this periodically (e.g., every 60s) from the daemon startup loop.
/// Returns the number of keys cleaned up.
pub fn cleanup_expired_room_keys() -> usize {
    let dir = room_keys_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };

    let now = chrono::Utc::now();
    let mut cleaned = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("meta") {
            continue;
        }

        let Ok(meta_str) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&meta_str) else {
            continue;
        };

        if let Some(exp_str) = meta.get("expires_at").and_then(|v| v.as_str()) {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(exp_str) {
                if now > exp {
                    let room_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");
                    tracing::info!("Cleaning up expired room key: {room_id} (expired {exp_str})");
                    delete_room_key(room_id);
                    cleaned += 1;
                }
            }
        }
    }

    cleaned
}

/// Parse a human-readable duration (e.g. "1h", "24h", "7d", "30m") into an
/// RFC 3339 `expires_at` timestamp from now.
pub fn parse_expires_in(expires_in: &str) -> Result<String, String> {
    let s = expires_in.trim();
    let (num_str, unit) = s.split_at(s.len().saturating_sub(1));
    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("Invalid expires_in format: {expires_in} (expected e.g. '1h', '24h', '7d')"))?;

    let duration = match unit {
        "s" => chrono::Duration::seconds(num),
        "m" => chrono::Duration::minutes(num),
        "h" => chrono::Duration::hours(num),
        "d" => chrono::Duration::days(num),
        _ => return Err(format!("Unknown unit in expires_in: {unit} (expected s/m/h/d)")),
    };

    let expires_at = chrono::Utc::now() + duration;
    Ok(expires_at.to_rfc3339())
}

// ── Timelock Vault ───────────────────────────────────────────────

/// Encrypt a message for timelock delivery.
///
/// Generates a fresh random symmetric key, encrypts the plaintext with
/// ChaCha20-Poly1305, and returns `(ciphertext_json, unlock_key_b64)`.
///
/// The caller should send the ciphertext as the message and the unlock key
/// separately — the server holds the key until `deliver_at`, then delivers both.
/// The sender discards the key immediately.
pub fn encrypt_timelock(plaintext: &str) -> Result<(String, String), String> {
    use chacha20poly1305::{aead::Aead, KeyInit, XChaCha20Poly1305, XNonce};
    use rand::RngCore;

    let b64 = base64::engine::general_purpose::STANDARD;

    // Generate random 256-bit key
    let mut key_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key_bytes);

    // Generate random 192-bit nonce (XChaCha)
    let mut nonce_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let cipher = XChaCha20Poly1305::new((&key_bytes).into());
    let nonce = XNonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| format!("Timelock encryption failed: {e}"))?;

    let envelope = serde_json::json!({
        "encrypted": true,
        "version": 1,
        "key_type": "timelock",
        "nonce": b64.encode(&nonce_bytes),
        "content": b64.encode(&ciphertext),
    });

    let envelope_json = serde_json::to_string(&envelope)
        .map_err(|e| format!("Failed to serialize timelock envelope: {e}"))?;

    let key_b64 = b64.encode(&key_bytes);

    Ok((envelope_json, key_b64))
}

/// Decrypt a timelock message using the provided key.
pub fn decrypt_timelock(envelope_json: &str, unlock_key_b64: &str) -> Result<String, String> {
    use chacha20poly1305::{aead::Aead, KeyInit, XChaCha20Poly1305, XNonce};

    let b64 = base64::engine::general_purpose::STANDARD;

    let envelope: serde_json::Value = serde_json::from_str(envelope_json)
        .map_err(|e| format!("Invalid timelock envelope: {e}"))?;

    if envelope.get("key_type").and_then(|v| v.as_str()) != Some("timelock") {
        return Err("Not a timelock envelope".to_string());
    }

    let nonce_b64 = envelope.get("nonce").and_then(|v| v.as_str())
        .ok_or("Missing nonce in timelock envelope")?;
    let content_b64 = envelope.get("content").and_then(|v| v.as_str())
        .ok_or("Missing content in timelock envelope")?;

    let key_bytes: [u8; 32] = b64.decode(unlock_key_b64)
        .map_err(|e| format!("Invalid unlock key: {e}"))?
        .try_into()
        .map_err(|_| "Unlock key must be 32 bytes".to_string())?;

    let nonce_bytes: [u8; 24] = b64.decode(nonce_b64)
        .map_err(|e| format!("Invalid nonce: {e}"))?
        .try_into()
        .map_err(|_| "Nonce must be 24 bytes".to_string())?;

    let ciphertext = b64.decode(content_b64)
        .map_err(|e| format!("Invalid ciphertext: {e}"))?;

    let cipher = XChaCha20Poly1305::new((&key_bytes).into());
    let nonce = XNonce::from_slice(&nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| "Timelock decryption failed (wrong key or tampered ciphertext)".to_string())?;

    String::from_utf8(plaintext)
        .map_err(|e| format!("Decrypted timelock message is not valid UTF-8: {e}"))
}
