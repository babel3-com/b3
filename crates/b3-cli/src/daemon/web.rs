//! Embedded web server for point-to-point dashboard.
//!
//! Runs inside the daemon process. Serves the agent dashboard and WebSocket
//! relay directly from the machine running Claude Code. No EC2 relay needed.
//!
//! PTY output writes directly to the in-memory SessionStore (zero network hop).
//! Browser connects via Cloudflare Tunnel → localhost → this server.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use base64::Engine;
use futures_util::SinkExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, RwLock, Mutex};

use b3_common::AgentEvent;
use crate::pty::manager::PtyResizer;
use portable_pty::PtySize;

// ── Pipeline Telemetry Counters ────────────────────────────────────
// Monotonic counters at every pipeline seam. A 10-second ticker logs
// a snapshot; any counter that stops advancing while its upstream
// keeps going reveals a dead stage in under 10 seconds.

pub struct PipelineCounters {
    // Terminal pipeline (daemon → browser)
    // NOTE: pty_reads and pty_broadcast_sends are NOT instrumented — the pty-reader
    // is a std::thread and threading AtomicU64 into it is messy for little gain.
    // pusher_recvs is the effective proxy for PTY liveness.
    pub pty_reads: AtomicU64,
    pub pty_broadcast_sends: AtomicU64,
    pub pusher_recvs: AtomicU64,
    pub store_updates: AtomicU64,
    pub events_sent: AtomicU64,
    pub event_consumer_recvs: AtomicU64,
    pub delta_msg_some: AtomicU64,
    pub emit_ok: AtomicU64,
    // Arc-wrapped so they can be shared with the bridge task (spawned into a separate tokio task)
    pub bridge_recvs: Arc<AtomicU64>,
    pub bridge_sends: Arc<AtomicU64>,
    // Arc-wrapped: cloned into the WS transport closure (move closure, different task)
    pub transport_writes: Arc<AtomicU64>,
    pub acks_received: AtomicU64,

    // Browser telemetry (received from browser heartbeat)
    pub browser_frames_in: AtomicU64,
}

impl Default for PipelineCounters {
    fn default() -> Self {
        Self {
            pty_reads: AtomicU64::new(0),
            pty_broadcast_sends: AtomicU64::new(0),
            pusher_recvs: AtomicU64::new(0),
            store_updates: AtomicU64::new(0),
            events_sent: AtomicU64::new(0),
            event_consumer_recvs: AtomicU64::new(0),
            delta_msg_some: AtomicU64::new(0),
            emit_ok: AtomicU64::new(0),
            bridge_recvs: Arc::new(AtomicU64::new(0)),
            bridge_sends: Arc::new(AtomicU64::new(0)),
            transport_writes: Arc::new(AtomicU64::new(0)),
            acks_received: AtomicU64::new(0),
            browser_frames_in: AtomicU64::new(0),
        }
    }
}

impl PipelineCounters {
    /// Snapshot all counters for structured logging.
    pub fn snapshot(&self) -> PipelineSnapshot {
        PipelineSnapshot {
            pty_reads: self.pty_reads.load(Ordering::Relaxed),
            pty_broadcast_sends: self.pty_broadcast_sends.load(Ordering::Relaxed),
            pusher_recvs: self.pusher_recvs.load(Ordering::Relaxed),
            store_updates: self.store_updates.load(Ordering::Relaxed),
            events_sent: self.events_sent.load(Ordering::Relaxed),
            event_consumer_recvs: self.event_consumer_recvs.load(Ordering::Relaxed),
            delta_msg_some: self.delta_msg_some.load(Ordering::Relaxed),
            emit_ok: self.emit_ok.load(Ordering::Relaxed),
            bridge_recvs: (*self.bridge_recvs).load(Ordering::Relaxed),
            bridge_sends: (*self.bridge_sends).load(Ordering::Relaxed),
            transport_writes: (*self.transport_writes).load(Ordering::Relaxed),
            acks_received: self.acks_received.load(Ordering::Relaxed),
            browser_frames_in: self.browser_frames_in.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub struct PipelineSnapshot {
    pub pty_reads: u64,
    pub pty_broadcast_sends: u64,
    pub pusher_recvs: u64,
    pub store_updates: u64,
    pub events_sent: u64,
    pub event_consumer_recvs: u64,
    pub delta_msg_some: u64,
    pub emit_ok: u64,
    pub bridge_recvs: u64,
    pub bridge_sends: u64,
    pub transport_writes: u64,
    pub acks_received: u64,
    pub browser_frames_in: u64,
}

impl std::fmt::Display for PipelineSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pty_rd={} bc_tx={} push_rx={} store={} ev_tx={} ev_rx={} delta={} emit={} br_rx={} br_tx={} tp_wr={} ack_rx={} b_in={}",
            self.pty_reads, self.pty_broadcast_sends, self.pusher_recvs,
            self.store_updates, self.events_sent, self.event_consumer_recvs,
            self.delta_msg_some, self.emit_ok, self.bridge_recvs,
            self.bridge_sends, self.transport_writes, self.acks_received,
            self.browser_frames_in)
    }
}

// ── Voice Job Reporting ─────────────────────────────────────────────

/// Fire-and-forget checkpoint report to the EC2 voice-jobs ledger.
/// Spawns a background task — never blocks the audio path.
fn report_voice_job(api_url: &str, api_key: &str, job_id: &str, status: &str, extra: Option<serde_json::Value>) {
    if job_id.is_empty() || api_url.is_empty() { return; }
    let url = format!("{}/api/voice-jobs/{}", api_url, job_id);
    let api_key = api_key.to_string();
    let version = format!("daemon:{}", env!("CARGO_PKG_VERSION"));
    let mut patch = serde_json::json!({
        "status": status,
        "reporter_version": version,
    });
    if let Some(extra) = extra {
        if let (Some(p), Some(e)) = (patch.as_object_mut(), extra.as_object()) {
            for (k, v) in e { p.insert(k.clone(), v.clone()); }
        }
    }
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .patch(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&patch)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
    });
}

/// Fire-and-forget: register a new voice job with the EC2 ledger.
fn register_voice_job(api_url: &str, api_key: &str, job_id: &str, agent_id: &str, job_type: &str, extra: Option<serde_json::Value>) {
    if job_id.is_empty() || api_url.is_empty() { return; }
    let url = format!("{}/api/voice-jobs", api_url);
    let api_key = api_key.to_string();
    let version = format!("daemon:{}", env!("CARGO_PKG_VERSION"));
    let mut body = serde_json::json!({
        "id": job_id,
        "agent_id": agent_id,
        "job_type": job_type,
        "reporter_version": version,
    });
    if let Some(extra) = extra {
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in e { b.insert(k.clone(), v.clone()); }
        }
    }
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
    });
}

// ── TTS Text Processing ─────────────────────────────────────────────

/// Progressive chunk sizes (chars). First chunk is small for low latency,
/// subsequent chunks grow to reduce request overhead.
const CHUNK_SIZES: &[usize] = &[100, 200, 300, 400, 500];
const MIN_CHUNK_CHARS: usize = 50;

// Pre-compiled regexes for web TTS text cleaning. Compiled once on first use.
mod web_tts_patterns {
    use std::sync::LazyLock;
    use regex::Regex;

    pub static CODE_BLOCKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?```").unwrap());
    pub static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]*)`").unwrap());
    pub static HEADERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
    pub static BOLD_ITALIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*\*(.+?)\*\*\*").unwrap());
    pub static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    pub static ITALIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(.+?)\*").unwrap());
    pub static UNDERLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
    pub static LINKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").unwrap());
    pub static IMAGES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]+\)").unwrap());
    pub static URLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+").unwrap());
    pub static BULLET_POINTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[-*+]\s+").unwrap());
    pub static NUMBERED_LISTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*\d+\.\s+").unwrap());
    pub static HORIZ_RULES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[-*_]{3,}\s*$").unwrap());
    pub static BLOCKQUOTES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^>\s?").unwrap());
    pub static STRIKETHROUGH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~(.+?)~~").unwrap());
    pub static ALL_CAPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[A-Z]{2,}\b").unwrap());
    pub static PARA_BREAKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{2,}").unwrap());
    pub static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n").unwrap());
    pub static MULTI_PERIODS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.(\s*\.)+").unwrap());
    pub static MULTI_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"  +").unwrap());
}

/// Clean text for Chatterbox TTS. Strips markdown, emoji, URLs, code blocks.
/// Converts unicode to ASCII. Chatterbox reads markdown syntax literally
/// and spells out ALL CAPS as acronyms.
///
/// `force_lowercase`: when true, ALL CAPS words (2+ uppercase letters) are
/// lowercased to prevent Chatterbox from spelling them out as acronyms.
fn clean_for_tts(text: &str, force_lowercase: bool) -> String {
    use web_tts_patterns::*;

    let mut s = text.to_string();

    // 1. Remove code blocks (``` ... ```)
    s = CODE_BLOCKS.replace_all(&s, "").to_string();
    // 2. Remove inline code
    s = INLINE_CODE.replace_all(&s, "$1").to_string();
    // 3. Remove markdown headers
    s = HEADERS.replace_all(&s, "").to_string();
    // 4. Bold/italic
    s = BOLD_ITALIC.replace_all(&s, "$1").to_string();
    s = BOLD.replace_all(&s, "$1").to_string();
    s = ITALIC.replace_all(&s, "$1").to_string();
    s = UNDERLINE.replace_all(&s, "$1").to_string();
    // 5. Links [text](url) → text
    s = LINKS.replace_all(&s, "$1").to_string();
    // 6. Images ![alt](url) → alt
    s = IMAGES.replace_all(&s, "$1").to_string();
    // 7. URLs
    s = URLS.replace_all(&s, "").to_string();
    // 8. List markers
    s = BULLET_POINTS.replace_all(&s, "").to_string();
    s = NUMBERED_LISTS.replace_all(&s, "").to_string();
    // 9. Horizontal rules
    s = HORIZ_RULES.replace_all(&s, "").to_string();
    // 10. Blockquote markers
    s = BLOCKQUOTES.replace_all(&s, "").to_string();
    // 11. Strikethrough
    s = STRIKETHROUGH.replace_all(&s, "$1").to_string();
    // 12. Emoji (strip common unicode emoji ranges)
    s = s.chars().filter(|c| {
        let cp = *c as u32;
        cp < 0x2600
            || (cp > 0x27BF && cp < 0x1F300)
            || (cp > 0x1FAFF && cp < 0x100000)
    }).collect();
    // 13. Lowercase ALL CAPS words (2+ uppercase letters) — conditional on setting
    if force_lowercase {
        s = ALL_CAPS.replace_all(&s, |caps: &regex::Captures| {
            caps[0].to_lowercase()
        }).to_string();
    }
    // 14. Unicode → ASCII replacements
    let replacements = [
        ('\u{2014}', " -- "), ('\u{2013}', " - "), ('\u{2018}', "'"),
        ('\u{2019}', "'"), ('\u{201C}', "\""), ('\u{201D}', "\""),
        ('\u{2026}', "..."), ('\u{2022}', ","), ('\u{00A0}', " "),
        ('\u{200B}', ""), ('\u{200C}', ""), ('\u{200D}', ""),
        ('\u{FEFF}', ""), ('\u{00B0}', " degrees "),
    ];
    for (from, to) in replacements {
        s = s.replace(from, to);
    }
    // 15. Paragraph breaks → sentence pause, single newlines → space
    s = PARA_BREAKS.replace_all(&s, ". ").to_string();
    s = NEWLINES.replace_all(&s, " ").to_string();
    // 16. Collapse multiple periods/spaces
    s = MULTI_PERIODS.replace_all(&s, ".").to_string();
    s = MULTI_SPACES.replace_all(&s, " ").to_string();
    // 17. Final ASCII-only pass
    s = s.chars().filter(|c| c.is_ascii()).collect();

    s.trim().to_string()
}

/// Split text into speakable chunks at sentence boundaries only.
///
/// Chunk size is a soft target — we ALWAYS break on sentence endings (.!?),
/// never mid-sentence. If no sentence break is found within target size,
/// we extend until we find one. Latency is acceptable; cut sentences are not.
fn chunk_text(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return vec![];
    }
    if text.len() <= CHUNK_SIZES[0] {
        return vec![text.to_string()];
    }

    let sentence_end = regex::Regex::new(r"[.!?][\s\n]").unwrap();
    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut chunk_index = 0;

    while !remaining.is_empty() {
        let current_max = CHUNK_SIZES[chunk_index.min(CHUNK_SIZES.len() - 1)];

        if remaining.len() <= current_max {
            chunks.push(remaining.to_string());
            break;
        }

        let search_text = &remaining[..current_max.min(remaining.len())];
        let mut break_point: Option<usize> = None;

        // Priority 1: Paragraph break within target
        if let Some(pos) = search_text.rfind("\n\n") {
            if pos > MIN_CHUNK_CHARS {
                break_point = Some(pos + 2);
            }
        }

        // Priority 2: Last sentence ending within target
        if break_point.is_none() {
            let mut last_match_end = None;
            for m in sentence_end.find_iter(search_text) {
                if m.end() > MIN_CHUNK_CHARS {
                    last_match_end = Some(m.end());
                }
            }
            break_point = last_match_end;
        }

        // Priority 3: Extend past target to find next sentence ending
        if break_point.is_none() {
            if let Some(m) = sentence_end.find(remaining) {
                break_point = Some(m.end());
            } else {
                // No sentence ending at all — take everything
                chunks.push(remaining.to_string());
                break;
            }
        }

        let bp = break_point.unwrap();
        let chunk = remaining[..bp].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        remaining = remaining[bp..].trim_start();
        chunk_index += 1;
    }

    chunks
}

// ── Session Store (single-agent, in-memory) ─────────────────────────

/// Terminal state buffer.
/// `terminal_data` is raw PTY bytes shared via Arc — cloning costs 8 bytes
/// (refcount bump) instead of 10.7MB memcpy when it was a base64 String.
#[derive(Debug, Clone)]
pub struct SessionBuffer {
    pub terminal_data: Arc<Vec<u8>>,  // raw PTY bytes, shared via Arc
    pub rows: u16,
    pub cols: u16,
    /// Monotonic count of total bytes ever written to this buffer.
    /// When the ring buffer drains the front, this keeps growing.
    /// Used by WS delta logic to compute correct deltas through wraps.
    pub write_offset: u64,
}

/// In-memory session store. Single agent, no DB.
#[derive(Clone)]
pub struct SessionStore {
    buffer: Arc<RwLock<Option<SessionBuffer>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn update(&self, raw_bytes: Arc<Vec<u8>>, rows: u16, cols: u16, write_offset: u64) {
        let mut buf = self.buffer.write().await;
        *buf = Some(SessionBuffer {
            terminal_data: raw_bytes,
            rows,
            cols,
            write_offset,
        });
    }

    pub async fn get(&self) -> Option<SessionBuffer> {
        self.buffer.read().await.clone()
    }

    /// Compute a terminal delta or full snapshot JSON message.
    /// Returns (json_message, new_offset) or None if no update needed.
    /// Both WS and RTC handlers call this — single source of delta logic.
    ///
    /// Deltas are capped at MAX_DELTA_CAP bytes to prevent large buffers
    /// from flooding the reliable channel. When the true delta exceeds
    /// the cap, only the most recent bytes are sent and the offset jumps
    /// forward — the browser loses some scrollback but stays live.
    pub async fn delta_msg(&self, prev_offset: u64) -> Option<(serde_json::Value, u64)> {
        /// Maximum raw bytes per delta. Keeps reliable-frame count manageable
        /// (256 KB / 32 KB frame = 8 frames) even on large terminal buffers.
        const MAX_DELTA_CAP: u64 = 256 * 1024;

        let buf = self.get().await?;
        let raw = &buf.terminal_data; // Arc<Vec<u8>> — no decode needed
        let cur_offset = buf.write_offset;
        let buf_len = raw.len() as u64;

        if cur_offset <= prev_offset {
            return None;
        }

        let new_bytes = cur_offset - prev_offset;
        let msg = if new_bytes <= buf_len {
            // Delta — encode only the new tail slice, capped to prevent flooding.
            let capped = new_bytes.min(MAX_DELTA_CAP);
            if capped < new_bytes {
                tracing::warn!(
                    "Delta capped: {}KB → {}KB (skipped {}KB)",
                    new_bytes / 1024, capped / 1024, (new_bytes - capped) / 1024
                );
            }
            let start = (buf_len - capped) as usize;
            let delta_b64 = base64::engine::general_purpose::STANDARD.encode(&raw[start..]);
            serde_json::json!({ "type": "delta", "data": delta_b64 })
        } else {
            // Too far behind — full snapshot, also capped.
            let cap = (MAX_DELTA_CAP as usize).min(raw.len());
            let start = raw.len() - cap;
            let full_b64 = base64::engine::general_purpose::STANDARD.encode(&raw[start..]);
            serde_json::json!({
                "type": "full",
                "terminal_data": full_b64,
                "rows": buf.rows,
                "cols": buf.cols,
            })
        };
        Some((msg, cur_offset))
    }
}

// ── Event Channel ───────────────────────────────────────────────────

/// Broadcast channel for terminal events.
#[derive(Clone)]
pub struct EventChannel {
    tx: broadcast::Sender<AgentEvent>,
}

impl EventChannel {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    pub fn send(&self, event: AgentEvent) {
        let count = self.tx.receiver_count();
        if count == 0 {
            tracing::warn!("EventChannel.send: ZERO receivers! Event dropped.");
        }
        let _ = self.tx.send(event);
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

// ── Shared State ────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WebState {
    pub sessions: SessionStore,
    pub events: EventChannel,
    pub agent_id: String,
    pub agent_name: String,
    pub agent_email: String,
    pub api_url: String,
    /// Domain extracted from api_url (e.g., "babel3.com").
    pub server_domain: String,
    /// Subsystem health registry — populated by pusher/puller/tunnel.
    pub health: Option<b3_common::health::HealthRegistry>,
    /// API key for authenticating to EC2 (credit checks, etc.)
    pub api_key: String,
    /// Per-session terminal sizes. Key = session_id, Value = (rows, cols).
    /// Used to compute minimum dimensions across all connected browsers.
    pub session_sizes: Arc<RwLock<std::collections::HashMap<u64, (u16, u16)>>>,
    /// PTY resize handle — shared with WebSocket handler for client-driven resize.
    pub pty_resizer: Option<PtyResizer>,
    /// Current PTY dimensions — updated on resize, read by local pusher for session buffer.
    pub pty_size: Arc<RwLock<(u16, u16)>>,
    /// Ring buffer of browser console log entries (newest last). Max 500 entries.
    pub browser_console: Arc<RwLock<Vec<String>>>,
    /// Pending browser eval requests: eval_id → oneshot sender for result.
    pub eval_pending: Arc<RwLock<std::collections::HashMap<String, tokio::sync::oneshot::Sender<String>>>>,
    /// Connected browser versions: session_id → server_version they loaded from.
    /// Only browsers send this (terminals don't), so this map distinguishes browsers from terminals.
    pub browser_versions: Arc<RwLock<std::collections::HashMap<u64, String>>>,
    /// Browser User-Agent strings: session_id → UA string (e.g. "Chrome/131", "Safari/17").
    pub browser_user_agents: Arc<RwLock<std::collections::HashMap<u64, String>>>,
    /// Browser volume levels: session_id → volume percentage (0-100).
    pub browser_volumes: Arc<RwLock<std::collections::HashMap<u64, u8>>>,
    /// Browser audio context state: session_id → true if AudioContext is unlocked/running.
    /// Only browsers with an active audio context can generate and play TTS.
    pub browser_audio_contexts: Arc<RwLock<std::collections::HashMap<u64, bool>>>,
    /// Last activity timestamp per session — updated on any WS message received.
    /// Used to prune stale connections when reporting session count.
    pub session_last_seen: Arc<RwLock<std::collections::HashMap<u64, std::time::Instant>>>,
    /// Multi-client eval responses: eval_id → (expected_count, collected responses).
    /// Unlike eval_pending (first responder wins), this collects from ALL clients.
    pub eval_multi: Arc<RwLock<std::collections::HashMap<String, tokio::sync::mpsc::Sender<(u64, String)>>>>,
    /// TTS msg_ids that have been played successfully by ANY browser.
    /// Checked before injecting [TTS LOST] — suppresses false loss from other browsers.
    pub played_tts: Arc<RwLock<std::collections::HashSet<String>>>,
    /// Last voice event timestamp per msg_id — used by archive watchdog to detect
    /// browser progress. Updated by voice_event() on each chunk delivery/playback event.
    pub tts_progress: Arc<RwLock<std::collections::HashMap<String, std::time::Instant>>>,
    /// GPU client for daemon-side TTS generation (when no browsers connected).
    pub gpu_client: Arc<super::gpu_client::GpuClient>,
    /// On-disk TTS message archive — stores WAV + metadata for browser replay.
    pub tts_archive: Arc<super::tts_archive::TtsArchive>,
    /// On-disk info panel archive — stores HTML content pushed by voice_share_info.
    pub info_archive: Arc<super::info_archive::InfoArchive>,
    /// Daemon password hash — if set, browsers must authenticate before accessing session content.
    pub daemon_password_hash: Option<String>,
    /// Development mode: serve browser dashboard from this local directory.
    /// When set, the daemon serves a full dashboard at / using files from this path.
    /// Expected structure: <dir>/static/open/css/, <dir>/static/open/js/, etc.
    pub browser_dir: Option<PathBuf>,
    /// Whether to force ALL CAPS words to lowercase in TTS text cleaning.
    /// Default true for backward compat. Fetched from EC2 agent settings on startup.
    pub force_lowercase: Arc<RwLock<bool>>,
    /// Per-browser session manager. Each browser gets its own Session with
    /// isolated MultiTransport (own seq counter, send buffer), delta offset,
    /// and RTC state. Shared by web.rs and rtc.rs.
    pub session_manager: Arc<b3_reliable::SessionManager>,
    /// Pipeline telemetry counters — monotonic, one per pipeline seam.
    /// A 10s ticker logs snapshots; divergence between adjacent counters
    /// reveals dead pipeline stages in under 10 seconds.
    pub counters: Arc<PipelineCounters>,
    /// Internal proxy bypass token — random, generated at daemon startup.
    /// Used by `ec2_proxy` to connect to `/ws-proxy` without daemon password auth.
    /// E2E encryption on the EC2 path authenticates the browser; this token
    /// authenticates the LOCAL connection from ec2_proxy → daemon web server.
    pub proxy_token: String,
    /// Channel events broadcast — the voice MCP process subscribes via SSE to
    /// receive transcriptions (and future channel-bound events) without PTY injection.
    pub channel_events: broadcast::Sender<ChannelEvent>,
}

/// Events delivered to the MCP process via the channel-events SSE endpoint.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type")]
pub enum ChannelEvent {
    #[serde(rename = "transcription")]
    Transcription { text: String, message_id: String },
    #[serde(rename = "hive_dm")]
    HiveDm { sender: String, text: String, message_id: String },
    #[serde(rename = "hive_room")]
    HiveRoom { sender: String, room_id: String, room_topic: String, text: String, message_id: String },
}

// ── PTY Output Ingestion ────────────────────────────────────────────

use b3_common::limits::MAX_BUFFER_BYTES;

/// Spawn a task that reads PTY output from the broadcast channel and
/// writes directly to the SessionStore. Replaces the HTTP pusher entirely.
pub fn spawn_local_pusher(
    state: WebState,
    mut pty_rx: broadcast::Receiver<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("Local pusher started — listening for PTY output");
        let mut accumulated = std::collections::VecDeque::with_capacity(MAX_BUFFER_BYTES + 4096);
        // Monotonic counter: total bytes ever appended to the buffer.
        // Never decreases, even when the ring buffer drains the front.
        let mut write_offset: u64 = 0;

        tracing::info!("Local pusher: entering recv loop");
        let mut recv_count: u64 = 0;
        loop {
            // Wait for PTY output — block until data arrives, then debounce
            match pty_rx.recv().await {
                Ok(bytes) => {
                    recv_count += 1;
                    state.counters.pusher_recvs.fetch_add(1, Ordering::Relaxed);
                    if recv_count <= 3 || recv_count % 100 == 0 {
                        tracing::info!("Local pusher: recv #{recv_count}, {} bytes", bytes.len());
                    }
                    write_offset += bytes.len() as u64;
                    accumulated.extend(bytes.iter().copied());
                    // Debounce: coalesce PTY writes to reduce SessionUpdated event rate.
                    // 50ms balances responsiveness with CPU cost — each event triggers
                    // delta extraction for every connected browser.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    while let Ok(more) = pty_rx.try_recv() {
                        write_offset += more.len() as u64;
                        accumulated.extend(more.iter().copied());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("PTY closed, local pusher exiting");
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Local pusher lagged, missed {n} messages");
                    // CRITICAL: yield to tokio before continuing. Without this, if the
                    // receiver is far behind (e.g. 512+ messages during 3s startup wait),
                    // every recv() returns Lagged synchronously — no .await point, no yield,
                    // the task busy-loops and starves the entire tokio runtime.
                    tokio::task::yield_now().await;
                    continue;
                }
            }

            if accumulated.is_empty() {
                continue;
            }

            // Cap buffer — prevent unbounded growth from fast PTY output.
            // VecDeque::drain(..n) is O(n) on drained elements only (not remaining).
            // For trimming 55 bytes from 8MB: O(55) instead of Vec's O(8MB) memmove.
            if accumulated.len() > MAX_BUFFER_BYTES {
                let excess = accumulated.len() - MAX_BUFFER_BYTES;
                accumulated.drain(..excess);
            }

            // Write directly to SessionStore (nanoseconds, no network).
            // Arc<Vec<u8>> — one allocation per event, shared by all consumers.
            // No base64 encode here; encode at the send boundary only.
            let raw_bytes = Arc::new(accumulated.make_contiguous().to_vec());
            let (rows, cols) = *state.pty_size.read().await;
            state.sessions.update(raw_bytes, rows, cols, write_offset).await;
            state.counters.store_updates.fetch_add(1, Ordering::Relaxed);

            // Broadcast SessionUpdated so WebSocket handlers know to send deltas
            state.events.send(AgentEvent::SessionUpdated {
                size: accumulated.len(),
            });
            state.counters.events_sent.fetch_add(1, Ordering::Relaxed);
        }
    })
}

// ── HTTP Handlers ───────────────────────────────────────────────────

async fn health(State(state): State<WebState>) -> (StatusCode, axum::response::Json<serde_json::Value>) {
    let (status_code, body) = match &state.health {
        Some(registry) => {
            let report = registry.report();
            let all_healthy = report.iter().all(|s| s.healthy);
            let subs: Vec<serde_json::Value> = report.iter().map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "healthy": s.healthy,
                    "consecutive_failures": s.consecutive_failures,
                    "last_error": s.detail,
                })
            }).collect();
            // Always return 200 — Cloudflare tunnel uses /health to decide if
            // the origin is alive. A 503 here causes CF to drop WebSocket
            // connections, even though the web server is fully functional.
            // Degraded subsystems (e.g., puller SSE failures) are reported in
            // the body for monitoring but don't affect the HTTP status code.
            let code = StatusCode::OK;
            (code, serde_json::json!({
                "status": if all_healthy { "ok" } else { "degraded" },
                "subsystems": subs,
            }))
        }
        None => (StatusCode::OK, serde_json::json!({ "status": "ok" })),
    };
    (status_code, axum::response::Json(body))
}

/// GET /api/browser-console — return recent browser console log entries.
/// Optional query params: ?last=N (default 100), ?filter=substring, ?secondary_filter=substring
async fn browser_console(
    State(state): State<WebState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let last = params.get("last").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100);
    let filter = params.get("filter").map(|s| s.to_lowercase());
    let filter2 = params.get("secondary_filter").map(|s| s.to_lowercase());

    let buf = state.browser_console.read().await;
    let entries: Vec<&String> = buf.iter().filter(|e| {
        let lower = e.to_lowercase();
        filter.as_ref().map_or(true, |f| lower.contains(f))
            && filter2.as_ref().map_or(true, |f| lower.contains(f))
    }).collect();
    let start = if entries.len() > last { entries.len() - last } else { 0 };
    let result: Vec<&String> = entries[start..].to_vec();

    Json(serde_json::json!({
        "count": result.len(),
        "total": buf.len(),
        "logs": result,
    }))
}

/// POST /api/browser-eval — execute JavaScript in the connected browser.
/// Body: { "code": "ENERGY_GATE_THRESHOLD" }
/// Returns the stringified result from eval().
async fn browser_eval(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let code = match body.get("code").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return Json(serde_json::json!({"error": "code parameter required"})),
    };

    let target_session = body.get("session_id").and_then(|v| v.as_u64());

    let eval_id = format!("eval-{:x}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() % 0xFFFFFFFF);
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();

    // Register pending result
    state.eval_pending.write().await.insert(eval_id.clone(), tx);

    // Send eval request to browser(s) — targeted or broadcast
    state.events.send(AgentEvent::BrowserEval {
        eval_id: eval_id.clone(),
        code,
        session_id: target_session,
    });

    // Wait for result with 5s timeout
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(result)) => Json(serde_json::json!({
            "eval_id": eval_id,
            "result": result,
        })),
        Ok(Err(_)) => {
            state.eval_pending.write().await.remove(&eval_id);
            Json(serde_json::json!({"error": "eval channel closed (no browser connected?)"}))
        }
        Err(_) => {
            state.eval_pending.write().await.remove(&eval_id);
            Json(serde_json::json!({"error": "timeout: no browser responded within 5s"}))
        }
    }
}

// ── File Browser ────────────────────────────────────────────────────

/// GET /api/files?path= — List directory contents.
/// Returns files and directories with name, type, size, and modified time.
/// Accepts absolute paths or paths relative to home directory.
async fn list_files(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let raw_path = params.get("path").map(|s| s.as_str()).unwrap_or(".");

    let full_path = if raw_path == "." || raw_path == "~" || raw_path.is_empty() {
        home.clone()
    } else if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    // Canonicalize to resolve symlinks — but allow any readable path
    let resolved = match full_path.canonicalize() {
        Ok(r) => r,
        Err(e) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": format!("path not found: {}", e)}))),
    };

    let mut entries = Vec::new();
    let mut read_dir = match tokio::fs::read_dir(&resolved).await {
        Ok(rd) => rd,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("cannot read directory: {}", e)}))),
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && params.get("hidden").map(|v| v.as_str()) != Some("true") {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(serde_json::json!({
            "name": name,
            "type": if meta.is_dir() { "dir" } else { "file" },
            "size": meta.len(),
            "modified": modified,
        }));
    }

    entries.sort_by(|a, b| {
        let a_dir = a["type"].as_str() == Some("dir");
        let b_dir = b["type"].as_str() == Some("dir");
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    // Display path: use ~ prefix if under home, otherwise absolute
    let display_path = if resolved == home {
        "~".to_string()
    } else if let Ok(rel) = resolved.strip_prefix(&home) {
        format!("~/{}", rel.display())
    } else {
        resolved.to_string_lossy().to_string()
    };

    (StatusCode::OK, Json(serde_json::json!({
        "path": display_path,
        "absolute": resolved.to_string_lossy(),
        "entries": entries,
        "count": entries.len(),
    })))
}

/// GET /api/file?path= — Read file content.
/// Returns file content as text with syntax hint, or base64 for images.
/// Max file size: 10MB for images, 1MB for text.
async fn read_file(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));

    let raw_path = match params.get("path") {
        Some(p) => p.as_str(),
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "path parameter required"}))),
    };

    let full_path = if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    let resolved = match full_path.canonicalize() {
        Ok(r) => r,
        Err(e) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": format!("file not found: {}", e)}))),
    };

    let meta = match tokio::fs::metadata(&resolved).await {
        Ok(m) => m,
        Err(e) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": format!("{}", e)}))),
    };

    if meta.is_dir() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "path is a directory, use /api/files instead"})));
    }

    let ext = resolved.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp");

    let max_size: u64 = if is_image { 10_485_760 } else { 1_048_576 }; // 10MB images, 1MB text
    if meta.len() > max_size {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "file too large",
            "size": meta.len(),
            "max": max_size,
        })));
    }

    let bytes = match tokio::fs::read(&resolved).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("{}", e)}))),
    };

    let modified = meta.modified().ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Images: return base64 encoded
    if is_image {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "ico" => "image/x-icon",
            "bmp" => "image/bmp",
            _ => "application/octet-stream",
        };
        return (StatusCode::OK, Json(serde_json::json!({
            "path": raw_path,
            "type": "image",
            "mime": mime,
            "data_url": format!("data:{};base64,{}", mime, b64),
            "size": meta.len(),
            "modified": modified,
        })));
    }

    // Check if binary (non-image)
    let is_binary = bytes.iter().take(8192).any(|&b| b == 0);
    if is_binary {
        return (StatusCode::OK, Json(serde_json::json!({
            "path": raw_path,
            "type": "binary",
            "size": meta.len(),
            "name": resolved.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "modified": modified,
        })));
    }

    let content = String::from_utf8_lossy(&bytes).to_string();

    let language = match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "sh" | "bash" => "bash",
        "sql" => "sql",
        "xml" => "xml",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "svelte" => "svelte",
        "dockerfile" => "dockerfile",
        _ => "text",
    };

    (StatusCode::OK, Json(serde_json::json!({
        "path": raw_path,
        "type": "text",
        "content": content,
        "language": language,
        "size": meta.len(),
        "lines": content.lines().count(),
        "modified": modified,
    })))
}

/// GET /api/file-raw?path= — Serve raw file bytes with correct Content-Type.
/// Used for PDF rendering, file downloads, etc.
async fn read_file_raw(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));

    let raw_path = match params.get("path") {
        Some(p) => p.as_str(),
        None => return (StatusCode::BAD_REQUEST, [("content-type", "text/plain")], b"path parameter required".to_vec()).into_response(),
    };

    let full_path = if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    let resolved = match full_path.canonicalize() {
        Ok(r) => r,
        Err(_) => return (StatusCode::NOT_FOUND, [("content-type", "text/plain")], b"file not found".to_vec()).into_response(),
    };

    // Max 20MB for raw file serving
    let meta = match tokio::fs::metadata(&resolved).await {
        Ok(m) => m,
        Err(_) => return (StatusCode::NOT_FOUND, [("content-type", "text/plain")], b"file not found".to_vec()).into_response(),
    };
    if meta.len() > 20_971_520 {
        return (StatusCode::BAD_REQUEST, [("content-type", "text/plain")], b"file too large (max 20MB)".to_vec()).into_response();
    }

    let bytes = match tokio::fs::read(&resolved).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "text/plain")], format!("read error: {e}").into_bytes()).into_response(),
    };

    let ext = resolved.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let content_type = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    };

    (StatusCode::OK, [("content-type", content_type)], bytes).into_response()
}

/// PUT /api/file — Save file content.
/// Body: { "path": "...", "content": "..." }
async fn write_file(
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));

    let raw_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "path required"}))),
    };
    let content = match body.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "content required"}))),
    };

    let full_path = if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    // Only allow writing to existing files (no creation via this endpoint)
    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "file does not exist"})));
    }

    match tokio::fs::write(&full_path, content.as_bytes()).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "path": raw_path,
            "size": content.len(),
        }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("write failed: {}", e),
        }))),
    }
}

/// POST /api/file-rename — Rename/move a file or directory.
/// Body: { "path": "...", "new_name": "..." }
async fn file_rename(
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));

    let raw_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "path required"}))),
    };
    let new_name = match body.get("new_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "new_name required"}))),
    };

    // Reject path traversal in new_name
    if new_name.contains('/') || new_name.contains("..") {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "new_name must be a simple filename"})));
    }

    let full_path = if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "file does not exist"})));
    }

    let new_path = full_path.parent().unwrap_or(&home).join(new_name);
    if new_path.exists() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "target already exists"})));
    }

    match tokio::fs::rename(&full_path, &new_path).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "old_path": full_path.to_string_lossy(),
            "new_path": new_path.to_string_lossy(),
        }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("rename failed: {}", e),
        }))),
    }
}

/// POST /api/file-copy — Copy a file.
/// Body: { "path": "...", "new_name": "...", "dest_dir": "..." (optional) }
async fn file_copy(
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));

    let raw_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "path required"}))),
    };
    let new_name = match body.get("new_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "new_name required"}))),
    };

    if new_name.contains('/') || new_name.contains("..") {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "new_name must be a simple filename"})));
    }

    let full_path = if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "file does not exist"})));
    }
    if !full_path.is_file() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "can only copy files, not directories"})));
    }

    // dest_dir allows pasting into a different directory
    let dest_dir = if let Some(d) = body.get("dest_dir").and_then(|v| v.as_str()) {
        if d.starts_with('/') {
            std::path::PathBuf::from(d)
        } else if d.starts_with("~/") {
            home.join(&d[2..])
        } else {
            home.join(d)
        }
    } else {
        full_path.parent().unwrap_or(&home).to_path_buf()
    };

    let new_path = dest_dir.join(new_name);
    if new_path.exists() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "target already exists"})));
    }

    match tokio::fs::copy(&full_path, &new_path).await {
        Ok(bytes) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "source": full_path.to_string_lossy(),
            "dest": new_path.to_string_lossy(),
            "size": bytes,
        }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("copy failed: {}", e),
        }))),
    }
}

/// POST /api/file-delete — Delete a file (not directories for safety).
/// Body: { "path": "..." }
async fn file_delete(
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));

    let raw_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "path required"}))),
    };

    let full_path = if raw_path.starts_with('/') {
        std::path::PathBuf::from(raw_path)
    } else if raw_path.starts_with("~/") {
        home.join(&raw_path[2..])
    } else {
        home.join(raw_path)
    };

    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "file does not exist"})));
    }
    if !full_path.is_file() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "can only delete files, not directories"})));
    }

    match tokio::fs::remove_file(&full_path).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "deleted": full_path.to_string_lossy(),
        }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("delete failed: {}", e),
        }))),
    }
}

/// GET /api/diagnostics — Show voice pipeline configuration and connectivity.
async fn diagnostics(State(state): State<WebState>) -> Json<serde_json::Value> {
    let runpod_api_key = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
    let runpod_gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
    let has_runpod = !runpod_api_key.is_empty() && !runpod_gpu_id.is_empty();

    let chatterbox_url = std::env::var("CHATTERBOX_URL")
        .unwrap_or_else(|_| "http://localhost:5125".to_string());
    let whisper_url = std::env::var("WHISPER_URL")
        .unwrap_or_else(|_| "http://localhost:5124/transcribe".to_string());

    // Collect connected browsers (only clients that sent "version" — real browsers, not terminals)
    let browser_versions = state.browser_versions.read().await;
    let user_agents = state.browser_user_agents.read().await;
    let volumes = state.browser_volumes.read().await;
    let audio_ctxs = state.browser_audio_contexts.read().await;
    let last_seen = state.session_last_seen.read().await;
    let sizes = state.session_sizes.read().await;
    let browsers: Vec<serde_json::Value> = browser_versions.iter().map(|(id, ver)| {
        let ua = user_agents.get(id).cloned().unwrap_or_default();
        let vol = volumes.get(id).copied().unwrap_or(100);
        let ac = audio_ctxs.get(id).copied().unwrap_or(false);
        let ago = last_seen.get(id).map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        let (rows, cols) = sizes.get(id).copied().unwrap_or((0, 0));
        serde_json::json!({
            "session_id": id, "version": ver, "user_agent": ua,
            "volume": vol, "audio_context": ac, "last_seen_ms": ago,
            "rows": rows, "cols": cols
        })
    }).collect();
    drop(browser_versions);
    drop(user_agents);
    drop(volumes);
    drop(audio_ctxs);
    drop(last_seen);
    drop(sizes);
    // Count terminal-only connections (have resize but no version)
    let all_clients = state.session_sizes.read().await.len();
    let terminal_sessions = all_clients.saturating_sub(browsers.len());

    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "agent_name": state.agent_name,
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "connected_browsers": browsers,
        "terminal_sessions": terminal_sessions,
        "voice_pipeline": {
            "backend": if has_runpod { "runpod" } else { "local" },
            "runpod_configured": has_runpod,
            "runpod_gpu_id": if has_runpod { &runpod_gpu_id } else { "" },
            "whisper_url": whisper_url,
            "chatterbox_url": chatterbox_url,
        },
        "env_vars": {
            "RUNPOD_API_KEY": if has_runpod { "set" } else { "missing" },
            "RUNPOD_GPU_ID": if has_runpod { "set" } else { "missing" },
        }
    }))
}

/// GET /api/gpu-config — return GPU credentials from daemon env vars.
/// Browser fetches GPU config from EC2 (source of truth), not here.
/// This endpoint exists for daemon-internal use (e.g. voice_health MCP tool).
async fn gpu_config() -> Json<serde_json::Value> {
    let api_key = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
    let gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
    let configured = !api_key.is_empty() && !gpu_id.is_empty();
    let local_gpu_url = std::env::var("LOCAL_GPU_URL").unwrap_or_default();
    let local_gpu_token = std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default();

    Json(serde_json::json!({
        "configured": configured,
        "gpu_url": if configured { format!("https://api.runpod.ai/v2/{}/runsync", gpu_id) } else { String::new() },
        "gpu_token": if configured { api_key } else { String::new() },
        "local_gpu_url": local_gpu_url,
        "local_gpu_token": local_gpu_token,
    }))
}

/// GET / — Dashboard page.
async fn site_placeholder(State(state): State<WebState>) -> impl IntoResponse {
    let html = SITE_PLACEHOLDER_HTML
        .replace("{{AGENT_NAME}}", &state.agent_name)
        .replace("{{SERVER_DOMAIN}}", &state.server_domain)
        .replace("{{API_URL}}", &state.api_url);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
        ],
        html,
    )
}

/// GET /api/session — read current session buffer.
async fn session_read(State(state): State<WebState>) -> impl IntoResponse {
    match state.sessions.get().await {
        Some(buf) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(buf.terminal_data.as_slice());
            Json(serde_json::json!({
                "agent_id": state.agent_id,
                "terminal_data": b64,
                "rows": buf.rows,
                "cols": buf.cols,
            }))
            .into_response()
        }
        None => Json(serde_json::json!({
            "agent_id": state.agent_id,
            "terminal_data": "",
            "rows": 24,
            "cols": 80,
        }))
        .into_response(),
    }
}

/// GET /ws-proxy — internal WebSocket relay used by the EC2 daemon proxy task.
///
/// Identical to `/ws` but skips daemon password auth — caller authenticates
/// via `proxy_token` (random, generated at daemon startup). The E2E channel
/// on the EC2 path already authenticated the browser; this token authenticates
/// the LOCAL hop from `ec2_proxy` to the daemon web server.
async fn proxy_ws(
    State(state): State<WebState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let provided = params.get("proxy_token").map(|s| s.as_str()).unwrap_or("");
    if provided != state.proxy_token {
        return (axum::http::StatusCode::UNAUTHORIZED, "invalid proxy token").into_response();
    }
    let session_id = params.get("session_id").and_then(|s| s.parse::<u64>().ok());
    // Upgrade and call handle_terminal_ws with no password — auth is already done at
    // the EC2 layer (E2E encryption) and at the proxy_token check above.
    ws.on_upgrade(move |socket| handle_terminal_ws_no_password(socket, state, session_id))
}

/// Variant of `handle_terminal_ws` that skips daemon password auth.
/// Used by the EC2 proxy path — authentication is provided by the E2E channel.
async fn handle_terminal_ws_no_password(socket: WebSocket, state: WebState, session_id: Option<u64>) {
    // Temporarily clear the password hash so handle_terminal_ws skips the auth gate.
    // The state is cloned (WebState is Clone), so this doesn't affect other connections.
    let no_auth_state = WebState { daemon_password_hash: None, ..state };
    handle_terminal_ws(socket, no_auth_state, session_id).await;
}

/// GET /ws — WebSocket terminal relay.
async fn terminal_ws(
    State(state): State<WebState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let session_id = params.get("session_id").and_then(|s| s.parse::<u64>().ok());
    ws.on_upgrade(move |socket| handle_terminal_ws(socket, state, session_id))
}

async fn handle_terminal_ws(socket: WebSocket, state: WebState, session_id: Option<u64>) {
    use futures_util::StreamExt as _;
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── Daemon password authentication ──
    // If a daemon password is set, the browser must authenticate before
    // accessing any session content. The browser sends {type: "auth", password: "..."}
    // after receiving auth_required. Non-auth messages (version, browser_info) sent
    // by the browser's onopen handler arrive first and must be skipped.
    //
    // IMPORTANT: binary frames (reliable Resume/ACK) sent by the browser's onopen
    // handler before auth completes must NOT be dropped. The browser calls sendResume(0)
    // synchronously in ws.onopen — before the auth challenge is even received. If the
    // auth loop discards these frames, the Resume gate times out every connect (5s delay).
    // Buffer them here and drain after session setup. See PR #228 for full analysis.
    let mut pre_auth_binary: Vec<Vec<u8>> = Vec::new();

    if let Some(ref pw_hash) = state.daemon_password_hash {
        // Send auth challenge
        let challenge = serde_json::json!({"type": "auth_required"});
        if ws_tx.send(Message::Text(challenge.to_string().into())).await.is_err() {
            return;
        }

        // Wait for auth — no timeout. Connection stays open until the browser sends
        // a valid password, disconnects, or errors. Wrong passwords get auth_failed
        // and the loop continues so they can retry without reconnecting.
        let mut authenticated = false;

        loop {
            match futures_util::StreamExt::next(&mut ws_rx).await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text.to_string()) {
                        if val.get("type").and_then(|t| t.as_str()) == Some("auth") {
                            let ok = val.get("password")
                                .and_then(|p| p.as_str())
                                .map(|pw| crate::config::Config::verify_daemon_password(pw, pw_hash))
                                .unwrap_or(false);
                            if ok {
                                authenticated = true;
                                break;
                            }
                            // Wrong password — tell them and let them retry
                            let _ = ws_tx.send(Message::Text(
                                serde_json::json!({"type": "auth_failed", "error": "Invalid daemon password"}).to_string().into()
                            )).await;
                            tracing::info!("[WS] Client sent wrong daemon password, waiting for retry");
                        }
                        // Non-auth message — skip (version, browser_info, pong, etc.)
                    }
                }
                Some(Ok(Message::Close(_))) | None => break, // Client disconnected
                Some(Err(_)) => break, // WS error
                Some(Ok(Message::Binary(data))) => {
                    // Buffer reliable frames (Resume/ACK) sent before auth completes.
                    // The browser's onopen fires sendResume(0) synchronously — before
                    // the auth challenge arrives. Dropping it causes a 5s gate timeout
                    // on every connect. Replay these frames after session setup.
                    pre_auth_binary.push(data.to_vec());
                }
                _ => {} // Ping/pong — skip
            }
        }

        if !authenticated {
            tracing::info!("[WS] Client disconnected before authenticating");
            return;
        }

        let _ = ws_tx.send(Message::Text(
            serde_json::json!({"type": "auth_ok"}).to_string().into()
        )).await;
        tracing::info!("[WS] Client authenticated with daemon password");
    }

    // Browser's stable session_id is the sole identifier for everything.
    // Browser must send session_id. No server-assigned IDs.
    let session_id = match session_id {
        Some(sid) => sid,
        None => {
            tracing::warn!("[WS] Rejected connection — no session_id");
            let _ = ws_tx.send(Message::Text(
                serde_json::json!({"type": "error", "message": "session_id query param required"}).to_string().into()
            )).await;
            return;
        }
    };

    // Tell the browser its session_id for targeted eval filtering
    let _ = ws_tx.send(Message::Text(
        serde_json::json!({"type": "welcome", "session_id": session_id}).to_string().into()
    )).await;

    // Send terminal buffer as initial render.
    // Cap at 256 KB to avoid overwhelming WebSocket connections through
    // Cloudflare tunnels. Large init frames (>1 MB) cause code-1006
    // abnormal closures and infinite reconnect loops.
    const MAX_WS_INIT_BYTES: usize = 64 * 1024;

    // ── Per-browser session ──────────────────────────────────────────
    // Reconnect to existing session or create new (shared logic from b3-reliable).
    // On tunnel reconnect, the existing session's send buffer is preserved
    // and Resume replays unacked frames.
    let (session, backend_rx) = state.session_manager
        .reconnect_or_create(Some(session_id), b3_reliable::Config::default()).await;

    // Reap stale sessions — any session other than this one whose WS has disconnected
    // (absent from session_last_seen, which is removed on WS close at the disconnect
    // handler). Prevents orphaned event consumers from accumulating across browser
    // switches (iOS app ↔ Safari, tab close → reopen, etc.).
    //
    // ONLY runs on new WS connect — avoids the tunnel-flap false positive that caused
    // the old periodic reaper to be removed (see comment below the session loop).
    // Tunnel-flap reconnects always reuse the same session_id (sessionStorage-scoped),
    // so they're excluded by the `s.id == session_id` check.
    //
    // Reaps regardless of sink_count — handles sessions with orphaned RTC sinks that
    // keep sink_count > 0 and prevent the grace-period cleanup from firing.
    {
        /// Minimum idle time before reaping a session that's still in last_seen
        /// but has no active sinks. Must be > max expected cellular recovery (~45s).
        const STALE_SESSION_SECS: u64 = 120;

        // Snapshot last_seen map — don't hold the read lock across async destroy calls.
        let last_seen_snapshot: std::collections::HashMap<u64, std::time::Instant> =
            state.session_last_seen.read().await.clone();

        // Collect stale sessions with their log info (single lock per session).
        struct StaleInfo { id: u64, elapsed_str: String, sink_count: usize, sinks: Vec<String> }
        let mut stale: Vec<StaleInfo> = Vec::new();
        for s in state.session_manager.all_sessions().await {
            if s.id == session_id {
                continue; // Skip the session we just created/reconnected to
            }
            let is_stale = match last_seen_snapshot.get(&s.id) {
                None => true, // WS disconnected — last_seen entry was removed
                Some(t) if t.elapsed().as_secs() > STALE_SESSION_SECS => true,
                _ => false,
            };
            if is_stale {
                let elapsed_str = match last_seen_snapshot.get(&s.id) {
                    None => "ws_gone".to_string(),
                    Some(t) => format!("{}s", t.elapsed().as_secs()),
                };
                let (sink_count, sinks) = {
                    let multi = s.multi.lock().await;
                    (multi.sink_count(), multi.sink_names())
                };
                stale.push(StaleInfo { id: s.id, elapsed_str, sink_count, sinks });
            }
        }

        // Destroy stale sessions outside the last_seen snapshot (already dropped above).
        for info in stale {
            tracing::warn!(
                "[SessionReaper] Destroying stale session={} last_seen={} sink_count={} sinks={:?}",
                info.id, info.elapsed_str, info.sink_count, info.sinks
            );
            state.session_manager.remove(info.id).await;
            state.session_last_seen.write().await.remove(&info.id);
        }
    }

    // Always send init on WS connect — the browser needs it to populate the terminal.
    if let Some(buf) = state.sessions.get().await {
        let raw = &buf.terminal_data; // Arc<Vec<u8>> — no decode needed
        let full_len = raw.len();

        let (init_bytes, truncated) = if raw.len() > MAX_WS_INIT_BYTES {
            (&raw[raw.len() - MAX_WS_INIT_BYTES..], true)
        } else {
            (raw.as_slice(), false)
        };
        let init_b64 = base64::engine::general_purpose::STANDARD.encode(init_bytes);

        if truncated {
            tracing::info!(
                raw_bytes = full_len,
                sent_bytes = init_bytes.len(),
                "WS init truncated to last {} KB",
                MAX_WS_INIT_BYTES / 1024,
            );
        }

        let msg = serde_json::json!({
            "type": "init",
            "terminal_data": init_b64,
            "rows": buf.rows,
            "cols": buf.cols,
        });
        if ws_tx
            .send(Message::Text(msg.to_string().into()))
            .await
            .is_err()
        {
            state.session_manager.remove(session_id).await;
            return;
        }

        tracing::info!("[WS] Session {session_id} init sent ({} bytes)", msg.to_string().len());
        // Set THIS session's offset — other sessions are unaffected
        *session.sent_offset.lock().await = buf.write_offset;
    }

    // Channel to funnel messages to WS sender.
    // Capacity 4096: on EC2 reconnect, the bridge replays the full terminal history
    // (potentially 33K+ delta frames) at fire-hose speed. The old 64-frame capacity
    // saturated in <1ms — every subsequent try_send returned Err(Full) and was
    // silently discarded, leaving the browser with zero deltas on the EC2 path.
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<Message>(4096);

    // Register WS sink on THIS session's MultiTransport
    {
        let msg_tx_reliable = msg_tx.clone();
        let transport_writes = Arc::clone(&state.counters.transport_writes);
        session.multi.lock().await.add_transport("ws", 1, move |data: &[u8]| {
            if msg_tx_reliable.try_send(Message::Binary(data.to_vec().into())).is_ok() {
                transport_writes.fetch_add(1, Ordering::Relaxed);
            } else {
                tracing::warn!("[WS] msg_tx full — frame dropped (transport overflow)");
            }
        });
    }
    let ws_reliable = session.multi.clone();
    tracing::info!("[WS] Session {session_id} connected, per-session WS sink registered");

    // Send Resume to browser — tells it our inbound state so it can detect seq
    // mismatch and reset if needed (same as GPU relay, PRs #195-196).
    session.multi.lock().await.send_resume();

    // Always start a fresh bridge. On new session, use the provided backend_rx.
    // On reconnect, abort the old bridge (may be stuck on a dead channel) and
    // create a fresh one. Don't check is_finished() — a bridge that's "alive"
    // but stuck is functionally dead.
    // Wire bridge counters to the shared pipeline counters.
    let bridge_counters = || Some(b3_reliable::BridgeCounters {
        bridge_recvs: Arc::clone(&state.counters.bridge_recvs),
        bridge_sends: Arc::clone(&state.counters.bridge_sends),
    });
    if let Some(rx) = backend_rx {
        tracing::info!("[WS] Session {session_id} NEW session — starting bridge");
        session.start_bridge(rx, bridge_counters()).await;
    } else {
        tracing::info!("[WS] Session {session_id} RECONNECT — restarting bridge unconditionally");
        session.restart_bridge(Some(b3_reliable::BridgeCounters {
            bridge_recvs: Arc::clone(&state.counters.bridge_recvs),
            bridge_sends: Arc::clone(&state.counters.bridge_sends),
        })).await;
    }

    // Gate: event consumer waits for Resume handshake before sending data.
    // Without this, the bridge pumps frames at the old next_seq (e.g. 1543)
    // before handle_resume resets it to 1. Those frames pile up in the
    // browser's reorder buffer, unreachable.
    let (resume_gate_tx, resume_gate_rx) = tokio::sync::oneshot::channel::<()>();
    let mut resume_gate = Some(resume_gate_tx);

    // Drain binary frames buffered during auth (Resume/ACK sent before auth completed).
    // The browser's onopen fires sendResume(0) before receiving the auth challenge —
    // those frames were buffered above instead of dropped. Feed them through now so the
    // Resume handshake completes immediately rather than waiting 5s for the next ACK.
    if !pre_auth_binary.is_empty() {
        tracing::info!(
            "[WS] Session {session_id} draining {} pre-auth binary frame(s) (Resume sent before auth completed)",
            pre_auth_binary.len()
        );
    }
    for data in pre_auth_binary {
        if b3_reliable::is_reliable(&data) {
            ws_reliable.lock().await.receive(&data);
        }
        if let Some(gate) = resume_gate.take() {
            tracing::info!("[WS] Session {session_id} Resume handshake complete (pre-auth buffer)");
            let _ = gate.send(());
        }
    }

    // Start per-session event consumer (reads AgentEvents → OutboundMessages)
    {
        let sessions_store = state.sessions.clone();
        let sent_offset = session.sent_offset.clone();
        let rtc_active = session.rtc_active.clone();
        let backend_tx = session.backend_tx.clone();
        let mut event_rx = state.events.subscribe();
        let counters = state.counters.clone();

        let event_handle = tokio::spawn(async move {
            use b3_reliable::{OutboundMessage, Priority};
            // Wait for Resume handshake to complete before pumping data.
            // On new sessions this fires immediately (browser sends Resume(0)
            // as first frame). On reconnects, handle_resume resets next_seq
            // before this gate opens, so all data goes out at correct seq.
            // Timeout after 5s to handle old browsers that don't send Resume.
            tokio::select! {
                _ = resume_gate_rx => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    tracing::warn!("[Session {session_id}] Resume gate timeout — proceeding without handshake");
                }
            }
            let mut emit_count: u64 = 0;
            macro_rules! emit {
                ($msg:expr, critical) => {
                    if backend_tx.lock().await.send(OutboundMessage {
                        payload: $msg.to_string().into_bytes(),
                        priority: Priority::Critical,
                    }).await.is_err() {
                        tracing::warn!("[Session {session_id}] emit failed (bridge dead?) after {emit_count} frames");
                        break;
                    }
                    emit_count += 1;
                    counters.emit_ok.fetch_add(1, Ordering::Relaxed);
                };
                ($msg:expr, best_effort) => {
                    if backend_tx.lock().await.send(OutboundMessage {
                        payload: $msg.to_string().into_bytes(),
                        priority: Priority::BestEffort,
                    }).await.is_err() {
                        tracing::warn!("[Session {session_id}] emit(BE) failed after {emit_count} frames");
                        break;
                    }
                    emit_count += 1;
                    counters.emit_ok.fetch_add(1, Ordering::Relaxed);
                };
            }

            let mut consecutive_lags: u32 = 0;
            loop {
                match event_rx.recv().await {
                    Ok(AgentEvent::SessionUpdated { size }) => {
                        consecutive_lags = 0;
                        emit_count += 1;
                        counters.event_consumer_recvs.fetch_add(1, Ordering::Relaxed);
                        if emit_count <= 3 || emit_count % 100 == 0 {
                            tracing::info!("[Session {session_id}] Event consumer: SessionUpdated #{emit_count} size={size}");
                        }
                        let mut prev = sent_offset.lock().await;
                        if let Some((msg, new_offset)) = sessions_store.delta_msg(*prev).await {
                            *prev = new_offset;
                            counters.delta_msg_some.fetch_add(1, Ordering::Relaxed);
                            // BestEffort: terminal data is reliably buffered and retransmittable.
                            // Critical priority was saturating msg_tx and delaying TTS audio chunks
                            // (which are Critical and time-sensitive). Session deltas are not.
                            emit!(msg, best_effort);
                        }
                        let _ = size; // suppress unused warning
                    }
                    Ok(AgentEvent::Transcription { text }) => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "transcription", "text": text });
                        emit!(msg, critical);
                    }
                    Ok(AgentEvent::Hive { sender, text }) => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "hive_dm", "sender": sender, "text": text });
                        emit!(msg, best_effort);
                    }
                    Ok(AgentEvent::HiveRoom { sender, room_id, room_topic, text }) => {
                        consecutive_lags = 0;
                        let members = crate::crypto::hive_integration::load_room_members(&room_id);
                        let msg = serde_json::json!({ "type": "hive_room", "sender": sender, "room_id": room_id, "room_topic": room_topic, "text": text, "members": members });
                        emit!(msg, best_effort);
                    }
                    Ok(AgentEvent::Tts { text, msg_id, voice }) if !rtc_active.load(std::sync::atomic::Ordering::Relaxed) => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "tts", "text": text, "msg_id": msg_id, "voice": voice });
                        emit!(msg, critical);
                    }
                    Ok(AgentEvent::TtsGenerating { msg_id, chunk, total }) if !rtc_active.load(std::sync::atomic::Ordering::Relaxed) => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "tts_generating", "msg_id": msg_id, "chunk": chunk, "total": total });
                        emit!(msg, best_effort);
                    }
                    Ok(AgentEvent::TtsStream { msg_id, text, original_text, voice, emotion, replying_to, split_chars, min_chunk_chars, conditionals_b64 }) if !rtc_active.load(std::sync::atomic::Ordering::Relaxed) => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "tts_stream", "msg_id": msg_id, "text": text, "original_text": original_text, "voice": voice, "emotion": emotion, "replying_to": replying_to, "split_chars": split_chars, "min_chunk_chars": min_chunk_chars, "conditionals_b64": conditionals_b64 });
                        emit!(msg, critical);
                    }
                    Ok(AgentEvent::Led { emotion }) => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "led", "emotion": emotion });
                        emit!(msg, best_effort);
                    }
                    Ok(AgentEvent::BrowserEval { eval_id, code, session_id: target }) => {
                        consecutive_lags = 0;
                        let mut msg = serde_json::json!({ "type": "browser_eval", "eval_id": eval_id, "code": code });
                        if let Some(tid) = target {
                            msg["target_session_id"] = serde_json::json!(tid);
                        }
                        emit!(msg, critical);
                    }
                    Ok(AgentEvent::Hive { ref sender, ref text }) if sender == "info" => {
                        consecutive_lags = 0;
                        let msg = serde_json::json!({ "type": "info", "html": text });
                        emit!(msg, best_effort);
                    }
                    Ok(_) => { consecutive_lags = 0; }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::warn!("[Session {session_id}] Event consumer: broadcast closed — exiting");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        consecutive_lags += 1;
                        tracing::warn!("[Session {session_id}] Event consumer: lagged by {n} events (strike {consecutive_lags}/5)");
                        if consecutive_lags >= 5 {
                            tracing::error!("[Session {session_id}] Event consumer killed: persistent lag ({consecutive_lags} consecutive)");
                            break;
                        }
                        let mut prev = sent_offset.lock().await;
                        if let Some((msg, new_offset)) = sessions_store.delta_msg(*prev).await {
                            *prev = new_offset;
                            // BestEffort: same reasoning as SessionUpdated path — terminal
                            // deltas are retransmittable and should not block TTS audio.
                            emit!(msg, best_effort);
                        }
                    }
                }
            }
            tracing::warn!("[Session {session_id}] Event consumer loop ended");
        });
        session.set_event_task(event_handle).await;
    }

    // Task 2: Forward channel messages to WebSocket
    let mut ws_send_task = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Task 2b: WebSocket keepalive — send application-level heartbeat every 5s.
    // WS Ping/Pong (every 30s) handles protocol-level liveness detection.
    // This heartbeat keeps the tunnel active during idle periods.
    let msg_tx_ping = msg_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let hb = serde_json::json!({ "type": "heartbeat" });
            if msg_tx_ping
                .send(Message::Text(hb.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Task 3 (main loop): Read keystrokes + resize from browser.
    // Uses select! to also watch ws_send_task — when the write side dies (browser
    // refreshed/closed without Close frame), we exit immediately instead of waiting
    // for ws_rx to eventually return None. This prevents zombie connections inflating
    // the client count.
    let events = state.events.clone();
    let session_sizes = state.session_sizes.clone();
    let pty_resizer = state.pty_resizer.clone();
    tracing::info!("[WS] Session {session_id} connected. pty_resizer={}", pty_resizer.is_some());
    state.session_last_seen.write().await.insert(session_id, std::time::Instant::now());

    // WS ping/pong keepalive: send a ping every 30s. If the browser doesn't
    // respond (pong), axum-tungstenite closes the connection and ws_rx returns
    // None. This is a PROTOCOL-LEVEL liveness check — no heuristic timer.
    // The old periodic stale reaper was removed because it killed live sessions
    // (all sessions died simultaneously on tunnel flap — the reaper couldn't
    // distinguish "tunnel reconnecting" from "browser gone").
    let mut ping_ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_ticker.tick().await; // consume the immediate first tick
    let ws_connected_at = std::time::Instant::now();
    let mut disconnect_reason = "unknown";

    loop {
        tokio::select! {
            // If the WS send task exits, the write side is dead — break immediately
            _ = &mut ws_send_task => {
                disconnect_reason = "send_task_exited";
                tracing::info!("[WS] Session {session_id} send task exited, closing connection");
                break;
            }
            // WS keepalive ping — browser responds with pong automatically.
            // If browser is gone, the WS layer detects the dead connection
            // and ws_rx returns None (handled in the msg arm below).
            _ = ping_ticker.tick() => {
                if msg_tx.send(Message::Ping(vec![].into())).await.is_err() {
                    disconnect_reason = "ping_send_failed";
                    tracing::info!("[WS] Session {session_id} ping failed (send channel closed)");
                    break;
                }
            }
            msg = futures_util::StreamExt::next(&mut ws_rx) => {
        state.session_last_seen.write().await.insert(session_id, std::time::Instant::now());
        match msg {
            // Binary frames from browser = reliable ACKs/Resume
            Some(Ok(Message::Binary(data))) => {
                if b3_reliable::is_reliable(&data) {
                    ws_reliable.lock().await.receive(&data);
                    state.counters.acks_received.fetch_add(1, Ordering::Relaxed);
                }
                // Open the gate after the first binary frame (Resume from browser).
                // handle_resume has now reset next_seq, so the event consumer
                // will send data at the correct seq number.
                if let Some(gate) = resume_gate.take() {
                    tracing::info!("[WS] Session {session_id} Resume handshake complete — unblocking event consumer");
                    let _ = gate.send(());
                }
                continue;
            }
            Some(Ok(Message::Text(text))) => {
                let data = text.to_string();
                if data.is_empty() { continue; }

                // Check if this is a JSON resize message: {"type":"resize","rows":N,"cols":N}
                if data.starts_with('{') {
                    if !data.contains("\"pong\"") {
                        tracing::info!("[WS] Session {session_id} JSON msg: {data}");
                    }
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                        if val.get("type").and_then(|t| t.as_str()) == Some("resize") {
                            let rows = val.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                            let cols = val.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                            tracing::info!("[WS] Session {session_id} resize request: {cols}x{rows}");
                            if rows > 0 && cols > 0 {
                                // Update this session's size
                                session_sizes.write().await.insert(session_id, (rows, cols));
                                // Compute minimum across all sessions
                                let sizes = session_sizes.read().await;
                                let min_rows = sizes.values().map(|(r, _)| *r).min().unwrap_or(24);
                                let min_cols = sizes.values().map(|(_, c)| *c).min().unwrap_or(80);
                                let num_clients = sizes.len();
                                drop(sizes);
                                tracing::info!("[WS] Resizing PTY to {min_cols}x{min_rows} (min of {num_clients} client(s))");
                                // Resize PTY to minimum
                                let resized = if let Some(ref resizer) = pty_resizer {
                                    match resizer.lock() {
                                        Ok(master) => {
                                            match master.resize(PtySize {
                                                rows: min_rows, cols: min_cols,
                                                pixel_width: 0, pixel_height: 0,
                                            }) {
                                                Ok(()) => {
                                                    tracing::info!("[WS] PTY resize OK: {min_cols}x{min_rows}");
                                                    true
                                                }
                                                Err(e) => { tracing::error!("[WS] PTY resize FAILED: {e}"); false }
                                            }
                                        }
                                        Err(e) => { tracing::error!("[WS] PTY resizer lock FAILED: {e}"); false }
                                    }
                                } else {
                                    tracing::warn!("[WS] No pty_resizer available!");
                                    false
                                };
                                // Update shared pty_size (after dropping MutexGuard)
                                if resized {
                                    *state.pty_size.write().await = (min_rows, min_cols);
                                }
                            }
                            continue;
                        }
                        // Heartbeat pong — client is alive, last_seen already updated above
                        if val.get("type").and_then(|t| t.as_str()) == Some("pong") {
                            continue;
                        }
                        // Echo ping — upstream health check from browser.
                        // Respond with echo_pong on the same WS connection.
                        if val.get("type").and_then(|t| t.as_str()) == Some("echo_ping") {
                            let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let pong = serde_json::json!({ "type": "echo_pong", "id": id });
                            let _ = msg_tx.send(Message::Text(pong.to_string().into())).await;
                            continue;
                        }
                        // Browser version announcement
                        if val.get("type").and_then(|t| t.as_str()) == Some("version") {
                            // js_version = actual cached JS running in browser (HC._jsVersion)
                            // server_version = what the browser fetched from /version endpoint
                            // We store js_version if available (new browsers), fall back to server_version (old)
                            let js_ver = val.get("js_version").and_then(|v| v.as_str());
                            let server_ver = val.get("server_version").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let display_ver = js_ver.unwrap_or(server_ver).to_string();
                            if let Some(jv) = js_ver {
                                tracing::info!("[WS] Session {session_id} reports js_version={jv} server_version={server_ver}");
                            } else {
                                tracing::info!("[WS] Session {session_id} reports server_version={server_ver}");
                            }
                            state.browser_versions.write().await.insert(session_id, display_ver);
                            continue;
                        }
                        // Browser info announcement (user-agent, volume, audio context)
                        if val.get("type").and_then(|t| t.as_str()) == Some("browser_info") {
                            if let Some(ua) = val.get("user_agent").and_then(|v| v.as_str()) {
                                let ua = ua.to_string();
                                tracing::info!("[WS] Session {session_id} user_agent={ua}");
                                state.browser_user_agents.write().await.insert(session_id, ua);
                            }
                            if let Some(vol) = val.get("volume").and_then(|v| v.as_u64()) {
                                let vol = vol.min(100) as u8;
                                tracing::info!("[WS] Session {session_id} volume={vol}");
                                state.browser_volumes.write().await.insert(session_id, vol);
                            }
                            if let Some(ac) = val.get("audio_context").and_then(|v| v.as_bool()) {
                                tracing::info!("[WS] Session {session_id} audio_context={ac}");
                                state.browser_audio_contexts.write().await.insert(session_id, ac);
                            }
                            continue;
                        }
                        // Browser volume change
                        if val.get("type").and_then(|t| t.as_str()) == Some("volume_change") {
                            if let Some(vol) = val.get("volume").and_then(|v| v.as_u64()) {
                                let vol = vol.min(100) as u8;
                                tracing::info!("[WS] Session {session_id} volume_change={vol}");
                                state.browser_volumes.write().await.insert(session_id, vol);
                            }
                            continue;
                        }
                        // Audio context state change (unlocked after user gesture)
                        if val.get("type").and_then(|t| t.as_str()) == Some("audio_context_change") {
                            if let Some(ac) = val.get("audio_context").and_then(|v| v.as_bool()) {
                                tracing::info!("[WS] Session {session_id} audio_context_change={ac}");
                                state.browser_audio_contexts.write().await.insert(session_id, ac);
                            }
                            continue;
                        }
                        // Browser eval result
                        if val.get("type").and_then(|t| t.as_str()) == Some("eval_result") {
                            if let Some(eval_id) = val.get("eval_id").and_then(|v| v.as_str()) {
                                let result = val.get("result").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                // Single-client eval (first responder wins)
                                let mut pending = state.eval_pending.write().await;
                                if let Some(tx) = pending.remove(eval_id) {
                                    let _ = tx.send(result.clone());
                                }
                                drop(pending);
                                // Multi-client eval (all responders collected)
                                let multi = state.eval_multi.read().await;
                                if let Some(tx) = multi.get(eval_id) {
                                    let _ = tx.send((session_id, result)).await;
                                }
                            }
                            continue;
                        }
                        // Browser disconnect event (structured reason from reliable.js)
                        if val.get("type").and_then(|t| t.as_str()) == Some("disconnect") {
                            let transport = val.get("transport").and_then(|v| v.as_str()).unwrap_or("?");
                            let reason = val.get("reason").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let duration_s = val.get("duration_s").and_then(|v| v.as_u64()).unwrap_or(0);
                            let frames_in = val.get("frames_in").and_then(|v| v.as_u64()).unwrap_or(0);
                            let frames_out = val.get("frames_out").and_then(|v| v.as_u64()).unwrap_or(0);
                            tracing::info!(
                                "[BrowserDisconnect] session={session_id} transport={transport} reason={reason} duration_s={duration_s} frames_in={frames_in} frames_out={frames_out}"
                            );
                            continue;
                        }
                        // Browser telemetry heartbeat (structured metrics from reliable.js)
                        if val.get("type").and_then(|t| t.as_str()) == Some("telemetry") {
                            // Update browser_frames_in from the daemon channel telemetry
                            if let Some(daemon) = val.get("daemon") {
                                if let Some(fi) = daemon.get("frames_in").and_then(|v| v.as_u64()) {
                                    state.counters.browser_frames_in.store(fi, Ordering::Relaxed);
                                }
                            }
                            tracing::info!("[BrowserTelemetry] session={session_id} {}", val);
                            continue;
                        }
                        // Browser console log forwarding
                        if val.get("type").and_then(|t| t.as_str()) == Some("browser_log") {
                            if let Some(logs) = val.get("logs").and_then(|l| l.as_array()) {
                                let mut buf = state.browser_console.write().await;
                                for entry in logs {
                                    if let Some(s) = entry.as_str() {
                                        buf.push(s.to_string());
                                    }
                                }
                                // Ring buffer: keep last 500 entries
                                if buf.len() > 500 {
                                    let drain = buf.len() - 500;
                                    buf.drain(..drain);
                                }
                            }
                            continue;
                        }
                        // STT lost notification
                        if val.get("type").and_then(|t| t.as_str()) == Some("stt_lost") {
                            let seq = val.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                            // Duration/energy may arrive as string or number from browser JS
                            let dur = val.get("duration").and_then(|v| {
                                v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                            }).unwrap_or(0.0);
                            let energy = val.get("energy").and_then(|v| {
                                v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                            }).unwrap_or(0.0);
                            let msg = format!(
                                "[TRANSCRIPTION LOST] seq={} duration={:.1}s energy={:.0} — audio saved to daemon for recovery",
                                seq, dur, energy
                            );
                            tracing::warn!("{msg}");
                            inject_text(&events, msg);
                            continue;
                        }
                        // TTS lost notification — save text for recovery
                        if val.get("type").and_then(|t| t.as_str()) == Some("tts_lost") {
                            let msg_id = val.get("msg_id").and_then(|v| v.as_str()).unwrap_or("unknown");
                            // Suppress false loss if another browser already played this TTS
                            if state.played_tts.read().await.contains(msg_id) {
                                tracing::info!(msg_id, "[TTS] Suppressed false loss — already played by another browser");
                                continue;
                            }
                            let text = val.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            let error = val.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let stage = val.get("stage").and_then(|v| v.as_str()).unwrap_or("unknown");

                            // Save the full text to disk for recovery
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let lost_dir = dirs::home_dir()
                                .unwrap_or_default()
                                .join(".b3")
                                .join("lost-tts");
                            let _ = std::fs::create_dir_all(&lost_dir);
                            let filename = format!("{msg_id}_{ts}.txt");
                            let filepath = lost_dir.join(&filename);
                            let _ = std::fs::write(&filepath, text);

                            let preview: String = text.chars().take(100).collect();
                            let msg = format!(
                                "[TTS LOST] msg={} stage={} error={} file=~/.b3/lost-tts/{} text=\"{}{}\"",
                                msg_id, stage, error, filename, preview,
                                if text.len() > 100 { "..." } else { "" }
                            );
                            tracing::warn!("{msg}");
                            inject_text(&events, msg);
                            continue;
                        }
                        // Browser switched to WS-only mode — remove dead RTC sink so frames route to WS
                        if val.get("type").and_then(|t| t.as_str()) == Some("rtc_inactive") {
                            session.rtc_active.store(false, std::sync::atomic::Ordering::Relaxed);
                            session.multi.lock().await.remove_transport("rtc");
                            tracing::info!("[WS] Browser requested rtc_inactive — removed RTC sink, WS delivers all");
                            continue;
                        }
                        // Catch-all: unrecognized JSON control message — log and drop.
                        // Without this, unknown JSON falls through to PtyInput injection,
                        // polluting the terminal prompt with stray JSON + trailing newlines.
                        let msg_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("(no type)");
                        tracing::debug!("[WS] Dropping unrecognized JSON msg type={msg_type}");
                        continue;
                    }
                    // JSON parse failed but starts with '{' — drop it, don't inject garbage
                    tracing::debug!("[WS] Dropping unparseable JSON-like message");
                    continue;
                }

                events.send(AgentEvent::PtyInput { data });
            }
            Some(Ok(Message::Close(_))) => {
                disconnect_reason = "clean_close";
                break;
            }
            None => {
                // No Close frame — abnormal closure (tunnel flap, browser crash, network drop)
                disconnect_reason = "connection_lost";
                break;
            }
            _ => {}
        }
            } // tokio::select! msg arm
        } // tokio::select!
    }

    // Structured disconnect log — captures reason, duration, and session context
    let duration_s = ws_connected_at.elapsed().as_secs();
    tracing::info!(
        "[Disconnect] session={session_id} transport=ws reason={disconnect_reason} duration_s={duration_s}"
    );

    // Clean up: remove WS transport sink from this session
    session.multi.lock().await.remove_transport("ws");
    // ALWAYS schedule cleanup — even if RTC was active. RTC sessions get a longer
    // grace period (30min) but must still be cleaned up to prevent zombie consumers.
    {
        let timeout_secs = if session.rtc_active.load(std::sync::atomic::Ordering::Relaxed) {
            1800 // 30 minutes for RTC sessions
        } else {
            300  // 5 minutes for WS-only sessions
        };
        let sm = state.session_manager.clone();
        let multi = session.multi.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)).await;
            if multi.lock().await.sink_count() == 0 {
                sm.remove(session_id).await;
                tracing::info!("[WS] Session {session_id} cleaned up (no reconnect in {timeout_secs}s)");
            }
        });
    }
    state.session_sizes.write().await.remove(&session_id);
    state.browser_versions.write().await.remove(&session_id);
    state.browser_user_agents.write().await.remove(&session_id);
    state.browser_volumes.write().await.remove(&session_id);
    state.browser_audio_contexts.write().await.remove(&session_id);
    state.session_last_seen.write().await.remove(&session_id);
    // If other sessions remain, resize PTY to their minimum
    {
        let sizes = state.session_sizes.read().await;
        if !sizes.is_empty() {
            let min_rows = sizes.values().map(|(r, _)| *r).min().unwrap_or(24);
            let min_cols = sizes.values().map(|(_, c)| *c).min().unwrap_or(80);
            drop(sizes);
            let resized = if let Some(ref resizer) = state.pty_resizer {
                if let Ok(master) = resizer.lock() {
                    master.resize(PtySize {
                        rows: min_rows, cols: min_cols,
                        pixel_width: 0, pixel_height: 0,
                    }).is_ok()
                } else { false }
            } else { false };
            if resized {
                *state.pty_size.write().await = (min_rows, min_cols);
            }
        }
    }

    // Per-session event_task + bridge are cleaned up by session_manager.remove()
    // (called above if RTC not active). If RTC is active, session stays alive.
    ping_task.abort();
}

/// Inject text into the PTY as a Transcription event.
/// The local puller in server.rs handles: write text → 500ms delay → \r (Enter).
/// ALL injection points MUST use this instead of sending PtyInput directly.
fn inject_text(events: &EventChannel, text: String) {
    events.send(AgentEvent::Transcription { text });
}

/// POST /api/inject — inject events (voice, hive) into the agent.
async fn inject(
    State(state): State<WebState>,
    Json(event): Json<AgentEvent>,
) -> impl IntoResponse {
    // Archive info events before broadcasting
    if let AgentEvent::Hive { ref sender, ref text } = event {
        if sender == "info" {
            let title = extract_info_title(text);
            state.info_archive.store(title, text.clone()).await;
        }
    }
    state.events.send(event);
    StatusCode::OK
}

/// Extract a title from HTML content (first heading or first 60 chars of text).
fn extract_info_title(html: &str) -> String {
    // Simple extraction: look for content between heading tags
    for tag in &["h1", "h2", "h3", "h4", "strong", "b"] {
        let open = format!("<{}", tag);
        let close = format!("</{}>", tag);
        if let Some(start) = html.find(&open) {
            if let Some(tag_end) = html[start..].find('>') {
                let content_start = start + tag_end + 1;
                if let Some(end) = html[content_start..].find(&close) {
                    let title = &html[content_start..content_start + end];
                    // Strip nested tags
                    let title: String = title.chars().filter(|c| *c != '<').collect();
                    let clean = title.trim();
                    if !clean.is_empty() {
                        return clean.chars().take(60).collect();
                    }
                }
            }
        }
    }
    // Fallback: strip all tags, take first 60 chars
    let stripped: String = html
        .chars()
        .scan(false, |in_tag, c| {
            if c == '<' { *in_tag = true; Some(None) }
            else if c == '>' { *in_tag = false; Some(None) }
            else if *in_tag { Some(None) }
            else { Some(Some(c)) }
        })
        .flatten()
        .collect();
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        "Info".to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

/// GET /api/info — list all info archive entries (summaries only).
async fn info_list(
    State(state): State<WebState>,
) -> impl IntoResponse {
    let entries = state.info_archive.list().await;
    Json(entries)
}

/// GET /api/info/:index — get full HTML content of a specific info entry.
async fn info_get(
    State(state): State<WebState>,
    axum::extract::Path(index): axum::extract::Path<usize>,
) -> impl IntoResponse {
    match state.info_archive.get(index).await {
        Some(entry) => Json(serde_json::json!(entry)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// GET /api/channel-events — SSE stream for the MCP voice process.
///
/// The MCP process subscribes here to receive transcription events as
/// `notifications/claude/channel` notifications instead of PTY injection.
async fn channel_events_sse(
    State(state): State<WebState>,
) -> axum::response::sse::Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    use tokio_stream::StreamExt as _;
    let rx = state.channel_events.subscribe();
    tracing::info!("MCP channel-events SSE subscriber connected");
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|result| {
            match result {
                Ok(event) => {
                    serde_json::to_string(&event)
                        .ok()
                        .map(|data| Ok(axum::response::sse::Event::default().data(data)))
                }
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "channel-events SSE subscriber lagged");
                    None
                }
            }
        });
    axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
}

/// POST /api/transcription — RunPod Whisper dual-output callback.
///
/// When the browser sends audio to RunPod Whisper, it includes this endpoint
/// as the callback_url. RunPod sends the transcription here so the daemon
/// receives it directly (no browser relay needed).
async fn transcription_callback(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let voice_job_id = body
        .get("voice_job_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if text.is_empty() {
        if !voice_job_id.is_empty() {
            report_voice_job(&state.api_url, &state.api_key, &voice_job_id, "failed",
                Some(serde_json::json!({"error": "empty_text", "error_stage": "daemon_injection"})));
        }
        return (StatusCode::OK, Json(serde_json::json!({"injected": false}))).into_response();
    }

    tracing::info!(text_len = text.len(), "Transcription callback received from RunPod");

    // Broadcast to channel-events SSE (for MCP channel notification delivery).
    // Also broadcast as AgentEvent::Transcription — puller uses it as fallback
    // when the MCP channel-events subscriber is not connected.
    let msg_id = format!("{:016x}", rand::random::<u64>());
    let channel_delivered = state.channel_events.send(ChannelEvent::Transcription {
        text: text.clone(),
        message_id: msg_id,
    }).unwrap_or(0) > 0;
    if !channel_delivered {
        // No MCP channel subscriber — fall back to PTY injection via local event loop.
        state.events.send(AgentEvent::Transcription { text });
    }
    tracing::info!(channel_delivered, "Transcription callback: channel_delivered={channel_delivered}");
    // Credits deducted by GPU worker, not daemon

    // Report successful injection to voice-jobs ledger
    if !voice_job_id.is_empty() {
        report_voice_job(&state.api_url, &state.api_key, &voice_job_id, "injected", None);
    }

    // Return structured response so browser knows injection succeeded
    (StatusCode::OK, Json(serde_json::json!({"injected": true}))).into_response()
}

/// POST /api/tts-audio — RunPod GPU audio delivery callback.
///
/// GPU worker calls this after synthesizing a chunk. The full payload
/// (including audio_b64) is broadcast to all WebSocket clients as-is.
/// No disk storage — the daemon is a thin pipe from GPU to browsers.
///
/// Browsers decode audio_b64 into a Blob and play directly.
/// EC2 receives a parallel callback from RunPod for persistence/logging.
async fn tts_audio_callback(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let msg_id = body.get("msg_id").and_then(|v| v.as_str()).unwrap_or("");
    let chunk = body.get("chunk").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let total = body.get("total").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let duration = body.get("duration_sec").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let gen_time = body.get("generation_sec").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let has_audio = body.get("audio_b64").and_then(|v| v.as_str()).is_some();

    if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
        tracing::error!(msg_id = %msg_id, chunk, "GPU TTS error: {err}");
        return StatusCode::BAD_REQUEST;
    }

    if !has_audio {
        tracing::error!(msg_id = %msg_id, chunk, "TTS callback missing audio_b64");
        return StatusCode::BAD_REQUEST;
    }

    tracing::info!(
        msg_id = %msg_id, chunk, total,
        duration_sec = duration, generation_sec = gen_time,
        "TTS audio from GPU → broadcasting to browsers"
    );

    // Broadcast the ENTIRE payload (with audio_b64) to all WebSocket clients.
    // Browsers decode base64 → Blob → Audio. No disk round-trip.
    state.events.send(AgentEvent::TtsAudioData {
        msg_id: msg_id.to_string(),
        chunk,
        total,
        audio_b64: body.get("audio_b64").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        duration_sec: duration,
        generation_sec: gen_time,
    });

    StatusCode::OK
}

/// Fetch the agent's `force_lowercase` setting from EC2.
/// Returns true (the default) if the request fails or the key is absent.
pub async fn fetch_force_lowercase(api_url: &str, agent_id: &str, api_key: &str) -> bool {
    let url = format!("{api_url}/api/agents/{agent_id}/settings");
    let client = reqwest::Client::new();
    let resp = client.get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(body) = r.json::<serde_json::Value>().await {
                // Default to true if key is absent (backward compat)
                body.get("force_lowercase").and_then(|v| v.as_bool()).unwrap_or(true)
            } else {
                true
            }
        }
        _ => true,
    }
}

/// POST /api/settings/refresh — re-fetch agent settings from EC2.
/// Called by the browser when settings change in the drawer (e.g., force_lowercase toggle).
async fn refresh_settings(
    State(state): State<WebState>,
) -> impl IntoResponse {
    let val = fetch_force_lowercase(&state.api_url, &state.agent_id, &state.api_key).await;
    *state.force_lowercase.write().await = val;
    tracing::info!(force_lowercase = val, "Settings refreshed from EC2");
    Json(serde_json::json!({ "force_lowercase": val }))
}

/// POST /api/tts — streaming TTS via GPU worker.
///
/// Fetch the agent's configured voice from EC2 settings.
/// Returns None if the request fails or no voice is set.
async fn resolve_agent_voice(api_url: &str, agent_id: &str, api_key: &str) -> Option<String> {
    let url = format!("{api_url}/api/agents/{agent_id}/voice-preference");
    let client = reqwest::Client::new();
    let resp = client.get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("voice").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

/// Daemon-side TTS generation and archival. Used by both the "no browsers"
/// immediate path and the watchdog timer fallback when browsers fail to archive.
async fn daemon_generate_and_archive(
    gpu: std::sync::Arc<super::gpu_client::GpuClient>,
    archive: std::sync::Arc<super::tts_archive::TtsArchive>,
    api_url: String,
    agent_id: String,
    api_key: String,
    msg_id: String,
    text: String,
    voice: String,
    emotion: String,
    origin: &str,
) {
    // Resolve "default" voice to agent's configured voice from EC2
    let effective_voice = if voice.is_empty() || voice == "default" {
        match resolve_agent_voice(&api_url, &agent_id, &api_key).await {
            Some(v) => {
                tracing::info!(voice = %v, "Resolved agent voice from EC2");
                v
            }
            None => "arabella".to_string(),
        }
    } else {
        voice
    };
    match gpu.generate_tts(&text, &effective_voice, &msg_id, &api_url, &agent_id, &api_key).await {
        Ok((wav_bytes, duration_sec)) => {
            let entry = b3_common::tts::TtsEntry {
                msg_id: msg_id.clone(),
                text,
                original_text: String::new(),
                voice: effective_voice,
                emotion,
                replying_to: String::new(), // daemon-generated TTS has no replying_to context
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                duration_sec,
                size_bytes: wav_bytes.len() as u64,
            };
            if let Err(e) = archive.store(entry, &wav_bytes).await {
                tracing::warn!(msg_id = %msg_id, error = %e, "Failed to archive TTS");
            } else {
                tracing::info!(msg_id = %msg_id, origin, "TTS archived (daemon-generated)");
                report_voice_job(&api_url, &api_key, &msg_id, "done", Some(serde_json::json!({
                    "audio_output": "daemon_archive",
                    "origin": origin,
                })));
            }
        }
        Err(e) => {
            tracing::warn!(msg_id = %msg_id, origin, error = %e, "Daemon TTS generation failed");
            report_voice_job(&api_url, &api_key, &msg_id, "failed", Some(serde_json::json!({
                "error": format!("{e}").chars().take(200).collect::<String>(),
                "error_stage": "daemon_tts_generation",
                "origin": origin,
            })));
        }
    }
}

/// Called by the Voice MCP's voice_say tool. Cleans text, then sends a
/// single TtsStream event to browsers. Browser calls GPU /run (async),
/// polls /stream/{jobId} for sub-chunks, plays via GaplessStreamPlayer.
///
/// Browser handles GPU routing directly (local-first + RunPod fallback).
/// Daemon just sends text/voice/emotion — no GPU URLs.
async fn tts(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let msg_id = body
        .get("msg_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let voice = body
        .get("voice")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let emotion = body
        .get("emotion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let replying_to = body
        .get("replying_to")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "text is required"}))).into_response();
    }

    // ── Instant pre-flight: read WS state + credit check (capped at 200ms) ──
    // Zero network for browser info — just reads HashMaps maintained on every WS message.
    // GPU probe removed: browser discovers GPU failures at /run time.

    // 1. Instant browser info from WS connection state (zero-cost reads)
    // Only count clients that sent a "version" message — those are real browsers.
    // Terminal sessions (tmux attach) only send "resize" and never "version".
    let browser_profiles: Vec<serde_json::Value> = {
        let versions = state.browser_versions.read().await;
        let sizes = state.session_sizes.read().await;
        let last_seen = state.session_last_seen.read().await;
        let user_agents = state.browser_user_agents.read().await;
        let volumes = state.browser_volumes.read().await;
        let audio_ctxs = state.browser_audio_contexts.read().await;
        versions.keys().map(|id| {
            let ver = versions.get(id).cloned().unwrap_or_default();
            let ago = last_seen.get(id)
                .map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
            let ua = user_agents.get(id).cloned().unwrap_or_default();
            let vol = volumes.get(id).copied().unwrap_or(100);
            let ac = audio_ctxs.get(id).copied().unwrap_or(false);
            let (rows, cols) = sizes.get(id).copied().unwrap_or((0, 0));
            serde_json::json!({
                "session_id": id, "version": ver, "last_seen_ms": ago,
                "user_agent": ua, "volume": vol, "audio_context": ac,
                "rows": rows, "cols": cols
            })
        }).collect()
    };
    // For TTS routing: only browsers with an active audio context can generate/play TTS.
    // A browser tab without audio context (never interacted) won't play audio anyway.
    let live_clients = browser_profiles.iter()
        .filter(|p| p.get("audio_context").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();

    // Credit check moved to GPU worker — daemon no longer gates on credits

    tracing::info!(msg_id = %msg_id, live_clients, "TTS pre-flight complete (instant)");

    // Clean text and dispatch to browser (browser handles GPU routing directly)
    let force_lowercase = *state.force_lowercase.read().await;
    let cleaned = clean_for_tts(&text, force_lowercase);

    tracing::info!(msg_id = %msg_id, text_len = text.len(), cleaned_len = cleaned.len(), "TTS streaming — browser will route to GPU");

    // Register TTS job with EC2 ledger (daemon is the originator)
    register_voice_job(
        &state.api_url, &state.api_key, &msg_id, &state.agent_id, "tts",
        Some(serde_json::json!({
            "msg_id": msg_id,
            "text_preview": if cleaned.len() > 80 { &cleaned[..80] } else { &cleaned },
            "voice": voice,
        })),
    );

    // Broadcast generating event
    state.events.send(AgentEvent::TtsGenerating {
        msg_id: msg_id.clone(),
        chunk: 0,
        total: 1,
    });

    // Fetch voice conditionals — browser needs these since it sends TTS requests
    // directly to the GPU sidecar (no daemon in path). Without conditionals, the
    // GPU worker tries to load a .wav file from /voices/ which may not be mounted.
    let conditionals_b64 = state.gpu_client
        .fetch_conditionals(&voice, &state.api_url, &state.agent_id, &state.api_key)
        .await;

    // Send TtsStream event — browser handles GPU selection (local-first + RunPod fallback)
    let cleaned_for_archive = cleaned.clone();
    let emotion_for_archive = emotion.clone();
    let tts_event = AgentEvent::TtsStream {
        msg_id: msg_id.clone(),
        text: cleaned,
        original_text: text.clone(),
        voice: voice.clone(),
        emotion,
        replying_to,
        split_chars: ".!?".to_string(),
        min_chunk_chars: 80,
        conditionals_b64,
    };
    state.events.send(tts_event);

    // Daemon-side TTS generation — generate audio directly and archive it.
    // Two paths: (1) no browsers → generate immediately, (2) browsers connected
    // → watchdog timer checks after 120s and generates if browser dropped the ball.
    {
        let gpu = state.gpu_client.clone();
        let archive = state.tts_archive.clone();
        let api_url = state.api_url.clone();
        let agent_id = state.agent_id.clone();
        let api_key = state.api_key.clone();
        let archive_msg_id = msg_id.clone();
        let archive_voice = voice.clone();

        if live_clients == 0 {
            // No browsers — generate immediately
            tokio::spawn(daemon_generate_and_archive(
                gpu, archive, api_url, agent_id, api_key,
                archive_msg_id, cleaned_for_archive, archive_voice, emotion_for_archive,
                "no_browsers",
            ));
        } else {
            // Browsers connected — progress-aware watchdog.
            // Polls every 30s: if the browser is still sending voice events (chunk
            // delivery, playback, etc.) we keep waiting. Only generate daemon-side
            // if the browser goes silent for 30s+ AND the archive is still missing.
            let tts_progress = state.tts_progress.clone();
            tokio::spawn(async move {
                // Initial grace period — give the browser time to start generating
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                loop {
                    // Check if browser already archived it
                    if archive.contains(&archive_msg_id).await {
                        tracing::debug!(msg_id = %archive_msg_id, "Archive watchdog: already archived, skipping");
                        // Clean up progress tracking
                        tts_progress.write().await.remove(&*archive_msg_id);
                        return;
                    }

                    // Check if browser is still making progress
                    let last_event = tts_progress.read().await.get(&*archive_msg_id).copied();
                    if let Some(t) = last_event {
                        if t.elapsed() < std::time::Duration::from_secs(30) {
                            // Browser is active — back off and check again
                            tracing::debug!(
                                msg_id = %archive_msg_id,
                                last_event_ago_ms = t.elapsed().as_millis(),
                                "Archive watchdog: browser making progress, waiting"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                            continue;
                        }
                    }

                    // No recent progress and no archive — browser stalled or disconnected
                    tracing::warn!(msg_id = %archive_msg_id, "Archive watchdog: browser stalled, daemon generating");
                    tts_progress.write().await.remove(&*archive_msg_id);
                    daemon_generate_and_archive(
                        gpu, archive, api_url, agent_id, api_key,
                        archive_msg_id, cleaned_for_archive, archive_voice, emotion_for_archive,
                        "watchdog",
                    ).await;
                    return;
                }
            });
        }
    }

    // Credits deducted by GPU worker, not daemon
    let credits_remaining: Option<i64> = None;

    tracing::info!(msg_id = %msg_id, live_clients, credits = ?credits_remaining, "TTS stream dispatched");

    Json(serde_json::json!({
        "status": "streaming",
        "msg_id": msg_id,
        "clients": live_clients,
        "browsers": browser_profiles,
        "credits_remaining": credits_remaining,
    })).into_response()
}


/// POST /api/audio-upload — transcribe audio via RunPod GPU worker or local Whisper.
///
/// Tries RunPod first (RUNPOD_GPU_ID + RUNPOD_API_KEY), falls back to local Whisper
/// (WHISPER_URL, default localhost:5124). Injects transcription into PTY on success.
async fn audio_upload(
    State(state): State<WebState>,
    mut multipart: axum::extract::Multipart,
) -> Result<impl IntoResponse, StatusCode> {
    // Extract audio bytes + optional preroll from multipart
    let mut audio_bytes: Option<Vec<u8>> = None;
    let mut preroll_bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "audio" | "file" => match field.bytes().await {
                Ok(bytes) => audio_bytes = Some(bytes.to_vec()),
                Err(_) => return Err(StatusCode::BAD_REQUEST),
            },
            "preroll" => {
                if let Ok(bytes) = field.bytes().await {
                    if !bytes.is_empty() {
                        preroll_bytes = Some(bytes.to_vec());
                    }
                }
            }
            _ => {}
        }
    }

    let audio_bytes = audio_bytes.ok_or(StatusCode::BAD_REQUEST)?;
    if audio_bytes.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    tracing::info!(
        audio_size = audio_bytes.len(),
        preroll_size = preroll_bytes.as_ref().map(|b| b.len()).unwrap_or(0),
        "Audio upload received at embedded server"
    );

    // Credit check moved to GPU worker — daemon no longer gates on credits

    let runpod_api_key = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
    let runpod_gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
    let local_gpu_url = std::env::var("LOCAL_GPU_URL").unwrap_or_default();
    let local_gpu_token = std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default();
    let use_runpod = !runpod_api_key.is_empty() && !runpod_gpu_id.is_empty();
    let use_local_gpu = !local_gpu_url.is_empty();

    let client = crate::http::build_client(std::time::Duration::from_secs(60))
        .unwrap_or_default();

    if use_local_gpu || use_runpod {
        let audio_b64 = base64::engine::general_purpose::STANDARD.encode(&audio_bytes);
        let payload = serde_json::json!({
            "input": {
                "action": "transcribe",
                "audio_b64": audio_b64,
                "language": "en",
                "agent_id": state.agent_id,
            }
        });

        // Try local GPU first (2s connect timeout), then RunPod fallback
        let rp = 'gpu: {
            if use_local_gpu {
                let local_url = format!("{}/runsync", local_gpu_url.trim_end_matches('/'));
                tracing::info!(audio_size = audio_bytes.len(), backend = "local-gpu", "STT: trying local GPU");
                let local_client = crate::http::build_client(std::time::Duration::from_secs(2))
                    .unwrap_or_default();
                match local_client.post(&local_url)
                    .header("Authorization", format!("Bearer {}", local_gpu_token))
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<serde_json::Value>().await {
                            Ok(rp) if rp.get("error").and_then(|v| v.as_str()).is_none() => {
                                tracing::info!("STT: local GPU succeeded");
                                break 'gpu rp;
                            }
                            Ok(rp) => tracing::warn!("Local GPU error: {:?}", rp.get("error")),
                            Err(e) => tracing::warn!("Local GPU response parse failed: {e}"),
                        }
                    }
                    Ok(resp) => tracing::warn!("Local GPU returned {}", resp.status()),
                    Err(e) => tracing::warn!("Local GPU unreachable: {e}"),
                }
            }

            // RunPod fallback
            if !use_runpod {
                tracing::error!("No GPU backend available for STT");
                return Err(StatusCode::BAD_GATEWAY);
            }
            let url = format!("https://api.runpod.ai/v2/{}/runsync", runpod_gpu_id);
            tracing::info!(audio_size = audio_bytes.len(), backend = "runpod-cloud", "STT: falling back to RunPod");
            match client.post(&url)
                .header("Authorization", &runpod_api_key)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(rp) => {
                            if let Some(err) = rp.get("error").and_then(|v| v.as_str()) {
                                tracing::error!("RunPod transcribe error: {err}");
                                return Err(StatusCode::BAD_GATEWAY);
                            }
                            rp
                        }
                        Err(e) => {
                            tracing::error!("Failed to parse RunPod response: {e}");
                            return Err(StatusCode::BAD_GATEWAY);
                        }
                    }
                }
                Ok(resp) => {
                    tracing::error!("RunPod transcribe returned {}", resp.status());
                    return Err(StatusCode::BAD_GATEWAY);
                }
                Err(e) => {
                    tracing::error!("RunPod transcribe request failed: {e}");
                    return Err(StatusCode::BAD_GATEWAY);
                }
            }
        };

        // Parse dual transcription results from /output
        let output = rp.pointer("/output").unwrap_or(&rp);
        let medium_text = output.pointer("/text").and_then(|v| v.as_str()).unwrap_or("").trim();
        let wx_text = output.pointer("/whisperx_text").and_then(|v| v.as_str()).unwrap_or("").trim();
        let medium_elapsed = output.pointer("/medium_elapsed").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let wx_elapsed = output.pointer("/whisperx_elapsed").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let embedding_sim = output.pointer("/embedding_similarity").and_then(|v| v.as_f64());

        // Confidence tier (same thresholds as whisper_api.py)
        let tier = match embedding_sim {
            Some(s) if s >= 0.85 => "high",
            Some(s) if s >= 0.60 => "medium",
            Some(_) => "noise",
            None if !medium_text.is_empty() || !wx_text.is_empty() => "medium",
            None => "noise",
        };

        tracing::info!(
            medium_text,
            wx_text,
            ?embedding_sim,
            tier,
            "Dual transcription result"
        );

        // Filter noise — skip injection
        if tier == "noise" {
            return Ok(Json(serde_json::json!({
                "filtered": "noise",
                "embedding_similarity": embedding_sim,
                "medium_text": medium_text,
                "whisperx_text": wx_text,
                "injected": false,
            })).into_response());
        }

        // Compose dual-format injection string
        let sim_pct = embedding_sim.map(|s| format!("{}%", (s * 100.0).round() as i32)).unwrap_or_else(|| "n/a".into());
        let injection = if wx_text.is_empty() {
            format!(
                "[text model: {:.1}s] {} [self-similarity: {}]",
                medium_elapsed, medium_text, sim_pct,
            )
        } else {
            format!(
                "[text model: {:.1}s] {} [voices model: {:.1}s] {} [self-similarity: {}]",
                medium_elapsed,
                if medium_text.is_empty() { "(empty)" } else { medium_text },
                wx_elapsed,
                if wx_text.is_empty() { "(empty)" } else { wx_text },
                sim_pct,
            )
        };

        if !injection.is_empty() {
            let msg_id = format!("{:016x}", rand::random::<u64>());
            let channel_delivered = state.channel_events.send(ChannelEvent::Transcription {
                text: injection.clone(),
                message_id: msg_id,
            }).unwrap_or(0) > 0;
            if !channel_delivered {
                state.events.send(AgentEvent::Transcription {
                    text: injection.clone(),
                });
            }
        }

        Ok(Json(serde_json::json!({
            "transcription": medium_text,
            "whisperx": wx_text,
            "confidence": tier,
            "embedding_similarity": embedding_sim,
            "injected": !medium_text.is_empty(),
        })).into_response())
    } else {
        // ── Local Whisper (dev mode, single model) ──
        let whisper_url = std::env::var("WHISPER_URL")
            .unwrap_or_else(|_| "http://localhost:5124/transcribe".to_string());
        tracing::info!(audio_size = audio_bytes.len(), backend = "local-whisper", whisper_url = %whisper_url, "STT transcription via local Whisper");

        let part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name("audio.webm")
            .mime_str("audio/webm")
            .unwrap_or_else(|_| {
                reqwest::multipart::Part::bytes(vec![])
                    .file_name("audio.webm")
            });

        let form = reqwest::multipart::Form::new()
            .part("audio", part)
            .text("inject", "false");

        let text = match client.post(&whisper_url).multipart(form).send().await {
            Ok(r) if r.status().is_success() => {
                match r.json::<serde_json::Value>().await {
                    Ok(body) => {
                        body.get("text")
                            .or_else(|| body.get("transcription"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse Whisper response: {e}");
                        return Err(StatusCode::BAD_GATEWAY);
                    }
                }
            }
            Ok(r) => {
                tracing::error!("Whisper returned {}", r.status());
                return Err(StatusCode::BAD_GATEWAY);
            }
            Err(e) => {
                tracing::error!("Whisper request failed: {e}. Set RUNPOD_API_KEY and RUNPOD_GPU_ID env vars, or run a local Whisper at {}", whisper_url);
                return Ok(Json(serde_json::json!({
                    "error": "stt_unavailable",
                    "message": format!("Speech-to-text unavailable. Local Whisper at {} is not reachable. Set RUNPOD_API_KEY and RUNPOD_GPU_ID for cloud transcription, or start a local Whisper server.", whisper_url),
                    "diagnostics_url": "/api/diagnostics"
                })).into_response());
            }
        };

        if !text.is_empty() {
            let msg_id = format!("{:016x}", rand::random::<u64>());
            let channel_delivered = state.channel_events.send(ChannelEvent::Transcription {
                text: text.clone(),
                message_id: msg_id,
            }).unwrap_or(0) > 0;
            if !channel_delivered {
                state.events.send(AgentEvent::Transcription {
                    text: text.clone(),
                });
            }
        }

        Ok(Json(serde_json::json!({
            "transcription": text,
            "injected": !text.is_empty(),
        })).into_response())
    }
}

/// POST /api/voice-event — browser reports delivery/playback events.
///
/// Events: "delivered" (chunk downloaded), "started" (playback began),
///         "finished" (playback ended), "error" (playback failed).
///
/// Browser sends these so the daemon (and EC2) can track what's actually
/// playing in the user's ears, not just what was generated.
async fn voice_event(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let msg_id = body.get("msg_id").and_then(|v| v.as_str()).unwrap_or("");
    let chunk = body.get("chunk").and_then(|v| v.as_u64());
    let event = body.get("event").and_then(|v| v.as_str()).unwrap_or("");
    let session_id = body.get("session_id").and_then(|v| v.as_str()).unwrap_or("unknown");

    tracing::info!(
        msg_id = %msg_id, chunk = ?chunk, event = %event, session_id = %session_id,
        "Voice event from browser"
    );

    // Track played msg_ids — "delivered" or "finished" means at least one browser played it.
    // Suppresses false [TTS LOST] from stale browsers that timeout on the same msg_id.
    if (event == "delivered" || event == "started" || event == "finished") && !msg_id.is_empty() {
        state.played_tts.write().await.insert(msg_id.to_string());
        // Update progress timestamp — archive watchdog uses this to detect active browser work.
        state.tts_progress.write().await.insert(msg_id.to_string(), std::time::Instant::now());
    }

    // Forward to EC2 for persistent tracking (fire-and-forget)
    let api_url = state.api_url.clone();
    let body_clone = body.clone();
    tokio::spawn(async move {
        let client = crate::http::build_client(std::time::Duration::from_secs(5))
            .unwrap_or_default();
        let url = format!("{}/api/voice-event", api_url);
        let _ = client.post(&url).json(&body_clone).send().await;
    });

    StatusCode::OK
}

/// POST /api/voice-report — browser reports playback complete or error.
///
/// Injects a one-liner into the PTY so the agent knows the outcome of a
/// voice_say call. Agent can then use browser_console(filter="msg_id")
/// for full details from the existing log stream.
async fn voice_report(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let msg_id = body.get("msg_id").and_then(|v| v.as_str()).unwrap_or("unknown");
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
    let scheduled = body.get("chunks_scheduled").and_then(|v| v.as_u64());
    let played = body.get("chunks_played").and_then(|v| v.as_u64());

    let chunks_info = match (played, scheduled) {
        (Some(p), Some(s)) => format!(" chunks={}/{}", p, s),
        _ => String::new(),
    };
    let error_info = body.get("error").and_then(|v| v.as_str())
        .map(|e| format!(" error={}", e)).unwrap_or_default();
    let stage_info = body.get("stage").and_then(|v| v.as_str())
        .map(|s| format!(" stage={}", s)).unwrap_or_default();

    let session_id = body.get("session_id")
        .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())))
        .unwrap_or_default();
    tracing::info!(msg_id = %msg_id, status = %status, session_id = %session_id, "Voice report from browser");

    // Track played msg_ids — suppresses false [TTS LOST] from other browsers
    if status == "delivered" || status == "finished" || status == "played" || status == "complete" {
        state.played_tts.write().await.insert(msg_id.to_string());
        // Update progress timestamp — archive watchdog uses this to detect active browser work.
        state.tts_progress.write().await.insert(msg_id.to_string(), std::time::Instant::now());
    }

    // Each browser does its own TTS generation — every report is a real playback event.
    // Include session_id and volume so the agent can distinguish browsers and detect muted playback.
    let session_info = if !session_id.is_empty() { format!(" session={}", session_id) } else { String::new() };
    let volume = body.get("volume").and_then(|v| v.as_u64());
    let vol_info = volume.map(|v| format!(" vol={}%", v)).unwrap_or_default();

    // Only inject into PTY if the browser says to (show_voice_report setting).
    // Default to true for backward compat with older browsers that don't send the flag.
    let should_inject = body.get("inject").and_then(|v| v.as_bool()).unwrap_or(true);
    if should_inject {
        inject_text(&state.events, format!(
            "[voice-report] msg={} status={}{}{}{}{}{}", msg_id, status, chunks_info, stage_info, error_info, session_info, vol_info
        ));
    }

    StatusCode::OK
}

// ── Credit Helpers ──────────────────────────────────────────────────
//
// When the "billing" feature is disabled (open-source builds), all credit
// checks return "allowed" and deductions are no-ops. This lets forks run
// without an EC2 billing backend.

// Billing enforcement moved to GPU worker (handler.py).
// The GPU worker checks credits before processing and deducts after completion.
// The daemon no longer participates in credit checks or deductions.

// ── File Upload ─────────────────────────────────────────────────────

/// POST /api/file-upload — upload a file and inject a notice into the PTY.
///
/// Accepts multipart form data with a "file" field and optional "dest_dir" field.
/// If dest_dir is provided, saves directly as {dest_dir}/{original_name} (no timestamp prefix).
/// Otherwise saves to ~/uploads/{timestamp}_{sanitized_filename}.
/// Injects "[file-upload] {path} ({size} KB)" into the terminal.
async fn file_upload(
    State(state): State<WebState>,
    mut multipart: axum::extract::Multipart,
) -> Result<impl IntoResponse, StatusCode> {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut original_name = String::from("upload");
    let mut dest_dir: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            if let Some(fname) = field.file_name() {
                original_name = fname.to_string();
            }
            match field.bytes().await {
                Ok(bytes) => file_bytes = Some(bytes.to_vec()),
                Err(_) => return Err(StatusCode::BAD_REQUEST),
            }
        } else if name == "dest_dir" {
            if let Ok(bytes) = field.bytes().await {
                dest_dir = Some(String::from_utf8_lossy(&bytes).to_string());
            }
        }
    }

    let file_bytes = file_bytes.ok_or(StatusCode::BAD_REQUEST)?;

    // 50 MB limit
    if file_bytes.len() > 50 * 1024 * 1024 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Sanitize filename
    let safe_name = original_name
        .replace('/', "_")
        .replace('\\', "_")
        .replace("..", "_");

    let (filepath, relative_path) = if let Some(ref dir) = dest_dir {
        // Upload to specific directory (from file browser)
        let dir_path = std::path::PathBuf::from(dir);
        if !dir_path.is_dir() {
            return Err(StatusCode::BAD_REQUEST);
        }
        let fp = dir_path.join(&safe_name);
        let rp = fp.to_string_lossy().to_string();
        (fp, rp)
    } else {
        // Default: ~/uploads/ with timestamp prefix
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let filename = format!("{ts}_{safe_name}");
        let upload_dir = dirs::home_dir()
            .unwrap_or_default()
            .join("uploads");
        let _ = std::fs::create_dir_all(&upload_dir);
        let fp = upload_dir.join(&filename);
        let rp = fp.to_string_lossy().to_string();
        (fp, rp)
    };

    if let Err(e) = std::fs::write(&filepath, &file_bytes) {
        tracing::error!("Failed to save upload: {e}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let size_kb = file_bytes.len() as f64 / 1024.0;

    tracing::info!("File uploaded: {relative_path} ({:.0} KB)", size_kb);

    let notice = format!(
        "[file-upload] {relative_path} ({:.0} KB)",
        size_kb
    );
    inject_text(&state.events, notice);

    Ok(Json(serde_json::json!({
        "status": "ok",
        "filename": safe_name,
        "path": relative_path,
        "size_kb": (size_kb as u64),
    })).into_response())
}

/// POST /api/file-upload-json — JSON-based upload for HTTP-over-WS relay path.
///
/// Accepts JSON: { "filename": "name.png", "data_b64": "base64...", "dest_dir": "/optional/path" }
/// Same logic as file_upload but works through the text-only WS relay channel.
async fn file_upload_json(
    State(state): State<WebState>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, StatusCode> {
    let filename = body.get("filename").and_then(|v| v.as_str()).unwrap_or("upload").to_string();
    let data_b64 = body.get("data_b64").and_then(|v| v.as_str()).ok_or(StatusCode::BAD_REQUEST)?;
    let dest_dir = body.get("dest_dir").and_then(|v| v.as_str()).map(|s| s.to_string());

    use base64::Engine as _;
    let file_bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // 50 MB limit
    if file_bytes.len() > 50 * 1024 * 1024 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Sanitize filename
    let safe_name = filename
        .replace('/', "_")
        .replace('\\', "_")
        .replace("..", "_");

    let (filepath, relative_path) = if let Some(ref dir) = dest_dir {
        let dir_path = std::path::PathBuf::from(dir);
        if !dir_path.is_dir() {
            return Err(StatusCode::BAD_REQUEST);
        }
        let fp = dir_path.join(&safe_name);
        let rp = fp.to_string_lossy().to_string();
        (fp, rp)
    } else {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let filename = format!("{ts}_{safe_name}");
        let upload_dir = dirs::home_dir()
            .unwrap_or_default()
            .join("uploads");
        let _ = std::fs::create_dir_all(&upload_dir);
        let fp = upload_dir.join(&filename);
        let rp = fp.to_string_lossy().to_string();
        (fp, rp)
    };

    if let Err(e) = std::fs::write(&filepath, &file_bytes) {
        tracing::error!("Failed to save upload: {e}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let size_kb = file_bytes.len() as f64 / 1024.0;
    tracing::info!("File uploaded (JSON): {relative_path} ({:.0} KB)", size_kb);

    let notice = format!("[file-upload] {relative_path} ({:.0} KB)", size_kb);
    inject_text(&state.events, notice);

    Ok(axum::Json(serde_json::json!({
        "status": "ok",
        "filename": safe_name,
        "path": relative_path,
        "size_kb": (size_kb as u64),
    })).into_response())
}

// ── Lost Audio Recovery ────────────────────────────────────────────

/// POST /api/lost-audio — save audio from a failed transcription for later recovery.
///
/// Accepts JSON body:
///   - "audio_b64": base64-encoded audio blob (webm)
///   - "seq": sequence number (integer)
///   - "duration": duration in seconds (float)
///   - "energy": average energy level (float)
///
/// JSON replaces the previous multipart form to work correctly through the
/// EC2 proxy path (sendHttpRequest does not serialize FormData correctly).
///
/// Saves to ~/.b3/lost-audio/lost_seq{N}_{timestamp}.webm
async fn lost_audio(
    State(state): State<WebState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<impl IntoResponse, StatusCode> {
    let audio_b64 = body.get("audio_b64")
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let seq: u64 = body.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
    let duration: f64 = body.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let energy: f64 = body.get("energy").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let audio_bytes = base64::engine::general_purpose::STANDARD
        .decode(audio_b64)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // 20 MB limit for audio
    if audio_bytes.len() > 20 * 1024 * 1024 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!("lost_seq{seq}_{ts}.webm");

    // Save to ~/.b3/lost-audio/
    let lost_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".b3")
        .join("lost-audio");
    let _ = std::fs::create_dir_all(&lost_dir);
    let filepath = lost_dir.join(&filename);

    if let Err(e) = std::fs::write(&filepath, &audio_bytes) {
        tracing::error!("Failed to save lost audio: {e}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let size_kb = audio_bytes.len() as f64 / 1024.0;
    tracing::info!(
        "Lost audio saved: {filename} ({:.0} KB, {:.1}s, energy={:.0})",
        size_kb, duration, energy
    );

    let abs_path = filepath.display();
    let local_gpu = std::env::var("LOCAL_GPU_URL").unwrap_or_default();
    let recover_url = if !local_gpu.is_empty() {
        format!("{}/runsync", local_gpu.trim_end_matches('/'))
    } else {
        let gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
        if gpu_id.is_empty() {
            "<SET_RUNPOD_GPU_ID_OR_LOCAL_GPU_URL>".to_string()
        } else {
            format!("https://api.runpod.ai/v2/{}/runsync", gpu_id)
        }
    };
    let notice = format!(
        "[LOST AUDIO SAVED] seq={seq} duration={duration:.1}s file={abs_path} ({size_kb:.0} KB) — \
        RECOVER: python3 -c \"import json,base64;d=base64.b64encode(open('{abs_path}','rb').read()).decode();\
        open('/tmp/rp.json','w').write(json.dumps({{'input':{{'action':'transcribe','audio_b64':d,'language':'en','preset':'multilingual'}}}}))\" && \
        curl -s -m 60 -H \"Authorization: Bearer $LOCAL_GPU_TOKEN\" -H \"Content-Type: application/json\" \
        -d @/tmp/rp.json {recover_url} | python3 -m json.tool"
    );
    inject_text(&state.events, notice);

    Ok(Json(serde_json::json!({
        "status": "ok",
        "filename": filename,
        "path": format!("~/.b3/lost-audio/{filename}"),
        "size_kb": (size_kb as u64),
        "seq": seq,
        "duration": duration,
    })).into_response())
}

// ── Hive Proxy ─────────────────────────────────────────────────────
//
// Thin passthrough to EC2 `/api/hive/*` endpoints. The daemon proxies
// requests so MCP tools (running inside the daemon process) can call
// hive endpoints without needing to resolve the server URL themselves.

/// Proxy helper: forward a GET request to the EC2 hive API.
async fn hive_proxy_get(
    state: &WebState,
    path: &str,
) -> impl IntoResponse {
    let url = format!("{}/api/hive{}", state.api_url, path);
    let client = crate::http::build_client(std::time::Duration::from_secs(10))
        .unwrap_or_default();

    match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", state.api_key))
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = resp.text().await.unwrap_or_default();
            (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Err(e) => {
            (StatusCode::BAD_GATEWAY, format!("hive proxy error: {e}")).into_response()
        }
    }
}

/// Proxy helper: forward a POST request with JSON body to the EC2 hive API.
async fn hive_proxy_post(
    state: &WebState,
    path: &str,
    body: &str,
) -> impl IntoResponse {
    let url = format!("{}/api/hive{}", state.api_url, path);
    let client = crate::http::build_client(std::time::Duration::from_secs(10))
        .unwrap_or_default();

    match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", state.api_key))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = resp.text().await.unwrap_or_default();
            (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Err(e) => {
            (StatusCode::BAD_GATEWAY, format!("hive proxy error: {e}")).into_response()
        }
    }
}

async fn hive_agents(State(state): State<WebState>) -> impl IntoResponse {
    hive_proxy_get(&state, "/agents").await.into_response()
}

async fn hive_send(
    State(state): State<WebState>,
    body: String,
) -> impl IntoResponse {
    // E2E encrypt the message before proxying to EC2.
    // Parse the request, encrypt the message field, re-serialize.
    let mut req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    let target = match req.get("target").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return (StatusCode::BAD_REQUEST, "missing target").into_response(),
    };
    let plaintext = match req.get("message").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return (StatusCode::BAD_REQUEST, "missing message").into_response(),
    };
    let ephemeral = req.get("ephemeral").and_then(|v| v.as_bool()).unwrap_or(false);
    let deliver_at = req.get("deliver_at").and_then(|v| v.as_str()).map(String::from);

    // Timelock path: generate symmetric key, encrypt, send key to server
    if let Some(ref ts) = deliver_at {
        match crate::crypto::hive_integration::encrypt_timelock(&plaintext) {
            Ok((ciphertext_json, unlock_key_b64)) => {
                req["message"] = serde_json::Value::String(ciphertext_json);
                req["deliver_at"] = serde_json::Value::String(ts.clone());
                req["unlock_key"] = serde_json::Value::String(unlock_key_b64);
                // Key is now only in req — sender has no copy after this function returns
                tracing::info!("Timelock message to {target}, deliver_at={ts}");
                let encrypted_body = req.to_string();
                return hive_proxy_post(&state, "/send", &encrypted_body).await.into_response();
            }
            Err(e) => {
                tracing::error!("Timelock encryption failed: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("Timelock encryption failed: {e}")).into_response();
            }
        }
    }

    let encrypt_result = if ephemeral {
        crate::crypto::hive_integration::encrypt_dm_ephemeral_for_target(
            &state.api_url,
            &state.api_key,
            &state.agent_name,
            &state.agent_id,
            &target,
            &plaintext,
        )
        .await
    } else {
        crate::crypto::hive_integration::encrypt_dm_for_target(
            &state.api_url,
            &state.api_key,
            &state.agent_name,
            &state.agent_id,
            &target,
            &plaintext,
        )
        .await
    };

    match encrypt_result {
        Ok(encrypted_json) => {
            // Replace the plaintext message with the encrypted envelope
            req["message"] = serde_json::Value::String(encrypted_json);
            // Tell EC2 this is ephemeral so it can delete after delivery
            if ephemeral {
                req["ephemeral"] = serde_json::Value::Bool(true);
            }
            let encrypted_body = req.to_string();
            hive_proxy_post(&state, "/send", &encrypted_body).await.into_response()
        }
        Err(e) => {
            tracing::warn!("E2E encryption failed for hive_send to {target}: {e}. Sending plaintext.");
            // Fallback to plaintext — graceful degradation during migration
            hive_proxy_post(&state, "/send", &body).await.into_response()
        }
    }
}

async fn hive_direct_messages(State(state): State<WebState>) -> impl IntoResponse {
    // Fetch messages from EC2, then decrypt any encrypted content.
    let url = format!("{}/api/hive/messages", state.api_url);
    let client = match crate::http::build_client(std::time::Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("client error: {e}")).into_response(),
    };

    let resp = match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", state.api_key))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("hive proxy error: {e}")).into_response(),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    // Decrypt message content in-place
    if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(messages) = parsed.get_mut("messages").and_then(|v| v.as_array_mut()) {
            let my_name = state.agent_name.clone();
            for msg in messages.iter_mut() {
                if let (Some(content), Some(from_agent)) = (
                    msg.get("content").and_then(|v| v.as_str()).map(String::from),
                    msg.get("from_agent").and_then(|v| v.as_str()).map(String::from),
                ) {
                    if let Ok(decrypted) = crate::crypto::hive_integration::decrypt_dm_if_encrypted(
                        &state.api_url, &state.api_key, &my_name, &state.agent_id, &from_agent, &content,
                    ).await {
                        msg["content"] = serde_json::Value::String(decrypted);
                    }
                }
            }
            let decrypted_body = parsed.to_string();
            return (status, [(header::CONTENT_TYPE, "application/json")], decrypted_body).into_response();
        }
    }

    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn hive_list_rooms(State(state): State<WebState>) -> impl IntoResponse {
    hive_proxy_get(&state, "/rooms").await.into_response()
}

async fn hive_create_room(
    State(state): State<WebState>,
    body: String,
) -> impl IntoResponse {
    // Create room on EC2, then generate and distribute E2E room key.
    let req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    let url = format!("{}/api/hive/rooms", state.api_url);
    let client = match crate::http::build_client(std::time::Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("client error: {e}")).into_response(),
    };

    // Parse expires_in → expires_at before forwarding to EC2
    let expires_at = req.get("expires_in")
        .and_then(|v| v.as_str())
        .and_then(|exp_in| {
            match crate::crypto::hive_integration::parse_expires_in(exp_in) {
                Ok(ts) => Some(ts),
                Err(e) => {
                    tracing::warn!("Invalid expires_in '{exp_in}': {e}");
                    None
                }
            }
        });

    // Build the body to forward to EC2 (with expires_at if computed)
    let mut ec2_body = req.clone();
    if let Some(ref exp) = expires_at {
        ec2_body["expires_at"] = serde_json::Value::String(exp.clone());
    }
    let ec2_body_str = ec2_body.to_string();

    let ec2_resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", state.api_key))
        .header("Content-Type", "application/json")
        .body(ec2_body_str)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("hive proxy error: {e}")).into_response(),
    };

    let status = StatusCode::from_u16(ec2_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_body = ec2_resp.text().await.unwrap_or_default();

    if status.is_success() {
        // Generate and store room key
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&resp_body) {
            if let Some(room_id) = parsed.get("room_id").and_then(|v| v.as_str()) {
                let members = req.get("members")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                    .unwrap_or_default();

                // KNOWN LIMITATION (v1, Chen Wei review):
                // "Join before key" race — members may receive the room creation
                // notification via SSE before the key_distribution message arrives.
                // If they send a message in that window, it goes plaintext (graceful
                // fallback in hive_room_send). First messages may be unencrypted.
                // v2: queue outbound room messages until room key is available.
                match crate::crypto::hive_integration::generate_and_distribute_room_key(
                    &state.api_url,
                    &state.api_key,
                    room_id,
                    &state.agent_name,
                    &members,
                    expires_at.as_deref(),
                ).await {
                    Ok(key_dist_payload) => {
                        tracing::info!("Generated room key for {room_id}, sending key distribution");
                        // Send the key distribution as a room message so members
                        // can receive their encrypted copy of the room key.
                        let dist_body = serde_json::json!({ "message": key_dist_payload }).to_string();
                        let _ = hive_proxy_post(
                            &state,
                            &format!("/rooms/{room_id}/send"),
                            &dist_body,
                        ).await;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to generate room key for {room_id}: {e}");
                    }
                }
            }
        }
    }

    (status, [(header::CONTENT_TYPE, "application/json")], resp_body).into_response()
}

async fn hive_room_info(
    State(state): State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    hive_proxy_get(&state, &format!("/rooms/{id}")).await.into_response()
}

async fn hive_room_messages(
    State(state): State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    // Fetch room messages from EC2, then decrypt any encrypted content.
    let url = format!("{}/api/hive/rooms/{id}/messages", state.api_url);
    let client = match crate::http::build_client(std::time::Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("client error: {e}")).into_response(),
    };

    let resp = match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", state.api_key))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("hive proxy error: {e}")).into_response(),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    }

    // Decrypt room message content in-place using locally stored room key
    if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(messages) = parsed.get_mut("messages").and_then(|v| v.as_array_mut()) {
            for msg in messages.iter_mut() {
                if let Some(content) = msg.get("content").and_then(|v| v.as_str()).map(String::from) {
                    let decrypted = crate::crypto::hive_integration::decrypt_room_message_if_encrypted(&id, &content);
                    msg["content"] = serde_json::Value::String(decrypted);
                }
            }
            let decrypted_body = parsed.to_string();
            return (status, [(header::CONTENT_TYPE, "application/json")], decrypted_body).into_response();
        }
    }

    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn hive_room_send(
    State(state): State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: String,
) -> impl IntoResponse {
    // E2E encrypt room message with locally stored room key.
    let mut req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    if let Some(plaintext) = req.get("message").and_then(|v| v.as_str()).map(String::from) {
        match crate::crypto::hive_integration::encrypt_room_message(&id, &plaintext) {
            Ok(encrypted_json) => {
                req["message"] = serde_json::Value::String(encrypted_json);
            }
            Err(e) => {
                tracing::warn!("Room encryption failed for {id}: {e}. Sending plaintext.");
                // Fallback to plaintext — no room key yet or key missing
            }
        }
    }

    let encrypted_body = req.to_string();
    hive_proxy_post(&state, &format!("/rooms/{id}/send"), &encrypted_body).await.into_response()
}

async fn hive_room_join(
    State(state): State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: String,
) -> impl IntoResponse {
    hive_proxy_post(&state, &format!("/rooms/{id}/join"), &body).await.into_response()
}

/// POST /api/hive/rooms/:id/destroy-key — destroy the room key and purge all messages.
///
/// Sends a key_destroy message to all room members, deletes local room key,
/// then tells EC2 to purge all message blobs for this room.
async fn hive_room_destroy_key(
    State(state): State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    // Build and send the key_destroy message to the room so other members know to delete
    let destroy_payload = serde_json::json!({
        "message": serde_json::json!({
            "msg_type": "key_destroy",
            "room_id": id,
            "destroyed_by": state.agent_name,
        }).to_string()
    });
    let _ = hive_proxy_post(
        &state,
        &format!("/rooms/{id}/send"),
        &destroy_payload.to_string(),
    ).await;

    // Delete our own local room key immediately
    crate::crypto::hive_integration::delete_room_key(&id);

    // Tell EC2 to purge all message blobs for this room
    hive_proxy_post(&state, &format!("/rooms/{id}/destroy-key"), "{}").await.into_response()
}

// ── Auto-Update ─────────────────────────────────────────────────────

/// GET /api/update-status — check if a new version has been downloaded and staged.
/// Dashboard polls this to show a "Restart to apply" banner.
async fn update_status() -> impl IntoResponse {
    let marker_path = crate::config::Config::config_dir().join("update-ready.json");
    match std::fs::read_to_string(&marker_path) {
        Ok(data) => {
            match serde_json::from_str::<serde_json::Value>(&data) {
                Ok(info) => {
                    let staged_version = info.get("version").and_then(|v| v.as_str()).unwrap_or("");
                    let current = env!("CARGO_PKG_VERSION");
                    if staged_version == current || staged_version.is_empty() {
                        // Already running this version or invalid marker
                        let _ = std::fs::remove_file(&marker_path);
                        Json(serde_json::json!({"ready": false, "current_version": current})).into_response()
                    } else {
                        Json(serde_json::json!({
                            "ready": true,
                            "current_version": current,
                            "staged_version": staged_version,
                        })).into_response()
                    }
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&marker_path);
                    Json(serde_json::json!({"ready": false})).into_response()
                }
            }
        }
        Err(_) => {
            Json(serde_json::json!({
                "ready": false,
                "current_version": env!("CARGO_PKG_VERSION"),
            })).into_response()
        }
    }
}

/// POST /api/restart — restart the daemon by exec()ing into the (updated) binary.
///
/// This replaces the current process with the new binary. The PTY child
/// (Claude Code) is NOT affected — it runs in its own process and the PTY
/// file descriptors are inherited across exec(). However, the SSE connection,
/// web server, and tunnel will briefly disconnect and reconnect.
///
/// In practice, we do a clean shutdown + re-exec rather than a raw exec(),
/// because we need to clean up the Cloudflare tunnel and IPC socket first.
async fn restart_daemon() -> impl IntoResponse {
    tracing::info!("Restart requested via dashboard");

    // Clear the update-ready marker
    let marker_path = crate::config::Config::config_dir().join("update-ready.json");
    let _ = std::fs::remove_file(&marker_path);

    // Spawn a delayed restart so this response can be sent first
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("b3"));
        let args: Vec<String> = std::env::args().collect();

        tracing::info!("exec()ing into {} with args {:?}", exe.display(), &args[1..]);

        // On Unix, exec() replaces the current process image.
        // The new binary starts fresh — PTY child is unaffected because
        // it's a separate process.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(&exe)
                .args(&args[1..])
                .exec();
            // exec() only returns on error
            tracing::error!("exec() failed: {err}");
        }

        #[cfg(not(unix))]
        {
            // On Windows, spawn new process and exit current
            let _ = std::process::Command::new(&exe)
                .args(&args[1..])
                .spawn();
            std::process::exit(0);
        }
    });

    Json(serde_json::json!({"ok": true, "message": "Restarting..."}))
}

// ── TTS Archive Endpoints ────────────────────────────────────────────

/// POST /api/tts-upload — browser uploads stitched WAV after playback.
/// Accepts multipart/form-data (legacy/direct tunnel) or application/json
/// with `audio_b64` (base64-encoded WAV). The JSON path is required on the
/// EC2 proxy path because daemonFetch calls String(body) on the request body,
/// which would stringify FormData to "[object FormData]" and cause a 400.
async fn tts_upload(
    State(state): State<WebState>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let mut audio_bytes: Option<Vec<u8>> = None;
    let mut msg_id = String::new();
    let mut text = String::new();
    let mut original_text = String::new();
    let mut voice = String::new();
    let mut emotion = String::new();
    let mut replying_to = String::new();
    let mut duration_sec: f64 = 0.0;
    let mut voice_job_id = String::new();

    let content_type = request.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("application/json") {
        // JSON path: { audio_b64, msg_id, text, voice, emotion, duration_sec, voice_job_id }
        let body = match axum::body::to_bytes(request.into_body(), 50 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("body read error: {e}")}))).into_response(),
        };
        let v: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("invalid JSON: {e}")}))).into_response(),
        };
        let b64 = v["audio_b64"].as_str().unwrap_or("");
        if b64.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "audio_b64 is required"}))).into_response();
        }
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => audio_bytes = Some(bytes),
            Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("invalid base64: {e}")}))).into_response(),
        }
        msg_id = v["msg_id"].as_str().unwrap_or("").to_string();
        text = v["text"].as_str().unwrap_or("").to_string();
        original_text = v["original_text"].as_str().unwrap_or("").to_string();
        voice = v["voice"].as_str().unwrap_or("").to_string();
        emotion = v["emotion"].as_str().unwrap_or("").to_string();
        replying_to = v["replying_to"].as_str().unwrap_or("").to_string();
        duration_sec = v["duration_sec"].as_f64().unwrap_or(0.0);
        voice_job_id = v["voice_job_id"].as_str().unwrap_or("").to_string();
    } else {
        // Multipart path (direct tunnel, where native fetch handles FormData correctly)
        use axum::extract::FromRequest as _;
        let mut multipart: axum::extract::Multipart =
            match axum::extract::Multipart::from_request(request, &()).await {
                Ok(m) => m,
                Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("multipart error: {e}")}))).into_response(),
            };
        while let Ok(Some(field)) = multipart.next_field().await {
            let name = field.name().unwrap_or("").to_string();
            match name.as_str() {
                "audio" => {
                    if let Ok(bytes) = field.bytes().await {
                        audio_bytes = Some(bytes.to_vec());
                    }
                }
                "msg_id" => { msg_id = field.text().await.unwrap_or_default(); }
                "text" => { text = field.text().await.unwrap_or_default(); }
                "voice" => { voice = field.text().await.unwrap_or_default(); }
                "emotion" => { emotion = field.text().await.unwrap_or_default(); }
                "replying_to" => { replying_to = field.text().await.unwrap_or_default(); }
                "duration_sec" => {
                    duration_sec = field.text().await.unwrap_or_default()
                        .parse().unwrap_or(0.0);
                }
                "voice_job_id" => { voice_job_id = field.text().await.unwrap_or_default(); }
                _ => {}
            }
        }
    }

    let wav = match audio_bytes {
        Some(b) if !b.is_empty() => b,
        _ => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "audio is required"}))).into_response(),
    };
    if msg_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "msg_id is required"}))).into_response();
    }

    // Skip if already archived (e.g. daemon generated it)
    if state.tts_archive.contains(&msg_id).await {
        return Json(serde_json::json!({"status": "already_archived", "msg_id": msg_id})).into_response();
    }

    let entry = b3_common::tts::TtsEntry {
        msg_id: msg_id.clone(),
        text,
        original_text,
        voice,
        emotion,
        replying_to,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        duration_sec,
        size_bytes: wav.len() as u64,
    };

    match state.tts_archive.store(entry, &wav).await {
        Ok(()) => {
            tracing::info!(msg_id = %msg_id, "TTS archived (browser upload)");
            if !voice_job_id.is_empty() {
                report_voice_job(&state.api_url, &state.api_key, &voice_job_id, "archived",
                    Some(serde_json::json!({"audio_output": "browser_archive"})));
            }
            Json(serde_json::json!({"status": "archived", "msg_id": msg_id})).into_response()
        }
        Err(e) => {
            tracing::warn!(msg_id = %msg_id, error = %e, "TTS archive store failed");
            if !voice_job_id.is_empty() {
                report_voice_job(&state.api_url, &state.api_key, &voice_job_id, "failed",
                    Some(serde_json::json!({"error": format!("{e}").chars().take(200).collect::<String>(), "error_stage": "daemon_archive"})));
            }
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response()
        }
    }
}

/// GET /api/tts-history — list archived TTS messages (newest first).
/// Returns full text per entry (no audio data).
async fn tts_history(
    State(state): State<WebState>,
) -> Json<Vec<b3_common::tts::TtsEntry>> {
    Json(state.tts_archive.list().await)
}

/// GET /api/init-bundle — one-shot fetch of everything the browser needs on connect.
/// Replaces 5+ parallel HTTP requests with a single request to reduce tunnel contention.
async fn init_bundle(
    State(state): State<WebState>,
) -> Json<serde_json::Value> {
    // GPU config
    let api_key = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
    let gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
    let configured = !api_key.is_empty() && !gpu_id.is_empty();
    let gpu_config = serde_json::json!({
        "configured": configured,
        "gpu_url": if configured { format!("https://api.runpod.ai/v2/{}/runsync", gpu_id) } else { String::new() },
        "gpu_token": if configured { api_key } else { String::new() },
        "local_gpu_url": std::env::var("LOCAL_GPU_URL").unwrap_or_default(),
        "local_gpu_token": std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default(),
    });

    // TTS archive
    let tts_history = state.tts_archive.list().await;

    // Info archive
    let info_entries = state.info_archive.list().await;

    Json(serde_json::json!({
        "gpu_config": gpu_config,
        "tts_history": tts_history,
        "info_archive": info_entries,
    }))
}

/// GET /api/tts-archive/:msg_id — serve WAV audio file from archive.
async fn tts_archive_audio(
    State(state): State<WebState>,
    axum::extract::Path(msg_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.tts_archive.audio_path(&msg_id).await {
        Some(path) => {
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    (
                        StatusCode::OK,
                        [
                            (header::CONTENT_TYPE, "audio/wav"),
                            (header::CACHE_CONTROL, "max-age=86400"),
                        ],
                        bytes,
                    ).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Router ──────────────────────────────────────────────────────────

// ── Auth Middleware ──────────────────────────────────────────────────

/// Validate Bearer token, ?token= query param, or b3_token cookie.
/// All routes except /health require authentication.
/// When auth succeeds via ?token= query param, sets a b3_token cookie
/// so subsequent browser requests are authenticated.
async fn auth_middleware(
    State(state): State<WebState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Skip auth for health checks, CORS preflight, and internal EC2 proxy route.
    // /ws-proxy has its own auth (proxy_token check) — the global middleware checks
    // against the daemon password (api_key), which is the wrong credential for this route.
    if req.uri().path() == "/health"
        || req.uri().path() == "/ws-proxy"
        || req.method() == axum::http::Method::OPTIONS
    {
        return next.run(req).await;
    }
    if !check_token(&state, &req) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response();
    }
    next.run(req).await
}

fn check_token(state: &WebState, req: &axum::extract::Request) -> bool {
    // 1. Authorization: Bearer {token}
    if let Some(auth) = req.headers().get("authorization") {
        if let Ok(val) = auth.to_str() {
            if let Some(token) = val.strip_prefix("Bearer ") {
                return token == state.api_key;
            }
        }
    }
    // 2. ?token={token} query param (WebSocket can't set headers)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(token) = pair.strip_prefix("token=") {
                return token == state.api_key;
            }
        }
    }
    // 3. b3_token cookie (set by dashboard)
    if let Some(cookies) = req.headers().get("cookie") {
        if let Ok(cookie_str) = cookies.to_str() {
            for cookie in cookie_str.split(';') {
                let cookie = cookie.trim();
                if let Some(token) = cookie.strip_prefix("b3_token=") {
                    return token == state.api_key;
                }
            }
        }
    }
    false
}

/// Build the embedded web server router.
pub fn router(state: WebState) -> Router {
    // CORS: allow dashboard at the configured server domain (not arbitrary origins).
    // allow_credentials required for Set-Cookie on cross-origin /api/auth-cookie.
    let mut origins: Vec<axum::http::HeaderValue> = vec![
        format!("https://{}", state.server_domain).parse().unwrap(),
        format!("https://www.{}", state.server_domain).parse().unwrap(),
    ];
    // During domain migration, also allow the canonical domain if different from configured
    let canonical = b3_common::public_domain();
    if canonical != state.server_domain {
        if let Ok(v) = format!("https://{canonical}").parse() { origins.push(v); }
        if let Ok(v) = format!("https://www.{canonical}").parse() { origins.push(v); }
    }
    // Always allow babel3.com, babel3.ai (legacy), and hey-code.ai (dev) —
    // browsers on any domain may reach daemons registered on another.
    for domain in &["babel3.com", "babel3.ai", "hey-code.ai"] {
        if *domain != state.server_domain && *domain != canonical.as_str() {
            if let Ok(v) = format!("https://{domain}").parse() { origins.push(v); }
        }
    }
    // Allow localhost origins for dev servers
    if state.server_domain == "localhost" || state.api_url.contains("localhost") {
        if let Ok(v) = format!("http://{}", state.server_domain).parse() { origins.push(v); }
        for port in &["3000", "3100", "8080"] {
            if let Ok(v) = format!("http://localhost:{port}").parse() { origins.push(v); }
        }
    }
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .allow_credentials(true);

    // Development mode: serve browser dashboard from local directory.
    // Normal mode: serve user's site from ~/public/, fall back to placeholder.
    let site_service = if let Some(ref browser_dir) = state.browser_dir {
        tracing::info!("Development mode: serving browser from {}", browser_dir.display());
        // Serve static files from the browser_dir directly.
        // Expected structure: <browser_dir>/static/open/css/, <browser_dir>/static/open/js/, etc.
        // The dashboard HTML template should be at <browser_dir>/templates/dashboard-shell.html
        tower_http::services::ServeDir::new(browser_dir)
            .fallback(get(dev_dashboard).with_state(state.clone()))
    } else {
        let public_dir = dirs::home_dir()
            .unwrap_or_default()
            .join("public");
        tower_http::services::ServeDir::new(&public_dir)
            .fallback(get(site_placeholder).with_state(state.clone()))
    };

    // All routes require auth (Bearer token, ?token= query param, or b3_token cookie).
    // /health is exempted inside the middleware itself.
    Router::new()
        .route("/health", get(health))
        .route("/api/diagnostics", get(diagnostics))
        .route("/api/gpu-config", get(gpu_config))
        .route("/api/auth-cookie", get(auth_cookie))
        .route("/ws", get(terminal_ws))
        .route("/ws-proxy", get(proxy_ws))
        .route("/api/session", get(session_read))
        .route("/api/inject", axum::routing::post(inject))
        .route("/api/tts", axum::routing::post(tts))
        .route("/api/settings/refresh", axum::routing::post(refresh_settings))
        .route("/api/audio-upload", axum::routing::post(audio_upload))
        .route("/api/transcription", axum::routing::post(transcription_callback))
        .route("/api/channel-events", get(channel_events_sse))
        .route("/api/tts-audio", axum::routing::post(tts_audio_callback))
        .route("/api/voice-event", axum::routing::post(voice_event))
        .route("/api/voice-report", axum::routing::post(voice_report))
        .route("/api/tts-upload", axum::routing::post(tts_upload))
        .route("/api/tts-history", get(tts_history))
        .route("/api/init-bundle", get(init_bundle))
        .route("/api/tts-archive/:msg_id", get(tts_archive_audio))
        .route("/api/file-upload", axum::routing::post(file_upload))
        .route("/api/file-upload-json", axum::routing::post(file_upload_json))
        .route("/api/lost-audio", axum::routing::post(lost_audio))
        .route("/api/info", get(info_list))
        .route("/api/info/:index", get(info_get))
        .route("/api/browser-console", get(browser_console))
        .route("/api/browser-eval", axum::routing::post(browser_eval))
        .route("/api/files", get(list_files))
        .route("/api/file", get(read_file).put(write_file))
        .route("/api/file-raw", get(read_file_raw))
        .route("/api/file-rename", axum::routing::post(file_rename))
        .route("/api/file-copy", axum::routing::post(file_copy))
        .route("/api/file-delete", axum::routing::post(file_delete))
        .route("/api/hive/agents", get(hive_agents))
        .route("/api/hive/send", axum::routing::post(hive_send))
        .route("/api/hive/messages", get(hive_direct_messages))
        .route("/api/hive/rooms", get(hive_list_rooms).post(hive_create_room))
        .route("/api/hive/rooms/:id", get(hive_room_info))
        .route("/api/hive/rooms/:id/messages", get(hive_room_messages))
        .route("/api/hive/rooms/:id/send", axum::routing::post(hive_room_send))
        .route("/api/hive/rooms/:id/join", axum::routing::post(hive_room_join))
        .route("/api/hive/rooms/:id/destroy-key", axum::routing::post(hive_room_destroy_key))
        .route("/api/update-status", get(update_status))
        .route("/api/restart", axum::routing::post(restart_daemon))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth_middleware))
        .fallback_service(site_service)
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024)) // 50 MB
        .layer(cors)
        .with_state(state)
}

// ── Auth Cookie ─────────────────────────────────────────────────────

/// Set a b3_token cookie on the daemon's domain.
/// Called by the dashboard JS via daemonFetch() after connecting.
/// This allows sub-pages on the same domain to authenticate via cookie.
async fn auth_cookie(
    State(state): State<WebState>,
) -> impl IntoResponse {
    // SameSite=None required because the cookie is set via cross-origin fetch
    // (dashboard calls {agent}.{domain}/api/auth-cookie).
    // Secure required with SameSite=None. HttpOnly prevents JS access.
    let cookie = format!(
        "b3_token={}; Path=/; HttpOnly; SameSite=None; Secure; Max-Age=86400",
        state.api_key
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"status": "ok"})),
    )
}


/// Load env vars from ~/.b3/runpod.env if not already set.
/// Mirrors runpod_client.py's fallback-to-file pattern.
fn load_env_file() {
    let env_file = dirs::home_dir()
        .unwrap_or_default()
        .join(".b3")
        .join("runpod.env");

    if !env_file.exists() {
        return;
    }

    if let Ok(contents) = std::fs::read_to_string(&env_file) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if std::env::var(key).is_err() {
                    std::env::set_var(key, value);
                    tracing::info!("Loaded {key} from {}", env_file.display());
                }
            }
        }
    }
}

/// Start the embedded web server on the given port.
/// Start the embedded web server. Binds to the given port (0 = auto-assign).
/// Returns the actual port via the oneshot sender so the daemon can advertise it.
pub async fn start(state: WebState, port: u16, port_tx: tokio::sync::oneshot::Sender<u16>) -> anyhow::Result<()> {
    // Load GPU credentials from ~/.b3/runpod.env if env vars not set
    load_env_file();

    // Spawn background cleanup task for TTS audio files
    tokio::spawn(async {
        let tts_dir = &std::env::temp_dir().join("b3-tts");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
            if let Ok(mut entries) = tokio::fs::read_dir(tts_dir).await {
                let cutoff = std::time::SystemTime::now()
                    - std::time::Duration::from_secs(60 * 60);
                let mut removed = 0u32;
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if let Ok(meta) = entry.metadata().await {
                        if let Ok(modified) = meta.modified() {
                            if modified < cutoff {
                                let _ = tokio::fs::remove_file(entry.path()).await;
                                removed += 1;
                            }
                        }
                    }
                }
                if removed > 0 {
                    tracing::info!("TTS cleanup: removed {removed} old audio files");
                }
            }
        }
    });

    // Spawn background cleanup task for expired room keys (every 60s)
    tokio::spawn(async {
        // Run once at startup
        let n = crate::crypto::hive_integration::cleanup_expired_room_keys();
        if n > 0 {
            tracing::info!("Startup: cleaned up {n} expired room key(s)");
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let n = crate::crypto::hive_integration::cleanup_expired_room_keys();
            if n > 0 {
                tracing::info!("Periodic: cleaned up {n} expired room key(s)");
            }
        }
    });

    let app = router(state);

    // Always bind to an OS-assigned port. The daemon reports the actual port
    // to the server, which updates the Cloudflare tunnel ingress to match.
    // One code path whether there's one agent or ten on this machine.
    let _ = port; // kept in signature for compatibility
    let listener = tokio::net::TcpListener::bind(
        std::net::SocketAddr::from(([0, 0, 0, 0], 0u16)),
    ).await?;
    let actual_port = listener.local_addr()?.port();
    tracing::info!("Embedded web server listening on 0.0.0.0:{actual_port}");

    // Tell the daemon what port we got
    let _ = port_tx.send(actual_port);

    axum::serve(listener, app).await?;
    Ok(())
}

// ── Site Placeholder HTML ───────────────────────────────────────────

const SITE_PLACEHOLDER_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{AGENT_NAME}}.{{SERVER_DOMAIN}}</title>
<style>
:root { --bg: #0d1117; --surface: #161b22; --border: #30363d; --text: #c9d1d9; --dim: #8b949e; --accent: #58a6ff; }
* { margin: 0; padding: 0; box-sizing: border-box; }
body { background: var(--bg); color: var(--text); font-family: system-ui, -apple-system, sans-serif; min-height: 100vh; display: flex; align-items: center; justify-content: center; }
.card { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 2.5rem; max-width: 520px; width: 90%; text-align: center; }
h1 { font-size: 1.5rem; margin-bottom: 0.5rem; color: #fff; }
.subtitle { color: var(--dim); font-size: 0.95rem; margin-bottom: 1.5rem; }
.instructions { text-align: left; background: var(--bg); border: 1px solid var(--border); border-radius: 8px; padding: 1.25rem; margin-bottom: 1.5rem; }
.instructions h2 { font-size: 0.85rem; color: var(--dim); text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 0.75rem; }
.instructions p { font-size: 0.9rem; line-height: 1.6; margin-bottom: 0.5rem; }
code { background: rgba(88,166,255,0.1); color: var(--accent); padding: 0.15em 0.4em; border-radius: 4px; font-size: 0.85em; }
.dashboard-link { display: inline-block; margin-top: 0.5rem; color: var(--accent); text-decoration: none; font-size: 0.9rem; }
.dashboard-link:hover { text-decoration: underline; }
.dot { display: inline-block; width: 8px; height: 8px; background: #3fb950; border-radius: 50%; margin-right: 6px; vertical-align: middle; }
</style>
</head>
<body>
<div class="card">
    <h1>{{AGENT_NAME}}.{{SERVER_DOMAIN}}</h1>
    <p class="subtitle"><span class="dot"></span>Agent is running</p>
    <div class="instructions">
        <h2>Deploy your site here</h2>
        <p>This address is yours. Tell your agent what to build:</p>
        <p><code>build me a landing page and deploy it to my site</code></p>
        <p><code>create a portfolio site with my projects</code></p>
        <p>Your agent will generate the site and serve it at this URL.</p>
    </div>
    <a class="dashboard-link" href="{{API_URL}}/a/{{AGENT_NAME}}">Open dashboard →</a>
</div>
</body>
</html>"##;

// Legacy dashboard HTML removed — now served by EC2 at /a/{name}.
// See git history for the original 1200-line embedded dashboard.

// ── Development Mode Dashboard ──────────────────────────────────────

/// Serve the development dashboard from a local browser_dir.
/// Reads templates/dashboard-shell.html from the directory and injects
/// config vars so the dashboard works against the local daemon.
async fn dev_dashboard(
    State(state): State<WebState>,
) -> impl IntoResponse {
    let browser_dir = match &state.browser_dir {
        Some(dir) => dir,
        None => return (StatusCode::NOT_FOUND, "Development mode not active").into_response(),
    };

    // Try to read the dashboard template from the browser dir
    let template_path = browser_dir.join("templates").join("dashboard-shell.html");
    let html = match std::fs::read_to_string(&template_path) {
        Ok(content) => content,
        Err(_) => {
            // No template file — serve a helpful error page
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(
                    r#"<!DOCTYPE html><html><body style="background:#0d1117;color:#c9d1d9;font-family:system-ui;padding:2rem">
                    <h1>Development Mode</h1>
                    <p>Expected dashboard template at:</p>
                    <code>{}</code>
                    <p style="margin-top:1rem;color:#8b949e">
                    Point <code>--browser-dir</code> to your b3-server crate root, e.g.:<br>
                    <code>b3 start --browser-dir ./crates/b3-server</code>
                    </p></body></html>"#,
                    template_path.display()
                ),
            ).into_response();
        }
    };

    // Inject daemon config — same template vars EC2 uses (pages.rs agent_dashboard).
    // DAEMON_TOKEN = EC2 API key, used by the browser to authenticate WS/API calls.
    // Same credential EC2 injects — no additional exposure in dev mode.
    let version = env!("CARGO_PKG_VERSION");
    let html = html
        .replace("{{AGENT_ID}}", &state.agent_id)
        .replace("{{AGENT_NAME}}", &state.agent_name)
        .replace("{{AGENT_EMAIL}}", &state.agent_email)
        .replace("{{DAEMON_TOKEN}}", &state.api_key)
        .replace("{{HOSTED_SESSION_ID}}", "")
        .replace("{{DOMAIN}}", &state.server_domain)
        .replace("{{VERSION}}", version)
        // Dev mode: strip drawer placeholder (open-source builds don't have it)
        .replace("<!--DRAWER_HTML-->", "<!-- drawer: not available in development mode -->");

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
        ],
        html,
    ).into_response()
}
