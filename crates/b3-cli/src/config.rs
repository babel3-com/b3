//! Config directory resolution and config.json management.
//!
//! Config struct stores everything the agent needs to connect:
//!   - Identity: agent_id, agent_email
//!   - Auth: api_key
//!   - URLs: api_url (public HTTPS), web_url (dashboard)
//!
//! All runtime agent-to-server comms go through the EC2 encrypted proxy
//! (`daemon/ec2_proxy.rs`). api_url is used for initial setup and direct HTTPS calls.
//!
//! Config directory resolution (checked in order):
//!   1. fork_state::config_dir() static (set by pre_fork before daemon start)
//!   2. ./.b3/ in the current working directory
//!   3. Walk up to $HOME looking for .b3/config.json
//!   4. ~/.b3/ (default, existing behavior)
//!
//! Resolution runs once at process startup via OnceLock. For the daemon process,
//! fork_state::init() is called before fork() so the child inherits the value
//! without any env var pollution.
//!
//! Read/write with serde_json. Created during setup, read on every start.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub agent_id: String,
    pub agent_email: String,
    pub api_key: String,
    /// Public API URL — used for initial registration and direct HTTPS calls.
    pub api_url: String,
    /// Agent's mesh IP — retained for config.json compatibility with existing installs.
    /// No longer used for routing; all traffic goes through the EC2 proxy.
    #[serde(default)]
    pub wg_address: String,
    /// Relay endpoint — retained for config.json compatibility.
    #[serde(default)]
    pub relay_endpoint: String,
    /// Relay WireGuard public key — retained for config.json compatibility.
    #[serde(default)]
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
    /// When true, `b3 start` runs `b3 update` before launching the daemon.
    /// Opt-in during setup or via manual edit of .b3/config.json. Default: false.
    #[serde(default)]
    pub auto_update: bool,
}

impl Config {
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
    /// Resolve the config directory for this process (runs once, cached via OnceLock).
    ///
    /// Resolution order:
    ///   1. fork_state::config_dir() static (set by pre_fork before daemon fork)
    ///   2. ./.b3/ in CWD (if contains config.json)
    ///   3. Walk up from CWD to $HOME looking for .b3/config.json
    ///   4. ~/.b3/ (home default)
    fn resolve_config_dir() -> PathBuf {
        // 1. Fork state static — set by pre_fork/start_in_process before the daemon starts.
        //    Avoids the previous B3_CONFIG_DIR env-var approach that leaked into all descendants.
        if let Some(dir) = crate::daemon::fork_state::config_dir() {
            return dir.clone();
        }

        // 2+3. Walk up from CWD looking for .b3/config.json, stopping at $HOME
        if let Ok(cwd) = std::env::current_dir() {
            let home = dirs::home_dir().unwrap_or_default();
            let mut dir = cwd.as_path();
            loop {
                let candidate = dir.join(".b3");
                if candidate.join("config.json").exists() {
                    return candidate;
                }
                // Don't walk above home
                if dir == home || dir.parent().is_none() {
                    break;
                }
                dir = dir.parent().unwrap();
            }
        }

        // 4. Home default
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(".b3")
    }

    /// Directory for all Babel3 config (resolved once per process).
    pub fn config_dir() -> PathBuf {
        CONFIG_DIR.get_or_init(Self::resolve_config_dir).clone()
    }

    /// Called at the start of `b3 setup` to pin the config dir to CWD's .b3/
    /// when CWD is not $HOME and no explicit override is set.
    ///
    /// Without this, setup would fall through to ~/.b3/ in an empty folder,
    /// defeating per-folder agent isolation for new installs.
    pub fn init_for_setup() {
        // Only pin to CWD if no fork-state override is active and CWD isn't home.
        if crate::daemon::fork_state::config_dir().is_none() {
            let home = dirs::home_dir().unwrap_or_default();
            let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
            // Don't force ./.b3/ when already at home — home default is correct there.
            if cwd != home {
                // Force the lock to ./.b3/ before any other code calls config_dir().
                let _ = CONFIG_DIR.set(cwd.join(".b3"));
                return;
            }
        }
        // Otherwise let normal resolution run.
        let _ = CONFIG_DIR.set(Self::resolve_config_dir());
    }

    /// Path to main config file.
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
