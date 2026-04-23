<!-- verified-against: b3-cli@0.3.224 -->

# Daemon Architecture Reference

The b3 daemon (`crates/b3-cli/`) is both a CLI tool and a long-running background process.

---

## Module Layout

```
src/
├── main.rs              # CLI entry: clap args → command dispatch
├── config.rs            # Agent config loading/saving (~/.b3/)
├── http.rs              # Shared HTTP client utilities
├── logging.rs           # Logging setup
├── service.rs           # System service installation
├── commands/
│   ├── start.rs         # b3 start — launches the daemon
│   ├── stop.rs          # b3 stop — stops the daemon
│   ├── attach.rs        # b3 attach — attach terminal to running daemon
│   ├── login.rs         # b3 login — authenticate with server
│   ├── setup.rs         # b3 setup — first-run wizard
│   ├── set_password.rs  # b3 set-password — change agent password
│   ├── status.rs        # b3 status — daemon health
│   ├── hive.rs          # b3 hive — inter-agent messaging
│   ├── update.rs        # b3 update — self-update binary
│   ├── uninstall.rs     # b3 uninstall — remove installation
│   └── version.rs       # b3 version
├── daemon/
│   ├── server.rs        # Daemon orchestrator — PTY, relay, bridge, SessionManager
│   ├── ec2_proxy.rs     # E2E encrypted relay — ECDH, reverse tunnel, multi-server registration
│   ├── web.rs           # Axum web server, SessionStore (Arc<Vec<u8>>), routes
│   ├── rtc.rs           # WebRTC peer connections (secondary transport), ICE, PeerRegistry
│   ├── gpu_client.rs    # GPU TTS client (local-first + RunPod fallback, 2s timeout)
│   ├── info_archive.rs  # Persistent info panel storage (max 1000 entries)
│   ├── tts_archive.rs   # TTS message archive for offline replay (max 50)
│   ├── protocol.rs      # IPC message protocol (length-prefixed)
│   └── ipc.rs           # Unix socket IPC listener/client
├── bridge/
│   ├── puller.rs        # Server events → PTY input (SSE), hive dedup, RTC signaling relay
│   └── mod.rs
├── pty/
│   ├── manager.rs       # PTY spawn, read, write, resize, broadcast
│   ├── attach.rs        # Client attachment to running PTY
│   └── mod.rs
├── mcp/
│   ├── voice.rs         # MCP server — 19 tools for Claude Code
│   ├── manager.rs       # Dynamic MCP server loading/unloading
│   ├── registry.rs      # Plugin registry
│   └── mod.rs
├── crypto/
│   ├── hive.rs          # X25519 ECDH, HKDF, ChaCha20-Poly1305 AEAD
│   ├── hive_integration.rs  # TOFU pinning, room key storage, encrypt/decrypt
│   └── mod.rs
└── mesh/
    ├── config.rs        # WireGuard config generation
    ├── tunnel.rs        # Userspace tunnel, TCP proxy 127.0.0.1:13000
    └── mod.rs
```

---

## Related Crates

```
crates/
├── b3-common/           # Shared types: AgentEvent (~20 variants), SessionPush
├── b3-reliable/         # Mosh-like reliability layer
│   ├── frame.rs         # Wire format (0xB3 magic, Data/ACK/Resume)
│   ├── channel.rs       # ReliableChannel (seq, CRC32, send buffer, retransmit)
│   ├── multi.rs         # MultiTransport (priority routing, failover)
│   ├── session.rs       # Session + SessionManager (per-browser isolation)
│   └── bridge.rs        # backend_to_browser() async task
├── b3-webrtc/           # WebRTC abstraction over datachannel-rs
│   ├── lib.rs           # init_safe_logging() (TLS panic fix)
│   ├── channel.rs       # Framed protocol, 16KB chunking, ChunkAssembler
│   ├── peer.rs          # B3Peer, PeerEvent, data channel management
│   └── signaling.rs     # SDP/ICE types, TURN credentials (RFC 7635)
└── b3-gpu-relay/        # GPU WebRTC sidecar (reliability relay, port 5126)
```

---

## Daemon Startup Sequence (server.rs)

1. **Rotate logs** — Rename `daemon.log` → `daemon.log.prev` (crash evidence preservation)
2. **Clean up stale IPC** — Remove old Unix socket endpoint
3. **Write PID file** — For process management
4. **Stamp version** — Write b3 version to config (enables upgrade detection)
5. **Start WireGuard tunnel** — Userspace tunnel, write proxy port to `~/.b3/mesh-proxy.port`
6. **Verify API reachable** — Direct HTTPS to babel3.com (5 retries, 1s delay)
7. **Auto-provision relay session** — POST `/api/agents/{id}/provision-relay` if missing
8. **Register MCP** — Check if B3 plugin installed (reads `~/.claude/plugins/installed_plugins.json`). If plugin found: skip. Else: write `b3` entry to `.mcp.json` (non-destructive, backup original)
9. **Spawn PTY** — Claude Code in pseudo-terminal
10. **Start puller** — SSE events from EC2, hive dedup, RTC signaling relay
    - Spawn hive polling fallback (every 30s safety net)
11. **Create WebState with SessionManager** — SessionStore, EventChannel, PeerRegistry, TtsArchive, InfoArchive, GpuClient, SessionManager
12. **Populate lazy_web_state** — For puller access to web state
13. **Spawn local_pusher** — PTY output → SessionStore (zero network hop, replaces HTTP delta pushes)
14. **Spawn local_puller** — WebSocket keystrokes → PTY stdin
15. **Start web server** — Axum routes: dashboard, API, WebSocket
16. **Start IPC listener** — Unix socket for attach/detach/stop
17. **On attach** — Forward PTY output to client
18. **On detach** — PTY keeps running
19. **On stop** — Kill PTY, close tunnel, clean up

---

## WebRTC (daemon/rtc.rs)

Handles peer-to-peer data channels between browser and daemon.

**Three data channels per connection:**
- `"control"` (ordered, reliable) — JSON RPC, auth, control messages
- `"terminal"` (ordered, reliable) — Raw PTY bytes, bidirectional
- `"audio"` (ordered, reliable) — TTS WAV chunks

**Key types:**
- `PeerRegistry` — Maps session_id to `PeerEntry` (ICE sender channel + task handle + browser_session_id). On new offer from same browser: abort old peer, create new one.
- `IceCandidate` — candidate string + mid

**Functions:**
- `handle_offer(web_state, session_id, sdp, peer_registry)` → SDP answer
- `add_ice_candidate(registry, session_id, candidate, mid)` — Trickle ICE inject
- `handle_control_message()` — Auth, RPC routing to web handlers
- `handle_rpc()` — Routes to init-bundle, tts, inject, info, browser-console

**ICE configuration:**
- STUN: `stun:stun.l.google.com:19302` (env override: `STUN_URL`)
- TURN: Direct IP (env: `TURN_HOST`, `TURN_SECRET`)
- TURN credentials: RFC 7635 HMAC-SHA1, colons URL-encoded as `%3A`

---

## SessionManager Integration (daemon/web.rs)

**WebState** holds `session_manager: Arc<SessionManager>` for per-browser isolation.

**Per-session state:**
- Own `MultiTransport` (seq counter, send buffer, priority routing)
- Own `sent_offset` (per-client delta tracking into SessionStore)
- Own `rtc_active` flag
- Own bridge task (backend → browser via MultiTransport)

**On WebSocket connect:** `reconnect_or_create(client_id)` — reuses existing session or creates new one, preserving send buffer on reconnect.

**On WebRTC offer:** Links to session via `browser_session_id`, adds RTC as transport sink (priority 0).

**SessionStore** uses `Arc<Vec<u8>>` — raw bytes, no base64 encoding churn. Base64 only on wire for HTTP delta pushes.

---

## GPU Client (daemon/gpu_client.rs)

Submits TTS jobs to GPU workers with local-first routing.

**Routing:**
1. POST `/run` to local GPU (2s timeout)
2. If timeout or no local GPU configured → RunPod fallback
3. Poll GET `/stream/{jobId}` for audio chunks

**Constants:**
- `CACHE_TTL = 300s` — voice conditionals cache lifetime
- `LOCAL_TIMEOUT = 2s` — local GPU timeout before RunPod fallback

---

## Bridge: Puller (bridge/puller.rs)

Receives server events and injects into the PTY:

```
SSE /api/agents/{id}/events → parse AgentEvent → format → write to PTY stdin
```

**Key types:**
- `LazyWebState` — `Arc<RwLock<Option<WebState>>>` — deferred initialization, set after web server starts
- `HiveDedup` — 200-entry ring buffer for deduplicating hive messages (fingerprint-based: sender + message prefix)

**Event types handled:**
- `input` — Keystroke injection
- `transcription` — Voice transcription → `[MIC] speaker: text`
- `hive` — Inter-agent message → `[HIVE from=name] message`
- `hive_room` — Room message → `[HIVE-CONVERSATION] [topic] from=name: message`
- `browser_eval` — Execute JS in browser, return result
- `config_update` — Runtime config changes
- `resize` — Terminal resize
- `webrtc_offer` — SDP offer from browser (relayed via EC2). Routes to daemon's `rtc::handle_offer` or forwards to GPU sidecar based on `target` field
- `webrtc_answer` — SDP answer delivery (with `origin_url` for cross-server routing)
- `webrtc_ice` — Trickle ICE candidate → forwarded to PeerRegistry via mpsc channel

**Resilience:**
- Auto-reconnect on SSE disconnect (1s–30s exponential backoff)
- Safety net: `HIVE_POLL_INTERVAL = 30s` polls for missed SSE events
- Max permanent failures: 10 consecutive auth errors (401/403)

---

## Web Server (daemon/web.rs)

Local Axum server on agent-specific port.

**Key routes:**
- WebSocket terminal relay (`/api/ws`)
- Voice: audio upload → STT, TTS synthesis, audio delivery
- Hive: message sending/receiving (proxied to EC2)
- File browser: directory listing, file reading, PDF serving
- Browser console + eval endpoints
- Health and status endpoints

**Key types:**
- `WebState` — Shared web server state (SessionManager, PeerRegistry, EventChannel, GpuClient, archives)
- `SessionStore` — `Arc<Vec<u8>>` ring buffer of raw PTY output bytes
- `EventChannel` — Broadcast channel for browser events

---

## MCP Server (mcp/voice.rs)

Spawned by Claude Code as `b3 mcp voice`. Communicates over stdio using MCP JSON-RPC protocol.

**Port discovery:** Checks `B3_WEB_PORT` env var → agent config file → default 3100.

**Authentication:** Reads API key from `~/.b3/config.json` for daemon web server auth.

**19 tools:** voice_say, voice_status, voice_health, voice_logs, voice_share_info, voice_enroll_speakers, animation_add, email_draft, browser_console, browser_eval, hive_send, hive_status, hive_room_send, hive_room_messages, hive_room_create, hive_room_destroy, hive_room_list, hive_messages, restart_session.

---

## Hive Encryption (crypto/)

**DM encryption:** X25519 ECDH key agreement → HKDF key derivation → ChaCha20-Poly1305 AEAD. AAD is `sender:target` pair. TOFU public key pinning cached in `~/.b3/pinned-keys/`.

**Conversation rooms:** Room key stored at `~/.b3/room-keys/{room_id}.b64` (0o600 perms). Mandatory expiration on creation. Any member can destroy the key (irreversible).

**v1 limitations:** Per-message key fetch (TOFU cache mitigates), no replay window for DMs, room keys plaintext on disk.

---

## Configuration

```
~/.b3/
├── config.json          # Agent identity, API key, server URL, tunnel token
├── pinned-keys/         # TOFU public key cache
├── room-keys/           # Conversation room encryption keys
└── agents/
    └── <slug>/
        └── dashboard.port  # Per-agent web server port
```

---

## Key Data Types (b3-common)

### AgentEvent (~20 variants)

```rust
enum AgentEvent {
    Transcription { text },
    Hive { sender, text },
    PtyInput { data },
    Tts { text, msg_id, voice },
    TtsGenerating { msg_id, chunk, total },
    TtsAudio { msg_id, chunk, total, url },
    TtsAudioData { msg_id, chunk, total, audio_b64, duration_sec, generation_sec },
    TtsStream { msg_id, text, voice, emotion, split_chars, min_chunk_chars, conditionals_b64 },
    SessionUpdated { size },
    Led { emotion },
    PtyResize { rows, cols },
    HiveRoom { sender, room_id, room_topic, text },
    UpdateAvailable { version, url, sha256 },
    Notification { title, body, level, action? },
    BrowserEval { eval_id, code },
    WebRtcOffer { session_id, sdp, target?, origin_url? },
    WebRtcAnswer { session_id, sdp, agent_id? },
    WebRtcIce { session_id, candidate, mid },
    // ... additional variants
}
```
