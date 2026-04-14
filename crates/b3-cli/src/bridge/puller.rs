//! Inbound: SSE events → PTY injection.
//!
//! Holds SSE connection to GET /api/agents/{id}/events over the mesh.
//! Dispatches events to PTY writer for injection:
//!   - "transcription" → write text + \n to PTY stdin
//!   - "hive"          → write [HIVE from={sender}] {message}\n to PTY stdin
//!
//! Auto-reconnects on disconnect with 1s delay.

use futures_util::StreamExt;
use reqwest::Client;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;

use crate::config::Config;
use crate::daemon::web::WebState;
use crate::http;
use crate::pty::manager::PtyResizer;
use b3_common::AgentEvent;
use b3_common::backoff::Backoff;
use b3_common::health::SubsystemHandle;
use b3_common::limits::SSE_TIMEOUT;

/// Lazy web state — set after WebState is created (puller starts before it).
pub type LazyWebState = Arc<tokio::sync::RwLock<Option<WebState>>>;

use crate::daemon::rtc::PeerRegistry;

/// Tracks recently-seen hive message fingerprints to deduplicate between
/// SSE delivery and polling fallback. Both paths check this before injecting.
#[derive(Clone)]
struct HiveDedup {
    inner: Arc<Mutex<VecDeque<u64>>>,
}

impl HiveDedup {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Compute a fingerprint from sender + message content.
    fn fingerprint(sender: &str, text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        sender.hash(&mut hasher);
        // Use first 200 chars to avoid hashing huge messages
        let prefix: String = text.chars().take(200).collect();
        prefix.hash(&mut hasher);
        hasher.finish()
    }

    /// Check if a message was recently seen. If not, mark it as seen and return false.
    /// If already seen, return true (duplicate).
    async fn check_and_mark(&self, sender: &str, text: &str) -> bool {
        let fp = Self::fingerprint(sender, text);
        let mut dedup = self.inner.lock().await;
        if dedup.contains(&fp) {
            return true; // duplicate
        }
        dedup.push_back(fp);
        if dedup.len() > 200 {
            dedup.pop_front();
        }
        false // new message
    }
}

/// Maximum consecutive permanent failures before the puller gives up.
/// Permanent = auth failure (401/403). After this many, the API key is almost
/// certainly wrong and hammering the server every 30s is pointless.
const MAX_PERMANENT_FAILURES: u32 = 10;

/// Interval between hive message polls (safety net for missed SSE events).
const HIVE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Returns the agent's lowercase name (email prefix before '@').
/// Lowercase is canonical for hive key_distribution map lookups — the server
/// inserts keys with lowercased member names to handle mixed-case email prefixes.
fn agent_name(config: &Config) -> String {
    config.agent_email.split('@').next().unwrap_or("agent").to_lowercase()
}

/// Classified SSE connection result.
enum SseResult {
    /// Connection was established and stream ran until clean close or timeout.
    CleanClose,
    /// Server returned a permanent error (401, 403) — bad credentials.
    PermanentFailure(u16),
    /// Server returned a transient error (5xx, etc.) or stream broke mid-flight.
    TransientFailure(String),
    /// Network-level failure (DNS, connect timeout, TLS).
    NetworkError(String),
}

/// Run the SSE event pull loop.
///
/// Connects to the server's SSE endpoint on ALL known servers in parallel.
/// Each server gets its own reconnect loop with independent backoff and
/// per-server `last_event_id` watermarks. The `HiveDedup` is shared across
/// all loops so a message delivered by server A is not re-injected when
/// server B delivers the same event.
///
/// Also spawns a background hive polling task (primary server only) as a
/// safety net for messages that fall through the SSE cracks.
pub async fn pull_loop(
    config: &Config,
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pty_resizer: Option<PtyResizer>,
    health: Option<SubsystemHandle>,
    web_state: LazyWebState,
    peer_registry: PeerRegistry,
) {
    let client = match http::streaming_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to create HTTP client for puller: {e}");
            return;
        }
    };

    // Shared dedup across all SSE loops and the polling fallback.
    let hive_dedup = HiveDedup::new();

    // Spawn hive message polling fallback on primary server only.
    let poll_writer = pty_writer.clone();
    let poll_config = config.clone();
    let poll_dedup = hive_dedup.clone();
    let poll_ws = web_state.clone();
    tokio::spawn(async move {
        hive_poll_loop(&poll_config, poll_writer, poll_dedup, &poll_ws).await;
    });

    // Build the server URL list: primary first, then peers from config.
    // config.servers is populated at setup from RegisterResponse and refreshed
    // via config-fields on each daemon start (lands on next start).
    let mut server_urls: Vec<String> = vec![config.api_url.clone()];
    for peer in &config.servers {
        // Deduplicate: skip if the peer resolves to the same host as primary.
        let primary_host = config.api_url
            .trim_start_matches("https://").trim_start_matches("http://")
            .split('/').next().and_then(|h| h.split(':').next()).unwrap_or("");
        let peer_host = peer
            .trim_start_matches("https://").trim_start_matches("http://")
            .split('/').next().and_then(|h| h.split(':').next()).unwrap_or("");
        if peer_host != primary_host && !peer_host.is_empty() {
            server_urls.push(peer.clone());
        }
    }

    if server_urls.len() > 1 {
        tracing::info!(
            server_count = server_urls.len(),
            "[Puller] Starting SSE fan-out across {} servers",
            server_urls.len()
        );
    }

    // Spawn N-1 peer loops in background; run primary loop in this task.
    for peer_url in server_urls.iter().skip(1).cloned() {
        let peer_client = client.clone();
        let peer_config = config.clone();
        let peer_writer = pty_writer.clone();
        let peer_resizer = pty_resizer.clone();
        let peer_dedup = hive_dedup.clone();
        let peer_web_state = web_state.clone();
        let peer_registry = peer_registry.clone();
        tokio::spawn(async move {
            pull_loop_for_server(
                &peer_url,
                &peer_client,
                &peer_config,
                &peer_writer,
                &peer_resizer,
                None, // peer loops don't report to health subsystem
                &peer_dedup,
                &peer_web_state,
                &peer_registry,
            ).await;
        });
    }

    // Primary loop runs in this task (carries health reporting).
    let primary_url = config.api_url.clone();
    pull_loop_for_server(
        &primary_url,
        &client,
        config,
        &pty_writer,
        &pty_resizer,
        health,
        &hive_dedup,
        &web_state,
        &peer_registry,
    ).await;
}

/// Single-server SSE reconnect loop.
///
/// `server_url` is the base API URL for this server (e.g. `https://babel3.com`).
/// Runs until a permanent auth failure exhausts retries.
async fn pull_loop_for_server(
    server_url: &str,
    client: &Client,
    config: &Config,
    pty_writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    pty_resizer: &Option<PtyResizer>,
    health: Option<SubsystemHandle>,
    hive_dedup: &HiveDedup,
    web_state: &LazyWebState,
    peer_registry: &PeerRegistry,
) {
    let url = format!("{}/api/agents/{}/events", server_url, config.agent_id);

    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
    let mut permanent_failures: u32 = 0;

    loop {
        tracing::info!("Connecting to SSE: {url}");

        let delay = match connect_sse(client, &url, config, pty_writer, pty_resizer, hive_dedup, web_state, peer_registry).await {
            SseResult::CleanClose => {
                tracing::info!(
                    consecutive_failures = backoff.consecutive_failures,
                    "SSE connection closed normally"
                );
                if let Some(ref h) = health { h.record_success(); }
                backoff.reset();
                permanent_failures = 0;
                Duration::from_secs(1)
            }
            SseResult::PermanentFailure(status) => {
                permanent_failures += 1;
                tracing::error!(
                    http_status = status,
                    consecutive_failures = backoff.consecutive_failures + 1,
                    permanent_failures,
                    max_permanent = MAX_PERMANENT_FAILURES,
                    "puller permanent failure — check api_key"
                );
                if let Some(ref h) = health { h.record_failure(&format!("HTTP {status}")); }

                if permanent_failures >= MAX_PERMANENT_FAILURES {
                    tracing::error!(
                        "Puller giving up after {permanent_failures} permanent auth failures. \
                         Fix api_key in ~/.b3/config.json and restart the daemon."
                    );
                    return;
                }
                backoff.next_delay_max()
            }
            SseResult::TransientFailure(ref reason) => {
                let delay = backoff.next_delay();
                if backoff.should_log() {
                    tracing::warn!(
                        error = %reason,
                        consecutive_failures = backoff.consecutive_failures,
                        delay_ms = delay.as_millis() as u64,
                        "puller transient failure"
                    );
                }
                if let Some(ref h) = health { h.record_failure(reason); }
                delay
            }
            SseResult::NetworkError(ref reason) => {
                let delay = backoff.next_delay();
                if backoff.should_log() {
                    tracing::warn!(
                        error = %reason,
                        consecutive_failures = backoff.consecutive_failures,
                        delay_ms = delay.as_millis() as u64,
                        "puller network error"
                    );
                }
                if let Some(ref h) = health { h.record_failure(reason); }
                delay
            }
        };

        tracing::info!(delay_ms = delay.as_millis() as u64, "reconnecting SSE");
        tokio::time::sleep(delay).await;
    }
}

async fn connect_sse(
    client: &Client,
    url: &str,
    config: &Config,
    pty_writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    pty_resizer: &Option<PtyResizer>,
    hive_dedup: &HiveDedup,
    web_state: &LazyWebState,
    peer_registry: &PeerRegistry,
) -> SseResult {
    let response = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Accept", "text/event-stream")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return SseResult::NetworkError(e.to_string()),
    };

    let status = response.status().as_u16();
    if !response.status().is_success() {
        return match status {
            401 | 403 => SseResult::PermanentFailure(status),
            _ => SseResult::TransientFailure(format!("HTTP {status}")),
        };
    }

    tracing::debug!(http_status = status, "SSE connected");

    use eventsource_stream::Eventsource;
    let mut stream = response.bytes_stream().eventsource();

    tracing::debug!("starting SSE event loop");

    let sse_timeout = SSE_TIMEOUT;

    loop {
        match tokio::time::timeout(sse_timeout, stream.next()).await {
            Ok(Some(Ok(event))) => {
                if event.event == "keepalive" { continue; }
                tracing::trace!(event_type = %event.event, data_len = event.data.len(), "SSE event");
                handle_event(&event.event, &event.data, pty_writer, pty_resizer, config, hive_dedup, &web_state, peer_registry).await;
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("SSE stream error: {e}");
                break;
            }
            Ok(None) => {
                tracing::debug!("SSE stream ended (server closed)");
                break;
            }
            Err(_) => {
                tracing::warn!("SSE timeout — no data for 60s, reconnecting");
                break;
            }
        }
    }

    SseResult::CleanClose
}

async fn handle_event(
    event_type: &str,
    data: &str,
    pty_writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    pty_resizer: &Option<PtyResizer>,
    config: &Config,
    hive_dedup: &HiveDedup,
    web_state: &LazyWebState,
    peer_registry: &PeerRegistry,
) {
    match event_type {
        "transcription" => {
            // Transcription delivery: prefer MCP channel notification over PTY injection.
            // The puller sends to channel_events directly — if send() returns Ok(n) with
            // n > 0, at least one SSE subscriber received it (MCP process). If no subscribers,
            // fall back to PTY injection. This avoids the TOCTOU race of checking
            // receiver_count() separately from the broadcast.
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::Transcription { text }) => {
                    let trunc: String = text.chars().take(80).collect();
                    // Try channel delivery first — send and check are colocated (no race).
                    let channel_delivered = if let Some(ws) = web_state.read().await.as_ref() {
                        let msg_id = format!("{:016x}", rand::random::<u64>());
                        ws.channel_events.send(crate::daemon::web::ChannelEvent::Transcription {
                            text: text.clone(),
                            message_id: msg_id,
                        }).unwrap_or(0) > 0
                    } else {
                        false
                    };
                    if channel_delivered {
                        tracing::info!(
                            "Transcription ({} bytes) delivered via MCP channel: {trunc}",
                            text.len()
                        );
                    } else {
                        tracing::warn!(
                            "Transcription ({} bytes) dropped — no MCP channel subscriber: {trunc}",
                            text.len()
                        );
                    }
                }
                _ => {
                    tracing::warn!("Unexpected transcription event format: {data}");
                }
            }
        }
        "hive" => {
            // Parse and inject hive message into PTY stdin.
            // Decrypt E2E encrypted messages before injection.
            // Sanitize newlines — PTY interprets \n as Enter, fragmenting the message.
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::Hive { sender, text }) => {
                    // Mark in dedup so the polling fallback won't re-inject
                    hive_dedup.check_and_mark(&sender, &text).await;

                    let my_name = agent_name(config);
                    let my_agent_id = &config.agent_id;

                    // Check for timelock delivery (server bundled unlock_key)
                    let decrypted = if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                        if parsed.get("timelock_delivery").and_then(|v| v.as_bool()) == Some(true) {
                            let content = parsed.get("content").and_then(|v| v.as_str()).unwrap_or("");
                            let unlock_key = parsed.get("unlock_key").and_then(|v| v.as_str()).unwrap_or("");
                            match crate::crypto::hive_integration::decrypt_timelock(content, unlock_key) {
                                Ok(plaintext) => {
                                    tracing::info!("Timelock message from {sender} decrypted OK");
                                    plaintext
                                }
                                Err(e) => {
                                    tracing::error!("Timelock decryption failed from {sender}: {e}");
                                    format!("[timelock decryption failed: {e}]")
                                }
                            }
                        } else {
                            // Normal DM — try E2E decryption
                            match crate::crypto::hive_integration::decrypt_dm_if_encrypted(
                                &config.api_url, &config.api_key, &my_name, my_agent_id, &sender, &text,
                            ).await {
                                Ok(plaintext) => plaintext,
                                Err(e) => { tracing::warn!("Hive decryption failed from {sender}: {e}"); text }
                            }
                        }
                    } else {
                        // Not JSON — try E2E decryption
                        match crate::crypto::hive_integration::decrypt_dm_if_encrypted(
                            &config.api_url, &config.api_key, &my_name, my_agent_id, &sender, &text,
                        ).await {
                            Ok(plaintext) => plaintext,
                            Err(e) => { tracing::warn!("Hive decryption failed from {sender}: {e}"); text }
                        }
                    };
                    let sanitized = decrypted.replace('\n', "\\n");
                    let trunc: String = sanitized.chars().take(80).collect();
                    tracing::info!("Hive from {sender} ({} bytes): {trunc}", sanitized.len());

                    // Try channel delivery first (MCP notification path)
                    let msg_id = format!("{:016x}", rand::random::<u64>());
                    let channel_delivered = if let Some(ws) = web_state.read().await.as_ref() {
                        ws.channel_events.send(crate::daemon::web::ChannelEvent::HiveDm {
                            sender: sender.clone(),
                            text: sanitized.clone(),
                            message_id: msg_id,
                        }).unwrap_or(0) > 0
                    } else {
                        false
                    };
                    // Broadcast to browser WS for overlay display (regardless of channel/PTY path)
                    if let Some(ws) = web_state.read().await.as_ref() {
                        let receivers = ws.events.receiver_count();
                        tracing::info!("Broadcasting Hive DM to browser events (receivers={receivers})");
                        ws.events.send(AgentEvent::Hive {
                            sender: sender.clone(),
                            text: sanitized.clone(),
                        });
                    } else {
                        tracing::warn!("No web_state for hive browser broadcast");
                    }

                    if channel_delivered {
                        tracing::info!("Hive from {sender} delivered via MCP channel");
                    } else {
                        tracing::warn!("Hive from {sender} dropped — no MCP channel subscriber");
                    }
                }
                _ => {
                    tracing::warn!("Unexpected hive event format: {data}");
                }
            }
        }
        "hive_room" => {
            // Parse and inject hive room message into PTY stdin.
            // Decrypt E2E encrypted room messages using locally stored room key.
            // Sanitize newlines — PTY interprets \n as Enter, fragmenting the message.
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::HiveRoom { sender, room_id, room_topic, text }) => {
                    // Check if this is a key_distribution message (room key setup)
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                        if parsed.get("msg_type").and_then(|v| v.as_str()) == Some("key_distribution") {
                            let my_name = agent_name(config);

                            // Creator verification (Vera review blocker):
                            // First key_distribution for a room establishes the creator.
                            // Subsequent ones MUST come from the same creator.
                            let stored_creator = crate::crypto::hive_integration::load_room_meta(&room_id);
                            if let Some(ref expected_creator) = stored_creator {
                                if expected_creator != &sender {
                                    tracing::error!(
                                        "⚠️ [HIVE-SECURITY] key_distribution for room {room_id} from \
                                         {sender}, but stored creator is {expected_creator}. \
                                         Rejecting — possible key injection attack."
                                    );
                                    return;
                                }
                            }
                            // If no stored creator, this is first contact — sender becomes creator
                            // (stored by receive_room_key → store_room_key).

                            let expires_at = parsed.get("expires_at").and_then(|v| v.as_str());
                            if let Some(keys) = parsed.get("keys").and_then(|v| v.as_object()) {
                                // Case-insensitive lookup: server lowercases keys, but safety net
                                // handles any edge case where casing is inconsistent.
                                let my_encrypted_key = keys.get(my_name.as_str())
                                    .or_else(|| keys.iter().find(|(k, _)| k.to_lowercase() == my_name).map(|(_, v)| v));
                                if let Some(my_encrypted_key) = my_encrypted_key {
                                    match crate::crypto::hive_integration::receive_room_key(
                                        &config.api_url,
                                        &config.api_key,
                                        &room_id,
                                        &sender,
                                        my_encrypted_key,
                                        expires_at,
                                    ).await {
                                        Ok(()) => {
                                            tracing::info!("Received room key for {room_id} from {sender}");
                                            // Store member list from key_distribution keys
                                            let members: Vec<String> = keys.keys().cloned().collect();
                                            crate::crypto::hive_integration::store_room_members(&room_id, &members);
                                        }
                                        Err(e) => tracing::error!("Failed to receive room key for {room_id}: {e}"),
                                    }
                                } else {
                                    tracing::debug!("key_distribution for {room_id} has no key for us ({my_name})");
                                }
                            }
                            // Don't inject key_distribution into PTY — it's infrastructure, not content
                            return;
                        }

                        // Handle key_destroy: any participant can destroy all room keys
                        if parsed.get("msg_type").and_then(|v| v.as_str()) == Some("key_destroy") {
                            let destroyed_by = parsed.get("destroyed_by").and_then(|v| v.as_str()).unwrap_or(&sender);
                            tracing::info!("Room {room_id} key destroyed by {destroyed_by}");

                            // Delete our local room key immediately
                            crate::crypto::hive_integration::delete_room_key(&room_id);

                            // ACK the destruction to the server so it can purge blobs
                            let ack_url = format!("{}/api/hive/rooms/{}/destroy-key", config.api_url, room_id);
                            if let Ok(client) = crate::http::build_client(std::time::Duration::from_secs(10)) {
                                let _ = client
                                    .post(&ack_url)
                                    .header("Authorization", format!("Bearer {}", config.api_key))
                                    .send()
                                    .await;
                            }

                            // Deliver key_destroy notice via channel or PTY fallback
                            let notice = format!("Key destroyed by {destroyed_by}. Room is now unreadable.");
                            let msg_id = format!("{:016x}", rand::random::<u64>());
                            let channel_delivered = if let Some(ws) = web_state.read().await.as_ref() {
                                ws.channel_events.send(crate::daemon::web::ChannelEvent::HiveRoom {
                                    sender: "system".to_string(),
                                    room_id: room_id.clone(),
                                    room_topic: room_topic.clone(),
                                    text: notice.clone(),
                                    message_id: msg_id,
                                }).unwrap_or(0) > 0
                            } else {
                                false
                            };
                            if !channel_delivered {
                                tracing::warn!(
                                    "key_destroy notice for room={room_id} dropped — no MCP channel subscriber"
                                );
                            }
                            return;
                        }
                    }

                    let decrypted = crate::crypto::hive_integration::decrypt_room_message_if_encrypted(
                        &room_id,
                        &text,
                    );
                    let sanitized = decrypted.replace('\n', "\\n");
                    tracing::info!("Hive room from {sender} in [{room_topic}] ({} bytes)", sanitized.len());

                    // Try channel delivery first
                    let msg_id = format!("{:016x}", rand::random::<u64>());
                    let channel_delivered = if let Some(ws) = web_state.read().await.as_ref() {
                        ws.channel_events.send(crate::daemon::web::ChannelEvent::HiveRoom {
                            sender: sender.clone(),
                            room_id: room_id.clone(),
                            room_topic: room_topic.clone(),
                            text: sanitized.clone(),
                            message_id: msg_id,
                        }).unwrap_or(0) > 0
                    } else {
                        false
                    };
                    // Broadcast to browser WS for overlay display
                    if let Some(ws) = web_state.read().await.as_ref() {
                        ws.events.send(AgentEvent::HiveRoom {
                            sender: sender.clone(),
                            room_id: room_id.clone(),
                            room_topic: room_topic.clone(),
                            text: sanitized.clone(),
                        });
                    }

                    if channel_delivered {
                        tracing::info!("Hive room from {sender} delivered via MCP channel");
                    } else {
                        tracing::warn!("Hive room from {sender} dropped — no MCP channel subscriber");
                    }
                }
                _ => {
                    tracing::warn!("Unexpected hive_room event format: {data}");
                }
            }
        }
        "pty_input" => {
            // Raw keyboard input — write directly to PTY stdin without modification
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::PtyInput { data: input }) => {
                    // Filter terminal focus report sequences — xterm.js sends these
                    // on tab focus/blur and they corrupt Claude's input
                    if input == "\x1b[I" || input == "\x1b[O" {
                        return;
                    }
                    if let Err(e) = inject(pty_writer, input.as_bytes()).await {
                        tracing::error!("Failed to inject PTY input: {e}");
                    }
                }
                _ => {
                    tracing::warn!("Unexpected pty_input event format: {data}");
                }
            }
        }
        "pty_resize" => {
            // Terminal resize from relay
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::PtyResize { rows, cols }) => {
                    tracing::debug!(cols, rows, "PtyResize via SSE");
                    if let Some(ref resizer) = pty_resizer {
                        match resizer.lock() {
                            Ok(master) => {
                                match master.resize(portable_pty::PtySize {
                                    rows, cols, pixel_width: 0, pixel_height: 0,
                                }) {
                                    Ok(()) => tracing::debug!(cols, rows, "PTY resized"),
                                    Err(e) => tracing::warn!(error = %e, "PTY resize failed"),
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "PTY resizer lock failed"),
                        }
                    } else {
                        tracing::debug!("no pty_resizer for SSE resize event");
                    }
                }
                _ => tracing::warn!("unexpected pty_resize format: {data}"),
            }
        }
        "update_available" => {
            // Server-pushed update: download new binary, verify, replace in place.
            // Does NOT restart — writes a marker file and the dashboard shows a
            // "Restart to apply" button.
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::UpdateAvailable { version, url, sha256 }) => {
                    tracing::info!(version = %version, url = %url, "update available");
                    tokio::spawn(async move {
                        if let Err(e) = download_and_stage_update(&version, &url, &sha256).await {
                            tracing::error!("Auto-update failed: {e}");
                        }
                    });
                }
                _ => {
                    tracing::warn!("Unexpected update_available event format: {data}");
                }
            }
        }
        "notification" | "session_updated" => {
            // Dashboard-only events — CLI doesn't need them
        }
        "webrtc_offer" => {
            // WebRTC SDP offer from browser via EC2.
            // gpu-worker offers are handled directly by the sidecar via its own SSE connection.
            // The daemon only handles offers targeted at itself (target=daemon).
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::WebRtcOffer { session_id, sdp, target, origin_url, browser_session_id }) => {
                    let config = config.clone();
                    let ws = web_state.clone();
                    let pr = peer_registry.clone();
                    tokio::spawn(async move {
                        if target == "gpu-worker" {
                            // Sidecar handles its own signaling via SSE — daemon is not in the path.
                            tracing::debug!(session_id, "Ignoring gpu-worker offer (handled by sidecar SSE)");
                        } else if target == "daemon" {
                            // Handle WebRTC offer for daemon — create peer, generate answer
                            tracing::info!(session_id, ?browser_session_id, "WebRTC offer for daemon — generating answer");
                            let web_state_guard = ws.read().await;
                            if let Some(ref web_state) = *web_state_guard {
                                match crate::daemon::rtc::handle_offer(web_state, &session_id, &sdp, &pr, browser_session_id).await {
                                    Ok(answer_sdp) => {
                                        let base = if origin_url.is_empty() { &config.api_url } else { &origin_url };
                                        let answer_url = format!(
                                            "{}/api/agents/{}/webrtc/answer",
                                            base, config.agent_id
                                        );
                                        let resp = reqwest::Client::new()
                                            .post(&answer_url)
                                            .bearer_auth(&config.api_key)
                                            .json(&serde_json::json!({
                                                "session_id": session_id,
                                                "sdp": answer_sdp,
                                            }))
                                            .send()
                                            .await;
                                        match resp {
                                            Ok(r) if r.status().is_success() => {
                                                tracing::info!(session_id, "WebRTC answer delivered to EC2");
                                            }
                                            Ok(r) => {
                                                tracing::warn!(session_id, status = %r.status(), "EC2 rejected WebRTC answer");
                                            }
                                            Err(e) => {
                                                tracing::warn!(session_id, "Failed to deliver WebRTC answer: {e}");
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(session_id, "WebRTC handle_offer failed: {e}");
                                    }
                                }
                            } else {
                                tracing::warn!(session_id, "WebRTC offer received but WebState not yet initialized");
                            }
                        }
                    });
                }
                _ => tracing::warn!("Malformed webrtc_offer event"),
            }
        }
        "webrtc_ice" => {
            // WebRTC trickle ICE candidate — forward to the peer connection
            match serde_json::from_str::<AgentEvent>(data) {
                Ok(AgentEvent::WebRtcIce { session_id, candidate, mid, target }) => {
                    if target == "daemon" {
                        let reg = peer_registry.clone();
                        tokio::spawn(async move {
                            crate::daemon::rtc::add_ice_candidate(&reg, &session_id, &candidate, &mid).await;
                        });
                    }
                    // GPU worker ICE candidates would be forwarded to the sidecar
                    // (not implemented yet — sidecar uses full ICE)
                }
                _ => {}
            }
        }
        "webrtc_answer" => {
            // WebRTC SDP answer — shouldn't arrive at daemon (daemon sends answers, not receives)
            tracing::debug!("Unexpected webrtc_answer event at daemon");
        }
        _ => {
            // Ignore unknown events — forward compatible
            tracing::debug!("Ignoring unknown SSE event type: {event_type}");
        }
    }
}

/// Write data to PTY stdin atomically (single lock, single write+flush).
/// PTY buffers are typically 4096+ bytes — our injections are well under that.
/// The old 10-byte chunked approach dropped the lock between chunks, allowing
/// other events (like a delayed Enter) to interleave and corrupt the injection.
async fn inject(
    pty_writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    data: &[u8],
) -> anyhow::Result<()> {
    tracing::trace!(bytes = data.len(), "inject: writing to PTY");
    let mut writer = pty_writer.lock().await;
    writer.write_all(data)?;
    writer.flush()?;
    Ok(())
}


// ── Hive Polling Fallback ──────────────────────────────────────────────

/// Background task that polls for undelivered hive messages every 30s.
///
/// This is a safety net for cross-server scenarios: when the sender's server
/// stores a message in the shared DB but can't deliver via SSE (because the
/// target agent's channel is on the sibling server), polling catches it.
///
/// Uses `since` parameter to only fetch messages newer than the last seen ID.
/// Dedup with SSE is handled by the shared HiveDedup fingerprint tracker.
async fn hive_poll_loop(
    config: &Config,
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    hive_dedup: HiveDedup,
    web_state: &LazyWebState,
) {
    let client = match crate::http::build_client(Duration::from_secs(15)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to create HTTP client for hive poll: {e}");
            return;
        }
    };

    let messages_url = format!("{}/api/hive/messages", config.api_url);
    let mut last_seen_id: i64 = 0;

    // Initial delay — let SSE connect first
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Seed last_seen_id from initial fetch (don't inject old messages)
    match client
        .get(&messages_url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .query(&[("limit", "1")])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
                    if let Some(first) = msgs.first() {
                        last_seen_id = first.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                        tracing::info!(last_seen_id, "Hive poll seeded from latest message");
                    }
                }
            }
        }
        Ok(resp) => {
            tracing::debug!(status = resp.status().as_u16(), "Hive poll seed request failed");
        }
        Err(e) => {
            tracing::debug!(error = %e, "Hive poll seed request failed");
        }
    }

    let mut interval = tokio::time::interval(HIVE_POLL_INTERVAL);
    interval.tick().await; // skip immediate tick

    loop {
        interval.tick().await;

        let resp = match client
            .get(&messages_url)
            .header("Authorization", format!("Bearer {}", config.api_key))
            .query(&[("since", &last_seen_id.to_string()), ("limit", &"50".to_string())])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "Hive poll request failed");
                continue;
            }
        };

        if !resp.status().is_success() {
            tracing::debug!(status = resp.status().as_u16(), "Hive poll non-success");
            continue;
        }

        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(error = %e, "Hive poll JSON parse failed");
                continue;
            }
        };

        let messages = match body.get("messages").and_then(|v| v.as_array()) {
            Some(m) => m,
            None => continue,
        };

        let my_name = agent_name(config);
        let my_agent_id = &config.agent_id;
        let mut injected = 0;

        for msg in messages {
            let msg_id = msg.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            if msg_id <= last_seen_id {
                continue;
            }

            // Only inject messages addressed to us
            let to_agent = msg.get("to_agent").and_then(|v| v.as_str()).unwrap_or("");
            if !to_agent.is_empty() && to_agent != my_name {
                last_seen_id = last_seen_id.max(msg_id);
                continue;
            }

            let from_agent = msg.get("from_agent").and_then(|v| v.as_str()).unwrap_or("unknown");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");

            if content.is_empty() {
                last_seen_id = last_seen_id.max(msg_id);
                continue;
            }

            // Dedup: skip if SSE already delivered this message
            if hive_dedup.check_and_mark(from_agent, content).await {
                tracing::debug!(msg_id, from_agent, "Hive poll: skipping (already delivered via SSE)");
                last_seen_id = last_seen_id.max(msg_id);
                continue;
            }

            // Decrypt if needed
            let decrypted = match crate::crypto::hive_integration::decrypt_dm_if_encrypted(
                &config.api_url, &config.api_key, &my_name, my_agent_id, from_agent, content,
            ).await {
                Ok(plaintext) => plaintext,
                Err(e) => {
                    tracing::warn!(msg_id, from_agent, error = %e, "Hive poll: decryption failed");
                    content.to_string()
                }
            };

            let sanitized = decrypted.replace('\n', "\\n");
            tracing::info!(msg_id, from_agent, bytes = sanitized.len(), "Hive poll: delivering missed message");

            // Try channel delivery first
            let ch_msg_id = format!("{:016x}", rand::random::<u64>());
            let channel_delivered = if let Some(ws) = web_state.read().await.as_ref() {
                ws.channel_events.send(crate::daemon::web::ChannelEvent::HiveDm {
                    sender: from_agent.to_string(),
                    text: sanitized.clone(),
                    message_id: ch_msg_id,
                }).unwrap_or(0) > 0
            } else {
                false
            };
            if channel_delivered {
                injected += 1;
            } else {
                tracing::warn!(msg_id, from = %from_agent, "Hive poll: message dropped — no MCP channel subscriber");
            }

            last_seen_id = last_seen_id.max(msg_id);
        }

        if injected > 0 {
            tracing::info!(injected, last_seen_id, "Hive poll: injected missed messages");
        }
    }
}

// ── Auto-update ────────────────────────────────────────────────────────

/// Path to the update-ready marker file.
fn update_ready_path() -> std::path::PathBuf {
    crate::config::Config::config_dir().join("update-ready.json")
}

/// Download, verify, and stage a new binary.
///
/// On success, writes `~/.b3/update-ready.json` so the dashboard can
/// show a "Restart to apply" notification. Does NOT restart the daemon —
/// that requires user consent via the dashboard button.
async fn download_and_stage_update(version: &str, url: &str, sha256: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    let current = env!("CARGO_PKG_VERSION");
    if version == current {
        tracing::debug!(version = current, "already running this version, skipping update");
        return Ok(());
    }

    tracing::info!(version, url, "downloading update");

    // Download to temp file
    let client = http::build_client(Duration::from_secs(60))?;

    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed: HTTP {}", resp.status());
    }

    let bytes = resp.bytes().await?;
    let tmpfile = std::env::temp_dir().join(format!("b3-update-{}", std::process::id()));
    let mut file = std::fs::File::create(&tmpfile)?;
    file.write_all(&bytes)?;
    file.flush()?;
    drop(file);

    // Verify SHA256
    if !sha256.is_empty() {
        let output = std::process::Command::new("sha256sum")
            .arg(&tmpfile)
            .output()
            .or_else(|_| {
                std::process::Command::new("shasum")
                    .args(["-a", "256"])
                    .arg(&tmpfile)
                    .output()
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let actual = stdout.split_whitespace().next().unwrap_or("");
        if actual != sha256 {
            let _ = std::fs::remove_file(&tmpfile);
            anyhow::bail!("Checksum mismatch: expected {sha256}, got {actual}");
        }
        tracing::debug!("checksum verified");
    }

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmpfile, std::fs::Permissions::from_mode(0o755))?;
    }

    // Replace running binary (unlink + rename)
    let current_exe = std::env::current_exe()?;
    let _ = std::fs::remove_file(&current_exe);
    if std::fs::rename(&tmpfile, &current_exe).is_err() {
        std::fs::copy(&tmpfile, &current_exe)?;
        let _ = std::fs::remove_file(&tmpfile);
    }

    // Write marker file for dashboard notification
    let marker = serde_json::json!({
        "version": version,
        "ready": true,
        "updated_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    let _ = std::fs::write(update_ready_path(), serde_json::to_string_pretty(&marker)?);

    tracing::info!(version, "update staged and ready, awaiting user restart");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shared buffer that implements Write and lets us read back what was written.
    #[derive(Clone)]
    struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(Arc::new(std::sync::Mutex::new(Vec::new())))
        }
        fn contents(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
        fn contents_str(&self) -> String {
            String::from_utf8_lossy(&self.contents()).into_owned()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn shared_writer() -> (SharedBuf, Arc<Mutex<Box<dyn Write + Send>>>) {
        let buf = SharedBuf::new();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(buf.clone())));
        (buf, writer)
    }

    fn test_lazy_ws() -> LazyWebState {
        Arc::new(tokio::sync::RwLock::new(None))
    }

    fn test_peer_reg() -> PeerRegistry {
        crate::daemon::rtc::new_peer_registry()
    }

    fn test_config() -> Config {
        Config {
            agent_id: "test-id".to_string(),
            agent_email: "test@hey-code.ai".to_string(),
            api_key: "test-key".to_string(),
            api_url: "https://test.example.com".to_string(),
            wg_address: "10.44.0.1".to_string(),
            relay_endpoint: "127.0.0.1:51820".to_string(),
            relay_public_key: "AAAA".to_string(),
            push_interval_ms: 100,
            web_url: "https://test.example.com/a/test".to_string(),
            b3_version: "0.0.0".to_string(),
            servers: vec![],
        }
    }

    #[tokio::test]
    async fn test_handle_transcription_event() {
        // Transcription no longer falls back to PTY injection — channel delivery only.
        // With no web_state (channel), the message is dropped (warning logged).
        let (buf, writer) = shared_writer();
        let data = r#"{"type":"transcription","text":"Hello Claude"}"#;
        handle_event("transcription", data, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        assert!(buf.contents().is_empty(), "Transcription must not fall back to PTY injection");
    }

    #[tokio::test]
    async fn test_handle_hive_event() {
        // Hive DM no longer falls back to PTY injection — channel delivery only.
        // With no web_state (channel), the message is dropped (warning logged).
        let (buf, writer) = shared_writer();
        let data = r#"{"type":"hive","sender":"cloister","text":"run the tests"}"#;
        handle_event("hive", data, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        assert!(buf.contents().is_empty(), "Hive DM must not fall back to PTY injection");
    }

    #[tokio::test]
    async fn test_handle_unknown_event_is_ignored() {
        let (buf, writer) = shared_writer();
        handle_event("unknown_type", "{}", &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        assert!(buf.contents().is_empty(), "Unknown events should not inject anything");
    }

    #[tokio::test]
    async fn test_handle_malformed_transcription() {
        let (buf, writer) = shared_writer();
        // Wrong JSON structure — missing "text" field
        handle_event("transcription", r#"{"type":"transcription"}"#, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        assert!(buf.contents().is_empty(), "Malformed event should not inject anything");
    }

    #[tokio::test]
    async fn test_handle_malformed_hive() {
        let (buf, writer) = shared_writer();
        // Wrong JSON — has "text" but missing "sender"
        handle_event("hive", r#"{"type":"hive","text":"hello"}"#, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        assert!(buf.contents().is_empty(), "Malformed hive should not inject anything");
    }

    #[tokio::test]
    async fn test_inject_writes_bytes() {
        let (buf, writer) = shared_writer();
        inject(&writer, b"test data").await.unwrap();
        assert_eq!(buf.contents(), b"test data");
    }

    #[tokio::test]
    async fn test_multiple_events_accumulate() {
        // Multiple transcription events without a channel subscriber — both dropped, no PTY writes.
        let (buf, writer) = shared_writer();
        let t1 = r#"{"type":"transcription","text":"first"}"#;
        let t2 = r#"{"type":"transcription","text":"second"}"#;
        handle_event("transcription", t1, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        handle_event("transcription", t2, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        assert!(buf.contents().is_empty(), "Transcription events must not fall back to PTY injection");
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use b3_common::backoff::Backoff;

    fn test_lazy_ws() -> LazyWebState {
        Arc::new(tokio::sync::RwLock::new(None))
    }

    fn test_peer_reg() -> PeerRegistry {
        crate::daemon::rtc::new_peer_registry()
    }

    fn test_config() -> Config {
        Config {
            agent_id: "test-id".to_string(),
            agent_email: "test@hey-code.ai".to_string(),
            api_key: "test-key".to_string(),
            api_url: "https://test.example.com".to_string(),
            wg_address: "10.44.0.1".to_string(),
            relay_endpoint: "127.0.0.1:51820".to_string(),
            relay_public_key: "AAAA".to_string(),
            push_interval_ms: 100,
            web_url: "https://test.example.com/a/test".to_string(),
            b3_version: "0.0.0".to_string(),
            servers: vec![],
        }
    }

    /// Malformed JSON for every event type should not panic or inject garbage.
    #[tokio::test]
    async fn malformed_json_all_event_types_safe() {
        let buf = SharedBuf::new();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(buf.clone())));

        let event_types = [
            "transcription", "hive", "hive_room", "pty_input",
            "pty_resize", "update_available", "tts", "led",
        ];
        let bad_jsons = [
            "not json at all",
            "{}",
            r#"{"type":"wrong"}"#,
            "",
            "null",
            r#"{"text": 42}"#,
        ];

        for event_type in &event_types {
            for bad_json in &bad_jsons {
                handle_event(event_type, bad_json, &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
            }
        }
        // None of the malformed events should have injected anything
        assert!(
            buf.contents().is_empty(),
            "Malformed JSON should never inject data: got {:?}",
            buf.contents_str()
        );
    }

    /// Unknown event types should be silently ignored (forward compatibility).
    #[tokio::test]
    async fn unknown_event_types_ignored() {
        let buf = SharedBuf::new();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(buf.clone())));

        for event_type in &["future_event", "v2_migration", "🎵", ""] {
            handle_event(event_type, "{}", &writer, &None, &test_config(), &HiveDedup::new(), &test_lazy_ws(), &test_peer_reg()).await;
        }
        assert!(buf.contents().is_empty());
    }

    /// Backoff must respect max even after many failures.
    #[test]
    fn puller_backoff_bounded() {
        let max = Duration::from_secs(30);
        let mut backoff = Backoff::new(Duration::from_secs(1), max);
        for _ in 0..100 {
            assert!(backoff.next_delay() <= max);
        }
    }

    /// Backoff reset truly resets — prevents sticky max from previous session.
    #[test]
    fn backoff_reset_clears_state() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        for _ in 0..50 {
            backoff.next_delay();
        }
        assert_eq!(backoff.consecutive_failures, 50);
        backoff.reset();
        assert_eq!(backoff.consecutive_failures, 0);
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    /// A shared buffer for tests.
    #[derive(Clone)]
    struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(Arc::new(std::sync::Mutex::new(Vec::new())))
        }
        fn contents(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
        fn contents_str(&self) -> String {
            String::from_utf8_lossy(&self.contents()).into_owned()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
