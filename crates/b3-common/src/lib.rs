//! Shared types between b3 CLI and server.
//!
//! API request/response structs are defined once here and used
//! by both the client (CLI) and server. No drift, no duplication.

pub mod backoff;
pub mod errors;
pub mod health;
pub mod limits;
pub mod tts;

use serde::{Deserialize, Serialize};

// ── Domain Configuration ─────────────────────────────────────────────

/// Public URL for the server (e.g., "https://babel3.com").
/// Reads from B3_PUBLIC_URL or HEYCODE_PUBLIC_URL env var.
/// Defaults to "https://babel3.com".
pub fn public_url() -> String {
    std::env::var("B3_PUBLIC_URL")
        .or_else(|_| std::env::var("HEYCODE_PUBLIC_URL"))
        .unwrap_or_else(|_| "https://babel3.com".to_string())
}

/// Public domain for the server (e.g., "babel3.com").
/// Derived from HEYCODE_PUBLIC_URL.
pub fn public_domain() -> String {
    let url = public_url();
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .and_then(|host| host.split(':').next())
        .unwrap_or("localhost")
        .to_string()
}

/// Peer server URLs for multi-server registration.
/// Reads from PEER_SERVERS env var: comma-separated URLs, e.g.
/// "https://b.babel3.com,https://c.babel3.com".
/// Returns empty Vec on single-server deployments.
pub fn peer_servers() -> Vec<String> {
    std::env::var("PEER_SERVERS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// POST /api/agents/register request
#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub token: String,
    pub hostname: String,
    pub platform: String,
    pub requested_name: Option<String>,
}

/// POST /api/agents/register response
#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub agent_id: String,
    pub agent_email: String,
    pub wg_private_key: String,
    pub wg_address: String,
    pub relay_endpoint: String,
    pub relay_public_key: String,
    pub api_key: String,
    pub web_url: String,
    /// Peer server URLs for multi-server registration (e.g. ["https://b.babel3.com"]).
    /// Empty on single-server deployments. Daemon stores these in config.json.
    #[serde(default)]
    pub servers: Vec<String>,
}

/// GET /api/agents/:id/config-fields response — authoritative fields from server.
/// Daemon calls this on startup to refresh post-rename config drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigFields {
    pub agent_email: String,
    pub api_url: String,
    pub web_url: String,
    /// Current peer server URLs — refreshed on every daemon start.
    /// Empty on single-server deployments.
    #[serde(default)]
    pub servers: Vec<String>,
}

/// POST /api/session-push request — offset-based delta protocol.
///
/// The pusher sends only new bytes since the last acknowledged offset.
/// `offset == 0` means "full replace" (first push, resync, or force push).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPush {
    pub agent_id: String,
    pub offset: u64,       // byte offset into raw buffer where delta starts
    pub delta: String,     // base64-encoded new bytes only
    pub total_len: u64,    // expected total raw byte length after appending
    pub rows: u16,
    pub cols: u16,
}

/// Response from POST /api/session-push — server acknowledges received state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPushAck {
    pub acked_offset: u64, // server's raw buffer length after this push
}

/// Errors from the push endpoint.
#[derive(Debug)]
pub enum PushError {
    /// Client's offset doesn't match server's buffer length.
    OffsetMismatch { expected: u64 },
    /// Invalid base64 in delta field.
    InvalidBase64,
    /// Delta exceeds MAX_DELTA_BYTES.
    DeltaTooLarge,
}

/// SSE event types from GET /api/agents/{id}/events
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "transcription")]
    Transcription { text: String },
    #[serde(rename = "hive")]
    Hive { sender: String, text: String },
    /// Raw PTY input — keyboard bytes written directly to stdin without modification.
    #[serde(rename = "pty_input")]
    PtyInput { data: String },
    /// TTS output — text to be spoken aloud by the browser via Web Speech API.
    #[serde(rename = "tts")]
    Tts {
        text: String,
        msg_id: String,
        #[serde(default)]
        voice: String,
    },
    /// TTS generation started — tells the browser audio is being generated.
    #[serde(rename = "tts_generating")]
    TtsGenerating {
        msg_id: String,
        chunk: usize,
        total: usize,
    },
    /// TTS audio chunk ready — browser should fetch and play this URL.
    /// Used by local Chatterbox path (dev mode).
    #[serde(rename = "tts_audio")]
    TtsAudio {
        msg_id: String,
        chunk: usize,
        total: usize,
        url: String,
    },
    /// TTS audio data — GPU delivered audio directly via callback.
    /// Contains base64-encoded WAV. Browser decodes → Blob → Audio.
    /// No disk round-trip. Daemon is a thin pipe from GPU to browsers.
    #[serde(rename = "tts_audio_data")]
    TtsAudioData {
        msg_id: String,
        chunk: usize,
        total: usize,
        audio_b64: String,
        duration_sec: f64,
        generation_sec: f64,
    },
    /// TTS streaming command — browser handles GPU routing (local-first + RunPod).
    /// Single event for entire text. Browser calls gpuRun(), polls /stream/{jobId},
    /// plays via GaplessStreamPlayer. GPU worker splits text and yields audio chunks.
    #[serde(rename = "tts_stream")]
    TtsStream {
        msg_id: String,
        text: String,
        /// Original text before TTS cleaning — used for matching in browser overlay.
        #[serde(default)]
        original_text: String,
        voice: String,
        #[serde(default)]
        emotion: String,
        #[serde(default)]
        replying_to: String,
        #[serde(default)]
        split_chars: String,
        #[serde(default)]
        min_chunk_chars: usize,
        #[serde(default)]
        conditionals_b64: String,
    },
    /// Notification that session terminal data was updated.
    /// Dashboard uses this to fetch fresh data instead of polling.
    #[serde(rename = "session_updated")]
    SessionUpdated { size: usize },
    /// LED chromatophore — set terminal glow based on emotion text.
    #[serde(rename = "led")]
    Led { emotion: String },
    /// PTY resize — browser reports its terminal dimensions.
    /// Daemon tracks all connected clients and resizes PTY to the minimum.
    #[serde(rename = "pty_resize")]
    PtyResize { rows: u16, cols: u16 },
    /// Hive conversation room message — multi-agent broadcast within a room.
    #[serde(rename = "hive_room")]
    HiveRoom {
        sender: String,
        room_id: String,
        room_topic: String,
        text: String,
    },
    /// Server-pushed update available — daemon should download and prepare.
    #[serde(rename = "update_available")]
    UpdateAvailable {
        version: String,
        url: String,
        sha256: String,
    },
    /// Daemon notification — displayed as a toast/banner in the browser dashboard.
    #[serde(rename = "notification")]
    Notification {
        title: String,
        body: String,
        /// "info", "success", "warning", "action"
        level: String,
        /// Optional action button (e.g., {"label": "Restart", "endpoint": "/api/restart"})
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<serde_json::Value>,
    },
    /// Browser eval — execute JS in connected browser and return result.
    /// If session_id is Some, only that session executes. Otherwise all sessions race.
    #[serde(rename = "browser_eval")]
    BrowserEval {
        eval_id: String,
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<u64>,
    },

    // ── WebRTC signaling (relayed via SSE) ─────────────────────────

    /// WebRTC SDP offer — browser wants to establish a data channel.
    /// EC2 relays this to the daemon (or GPU worker via daemon).
    #[serde(rename = "webrtc_offer")]
    WebRtcOffer {
        session_id: String,
        sdp: String,
        /// "daemon" or "gpu-worker"
        target: String,
        /// URL of the server that received the offer (for posting answer back)
        #[serde(default)]
        origin_url: String,
        /// Browser's session_id (for RTC↔session association)
        #[serde(default, alias = "client_id")]
        browser_session_id: Option<u64>,
    },

    /// WebRTC SDP answer — daemon/GPU responds to browser's offer.
    #[serde(rename = "webrtc_answer")]
    WebRtcAnswer {
        session_id: String,
        sdp: String,
    },

    /// WebRTC ICE candidate — trickle ICE in both directions.
    #[serde(rename = "webrtc_ice")]
    WebRtcIce {
        session_id: String,
        candidate: String,
        mid: String,
        /// "daemon" or "gpu-worker"
        target: String,
    },
}

/// POST /api/agents/{id}/audio-upload response
#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptionResult {
    pub transcription: String,
    pub language: String,
    pub confidence: f32,
    pub duration_sec: f32,
    pub injected: bool,
}

/// Agent status info
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent_id: String,
    pub agent_email: String,
    pub status: String,
    pub last_seen: Option<String>,
    pub wg_address: String,
}

/// GET /api/agents — agent list entry (for hive discovery)
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentListEntry {
    pub agent_id: String,
    pub name: String,
    pub email: String,
    pub status: String,
    pub wg_address: String,
}

// ── Hive Types ──────────────────────────────────────────────────────

/// POST /api/hive/send request
#[derive(Debug, Serialize, Deserialize)]
pub struct HiveSendRequest {
    pub target: String,  // agent name or id
    pub message: String,
    /// Forward secrecy: one-time key, deleted after sending. Server deletes blob after delivery.
    #[serde(default)]
    pub ephemeral: bool,
    /// Timelock vault: ISO 8601 timestamp. Server holds message + unlock_key until this time.
    #[serde(default)]
    pub deliver_at: Option<String>,
    /// Timelock vault: base64-encoded symmetric key. Server stores this and delivers it with
    /// the message after deliver_at. Sender discards key immediately after sending.
    #[serde(default)]
    pub unlock_key: Option<String>,
}

/// POST /api/hive/rooms request — create a new conversation room
#[derive(Debug, Serialize, Deserialize)]
pub struct HiveCreateRoomRequest {
    pub topic: String,
    #[serde(default)]
    pub members: Vec<String>, // agent names to add (sender auto-included)
    /// RFC 3339 timestamp for room key expiration (mandatory for encrypted rooms).
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// POST /api/hive/rooms/:id/send request
#[derive(Debug, Serialize, Deserialize)]
pub struct HiveRoomSendRequest {
    pub message: String,
}

/// Hive message (returned in message history)
#[derive(Debug, Serialize, Deserialize)]
pub struct HiveMessage {
    pub id: i64,
    pub from_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_agent: Option<String>,
    pub content: String,
    pub msg_type: String,
    pub created_at: String,
}

/// Hive room summary (returned in room listing)
#[derive(Debug, Serialize, Deserialize)]
pub struct HiveRoom {
    pub id: String,
    pub topic: String,
    pub members: Vec<String>, // agent names
    pub created_at: String,
    pub updated_at: String,
}

/// GPU backend health
#[derive(Debug, Serialize, Deserialize)]
pub struct GpuHealth {
    pub service: String,
    pub active_backend: String,
    pub backends: Vec<BackendStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackendStatus {
    pub name: String,
    pub healthy: bool,
    pub latency_ms: Option<u64>,
    pub cost_per_sec: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_event_transcription_serialize() {
        let event = AgentEvent::Transcription {
            text: "hello world".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"transcription""#));
        assert!(json.contains(r#""text":"hello world""#));
    }

    #[test]
    fn test_agent_event_transcription_deserialize() {
        let json = r#"{"type":"transcription","text":"hello world"}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        match event {
            AgentEvent::Transcription { text } => assert_eq!(text, "hello world"),
            _ => panic!("Expected Transcription variant"),
        }
    }

    #[test]
    fn test_agent_event_hive_serialize() {
        let event = AgentEvent::Hive {
            sender: "cloister".to_string(),
            text: "run tests".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"hive""#));
        assert!(json.contains(r#""sender":"cloister""#));
        assert!(json.contains(r#""text":"run tests""#));
    }

    #[test]
    fn test_agent_event_hive_deserialize() {
        let json = r#"{"type":"hive","sender":"cloister","text":"run tests"}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        match event {
            AgentEvent::Hive { sender, text } => {
                assert_eq!(sender, "cloister");
                assert_eq!(text, "run tests");
            }
            _ => panic!("Expected Hive variant"),
        }
    }

    #[test]
    fn test_agent_event_roundtrip() {
        let original = AgentEvent::Transcription {
            text: "test roundtrip".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&deserialized).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn test_session_push_serialize() {
        let push = SessionPush {
            agent_id: "hc-abc123".to_string(),
            offset: 100,
            delta: "SGVsbG8=".to_string(), // base64 of "Hello"
            total_len: 105,
            rows: 24,
            cols: 80,
        };
        let json = serde_json::to_string(&push).unwrap();
        assert!(json.contains(r#""agent_id":"hc-abc123""#));
        assert!(json.contains(r#""offset":100"#));
        assert!(json.contains(r#""delta":"SGVsbG8=""#));
        assert!(json.contains(r#""total_len":105"#));
        assert!(json.contains(r#""rows":24"#));
        assert!(json.contains(r#""cols":80"#));
    }

    #[test]
    fn test_session_push_ack_roundtrip() {
        let ack = SessionPushAck { acked_offset: 42 };
        let json = serde_json::to_string(&ack).unwrap();
        let parsed: SessionPushAck = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.acked_offset, 42);
    }

    #[test]
    fn test_agent_event_unknown_type_fails() {
        let json = r#"{"type":"unknown","data":"stuff"}"#;
        let result = serde_json::from_str::<AgentEvent>(json);
        assert!(result.is_err(), "Unknown event type should fail to deserialize");
    }

    #[test]
    fn test_agent_event_hive_room_serialize() {
        let event = AgentEvent::HiveRoom {
            sender: "agent-a".to_string(),
            room_id: "room-123".to_string(),
            room_topic: "brainstorm".to_string(),
            text: "great idea".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"hive_room""#));
        assert!(json.contains(r#""sender":"agent-a""#));
        assert!(json.contains(r#""room_id":"room-123""#));
        assert!(json.contains(r#""room_topic":"brainstorm""#));
        assert!(json.contains(r#""text":"great idea""#));
    }

    #[test]
    fn test_agent_event_hive_room_deserialize() {
        let json = r#"{"type":"hive_room","sender":"agent-a","room_id":"r1","room_topic":"test","text":"hello"}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        match event {
            AgentEvent::HiveRoom { sender, room_id, room_topic, text } => {
                assert_eq!(sender, "agent-a");
                assert_eq!(room_id, "r1");
                assert_eq!(room_topic, "test");
                assert_eq!(text, "hello");
            }
            _ => panic!("Expected HiveRoom variant"),
        }
    }

    #[test]
    fn test_hive_send_request() {
        let req = HiveSendRequest {
            target: "agent-b".to_string(),
            message: "hello".to_string(),
            ephemeral: false,
            deliver_at: None,
            unlock_key: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HiveSendRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.target, "agent-b");
        assert_eq!(parsed.message, "hello");
    }
}
