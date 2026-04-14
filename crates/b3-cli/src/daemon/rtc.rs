//! WebRTC data channel handler for the daemon.
//!
//! Accepts SDP offers relayed from EC2 via SSE, creates peer connections,
//! and bridges data channel messages to the existing web handler infrastructure.
//!
//! Three data channels:
//! - "control" (ordered, reliable) — JSON RPC + control messages
//! - "terminal" (ordered, reliable) — raw PTY bytes, both directions
//! - "audio" (ordered, reliable) — TTS WAV chunks
//!
//! Compiled only when the `webrtc-experimental` feature is enabled.
//! Without the feature, this module provides no-op stubs with identical
//! public signatures so the rest of the codebase compiles unchanged.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// ICE candidate to inject into a peer.
pub struct IceCandidate {
    pub candidate: String,
    pub mid: String,
}

/// Entry for an active WebRTC peer: ICE injection channel + task handle for abort.
pub struct PeerEntry {
    pub ice_tx: tokio::sync::mpsc::Sender<IceCandidate>,
    pub task: tokio::task::JoinHandle<()>,
    /// Browser session that owns this peer — used to abort old peer on reconnect.
    pub browser_session_id: Option<u64>,
}

/// Registry of active WebRTC sessions.
/// Stores ICE sender + JoinHandle so we can abort stale bridge tasks on cleanup.
pub type PeerRegistry = Arc<Mutex<HashMap<String, PeerEntry>>>;

/// Create a new empty peer registry.
pub fn new_peer_registry() -> PeerRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Add a trickle ICE candidate — sends to the bridge task via channel (non-blocking).
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
pub async fn add_ice_candidate(
    registry: &PeerRegistry,
    session_id: &str,
    candidate: &str,
    mid: &str,
) {
    let peers = registry.lock().await;
    if let Some(entry) = peers.get(session_id) {
        let _ = entry.ice_tx.send(IceCandidate {
            candidate: candidate.to_string(),
            mid: mid.to_string(),
        }).await;
    }
}

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(not(feature = "webrtc-experimental"))]
pub async fn add_ice_candidate(
    _registry: &PeerRegistry,
    _session_id: &str,
    _candidate: &str,
    _mid: &str,
) {
    // WebRTC not compiled in — no-op
}

/// Handle a WebRTC offer from a browser (relayed via EC2 SSE).
///
/// Creates a peer connection, generates an answer, and spawns a task
/// to bridge data channel messages to the existing web handlers.
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
pub async fn handle_offer(
    web_state: &super::web::WebState,
    session_id: &str,
    sdp: &str,
    peer_registry: &PeerRegistry,
    browser_session_id: Option<u64>,
) -> anyhow::Result<String> {
    _handle_offer_impl(web_state, session_id, sdp, peer_registry, browser_session_id).await
}

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(not(feature = "webrtc-experimental"))]
pub async fn handle_offer(
    _web_state: &super::web::WebState,
    _session_id: &str,
    _sdp: &str,
    _peer_registry: &PeerRegistry,
    _browser_session_id: Option<u64>,
) -> anyhow::Result<String> {
    anyhow::bail!("WebRTC not compiled in — build with --features webrtc-experimental")
}

// ── Full WebRTC implementation (feature-gated) ────────────────────────────────

#[cfg(feature = "webrtc-experimental")]
use b3_common::AgentEvent;
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::channel::ChannelMessage;
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::peer::{B3Peer, PeerConfig, PeerEvent};
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::signaling::IceServer;
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::ChannelSender;
#[cfg(feature = "webrtc-experimental")]
use tokio::sync::broadcast;
#[cfg(feature = "webrtc-experimental")]
use tracing::{info, warn};
#[cfg(feature = "webrtc-experimental")]
use crate::config::Config;

#[cfg(feature = "webrtc-experimental")]
async fn _handle_offer_impl(
    web_state: &super::web::WebState,
    session_id: &str,
    sdp: &str,
    peer_registry: &PeerRegistry,
    browser_session_id: Option<u64>,
) -> anyhow::Result<String> {
    info!(session_id, ?browser_session_id, "Handling WebRTC offer for daemon");

    // Early rejection: if browser_session_id is provided but no matching session
    // exists, reject BEFORE creating a peer. Prevents wasting network resources
    // on stale browsers that will be rejected anyway.
    if let Some(bsid) = browser_session_id {
        if web_state.session_manager.get(bsid).await.is_none() {
            warn!(session_id, browser_session_id = bsid,
                  "Rejecting RTC offer — session not found (early reject, no peer created)");
            anyhow::bail!("session not found");
        }
    }

    // One RTC peer per browser session. If a new offer arrives from the same
    // browser (same browser_session_id), abort the old peer first. Different
    // browsers keep their own peers.
    {
        let mut peers = peer_registry.lock().await;
        // Remove finished tasks (natural cleanup)
        peers.retain(|sid, entry| {
            if entry.task.is_finished() {
                info!(session_id = sid, "Removing finished peer task");
                false
            } else {
                true
            }
        });
        // Abort old peer for the SAME browser session
        if let Some(bsid) = browser_session_id {
            let old_keys: Vec<String> = peers.iter()
                .filter(|(_, entry)| entry.browser_session_id == Some(bsid))
                .map(|(k, _)| k.clone())
                .collect();
            for key in old_keys {
                if let Some(old_entry) = peers.remove(&key) {
                    info!(session_id, browser_session_id = bsid,
                          old_session = key, "Aborting old RTC peer (new offer for same browser)");
                    old_entry.task.abort();
                }
            }
        }
    }

    let stun_url =
        std::env::var("STUN_URL").unwrap_or_else(|_| "stun:stun.l.google.com:19302".into());
    let turn_host = std::env::var("TURN_HOST").unwrap_or_else(|_| "turn.babel3.com".into());
    let turn_secret = std::env::var("TURN_SECRET").unwrap_or_default();

    let mut ice_servers = vec![IceServer {
        urls: vec![stun_url],
        username: None,
        credential: None,
    }];

    if !turn_secret.is_empty() {
        let (username, credential) =
            b3_webrtc::signaling::generate_turn_credentials(&turn_secret, 3600);
        ice_servers.push(IceServer {
            urls: vec![format!("turn:{turn_host}:3478?transport=udp")],
            username: Some(username.clone()),
            credential: Some(credential.clone()),
        });
        ice_servers.push(IceServer {
            urls: vec![format!("turns:{turn_host}:5349?transport=tcp")],
            username: Some(username),
            credential: Some(credential),
        });
        info!(session_id, "TURN configured for daemon: {turn_host}");
    } else {
        warn!(session_id, "No TURN_SECRET — daemon has STUN only");
    }

    let config = PeerConfig {
        ice_servers,
        force_relay: false,
    };

    let mut peer = B3Peer::new(config)?;

    // Pre-create channels
    let (_control_sender, _control_receiver, _control_open) =
        peer.create_channel("control")?;
    let (_terminal_sender, _terminal_receiver, _terminal_open) =
        peer.create_channel("terminal")?;

    // Set remote description (browser's offer)
    peer.set_remote_description(sdp, "offer")?;

    // Collect answer SDP
    let mut answer_sdp = None;
    let timeout = tokio::time::Duration::from_secs(10);
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, peer.next_event()).await {
            Ok(Some(PeerEvent::LocalDescription { sdp, sdp_type })) => {
                if sdp_type == "answer" {
                    answer_sdp = Some(sdp);
                    // Don't break — wait for GatheringComplete to get full candidates
                }
            }
            Ok(Some(PeerEvent::LocalCandidate { .. })) => {
                // ICE candidate gathered — will be in final local_description()
            }
            Ok(Some(PeerEvent::GatheringComplete)) => {
                if answer_sdp.is_some() {
                    // Get final SDP with all gathered candidates
                    if let Some(final_sdp) = peer.local_description() {
                        answer_sdp = Some(final_sdp);
                    }
                    break;
                }
            }
            Ok(Some(PeerEvent::ConnectionStateChange(state))) => {
                info!(session_id, state, "WebRTC connection state");
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }

    let answer = answer_sdp.ok_or_else(|| anyhow::anyhow!("answer generation timeout"))?;

    // Create ICE candidate injection channel
    let (ice_tx, ice_rx) = tokio::sync::mpsc::channel::<IceCandidate>(32);

    // Spawn bridge task — peer is OWNED by the task (no mutex needed)
    let web_state = web_state.clone();
    let sid = session_id.to_string();
    let registry = peer_registry.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = rtc_bridge_task_inner(peer, ice_rx, &web_state, &sid, browser_session_id).await {
            warn!(session_id = sid, "WebRTC bridge task ended: {e}");
        }
        // Remove from registry when done
        registry.lock().await.remove(&sid);
    });

    // Store both ICE sender and task handle — so cleanup can abort the task
    let peer_count = {
        let mut reg = peer_registry.lock().await;
        reg.insert(session_id.to_string(), PeerEntry { ice_tx, task, browser_session_id });
        reg.len()
    };

    // Log peer count + process RSS for memory leak detection
    let rss_kb = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map(|pages| pages * 4) // pages to KB
        .unwrap_or(0);
    info!(
        session_id,
        peer_count,
        rss_mb = rss_kb / 1024,
        "WebRTC answer generated for daemon"
    );
    Ok(answer)
}

/// Bridge task: processes peer events, data channel messages, and ICE candidates.
/// Owns the peer exclusively — no mutex contention.
/// ICE candidates arrive via mpsc channel from the puller.
#[cfg(feature = "webrtc-experimental")]
async fn rtc_bridge_task_inner(
    mut peer: B3Peer,
    mut ice_rx: tokio::sync::mpsc::Receiver<IceCandidate>,
    web_state: &super::web::WebState,
    session_id: &str,
    browser_session_id: Option<u64>,
) -> anyhow::Result<()> {
    let rss_kb = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map(|pages| pages * 4)
        .unwrap_or(0);
    info!(session_id, rss_mb = rss_kb / 1024, "WebRTC bridge task started");

    // Look up the per-browser session to register RTC on.
    // Use browser_session_id from the WebRTC offer for precise association.
    // Fall back to most_recent() for backward compat (old browsers without session_id).
    let client_session = if let Some(sid) = browser_session_id {
        match web_state.session_manager.get(sid).await {
            Some(s) => {
                info!(session_id, browser_session_id = sid, "RTC associated with session (by session_id)");
                s
            }
            None => {
                // Do NOT fall back to most_recent — it cross-wires a stale browser's
                // broken RTC to an active browser's session. When the stale peer dies,
                // it removes the RTC transport from the wrong session.
                warn!(session_id, browser_session_id = sid,
                      "session_id not found in session manager — rejecting RTC offer");
                return Ok(());
            }
        }
    } else {
        // No browser_session_id at all — browser too old for RTC association.
        // Reject rather than guess with most_recent (cross-wiring risk).
        warn!(session_id, "No browser_session_id in RTC offer — rejecting");
        return Ok(());
    };

    // Track all child task handles so we can abort them when this bridge task ends.
    // Without this, child tasks (event_relay, terminal_output, control handler)
    // become orphans holding broadcast subscribers and ChannelSenders indefinitely.
    let mut child_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // No hard deadline on bridge lifetime. Cleanup relies on ICE state machine:
    // - "disconnected" → 30s grace period for recovery
    // - "closed" / "failed" → immediate exit
    // Heuristic timeouts were removed — they killed healthy connections.
    let mut disconnect_deadline: Option<tokio::time::Instant> = None;

    loop {
        // Only arm the timeout when ICE is in "disconnected" state (30s grace)
        let disconnect_sleep = async {
            match disconnect_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await, // never fires
            }
        };

        tokio::select! {
            // ICE disconnect grace period — 30s to recover before closing
            _ = disconnect_sleep => {
                warn!(session_id, "Peer disconnected for 30s without recovery — closing bridge");
                break;
            }
            // Peer events (connection state, incoming channels)
            event = peer.next_event() => {
        match event {
            Some(PeerEvent::ConnectionStateChange(state)) => {
                info!(session_id, state, child_tasks = child_tasks.len(), "WebRTC state change");
                if state == "closed" || state == "failed" {
                    client_session.multi.lock().await.remove_transport("rtc");
                    client_session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                if state == "disconnected" {
                    client_session.multi.lock().await.remove_transport("rtc");
                    client_session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
                    disconnect_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
                }
                if state == "connected" {
                    // Recovered from disconnect
                    disconnect_deadline = None;
                }
            }
            Some(PeerEvent::IncomingChannel {
                label,
                sender,
                mut receiver,
            }) => {
                info!(session_id, label, "Incoming data channel");

                match label.as_str() {
                    "control" => {
                        let ws = web_state.clone();
                        let control_sender = sender.clone();

                        // Mark RTC as active — WS handler skips TTS events
                        client_session.rtc_active.store(true, std::sync::atomic::Ordering::Relaxed);
                        info!(session_id, "RTC control channel active — WS will skip TTS events");

                        // Subscribe to daemon events and forward to browser
                        let event_sender = sender.clone();
                        let event_rx = ws.events.subscribe();
                        let rtc_active_flag = client_session.rtc_active.clone();
                        child_tasks.push(tokio::spawn(async move {
                            event_relay_task(event_rx, event_sender).await;
                            // Channel died — clear rtc_active so WS resumes TTS delivery
                            rtc_active_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                            info!("Event relay ended — WS resumes TTS events");
                        }));

                        // Process incoming control messages
                        child_tasks.push(tokio::spawn(async move {
                            let mut authenticated = ws.daemon_password_hash.is_none();

                            // If password required, send auth challenge
                            if !authenticated {
                                let _ = control_sender
                                    .send_json(serde_json::json!({"type": "auth_required"}))
                                    .await;
                            }

                            while let Some(msg) = receiver.recv().await {
                                match msg {
                                    ChannelMessage::Json(value) => {
                                        let response = handle_control_message(
                                            &value,
                                            &ws,
                                            &mut authenticated,
                                        )
                                        .await;
                                        if let Some(resp) = response {
                                            if let Err(e) =
                                                control_sender.send_json(resp).await
                                            {
                                                warn!("control send error: {e}");
                                                break;
                                            }
                                        }
                                    }
                                    ChannelMessage::Ping => {
                                        let _ =
                                            control_sender.send(ChannelMessage::Pong).await;
                                    }
                                    _ => {}
                                }
                            }
                        }));
                    }
                    "terminal" => {
                        let ws = web_state.clone();
                        let terminal_sender = sender.clone();

                        // Register RTC terminal sink with shared MultiTransport.
                        // Priority 0 = preferred over WS (priority 1).
                        // The WS event loop handles all events — MultiTransport routes
                        // frames to the best available transport automatically.
                        let sender_for_multi = terminal_sender.clone();
                        let multi_for_remove = client_session.multi.clone();
                        client_session.multi.lock().await.add_transport("rtc", 0, move |data: &[u8]| {
                            let s = sender_for_multi.clone();
                            let m = multi_for_remove.clone();
                            let d = data.to_vec();
                            tokio::spawn(async move {
                                if s.send_binary(d).await.is_err() {
                                    // Channel dead — remove zombie sink so WS takes over
                                    m.lock().await.remove_transport("rtc");
                                    warn!("RTC terminal send failed — removed zombie sink, WS will take over");
                                }
                            });
                        });
                        // Send Resume to browser — tells it our inbound state so it can
                        // detect seq mismatch and reset. Same as GPU relay (PRs #195, #196).
                        // Bidirectional reset (PRs #193-195) makes this safe — browser's
                        // _handleResume only resets on unreplayable gap.
                        client_session.multi.lock().await.send_resume();
                        client_session.rtc_active.store(true, std::sync::atomic::Ordering::Relaxed);
                        info!(session_id, "RTC terminal registered with MultiTransport (priority 0)");

                        // Terminal input: browser keystrokes → PTY
                        // Also handles reliable ACKs/Resume from browser → MultiTransport
                        let events = ws.events.clone();
                        let multi_for_input = client_session.multi.clone();
                        child_tasks.push(tokio::spawn(async move {
                            while let Some(msg) = receiver.recv().await {
                                if let ChannelMessage::Binary(data) = msg {
                                    // Check for reliable frame (ACK/Resume from browser)
                                    if b3_reliable::is_reliable(&data) {
                                        multi_for_input.lock().await.receive(&data);
                                        continue;
                                    }
                                    // Otherwise: PTY keyboard input
                                    let text = String::from_utf8_lossy(&data).to_string();
                                    if !text.is_empty() {
                                        let _ = events.send(AgentEvent::PtyInput {
                                            data: text,
                                        });
                                    }
                                }
                            }
                        }));

                        info!(session_id, "Terminal channel wired to PTY");
                    }
                    other => {
                        warn!(session_id, channel = other, "Unknown data channel");
                    }
                }
            }
            Some(_) => continue,
            None => break,
        }
            }
            // ICE candidates from the puller (trickle ICE)
            ice = ice_rx.recv() => {
                match ice {
                    Some(cand) => {
                        if let Err(e) = peer.add_remote_candidate(&cand.candidate, &cand.mid) {
                            warn!(session_id, "Failed to add ICE candidate: {e}");
                        } else {
                            info!(session_id, "Added trickle ICE candidate");
                        }
                    }
                    None => {} // Channel closed
                }
            }
        }
    }

    // Abort all child tasks — without this, event_relay, terminal_output, and
    // control handler tasks become orphans holding broadcast subscribers and
    // ChannelSenders to dead peers. Each orphan subscribes to all daemon events
    // and accumulates backpressure references. 9 peers × 4 orphan tasks = 36
    // orphan tasks holding references that prevent memory from being freed.
    let child_count = child_tasks.len();
    for task in child_tasks {
        task.abort();
    }
    info!(session_id, child_count, "Aborted child tasks");

    // Remove RTC transport sink — WS becomes the active transport
    client_session.multi.lock().await.remove_transport("rtc");
    client_session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
    info!(session_id, "RTC transport removed — WS is now active");

    let rss_kb = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map(|pages| pages * 4)
        .unwrap_or(0);
    info!(session_id, rss_mb = rss_kb / 1024, "WebRTC bridge task ended");
    Ok(())
}

/// Relay daemon events to browser via control data channel.
/// Mirrors the WS event relay in web.rs.
#[cfg(feature = "webrtc-experimental")]
async fn event_relay_task(
    mut event_rx: broadcast::Receiver<AgentEvent>,
    sender: ChannelSender,
) {
    let mut consecutive_lags: u32 = 0;
    loop {
        match event_rx.recv().await {
            Ok(event) => {
                // Convert AgentEvent to JSON push event
                let json = match &event {
                    AgentEvent::TtsStream {
                        msg_id,
                        text,
                        original_text,
                        voice,
                        emotion,
                        replying_to,
                        split_chars,
                        min_chunk_chars,
                        conditionals_b64,
                    } => serde_json::json!({
                        "event": "tts_stream",
                        "msg_id": msg_id,
                        "text": text,
                        "original_text": original_text,
                        "voice": voice,
                        "emotion": emotion,
                        "replying_to": replying_to,
                        "split_chars": split_chars,
                        "min_chunk_chars": min_chunk_chars,
                        "conditionals_b64": conditionals_b64,
                    }),
                    AgentEvent::TtsAudioData {
                        msg_id,
                        chunk,
                        total,
                        audio_b64,
                        duration_sec,
                        generation_sec,
                    } => serde_json::json!({
                        "event": "tts_audio_data",
                        "msg_id": msg_id,
                        "chunk": chunk,
                        "total": total,
                        "audio_b64": audio_b64,
                        "duration_sec": duration_sec,
                        "generation_sec": generation_sec,
                    }),
                    AgentEvent::TtsGenerating {
                        msg_id,
                        chunk,
                        total,
                    } => serde_json::json!({
                        "event": "tts_generating",
                        "msg_id": msg_id,
                        "chunk": chunk,
                        "total": total,
                    }),
                    AgentEvent::Led { emotion } => serde_json::json!({
                        "event": "led",
                        "emotion": emotion,
                    }),
                    AgentEvent::Transcription { text } => serde_json::json!({
                        "event": "transcription",
                        "text": text,
                    }),
                    AgentEvent::Hive { sender: s, text } => serde_json::json!({
                        "event": "hive",
                        "sender": s,
                        "text": text,
                    }),
                    AgentEvent::HiveRoom {
                        sender: s,
                        room_id,
                        room_topic,
                        text,
                    } => serde_json::json!({
                        "event": "hive_room",
                        "sender": s,
                        "room_id": room_id,
                        "room_topic": room_topic,
                        "text": text,
                    }),
                    AgentEvent::BrowserEval { eval_id, code, session_id: target } => {
                        // Include target_session_id so browser JS can filter
                        let mut msg = serde_json::json!({
                            "event": "browser_eval",
                            "eval_id": eval_id,
                            "code": code,
                        });
                        if let Some(tid) = target {
                            msg["target_session_id"] = serde_json::json!(tid);
                        }
                        msg
                    }
                    AgentEvent::Notification {
                        title,
                        body,
                        level,
                        action,
                    } => serde_json::json!({
                        "event": "notification",
                        "title": title,
                        "body": body,
                        "level": level,
                        "action": action,
                    }),
                    // Skip events not relevant to browser
                    AgentEvent::PtyInput { .. }
                    | AgentEvent::PtyResize { .. }
                    | AgentEvent::SessionUpdated { .. }
                    | AgentEvent::WebRtcOffer { .. }
                    | AgentEvent::WebRtcAnswer { .. }
                    | AgentEvent::WebRtcIce { .. }
                    | AgentEvent::UpdateAvailable { .. } => continue,
                    // Send remaining events as generic JSON
                    _ => match serde_json::to_value(&event) {
                        Ok(v) => v,
                        Err(_) => continue,
                    },
                };

                consecutive_lags = 0;
                if let Err(e) = sender.send_json(json).await {
                    warn!("event relay send error: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                consecutive_lags += 1;
                warn!("event relay lagged, missed {n} events (strike {consecutive_lags}/5)");
                if consecutive_lags >= 5 {
                    tracing::error!("event relay killed: persistent lag ({consecutive_lags} consecutive)");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Handle a control channel JSON message.
/// Routes RPC requests to web handlers and processes control messages.
#[cfg(feature = "webrtc-experimental")]
async fn handle_control_message(
    value: &serde_json::Value,
    web_state: &super::web::WebState,
    authenticated: &mut bool,
) -> Option<serde_json::Value> {
    // ── Authentication gate ──
    let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if msg_type == "auth" {
        let password = value.get("password").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(ref pw_hash) = web_state.daemon_password_hash {
            if Config::verify_daemon_password(password, pw_hash) {
                *authenticated = true;
                info!("WebRTC client authenticated");
                return Some(serde_json::json!({"type": "auth_ok"}));
            } else {
                return Some(serde_json::json!({"type": "auth_failed"}));
            }
        }
        *authenticated = true;
        return Some(serde_json::json!({"type": "auth_ok"}));
    }

    // Block unauthenticated requests
    if !*authenticated {
        return Some(serde_json::json!({"type": "auth_required"}));
    }

    // ── RPC requests ──
    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
        let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("/");
        let body = value.get("body");

        return Some(handle_rpc(id, path, body, web_state).await);
    }

    // ── Direct control messages ──
    match msg_type {
        "pong" => None,
        "version" => {
            // Browser reporting server version — log and continue
            None
        }
        "browser_info" => {
            // Browser capabilities — store for TTS routing decisions
            let volume = value
                .get("volume")
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as u8;
            let audio_ctx = value
                .get("audio_context")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Store in web_state (use session_id 0 for WebRTC clients)
            web_state.browser_volumes.write().await.insert(0, volume);
            web_state
                .browser_audio_contexts
                .write()
                .await
                .insert(0, audio_ctx);
            None
        }
        "resize" => {
            let rows = value
                .get("rows")
                .and_then(|v| v.as_u64())
                .unwrap_or(24) as u16;
            let cols = value
                .get("cols")
                .and_then(|v| v.as_u64())
                .unwrap_or(80) as u16;
            let _ = web_state
                .events
                .send(AgentEvent::PtyResize { rows, cols });
            None
        }
        "terminal_input" => {
            if let Some(data) = value.get("data").and_then(|v| v.as_str()) {
                let _ = web_state.events.send(AgentEvent::PtyInput {
                    data: data.to_string(),
                });
            }
            None
        }
        "eval_result" => {
            let eval_id = value
                .get("eval_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let result = value
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Some(tx) = web_state.eval_pending.write().await.remove(eval_id) {
                let _ = tx.send(result.to_string());
            }
            None
        }
        _ => {
            warn!(msg_type, "Unknown WebRTC control message");
            None
        }
    }
}

/// Route an RPC request to the appropriate handler.
#[cfg(feature = "webrtc-experimental")]
async fn handle_rpc(
    id: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    web_state: &super::web::WebState,
) -> serde_json::Value {
    match path {
        "/api/init-bundle" => {
            // Build init bundle (same as web.rs init_bundle handler)
            let tts_entries = web_state.tts_archive.list().await;
            let info_entries = web_state.info_archive.list().await;

            let gpu_config = serde_json::json!({
                "local_gpu_url": std::env::var("LOCAL_GPU_URL").unwrap_or_default(),
            });

            serde_json::json!({
                "id": id,
                "status": 200,
                "body": {
                    "tts_history": tts_entries,
                    "info_archive": info_entries,
                    "gpu_config": gpu_config,
                }
            })
        }
        "/api/tts-history" => {
            let entries = web_state.tts_archive.list().await;
            serde_json::json!({
                "id": id,
                "status": 200,
                "body": entries,
            })
        }
        "/api/inject" | "/api/submitTranscription" => {
            // Inject event (transcription, hive message, etc.)
            if let Some(body) = body {
                if let Some(text) = body.get("text").and_then(|v| v.as_str()) {
                    let _ = web_state.events.send(AgentEvent::Transcription {
                        text: text.to_string(),
                    });
                }
            }
            serde_json::json!({
                "id": id,
                "status": 200,
                "body": {"ok": true}
            })
        }
        "/api/tts" => {
            // TTS request — broadcast TtsStream event
            if let Some(body) = body {
                let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let msg_id = body.get("msg_id").and_then(|v| v.as_str()).unwrap_or("");
                let voice = body.get("voice").and_then(|v| v.as_str()).unwrap_or("default");
                let emotion = body.get("emotion").and_then(|v| v.as_str()).unwrap_or("");

                let force_lowercase = *web_state.force_lowercase.read().await;
                let cleaned = if force_lowercase {
                    text.to_lowercase()
                } else {
                    text.to_string()
                };

                let _ = web_state.events.send(AgentEvent::TtsStream {
                    msg_id: msg_id.to_string(),
                    text: cleaned,
                    original_text: text.to_string(),
                    voice: voice.to_string(),
                    emotion: emotion.to_string(),
                    replying_to: String::new(),
                    split_chars: ".!?".to_string(),
                    min_chunk_chars: 80,
                    conditionals_b64: String::new(),
                });
            }
            serde_json::json!({
                "id": id,
                "status": 200,
                "body": {"status": "streaming"}
            })
        }
        "/api/info" => {
            let entries = web_state.info_archive.list().await;
            serde_json::json!({
                "id": id,
                "status": 200,
                "body": entries,
            })
        }
        "/api/browser-console" => {
            let logs = web_state.browser_console.read().await;
            let entries: Vec<&str> = logs.iter().map(|s| s.as_str()).collect();
            serde_json::json!({
                "id": id,
                "status": 200,
                "body": entries,
            })
        }
        _ => {
            warn!(id, path, "Unknown RPC path");
            serde_json::json!({
                "id": id,
                "status": 404,
                "body": {"error": "not found"}
            })
        }
    }
}
