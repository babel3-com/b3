//! WireGuard configuration parsed from agent credentials.
//!
//! Reads the agent's private key from ~/.b3/wg/private.key and
//! combines it with relay info from config.json to build the WG tunnel config.

use std::net::{Ipv4Addr, SocketAddr};

use crate::config::Config;

/// WireGuard tunnel configuration derived from agent registration credentials.
pub struct WgConfig {
    /// Agent's X25519 private key (32 bytes, from ~/.b3/wg/private.key).
    pub private_key: [u8; 32],
    /// Relay server's WireGuard public key (32 bytes).
    pub peer_public_key: [u8; 32],
    /// Relay server's public UDP endpoint (e.g. 34.x.x.x:51820).
    pub peer_endpoint: SocketAddr,
    /// Agent's mesh IP address (e.g. 10.44.1.15).
    pub source_ip: Ipv4Addr,
    /// PersistentKeepalive interval in seconds (25 for NAT traversal).
    pub keepalive: u16,
}

impl WgConfig {
    /// Build WG config from the saved agent config + private key file.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        // Read private key from disk
        let key_path = Config::config_dir().join("wg").join("private.key");
        let key_b64 = std::fs::read_to_string(&key_path)
            .map_err(|e| anyhow::anyhow!("Cannot read WG private key at {}: {e}", key_path.display()))?;
        let private_key = decode_base64_key(key_b64.trim(), "private key")?;

        // Parse relay public key
        let peer_public_key = decode_base64_key(&config.relay_public_key, "relay public key")?;

        // Parse relay endpoint
        let peer_endpoint: SocketAddr = config.relay_endpoint.parse()
            .map_err(|e| anyhow::anyhow!("Invalid relay endpoint '{}': {e}", config.relay_endpoint))?;

        // Parse agent mesh IP
        let source_ip: Ipv4Addr = config.wg_address.parse()
            .map_err(|e| anyhow::anyhow!("Invalid mesh IP '{}': {e}", config.wg_address))?;

        Ok(WgConfig {
            private_key,
            peer_public_key,
            peer_endpoint,
            source_ip,
            keepalive: 25,
        })
    }
}

/// Decode a base64-encoded 32-byte key into a fixed-size array.
fn decode_base64_key(b64: &str, label: &str) -> anyhow::Result<[u8; 32]> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| anyhow::anyhow!("Invalid base64 for {label}: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("{label} must be 32 bytes, got {}", v.len()))?;
    Ok(arr)
}
