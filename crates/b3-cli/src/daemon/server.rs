//! Daemon server: owns PTY, bridge. Accepts client connections via Unix socket.
//!
//! Lifecycle:
//!   1. Verify API reachable
//!   2. Spawn Claude Code in PTY
//!   3. Start puller task (EC2 → daemon: injections, hive, resize)
//!   4. Listen on Unix socket for attach/detach/stop
//!   5. On client attach: forward PTY output to client, client input to PTY
//!   6. On client detach or disconnect: PTY keeps running
//!   7. On stop: kill PTY, clean up, exit

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Notify};

use crate::daemon::ipc::{IpcListener, IpcStream};

use crate::config::Config;
use crate::daemon::protocol::{self, Message};
use crate::daemon::web::{self, WebState, SessionStore, EventChannel};
use crate::pty::manager::PtyManager;
use crate::bridge::puller;
use b3_common::AgentEvent;
use b3_common::health::HealthRegistry;

/// Shared daemon state accessible from client handler tasks.
struct DaemonState {
    config: Config,
    pty: Mutex<PtyManager>,
    /// Broadcast sender for PTY output — clients subscribe to get output.
    output_tx: broadcast::Sender<Vec<u8>>,
    /// Current terminal dimensions.
    rows: Mutex<u16>,
    cols: Mutex<u16>,
    /// Signal to shut down the daemon.
    /// Uses watch instead of Notify — Notify::notify_waiters() can miss notifications
    /// if no task is currently awaiting .notified() (race between select! branches).
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// Startup banner bytes — sent to each IPC client right after Welcome.
    /// Cached here because the broadcast channel drops messages sent before
    /// any subscriber; IPC clients connect after inject_output() fires.
    banner: Vec<u8>,
}

/// Path for the daemon PID file.
pub fn pid_path() -> PathBuf {
    Config::config_dir().join("b3.pid")
}

/// Check if a daemon is already running.
pub fn is_running() -> bool {
    let pid_file = pid_path();
    if !pid_file.exists() {
        return false;
    }

    // Read PID and check if process exists
    if let Ok(contents) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if process_exists(pid) {
                return true;
            }
        }
    }

    // Stale PID file — clean up
    let _ = std::fs::remove_file(&pid_file);
    false
}

/// Check if a process with the given PID exists.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::Foundation::CloseHandle;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            CloseHandle(handle);
            true
        } else {
            false
        }
    }
}

/// Send SIGTERM to a process (Unix) or TerminateProcess (Windows).
#[cfg(unix)]
pub fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[cfg(windows)]
pub fn kill_process(pid: u32) {
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    use windows_sys::Win32::Foundation::CloseHandle;
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Write PID file.
fn write_pid() -> anyhow::Result<()> {
    let pid = std::process::id();
    std::fs::write(pid_path(), pid.to_string())?;
    Ok(())
}

/// Clean up runtime files.
fn cleanup() {
    crate::daemon::ipc::cleanup_endpoint();
    let _ = std::fs::remove_file(pid_path());
}

/// Run the daemon. This is the main entry point after daemonization.
///
/// If `foreground` is true, run in the current process (for debugging / first attach).
/// If false, the caller should have already daemonized.
pub async fn run_daemon(
    mut config: Config,
    foreground: bool,
    claude_args: Vec<String>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    // Initialize tracing to log file for debugging.
    // Use ~/.b3/daemon.log so each OS user gets their own log file.
    // Falls back to /tmp/b3-daemon.log if home dir unavailable.
    let log_path = dirs::home_dir()
        .map(|h| h.join(".b3").join("daemon.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("b3-daemon.log"));
    // Rotate previous log before truncating — preserves crash evidence.
    if log_path.exists() {
        let prev = log_path.with_extension("log.prev");
        let _ = std::fs::rename(&log_path, &prev);
    }
    let log_file = std::fs::File::create(&log_path)
        .unwrap_or_else(|_| {
            // Fallback: platform-appropriate null device
            #[cfg(unix)]
            { std::fs::File::create("/dev/null").unwrap() }
            #[cfg(windows)]
            { std::fs::File::create("NUL").unwrap() }
        });
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("debug"))
        .init();

    // Clean up any stale IPC endpoint
    crate::daemon::ipc::cleanup_endpoint();

    // Write PID
    write_pid()?;

    // Set up cleanup on exit
    let _cleanup_guard = CleanupGuard;

    // Mark this process tree as inside a Babel3 session.
    // Child processes (Claude Code, bash) inherit this env var.
    // `b3 start` checks for it to prevent nested sessions.
    std::env::set_var("B3_SESSION", "1");

    // 0. Stamp version on config (enables upgrade detection)
    let current_version = env!("CARGO_PKG_VERSION");
    if config.b3_version != current_version {
        config.b3_version = current_version.to_string();
        let _ = config.save();
        if foreground {
            println!("  Config updated to v{current_version}");
        }
    }

    if foreground {
        println!("Starting Babel3 v{}...", current_version);
        println!("  Agent:    {} ({})", config.agent_email, config.agent_id);
        if let Some(dir) = crate::daemon::fork_state::browser_dir() {
            println!("  Browser:  DEVELOPMENT ({})", dir.display());
        }
    }

    // 1. Verify API reachable
    let api_health_url = format!("{}/health", config.api_url);
    let client = crate::http::build_client(std::time::Duration::from_secs(10))?;

    let mut connected = false;
    for attempt in 1..=5 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        match client.get(&api_health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                connected = true;
                break;
            }
            Ok(resp) => {
                tracing::debug!("Health check attempt {attempt}: status {}", resp.status());
            }
            Err(e) => {
                tracing::debug!("Health check attempt {attempt}: {e}");
            }
        }
    }

    if !connected {
        anyhow::bail!("Cannot reach API server at {}", config.api_url);
    }

    if foreground {
        println!("  ✓ API server reachable");
    }

    // 2b. Refresh config fields from server — detects post-rename config drift
    //     (api_url, web_url, agent_email stale after a server rename operation).
    //     Best-effort: logs changes and updates config.json, continues on failure.
    {
        let refresh_url = format!(
            "{}/api/agents/{}/config-fields",
            config.api_url, config.agent_id
        );
        match client
            .get(&refresh_url)
            .header("Authorization", format!("Bearer {}", config.api_key))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(fields) = resp.json::<b3_common::AgentConfigFields>().await {
                    let mut changed = false;
                    if fields.agent_email != config.agent_email {
                        tracing::info!(
                            "[Config] Refreshed from server: agent_email changed from {:?} to {:?}",
                            config.agent_email, fields.agent_email
                        );
                        config.agent_email = fields.agent_email;
                        changed = true;
                    }
                    if fields.api_url != config.api_url {
                        tracing::info!(
                            "[Config] Refreshed from server: api_url changed from {:?} to {:?}",
                            config.api_url, fields.api_url
                        );
                        config.api_url = fields.api_url;
                        changed = true;
                    }
                    if fields.web_url != config.web_url {
                        tracing::info!(
                            "[Config] Refreshed from server: web_url changed from {:?} to {:?}",
                            config.web_url, fields.web_url
                        );
                        config.web_url = fields.web_url;
                        changed = true;
                    }
                    if !fields.servers.is_empty() && fields.servers != config.servers {
                        tracing::info!(
                            "[Config] Refreshed from server: servers changed from {:?} to {:?}",
                            config.servers, fields.servers
                        );
                        config.servers = fields.servers;
                        changed = true;
                    }
                    if changed {
                        let _ = config.save();
                        if foreground {
                            println!("  ✓ Config refreshed from server");
                        }
                    } else {
                        tracing::debug!("[Config] Refresh complete — no drift detected");
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!("[Config] Config refresh returned {}: skipping", resp.status());
            }
            Err(e) => {
                tracing::warn!("[Config] Config refresh failed: {e} — continuing with local config");
            }
        }
    }

    // 3. Ensure CLAUDE.md has voice instructions (up-to-date)
    //    .mcp.json registration is handled by start.rs self-heal (register_voice_mcp)
    //    which runs moments before the daemon spawns — no need to duplicate it here.
    {
        // Also ensure CLAUDE.md has voice instructions (up-to-date)
        let claude_md_path = std::env::current_dir()
            .unwrap_or_default()
            .join("CLAUDE.md");
        let current_version = env!("CARGO_PKG_VERSION");
        let version_tag = format!("<!-- b3 v{current_version} -->");
        let web_url = &config.web_url;
        let agent_name = &config.agent_email;
        let voice_section = format!(
            "\n\n## Babel3 Voice\n\n\
             This project has Babel3 voice integration. The user can talk to you from their phone\n\
             at {web_url} and you can speak responses aloud.\n\n\
             **IMPORTANT: You have voice tools via the `b3` MCP. They ARE available and working.\n\
             Call them directly — do NOT check resources or capabilities first. Just call the tool.**\n\n\
             **Voice tools (call these directly):**\n\n\
             - `say(text, emotion, replying_to)` — Speak text aloud to the user's browser. All three parameters are REQUIRED.\n\
             - `voice_status()` — Check voice pipeline health.\n\
             - `voice_health()` — Deep health check (curls endpoints directly).\n\
             - `voice_logs(lines?)` — Get recent daemon logs.\n\
             - `voice_delivery_status(msg_id)` — Check message delivery.\n\
             - `voice_share_info(html)` — Push HTML content to connected browsers.\n\
             - `voice_show_image(path)` — Push an image to connected browsers.\n\
             - `voice_enroll_speakers(embeddings_dir)` — Upload speaker embeddings for diarization.\n\n\
             **When you receive voice input** (via <channel> tags or lines starting with `[M`), ALWAYS respond with `say()`.\n\
             The user is on their phone — text responses go to a terminal they're not watching.\n\
             Voice goes to their ears. Audio in = audio out.\n\n\
             **ALWAYS include all three parameters** when calling `say()`. The `replying_to` echoes back what the\n\
             user said (or a summary of context). The `emotion` drives the LED chromatophore animation on the\n\
             dashboard — it maps to colors and patterns via embeddings.\n\
             Use a short, natural English phrase describing the emotional tone (e.g., \"warm joy\", \"thoughtful\",\n\
             \"excited curiosity\", \"gentle reassurance\", \"playful mischief\"). Be specific and varied — avoid\n\
             repeating the same emotion. If unsure, use \"neutral calm\".\n\n\
             **Example:** User says \"Testing 1 2 3\" → you call `say(text=\"I can hear you! Your microphone is working great.\", emotion=\"cheerful enthusiasm\", replying_to=\"Testing 1 2 3\")`\n\n\
             Agent: {agent_name}\n\
             {version_tag}\n",
        );

        let existing = std::fs::read_to_string(&claude_md_path).unwrap_or_default();
        // Migrate old "HeyCode Voice" section to "Babel3 Voice"
        let existing = if existing.contains("## HeyCode Voice") {
            existing.replace("## HeyCode Voice", "## Babel3 Voice")
        } else {
            existing
        };
        if existing.contains(&version_tag) {
            // Already up-to-date — nothing to do
        } else if let Some(start) = existing.find("\n## Babel3 Voice") {
            // Section exists but is stale — replace it
            // Find end marker — handle both old (<!-- hc v, <!-- heycode v) and new (<!-- b3 v) tags
            let section_end = ["<!-- b3 v", "<!-- hc v", "<!-- heycode v"].iter()
                .filter_map(|tag| existing[start..].find(tag))
                .min()
                .and_then(|pos| existing[start + pos..].find('\n').map(|nl| start + pos + nl))
                .unwrap_or(existing.len());
            let mut updated = String::with_capacity(existing.len());
            updated.push_str(&existing[..start]);
            updated.push_str(&voice_section);
            if section_end < existing.len() {
                updated.push_str(&existing[section_end..]);
            }
            let _ = std::fs::write(&claude_md_path, updated);
            tracing::info!("CLAUDE.md voice section updated to v{current_version}");
        } else {
            // No section yet — append
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&claude_md_path)
                .ok();
            if let Some(ref mut f) = file {
                let _ = std::io::Write::write_all(f, voice_section.as_bytes());
            }
        }

        if foreground {
            let display_path = std::env::current_dir().unwrap_or_default().join(".mcp.json");
            println!("  ✓ Voice MCP registered in {}", display_path.display());
        }

        // Auto-allow b3 MCP tools so Claude doesn't prompt on every voice_say.
        // Claude Code reads ~/.claude/settings.json for permissions.
        let settings_dir = dirs::home_dir().unwrap_or_default().join(".claude");
        let _ = std::fs::create_dir_all(&settings_dir);
        let settings_path = settings_dir.join("settings.json");
        let mut settings: serde_json::Value = if settings_path.exists() {
            match std::fs::read_to_string(&settings_path) {
                Ok(s) => match serde_json::from_str(&s) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("Corrupted settings.json at {}: {e} — using empty settings", settings_path.display());
                        serde_json::json!({})
                    }
                },
                Err(e) => {
                    tracing::error!("Cannot read {}: {e}", settings_path.display());
                    serde_json::json!({})
                }
            }
        } else {
            serde_json::json!({})
        };
        // Ensure permissions.allow contains the b3 MCP tool pattern
        let allow_pattern = "mcp__b3__";
        if let Some(perms) = settings.get_mut("permissions") {
            if let Some(allow) = perms.get_mut("allow") {
                if let Some(arr) = allow.as_array_mut() {
                    if !arr.iter().any(|v| v.as_str() == Some(allow_pattern)) {
                        arr.push(serde_json::Value::String(allow_pattern.to_string()));
                    }
                }
            } else {
                perms["allow"] = serde_json::json!([allow_pattern]);
            }
        } else {
            settings["permissions"] = serde_json::json!({
                "allow": [allow_pattern]
            });
        }
        if let Ok(json_str) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(&settings_path, json_str);
        }
    }

    // Spawn Claude Code in PTY (fall back to bash if Claude exits quickly)
    let (rows, cols) = terminal_size().unwrap_or((24, 80));

    if foreground {
        println!("\nSpawning Claude Code ({cols}x{rows})...");
    }

    // Inject --channels plugin:b3 for MCP channel notification delivery.
    // This enables the voice MCP to deliver transcriptions as <channel> tags
    // instead of PTY injection. Skip if user already passed --channels.
    let mut claude_args_final = claude_args.clone();
    if !claude_args_final.iter().any(|a| a == "--channels") {
        claude_args_final.push("--channels".to_string());
        claude_args_final.push("plugin:b3".to_string());
    }
    let claude_args_refs: Vec<&str> = claude_args_final.iter().map(|s| s.as_str()).collect();
    let mut pty = match PtyManager::spawn_with_args(rows, cols, &claude_args_refs) {
        Ok(pty) => {
            if foreground {
                println!("  ✓ Claude Code running in PTY");
            }
            pty
        }
        Err(e) => {
            if foreground {
                eprintln!("  ⚠ Could not start Claude Code: {e}");
                eprintln!("  → Falling back to bash (run `claude` inside to start Claude Code)");
            }
            PtyManager::spawn_command("bash", &["--login"], rows, cols)?
        }
    };

    // Subscribe to PTY output BEFORE the startup wait, so we capture
    // Claude's welcome screen and any early output.
    let mut local_pusher_rx_early = pty.subscribe_output();
    let mut rebroadcast_rx_early = pty.subscribe_output();

    // Give Claude a moment to start, then check if it exited (not logged in, etc.)
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    if matches!(pty.try_wait(), Ok(Some(_))) {
        if foreground {
            eprintln!("  ⚠ Claude Code exited (not logged in?)");
            eprintln!("  → Falling back to bash (run `claude login` then `claude` to start)");
        }
        pty = PtyManager::spawn_command("bash", &["--login"], rows, cols)?;
        // Re-subscribe to the NEW PTY's broadcast channel. The old subscriptions
        // point to the dead PTY and will immediately get RecvError::Closed,
        // killing the local pusher and rebroadcast tasks silently.
        local_pusher_rx_early = pty.subscribe_output();
        rebroadcast_rx_early = pty.subscribe_output();
    }

    // Get writer handle for the puller
    let pty_writer = pty.writer_handle();

    // Create a broadcast channel that re-broadcasts PTY output for clients
    let (output_tx, _) = broadcast::channel::<Vec<u8>>(512);
    let output_tx_clone = output_tx.clone();

    // Spawn a task to re-broadcast PTY output to client channels
    // Uses early subscription created before the 3s startup wait.
    // Clamps cursor-up sequences to prevent terminal scroll-to-top (Ink bug workaround).
    let mut pty_rx = rebroadcast_rx_early;
    tokio::spawn(async move {
        loop {
            match pty_rx.recv().await {
                Ok(data) => {
                    let data = clamp_cursor_up(data);
                    let _ = output_tx_clone.send(data);
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Client broadcast lagged, missed {n} messages");
                    tokio::task::yield_now().await;
                }
            }
        }
    });

    // Health registry — subsystems report success/failure here.
    let health_registry = HealthRegistry::new();
    let puller_health = health_registry.register("puller");

    // 4. Start puller (EC2 → daemon)
    // Carries injection commands, hive messages, and resize events.
    // No pusher — session content never leaves the daemon.

    // Lazy web state — puller starts before WebState is created.
    // Populated after WebState init so the puller can handle WebRTC offers.
    let lazy_web_state: puller::LazyWebState =
        Arc::new(tokio::sync::RwLock::new(None));
    let lazy_ws_puller = lazy_web_state.clone();
    let peer_registry = crate::daemon::rtc::new_peer_registry();

    let pull_config = config.clone();
    let pull_resizer = pty.resizer();
    let pull_handle = tokio::spawn(async move {
        puller::pull_loop(&pull_config, pty_writer, Some(pull_resizer), Some(puller_health), lazy_ws_puller, peer_registry).await;
    });

    if foreground {
        println!("  ✓ Session bridge active (pull only)");
    }

    // Ensure daemon password exists BEFORE creating WebState (so the hash is available)
    match crate::config::Config::ensure_daemon_password() {
        Ok((password, true)) => {
            if foreground {
                println!("  ✓ Daemon password generated: {password}");
                println!("    Save this password — you'll need it to connect from a browser.");
                println!("    To reset: b3 set-password");
            }
            tracing::info!("Generated new daemon password");
        }
        Ok((_, false)) => {
            if foreground {
                println!("  ✓ Daemon password loaded");
            }
        }
        Err(e) => {
            tracing::warn!("Failed to ensure daemon password: {e}");
        }
    }

    // 4b. Start embedded web server (point-to-point dashboard)

    // Generate a random internal proxy token — used by ec2_proxy to connect to /ws-proxy
    // without the user-facing daemon password. 32 random bytes, hex-encoded.
    // Hex (0-9a-f only) is unconditionally URL-safe — no encoding needed in query strings.
    let proxy_token = {
        use rand::RngCore as _;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    };

    let web_state = WebState {
        sessions: SessionStore::new(),
        events: EventChannel::new(),
        agent_id: config.agent_id.clone(),
        agent_name: config.agent_email.split('@').next().unwrap_or("agent").to_string(),
        agent_email: config.agent_email.clone(),
        api_url: config.api_url.clone(),
        server_domain: config.server_domain().to_string(),
        api_key: config.api_key.clone(),
        session_sizes: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        pty_resizer: Some(pty.resizer()),
        pty_size: std::sync::Arc::new(tokio::sync::RwLock::new((rows, cols))),
        browser_console: std::sync::Arc::new(tokio::sync::RwLock::new(Vec::new())),
        eval_pending: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        browser_versions: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        browser_user_agents: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        browser_volumes: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        browser_audio_contexts: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        session_last_seen: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        eval_multi: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        health: Some(health_registry),
        // GPU client initialized with a placeholder — re-created after env loading (line ~700)
        played_tts: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new())),
        tts_progress: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        gpu_client: std::sync::Arc::new(crate::daemon::gpu_client::GpuClient::from_env()),
        tts_archive: std::sync::Arc::new(crate::daemon::tts_archive::TtsArchive::open(
            dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(".b3/tts-archive")
        )),
        info_archive: std::sync::Arc::new(crate::daemon::info_archive::InfoArchive::open(
            dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(".b3/info-archive")
        )),
        daemon_password_hash: crate::config::Config::load_daemon_password_hash(),
        browser_dir: crate::daemon::fork_state::browser_dir().map(|p| {
            // Canonicalize to resolve symlinks and ../ — limits what ServeDir will serve.
            // Dev-only feature behind explicit --browser-dir flag, but be defensive.
            std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())
        }),
        app_port: {
            let val = crate::daemon::fork_state::app_port();
            tracing::info!(app_port = ?val, "[Daemon] app proxy config");
            val
        },
        app_public: {
            let val = crate::daemon::fork_state::app_public();
            tracing::info!(app_public = val, "[Daemon] app proxy config");
            val
        },
        force_lowercase: std::sync::Arc::new(tokio::sync::RwLock::new(true)),
        session_manager: Arc::new(b3_reliable::SessionManager::new()),
        counters: std::sync::Arc::new(crate::daemon::web::PipelineCounters::default()),
        proxy_token: proxy_token.clone(),
        channel_events: tokio::sync::broadcast::channel(256).0,
    };

    // Populate lazy web state so the puller can handle WebRTC offers
    *lazy_web_state.write().await = Some(web_state.clone());

    // Fetch force_lowercase setting from EC2 in background (non-blocking)
    {
        let api_url = web_state.api_url.clone();
        let agent_id = web_state.agent_id.clone();
        let api_key = web_state.api_key.clone();
        let force_lc = web_state.force_lowercase.clone();
        tokio::spawn(async move {
            let val = web::fetch_force_lowercase(&api_url, &agent_id, &api_key).await;
            *force_lc.write().await = val;
            tracing::info!(force_lowercase = val, "Agent settings: force_lowercase loaded from EC2");
        });
    }

    // Pipeline telemetry ticker — logs counter snapshot every 10s.
    // Any counter that stops advancing while its upstream keeps going
    // reveals a dead pipeline stage in under 10 seconds.
    {
        let counters = web_state.counters.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                let snap = counters.snapshot();
                tracing::info!("[Telemetry] {snap}");
            }
        });
    }

    // GPU tunnel URL refresh — poll EC2 every 60s for current tunnel URL.
    // The GPU worker registers its Cloudflare tunnel with EC2 every 60s,
    // but the daemon only reads LOCAL_GPU_URL from runpod.env at startup.
    // Without this, Cloudflare tunnel rotations make the daemon's GPU client
    // hit stale URLs and fall back to RunPod unnecessarily.
    {
        let api_url = config.api_url.clone();
        let api_key = config.api_key.clone();
        tokio::spawn(async move {
            let client = crate::http::build_client(std::time::Duration::from_secs(5))
                .unwrap_or_default();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await; // skip immediate tick (startup fetch already happened)
            loop {
                interval.tick().await;
                let url = format!("{}/api/gpu/config", api_url);
                let resp = client.get(&url)
                    .header("Authorization", format!("Bearer {}", api_key))
                    .send()
                    .await;
                if let Ok(resp) = resp {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(new_url) = body.get("local_gpu_url").and_then(|v| v.as_str()) {
                            let current = std::env::var("LOCAL_GPU_URL").unwrap_or_default();
                            if !new_url.is_empty() && new_url != current {
                                tracing::info!(
                                    old = %current, new = %new_url,
                                    "GPU tunnel URL updated from EC2 signaling"
                                );
                                std::env::set_var("LOCAL_GPU_URL", new_url);
                            }
                        }
                    }
                }
            }
        });
    }

    // Local pusher: PTY output → SessionStore (zero network hop)
    // Uses the early subscription created before the 3s startup wait.
    let local_pusher_handle = web::spawn_local_pusher(
        web_state.clone(),
        local_pusher_rx_early,
    );

    // Inject startup banner into the PTY output broadcast so browser terminals
    // see it in the session buffer alongside the local terminal.
    // Single source of truth: crate::commands::start::startup_banner() owns the text.
    // Also cache in DaemonState so handle_client can replay it to IPC clients that
    // connect after this broadcast fires (broadcast drops messages with no subscribers).
    let banner_bytes = crate::commands::start::startup_banner(&config).into_bytes();
    pty.inject_output(banner_bytes.clone());

    // Local puller: WebSocket keystrokes → PTY stdin
    let web_events = web_state.events.clone();
    let local_pty_writer = pty.writer_handle();
    let local_puller_resizer = web_state.pty_resizer.clone();
    let local_puller_pty_size = web_state.pty_size.clone();
    let local_puller_handle = tokio::spawn(async move {
        let mut event_rx = web_events.subscribe();
        loop {
            match event_rx.recv().await {
                Ok(AgentEvent::PtyInput { data }) => {
                    // Filter focus report sequences
                    if data == "\x1b[I" || data == "\x1b[O" {
                        continue;
                    }
                    let mut writer = local_pty_writer.lock().await;
                    if let Err(e) = std::io::Write::write_all(&mut *writer, data.as_bytes()) {
                        tracing::error!("Local PTY write error: {e}");
                    }
                    let _ = std::io::Write::flush(&mut *writer);
                }
                Ok(AgentEvent::PtyResize { rows, cols }) => {
                    tracing::info!("[Puller] PtyResize event: {cols}x{rows}");
                    let resized = if let Some(ref resizer) = local_puller_resizer {
                        match resizer.lock() {
                            Ok(master) => {
                                match master.resize(portable_pty::PtySize {
                                    rows, cols, pixel_width: 0, pixel_height: 0,
                                }) {
                                    Ok(()) => { tracing::info!("[Puller] PTY resized to {cols}x{rows}"); true }
                                    Err(e) => { tracing::error!("[Puller] PTY resize failed: {e}"); false }
                                }
                            }
                            Err(e) => { tracing::error!("[Puller] PTY resizer lock failed: {e}"); false }
                        }
                    } else {
                        tracing::warn!("[Puller] No pty_resizer available for resize event");
                        false
                    };
                    if resized {
                        *local_puller_pty_size.write().await = (rows, cols);
                    }
                }
                Ok(AgentEvent::Transcription { text }) => {
                    // PTY injection removed — transcription must be delivered via MCP channel.
                    // The puller routes transcriptions to the channel subscriber; this local
                    // event handler no longer writes to PTY stdin.
                    let trunc: String = text.chars().take(80).collect();
                    tracing::warn!("[Local] Transcription event received but PTY injection disabled ({} bytes): {trunc}", text.len());
                }
                Ok(AgentEvent::Notification { title, body, .. }) => {
                    // PTY injection removed — notifications are informational only.
                    // Agents receive them via the browser overlay, not via PTY stdin.
                    tracing::info!("[Local] Notification: [{title}] {body}");
                }
                Ok(_) => {} // Ignore other events
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Local puller lagged, missed {n} events");
                }
            }
        }
    });

    // Start embedded web server on an OS-assigned port.
    // The daemon reports the actual port to the server, which updates the
    // Cloudflare tunnel ingress to match. One path for all agents.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel::<u16>();
    let web_state_server = web_state.clone();
    let web_handle = tokio::spawn(async move {
        if let Err(e) = web::start(web_state_server, 0, port_tx).await {
            tracing::error!("Embedded web server error: {e}");
        }
    });

    // Wait for the actual port assignment
    let web_port = port_rx.await.unwrap_or(0);

    // Write port to agent-scoped file so Voice MCP and other tools can discover it
    let port_file = config.dashboard_port_file();
    let _ = std::fs::write(&port_file, web_port.to_string());
    // Also set env var for child processes (Voice MCP reads B3_WEB_PORT)
    std::env::set_var("B3_WEB_PORT", web_port.to_string());

    if foreground {
        println!("  ✓ Local dashboard on http://localhost:{web_port}");
    }

    // 4b1. Start EC2 daemon proxy registration.
    //      Loads or generates a long-term x25519 keypair, then connects to
    //      EC2's /api/daemon-register on all known servers.
    //      Runs in background — non-blocking, reconnects on failure.
    //      Falls back to single-server mode if no peers are configured.
    {
        match crate::daemon::ec2_proxy::load_or_generate_keypair() {
            Ok(secret) => {
                // Build the server URL list: primary first, then peers.
                // Source: config.json servers (stored at setup from RegisterResponse, refreshed
                // via config-fields on each daemon start and saved back to config.json).
                // The config-fields refresh above updates config.servers in memory and saves it,
                // so the fresh list is available on the NEXT daemon start, not this one.
                // Falls back to primary-only if no peers are configured.
                let primary_url = config.api_url.clone();
                let mut server_urls: Vec<String> = if !config.servers.is_empty() {
                    config.servers.clone()
                } else {
                    vec![primary_url]
                };
                // Ensure the primary is always first (deduplicate if already present).
                let primary_domain = config.server_domain().to_string();
                server_urls.retain(|u| crate::daemon::ec2_proxy::url_to_domain_pub(u) != primary_domain);
                server_urls.insert(0, config.api_url.clone());

                let peer_count = server_urls.len().saturating_sub(1);
                let agent_id_proxy = config.agent_id.clone();
                let api_key_proxy = config.api_key.clone();
                crate::daemon::ec2_proxy::spawn_multi_server_registration(
                    server_urls,
                    agent_id_proxy,
                    api_key_proxy,
                    web_port,
                    web_port,
                    proxy_token.clone(),
                    secret,
                    web_state.clone(),
                );
                if foreground {
                    if peer_count > 0 {
                        println!("  ✓ EC2 daemon proxy registration started ({} server(s))", peer_count + 1);
                    } else {
                        println!("  ✓ EC2 daemon proxy registration started");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("EC2 proxy keypair generation failed: {e}");
                if foreground {
                    eprintln!("  ⚠ EC2 daemon proxy unavailable: {e}");
                }
            }
        }
    }

    // 4b2. Load GPU credentials: env vars > ~/.b3/runpod.env > server fetch.
    //      Load persisted runpod.env first (written by previous server fetches).
    if std::env::var("RUNPOD_API_KEY").unwrap_or_default().is_empty()
        || std::env::var("RUNPOD_GPU_ID").unwrap_or_default().is_empty()
    {
        if let Some(home) = dirs::home_dir() {
            let env_path = home.join(".b3").join("runpod.env");
            if env_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&env_path) {
                    for line in content.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') { continue; }
                        if let Some((key, val)) = line.split_once('=') {
                            let key = key.trim();
                            let val = val.trim();
                            if !val.is_empty() && std::env::var(key).unwrap_or_default().is_empty() {
                                std::env::set_var(key, val);
                            }
                        }
                    }
                    tracing::info!("GPU credentials loaded from {}", env_path.display());
                }
            }
        }
    }

    // Fetch from server if still missing (and persist for next time).
    if std::env::var("RUNPOD_API_KEY").unwrap_or_default().is_empty()
        || std::env::var("RUNPOD_GPU_ID").unwrap_or_default().is_empty()
    {
        let gpu_config_url = format!("{}/api/gpu/config", config.api_url);
        match reqwest::Client::new()
            .get(&gpu_config_url)
            .header("Authorization", format!("Bearer {}", config.api_key))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if body.get("configured").and_then(|v| v.as_bool()).unwrap_or(false) {
                        if let Some(token) = body.get("gpu_token").and_then(|v| v.as_str()) {
                            std::env::set_var("RUNPOD_API_KEY", token);
                        }
                        if let Some(url) = body.get("gpu_url").and_then(|v| v.as_str()) {
                            // gpu_url is "https://api.runpod.ai/v2/{id}/runsync" — extract the endpoint ID
                            if let Some(id) = url.strip_prefix("https://api.runpod.ai/v2/").and_then(|s| s.strip_suffix("/runsync")) {
                                std::env::set_var("RUNPOD_GPU_ID", id);
                            }
                        }
                    // Store local GPU config from server response
                    if let Some(local_url) = body.get("local_gpu_url").and_then(|v| v.as_str()) {
                        if !local_url.is_empty() {
                            std::env::set_var("LOCAL_GPU_URL", local_url);
                        }
                    }
                    if let Some(local_token) = body.get("local_gpu_token").and_then(|v| v.as_str()) {
                        if !local_token.is_empty() {
                            std::env::set_var("LOCAL_GPU_TOKEN", local_token);
                        }
                    }

                        // Persist to ~/.b3/runpod.env so credentials survive restarts without network
                        if let Some(home) = dirs::home_dir() {
                            let env_path = home.join(".b3").join("runpod.env");
                            let gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
                            let api_key_val = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
                            let local_gpu_url = std::env::var("LOCAL_GPU_URL").unwrap_or_default();
                            let local_gpu_token = std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default();
                            let mut content = String::new();
                            if !gpu_id.is_empty() && !api_key_val.is_empty() {
                                content.push_str(&format!("RUNPOD_GPU_ID={}\nRUNPOD_API_KEY={}\n", gpu_id, api_key_val));
                            }
                            if !local_gpu_url.is_empty() {
                                content.push_str(&format!("LOCAL_GPU_URL={}\n", local_gpu_url));
                            }
                            if !local_gpu_token.is_empty() {
                                content.push_str(&format!("LOCAL_GPU_TOKEN={}\n", local_gpu_token));
                            }
                            if !content.is_empty() {
                                let _ = std::fs::write(&env_path, &content);
                                tracing::info!("GPU credentials persisted to {}", env_path.display());
                            }
                        }
                        if foreground {
                            println!("  ✓ GPU config fetched from server");
                        }
                        tracing::info!("GPU config loaded from server");
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!("GPU config fetch returned {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("GPU config fetch failed: {e}");
            }
        }
    }

    // Build shared state
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(DaemonState {
        config: config.clone(),
        pty: Mutex::new(pty),
        output_tx,
        rows: Mutex::new(rows),
        cols: Mutex::new(cols),
        shutdown_tx,
        shutdown_rx,
        banner: banner_bytes,
    });

    // 5. Listen for IPC connections (Unix socket on Unix, TCP localhost on Windows)
    let listener = IpcListener::bind()?;

    if foreground {
        println!("  ✓ Daemon listening for connections");

        println!();
        println!("══════════════════════════════════════════");
        println!("  Babel3 daemon running.");
        println!();
        println!("  Local:    http://localhost:{web_port}");
        println!("  Share:    {}", config.web_url);
        println!();
        println!("  Attach:   b3 attach");
        println!("  Stop:     b3 stop  or  Ctrl+B q");
        println!("  Detach:   Ctrl+B d");
        println!("══════════════════════════════════════════");
    }

    // Signal that daemon is ready for attach connections
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    // 6. Accept loop + shutdown + child exit monitoring
    let state_exit = state.clone();
    let exit_monitor = tokio::spawn(async move {
        // Grace period: don't check for exit during startup (Claude Code takes
        // a few seconds to initialize, theme picker, login prompt, etc.)
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        loop {
            let exited = {
                let mut pty = state_exit.pty.lock().await;
                matches!(pty.try_wait(), Ok(Some(_)))
            };
            if exited {
                tracing::info!("Claude Code exited, shutting down daemon");
                let _ = state_exit.shutdown_tx.send(true);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });

    let mut shutdown_rx = state.shutdown_rx.clone();
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok(stream) => {
                        let client_state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, client_state).await {
                                tracing::debug!("Client disconnected: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("IPC accept error: {e}");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("Daemon shutdown requested");
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if foreground {
                    eprintln!("\nShutting down...");
                }
                break;
            }
        }
    }

    // 7. Clean shutdown with force-exit deadline.
    // WebRTC peers can take 30+ seconds to time out their ICE connectivity checks.
    // Don't let stale peers block shutdown — force-exit after 5 seconds.
    let shutdown_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);

    pull_handle.abort();
    exit_monitor.abort();
    local_pusher_handle.abort();
    local_puller_handle.abort();
    web_handle.abort();
    {
        let mut pty = state.pty.lock().await;
        let _ = pty.kill();
    }

    if foreground {
        println!("Done.");
    }

    // Deregister boot service — Ctrl+B q and b3 stop are equivalent.
    // After a clean shutdown the daemon should not auto-restart on next boot.
    if let Err(e) = crate::service::uninstall() {
        tracing::debug!("service::uninstall on shutdown: {e}");
    }

    // Force-exit: libdatachannel C++ threads may still be running teardown.
    // Don't wait — they'll be killed when the process exits.
    let remaining = shutdown_deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        tracing::warn!("Shutdown deadline exceeded — force exiting");
    } else {
        tokio::time::sleep(remaining).await;
        tracing::info!("Shutdown deadline reached — exiting");
    }
    std::process::exit(0);
}

/// Handle a single client connection.
async fn handle_client(stream: IpcStream, state: Arc<DaemonState>) -> anyhow::Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    // Send Welcome with current PTY dimensions
    let rows = *state.rows.lock().await;
    let cols = *state.cols.lock().await;
    protocol::write_message(
        &mut writer,
        &Message::Welcome { rows, cols },
    )
    .await?;

    // Replay startup banner — the broadcast fired before this client subscribed,
    // so it was dropped. Send it directly as the first Output frame.
    if !state.banner.is_empty() {
        protocol::write_message(
            &mut writer,
            &Message::Output(state.banner.clone()),
        )
        .await?;
    }

    // Subscribe to PTY output
    let mut output_rx = state.output_tx.subscribe();

    // Spawn output forwarding task (daemon → client)
    let mut output_writer = writer;
    let output_task = tokio::spawn(async move {
        loop {
            match output_rx.recv().await {
                Ok(data) => {
                    if let Err(e) = protocol::write_message(
                        &mut output_writer,
                        &Message::Output(data),
                    )
                    .await
                    {
                        tracing::debug!("Failed to send output to client: {e}");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // PTY closed — send Exited
                    let _ = protocol::write_message(
                        &mut output_writer,
                        &Message::Exited(0),
                    )
                    .await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Client lagged, missed {n} output chunks");
                }
            }
        }
    });

    // Read input from client (client → daemon)
    loop {
        match protocol::read_message(&mut reader).await {
            Ok(Message::Input(data)) => {
                let pty = state.pty.lock().await;
                let writer_handle = pty.writer_handle();
                let mut w = writer_handle.lock().await;
                if let Err(e) = w.write_all(&data) {
                    tracing::debug!("PTY write error: {e}");
                }
                let _ = w.flush();
            }
            Ok(Message::Resize { rows, cols }) => {
                let pty = state.pty.lock().await;
                if let Err(e) = pty.resize(rows, cols) {
                    tracing::debug!("PTY resize error: {e}");
                }
                *state.rows.lock().await = rows;
                *state.cols.lock().await = cols;
            }
            Ok(Message::Detach) => {
                tracing::info!("Client detached");
                break;
            }
            Ok(Message::Stop) => {
                tracing::info!("Stop command received from client");
                let _ = state.shutdown_tx.send(true);
                break;
            }
            Ok(Message::Status) => {
                let running = {
                    let mut pty = state.pty.lock().await;
                    matches!(pty.try_wait(), Ok(None))
                };
                let status = serde_json::json!({
                    "agent_id": state.config.agent_id,
                    "agent_email": state.config.agent_email,
                    "running": running,
                    "rows": *state.rows.lock().await,
                    "cols": *state.cols.lock().await,
                });
                // We need to send through the output task's writer.
                // For simplicity, use the output channel — but status responses
                // don't go through broadcast. Instead we'll handle this specially.
                // TODO: For now, status queries from attached clients aren't fully supported.
                // The `b3 status` command uses direct socket connection instead.
                tracing::debug!("Status query: {status}");
            }
            Err(e) => {
                // Client disconnected
                tracing::debug!("Client read error (disconnected?): {e}");
                break;
            }
            Ok(other) => {
                tracing::warn!("Unexpected message from client: {other:?}");
            }
        }
    }

    output_task.abort();
    Ok(())
}

fn terminal_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok().map(|(c, r)| (r, c))
}


/// RAII guard that cleans up PID file and socket on drop.
struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup();
    }
}

// ── Cursor-Up Clamping ────────────────────────────────────────────────
//
// Ink (React for CLI, used by Claude Code) emits cursor-up sequences to
// redraw content. When the content exceeds the viewport, these sequences
// can send the cursor above the visible area into scrollback, causing the
// terminal to jump to the top.
//
// Two patterns:
// 1. Single large cursor-up: \x1b[200A (CSI Cursor Up 200)
// 2. Cumulative cursor-up-one: \x1b[1A repeated 200 times (from eraseLines loop)
//
// We clamp both to MAX_CURSOR_UP_ROWS. Applied at the PTY rebroadcast
// point so all consumers (browser, b3 attach) are protected.

const MAX_CURSOR_UP_ROWS: usize = 100;

fn clamp_cursor_up(data: Vec<u8>) -> Vec<u8> {
    // Quick check: skip if no ESC byte present
    if !data.contains(&0x1b) {
        return data;
    }

    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    let len = data.len();
    let mut cumulative_up: usize = 0;

    while i < len {
        // Check for ESC [ sequence
        if data[i] == 0x1b && i + 1 < len && data[i + 1] == b'[' {
            // Parse CSI sequence: ESC [ <digits> <letter>
            let start = i;
            i += 2; // skip ESC [
            let num_start = i;
            while i < len && data[i].is_ascii_digit() {
                i += 1;
            }
            if i < len && data[i] == b'A' {
                // Cursor Up: ESC [ N A
                let n: usize = if num_start < i {
                    std::str::from_utf8(&data[num_start..i])
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                } else {
                    1 // ESC [ A with no number = cursor up 1
                };
                i += 1; // skip 'A'

                cumulative_up += n;

                if cumulative_up <= MAX_CURSOR_UP_ROWS {
                    // Emit the original sequence
                    result.extend_from_slice(&data[start..i]);
                } else if cumulative_up - n < MAX_CURSOR_UP_ROWS {
                    // Partially emit — only the remaining rows before hitting max
                    let remaining = MAX_CURSOR_UP_ROWS - (cumulative_up - n);
                    if remaining > 0 {
                        result.extend_from_slice(format!("\x1b[{}A", remaining).as_bytes());
                    }
                    // else: drop entirely — we've hit the max
                }
                // else: drop entirely — already over max
                continue;
            } else {
                // Not a Cursor Up — reset cumulative counter and emit as-is
                cumulative_up = 0;
                result.extend_from_slice(&data[start..=i.min(len - 1)]);
                if i < len { i += 1; }
                continue;
            }
        }

        // Non-CSI byte: reset cumulative cursor-up counter on any non-cursor-up output
        if data[i] != 0x1b {
            // Only reset on visible output, not on other ESC sequences
            if data[i] >= 0x20 && data[i] != 0x7f {
                cumulative_up = 0;
            }
        }
        result.push(data[i]);
        i += 1;
    }

    result
}
