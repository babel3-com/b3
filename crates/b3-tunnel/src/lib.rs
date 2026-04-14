//! Reverse tunnel client — shared logic for GPU relay and daemon proxy.
//!
//! Both the GPU relay and the daemon are behind NAT/Docker and cannot accept
//! inbound connections from EC2. This crate implements the client side of a
//! reverse tunnel: the worker maintains a persistent **control WS** to EC2,
//! receives `session_request` messages, dials back to EC2's session endpoint,
//! and hands the established connection to a caller-supplied `session_handler`.
//!
//! # Pattern (bore-style)
//!
//! ```text
//! Browser connects to EC2 /api/gpu-proxy-ws
//!   → EC2 sends {"type":"session_request","session_id":"Y"} on control WS
//!   → b3-tunnel dials EC2 /api/gpu-session?session_id=Y (dial-back)
//!   → EC2 pairs incoming dial-back with waiting browser by session_id
//!   → b3-tunnel calls session_handler(session_id, ec2_ws)
//!   → session_handler opens local service WS and splices (or does ECDH, etc.)
//!   → Session ends → both sides close → nothing shared
//! ```
//!
//! No shared slot. No ownership race. Session pipe born and dies with the browser session.
//!
//! # Per-caller session handlers
//!
//! The dial-back and control loop are generic. Per-session work is caller-specific:
//! - GPU relay: open `ws://localhost:5125` + splice bidirectionally (plain)
//! - Daemon: open local `/ws-proxy` + ECDH + HTTP-over-WS (application-layer)
//!
//! # Keepalive
//!
//! `run_tunnel_client` owns the full control WS lifecycle including Ping/Pong keepalive.
//! The caller passes `ping_interval` and `pong_timeout`. If no Pong is received within
//! `pong_timeout`, `run_tunnel_client` returns `Err`. The caller's reconnect loop handles
//! reconnection — no `last_pong` tracking needed in the caller.
//!
//! # Usage
//!
//! ```rust,ignore
//! // After connecting control WS and sending registration frame:
//! let (control_tx, control_rx) = control_ws.split();
//! run_tunnel_client(
//!     control_tx, control_rx,
//!     worker_id,
//!     "wss://babel3.com/api/gpu-session".to_string(),
//!     Duration::from_secs(5),   // ping_interval
//!     Duration::from_secs(10),  // pong_timeout
//!     |session_id, ec2_ws| async move {
//!         // GPU relay: open local GPU WS + splice
//!         let gpu_ws = connect_async("ws://localhost:5125").await.unwrap().0;
//!         splice_ws(ec2_ws, gpu_ws, &session_id).await;
//!     },
//! ).await?;
//! ```

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

pub type TungsteniteWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Run the reverse tunnel control loop on an already-connected control WebSocket.
///
/// On `{"type":"session_request","session_id":"Y"}`:
/// 1. Dials back to `ec2_session_base?session_id=Y`
/// 2. Calls `session_handler(session_id, dial_back_ws)` in a new task
///
/// On any other unknown message type, if `extra_handler` is `Some`, calls it with
/// the parsed JSON value. The handler returns `Option<String>` — if `Some(json)`,
/// the string is sent back as a Text frame on the control WS. The handler runs in
/// a spawned task; its response is delivered via an internal mpsc channel.
///
/// Owns Ping/Pong keepalive — sends WS Ping every `ping_interval`, returns
/// `Err` if no Pong received within `pong_timeout`. Caller's reconnect loop
/// handles reconnection.
///
/// Returns `Ok(())` on clean WS close, `Err` on timeout or error.
pub async fn run_tunnel_client<Tx, Rx, F, Fut, EH, EHFut>(
    mut control_tx: Tx,
    mut control_rx: Rx,
    worker_id: String,
    ec2_session_base: String,
    ping_interval: Duration,
    pong_timeout: Duration,
    dial_back_token: Option<String>,
    session_handler: F,
    extra_handler: Option<EH>,
) -> anyhow::Result<()>
where
    Tx: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin + Send + 'static,
    Rx: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
    F: Fn(String, TungsteniteWs) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    EH: Fn(serde_json::Value) -> EHFut + Send + Sync + 'static,
    EHFut: Future<Output = Option<String>> + Send + 'static,
{
    info!(worker_id, "tunnel_client: control loop active");

    let session_handler = Arc::new(session_handler);
    let extra_handler = extra_handler.map(Arc::new);
    // Channel for extra handler responses → control_tx (avoids holding tx across await)
    let (extra_resp_tx, mut extra_resp_rx) = mpsc::unbounded_channel::<String>();
    let mut ping_tick = tokio::time::interval(ping_interval);
    let mut last_pong = Instant::now();

    loop {
        tokio::select! {
            msg = control_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        let v: serde_json::Value = match serde_json::from_str(&t) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(worker_id, "tunnel_client: invalid JSON on control WS: {e}");
                                continue;
                            }
                        };

                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("session_request") => {
                                let session_id = match v.get("session_id").and_then(|s| s.as_str()) {
                                    Some(s) => s.to_string(),
                                    None => {
                                        warn!(worker_id, "tunnel_client: session_request missing session_id");
                                        continue;
                                    }
                                };

                                info!(worker_id, session_id, "tunnel_client: session requested — dialing back EC2");

                                let base = ec2_session_base.clone();
                                let wid = worker_id.clone();
                                let handler = session_handler.clone();
                                let sid = session_id.clone();
                                let token = dial_back_token.clone();
                                tokio::spawn(async move {
                                    match dial_ec2_session(&sid, &base, token.as_deref()).await {
                                        Ok(ws) => {
                                            handler(sid, ws).await;
                                        }
                                        Err(e) => {
                                            warn!(
                                                worker_id = %wid,
                                                session_id = %sid,
                                                "tunnel_client: EC2 dial-back failed: {e}"
                                            );
                                        }
                                    }
                                });
                            }

                            // Relay liveness probe from EC2 sent as text JSON Ping —
                            // respond with WS-level Pong and update last_pong.
                            Some("ping") => {
                                last_pong = Instant::now();
                                let _ = control_tx.send(Message::Pong(vec![])).await;
                            }

                            other => {
                                if let Some(ref handler) = extra_handler {
                                    let handler = handler.clone();
                                    let v_clone = v.clone();
                                    let resp_tx = extra_resp_tx.clone();
                                    tokio::spawn(async move {
                                        if let Some(resp) = handler(v_clone).await {
                                            let _ = resp_tx.send(resp);
                                        }
                                    });
                                } else {
                                    // Forward-compatible: unknown control messages logged and ignored.
                                    warn!(
                                        worker_id,
                                        msg_type = ?other,
                                        "tunnel_client: unknown control message type — ignoring"
                                    );
                                }
                            }
                        }
                    }

                    Some(Ok(Message::Ping(data))) => {
                        // WS-level Ping from EC2 — echo Pong and update last_pong.
                        last_pong = Instant::now();
                        let _ = control_tx.send(Message::Pong(data)).await;
                    }

                    Some(Ok(Message::Pong(_))) => {
                        // Response to our WS-level Ping — connection is alive.
                        last_pong = Instant::now();
                    }

                    Some(Ok(Message::Close(_))) | None => {
                        info!(worker_id, "tunnel_client: control WS closed");
                        return Ok(());
                    }

                    Some(Err(e)) => {
                        warn!(worker_id, "tunnel_client: control WS error: {e}");
                        bail!("control WS error: {e}");
                    }

                    Some(Ok(_)) => {} // Binary or other frames on control WS — ignore
                }
            }

            // Drain extra handler responses (e.g. logs_response from GPU relay)
            Some(resp) = extra_resp_rx.recv() => {
                if control_tx.send(Message::Text(resp)).await.is_err() {
                    bail!("control WS send failed (extra handler response)");
                }
            }

            _ = ping_tick.tick() => {
                if last_pong.elapsed() > pong_timeout {
                    warn!(
                        worker_id,
                        elapsed_secs = last_pong.elapsed().as_secs(),
                        "tunnel_client: Pong timeout — dropping control connection"
                    );
                    bail!("control WS pong timeout");
                }
                if control_tx.send(Message::Ping(vec![])).await.is_err() {
                    bail!("control WS send failed");
                }
            }
        }
    }
}

/// Dial back to the EC2 session endpoint.
///
/// Called internally by [`run_tunnel_client`] on `session_request`.
///
/// `ec2_session_base`: e.g. `"wss://babel3.com/api/gpu-session"`
/// `auth_token`: if `Some`, added as `Authorization: Bearer <token>` on the WS upgrade request.
async fn dial_ec2_session(
    session_id: &str,
    ec2_session_base: &str,
    auth_token: Option<&str>,
) -> anyhow::Result<TungsteniteWs> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

    let url = format!("{}?session_id={}", ec2_session_base, session_id);
    let mut request = url.as_str().into_client_request()
        .context("invalid EC2 session dial-back URL")?;

    if let Some(token) = auth_token {
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {}", token).parse().context("invalid auth token header value")?,
        );
    }

    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .context("EC2 session dial-back timeout")?
    .context("EC2 session dial-back failed")?;
    Ok(ws)
}

/// Splice two WebSocket connections bidirectionally until either side closes.
///
/// For GPU relay sessions: open local GPU WS + call this to pipe frames.
/// For application-layer sessions (ECDH, HTTP-over-WS): implement the loop directly.
pub async fn splice_ws(ws_a: TungsteniteWs, ws_b: TungsteniteWs, session_id: &str) {
    let (mut a_tx, mut a_rx) = ws_a.split();
    let (mut b_tx, mut b_rx) = ws_b.split();

    let a_to_b = async {
        while let Some(msg) = a_rx.next().await {
            match msg {
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(msg) => {
                    if b_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    let b_to_a = async {
        while let Some(msg) = b_rx.next().await {
            match msg {
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(msg) => {
                    if a_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = a_to_b => {}
        _ = b_to_a => {}
    }

    info!(session_id, "splice_ws: pipe closed");
}
