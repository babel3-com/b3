//! BackendBridge — connects a MultiTransport to any local backend via channels.
//!
//! The bridge is the shared core loop used by both daemon and GPU relay:
//! - Backend produces messages → bridge sends them through MultiTransport → browser
//! - Browser sends reliable frames → transport handler unwraps → forwards to backend
//! - Periodic tick for ACK coalescing
//! - Periodic keepalive to ALL transports (prevents watchdog kills on idle fallback)
//!
//! The bridge doesn't know what the backend is. The daemon feeds it SessionStore
//! deltas and agent events. The GPU relay feeds it GPU worker WebSocket messages.
//! Same loop, same reliability, different backends.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

use crate::{MultiTransport, Priority};

/// A message from the backend, ready to send to the browser via MultiTransport.
pub struct OutboundMessage {
    pub payload: Vec<u8>,
    pub priority: Priority,
}

/// Optional telemetry counters for the bridge loop.
/// When provided, the bridge increments these on each recv and send.
pub struct BridgeCounters {
    pub bridge_recvs: Arc<AtomicU64>,
    pub bridge_sends: Arc<AtomicU64>,
}

/// Run the backend→browser bridge loop.
///
/// Reads from `backend_rx` and sends through the shared MultiTransport.
/// Ticks every 200ms for ACK coalescing.
/// Sends keepalive to ALL transports every 5s (prevents watchdog kills on idle fallback).
/// Runs until `backend_rx` is closed (sender dropped).
///
/// Browser→backend direction is NOT part of this loop — it's handled by
/// each transport handler (WS/RTC) which calls `multi.receive()` to unwrap
/// reliable frames, then forwards the plain payload to the backend via
/// whatever mechanism the backend uses (event channel, WS send, etc.)
pub async fn backend_to_browser(
    multi: Arc<Mutex<MultiTransport>>,
    mut backend_rx: tokio::sync::mpsc::Receiver<OutboundMessage>,
    counters: Option<BridgeCounters>,
) {
    tracing::info!("Bridge: started, waiting for messages");
    let mut tick_interval = tokio::time::interval(tokio::time::Duration::from_millis(200));
    let mut keepalive_interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
    let mut msg_count: u64 = 0;

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                multi.lock().await.tick();
            }
            _ = keepalive_interval.tick() => {
                // Send ACK to ALL transports — keeps idle fallback transports alive
                // so their send tasks don't get killed by watchdogs.
                multi.lock().await.keepalive_all();
            }
            msg = backend_rx.recv() => {
                match msg {
                    Some(m) => {
                        msg_count += 1;
                        if let Some(ref c) = counters {
                            c.bridge_recvs.fetch_add(1, Ordering::Relaxed);
                        }
                        if msg_count <= 3 || msg_count % 100 == 0 {
                            tracing::info!("Bridge: msg #{msg_count}, {} bytes", m.payload.len());
                        }
                        multi.lock().await.send(&m.payload, m.priority);
                        if let Some(ref c) = counters {
                            c.bridge_sends.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    None => {
                        tracing::info!("Bridge: backend_rx closed after {msg_count} messages");
                        break;
                    }
                }
            }
        }
    }
}
