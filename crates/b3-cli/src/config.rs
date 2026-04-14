//! ~/.b3/config.json management.
//!
//! Config struct stores everything the agent needs to connect:
//!   - Identity: agent_id, agent_email
//!   - Auth: api_key
//!   - Mesh: wg_address, relay_endpoint, relay_public_key
//!   - URLs: api_url (public, for initial setup only), web_url (dashboard)
//!
//! All runtime agent-to-server comms go through the WireGuard mesh.
//! The agent reaches the API server via the tunnel (default: 10.44.0.1:3100,
//! configurable via B3_SERVER_MESH_IP / B3_SERVER_MESH_PORT env vars).
//! api_url (public HTTPS) is only used during initial `b3 setup`.
//!
//! Read/write with serde_json. Created during setup, read on every start.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server's mesh IP — the relay runs the API server at this address.
/// Override with B3_SERVER_MESH_IP for self-hosted deployments.
pub fn server_mesh_ip() -> String {
    std::env::var("B3_SERVER_MESH_IP").unwrap_or_else(|_| "10.44.0.1".to_string())
}

/// Server's mesh port. Override with B3_SERVER_MESH_PORT.
pub fn server_mesh_port() -> u16 {
    std::env::var("B3_SERVER_MESH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3100)
}

/// Local TCP proxy port — the userspace tunnel listens here.
/// Agent code (reqwest, SSE) connects to 127.0.0.1:MESH_PROXY_PORT
/// which is forwarded through the WireGuard tunnel to 10.44.0.1:3000.
/// Override with B3_MESH_PROXY_PORT env var for multi-tenant hosting.
pub const MESH_PROXY_PORT_DEFAULT: u16 = 13000;

pub fn mesh_proxy_port() -> u16 {
    // 1. Env var override (explicit)
    if let Ok(port) = std::env::var("B3_MESH_PROXY_PORT") {
        if let Ok(p) = port.parse() {
            return p;
        }
    }
    // 2. Runtime port file (written by daemon after binding, handles fallback)
    if let Some(home) = dirs::home_dir() {
        let port_file = home.join(".b3").join("mesh-proxy.port");
        if let Ok(contents) = std::fs::read_to_string(&port_file) {
            if let Ok(p) = contents.trim().parse() {
                return p;
            }
        }
    }
    // 3. Default
    MESH_PROXY_PORT_DEFAULT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub agent_id: String,
    pub agent_email: String,
    pub api_key: String,
    /// Public API URL — used ONLY for initial registration (before mesh is up).
    pub api_url: String,
    /// Agent's mesh IP (e.g. "10.44.1.15").
    pub wg_address: String,
    /// Relay endpoint for WireGuard tunnel (e.g. "34.x.x.x:51820").
    pub relay_endpoint: String,
    /// Relay's WireGuard public key (base64).
    pub relay_public_key: String,
    pub push_interval_ms: u64,
    pub web_url: String,
    /// Babel3 version that created/last updated this config.
    /// Used for upgrade/downgrade management.
    #[serde(default)]
    pub b3_version: String,
    /// Peer server URLs for multi-homed daemon registration and SSE fan-out.
    /// Populated from RegisterResponse.servers at setup time.
    /// Daemon connects to all servers independently for redundancy.
    #[serde(default)]
    pub servers: Vec<String>,
}

impl Config {
    /// URL for API calls over the mesh via the local tunnel proxy.
    /// HTTP, not HTTPS — encryption is handled by WireGuard at the tunnel layer.
    #[allow(dead_code)]
    pub fn mesh_api_url(&self) -> String {
        format!("http://127.0.0.1:{}", mesh_proxy_port())
    }

    /// Domain extracted from api_url (e.g., "babel3.com" from "https://babel3.com").
    /// Used for CORS origins, tunnel hostnames, GPU proxy URLs, etc.
    pub fn server_domain(&self) -> &str {
        self.api_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .and_then(|host| host.split(':').next())
            .unwrap_or("localhost")
    }
}

impl Config {
    /// Directory for all Babel3 config: ~/.b3/
    pub fn config_dir() -> PathBuf {
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(".b3")
    }

    /// Path to main config file: ~/.b3/config.json
    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    /// Agent-scoped runtime directory: ~/.b3/agents/<slug>/
    /// Used for per-agent port files, PID files, etc.
    /// Creates the directory if it doesn't exist.
    pub fn agent_dir(&self) -> PathBuf {
        let slug = self.agent_email.split('@').next().unwrap_or("default");
        let dir = Self::config_dir().join("agents").join(slug);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Port file for this agent's dashboard: ~/.b3/agents/<slug>/dashboard.port
    pub fn dashboard_port_file(&self) -> PathBuf {
        self.agent_dir().join("dashboard.port")
    }

    /// Read the dashboard port for this agent. Returns None if not running.
    pub fn dashboard_port(&self) -> Option<u16> {
        std::fs::read_to_string(self.dashboard_port_file())
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// Build the inject URL for this agent's running daemon.
    pub fn inject_url(&self) -> Option<String> {
        self.dashboard_port()
            .map(|port| format!("http://127.0.0.1:{}/api/inject", port))
    }

    /// Save config to disk. Creates ~/.b3/ with 700 permissions.
    pub fn save(&self) -> anyhow::Result<()> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }

        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::config_path(), json)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(Self::config_path(), std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

    /// Validate config values are within acceptable ranges.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.api_key.is_empty(), "api_key cannot be empty");
        anyhow::ensure!(!self.api_url.is_empty(), "api_url cannot be empty");
        anyhow::ensure!(
            self.api_url.starts_with("http"),
            "api_url must be an HTTP(S) URL, got: {}", self.api_url
        );
        anyhow::ensure!(
            self.push_interval_ms >= 50 && self.push_interval_ms <= 10_000,
            "push_interval_ms must be 50-10000, got: {}", self.push_interval_ms
        );
        Ok(())
    }

    /// Load config from disk and validate.
    pub fn load() -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(Self::config_path())?;
        let config: Self = serde_json::from_str(&data)?;
        config.validate()?;
        Ok(config)
    }

    /// Check if config file exists (agent is already set up).
    pub fn exists() -> bool {
        Self::config_path().exists()
    }

    // ── Daemon Password ─────────────────────────────────────────────

    /// Path to daemon password hash: ~/.b3/daemon_password
    pub fn daemon_password_path() -> PathBuf {
        Self::config_dir().join("daemon_password")
    }

    /// Load the daemon password hash from disk. Returns None if not set.
    pub fn load_daemon_password_hash() -> Option<String> {
        std::fs::read_to_string(Self::daemon_password_path())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Save a daemon password hash to disk with restrictive permissions.
    pub fn save_daemon_password_hash(hash: &str) -> anyhow::Result<()> {
        let path = Self::daemon_password_path();
        std::fs::write(&path, hash)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Generate a random daemon password (8 lowercase alphanumeric chars).
    /// Returns the plaintext password. Caller is responsible for hashing and saving.
    pub fn generate_daemon_password() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let chars: Vec<char> = (0..8)
            .map(|_| {
                let idx = rng.gen_range(0..36);
                if idx < 10 { (b'0' + idx) as char } else { (b'a' + idx - 10) as char }
            })
            .collect();
        chars.into_iter().collect()
    }

    /// Hash a daemon password using Argon2id with a random salt.
    /// Returns a PHC-format string: `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`
    pub fn hash_daemon_password(password: &str) -> String {
        use argon2::{Argon2, password_hash::{PasswordHasher, SaltString, rand_core::OsRng}};
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("Argon2 hashing failed")
            .to_string()
    }

    /// Verify a daemon password against the stored Argon2id hash.
    pub fn verify_daemon_password(password: &str, hash: &str) -> bool {
        if hash.starts_with("$argon2") {
            use argon2::{Argon2, password_hash::{PasswordHash, PasswordVerifier}};
            let Ok(parsed) = PasswordHash::new(hash) else { return false };
            Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
        } else {
            // Unrecognized hash format — reject
            tracing::warn!("Daemon password hash in unrecognized format. Run `b3 set-password` to reset.");
            false
        }
    }

    /// Ensure a daemon password exists. If not, generate one and save it.
    /// Returns the plaintext password (for display to user on first run).
    pub fn ensure_daemon_password() -> anyhow::Result<(String, bool)> {
        if let Some(_hash) = Self::load_daemon_password_hash() {
            // Password already exists — we can't recover the plaintext
            Ok((String::new(), false))
        } else {
            let password = Self::generate_daemon_password();
            let hash = Self::hash_daemon_password(&password);
            Self::save_daemon_password_hash(&hash)?;
            Ok((password, true))
        }
    }

    // ── WireGuard Key (E2E Encrypted Hive) ──────────────────────────

    /// Path to the WireGuard private key: ~/.b3/wg/private.key
    pub fn wg_private_key_path() -> PathBuf {
        Self::config_dir().join("wg").join("private.key")
    }

    /// Load the WireGuard X25519 private key from disk.
    /// Returns None if the key file doesn't exist (pre-setup or standalone mode).
    pub fn load_wg_private_key() -> Option<x25519_dalek::StaticSecret> {
        let b64 = std::fs::read_to_string(Self::wg_private_key_path()).ok()?;
        let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .ok()?
            .try_into()
            .ok()?;
        Some(x25519_dalek::StaticSecret::from(bytes))
    }
}
