//! Unified b3 MCP server — all 24 tools in one stdio process.
//!
//! Spawned by Claude Code as `b3 mcp` (or legacy `b3 mcp voice`).
//! Communicates over stdio using MCP JSON-RPC protocol.
//!
//! Voice/hive/browser tools (19): say, voice_status, voice_health,
//!   voice_logs, voice_share_info, animation_add, email_draft,
//!   voice_enroll_speakers, browser_console, browser_eval,
//!   hive_send, hive_status, hive_room_send, hive_room_messages,
//!   hive_room_create, hive_room_destroy,
//!   hive_room_list, hive_messages, restart_session.
//!
//! Session-memory tools (5): list_sessions, resurface,
//!   find_active_session, get_session_info, add_session_dir.
//!   Dispatched to session_memory module handlers.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Port discovery
// ---------------------------------------------------------------------------

/// Get the dashboard port. Checks (in order):
/// 1. B3_WEB_PORT env var (set by daemon for child processes)
/// 2. Agent-scoped port file: ~/.b3/agents/<slug>/dashboard.port
/// 3. Falls back to 3100
fn dashboard_port() -> String {
    if let Ok(port) = std::env::var("B3_WEB_PORT") {
        return port;
    }
    if let Ok(config) = crate::config::Config::load() {
        if let Some(port) = config.dashboard_port() {
            return port.to_string();
        }
    }
    "3100".to_string()
}

/// Get the API key for authenticating with the daemon's web server.
/// Reads from the resolved config dir (inherits B3_CONFIG_DIR from daemon env).
fn daemon_api_key() -> String {
    let config_path = crate::config::Config::config_path();
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = serde_json::from_str::<Value>(&content) {
            if let Some(key) = config.get("api_key").and_then(|v| v.as_str()) {
                return key.to_string();
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool_definitions() -> Value {
    json!([
        {
            "name": "say",
            "description": "Speak text aloud to the user via Chatterbox TTS.\n\nText is cleaned (markdown stripped, abbreviations expanded), chunked at sentence boundaries, and synthesized via RunPod Chatterbox. Audio WAV files are served to connected browsers for playback via <audio> elements.\n\nThe voice is selected by the user in their browser settings panel — agents do not control which voice is used.\n\nUsage: say(text=\"your response\", emotion=\"warm welcome\", replying_to=\"user's transcription\")\n\nReturns immediately — audio generates asynchronously and streams to browsers as chunks complete.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to speak (can be any length)"
                    },
                    "emotion": {
                        "type": "string",
                        "description": "Emotional state for LED chromatophore (e.g., \"warm joy\", \"focused determination\")"
                    },
                    "replying_to": {
                        "type": "string",
                        "description": "What you are responding to — the user's transcription, a summary of the context, or empty string if unprompted"
                    }
                },
                "required": ["text", "emotion", "replying_to"]
            }
        },
        {
            "name": "voice_status",
            "description": "Check voice pipeline status. Returns component health for TTS, transcription, and delivery systems.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "voice_health",
            "description": "Deep health check of the voice pipeline. Curls endpoints directly to verify actual reachability, not just systemd status.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "voice_logs",
            "description": "Get recent voice pipeline logs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lines": {
                        "type": "integer",
                        "description": "Number of log lines to return (default: 50)",
                        "default": 50
                    }
                },
                "required": []
            }
        },
        {
            "name": "voice_share_info",
            "description": "Push HTML content to the voice app's Info tab. Useful for sharing formatted data, links, or status with the user.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "html": {
                        "type": "string",
                        "description": "HTML content to display in the Info tab"
                    }
                },
                "required": ["html"]
            }
        },
        {
            "name": "animation_add",
            "description": "Register a custom LED animation. Provide a name, description (used for emotion matching), and the file path containing the animation code.\n\nThe animation function must follow the signature: function(strip, delta)\n  - strip.baseColor: [r, g, b]\n  - strip.palette: [[r,g,b], ...]\n  - strip.patternState: {phase, offset, sparkles, drops}\n  - strip.numLEDs: 300\n  - strip.setPixel(index, r, g, b)\n  - strip.setAllPixels(r, g, b)\n  - delta: ms since last frame (~33ms)\n\nThe server embeds the description and stores the animation per-user. During emotion matching, custom animations compete alongside built-in defaults via cosine similarity.\n\nExample: animation_add(name=\"rainfall\", description=\"gentle blue drops falling like rain on a window\", file_path=\"/home/user/animations/rainfall.js\")",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Animation name (e.g., 'rainfall', 'heartbeat'). Max 64 chars."
                    },
                    "description": {
                        "type": "string",
                        "description": "Natural language description of the animation's mood/feel. This is embedded and used for emotion matching."
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Path to the JS file containing the animation function body. The file contents are read and submitted as pattern_code."
                    }
                },
                "required": ["name", "description", "file_path"]
            }
        },
        {
            "name": "email_draft",
            "description": "Submit an outbound email draft for owner review.\n\nThe email is NOT sent immediately — it goes into a review queue. The account owner reviews the draft, can edit it, and approves or rejects before it's sent via the agent's babel3.com email address.\n\nUse this to respond to emails, reach out to people, or communicate as your agent identity.\n\nExample: email_draft(to=\"user@example.com\", subject=\"Re: Your question\", body=\"Thanks for reaching out...\")",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient email address"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Email subject line"
                    },
                    "body": {
                        "type": "string",
                        "description": "Email body text (plain text or HTML)"
                    },
                    "in_reply_to": {
                        "type": "integer",
                        "description": "Optional: inbox email ID this is a reply to"
                    }
                },
                "required": ["to", "subject", "body"]
            }
        },
        {
            "name": "voice_enroll_speakers",
            "description": "Send speaker embeddings to the RunPod GPU worker for diarization.\n\nReads .npy files from a directory and uploads them to the GPU worker's in-memory store, keyed by agent_id. Each daemon/agent maintains its own speaker set.\n\nCall this once when the agent starts, or after re-enrolling speakers.\n\nRequires RUNPOD_GPU_ID and RUNPOD_API_KEY env vars to be set.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "embeddings_dir": {
                        "type": "string",
                        "description": "Path to directory containing .npy speaker embedding files (e.g., /path/to/family-embeddings/)"
                    }
                },
                "required": ["embeddings_dir"]
            }
        },
        {
            "name": "browser_console",
            "description": "Read the browser's dev console log from the connected Babel3 dashboard.\n\nReturns recent log entries from the browser, including STT progress, energy gate filters, TTS streaming status, garbage filter results, and all debug output.\n\nUse this to inspect what's happening in the browser without asking the user to paste console output. Supports filtering by substring.\n\nShort results (≤2000 chars) return inline. Long results are written to a temp file and the path is returned — use Read, Grep, or Bash to analyze.\n\nExamples:\n  browser_console()                                              -- last 100 entries\n  browser_console(last=50)                                       -- last 50 entries\n  browser_console(filter=\"energy\")                               -- only entries containing 'energy'\n  browser_console(filter=\"Trim\", secondary_filter=\"2b8ec294\")    -- trim data for a specific msg_id",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "last": {
                        "type": "integer",
                        "description": "Number of recent entries to return (default: 100)",
                        "default": 100
                    },
                    "filter": {
                        "type": "string",
                        "description": "Optional substring filter (case-insensitive)"
                    },
                    "secondary_filter": {
                        "type": "string",
                        "description": "Optional second substring filter (AND logic, case-insensitive). Both filter and secondary_filter must match."
                    }
                },
                "required": []
            }
        },
        {
            "name": "browser_eval",
            "description": "Execute JavaScript in the connected browser dashboard and return the result.\n\nRuns eval() in the browser context. Can read/write any JS variable, call functions, inspect DOM state, or modify runtime configuration.\n\nExamples:\n  browser_eval(code=\"ENERGY_GATE_THRESHOLD\")              -- read current threshold\n  browser_eval(code=\"ENERGY_GATE_THRESHOLD = 15\")          -- change threshold live\n  browser_eval(code=\"SPEECH_THRESHOLD\")                     -- read speech detection threshold\n  browser_eval(code=\"_garbageEntries.length\")               -- count garbage entries\n  browser_eval(code=\"isListening\")                          -- check if mic is active\n  browser_eval(code=\"gaplessPlayer.isActive()\")             -- check if TTS is playing\n  browser_eval(code=\"document.getElementById('text-input').placeholder\")  -- read UI state\n\nReturns the stringified result. Errors return 'ERROR: ...'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "JavaScript code to execute in the browser"
                    },
                    "session_id": {
                        "type": "integer",
                        "description": "Target a specific browser session by ID. Get IDs from voice_status() browsers list. If omitted, all sessions race and first response wins."
                    }
                },
                "required": ["code"]
            }
        },
        {
            "name": "hive_send",
            "description": "Send a message to another agent belonging to the same user.\n\nDelivers the message via SSE to the target agent's PTY, where it appears as:\n  [HIVE from=<your-name>] <message>\n\nThe target agent's Claude session will see and respond to the message.\n\nSet ephemeral=true for forward secrecy: the message is encrypted with a one-time key that the sender discards immediately. Neither sender nor server can decrypt after sending. The server deletes the ciphertext after confirmed delivery.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Target agent name or ID"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message text to send"
                    },
                    "ephemeral": {
                        "type": "boolean",
                        "description": "Forward secrecy mode. One-time encryption key, deleted after sending. Server deletes ciphertext after delivery."
                    },
                    "deliver_at": {
                        "type": "string",
                        "description": "Timelock vault: ISO 8601 timestamp (e.g. '2026-03-15T12:00:00Z'). Message is encrypted, key held by server until this time. Nobody can read it in the interim — not even the sender. The recipient can be yourself."
                    }
                },
                "required": ["target", "message"]
            }
        },
        {
            "name": "hive_status",
            "description": "List all agents belonging to the same user. Shows agent names, IDs, and which one is you.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "hive_room_send",
            "description": "Send a message to a conversation room. All room members (except you) will receive the message via SSE.\n\nMessages appear as:\n  [HIVE-CONVERSATION] [<topic>] from=<your-name>: <message>",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room_id": {
                        "type": "string",
                        "description": "Room ID (UUID)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message text to send to the room"
                    }
                },
                "required": ["room_id", "message"]
            }
        },
        {
            "name": "hive_room_messages",
            "description": "Get message history for a conversation room. Returns recent messages with sender, content, and timestamp.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room_id": {
                        "type": "string",
                        "description": "Room ID (UUID)"
                    }
                },
                "required": ["room_id"]
            }
        },
        {
            "name": "hive_room_create",
            "description": "Create a new conversation room with a topic and initial member agents.\n\nReturns the room ID which can be used with hive_room_send to send messages.\n\nThe expires_in parameter is required — room encryption keys have mandatory expiration. After expiration, the room becomes unreadable and keys are auto-deleted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "Room topic / description"
                    },
                    "members": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent names or IDs to add to the room"
                    },
                    "expires_in": {
                        "type": "string",
                        "description": "Mandatory key expiration duration (e.g. '1h', '24h', '7d', '30m'). Room keys are auto-deleted after this period."
                    }
                },
                "required": ["topic", "members", "expires_in"]
            }
        },
        {
            "name": "hive_room_destroy",
            "description": "Destroy the encryption key for a conversation room. All participants will delete their keys and the server will purge all message ciphertext. The room becomes permanently unreadable.\n\nAny room member can trigger this — not just the creator. This is irreversible.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room_id": {
                        "type": "string",
                        "description": "Room ID (UUID)"
                    }
                },
                "required": ["room_id"]
            }
        },
        {
            "name": "hive_room_list",
            "description": "List all conversation rooms for your user. Shows room IDs, topics, and member lists.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "hive_messages",
            "description": "Get direct message history (messages sent/received outside of rooms). Returns recent DMs with sender, recipient, content, and timestamp.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "restart_session",
            "description": "Restart the Babel3 daemon with an update. Spawns a detached external process that:\n1. Stops the daemon (b3 stop)\n2. Updates the binary (b3 update)\n3. Starts fresh from the current working directory (b3 start)\n4. Injects a command into the new session (to relaunch Claude Code, etc.)\n5. Injects a notice about the restart log location\n\nThe post-startup command is resolved in this order:\n1. post_command parameter (if provided)\n2. B3_LAUNCH_CMD environment variable (if set)\n3. No post-command (daemon starts but no Claude session is launched)\n\nAgents should set B3_LAUNCH_CMD once so that restart_session() works without arguments.\n\nWARNING: This will kill your current session. The user must wait ~10-15 seconds for the new session to be ready.\n\nThe restart log is saved to /tmp/b3-restart-{timestamp}.log and its path is injected into the new session so the agent can review what happened.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "post_command": {
                        "type": "string",
                        "description": "Command to inject into the new session after startup. If omitted, falls back to B3_LAUNCH_CMD env var."
                    }
                },
                "required": []
            }
        },
        // ── Session-memory tools ──────────────────────────────────────────
        {
            "name": "find_active_session",
            "description": "Find the currently active Claude session file (most recently modified .jsonl).",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "get_session_info",
            "description": "Get metadata about a session: timestamps, line count, size, files written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session UUID (from list_sessions). Resolved to file path automatically."
                    },
                    "session_file": {
                        "type": "string",
                        "description": "Path to session JSONL. Optional — uses active session if omitted."
                    }
                },
                "required": []
            }
        },
        {
            "name": "list_sessions",
            "description": "List all Claude session files with metadata, sorted by size descending.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_size_mb": {
                        "type": "number",
                        "description": "Minimum file size in MB (default 0 = all). Use 1.0 to skip tiny sub-sessions."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions to return (default 50, 0 = unlimited)."
                    },
                    "after": {
                        "type": "string",
                        "description": "Only sessions with last_timestamp after this ISO date."
                    },
                    "before": {
                        "type": "string",
                        "description": "Only sessions with last_timestamp before this ISO date."
                    }
                },
                "required": []
            }
        },
        {
            "name": "add_session_dir",
            "description": "Add a directory to the persistent session search list (~/.b3/session-dirs.json). The directory will be included in all future list_sessions and resurface calls without requiring env vars or config restarts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": {
                        "type": "string",
                        "description": "Absolute path to the directory containing session JSONL files."
                    }
                },
                "required": ["dir"]
            }
        },
        {
            "name": "resurface",
            "description": "Resurface moments from a session — extract and filter content by type with optional sampling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_line": {
                        "type": "integer",
                        "description": "Starting line number. Negative = from end (e.g. -1000 = last 1000 lines)."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Ending line number. -1 = end of file."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session UUID (from list_sessions). Resolved to file path automatically."
                    },
                    "session_file": {
                        "type": "string",
                        "description": "Path to session JSONL. Optional — uses active session if omitted."
                    },
                    "include_timestamps": {
                        "type": "boolean",
                        "description": "Include timestamps in output (default true)."
                    },
                    "frac_sample": {
                        "type": "number",
                        "description": "Random sample fraction 0.0-1.0. Omit or 1.0 for all lines."
                    },
                    "max_word_length": {
                        "type": "integer",
                        "description": "Truncate output lines to this many words."
                    },
                    "seed": {
                        "type": "integer",
                        "description": "Random seed for reproducible sampling."
                    },
                    "output_file": {
                        "type": "string",
                        "description": "Write output to this file path instead of returning (saves tokens)."
                    },
                    "record_type": {
                        "type": "string",
                        "description": "What to extract: conversation (default), tool_use, bash, grep, find, write, read, all.",
                        "enum": ["conversation", "tool_use", "bash", "grep", "find", "write", "read", "all"]
                    },
                    "tool_result_limit": {
                        "type": "integer",
                        "description": "Max chars for tool_result content in 'all' mode (default 100, 0 = unlimited)."
                    }
                },
                "required": ["start_line", "end_line"]
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Text preprocessing — clean_for_tts
// ---------------------------------------------------------------------------

// Pre-compiled regexes for TTS text cleaning. Compiled once on first use.
mod tts_patterns {
    use std::sync::LazyLock;
    use regex::Regex;

    pub static BOLD_DOUBLE_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    pub static BOLD_DOUBLE_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
    pub static ITALIC_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(.+?)\*").unwrap());
    pub static ITALIC_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(.+?)_").unwrap());
    pub static HEADERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
    pub static LINKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").unwrap());
    pub static CODE_BLOCKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```[^\n]*\n.*?```").unwrap());
    pub static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
    pub static BULLET_POINTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[-*+]\s+").unwrap());
    pub static NUMBERED_LISTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*\d+\.\s+").unwrap());
    pub static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
}

/// Clean text for TTS consumption. Strips markdown formatting, normalizes
/// whitespace, expands common abbreviations that TTS engines mangle.
fn clean_for_tts(text: &str) -> String {
    use tts_patterns::*;

    let mut result = text.to_string();

    // Strip markdown bold/italic
    result = BOLD_DOUBLE_STAR.replace_all(&result, "$1").to_string();
    result = BOLD_DOUBLE_UNDER.replace_all(&result, "$1").to_string();
    result = ITALIC_STAR.replace_all(&result, "$1").to_string();
    result = ITALIC_UNDER.replace_all(&result, "$1").to_string();

    // Strip markdown headers: ## Header → Header
    result = HEADERS.replace_all(&result, "").to_string();

    // Strip markdown links: [text](url) → text
    result = LINKS.replace_all(&result, "$1").to_string();

    // Strip code blocks (BEFORE inline code — backtick matching order matters)
    result = CODE_BLOCKS.replace_all(&result, "").to_string();

    // Strip inline code: `code` → code
    result = INLINE_CODE.replace_all(&result, "$1").to_string();

    // Strip bullet points: - item or * item → item
    result = BULLET_POINTS.replace_all(&result, "").to_string();

    // Strip numbered lists: 1. item → item
    result = NUMBERED_LISTS.replace_all(&result, "").to_string();

    // Expand common abbreviations
    result = result.replace("e.g.", "for example");
    result = result.replace("i.e.", "that is");
    result = result.replace("etc.", "etcetera");
    result = result.replace("vs.", "versus");
    result = result.replace("Dr.", "Doctor");
    result = result.replace("Mr.", "Mister");
    result = result.replace("Mrs.", "Missus");
    result = result.replace("Ms.", "Ms");

    // Normalize em dashes
    result = result.replace("—", " — ");
    result = result.replace("–", " — ");

    // Collapse multiple whitespace/newlines
    result = WHITESPACE.replace_all(&result, " ").to_string();

    result.trim().to_string()
}

// ---------------------------------------------------------------------------
// msg_id generation
// ---------------------------------------------------------------------------

fn generate_msg_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let id: u32 = rng.gen();
    format!("{:08x}", id)
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

fn handle_voice_say(params: &Value) -> Value {
    let replying_to = params
        .get("replying_to")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let text = params
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let emotion = params
        .get("emotion")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if text.is_empty() {
        return tool_error("text parameter is required and cannot be empty");
    }

    let msg_id = generate_msg_id();
    let cleaned = clean_for_tts(text);

    // POST to the embedded web server which calls RunPod Chatterbox,
    // saves WAV chunks, and broadcasts audio URLs via WebSocket.
    let port = dashboard_port();
    let api_key = daemon_api_key();
    let url = format!("http://127.0.0.1:{}/api/tts", port);
    let auth_header = format!("Bearer {}", api_key);

    let body = json!({
        "text": cleaned,
        "msg_id": msg_id,
        "emotion": emotion,
        "replying_to": replying_to,
    });

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "2",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &format!("Authorization: {}", auth_header),
            "-d", &body.to_string(),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let response: Value = match serde_json::from_str(stdout.trim()) {
                Ok(v) => v,
                Err(_) => {
                    // Curl failed or daemon returned non-JSON (e.g. 401 Unauthorized)
                    return tool_result(json!({
                        "status": "error",
                        "error": format!(
                            "Daemon returned non-JSON (exit={}, port={}): {}",
                            exit_code, port, stdout.trim()
                        ),
                        "stderr": stderr.trim(),
                        "port": port,
                        "has_api_key": !api_key.is_empty(),
                    }));
                }
            };

            // Find a safe UTF-8 boundary for the preview (don't panic on multi-byte chars)
            let preview = if cleaned.len() > 80 {
                let boundary = cleaned.floor_char_boundary(80);
                format!("{}...", &cleaned[..boundary])
            } else {
                cleaned.clone()
            };

            // Derive status/note from daemon response for the agent
            let clients = response.get("clients").and_then(|v| v.as_u64()).unwrap_or(0);
            let relayed = response.get("relayed").and_then(|v| v.as_bool()).unwrap_or(false);
            let is_credits_depleted = response.get("error")
                .and_then(|v| v.as_str())
                .map(|e| e == "credits_depleted")
                .unwrap_or(false);

            let (status, note) = if is_credits_depleted {
                ("credits_depleted", if clients > 0 {
                    "Browser connected but credits depleted — audio cannot play. User sees credit popup."
                } else {
                    "Credits depleted and no browsers connected"
                })
            } else if clients > 0 {
                ("delivered", "Text sent to browser for speech synthesis")
            } else if relayed {
                ("delivered_via_relay", "Text forwarded to EC2 relay for browser delivery")
            } else {
                ("no_listeners", "No browser tabs connected — open the dashboard to hear voice")
            };

            // Start with the full daemon response (pass through all fields),
            // then overlay MCP-specific fields. This way new daemon fields
            // (credits_remaining, warnings, etc.) flow through automatically.
            let mut result = response.clone();
            result["status"] = json!(status);
            result["note"] = json!(note);
            result["text_preview"] = json!(preview);
            result["emotion"] = json!(emotion);

            tool_result(result)
        }
        Err(e) => {
            tool_error(&format!("Failed to send TTS (is b3 daemon running?): {}", e))
        }
    }
}

fn handle_voice_status(_params: &Value) -> Value {
    let port = dashboard_port();
    let runpod_gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
    let runpod_api_key = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
    let use_runpod = !runpod_gpu_id.is_empty() && !runpod_api_key.is_empty();

    let mut status = json!({});

    // Check embedded web server (TTS + audio serving)
    match std::process::Command::new("curl")
        .args(["-s", "-m", "3", &format!("http://127.0.0.1:{}/health", port)])
        .output()
    {
        Ok(output) if output.status.success() => {
            status["embedded_server"] = json!({"status": "ok"});
        }
        _ => {
            status["embedded_server"] = json!({"status": "down", "error": "embedded server not reachable"});
        }
    }

    // GPU configuration
    if use_runpod {
        status["gpu"] = json!({
            "status": "configured",
            "endpoint": runpod_gpu_id,
            "method": "runpod_unified_worker",
            "note": "Whisper + Chatterbox + embeddings on single GPU"
        });
    } else {
        status["gpu"] = json!({
            "status": "not_configured",
            "note": "Set RUNPOD_GPU_ID + RUNPOD_API_KEY for RunPod, or WHISPER_URL + CHATTERBOX_URL for local"
        });
    }

    tool_result(status)
}

fn handle_voice_health(_params: &Value) -> Value {
    let port = dashboard_port();
    let api_key = daemon_api_key();

    let mut health = json!({
        "components": {},
        "overall": "unknown"
    });
    let mut all_ok = true;

    // 1. Check daemon is running (same path voice_say uses)
    let daemon_url = format!("http://127.0.0.1:{}/health", port);
    let daemon_ok = match std::process::Command::new("curl")
        .args(["-s", "-m", "5", "-w", "\n%{http_code}", &daemon_url])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.trim().split('\n').collect();
            let code: u16 = lines.last().and_then(|s| s.parse().ok()).unwrap_or(0);
            code == 200
        }
        _ => false,
    };

    if !daemon_ok {
        health["components"]["daemon"] = json!({
            "status": "unreachable",
            "label": format!("Babel3 daemon not running on port {}", port)
        });
        health["overall"] = json!("degraded");
        return tool_result(health);
    }
    health["components"]["daemon"] = json!({
        "status": "healthy",
        "label": "Babel3 daemon"
    });

    // 2. Get daemon's diagnostics — shows GPU config, connected browsers, env vars
    //    from the daemon's OWN process (not the MCP process).
    let diag_url = format!("http://127.0.0.1:{}/api/diagnostics", port);
    let auth_header = format!("Bearer {}", api_key);
    let diag: Value = match std::process::Command::new("curl")
        .args(["-s", "-m", "5", "-H", &format!("Authorization: {}", auth_header), &diag_url])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            serde_json::from_str(stdout.trim()).unwrap_or(json!({}))
        }
        _ => json!({}),
    };

    // Connected browsers
    let browsers = diag.get("connected_browsers")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if browsers > 0 {
        health["components"]["browsers"] = json!({
            "status": "healthy",
            "label": format!("{} browser(s) connected", browsers)
        });
    } else {
        health["components"]["browsers"] = json!({
            "status": "no_clients",
            "label": "No browsers connected — voice_say will queue but not play"
        });
        all_ok = false;
    }

    // RunPod credentials in daemon process
    let runpod_configured = diag.get("voice_pipeline")
        .and_then(|v| v.get("runpod_configured"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let runpod_gpu_id = diag.get("voice_pipeline")
        .and_then(|v| v.get("runpod_gpu_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // 3. Check local GPU worker (primary TTS path — same check daemon's tts() does)
    let api_url = crate::config::Config::load()
        .map(|c| c.api_url)
        .unwrap_or_else(|_| b3_common::public_url());
    let local_gpu_health = format!("{}/local-gpu/health", api_url.trim_end_matches('/'));
    let local_gpu_ok = match std::process::Command::new("curl")
        .args(["-s", "-m", "3", "-o", "/dev/null", "-w", "%{http_code}", &local_gpu_health])
        .output()
    {
        Ok(output) => {
            String::from_utf8_lossy(&output.stdout).trim() == "200"
        }
        _ => false,
    };

    if local_gpu_ok {
        health["components"]["local_gpu"] = json!({
            "status": "healthy",
            "label": "Local GPU worker (via EC2 proxy)"
        });
    } else {
        health["components"]["local_gpu"] = json!({
            "status": "unreachable",
            "label": "Local GPU worker (via EC2 proxy)"
        });
    }

    // 4. Check RunPod GPU (cloud fallback — using daemon's credentials)
    if runpod_configured {
        // Ask daemon for GPU config so we can check RunPod health with the right credentials
        let gpu_config_url = format!("http://127.0.0.1:{}/api/gpu-config", port);
        let gpu_cfg: Value = match std::process::Command::new("curl")
            .args(["-s", "-m", "5", "-H", &format!("Authorization: {}", auth_header), &gpu_config_url])
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                serde_json::from_str(stdout.trim()).unwrap_or(json!({}))
            }
            _ => json!({}),
        };
        let gpu_token = gpu_cfg.get("gpu_token").and_then(|v| v.as_str()).unwrap_or("");

        let health_url = format!("https://api.runpod.ai/v2/{}/health", runpod_gpu_id);
        match std::process::Command::new("curl")
            .args([
                "-s", "-m", "10",
                "-H", &format!("Authorization: Bearer {}", gpu_token),
                &health_url,
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let data: Value = serde_json::from_str(stdout.trim()).unwrap_or(json!({}));
                let workers = data.get("workers").and_then(|w| w.get("ready")).and_then(|v| v.as_u64()).unwrap_or(0);
                if workers > 0 {
                    health["components"]["runpod_gpu"] = json!({
                        "status": "healthy",
                        "label": "RunPod GPU worker",
                        "endpoint": runpod_gpu_id,
                        "ready_workers": workers
                    });
                } else {
                    health["components"]["runpod_gpu"] = json!({
                        "status": "cold",
                        "label": "RunPod GPU (no active workers — cold start on next request)",
                        "endpoint": runpod_gpu_id
                    });
                }
            }
            _ => {
                health["components"]["runpod_gpu"] = json!({
                    "status": "unreachable",
                    "label": "RunPod GPU worker",
                    "endpoint": runpod_gpu_id
                });
                all_ok = false;
            }
        }
    } else {
        health["components"]["runpod_gpu"] = json!({
            "status": "not_configured",
            "label": "RunPod GPU — daemon has no RunPod credentials"
        });
        all_ok = false;
    }

    // Overall: healthy only if all components work
    if !local_gpu_ok { all_ok = false; }

    health["overall"] = json!(if all_ok { "healthy" } else { "degraded" });
    tool_result(health)
}

fn handle_voice_logs(params: &Value) -> Value {
    let lines = params
        .get("lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(50);

    // Read Babel3 daemon logs — prefer per-user log, fall back to legacy paths
    let tmp = std::env::temp_dir();
    let mut log_locations = Vec::new();
    if let Some(home) = dirs::home_dir() {
        log_locations.push(home.join(".b3").join("daemon.log"));
    }
    log_locations.push(tmp.join("b3-daemon.log"));
    log_locations.push(tmp.join("b3-daemon-stdout.log"));
    log_locations.push(tmp.join("b3-out.log"));

    for log_path in &log_locations {
        if let Ok(content) = std::fs::read_to_string(log_path) {
            let log_lines: Vec<&str> = content.lines().collect();
            let start = if log_lines.len() > lines as usize {
                log_lines.len() - lines as usize
            } else {
                0
            };
            return tool_result(json!({
                "source": log_path,
                "logs": log_lines[start..].join("\n")
            }));
        }
    }

    // Try journalctl as fallback
    match std::process::Command::new("journalctl")
        .args(["-u", "b3", "-n", &lines.to_string(), "--no-pager", "-o", "short-iso"])
        .output()
    {
        Ok(output) if output.status.success() => {
            tool_result(json!({"source": "journalctl", "logs": String::from_utf8_lossy(&output.stdout).trim()}))
        }
        _ => tool_result(json!({"logs": "No Babel3 logs found. Is the daemon running? Try: b3 start"})),
    }
}


fn handle_voice_share_info(params: &Value) -> Value {
    let html = match params.get("html").and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return tool_error("html parameter is required"),
    };

    // Broadcast as a TTS message with the HTML content — the browser
    // can display it or ignore it. For now, we send it as a special message.
    let port = dashboard_port();
    let api_key = daemon_api_key();
    let url = format!("http://127.0.0.1:{}/api/inject", port);
    let body = json!({
        "type": "hive",
        "sender": "info",
        "text": html,
    });

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "5",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &format!("Authorization: Bearer {}", api_key),
            "-d", &body.to_string(),
            &url,
        ])
        .output()
    {
        Ok(_) => tool_result(json!({"status": "ok", "note": "Info shared via embedded server"})),
        Err(e) => tool_error(&format!("Failed to share info: {}", e)),
    }
}

fn handle_animation_add(params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.trim(),
        None => return tool_error("name parameter is required"),
    };
    let description = match params.get("description").and_then(|v| v.as_str()) {
        Some(d) => d.trim(),
        None => return tool_error("description parameter is required"),
    };
    let file_path = match params.get("file_path").and_then(|v| v.as_str()) {
        Some(f) => f.trim(),
        None => return tool_error("file_path parameter is required"),
    };

    if name.is_empty() || description.is_empty() || file_path.is_empty() {
        return tool_error("name, description, and file_path are all required");
    }

    // Read the animation code from the file
    let code = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => return tool_error(&format!("Failed to read {}: {}", file_path, e)),
    };

    if code.len() > 4096 {
        return tool_error("Animation code must be 4KB or less");
    }

    // POST to EC2 /api/animations (requires auth cookie — use daemon proxy)
    let ec2_base = std::env::var("B3_EC2_BASE").unwrap_or_else(|_| "https://babel3.com".to_string());
    let daemon_token = daemon_api_key();
    let url = format!("{}/api/animations", ec2_base);

    let body = json!({
        "name": name,
        "description": description,
        "pattern_code": code,
    });

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "15",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &format!("Cookie: session={}", get_session_cookie()),
            "-d", &body.to_string(),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(resp) = serde_json::from_str::<Value>(&stdout) {
                if resp.get("id").is_some() {
                    tool_result(json!({
                        "status": "created",
                        "name": name,
                        "description": description,
                        "file": file_path,
                        "response": resp,
                    }))
                } else {
                    tool_error(&format!("Server returned: {}", stdout.chars().take(200).collect::<String>()))
                }
            } else {
                tool_error(&format!("Unexpected response: {}", stdout.chars().take(200).collect::<String>()))
            }
        }
        Err(e) => tool_error(&format!("Failed to submit animation: {}", e)),
    }
}

/// Try to read the session cookie from the daemon's stored credentials.
fn get_session_cookie() -> String {
    let cookie_path = crate::config::Config::config_dir().join("session-cookie");
    std::fs::read_to_string(cookie_path).unwrap_or_default().trim().to_string()
}

fn handle_email_draft(params: &Value) -> Value {
    let to = match params.get("to").and_then(|v| v.as_str()) {
        Some(t) => t.trim(),
        None => return tool_error("to parameter is required"),
    };
    let subject = match params.get("subject").and_then(|v| v.as_str()) {
        Some(s) => s.trim(),
        None => return tool_error("subject parameter is required"),
    };
    let body = match params.get("body").and_then(|v| v.as_str()) {
        Some(b) => b.trim(),
        None => return tool_error("body parameter is required"),
    };
    let in_reply_to = params.get("in_reply_to").and_then(|v| v.as_i64());

    if to.is_empty() || subject.is_empty() || body.is_empty() {
        return tool_error("to, subject, and body are all required");
    }

    // POST to EC2 /api/email/agent-draft (uses agent API key auth)
    let ec2_base = std::env::var("B3_EC2_BASE").unwrap_or_else(|_| "https://babel3.com".to_string());
    let url = format!("{}/api/email/agent-draft", ec2_base);

    let mut draft = serde_json::json!({
        "to": to,
        "subject": subject,
        "body_html": body,
        "body_text": body,
    });
    if let Some(reply_id) = in_reply_to {
        draft["in_reply_to"] = serde_json::json!(reply_id);
    }

    let api_key = daemon_api_key();
    let auth_header = format!("Authorization: Bearer {}", api_key);

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "10",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &auth_header,
            "-d", &draft.to_string(),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(resp) = serde_json::from_str::<Value>(&stdout) {
                if resp.get("id").is_some() {
                    tool_result(json!({
                        "status": "draft_submitted",
                        "draft_id": resp["id"],
                        "to": to,
                        "subject": subject,
                        "note": "Draft submitted for owner review. It will be sent after approval.",
                    }))
                } else {
                    tool_error(&format!("Server returned: {}", stdout.chars().take(200).collect::<String>()))
                }
            } else {
                tool_error(&format!("Unexpected response: {}", stdout.chars().take(200).collect::<String>()))
            }
        }
        Err(e) => tool_error(&format!("Failed to submit draft: {}", e)),
    }
}

fn handle_voice_enroll_speakers(params: &Value) -> Value {
    let embeddings_dir = match params.get("embeddings_dir").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => return tool_error("embeddings_dir parameter is required"),
    };

    let dir_path = std::path::Path::new(embeddings_dir);
    if !dir_path.is_dir() {
        return tool_error(&format!("Not a directory: {}", embeddings_dir));
    }

    // Get GPU config from env — try local GPU first, then RunPod
    let local_gpu_url = std::env::var("LOCAL_GPU_URL").unwrap_or_default();
    let local_gpu_token = std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default();
    let gpu_id = std::env::var("RUNPOD_GPU_ID").unwrap_or_default();
    let gpu_token = std::env::var("RUNPOD_API_KEY").unwrap_or_default();

    if local_gpu_url.is_empty() && gpu_id.is_empty() {
        return tool_error("No GPU backend configured — set LOCAL_GPU_URL or RUNPOD_GPU_ID");
    }

    // Get agent_id from config (B3_CONFIG_DIR is inherited from daemon env)
    let agent_id = {
        let config_path = crate::config::Config::config_path();
        let id = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|content| serde_json::from_str::<Value>(&content).ok())
            .and_then(|config| config.get("agent_id").and_then(|v| v.as_str()).map(str::to_string));
        id.unwrap_or_else(|| "unknown".to_string())
    };

    // Read .npy files and base64-encode them
    let mut speakers = serde_json::Map::new();
    match std::fs::read_dir(dir_path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.ends_with(".npy") && !fname.ends_with(".bak") {
                    match std::fs::read(entry.path()) {
                        Ok(bytes) => {
                            let name = fname.trim_end_matches(".npy").to_uppercase();
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            speakers.insert(name, Value::String(b64));
                        }
                        Err(e) => {
                            eprintln!("[voice-mcp] Failed to read {}: {}", fname, e);
                        }
                    }
                }
            }
        }
        Err(e) => return tool_error(&format!("Failed to read directory: {}", e)),
    }

    if speakers.is_empty() {
        return tool_result(json!({
            "status": "no_speakers",
            "note": format!("No .npy files found in {}", embeddings_dir)
        }));
    }

    // Build the enrollment payload
    let payload = json!({
        "input": {
            "action": "enroll",
            "agent_id": agent_id,
            "speakers": speakers,
        }
    });

    let payload_str = payload.to_string();
    let speaker_names: Vec<String> = speakers.keys().cloned().collect();
    let speaker_count = speakers.len();

    // Resolve GPU URL: local first, then RunPod
    let (gpu_url, auth_header) = if !local_gpu_url.is_empty() {
        (format!("{}/runsync", local_gpu_url.trim_end_matches('/')),
         format!("Bearer {}", local_gpu_token))
    } else {
        (format!("https://api.runpod.ai/v2/{}/runsync", gpu_id),
         format!("Bearer {}", gpu_token))
    };

    // Send to GPU via curl (synchronous — MCP is blocking anyway)
    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "30",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &format!("Authorization: {}", auth_header),
            "-d", &payload_str,
            &gpu_url,
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let resp: Value = serde_json::from_str(stdout.trim())
                .unwrap_or(json!({"status": "unknown"}));

            let enrolled = resp
                .pointer("/output/enrolled")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            tool_result(json!({
                "status": "enrolled",
                "agent_id": agent_id,
                "enrolled": enrolled,
                "speakers": speaker_names,
                "note": format!("Enrolled {} speakers on GPU worker for agent '{}'", speaker_count, agent_id)
            }))
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            tool_error(&format!(
                "GPU worker enrollment failed (exit {}): stdout={}, stderr={}",
                output.status, stdout.trim(), stderr.trim()
            ))
        }
        Err(e) => tool_error(&format!("Failed to reach GPU worker: {}", e)),
    }
}

// ---------------------------------------------------------------------------
// Browser console tool handler
// ---------------------------------------------------------------------------

fn handle_browser_console(params: &Value) -> Value {
    let last = params.get("last").and_then(|v| v.as_u64()).unwrap_or(100);
    let filter = params.get("filter").and_then(|v| v.as_str()).unwrap_or("");
    let secondary_filter = params.get("secondary_filter").and_then(|v| v.as_str()).unwrap_or("");

    let port = dashboard_port();
    let api_key = daemon_api_key();
    let mut url = format!("http://127.0.0.1:{}/api/browser-console?last={}", port, last);
    if !filter.is_empty() {
        url.push_str(&format!("&filter={}", filter));
    }
    if !secondary_filter.is_empty() {
        url.push_str(&format!("&secondary_filter={}", secondary_filter));
    }

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "5",
            "-H", &format!("Authorization: Bearer {}", api_key),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => {
                    let count = v.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
                    let total = v.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
                    let logs = v.get("logs").and_then(|l| l.as_array());

                    // Estimate character count (~tokens) of the logs array
                    let char_count: usize = logs.map(|entries| {
                        entries.iter()
                            .map(|e| e.as_str().map(|s| s.len()).unwrap_or(0))
                            .sum()
                    }).unwrap_or(0);

                    // Short result (≤2000 chars, ~500 tokens): return inline
                    if char_count <= 2000 {
                        return tool_result(v);
                    }

                    // Long result: write to temp file, return path
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let filename = format!("browser-console-{}.txt", ts);
                    let filepath = std::env::temp_dir().join(&filename);

                    let file_content = if let Some(entries) = logs {
                        entries.iter()
                            .map(|e| e.as_str().unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        serde_json::to_string_pretty(&v).unwrap_or_default()
                    };

                    match std::fs::write(&filepath, &file_content) {
                        Ok(_) => tool_result(json!({
                            "count": count,
                            "total": total,
                            "chars": char_count,
                            "file": filepath.to_string_lossy(),
                            "note": format!(
                                "{} log entries ({} chars) written to file. Use Read, Grep, or Bash tools to analyze.",
                                count, char_count
                            )
                        })),
                        Err(e) => {
                            // Fall back to inline if file write fails
                            eprintln!("browser_console: failed to write temp file: {}", e);
                            tool_result(v)
                        }
                    }
                }
                Err(_) => tool_error(&format!("Daemon returned non-JSON: {}", stdout.trim())),
            }
        }
        Err(e) => tool_error(&format!("Failed to reach daemon: {}", e)),
    }
}

fn handle_browser_eval(params: &Value) -> Value {
    let code = match params.get("code").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return tool_error("code parameter is required"),
    };

    let target_session = params.get("session_id").and_then(|v| v.as_u64());

    let port = dashboard_port();
    let api_key = daemon_api_key();
    let url = format!("http://127.0.0.1:{}/api/browser-eval", port);
    let mut body = serde_json::json!({ "code": code });
    if let Some(sid) = target_session {
        body["session_id"] = serde_json::json!(sid);
    }

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "10",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &format!("Authorization: Bearer {}", api_key),
            "-d", &body.to_string(),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => tool_result(v),
                Err(_) => tool_error(&format!("Daemon returned non-JSON: {}", stdout.trim())),
            }
        }
        Err(e) => tool_error(&format!("Failed to reach daemon: {}", e)),
    }
}

// ---------------------------------------------------------------------------
// Hive tool handlers
// ---------------------------------------------------------------------------

/// Helper: call a daemon hive endpoint via curl and return the parsed JSON.
fn hive_daemon_get(path: &str) -> Value {
    let port = dashboard_port();
    let api_key = daemon_api_key();
    let url = format!("http://127.0.0.1:{}/api/hive{}", port, path);

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "10",
            "-H", &format!("Authorization: Bearer {}", api_key),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => tool_result(v),
                Err(_) => tool_error(&format!("Daemon returned non-JSON: {}", stdout.trim())),
            }
        }
        Err(e) => tool_error(&format!("Failed to reach daemon: {}", e)),
    }
}

/// Helper: POST JSON to a daemon hive endpoint.
fn hive_daemon_post(path: &str, body: &Value) -> Value {
    let port = dashboard_port();
    let api_key = daemon_api_key();
    let url = format!("http://127.0.0.1:{}/api/hive{}", port, path);

    match std::process::Command::new("curl")
        .args([
            "-s", "-m", "10",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "-H", &format!("Authorization: Bearer {}", api_key),
            "-d", &body.to_string(),
            &url,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => tool_result(v),
                Err(_) => tool_error(&format!("Daemon returned non-JSON: {}", stdout.trim())),
            }
        }
        Err(e) => tool_error(&format!("Failed to reach daemon: {}", e)),
    }
}

fn handle_hive_send(params: &Value) -> Value {
    let target = match params.get("target").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return tool_error("target parameter is required"),
    };
    let message = match params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return tool_error("message parameter is required"),
    };
    let ephemeral = params.get("ephemeral").and_then(|v| v.as_bool()).unwrap_or(false);
    let deliver_at = params.get("deliver_at").and_then(|v| v.as_str());
    let mut payload = json!({ "target": target, "message": message });
    if ephemeral {
        payload["ephemeral"] = json!(true);
    }
    if let Some(ts) = deliver_at {
        payload["deliver_at"] = json!(ts);
    }
    hive_daemon_post("/send", &payload)
}

fn handle_hive_status(_params: &Value) -> Value {
    hive_daemon_get("/agents")
}

fn handle_hive_room_send(params: &Value) -> Value {
    let room_id = match params.get("room_id").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => return tool_error("room_id parameter is required"),
    };
    let message = match params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return tool_error("message parameter is required"),
    };
    hive_daemon_post(&format!("/rooms/{}/send", room_id), &json!({ "message": message }))
}

fn handle_hive_room_messages(params: &Value) -> Value {
    let room_id = match params.get("room_id").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => return tool_error("room_id parameter is required"),
    };
    hive_daemon_get(&format!("/rooms/{}/messages", room_id))
}

fn handle_hive_room_create(params: &Value) -> Value {
    let topic = params.get("topic").and_then(|v| v.as_str()).unwrap_or("");
    let members = params.get("members").and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let expires_in = params.get("expires_in").and_then(|v| v.as_str());

    if members.is_empty() {
        return tool_error("members parameter is required (at least one agent name/ID)");
    }
    if expires_in.is_none() {
        return tool_error("expires_in parameter is required (e.g. '1h', '24h', '7d')");
    }

    let mut body = json!({ "topic": topic, "members": members });
    if let Some(exp) = expires_in {
        body["expires_in"] = json!(exp);
    }

    hive_daemon_post("/rooms", &body)
}

fn handle_hive_room_destroy(params: &Value) -> Value {
    let room_id = match params.get("room_id").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => return tool_error("room_id parameter is required"),
    };
    hive_daemon_post(&format!("/rooms/{}/destroy-key", room_id), &json!({}))
}

fn handle_hive_room_list(_params: &Value) -> Value {
    hive_daemon_get("/rooms")
}

fn handle_hive_messages(_params: &Value) -> Value {
    hive_daemon_get("/messages")
}

// ---------------------------------------------------------------------------
// restart_session handler
// ---------------------------------------------------------------------------

fn handle_restart_session(params: &Value) -> Value {
    // Resolve post_command: explicit param > B3_LAUNCH_CMD env var > empty
    let post_command = params
        .get("post_command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| std::env::var("B3_LAUNCH_CMD").ok().filter(|s| !s.is_empty()))
        .unwrap_or_default();
    let post_command = post_command.as_str();

    // Get CWD — set via --start-cwd CLI arg (written to .mcp.json by register_voice_mcp)
    let cwd = crate::daemon::fork_state::start_cwd()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/home".to_string())
        });

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_file = format!("/tmp/b3-restart-{}.log", timestamp);
    let script_file = format!("/tmp/b3-restart-{}.sh", timestamp);

    // Build the restart script
    let post_cmd_section = if post_command.is_empty() {
        String::new()
    } else {
        format!(
            r#"
# Inject post-command (wait for shell to be fully ready)
# Split text and Enter with a delay so terminal treats Enter as "submit", not "multi-line paste"
sleep 5
echo "[$(date)] Injecting post_command: {pc}"
curl -s -m 5 http://127.0.0.1:$PORT/api/inject \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{{"type":"pty_input","data":"{pc_escaped}"}}'
sleep 0.5
curl -s -m 5 http://127.0.0.1:$PORT/api/inject \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{{"type":"pty_input","data":"\r"}}'
echo ""
"#,
            pc = post_command,
            pc_escaped = post_command.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"),
        )
    };

    let api_key = daemon_api_key();

    let script = format!(
        r##"#!/bin/bash
# Babel3 restart script — auto-generated, runs fully detached
# Source .bashrc for env vars (B3_START_CWD, B3_LAUNCH_CMD, etc.)
[ -f "$HOME/.bashrc" ] && source "$HOME/.bashrc" 2>/dev/null
# Override CWD if .bashrc provided B3_START_CWD
CWD="${{B3_START_CWD:-{cwd}}}"
LOG="{log_file}"
API_KEY="{api_key}"

exec >> "$LOG" 2>&1
echo "=== Babel3 Restart ==="
echo "Started: $(date)"
echo "CWD: $CWD"
echo "PID: $$"
echo ""

# Wait for the MCP tool response to be sent back to Claude
sleep 2

# Clear nesting guard — this script is allowed to stop/start
unset B3_SESSION

# Step 1: Stop the daemon
echo "[$(date)] Stopping daemon..."
b3 stop --force 2>&1
echo "[$(date)] Stop exit code: $?"

# Verify daemon is actually dead — kill by PID if stop failed
PID_FILE="$HOME/.b3/b3.pid"
if [ -f "$PID_FILE" ]; then
    OLD_PID=$(cat "$PID_FILE" | tr -d '[:space:]')
    if kill -0 "$OLD_PID" 2>/dev/null; then
        echo "[$(date)] Daemon still alive (PID $OLD_PID) — sending SIGKILL"
        kill -9 "$OLD_PID" 2>/dev/null
        sleep 1
    fi
    rm -f "$PID_FILE"
fi
echo ""

# Step 2: Update the binary
echo "[$(date)] Updating b3..."
b3 update 2>&1
UPDATE_EXIT=$?
echo "[$(date)] Update exit code: $UPDATE_EXIT"
echo ""

# Step 3: Start fresh from original CWD
echo "[$(date)] Starting daemon from $CWD..."
cd "$CWD" || cd /home
b3 start 2>&1 &
START_PID=$!
echo "[$(date)] Start PID: $START_PID"

# Step 4: Wait for daemon to be ready
echo "[$(date)] Waiting for daemon to be ready..."
for i in $(seq 1 30); do
    sleep 1
    AGENT_SLUG=$(python3 -c "import json; print(json.load(open('$HOME/.b3/config.json'))['agent_email'].split('@')[0])" 2>/dev/null || echo "default")
    PORT_FILE="$HOME/.b3/agents/$AGENT_SLUG/dashboard.port"
    if [ -f "$PORT_FILE" ]; then
        PORT=$(cat "$PORT_FILE" | tr -d '[:space:]')
        HTTP_CODE=$(curl -s -o /dev/null -w "%{{http_code}}" -m 2 "http://127.0.0.1:$PORT/health" 2>/dev/null)
        if [ "$HTTP_CODE" = "200" ]; then
            echo "[$(date)] Daemon ready on port $PORT (attempt $i)"
            break
        fi
    fi
    if [ $i -eq 30 ]; then
        echo "[$(date)] TIMEOUT: Daemon did not become ready in 30s"
        echo "=== Restart FAILED ==="
        exit 1
    fi
done
echo ""

# Step 5: Wait for PTY shell to be ready (bash init takes a few seconds after daemon is up)
sleep 5
{post_cmd_section}
# Step 6: Wait for Claude to boot, then inject restart notice into the Claude session
# Split text and Enter with a delay so terminal treats Enter as "submit", not "multi-line paste"
sleep 10
echo "[$(date)] Injecting restart notice into Claude session..."
NOTICE="[restart-notice] Session restarted via restart_session MCP tool. Restart log: {log_file}"
# Send an Enter first to dismiss any menu/prompt the cursor may be on
# (e.g. Claude Code's "trust this folder?" prompt on newer versions)
curl -s -m 5 "http://127.0.0.1:$PORT/api/inject" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{{"type":"pty_input","data":"\r"}}'
# Wait for Claude to transition from menu to input prompt before injecting notice
sleep 3
INJECT_JSON=$(printf '{{"type":"pty_input","data":"%s"}}' "$NOTICE")
curl -s -m 5 "http://127.0.0.1:$PORT/api/inject" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d "$INJECT_JSON"
sleep 0.5
curl -s -m 5 "http://127.0.0.1:$PORT/api/inject" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{{"type":"pty_input","data":"\r"}}'
echo ""
echo "[$(date)] Restart complete."
echo "=== Restart DONE ==="
"##,
        log_file = log_file,
        cwd = cwd,
        api_key = api_key,
        post_cmd_section = post_cmd_section,
    );

    // Write the script
    if let Err(e) = std::fs::write(&script_file, &script) {
        return tool_error(&format!("Failed to write restart script: {}", e));
    }

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_file, std::fs::Permissions::from_mode(0o755));
    }

    // Double-fork + setsid: spawn a fully detached process that survives daemon death
    #[cfg(unix)]
    {
        use std::process::Command;

        // setsid --fork detaches immediately — the script runs in its own session
        let result = Command::new("setsid")
            .args(["--fork", "bash", &script_file])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        match result {
            Ok(child) => {
                let pid = child.id();
                eprintln!("[b3-mcp] restart_session: spawned detached process PID {}", pid);

                let cmd_source = if params.get("post_command").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
                    "parameter"
                } else if !post_command.is_empty() {
                    "B3_LAUNCH_CMD"
                } else {
                    "none"
                };

                tool_result(json!({
                    "status": "restart_initiated",
                    "message": format!(
                        "Restart process launched (PID {}). The daemon will stop in ~2 seconds. \
                         Your session will end. A new session will start in ~10-15 seconds. \
                         Restart log: {}",
                        pid, log_file
                    ),
                    "log_file": log_file,
                    "script_file": script_file,
                    "pid": pid,
                    "cwd": cwd,
                    "has_post_command": !post_command.is_empty(),
                    "post_command_source": cmd_source,
                    "post_command": post_command,
                }))
            }
            Err(e) => {
                // setsid not available? Fall back to nohup
                eprintln!("[b3-mcp] setsid failed: {}, trying nohup fallback", e);

                let fallback = Command::new("bash")
                    .args(["-c", &format!(
                        "nohup bash {} </dev/null >/dev/null 2>&1 &",
                        script_file
                    )])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();

                match fallback {
                    Ok(_) => {
                        tool_result(json!({
                            "status": "restart_initiated",
                            "message": format!(
                                "Restart process launched (nohup fallback). The daemon will stop in ~2 seconds. \
                                 Your session will end. Restart log: {}",
                                log_file
                            ),
                            "log_file": log_file,
                            "note": "Used nohup fallback — setsid was not available",
                        }))
                    }
                    Err(e2) => {
                        tool_error(&format!(
                            "Failed to spawn restart process. setsid error: {}, nohup error: {}",
                            e, e2
                        ))
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    {
        tool_error("restart_session is not supported on Windows yet")
    }
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

fn tool_result(data: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".to_string())
        }],
        "isError": false
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": message
        }],
        "isError": true
    })
}

/// Wrap a session-memory handler's return value in the MCP tool-result envelope.
/// Session-memory handlers return raw JSON (e.g. `{"total": N, "sessions": [...]}`)
/// instead of the `{"content": [{"type":"text","text":"..."}], "isError": ...}` shape
/// that voice/hive handlers apply via `tool_result()`. If the handler already returned
/// an error envelope (via its local `tool_error`), it's passed through unchanged.
fn wrap_session_memory_result(result: Value) -> Value {
    if result.get("content").is_some() {
        // Already an MCP-shaped response (tool_error path) — pass through.
        result
    } else {
        let text = serde_json::to_string(&result).unwrap_or_default();
        json!({
            "content": [{"type": "text", "text": text}],
            "isError": false,
        })
    }
}

fn make_response(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

fn make_error_response(id: Value, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Channel notification delivery
// ---------------------------------------------------------------------------

/// Write a `notifications/claude/channel` JSON-RPC notification to stdout.
/// This is a fire-and-forget notification (no `id` field) per the MCP spec.
fn send_channel_notification(
    stdout: &Arc<Mutex<io::Stdout>>,
    content: &str,
    chat_id: &str,
    message_id: &str,
    user: &str,
) {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": {
                "chat_id": chat_id,
                "message_id": message_id,
                "user": user,
                "ts": chrono::Utc::now().to_rfc3339(),
            }
        }
    });
    // Must be single-line — JSON-RPC over stdio is newline-delimited
    let line = match serde_json::to_string(&notification) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[b3-mcp] channel notification serialize error: {e}");
            return;
        }
    };
    if let Ok(mut out) = stdout.lock() {
        let _ = writeln!(out, "{}", line);
        let _ = out.flush();
    }
}

/// Background thread: connects to daemon's /api/channel-events SSE endpoint
/// and delivers transcription events as MCP channel notifications.
/// Reconnects with backoff on disconnect.
fn channel_events_watcher(
    stdout: Arc<Mutex<io::Stdout>>,
    port: &str,
    api_key: &str,
) {
    let url = format!("http://127.0.0.1:{}/api/channel-events?token={}", port, api_key);
    let mut backoff = std::time::Duration::from_secs(1);
    let max_backoff = std::time::Duration::from_secs(30);

    loop {
        eprintln!("[b3-mcp] Connecting to channel-events SSE at port {}", port);

        match ureq::get(&url).call() {
            Ok(response) => {
                backoff = std::time::Duration::from_secs(1); // reset on success
                let reader = io::BufReader::new(response.into_reader());
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            let line = line.trim().to_string();
                            if line.starts_with("data:") {
                                let data = line[5..].trim();
                                if data.is_empty() || data == ":" {
                                    continue;
                                }
                                // Parse ChannelEvent and dispatch as channel notification
                                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                    let msg_id = event.get("message_id").and_then(|v| v.as_str()).unwrap_or("unknown");

                                    match event_type {
                                        "transcription" => {
                                            if !text.is_empty() {
                                                eprintln!("[b3-mcp] Channel: transcription ({} bytes)", text.len());
                                                send_channel_notification(&stdout, text, "voice", msg_id, "voice");
                                            }
                                        }
                                        "hive_dm" => {
                                            let sender = event.get("sender").and_then(|v| v.as_str()).unwrap_or("unknown");
                                            if !text.is_empty() {
                                                eprintln!("[b3-mcp] Channel: hive DM from {sender} ({} bytes)", text.len());
                                                send_channel_notification(&stdout, text, "hive_dm", msg_id, sender);
                                            }
                                        }
                                        "hive_room" => {
                                            let sender = event.get("sender").and_then(|v| v.as_str()).unwrap_or("unknown");
                                            let room_id = event.get("room_id").and_then(|v| v.as_str()).unwrap_or("unknown");
                                            if !text.is_empty() {
                                                let chat_id = format!("hive_room_{room_id}");
                                                eprintln!("[b3-mcp] Channel: hive room from {sender} in {room_id} ({} bytes)", text.len());
                                                send_channel_notification(&stdout, text, &chat_id, msg_id, sender);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            // Skip SSE comments (lines starting with ":") and event: lines
                        }
                        Err(e) => {
                            eprintln!("[b3-mcp] channel-events SSE read error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[b3-mcp] channel-events SSE connect failed: {e}");
            }
        }

        // Backoff before reconnect
        eprintln!("[b3-mcp] channel-events SSE disconnected, retrying in {:?}", backoff);
        std::thread::sleep(backoff);
        backoff = std::cmp::min(backoff * 2, max_backoff);
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

pub async fn run() -> anyhow::Result<()> {
    // MCP server runs synchronously on stdio — no tokio needed for the loop,
    // but we keep the async signature for consistency with other commands.
    let stdin = io::stdin();
    let stdout = Arc::new(Mutex::new(io::stdout()));

    // Spawn background thread: subscribes to daemon's channel-events SSE and
    // writes notifications/claude/channel to stdout for transcription delivery.
    {
        let stdout_for_watcher = stdout.clone();
        let port = dashboard_port();
        let api_key = daemon_api_key();
        std::thread::spawn(move || {
            channel_events_watcher(stdout_for_watcher, &port, &api_key);
        });
    }

    // Death instrumentation: log to a file on any exit so we can diagnose
    // why the MCP process dies mid-session (see investigations/b3-mcp-death.md)
    let death_log = dirs::home_dir()
        .map(|h| h.join(".b3").join("mcp-death.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/b3-mcp-death.log"));

    // Install signal handlers to catch what kills us
    #[cfg(unix)]
    {
        use std::sync::atomic::{AtomicI32, Ordering};
        static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

        unsafe extern "C" fn signal_handler(sig: libc::c_int) {
            LAST_SIGNAL.store(sig, Ordering::Relaxed);
            // Write directly to stderr (signal-safe)
            let msg: &[u8] = match sig {
                libc::SIGTERM => b"[b3-mcp] SIGTERM received\n",
                libc::SIGINT  => b"[b3-mcp] SIGINT received\n",
                libc::SIGHUP  => b"[b3-mcp] SIGHUP received\n",
                libc::SIGPIPE => b"[b3-mcp] SIGPIPE received\n",
                _             => b"[b3-mcp] unknown signal received\n",
            };
            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());

            // Also write to death log file
            let path = b"/tmp/b3-mcp-signal.log\0";
            let fd = libc::open(
                path.as_ptr() as *const libc::c_char,
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                0o644,
            );
            if fd >= 0 {
                libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
                libc::close(fd);
            }

            // Exit cleanly on any terminal signal.
            // SIGPIPE MUST exit — if we just log and return, the write to stderr
            // triggers another SIGPIPE, creating an infinite spin loop that
            // writes 14MB/s and saturates disk I/O (discovered 2026-03-24).
            libc::_exit(128 + sig);
        }

        unsafe {
            let handler = signal_handler as *const () as libc::sighandler_t;
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGHUP, handler);
            libc::signal(libc::SIGPIPE, handler);
        }
    }

    // Panic hook: log panics to death log before crashing
    let death_log_clone = death_log.clone();
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("[b3-mcp] PANIC: {} (PID {})\n", info, std::process::id());
        eprintln!("{}", msg.trim());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log_clone) {
            use std::io::Write as _;
            let _ = write!(f, "{}", msg);
        }
    }));

    // Log to stderr (Claude Code reads stdout as MCP protocol)
    let my_pid = std::process::id();
    eprintln!("[b3-mcp] Starting stdio MCP server (PID {})", my_pid);

    // Orphan watchdog: if our parent dies, ppid becomes 1 (init).
    // Exit immediately to avoid burning CPU as an orphaned process.
    #[cfg(unix)]
    {
        let parent_pid = unsafe { libc::getppid() };
        eprintln!("[b3-mcp] Parent PID: {}", parent_pid);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let ppid = unsafe { libc::getppid() };
                if ppid == 1 {
                    eprintln!("[b3-mcp] Parent died (ppid=1), self-terminating (PID {})", my_pid);
                    std::process::exit(0);
                }
            }
        });
    }

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[b3-mcp] stdin read error: {}", e);
                break;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[b3-mcp] parse error: {} | line: {}", e, line);
                // Send parse error response
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                };
                if let Ok(mut out) = stdout.lock() {
                    let _ = writeln!(out, "{}", serde_json::to_string(&resp)?);
                    let _ = out.flush();
                }
                continue;
            }
        };

        // Notifications have no id — don't respond
        if request.id.is_none() {
            eprintln!(
                "[b3-mcp] notification: {}",
                request.method
            );
            continue;
        }

        let id = request.id.unwrap();

        let response = match request.method.as_str() {
            "initialize" => {
                make_response(
                    id,
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": {
                                "listChanged": false
                            },
                            "experimental": {
                                "claude/channel": {}
                            }
                        },
                        "serverInfo": {
                            "name": "b3",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "instructions": "Babel3 agent MCP — voice I/O, health monitoring, speaker enrollment, inter-agent messaging, and session recall.\n\nVoice transcriptions arrive as <channel source=\"b3\" chat_id=\"voice\"> messages. Treat these as the user speaking to you. Reply with say() so your response goes to their ears. Always pass replying_to with the user's transcription text (or a summary of what you're responding to).\n\nHive messages arrive as <channel source=\"b3\" chat_id=\"hive_dm\" user=\"agent-name\"> for direct messages and <channel source=\"b3\" chat_id=\"hive_room_{id}\" user=\"agent-name\"> for room messages.\n\nVoice: say(text, emotion, replying_to) speaks text to the user's browser instantly via WebSocket. voice_status() and voice_health() check pipeline state.\n\nSpeakers: voice_enroll_speakers() uploads .npy embeddings to the GPU worker for diarization.\n\nInfo: voice_share_info() pushes HTML to browsers, voice_show_image() pushes images.\n\nHive: hive_send() messages another agent. hive_status() lists your sibling agents. hive_room_create() creates a room, hive_room_send() sends to a room, hive_room_messages() reads room history, hive_room_list() lists rooms.\n\nSession recall: list_sessions() lists all Claude Code session files. resurface() extracts and filters session content with optional sampling. find_active_session() finds the current session. get_session_info() returns metadata. add_session_dir() adds a directory to the search path."
                    }),
                )
            }

            "tools/list" => {
                make_response(
                    id,
                    json!({
                        "tools": tool_definitions()
                    }),
                )
            }

            "resources/list" => {
                make_response(
                    id,
                    json!({
                        "resources": []
                    }),
                )
            }

            "prompts/list" => {
                make_response(
                    id,
                    json!({
                        "prompts": []
                    }),
                )
            }

            "tools/call" => {
                let tool_name = request
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let arguments = request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));

                eprintln!("[b3-mcp] tools/call: {}", tool_name);

                let result = match tool_name {
                    "say" | "voice_say" => handle_voice_say(&arguments),
                    "voice_status" => handle_voice_status(&arguments),
                    "voice_health" => handle_voice_health(&arguments),
                    "voice_logs" => handle_voice_logs(&arguments),
                    "voice_share_info" => handle_voice_share_info(&arguments),
                    "animation_add" => handle_animation_add(&arguments),
                    "email_draft" => handle_email_draft(&arguments),
                    "voice_enroll_speakers" => handle_voice_enroll_speakers(&arguments),
                    "browser_console" => handle_browser_console(&arguments),
                    "browser_eval" => handle_browser_eval(&arguments),
                    "hive_send" => handle_hive_send(&arguments),
                    "hive_status" => handle_hive_status(&arguments),
                    "hive_room_send" => handle_hive_room_send(&arguments),
                    "hive_room_messages" => handle_hive_room_messages(&arguments),
                    "hive_room_create" => handle_hive_room_create(&arguments),
                    "hive_room_list" => handle_hive_room_list(&arguments),
                    "hive_room_destroy" => handle_hive_room_destroy(&arguments),
                    "hive_messages" => handle_hive_messages(&arguments),
                    "restart_session" => handle_restart_session(&arguments),
                    // Session-memory tools — dispatched to session_memory module.
                    // These handlers return raw data (e.g. {"total": N, "sessions": [...]})
                    // rather than the MCP envelope that voice/hive handlers apply via
                    // tool_result(). Wrap here to match MCP spec: tools/call result must
                    // be {"content": [{"type": "text", "text": ...}]}.
                    // tool_error already returns the wrapped shape, so we pass it through.
                    "find_active_session" => wrap_session_memory_result(super::session_memory::handle_find_active_session(&arguments)),
                    "get_session_info"    => wrap_session_memory_result(super::session_memory::handle_get_session_info(&arguments)),
                    "list_sessions"       => wrap_session_memory_result(super::session_memory::handle_list_sessions(&arguments)),
                    "resurface"           => wrap_session_memory_result(super::session_memory::handle_resurface(&arguments)),
                    "add_session_dir"     => wrap_session_memory_result(super::session_memory::handle_add_session_dir(&arguments)),
                    _ => tool_error(&format!("Unknown tool: {}", tool_name)),
                };

                make_response(id, result)
            }

            "ping" => make_response(id, json!({})),

            _ => {
                eprintln!(
                    "[b3-mcp] unknown method: {}",
                    request.method
                );
                make_error_response(id, -32601, &format!("Method not found: {}", request.method))
            }
        };

        let json_str = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[b3-mcp] FATAL: JSON serialization failed: {} (PID {})", e, std::process::id());
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log) {
                    use std::io::Write as _;
                    let _ = writeln!(f, "[b3-mcp] exit: JSON serialization error: {}", e);
                }
                return Err(e.into());
            }
        };
        let mut out = stdout.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = writeln!(out, "{}", json_str) {
            eprintln!("[b3-mcp] FATAL: stdout write failed: {} (PID {})", e, std::process::id());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log) {
                use std::io::Write as _;
                let _ = writeln!(f, "[b3-mcp] exit: stdout write error: {}", e);
            }
            return Err(e.into());
        }
        if let Err(e) = out.flush() {
            eprintln!("[b3-mcp] FATAL: stdout flush failed: {} (PID {})", e, std::process::id());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log) {
                use std::io::Write as _;
                let _ = writeln!(f, "[b3-mcp] exit: stdout flush error: {}", e);
            }
            return Err(e.into());
        }
        drop(out); // Release lock immediately
    }

    let exit_reason = "stdin closed (EOF)";
    eprintln!("[b3-mcp] {} — shutting down (PID {})", exit_reason, std::process::id());

    // Write death report to file for post-mortem analysis
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log) {
        use std::io::Write as _;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}] PID {} exit: {} ", now, std::process::id(), exit_reason);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_for_tts_strips_markdown() {
        assert_eq!(clean_for_tts("**bold** text"), "bold text");
        assert_eq!(clean_for_tts("*italic* text"), "italic text");
        assert_eq!(clean_for_tts("## Header"), "Header");
        assert_eq!(clean_for_tts("[link](http://example.com)"), "link");
        assert_eq!(clean_for_tts("`code`"), "code");
    }

    #[test]
    fn test_clean_for_tts_expands_abbreviations() {
        assert_eq!(clean_for_tts("e.g. this"), "for example this");
        assert_eq!(clean_for_tts("i.e. that"), "that is that");
    }

    #[test]
    fn test_clean_for_tts_normalizes_whitespace() {
        assert_eq!(clean_for_tts("hello   \n\n  world"), "hello world");
    }

    #[test]
    fn test_clean_for_tts_strips_lists() {
        assert_eq!(clean_for_tts("- item one\n- item two"), "item one item two");
        assert_eq!(
            clean_for_tts("1. first\n2. second"),
            "first second"
        );
    }

    #[test]
    fn test_clean_for_tts_strips_code_blocks() {
        let input = "Before\n```rust\nfn main() {}\n```\nAfter";
        assert_eq!(clean_for_tts(input), "Before After");
    }

    #[test]
    fn test_generate_msg_id_format() {
        let id = generate_msg_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_voice_say_empty_text() {
        let result = handle_voice_say(&json!({"text": ""}));
        assert!(result.get("isError").unwrap().as_bool().unwrap());
    }

    #[test]
    fn test_voice_say_returns_status() {
        let result = handle_voice_say(&json!({"text": "Hello world"}));
        let content = result.get("content").unwrap().as_array().unwrap();
        let text = content[0].get("text").unwrap().as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert!(data.get("msg_id").is_some());
        // status will be "no_listeners" or error in test (no server running)
        assert!(data.get("status").is_some() || data.get("isError").is_some());
    }
}
