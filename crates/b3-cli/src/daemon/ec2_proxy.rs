//! EC2 daemon proxy — reverse tunnel client for E2E encrypted browser ↔ daemon relay.
//!
//! This module handles the daemon side of the EC2 proxy transport:
//!
//! 1. **Keypair management**: loads or generates a long-term x25519 keypair,
//!    persisted to `~/.b3/daemon-proxy.key`.
//!
//! 2. **EC2 registration**: connects to `wss://{server}/api/daemon-register?agent_id=X`,
//!    sends `{"type":"register","pubkey":"...","lan_port":N}`, stays connected as
//!    **control channel only** (no data flows on the registration WS).
//!    Reconnects with exponential backoff on disconnect.
//!
//! 3. **Per-session reverse tunnel**: when a browser connects via EC2:
//!    - EC2 sends `{"type":"session_request","session_id":"Y"}` on the control WS
//!    - `b3-tunnel::run_tunnel_client` dials back EC2's `/api/daemon-session?session_id=Y`
//!    - The session handler opens local `/ws-proxy` (write-only: input from browser→PTY)
//!    - Events flow directly: state.events → enc.send() → to_ec2_tx (bounded async channel)
//!    - Session ends → pipe closes → nothing shared
//!
//! # Transport architecture (EC2 path — Option C)
//!
//! ```text
//! Browser ←→ EC2 (/api/daemon-session) ←→ (dial-back WS) ←→ ECDH ←→ /ws-proxy ←→ PTY (input only)
//!                                                 ↑ output path
//!                              state.events.subscribe() → delta_msg() → enc.send() → to_ec2_tx
//! ```
//!
//! # Why Option C (direct event subscription)?
//!
//! The old path forwarded PTY output through the ReliableChannel bridge via try_send, which
//! saturates under flush_unacked() bursts (33K frames in <1ms on reconnect). try_send is
//! structural — the bridge closure is sync, cannot .await.
//!
//! Option C bypasses the bridge entirely for terminal delta output:
//! - ec2_proxy subscribes directly to state.events (broadcast channel)
//! - On SessionUpdated: calls state.sessions.delta_msg(prev).await
//! - Sends delta via enc.send() → to_ec2_tx.send().await (1 hop, bounded, natural backpressure)
//! - /ws-proxy connection used ONLY for input (browser keystrokes → PTY)
//!
//! # Why reverse tunnel?
//!
//! The daemon is behind NAT/WSL2 and cannot accept inbound connections from EC2.
//! Using the bore-style reverse tunnel pattern, the daemon dials outbound on both
//! the control WS (registration) and per-session dial-back connections.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::header::AUTHORIZATION,
        Message,
    },
};
use x25519_dalek::{PublicKey, StaticSecret};

use b3_common::AgentEvent;
use b3_reliable::{EncryptedChannel, Priority};
use b3_tunnel::run_tunnel_client;

use crate::config::Config;
use crate::daemon::web::WebState;

// ── Multi-server registration manager ────────────────────────────────

/// Spawn registrations on the primary server plus all peer servers.
///
/// The primary server loop (`server_urls[0]`) runs permanently with a reconnect
/// loop. On each reconnect of the primary, the peer shutdown channel is dropped
/// (causing all peer loops to exit), then N-1 fresh peer loops are spawned.
///
/// `server_urls` must have at least one entry (the primary). Additional entries
/// are peer servers (replicas / PoPs). If only one URL is provided, this behaves
/// identically to a single `spawn_ec2_registration` call.
///
/// All registration frames include `"multi_server": true` so the server skips
/// sibling forwarding for this agent.
pub fn spawn_multi_server_registration(
    server_urls: Vec<String>,
    agent_id: String,
    api_key: String,
    lan_port: u16,
    web_port: u16,
    proxy_token: String,
    secret: StaticSecret,
    state: WebState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let secret_bytes: [u8; 32] = secret.to_bytes();

        // Derive the primary domain from the first URL (full URL or bare domain).
        let primary_url = server_urls.first().cloned().unwrap_or_default();
        let primary_domain = url_to_domain(&primary_url);

        tracing::info!(
            agent_id,
            primary_domain,
            peer_count = server_urls.len().saturating_sub(1),
            "[EC2Proxy] Starting multi-server registration manager"
        );

        // Peer shutdown sender — dropping it causes all peer loops to exit.
        let mut peer_shutdown_tx: Option<tokio::sync::watch::Sender<bool>> = None;

        let mut backoff = Duration::from_secs(1);

        loop {
            // ── Connect to primary server ───────────────────────────────
            let register_url = format!(
                "wss://{}/api/daemon-register?agent_id={}",
                primary_domain, agent_id
            );

            let mut request = match register_url.as_str().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("[EC2Proxy] Invalid primary register URL: {e}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            request.headers_mut().insert(
                AUTHORIZATION,
                format!("Bearer {}", api_key).parse().unwrap(),
            );

            let ws = match tokio::time::timeout(Duration::from_secs(15), connect_async(request)).await {
                Ok(Ok((ws, _))) => ws,
                Ok(Err(e)) => {
                    tracing::warn!("[EC2Proxy] Primary connection failed: {e} — retry in {}s", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
                Err(_) => {
                    tracing::warn!("[EC2Proxy] Primary connection timeout — retry in {}s", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };

            backoff = Duration::from_secs(1);
            tracing::info!(agent_id, primary_domain, "[EC2Proxy] Connected to primary daemon-register");

            let (mut control_tx, control_rx) = ws.split();

            let reg_frame = serde_json::json!({
                "type": "register",
                "pubkey": base64::engine::general_purpose::STANDARD
                    .encode(PublicKey::from(&StaticSecret::from(secret_bytes)).as_bytes()),
                "lan_port": lan_port,
                "multi_server": true,
            }).to_string();

            if control_tx.send(Message::Text(reg_frame)).await.is_err() {
                tracing::warn!(agent_id, "[EC2Proxy] Failed to send registration frame to primary — reconnecting");
                continue;
            }

            // ── Shut down stale peer loops, spawn fresh ones ────────────
            // Drop the old sender first — that signals all peer loops to exit.
            drop(peer_shutdown_tx.take());

            if server_urls.len() > 1 {
                let (shutdown_tx, _) = tokio::sync::watch::channel(false);
                for peer_url in server_urls.iter().skip(1) {
                    let peer_domain = url_to_domain(peer_url);
                    spawn_ec2_registration(
                        peer_domain,
                        agent_id.clone(),
                        api_key.clone(),
                        lan_port,
                        web_port,
                        proxy_token.clone(),
                        StaticSecret::from(secret_bytes),
                        state.clone(),
                        Some(shutdown_tx.subscribe()),
                    );
                }
                peer_shutdown_tx = Some(shutdown_tx);
                tracing::info!(
                    agent_id,
                    "[EC2Proxy] Spawned {} peer registration loop(s)",
                    server_urls.len() - 1
                );
            }

            // ── Run the primary control loop ────────────────────────────
            let ec2_session_base = format!("wss://{}/api/daemon-session", primary_domain);
            let wid = agent_id.clone();
            let wp = web_port;
            let pt = proxy_token.clone();
            let ak = api_key.clone();
            let st = state.clone();

            let result = run_tunnel_client(
                control_tx,
                control_rx,
                agent_id.clone(),
                ec2_session_base,
                Duration::from_secs(5),
                Duration::from_secs(15),
                Some(ak.clone()),
                move |session_id, ec2_ws| {
                    let wid = wid.clone();
                    let pt = pt.clone();
                    let ak = ak.clone();
                    let st = st.clone();
                    async move {
                        run_daemon_session(session_id, ec2_ws, wid, wp, pt, ak, secret_bytes, st).await;
                    }
                },
                None::<fn(serde_json::Value) -> std::future::Ready<Option<String>>>,
            ).await;

            match result {
                Ok(()) => tracing::info!(agent_id, "[EC2Proxy] Primary control WS closed cleanly — reconnecting"),
                Err(e) => tracing::warn!(agent_id, "[EC2Proxy] Primary control WS error: {e} — reconnecting"),
            }
            // Loop back — peer loops will be shut down and re-spawned on next connect.
        }
    })
}

/// Extract a bare `host[:port]` domain from a URL or passthrough if already bare.
/// Public alias used by server.rs for deduplication.
pub fn url_to_domain_pub(url: &str) -> String {
    url_to_domain(url)
}

fn url_to_domain(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

// ── Keypair management ───────────────────────────────────────────────

fn keypair_path() -> PathBuf {
    Config::config_dir().join("daemon-proxy.key")
}

fn load_keypair() -> Option<StaticSecret> {
    let raw = std::fs::read_to_string(keypair_path()).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw.trim()).ok()?;
    if bytes.len() != 32 { return None; }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(StaticSecret::from(arr))
}

fn generate_and_save_keypair() -> anyhow::Result<StaticSecret> {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let secret = StaticSecret::from(bytes);
    let b64 = base64::engine::general_purpose::STANDARD.encode(secret.to_bytes());
    let path = keypair_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, b64)?;
    tracing::info!("[EC2Proxy] Generated new daemon-proxy keypair at {}", path.display());
    Ok(secret)
}

/// Load or generate the daemon's long-term x25519 keypair.
pub fn load_or_generate_keypair() -> anyhow::Result<StaticSecret> {
    match load_keypair() {
        Some(secret) => {
            tracing::info!("[EC2Proxy] Loaded existing daemon-proxy keypair");
            Ok(secret)
        }
        None => generate_and_save_keypair(),
    }
}

// ── EC2 registration loop ─────────────────────────────────────────────

/// Spawn a single EC2 registration loop.
///
/// Runs until `shutdown_rx` fires or the sender is dropped (peer loops), or
/// forever for the primary loop (pass `None`).
///
/// Registration frames include `"multi_server": true` to suppress sibling forwarding.
fn spawn_ec2_registration(
    server_domain: String,
    agent_id: String,
    api_key: String,
    lan_port: u16,
    web_port: u16,
    proxy_token: String,
    secret: StaticSecret,
    state: WebState,
    shutdown_rx: Option<tokio::sync::watch::Receiver<bool>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let secret_bytes: [u8; 32] = secret.to_bytes();
        let pubkey_b64 = base64::engine::general_purpose::STANDARD
            .encode(PublicKey::from(&StaticSecret::from(secret_bytes)).as_bytes());

        tracing::info!(
            agent_id,
            lan_port,
            web_port,
            pubkey_prefix = %&pubkey_b64[..8.min(pubkey_b64.len())],
            "[EC2Proxy] Starting registration loop (domain={})", server_domain
        );

        let mut backoff = Duration::from_secs(1);

        loop {
            // Check for cooperative shutdown before each connection attempt.
            if let Some(ref rx) = shutdown_rx {
                if rx.has_changed().is_err() {
                    // Sender dropped — exit.
                    tracing::info!(agent_id, "[EC2Proxy] Peer shutdown signal — exiting ({})", server_domain);
                    return;
                }
            }

            let register_url = format!(
                "wss://{}/api/daemon-register?agent_id={}",
                server_domain, agent_id
            );

            let mut request = match register_url.as_str().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("[EC2Proxy] Invalid register URL: {e}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            request.headers_mut().insert(
                AUTHORIZATION,
                format!("Bearer {}", api_key).parse().unwrap(),
            );

            let ws = match tokio::time::timeout(Duration::from_secs(15), connect_async(request)).await {
                Ok(Ok((ws, _))) => ws,
                Ok(Err(e)) => {
                    tracing::warn!("[EC2Proxy] Connection failed: {e} — retry in {}s", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
                Err(_) => {
                    tracing::warn!("[EC2Proxy] Connection timeout — retry in {}s", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };

            backoff = Duration::from_secs(1);
            tracing::info!(agent_id, "[EC2Proxy] Connected to EC2 daemon-register ({})", server_domain);

            let (mut control_tx, control_rx) = ws.split();

            // Send registration frame on control WS before handing off to tunnel client.
            // "multi_server": true tells the server to skip sibling forwarding for this agent.
            let reg_frame = serde_json::json!({
                "type": "register",
                "pubkey": &pubkey_b64,
                "lan_port": lan_port,
                "multi_server": true,
            }).to_string();

            if control_tx.send(Message::Text(reg_frame)).await.is_err() {
                tracing::warn!(agent_id, "[EC2Proxy] Failed to send registration frame — reconnecting");
                continue;
            }

            // Run the reverse tunnel control loop.
            // On session_request: dials back EC2 /api/daemon-session?session_id=Y,
            // then calls the session handler with the established connection.
            let ec2_session_base = format!("wss://{}/api/daemon-session", server_domain);
            let wid = agent_id.clone();
            let wp = web_port;
            let pt = proxy_token.clone();
            let ak = api_key.clone();
            let st = state.clone();

            let result = run_tunnel_client(
                control_tx,
                control_rx,
                agent_id.clone(),
                ec2_session_base,
                Duration::from_secs(5),   // ping_interval
                Duration::from_secs(15),  // pong_timeout (generous for slow connections)
                Some(ak.clone()),         // dial_back_token: daemon Bearer token for /api/daemon-session auth
                move |session_id, ec2_ws| {
                    let wid = wid.clone();
                    let pt = pt.clone();
                    let ak = ak.clone();
                    let st = st.clone();
                    async move {
                        run_daemon_session(session_id, ec2_ws, wid, wp, pt, ak, secret_bytes, st).await;
                    }
                },
                None::<fn(serde_json::Value) -> std::future::Ready<Option<String>>>,
            ).await;

            match result {
                Ok(()) => tracing::info!(agent_id, "[EC2Proxy] Control WS closed cleanly — reconnecting"),
                Err(e) => tracing::warn!(agent_id, "[EC2Proxy] Control WS error: {e} — reconnecting"),
            }
        }
    })
}

// ── Per-session handler ───────────────────────────────────────────────

/// Handle one browser session: ECDH handshake + direct event subscription for output.
///
/// `ec2_ws`: the dial-back WS established by b3-tunnel (browser frames arrive here).
///
/// **Option C architecture:**
/// - Output path: state.events → delta_msg() → enc.send() → to_ec2_tx (async bounded, 1 hop)
/// - Input path: ec2_rx → enc.receive() → local /ws-proxy → PTY stdin
///
/// /ws-proxy is opened write-only (input). Terminal output bypasses ReliableChannel bridge
/// entirely — no flush_unacked() burst, natural async backpressure.
async fn run_daemon_session(
    session_id: String,
    ec2_ws: b3_tunnel::TungsteniteWs,
    agent_id: String,
    web_port: u16,
    proxy_token: String,
    api_key: String,
    secret_bytes: [u8; 32],
    state: WebState,
) {
    use tokio::sync::mpsc;

    tracing::info!(agent_id, session_id, "[EC2Proxy] Session started (Option C: direct event subscription)");

    let (mut ec2_tx, mut ec2_rx) = ec2_ws.split();

    // Channel: EncryptedChannel output → EC2 WS writer task.
    // Bounded async channel — backpressure propagates naturally to enc.send() callers.
    let (to_ec2_tx, mut to_ec2_rx) = mpsc::channel::<Vec<u8>>(256);

    // Spawn EC2 writer task — save handle for abort at session end
    let ec2_writer_handle = tokio::spawn(async move {
        while let Some(data) = to_ec2_rx.recv().await {
            let msg = match std::str::from_utf8(&data) {
                Ok(s) => Message::Text(s.to_string()),
                Err(_) => Message::Binary(data),
            };
            if ec2_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // EncryptedChannel: enc.send() → to_ec2_tx (via sync try_send in the closure).
    // In Option C, enc.send() is used for ALL output (terminal deltas + events).
    // The closure is sync (Fn, not async), so we use try_send with a large capacity channel.
    // The to_ec2_tx capacity (256) is sufficient because:
    //   - We call enc.send() once per SessionUpdated event (one delta at a time)
    //   - There is no flush_unacked() burst — we bypass the ReliableChannel bridge entirely
    //   - EncryptedChannel wraps the inner ReliableChannel, which may queue frames,
    //     but flushes them one-at-a-time on each send(), not in a 33K-frame burst
    let to_ec2_enc = to_ec2_tx.clone();
    let mut enc = EncryptedChannel::new(
        b3_reliable::Config::default(),
        StaticSecret::from(secret_bytes),
        move |data: &[u8]| {
            if to_ec2_enc.try_send(data.to_vec()).is_err() {
                tracing::warn!("[EC2Proxy] to_ec2_tx full — frame dropped (enc output overflow)");
            }
        },
    );
    enc.set_version(env!("CARGO_PKG_VERSION"));

    // Channel: local /ws-proxy → EncryptedChannel (not used in Option C — local WS is write-only)
    // Kept as a dead channel so the type system is happy; no reader spawned.
    let (local_to_enc_tx, mut local_to_enc_rx) = mpsc::channel::<Vec<u8>>(256);

    // Channel: HTTP-over-WS responses → enc output path
    let (http_resp_tx, mut http_resp_rx) = mpsc::channel::<Vec<u8>>(64);

    // local_ws_tx: write-only handle to /ws-proxy (browser keystrokes → PTY input only)
    let mut local_ws_tx: Option<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
            Message>
    > = None;

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    // Subscribe directly to the daemon's event broadcast.
    // On SessionUpdated: delta_msg() → to_ec2_tx (async, bounded backpressure).
    let mut event_rx = state.events.subscribe();
    let sessions_store = state.sessions.clone();
    // Track the last offset we sent so delta_msg() only sends new bytes.
    let mut sent_offset: u64 = 0;

    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));
    tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // ── Input from EC2 (browser → daemon) ────────────────────────
            msg = ec2_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        let just_ready = handle_ec2_frame(
                            &agent_id, &session_id, web_port, &proxy_token, &api_key,
                            t.as_bytes(), &mut enc, &mut local_ws_tx,
                            &to_ec2_tx, &local_to_enc_tx, &http_resp_tx, &http_client,
                        ).await;
                        if just_ready {
                            send_init_frame(&agent_id, &session_id, &sessions_store, &mut enc, &mut sent_offset).await;
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        let just_ready = handle_ec2_frame(
                            &agent_id, &session_id, web_port, &proxy_token, &api_key,
                            &b, &mut enc, &mut local_ws_tx,
                            &to_ec2_tx, &local_to_enc_tx, &http_resp_tx, &http_client,
                        ).await;
                        if just_ready {
                            send_init_frame(&agent_id, &session_id, &sessions_store, &mut enc, &mut sent_offset).await;
                        }
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        tracing::info!(agent_id, session_id, "[EC2Proxy] Session EC2 WS closed");
                        ec2_writer_handle.abort();
                        return;
                    }
                    _ => {}
                }
            }

            // ── Output from PTY → browser (Option C: direct event path) ──
            event = event_rx.recv() => {
                if !enc.is_ready() {
                    // Not yet through handshake — drop events (browser hasn't connected yet)
                    continue;
                }
                match event {
                    Ok(AgentEvent::SessionUpdated { .. }) => {
                        // Hot path: PTY output arrived. Compute delta and send directly.
                        // enc.send() calls the to_ec2_tx closure synchronously.
                        // No ReliableChannel bridge, no flush_unacked() burst — 1 hop.
                        // BestEffort: terminal deltas are retransmittable and should not
                        // block TTS audio chunks (Critical) in the inner ReliableChannel.
                        if let Some((msg, new_offset)) = sessions_store.delta_msg(sent_offset).await {
                            sent_offset = new_offset;
                            let payload = msg.to_string().into_bytes();
                            enc.send(&payload, Priority::BestEffort);
                        }
                    }
                    Ok(AgentEvent::Transcription { text }) => {
                        let msg = serde_json::json!({ "type": "transcription", "text": text });
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::Tts { text, msg_id, voice }) => {
                        let msg = serde_json::json!({ "type": "tts", "text": text, "msg_id": msg_id, "voice": voice });
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::TtsGenerating { msg_id, chunk, total }) => {
                        let msg = serde_json::json!({ "type": "tts_generating", "msg_id": msg_id, "chunk": chunk, "total": total });
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::TtsStream { msg_id, text, original_text, voice, emotion, replying_to, split_chars, min_chunk_chars, conditionals_b64 }) => {
                        let msg = serde_json::json!({ "type": "tts_stream", "msg_id": msg_id, "text": text, "original_text": original_text, "voice": voice, "emotion": emotion, "replying_to": replying_to, "split_chars": split_chars, "min_chunk_chars": min_chunk_chars, "conditionals_b64": conditionals_b64 });
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::Led { emotion }) => {
                        let msg = serde_json::json!({ "type": "led", "emotion": emotion });
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::BrowserEval { eval_id, code, session_id: target }) => {
                        let mut msg = serde_json::json!({ "type": "browser_eval", "eval_id": eval_id, "code": code });
                        if let Some(tid) = target {
                            msg["target_session_id"] = serde_json::json!(tid);
                        }
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::Hive { ref sender, ref text }) if sender == "info" => {
                        let msg = serde_json::json!({ "type": "info", "html": text });
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(AgentEvent::Notification { title, body, level, action }) => {
                        let mut msg = serde_json::json!({ "type": "notification", "title": title, "body": body, "level": level });
                        if let Some(a) = action {
                            msg["action"] = a;
                        }
                        send_event_msg(&mut enc, msg);
                    }
                    Ok(_) => {} // Other events not forwarded to EC2 browser
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::warn!(agent_id, session_id, "[EC2Proxy] Event channel closed — ending session");
                        ec2_writer_handle.abort();
                        return;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(agent_id, session_id, "[EC2Proxy] Event consumer lagged by {n} — catching up with delta");
                        // Lagged — catch up with a single delta instead of trying to replay missed events.
                        // BestEffort: same reasoning as SessionUpdated — terminal data is retransmittable.
                        if let Some((msg, new_offset)) = sessions_store.delta_msg(sent_offset).await {
                            sent_offset = new_offset;
                            let payload = msg.to_string().into_bytes();
                            enc.send(&payload, Priority::BestEffort);
                        }
                    }
                }
            }

            // ── Local /ws-proxy → enc (not used in Option C, dead arm) ──
            Some(_data) = local_to_enc_rx.recv() => {
                // In Option C, /ws-proxy is write-only (browser→PTY input).
                // This arm is never triggered (no reader spawned for local WS output).
                // Kept to prevent the channel from being dropped prematurely.
            }

            // ── HTTP-over-WS responses ────────────────────────────────────
            Some(resp) = http_resp_rx.recv() => {
                if enc.is_ready() {
                    enc.send(&resp, Priority::Critical);
                }
            }

            // ── EncryptedChannel tick (retransmit / keepalive) ───────────
            _ = tick_interval.tick() => {
                if enc.is_ready() {
                    enc.tick();
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Send the terminal init frame immediately after ECDH handshake completes.
///
/// The browser's ws.js handler at line 479 does `HC.term.reset()` and writes
/// the full buffer on `{type:"init"}`. Without it, the terminal never populates —
/// the first `{type:"delta"}` appends to an empty buffer, which the browser
/// treats as garbage.
///
/// Also sets `sent_offset` to `buf.write_offset` so subsequent delta_msg() calls
/// don't re-send the full buffer as a delta.
async fn send_init_frame(
    agent_id: &str,
    session_id: &str,
    sessions_store: &crate::daemon::web::SessionStore,
    enc: &mut EncryptedChannel,
    sent_offset: &mut u64,
) {
    if let Some(buf) = sessions_store.get().await {
        let init_b64 = base64::engine::general_purpose::STANDARD.encode(buf.terminal_data.as_slice());
        let init_msg = serde_json::json!({
            "type": "init",
            "terminal_data": init_b64,
            "rows": buf.rows,
            "cols": buf.cols,
        });
        let payload = init_msg.to_string().into_bytes();
        enc.send(&payload, Priority::Critical);
        *sent_offset = buf.write_offset;
        tracing::info!(agent_id, session_id, "[EC2Proxy] Init frame sent ({} bytes, offset={})", buf.terminal_data.len(), buf.write_offset);
    } else {
        tracing::info!(agent_id, session_id, "[EC2Proxy] No session buffer yet — init frame skipped (will catch up via delta)");
    }
}

/// Send a JSON event message to the EC2 browser via enc.send().
/// Returns immediately if enc is not ready.
fn send_event_msg(
    enc: &mut EncryptedChannel,
    msg: serde_json::Value,
) {
    if !enc.is_ready() { return; }
    let payload = msg.to_string().into_bytes();
    enc.send(&payload, Priority::Critical);
}

// ── EC2 frame handler ─────────────────────────────────────────────────

/// Handle a single frame from EC2 on the per-session dial-back WS.
///
/// Returns `true` if the ECDH handshake just completed this call — caller should
/// send the terminal init frame immediately after.
///
/// Handles: ECDH handshake, browser_disconnected reset, HTTP-over-WS proxying,
/// and regular app frames forwarded to local PTY WS (write-only, input path).
async fn handle_ec2_frame(
    agent_id: &str,
    session_id: &str,
    web_port: u16,
    proxy_token: &str,
    api_key: &str,
    data: &[u8],
    enc: &mut EncryptedChannel,
    local_ws_tx: &mut Option<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
            Message>>,
    to_ec2_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    local_to_enc_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    http_resp_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    http_client: &reqwest::Client,
) -> bool {
    // ── Control frame check ──────────────────────────────────────────
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
        match v.get("type").and_then(|t| t.as_str()) {
            Some("browser_disconnected") => {
                tracing::info!(agent_id, session_id, "[EC2Proxy] Browser disconnected — session ending");
                // Session is ending — local_ws_tx drop closes /ws-proxy cleanly.
                *local_ws_tx = None;
                return false;
            }
            Some("handshake") => {
                if enc.is_ready() {
                    tracing::info!(agent_id, session_id, "[EC2Proxy] Handshake received while READY — resetting for new browser");
                    *local_ws_tx = None;
                    let to_ec2_reset = to_ec2_tx.clone();
                    enc.reset_for_reconnect(move |d: &[u8]| {
                        let _ = to_ec2_reset.try_send(d.to_vec());
                    });
                }
                // Fall through to handshake handling below.
            }
            Some("echo_ping") => {
                // Upstream health probe from browser reliable.js — respond with echo_pong.
                // Sent as a raw text frame (bypassing the encrypted reliable channel) so
                // the daemon's EncryptedChannel must NOT try to decrypt it.
                // Respond via to_ec2_tx (EC2 writer sends text frames for valid UTF-8).
                let id = v.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let pong = serde_json::json!({ "type": "echo_pong", "id": id });
                let _ = to_ec2_tx.try_send(pong.to_string().into_bytes());
                return false;
            }
            Some("telemetry") | Some("browser_log") => {
                // These are sent via sendRaw (raw text frame, not encrypted).
                // Forward directly to local /ws-proxy so handle_terminal_ws can process them.
                // Do NOT pass to enc.receive() — that would fail AES-GCM decryption.
                if let Some(ref mut ltx) = local_ws_tx {
                    let text = match std::str::from_utf8(data) {
                        Ok(s) => s.to_string(),
                        Err(_) => return false,
                    };
                    if ltx.send(Message::Text(text)).await.is_err() {
                        *local_ws_tx = None;
                    }
                }
                return false;
            }
            _ => {}
        }
    }

    // ── HANDSHAKING path ─────────────────────────────────────────────
    if !enc.is_ready() {
        let v = match serde_json::from_slice::<serde_json::Value>(data) {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(agent_id, session_id, "[EC2Proxy] Non-JSON frame before handshake — ignored");
                return false;
            }
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("handshake") {
            tracing::warn!(agent_id, session_id, "[EC2Proxy] Expected handshake frame, got type={:?}", v.get("type"));
            return false;
        }

        // The browser's session_id (u64, from the handshake frame) is used for /ws-proxy —
        // not the EC2 tunnel session_id (hex string used only for dial-back pairing).
        // /ws-proxy rejects connections where session_id is not a valid u64.
        let pty_session_id = v.get("session_id")
            .and_then(|s| s.as_u64().or_else(|| s.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);
        let url = format!(
            "ws://127.0.0.1:{}/ws-proxy?proxy_token={}&session_id={}",
            web_port, proxy_token, pty_session_id
        );
        match connect_async(url.as_str()).await {
            Ok((ws, _)) => {
                let (ltx, _lrx) = ws.split();
                *local_ws_tx = Some(ltx);
                // NOTE: _lrx (local WS reader) is intentionally dropped.
                // In Option C, terminal output flows via state.events → delta_msg(),
                // not through the local WS pipe. We only need the write half for PTY input.
                let _ = local_to_enc_tx; // suppress unused warning — kept for API compat

                enc.receive(data);
                tracing::info!(agent_id, session_id, "[EC2Proxy] /ws-proxy open (write-only for input) — ECDH complete");
                // Signal to caller: handshake just completed — send init frame now.
                return true;
            }
            Err(e) => {
                tracing::warn!(agent_id, session_id, "[EC2Proxy] Local /ws-proxy connect failed: {e}");
                return false;
            }
        }
    }

    // ── READY path ───────────────────────────────────────────────────
    let msgs = enc.receive(data);
    for m in msgs {
        // HTTP-over-WS: intercept http_request frames before forwarding to local WS.
        if let Ok(req) = serde_json::from_slice::<serde_json::Value>(&m.payload) {
            if req.get("type").and_then(|t| t.as_str()) == Some("http_request") {
                let req_id = req.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let method  = req.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_string();
                let path    = req.get("path").and_then(|v| v.as_str()).unwrap_or("/").to_string();
                let body    = req.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let content_type = req
                    .get("headers").and_then(|h| h.get("Content-Type")).and_then(|v| v.as_str())
                    .unwrap_or("application/json").to_string();

                // Restrict to /api/ paths — no arbitrary local access.
                if !path.starts_with("/api/") {
                    tracing::warn!(agent_id, session_id, path, "[EC2Proxy] HTTP-over-WS: forbidden path");
                    let resp = serde_json::json!({
                        "type": "http_response",
                        "id": req_id,
                        "status": 403,
                        "body": "{\"error\":\"forbidden\"}",
                    }).to_string();
                    let _ = http_resp_tx.try_send(resp.into_bytes());
                    continue;
                }

                tracing::info!(agent_id, session_id, method, path, "[EC2Proxy] HTTP-over-WS request");

                let url = format!("http://127.0.0.1:{}{}", web_port, path);
                let api_key_owned = api_key.to_string();
                let resp_tx = http_resp_tx.clone();
                let client = http_client.clone();

                tokio::spawn(async move {
                    let builder = match method.as_str() {
                        "POST"   => client.post(&url).header("Content-Type", &content_type).body(body),
                        "PUT"    => client.put(&url).header("Content-Type", &content_type).body(body),
                        "PATCH"  => client.patch(&url).header("Content-Type", &content_type).body(body),
                        "DELETE" => client.delete(&url).body(body),
                        _        => client.get(&url),
                    };
                    let builder = builder.header("Authorization", format!("Bearer {api_key_owned}"));

                    let (status, resp_body, resp_content_type) = match builder.send().await {
                        Ok(r) => {
                            let s = r.status().as_u16();
                            let ct = r.headers()
                                .get("content-type")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("application/json")
                                .to_string();
                            // Binary content types must be base64-encoded — WS frames are text-only.
                            let is_binary = ct.starts_with("audio/") || ct.starts_with("image/")
                                || ct == "application/octet-stream";
                            let b = if is_binary {
                                use base64::Engine as _;
                                match r.bytes().await {
                                    Ok(bytes) => base64::engine::general_purpose::STANDARD.encode(&bytes),
                                    Err(_) => String::new(),
                                }
                            } else {
                                r.text().await.unwrap_or_default()
                            };
                            (s, b, ct)
                        }
                        Err(e) => {
                            tracing::warn!("[EC2Proxy] HTTP-over-WS reqwest error: {e}");
                            (502u16, format!("{{\"error\":\"{}\" }}",  e), "application/json".to_string())
                        }
                    };

                    let resp_frame = serde_json::json!({
                        "type": "http_response",
                        "id": req_id,
                        "status": status,
                        "body": resp_body,
                        "content_type": resp_content_type,
                    }).to_string();
                    let _ = resp_tx.send(resp_frame.into_bytes()).await;
                });
                continue;
            }
        }

        // Regular app frame — forward to local PTY WS (input path: browser keystrokes → PTY).
        if let Some(ref mut ltx) = local_ws_tx {
            let msg_out = match std::str::from_utf8(&m.payload) {
                Ok(s) => Message::Text(s.to_string()),
                Err(_) => Message::Binary(m.payload),
            };
            if ltx.send(msg_out).await.is_err() {
                tracing::info!(agent_id, session_id, "[EC2Proxy] Local WS send failed — session ending");
                *local_ws_tx = None;
                return false;
            }
        } else {
            tracing::warn!(agent_id, session_id, "[EC2Proxy] Frame dropped — local_ws_tx not set (pre-handshake)");
        }
    }
    false
}
