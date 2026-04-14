//! b3-gpu-relay — Reliability relay sidecar for the GPU worker.
//!
//! Per-browser sessions via SessionManager: each browser gets its own
//! MultiTransport with independent seq numbers. GPU WS responses are
//! routed to the requesting session by msg_id.
//!
//! Supports both WS and WebRTC transports per session (same as daemon).
//!
//! EC2 registration: on startup, connects to EC2 /api/gpu-register and
//! maintains a keepalive WS. EC2 uses this channel to forward browser
//! frames to /ws-proxy (local endpoint that bridges to GPU worker).

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    Router,
};
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
use axum::{extract::Json, http::StatusCode};
use b3_reliable::{frame, bridge::OutboundMessage, Config, EncryptedChannel, ReliableChannel, SessionManager, Priority};
use b3_tunnel::{run_tunnel_client, TungsteniteWs};
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::channel::ChannelMessage;
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::peer::{B3Peer, PeerConfig, PeerEvent};
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
use b3_webrtc::signaling::IceServer;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use x25519_dalek::{PublicKey, StaticSecret};

// ── Types ──────────────────────────────────────────────────────────

type GpuTx = Arc<Mutex<futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Message,
>>>;

#[derive(Clone)]
struct AppState {
    session_manager: Arc<SessionManager>,
    /// Maps msg_id → session_id for routing GPU responses to the requesting browser.
    pending_requests: Arc<Mutex<HashMap<String, u64>>>,
    /// Shared GPU WS write handle — all sessions forward browser messages here.
    gpu_tx: GpuTx,
    // EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
    #[cfg(feature = "webrtc-experimental")]
    /// Active RTC bridge abort handles per session — enforces one bridge per session.
    /// New offer aborts the old bridge before spawning a new one.
    rtc_bridges: Arc<Mutex<HashMap<u64, tokio::task::AbortHandle>>>,
    /// True once the EC2 registration WS has connected successfully.
    /// handler.py polls /health for this flag before yielding gpu_worker_id to browsers.
    ec2_registered: Arc<std::sync::atomic::AtomicBool>,
}

// ── Config ─────────────────────────────────────────────────────────

fn gpu_ws_url() -> String {
    let base = std::env::var("LOCAL_GPU_URL").unwrap_or_else(|_| "http://localhost:5125".to_string());
    let token = std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default();
    let ws_base = base.replace("http://", "ws://").replace("https://", "wss://") + "/ws";
    if token.is_empty() { ws_base } else { format!("{ws_base}?token={token}") }
}

fn relay_port() -> u16 {
    std::env::var("GPU_RELAY_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(5126)
}

/// Reads GPU WS messages and routes to the owning session by msg_id.
/// Falls back to sending to all sessions if msg_id is unknown.
/// Times out after 30s of inactivity — detects zombie TCP connections
/// where the socket is open but the worker process is dead.
async fn gpu_ws_reader(
    mut gpu_rx: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    session_manager: Arc<SessionManager>,
    pending_requests: Arc<Mutex<HashMap<String, u64>>>,
) {
    loop {
        // 30s timeout: GPU worker sends pings every 5s. 30s = 6 missed pings = zombie.
        let msg = match tokio::time::timeout(
            tokio::time::Duration::from_secs(30),
            gpu_rx.next()
        ).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                warn!("GPU worker WS error: {e}");
                break;
            }
            Ok(None) => break, // stream ended
            Err(_) => {
                warn!("GPU worker WS timeout (30s) — zombie connection detected");
                break;
            }
        };
        let (payload, priority) = match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                let p = classify_priority(&text);
                (text.into_bytes(), p)
            }
            tokio_tungstenite::tungstenite::Message::Binary(data) => {
                (data.to_vec(), Priority::Critical)
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };

        // Log GPU→relay data flow
        let text_preview = std::str::from_utf8(&payload).ok().unwrap_or("");
        let is_ping = text_preview.contains("\"type\":\"ping\"") || text_preview.contains("\"type\": \"ping\"");
        if !is_ping {
            let sessions = session_manager.count().await;
            tracing::debug!(payload_len = payload.len(), sessions, "GPU→relay: data received from worker");
        }
        // Log ping count every 60 pings (~5min at 5s interval) to confirm WS is alive
        {
            static PING_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if is_ping {
                let c = PING_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if c % 60 == 0 {
                    info!("GPU worker WS alive — {} pings received", c);
                }
            }
        }

        let outbound = OutboundMessage { payload: payload.clone(), priority };

        // Try to extract msg_id for routing.
        // GPU worker appends suffixes like "-partial-18" to stream_ids.
        // Match by exact key first, then by prefix (e.g. "abc-partial-18" matches "abc").
        let msg_id = extract_msg_id(&payload);
        let target_session_id = if let Some(ref id) = msg_id {
            let pending = pending_requests.lock().await;
            pending.get(id).copied().or_else(|| {
                // Prefix match: find a pending key that is a prefix of this msg_id
                pending.iter()
                    .find(|(k, _)| id.starts_with(k.as_str()))
                    .map(|(_, &v)| v)
            })
        } else {
            None
        };

        // Check if this is a final response (remove from pending)
        if let Some(ref id) = msg_id {
            if is_final_response(&payload) {
                pending_requests.lock().await.remove(id);
            }
        }

        if let Some(session_id) = target_session_id {
            if let Some(session) = session_manager.get(session_id).await {
                let _ = session.backend_tx.lock().await.send(outbound).await;
            }
        } else if msg_id.is_some() {
            // Had a msg_id/job_id but no matching session — the session was cleaned up.
            // Drop the response. Do NOT broadcast to all sessions.
            let pending_keys: Vec<String> = pending_requests.lock().await.keys().cloned().collect();
            let session_ids = session_manager.ids().await;
            warn!(msg_id = ?msg_id, pending_count = pending_keys.len(),
                  session_count = session_ids.len(),
                  "GPU response dropped — msg_id not in pending_requests. \
                   Pending: {:?}, Sessions: {:?}",
                  pending_keys.iter().take(5).collect::<Vec<_>>(),
                  session_ids.iter().take(5).collect::<Vec<_>>());
        }
        // No msg_id (e.g. worker pings) — silently discard.
    }
    info!("GPU worker WS closed");
}

// ── Main ───────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into()))
        .init();

    let port = relay_port();

    // Create a dummy gpu_tx — will be replaced by connect_gpu
    let dummy_tx = {
        // We need a real connection first
        let gpu_url = gpu_ws_url();
        let mut gpu_conn = None;
        for attempt in 1..=120 {
            match tokio_tungstenite::connect_async(&gpu_url).await {
                Ok(c) => { gpu_conn = Some(c); break; }
                Err(e) => {
                    if attempt <= 3 || attempt % 5 == 0 {
                        info!("GPU worker not ready (attempt {attempt}): {e}");
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        }
        match gpu_conn {
            Some((ws, _)) => {
                info!("Connected to GPU worker");
                let (gpu_tx, gpu_rx) = ws.split();
                (gpu_tx, Some(gpu_rx))
            }
            None => {
                warn!("Failed to connect to GPU worker — exiting");
                return;
            }
        }
    };

    // Generate proxy token (32 random bytes, hex-encoded)
    let proxy_token = {
        use rand::RngCore as _;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    info!(proxy_token_prefix = %&proxy_token[..8], "Generated proxy token");

    let ec2_registered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let state = AppState {
        session_manager: Arc::new(SessionManager::new()),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        gpu_tx: Arc::new(Mutex::new(dummy_tx.0)),
        // EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
        #[cfg(feature = "webrtc-experimental")]
        rtc_bridges: Arc::new(Mutex::new(HashMap::new())),
        ec2_registered: ec2_registered.clone(),
    };

    // Start GPU WS reader with auto-reconnect
    {
        let sm = state.session_manager.clone();
        let pending = state.pending_requests.clone();
        let gpu_tx = state.gpu_tx.clone();
        if let Some(gpu_rx) = dummy_tx.1 {
            let sm2 = sm.clone();
            let pending2 = pending.clone();
            tokio::spawn(async move {
                gpu_ws_reader(gpu_rx, sm2, pending2).await;
            });
        }
        // Reconnect loop: if GPU WS reader exits, reconnect after backoff
        tokio::spawn(async move {
            let mut backoff = 1u64;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                // Check if we can reach the worker
                let gpu_url = gpu_ws_url();
                match tokio_tungstenite::connect_async(&gpu_url).await {
                    Ok((ws, _)) => {
                        let (new_tx, gpu_rx) = ws.split();
                        *gpu_tx.lock().await = new_tx;
                        info!("GPU worker WS reconnected");
                        backoff = 1;
                        let sm3 = sm.clone();
                        let pending3 = pending.clone();
                        gpu_ws_reader(gpu_rx, sm3, pending3).await;
                        warn!("GPU worker WS closed — will reconnect in {backoff}s");
                    }
                    Err(_) => {
                        // Worker not ready — exponential backoff
                        backoff = (backoff * 2).min(30);
                        tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    }
                }
            }
        });
    }

    // Spawn EC2 registration task if configured
    {
        let b3_api_url = std::env::var("B3_API_URL").unwrap_or_default();
        let gpu_worker_key = std::env::var("GPU_WORKER_KEY").unwrap_or_default();
        let worker_id = std::env::var("B3_WORKER_ID")
            .or_else(|_| std::env::var("RUNPOD_POD_ID"))
            .unwrap_or_default();

        if !b3_api_url.is_empty() && !gpu_worker_key.is_empty() && !worker_id.is_empty() {
            let token = proxy_token.clone();
            let wid = worker_id.clone();
            let key = gpu_worker_key.clone();
            let api_url = b3_api_url.trim_end_matches('/').to_string();
            let ec2_reg_flag = ec2_registered.clone();
            tokio::spawn(async move {
                spawn_ec2_registration(api_url, wid, key, token, ec2_reg_flag).await;
            });
        } else {
            info!("EC2 registration skipped — B3_API_URL, GPU_WORKER_KEY, or B3_WORKER_ID not set");
        }
    }

    // EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
    #[cfg(feature = "webrtc-experimental")]
    let routes = Router::new()
        .route("/ws", axum::routing::get(ws_handler))
        .route("/webrtc/offer", axum::routing::post(rtc_offer_handler).options(cors_preflight))
        .route("/health", axum::routing::get(health));
    #[cfg(not(feature = "webrtc-experimental"))]
    let routes = Router::new()
        .route("/ws", axum::routing::get(ws_handler))
        .route("/health", axum::routing::get(health));
    let app = routes.with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!("b3-gpu-relay (per-session + msg_id routing) listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ── Health ─────────────────────────────────────────────────────────

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.session_manager.count().await;
    let ec2_registered = state.ec2_registered.load(std::sync::atomic::Ordering::Relaxed);
    (
        [
            ("access-control-allow-origin", "*"),
            ("access-control-allow-methods", "GET"),
        ],
        axum::Json(serde_json::json!({
            "status": "ok",
            "service": "b3-gpu-relay",
            "version": env!("B3_VERSION"),
            "sessions": sessions,
            "ec2_registered": ec2_registered,
        })),
    )
}

// ── Browser WS handler ────────────────────────────────────────────

async fn ws_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let session_id = params.get("session_id").and_then(|s| s.parse::<u64>().ok());
    ws.on_upgrade(move |socket| handle_browser_ws(socket, state, session_id))
}

async fn handle_browser_ws(browser_ws: WebSocket, state: AppState, session_id: Option<u64>) {
    let (browser_tx, mut browser_rx) = browser_ws.split();

    // Reconnect to existing session by ID or create new (shared logic from b3-reliable)
    let (session, backend_rx) = state.session_manager
        .reconnect_or_create(session_id, Config::default()).await;
    let client_id = session.id;
    if let Some(rx) = backend_rx {
        session.start_bridge(rx, None).await;
        info!(client_id, "Browser WS connected — new session");
    } else {
        info!(client_id, "Browser WS reconnected — reusing session (send buffer preserved)");
    }

    // Reject duplicate WS connections: check BEFORE removing the old transport so we
    // don't touch the live connection if one is already registered.
    // `has_transport("ws")` → true means a live (or recently-dead-but-unclean) sink exists.
    // False-reject on stale sink is benign — GpuWorkerWS retries on backoff (1s) after
    // the old cleanup calls remove_transport("ws"), at which point has_transport returns false.
    if session.multi.lock().await.has_transport("ws") {
        warn!(client_id, "Rejected duplicate WS connection — session WS sink already registered");
        return;
    }

    // Remove old dead WS sink, keep send buffer for Resume replay
    session.multi.lock().await.remove_transport("ws");

    // Register WS sink on this session's MultiTransport
    let (to_browser_tx, mut to_browser_rx) = mpsc::channel::<Vec<u8>>(256);
    {
        let tx = to_browser_tx.clone();
        session.multi.lock().await.add_transport("ws", 1, move |data: &[u8]| {
            let _ = tx.try_send(data.to_vec());
        });
    }

    // Send Resume to browser — tells it our inbound state so it can reset if needed.
    // Critical after relay restart: browser may have high outbound seq from a previous
    // relay instance. Our Resume(last_contiguous=0) triggers browser's _handleResume
    // which detects the mismatch and resets its outbound channel.
    {
        let mut multi = session.multi.lock().await;
        multi.send_resume();
    }

    // Send task: drain reliable frames → browser WS
    let send_task = tokio::spawn(async move {
        let mut browser_tx = browser_tx;
        while let Some(data) = to_browser_rx.recv().await {
            if browser_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    info!(client_id, "WS sink registered on per-session MultiTransport");

    // Main loop: browser → GPU (unwrap reliable, handle echo/pong, forward)
    let gpu_tx = state.gpu_tx.clone();
    let multi = session.multi.clone();
    let pending = state.pending_requests.clone();
    'browser_loop: while let Some(Ok(msg)) = browser_rx.next().await {
        match msg {
            Message::Binary(data) => {
                let data = data.to_vec();
                if frame::is_reliable(&data) {
                    let msgs = multi.lock().await.receive(&data);
                    for m in msgs {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        // Echo probe: respond locally
                        if text.contains("\"type\":\"echo\"") || text.contains("\"type\": \"echo\"") {
                            tracing::debug!(client_id, "Echo probe received from browser — responding");
                            multi.lock().await.send(text.as_bytes(), Priority::Critical);
                            continue;
                        }
                        // Pong: discard
                        if text.contains("\"type\":\"pong\"") || text.contains("\"type\": \"pong\"") {
                            continue;
                        }
                        // Track msg_id → session for response routing
                        if let Some(msg_id) = extract_msg_id(text.as_bytes()) {
                            info!(client_id, msg_id, "Browser→GPU: tracking job for response routing");
                            pending.lock().await.insert(msg_id, client_id);
                        }
                        // rtc_inactive: remove RTC sink
                        if text.contains("\"type\":\"rtc_inactive\"") || text.contains("\"type\": \"rtc_inactive\"") {
                            session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
                            multi.lock().await.remove_transport("rtc");
                            info!(client_id, "rtc_inactive — removed RTC sink");
                            continue;
                        }
                        // Forward to GPU — on failure close the browser WS so the
                        // reliability layer detects the dead channel and surfaces the
                        // error to the application (instead of silently dropping messages
                        // while status().active remains true).
                        if gpu_tx.lock().await.send(
                            tokio_tungstenite::tungstenite::Message::Text(text.into())
                        ).await.is_err() {
                            warn!(client_id, "GPU WS send failed — closing browser WS to surface failure");
                            break 'browser_loop;
                        }
                    }
                }
            }
            Message::Text(text) => {
                // Track msg_id for unframed text messages too
                if let Some(msg_id) = extract_msg_id(text.as_bytes()) {
                    pending.lock().await.insert(msg_id, client_id);
                }
                if gpu_tx.lock().await.send(
                    tokio_tungstenite::tungstenite::Message::Text(text.into())
                ).await.is_err() {
                    warn!(client_id, "GPU WS send failed (text path) — closing browser WS");
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Cleanup
    send_task.abort();
    session.multi.lock().await.remove_transport("ws");

    // Keep session alive for reconnect — send buffer retains unacked frames.
    // But clean up sessions that haven't reconnected after 5 minutes.
    info!(client_id, "Browser WS disconnected — session kept for reconnect (5min TTL)");
    let sm = state.session_manager.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        // If no WS sink was re-registered, this session is dead
        if let Some(session) = sm.get(client_id).await {
            let sinks = session.multi.lock().await.sink_count();
            if sinks == 0 {
                sm.remove(client_id).await;
                info!(client_id, "Stale session cleaned up (no reconnect in 5min)");
            }
        }
    });
}

// ── CORS preflight ────────────────────────────────────────────────

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
async fn cors_preflight() -> impl IntoResponse {
    (
        StatusCode::NO_CONTENT,
        [
            ("access-control-allow-origin", "*"),
            ("access-control-allow-methods", "POST, OPTIONS"),
            ("access-control-allow-headers", "content-type"),
            ("access-control-max-age", "86400"),
        ],
    )
}

// ── WebRTC offer handler ──────────────────────────────────────────

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
#[derive(Deserialize)]
struct RtcOfferRequest {
    sdp: String,
    #[serde(default, alias = "client_id")]
    session_id: Option<u64>,
}

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
async fn rtc_offer_handler(
    State(state): State<AppState>,
    Json(req): Json<RtcOfferRequest>,
) -> impl IntoResponse {
    match handle_rtc_offer(&state, &req.sdp, req.session_id).await {
        Ok(answer_sdp) => (
            StatusCode::OK,
            [(
                "access-control-allow-origin",
                "*",
            )],
            axum::Json(serde_json::json!({ "sdp": answer_sdp })),
        ).into_response(),
        Err(e) => {
            warn!("RTC offer failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("access-control-allow-origin", "*")],
                e.to_string(),
            ).into_response()
        }
    }
}

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
async fn handle_rtc_offer(
    state: &AppState,
    sdp: &str,
    client_id: Option<u64>,
) -> anyhow::Result<String> {
    info!(?client_id, "WebRTC offer received");

    // ICE servers: STUN + optional TURN
    let stun_url = std::env::var("STUN_URL")
        .unwrap_or_else(|_| "stun:stun.l.google.com:19302".into());
    let turn_host = std::env::var("TURN_HOST").unwrap_or_default();
    let turn_secret = std::env::var("TURN_SECRET").unwrap_or_default();

    let mut ice_servers = vec![IceServer {
        urls: vec![stun_url],
        username: None,
        credential: None,
    }];

    if !turn_secret.is_empty() && !turn_host.is_empty() {
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
    }

    let config = PeerConfig {
        ice_servers,
        force_relay: false,
    };

    let mut peer = B3Peer::new(config)?;
    peer.set_remote_description(sdp, "offer")?;

    // Collect answer SDP
    let mut answer_sdp = None;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, peer.next_event()).await {
            Ok(Some(PeerEvent::LocalDescription { sdp, sdp_type })) => {
                if sdp_type == "answer" {
                    answer_sdp = Some(sdp);
                }
            }
            Ok(Some(PeerEvent::GatheringComplete)) => {
                if answer_sdp.is_some() {
                    if let Some(final_sdp) = peer.local_description() {
                        answer_sdp = Some(final_sdp);
                    }
                    break;
                }
            }
            Ok(Some(PeerEvent::ConnectionStateChange(state))) => {
                info!(state, "RTC connection state during offer");
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }

    let answer = answer_sdp.ok_or_else(|| anyhow::anyhow!("answer generation timeout"))?;

    // Find or create session for this browser
    let session = if let Some(cid) = client_id {
        match state.session_manager.get(cid).await {
            Some(s) => {
                info!(client_id = cid, "RTC associated with existing session");
                s
            }
            None => {
                // Create session for RTC-only browser (no WS yet)
                let (s, rx) = state.session_manager.create_with_id(cid, Config::default()).await;
                s.start_bridge(rx, None).await;
                info!(client_id = cid, "RTC created new session");
                s
            }
        }
    } else {
        // Do NOT fall back to most_recent — it cross-wires stale browser's
        // RTC to an active session. Reject offers without session_id.
        anyhow::bail!("No session_id in RTC offer — rejecting");
    };

    // Abort any existing RTC bridge for this session — one peer per session
    {
        let mut bridges = state.rtc_bridges.lock().await;
        if let Some(old_handle) = bridges.remove(&session.id) {
            info!(client_id = session.id, "Aborting previous RTC bridge (new offer arrived)");
            old_handle.abort();
        }
    }

    // Spawn bridge task and track its abort handle
    let gpu_tx = state.gpu_tx.clone();
    let pending = state.pending_requests.clone();
    let rtc_bridges = state.rtc_bridges.clone();
    let session_id = session.id;
    let handle = tokio::spawn(async move {
        rtc_bridge_task(peer, session, gpu_tx, pending).await;
    });
    rtc_bridges.lock().await.insert(session_id, handle.abort_handle());

    info!("WebRTC answer generated");
    Ok(answer)
}

// EXPERIMENTAL: WebRTC support is not actively maintained and may not work.
#[cfg(feature = "webrtc-experimental")]
async fn rtc_bridge_task(
    mut peer: B3Peer,
    session: Arc<b3_reliable::Session>,
    gpu_tx: GpuTx,
    pending: Arc<Mutex<HashMap<String, u64>>>,
) {
    let client_id = session.id;
    let mut child_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // No hard deadline. Cleanup via ICE state machine only.
    let mut disconnect_deadline: Option<tokio::time::Instant> = None;

    loop {
        let disconnect_sleep = async {
            match disconnect_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            _ = disconnect_sleep => {
                warn!(client_id, "RTC peer disconnected for 30s — closing bridge");
                break;
            }
            event = peer.next_event() => {
                match event {
                    Some(PeerEvent::ConnectionStateChange(state)) => {
                        info!(client_id, state, "RTC state change");
                        if state == "closed" || state == "failed" {
                            session.multi.lock().await.remove_transport("rtc");
                            session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        if state == "disconnected" {
                            session.multi.lock().await.remove_transport("rtc");
                            session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
                            disconnect_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
                        }
                        if state == "connected" {
                            disconnect_deadline = None;
                        }
                    }
                    Some(PeerEvent::IncomingChannel { label, sender, mut receiver }) => {
                        info!(client_id, label, "RTC data channel opened");

                        match label.as_str() {
                            "terminal" | "data" => {
                                // Register RTC sink on session MultiTransport (priority 0 = preferred)
                                let sender_for_multi = sender.clone();
                                let multi_for_remove = session.multi.clone();
                                session.multi.lock().await.add_transport("rtc", 0, move |data: &[u8]| {
                                    let s = sender_for_multi.clone();
                                    let m = multi_for_remove.clone();
                                    let d = data.to_vec();
                                    tokio::spawn(async move {
                                        if s.send_raw(d).await.is_err() {
                                            // Channel dead — remove zombie sink so WS takes over
                                            m.lock().await.remove_transport("rtc");
                                            warn!("GPU RTC send failed — removed zombie sink, WS will take over");
                                        }
                                    });
                                });
                                session.rtc_active.store(true, std::sync::atomic::Ordering::Relaxed);
                                info!(client_id, "RTC sink registered (priority 0)");

                                // Send Resume to browser via RTC — same as WS path.
                                // Tells browser our inbound state so it can reset if seq mismatched.
                                session.multi.lock().await.send_resume();

                                // Data channel → GPU (unwrap reliable, forward)
                                let multi = session.multi.clone();
                                let gpu = gpu_tx.clone();
                                let pend = pending.clone();
                                let cid = client_id;
                                child_tasks.push(tokio::spawn(async move {
                                    info!(client_id = cid, "RTC data receiver loop started");
                                    while let Some(msg) = receiver.recv().await {
                                        match &msg {
                                            ChannelMessage::Binary(data) => {
                                                info!(client_id = cid, len = data.len(), first_byte = format!("0x{:02x}", data.get(0).copied().unwrap_or(0)), "RTC recv: Binary");
                                            }
                                            ChannelMessage::Ping => {
                                                info!(client_id = cid, "RTC recv: Ping");
                                            }
                                            ChannelMessage::Pong => {}
                                            ChannelMessage::Json(_v) => {
                                                info!(client_id = cid, "RTC recv: Json");
                                            }
                                        }
                                        if let ChannelMessage::Binary(data) = msg {
                                            if frame::is_reliable(&data) {
                                                let msgs = multi.lock().await.receive(&data);
                                                info!(client_id = cid, msg_count = msgs.len(), "RTC reliable unwrap");
                                                for m in msgs {
                                                    let text = String::from_utf8_lossy(&m.payload).to_string();
                                                    if text.contains("\"type\":\"echo\"") || text.contains("\"type\": \"echo\"") {
                                                        multi.lock().await.send(text.as_bytes(), Priority::Critical);
                                                        continue;
                                                    }
                                                    if text.contains("\"type\":\"pong\"") || text.contains("\"type\": \"pong\"") {
                                                        continue;
                                                    }
                                                    if let Some(msg_id) = extract_msg_id(text.as_bytes()) {
                                                        info!(client_id = cid, msg_id, "RTC→GPU: tracking job for response routing");
                                                        pend.lock().await.insert(msg_id, cid);
                                                    }
                                                    let _ = gpu.lock().await.send(
                                                        tokio_tungstenite::tungstenite::Message::Text(text.into())
                                                    ).await;
                                                }
                                            }
                                        }
                                    }
                                }));
                            }
                            "control" => {
                                // Control channel: handle ping/pong
                                let ctrl_sender = sender.clone();
                                child_tasks.push(tokio::spawn(async move {
                                    while let Some(msg) = receiver.recv().await {
                                        match msg {
                                            ChannelMessage::Ping => {
                                                let _ = ctrl_sender.send(ChannelMessage::Pong).await;
                                            }
                                            _ => {}
                                        }
                                    }
                                }));
                            }
                            _ => {
                                warn!(client_id, label, "Unknown RTC data channel");
                            }
                        }
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
        }
    }

    // Cleanup
    for task in &child_tasks {
        task.abort();
    }
    session.multi.lock().await.remove_transport("rtc");
    session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
    info!(client_id, child_count = child_tasks.len(), "RTC bridge ended");
}

// ── Helpers ────────────────────────────────────────────────────────

fn classify_priority(json_text: &str) -> Priority {
    if json_text.contains("\"type\":\"progress\"") || json_text.contains("\"type\": \"progress\"") {
        return Priority::BestEffort;
    }
    if json_text.contains("\"type\":\"ping\"") || json_text.contains("\"type\": \"ping\"") {
        return Priority::BestEffort;
    }
    Priority::Critical
}

/// Extract msg_id from a JSON payload (fast string search, no full parse).
fn extract_msg_id(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    // Prefer job_id > stream_id > msg_id — the GPU worker responds with job_id,
    // and msg_id may exist in nested input fields (daemon's TTS msg_id inside submit.input)
    for key in &["\"job_id\"", "\"stream_id\"", "\"msg_id\""] {
        if let Some(idx) = text.find(key) {
            let after_key = &text[idx + key.len()..];
            let after_colon = after_key.trim_start().strip_prefix(':')?;
            let after_space = after_colon.trim_start();
            let after_quote = after_space.strip_prefix('"')?;
            let end = after_quote.find('"')?;
            return Some(after_quote[..end].to_string());
        }
    }
    None
}

/// Check if a GPU response is the final message for a msg_id.
/// Final messages: completed result, error, or non-streaming response.
fn is_final_response(payload: &[u8]) -> bool {
    let text = match std::str::from_utf8(payload) {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Only explicit terminal states are final.
    // Do NOT treat unknown types as final — "accepted", "chunk", etc. are intermediate.
    text.contains("\"type\":\"done\"") || text.contains("\"type\": \"done\"")
        || text.contains("\"type\":\"error\"") || text.contains("\"type\": \"error\"")
        || text.contains("\"type\":\"result\"") || text.contains("\"type\": \"result\"")
}

// ── Keypair management ────────────────────────────────────────────────

fn keypair_path() -> std::path::PathBuf {
    std::path::PathBuf::from("./gpu-worker.key")
}

fn load_keypair() -> Option<StaticSecret> {
    let raw = std::fs::read_to_string(keypair_path()).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw.trim()).ok()?;
    if bytes.len() != 32 { return None; }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(StaticSecret::from(arr))
}

fn load_or_generate_keypair() -> StaticSecret {
    if let Some(secret) = load_keypair() {
        info!("[EC2Reg] Loaded existing gpu-worker keypair");
        return secret;
    }
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let secret = StaticSecret::from(bytes);
    let b64 = base64::engine::general_purpose::STANDARD.encode(secret.to_bytes());
    if let Err(e) = std::fs::write(keypair_path(), &b64) {
        warn!("[EC2Reg] Could not save keypair to {}: {e}", keypair_path().display());
    } else {
        info!("[EC2Reg] Generated new gpu-worker keypair at {}", keypair_path().display());
    }
    secret
}

// ── EC2 registration ──────────────────────────────────────────────────

/// Connects to EC2 /api/gpu-register, maintains a control-only WS, and spawns
/// a fresh `run_gpu_session` task for each browser session.
///
/// Registration WS is now control-only (no data). Per-session data flows on
/// isolated dial-back connections via `b3-tunnel::run_tunnel_client`.
///
/// Reconnects with exponential backoff on disconnect or Pong timeout.
async fn spawn_ec2_registration(
    api_url: String,
    worker_id: String,
    gpu_worker_key: String,
    proxy_token: String,
    ec2_registered: Arc<std::sync::atomic::AtomicBool>,
) {
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{client::IntoClientRequest, http::header::AUTHORIZATION, Message},
    };

    let secret = Arc::new(load_or_generate_keypair());
    let pubkey_b64 = base64::engine::general_purpose::STANDARD
        .encode(PublicKey::from(secret.as_ref()).as_bytes());

    let is_runpod = std::env::var("RUNPOD_POD_ID").is_ok();
    let capabilities = serde_json::json!({
        "models": ["transcribe", "synthesize", "diarize", "embed"],
        "gpu": std::env::var("GPU_TYPE").unwrap_or_else(|_| "unknown".into()),
        "category": if is_runpod { "serverless" } else { "dedicated" },
    });
    let relay_version = env!("CARGO_PKG_VERSION");

    // EC2 session endpoint base — used by b3-tunnel for dial-back URLs.
    // Format: wss://babel3.com/api/gpu-session?session_id=Y
    let ec2_session_base = format!(
        "{}/api/gpu-session",
        api_url.replace("https://", "wss://").replace("http://", "ws://")
    );

    info!(
        worker_id,
        pubkey_prefix = %&pubkey_b64[..8.min(pubkey_b64.len())],
        "[EC2Reg] Starting EC2 registration loop (reverse tunnel)"
    );

    let mut backoff = std::time::Duration::from_secs(1);

    loop {
        let register_url = format!(
            "{}/api/gpu-register?worker_id={}",
            api_url.replace("https://", "wss://").replace("http://", "ws://"),
            worker_id
        );

        let mut request = match register_url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                warn!("[EC2Reg] Invalid register URL: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue;
            }
        };
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {}", gpu_worker_key).parse().unwrap(),
        );

        let ws = match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            connect_async(request),
        ).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                warn!("[EC2Reg] Connection failed: {e} — retry in {}s", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue;
            }
            Err(_) => {
                warn!("[EC2Reg] Connection timeout — retry in {}s", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                continue;
            }
        };

        backoff = std::time::Duration::from_secs(1);
        ec2_registered.store(true, std::sync::atomic::Ordering::Relaxed);
        info!(worker_id, "[EC2Reg] Connected to EC2 gpu-register (control-only)");

        let (ec2_tx, ec2_rx) = ws.split();

        // Send registration frame
        use futures_util::SinkExt as _;
        let mut ec2_tx_reg = ec2_tx;
        let reg_frame = serde_json::json!({
            "type": "register",
            "pubkey": &pubkey_b64,
            "proxy_token": &proxy_token,
            "capabilities": &capabilities,
            "version": relay_version,
        }).to_string();

        if ec2_tx_reg.send(Message::Text(reg_frame)).await.is_err() {
            warn!("[EC2Reg] Failed to send registration frame");
            continue;
        }

        // Hand off to b3-tunnel control loop.
        // On each session_request: b3-tunnel dials back EC2 /api/gpu-session?session_id=Y
        // and calls run_gpu_session(session_id, ec2_session_ws).
        // Keepalive: Ping every 5s, timeout after 10s — run_tunnel_client owns this.
        let secret_clone = secret.clone();
        let wid = worker_id.clone();
        let local_gpu_url = std::env::var("LOCAL_GPU_URL")
            .unwrap_or_else(|_| "http://localhost:5125".to_string());
        let gpu_secret = std::env::var("GPU_SECRET").unwrap_or_default();
        let logs_local_url = local_gpu_url.clone();
        let logs_gpu_secret = gpu_secret.clone();

        let result = run_tunnel_client(
            ec2_tx_reg,
            ec2_rx,
            worker_id.clone(),
            ec2_session_base.clone(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(10),
            None, // GPU session endpoint auth: GPU_WORKER_KEY checked on control WS upgrade
            move |session_id, ec2_ws| {
                let secret = secret_clone.clone();
                let worker_id = wid.clone();
                async move {
                    run_gpu_session(session_id, ec2_ws, secret, worker_id).await;
                }
            },
            Some(move |msg: serde_json::Value| {
                let url = logs_local_url.clone();
                let secret = logs_gpu_secret.clone();
                async move {
                    handle_logs_request(msg, url, secret).await
                }
            }),
        ).await;

        match result {
            Ok(()) => info!(worker_id, "[EC2Reg] Control WS closed cleanly — reconnecting"),
            Err(e) => warn!(worker_id, "[EC2Reg] Control WS error: {e} — reconnecting"),
        }

        ec2_registered.store(false, std::sync::atomic::Ordering::Relaxed);
        info!(worker_id, "[EC2Reg] ec2_registered cleared — waiting for reconnect");

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }
}

// ── logs_request handler ─────────────────────────────────────────────
//
// Called by b3-tunnel's extra_handler when EC2 sends:
//   {"type":"logs_request","request_id":"<uuid>","lines":200}
// Fetches http://localhost:5125/logs?lines=N with GPU_SECRET Bearer auth,
// returns the response as JSON to be sent back on the control WS:
//   {"type":"logs_response","request_id":"<uuid>","text":"<log lines>"}
async fn handle_logs_request(
    msg: serde_json::Value,
    local_gpu_url: String,
    gpu_secret: String,
) -> Option<String> {
    let request_id = msg.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let lines = msg.get("lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(200);

    let url = format!("{}/logs?lines={}", local_gpu_url.trim_end_matches('/'), lines);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[Logs] Failed to build HTTP client: {e}");
            let resp = serde_json::json!({
                "type": "logs_response",
                "request_id": &request_id,
                "error": format!("client build failed: {e}"),
            });
            return Some(resp.to_string());
        }
    };

    let mut req = client.get(&url);
    if !gpu_secret.is_empty() {
        req = req.bearer_auth(&gpu_secret);
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let text = resp.text().await.unwrap_or_default();
            let response = serde_json::json!({
                "type": "logs_response",
                "request_id": &request_id,
                "text": text,
            });
            Some(response.to_string())
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            warn!("[Logs] Worker /logs returned HTTP {status}");
            let response = serde_json::json!({
                "type": "logs_response",
                "request_id": &request_id,
                "error": format!("worker returned HTTP {status}"),
            });
            Some(response.to_string())
        }
        Err(e) => {
            warn!("[Logs] Failed to fetch worker /logs: {e}");
            let response = serde_json::json!({
                "type": "logs_response",
                "request_id": &request_id,
                "error": format!("fetch failed: {e}"),
            });
            Some(response.to_string())
        }
    }
}

/// Handle one isolated browser↔GPU session.
///
/// Called by b3-tunnel for each `session_request` with a fresh dial-back WS to EC2.
/// Opens a new connection to the local GPU worker and splices the two WebSockets.
///
/// Each session gets its own EncryptedChannel + ReliableChannel + GPU WS connection.
/// No shared state. When the session ends (either side closes), everything is dropped.
///
/// # Handshake ordering invariant
///
/// GPU worker MUST be connected before ECDH handshake_ok is sent. If GPU connect fails,
/// enc.receive is not called — browser times out and retries. This matches the pre-refactor
/// behavior in handle_ec2_relay_frame.
async fn run_gpu_session(
    session_id: String,
    ec2_ws: TungsteniteWs,
    secret: Arc<StaticSecret>,
    worker_id: String,
) {
    use tokio_tungstenite::tungstenite::Message as TMsg;

    info!(worker_id, session_id, "[Session] Starting GPU session");

    // EC2 WS split: tx for encrypted output, rx for browser frames
    let (ec2_tx_raw, mut ec2_rx) = ec2_ws.split();

    // EncryptedChannel output → EC2 WS via mpsc (avoids holding tx across await)
    let (to_ec2_tx, mut to_ec2_rx) = mpsc::channel::<Vec<u8>>(256);
    let to_ec2_enc = to_ec2_tx.clone();
    let secret_bytes: [u8; 32] = secret.to_bytes();
    let mut enc = EncryptedChannel::new(
        b3_reliable::Config::default(),
        StaticSecret::from(secret_bytes),
        move |data: &[u8]| {
            let _ = to_ec2_enc.try_send(data.to_vec());
        },
    );
    enc.set_version(env!("CARGO_PKG_VERSION"));

    // Spawn EC2 writer task
    let wid_writer = worker_id.clone();
    let sid_writer = session_id.clone();
    let ec2_writer_handle = tokio::spawn(async move {
        let mut ec2_tx = ec2_tx_raw;
        while let Some(data) = to_ec2_rx.recv().await {
            let msg = match std::str::from_utf8(&data) {
                Ok(s) => TMsg::Text(s.to_string()),
                Err(_) => TMsg::Binary(data),
            };
            if ec2_tx.send(msg).await.is_err() { break; }
        }
        info!(worker_id = %wid_writer, session_id = %sid_writer, "[Session] EC2 writer ended");
    });

    // Outer ReliableChannel: ACKs, Resume, chunk tag stripping
    let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut outer_rc = ReliableChannel::new(
        Config { max_send_buffer: 1000, ack_interval_ms: 200, max_frame_payload: 262144 },
        move |data: &[u8]| { let _ = flush_tx.send(data.to_vec()); },
    );

    // GPU WS connection + reader
    let (gpu_to_ec2_tx, mut gpu_to_ec2_rx) = mpsc::channel::<Vec<u8>>(256);
    let mut gpu_writer = None::<futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        TMsg,
    >>;
    let mut gpu_reader_handle = None::<tokio::task::JoinHandle<()>>;

    // Attempt initial GPU connect (non-fatal — retry on handshake)
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio_tungstenite::connect_async(&gpu_ws_url()),
    ).await {
        Ok(Ok((ws, _))) => {
            let (gtx, grx) = ws.split();
            gpu_writer = Some(gtx);
            let tx = gpu_to_ec2_tx.clone();
            let wid2 = worker_id.clone();
            let sid2 = session_id.clone();
            gpu_reader_handle = Some(tokio::spawn(async move {
                let mut grx = grx;
                while let Some(msg) = grx.next().await {
                    match msg {
                        Ok(TMsg::Text(t)) => { if tx.send(t.into_bytes()).await.is_err() { break; } }
                        Ok(TMsg::Binary(b)) => { if tx.send(b.to_vec()).await.is_err() { break; } }
                        Ok(TMsg::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                info!(worker_id = %wid2, session_id = %sid2, "[Session] GPU WS reader ended");
            }));
            info!(worker_id, session_id, "[Session] GPU worker connected");
        }
        Ok(Err(e)) => warn!(worker_id, session_id, "[Session] GPU initial connect failed: {e} — will retry on handshake"),
        Err(_) => warn!(worker_id, session_id, "[Session] GPU initial connect timeout — will retry on handshake"),
    }

    // ACK tick for EncryptedChannel + outer RC
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(100));
    tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(worker_id, session_id, "[Session] Session loop active");

    loop {
        tokio::select! {
            msg = ec2_rx.next() => {
                match msg {
                    Some(Ok(TMsg::Text(t))) => {
                        handle_session_frame(
                            &worker_id, &session_id, t.as_bytes(),
                            &mut enc, &mut gpu_writer, &mut gpu_reader_handle,
                            &gpu_to_ec2_tx, &mut outer_rc, &mut flush_rx,
                        ).await;
                    }
                    Some(Ok(TMsg::Binary(b))) => {
                        handle_session_frame(
                            &worker_id, &session_id, &b,
                            &mut enc, &mut gpu_writer, &mut gpu_reader_handle,
                            &gpu_to_ec2_tx, &mut outer_rc, &mut flush_rx,
                        ).await;
                    }
                    Some(Ok(TMsg::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }

            Some(data) = gpu_to_ec2_rx.recv() => {
                if enc.is_ready() {
                    let before_drain = flush_rx.len();
                    outer_rc.send(&data, Priority::Critical);
                    let mut drained = 0usize;
                    while let Ok(outbound) = flush_rx.try_recv() {
                        enc.send(&outbound, Priority::Critical);
                        drained += 1;
                    }
                    tracing::debug!(
                        worker_id, session_id,
                        bytes = data.len(),
                        flush_before = before_drain,
                        flush_drained = drained,
                        "[Session] GPU→EC2: forwarded frame"
                    );
                } else {
                    tracing::warn!(
                        worker_id, session_id,
                        bytes = data.len(),
                        "[Session] GPU→EC2: enc NOT ready — frame dropped"
                    );
                }
            }

            _ = tick_interval.tick() => {
                if enc.is_ready() {
                    enc.tick();
                    outer_rc.tick();
                    while let Ok(outbound) = flush_rx.try_recv() {
                        enc.send(&outbound, Priority::Critical);
                    }
                }
            }
        }
    }

    ec2_writer_handle.abort();
    if let Some(h) = gpu_reader_handle { h.abort(); }
    info!(worker_id, session_id, "[Session] GPU session ended");
}

/// Handle a single frame from EC2 on a per-session WS.
///
/// Simplified vs the old handle_ec2_relay_frame: no browser_disconnected reset
/// (session ends → task drops → EncryptedChannel dropped naturally). No
/// gpu_reconnecting flag (GPU connect happens once at session start or on handshake).
///
/// # Handshake ordering invariant
/// GPU MUST be connected before enc.receive (ECDH completes → handshake_ok sent).
async fn handle_session_frame(
    worker_id: &str,
    session_id: &str,
    data: &[u8],
    enc: &mut EncryptedChannel,
    gpu_writer: &mut Option<futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        tokio_tungstenite::tungstenite::Message,
    >>,
    gpu_reader_handle: &mut Option<tokio::task::JoinHandle<()>>,
    gpu_to_ec2_tx: &mpsc::Sender<Vec<u8>>,
    outer_rc: &mut ReliableChannel,
    flush_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) {
    use tokio_tungstenite::tungstenite::Message as TMsg;

    // ── HANDSHAKING path ─────────────────────────────────────────────
    if !enc.is_ready() {
        // Ensure GPU is connected before completing ECDH (handshake_ok must not be sent
        // until GPU is ready — browser would start sending encrypted frames to /dev/null).
        if gpu_writer.is_none() {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio_tungstenite::connect_async(&gpu_ws_url()),
            ).await {
                Ok(Ok((ws, _))) => {
                    let (gtx, grx) = ws.split();
                    *gpu_writer = Some(gtx);
                    let tx = gpu_to_ec2_tx.clone();
                    let wid = worker_id.to_string();
                    let sid = session_id.to_string();
                    *gpu_reader_handle = Some(tokio::spawn(async move {
                        let mut grx = grx;
                        while let Some(msg) = grx.next().await {
                            match msg {
                                Ok(TMsg::Text(t)) => { if tx.send(t.into_bytes()).await.is_err() { break; } }
                                Ok(TMsg::Binary(b)) => { if tx.send(b.to_vec()).await.is_err() { break; } }
                                Ok(TMsg::Close(_)) | Err(_) => break,
                                _ => {}
                            }
                        }
                        info!(worker_id = %wid, session_id = %sid, "[Session] GPU WS reader (handshake-connect) ended");
                    }));
                    info!(worker_id, session_id, "[Session] GPU connected on handshake — completing ECDH");
                }
                Ok(Err(e)) => {
                    warn!(worker_id, session_id, "[Session] GPU connect failed on handshake: {e}");
                    return;
                }
                Err(_) => {
                    warn!(worker_id, session_id, "[Session] GPU connect timeout on handshake");
                    return;
                }
            }
        }
        enc.receive(data);
        info!(worker_id, session_id, "[Session] ECDH complete — handshake_ok sent");
        return;
    }

    // ── READY path ───────────────────────────────────────────────────

    // Reconnect GPU WS if it was closed (e.g., per-job close or send failure).
    // gpu_writer is cleared on send failure below — reconnect before forwarding next frame.
    if gpu_writer.is_none() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio_tungstenite::connect_async(&gpu_ws_url()),
        ).await {
            Ok(Ok((ws, _))) => {
                let (gtx, grx) = ws.split();
                *gpu_writer = Some(gtx);
                let tx = gpu_to_ec2_tx.clone();
                let wid = worker_id.to_string();
                let sid = session_id.to_string();
                if let Some(h) = gpu_reader_handle.take() { h.abort(); }
                *gpu_reader_handle = Some(tokio::spawn(async move {
                    let mut grx = grx;
                    while let Some(msg) = grx.next().await {
                        match msg {
                            Ok(TMsg::Text(t)) => { if tx.send(t.into_bytes()).await.is_err() { break; } }
                            Ok(TMsg::Binary(b)) => { if tx.send(b.to_vec()).await.is_err() { break; } }
                            Ok(TMsg::Close(_)) | Err(_) => break,
                            _ => {}
                        }
                    }
                    info!(worker_id = %wid, session_id = %sid, "[Session] GPU WS reader (reconnect) ended");
                }));
                info!(worker_id, session_id, "[Session] GPU WS reconnected");
            }
            Ok(Err(e)) => {
                warn!(worker_id, session_id, "[Session] GPU WS reconnect failed: {e} — dropping frame");
                return;
            }
            Err(_) => {
                warn!(worker_id, session_id, "[Session] GPU WS reconnect timeout — dropping frame");
                return;
            }
        }
    }

    let msgs = enc.receive(data);
    for m in msgs {
        let delivered = outer_rc.receive(&m.payload);
        for msg in delivered {
            if let Some(ref mut gtx) = gpu_writer {
                let msg_out = match std::str::from_utf8(&msg.payload) {
                    Ok(s) => TMsg::Text(s.to_string()),
                    Err(_) => TMsg::Binary(msg.payload.into()),
                };
                if gtx.send(msg_out).await.is_err() {
                    warn!(worker_id, session_id, "[Session] GPU WS send failed — clearing writer for reconnect");
                    *gpu_writer = None;
                    if let Some(h) = gpu_reader_handle.take() { h.abort(); }
                }
            }
        }
    }
    while let Ok(outbound) = flush_rx.try_recv() {
        enc.send(&outbound, Priority::Critical);
    }
}

