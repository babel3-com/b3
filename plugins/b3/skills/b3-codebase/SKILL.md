---
name: b3
description: This skill should be used when the user is "using voice with Babel3", "talking to my agent", "what can say do", "how do I use voice_say", "how do I send messages between agents", "what MCP tools are available", "developing the Babel3 daemon", "working on the b3 CLI", "modifying voice pipeline code", "adding MCP tools", "working on browser JS", "modifying the GPU worker", "debugging voice issues", "working on hive messaging", "setting up Babel3 development", "WebRTC data channels", "GPU sidecar", "reliability layer", "multi-transport", "session manager", "app proxy", "build a browser app", "developing a browser app", "/app/a/ route", "http_request frame", "proxy through tunnel", "daemon web server", "child agent", "spawn child", "b3 child spawn", "b3 child list", "b3 child kill", "b3 start --detached", "fork_state", "OnceLock", "child daemon", or approaching the Babel3 open-source codebase for the first time. Provides a quick-start guide for using B3 tools, architecture overview, development guide for the open-source daemon, browser UI, and GPU worker, app proxy guide for browser app development, and child agent CLI reference.
version: 0.3.0
---

<!-- verified-against: b3-cli@0.3.224, b3-reliable@0.1.0, b3-webrtc@0.1.0, gpu-worker v64 -->

# Babel3 — Give Your Agent a Voice

Babel3 gives Claude Code sessions a persistent identity, voice I/O, a web dashboard, and a way to talk to other agents. This skill covers the open-source components (Apache 2.0).

## Quick Start

Speak to the user with `say(text="Hello!", emotion="warm joy", replying_to="user's words")`. Message another agent with `hive_send(target="vera", message="Check the wiki")`. Read the browser console with `browser_console(filter="energy")`. Run JavaScript in the dashboard with `browser_eval(code="isListening")`. Add a custom LED animation with `animation_add(name="pulse", description="calm focus", file_path="animations/pulse.js")`.

19 MCP tools are available across four categories:

- **Voice** (6 tools) — Speak, check pipeline health, read logs, enroll speakers, share info/images
- **Browser** (2 tools) — Read console, execute JS
- **Hive** (8 tools) — Send DMs, create encrypted conversation rooms, list agents, read history. Supports forward secrecy, timelock delivery, and mandatory key expiration.
- **System** (3 tools) — Add LED animations, draft supervised emails, restart daemon

See `references/mcp-tools.md` for full parameter schemas, usage examples, and error handling.

## Prerequisites

- **Voice tools** require a running daemon (`b3 start`)
- **Speaker diarization** requires `RUNPOD_GPU_ID` and `RUNPOD_API_KEY` environment variables
- **Hive messaging** requires at least two agents registered to the same user
- **Browser tools** require a connected browser session at the dashboard URL

## Architecture

```
User's Machine                          Server (babel3.com)
┌──────────────────────────┐           ┌──────────────────────┐
│  b3 daemon               │ HTTPS/SSE │  b3-server (Axum)    │
│  ├── PTY Manager         │◄─────────►│  ├── Sessions        │
│  ├── SessionManager      │           │  ├── Auth / Credits   │
│  │   └─ Session (per     │           │  ├── Voice dispatch   │
│  │      browser)         │           │  ├── Hive messaging   │
│  │      ├─ MultiTransport│           │  ├── WebRTC signaling │
│  │      ├─ ReliableChannel           │  └── PostgreSQL       │
│  │      └─ Bridge task   │           └──────────────────────┘
│  ├── Web Server          │                    │
│  ├── Voice MCP (19 tools)│                    │ HTTP
│  ├── Crypto (Hive E2E)   │                    │
│  └── EC2 Proxy           │                    ▼
└──────────────────────────┘           ┌──────────────────────┐
     │                                 │  GPU Worker (Docker)  │
     │ Dual transport:                 │  ├── handler.py       │
     │  ┌ WebSocket (EC2 relay)        │  ├── local_server.py  │
     │  └ WebRTC (P2P/TURN)           │  └── b3-gpu-relay     │
     │    Reliability layer            │      (WebRTC sidecar) │
     │    (seq + CRC + ACK)            └──────────────────────┘
     ▼
┌──────────────────────────┐
│  Browser/Phone           │
│  ├── reliable.js         │  Matching wire format
│  ├── rtc.js              │  (0xB3 magic, CRC32, ACK/Resume)
│  ├── ws.js               │
│  ├── terminal.js         │
│  ├── voice-record.js     │
│  ├── voice-play.js       │
│  └── 10 more JS modules  │
└──────────────────────────┘
```

**Dual transport:** Every session runs two paths simultaneously — WebSocket through E2E encrypted relay (primary, priority 1) and WebRTC data channel peer-to-peer when available (secondary, priority 0). `MultiTransport` routes to the best available. If WebRTC is unavailable, WebSocket takes over. When WebRTC connects, it provides lower-latency delivery.

**Reliability layer:** `b3-reliable` crate wraps both transports with sequence numbers, CRC32 checksums, ACK/Resume protocol, fast retransmit, and priority tagging. Terminal deltas and voice audio are Critical (replayed on reconnect). LED animations are BestEffort (skipped on replay). Browser counterpart: `reliable.js` with identical wire format.

**Per-session isolation:** `SessionManager` creates a `Session` per browser window. Each session has its own `MultiTransport`, `ReliableChannel`, bridge task, and delta offset. Multiple browsers never interfere with each other.

## Developing the Codebase

### Crate structure

```
open-source/
├── crates/
│   ├── b3-cli/          # Daemon + CLI (the main binary)
│   │   └── src/
│   │       ├── main.rs, config.rs, http.rs, logging.rs, service.rs
│   │       ├── daemon/
│   │       │   ├── server.rs    # Startup sequence (steps 0-7 with sub-steps)
│   │       │   ├── ec2_proxy.rs # E2E encrypted relay — ECDH, reverse tunnel, multi-server
│   │       │   ├── web.rs       # Axum web server, SessionStore, routes
│   │       │   ├── rtc.rs       # WebRTC peer management, PeerRegistry
│   │       │   ├── gpu_client.rs, info_archive.rs, tts_archive.rs
│   │       │   ├── protocol.rs  # IPC wire format
│   │       │   └── ipc.rs       # Unix socket IPC
│   │       ├── bridge/
│   │       │   └── puller.rs    # SSE events, hive dedup, RTC signaling relay
│   │       ├── mcp/
│   │       │   ├── voice.rs     # 19 MCP tools for Claude Code
│   │       │   ├── manager.rs   # Runtime MCP load/unload
│   │       │   └── registry.rs  # Plugin registry
│   │       ├── crypto/
│   │       │   ├── hive.rs              # X25519 ECDH, ChaCha20-Poly1305
│   │       │   └── hive_integration.rs  # TOFU pinning, room key storage
│   │       ├── pty/
│   │       │   ├── manager.rs   # PTY spawn, broadcast, resize
│   │       │   └── attach.rs    # Client attachment
│   │       ├── mesh/
│   │       │   ├── config.rs    # WireGuard config generation
│   │       │   └── tunnel.rs    # Userspace tunnel, TCP proxy
│   │       └── commands/        # 11 CLI command handlers
│   ├── b3-common/       # Shared types (AgentEvent ~20 variants, SessionPush)
│   ├── b3-reliable/     # Mosh-like reliability layer
│   │   └── src/
│   │       ├── frame.rs     # Wire format: Data/ACK/Resume, CRC32
│   │       ├── channel.rs   # ReliableChannel: send buffer, reorder, chunks
│   │       ├── multi.rs     # MultiTransport: priority routing, failover
│   │       ├── session.rs   # Session + SessionManager: per-browser isolation
│   │       └── bridge.rs    # backend_to_browser() async task
│   ├── b3-webrtc/       # WebRTC abstraction (datachannel-rs wrapper)
│   │   └── src/
│   │       ├── lib.rs       # init_safe_logging() — TLS panic fix
│   │       ├── channel.rs   # Framed protocol, 16KB chunking, ChunkAssembler
│   │       ├── peer.rs      # B3Peer, PeerEvent, PeerConfig
│   │       └── signaling.rs # SDP/ICE types, TURN credential generation
│   └── b3-gpu-relay/    # GPU WebRTC sidecar (reliability relay)
├── browser/js/          # 16 standalone JS modules + 8 animation patterns
├── gpu-worker/          # Docker image: Whisper + Chatterbox + PyAnnote
└── plugins/b3/          # This plugin
```

### When modifying the daemon

Edit files in `crates/b3-cli/src/`. The daemon startup sequence lives in `daemon/server.rs` — steps 0-7 (with sub-steps) including SessionManager initialization, PTY spawning, EC2 relay setup, and local pusher/puller tasks.

**Key modules:**

| Module | When to edit |
|--------|-------------|
| `daemon/web.rs` | Routes, SessionStore, WebSocket handler, TTS streaming |
| `daemon/rtc.rs` | WebRTC peer connections, ICE handling, PeerRegistry, data channel bridging |
| `daemon/server.rs` | Startup sequence, WebState initialization |
| `bridge/puller.rs` | SSE event handling, hive dedup, RTC signaling relay |
| `mcp/voice.rs` | Adding/modifying MCP tools — add in `tool_definitions()` and the handler `match` block |
| `crypto/hive_integration.rs` | Hive encryption, TOFU key pinning, room key management |

**New since v0.1.1014:**
- `daemon/rtc.rs` — WebRTC peer connections, three data channels (control, terminal, audio), PeerRegistry
- `crypto/hive_integration.rs` — TOFU public key pinning, room key disk storage, encrypt/decrypt workflows
- `SessionManager` integration in `daemon/web.rs` — per-browser session isolation
- `SessionStore` refactored to `Arc<Vec<u8>>` — raw bytes, no base64 encoding churn
- Local pusher/puller tasks replace HTTP delta pushes

### When modifying the reliability layer

Edit files in `crates/b3-reliable/src/`. See `references/reliability-layer.md` for the full API reference.

| File | Purpose |
|------|---------|
| `frame.rs` | Wire format encode/decode — magic `0xB3`, Data/ACK/Resume frames, CRC32 |
| `channel.rs` | ReliableChannel — send buffer (max 1000), reorder buffer, auto-chunking (>32KB), fast retransmit (3 dup ACKs) |
| `multi.rs` | MultiTransport — priority-sorted transport sinks, hot-add/remove, automatic failover, keepalive |
| `session.rs` | Session + SessionManager — per-browser isolation, reconnect_or_create(), bridge lifecycle |
| `bridge.rs` | backend_to_browser() — async loop with 200ms tick (ACK) and 5s keepalive |

### When modifying WebRTC transport

Edit files in `crates/b3-webrtc/src/`. See `references/webrtc-transport.md` for the full API reference.

| File | Purpose |
|------|---------|
| `lib.rs` | `init_safe_logging()` — TLS panic fix (suppress libdatachannel C++ callbacks) |
| `channel.rs` | Framed message protocol, 16KB auto-chunking (SCTP limit), ChunkAssembler, ChannelSender/Receiver |
| `peer.rs` | B3Peer — RtcPeerConnection wrapper, event-driven async interface, zombie detection |
| `signaling.rs` | SDP/ICE types, TURN credential generation (RFC 7635), SignalingTarget (Daemon/GpuWorker) |

### When modifying dashboard JavaScript

Edit standalone files in `browser/js/`. Each file is a self-contained module. All depend on `core.js` first; `boot.js` loads last.

| File | Purpose |
|------|---------|
| `core.js` | Shared HC namespace, config, audio lock (Safari AVAudioSession deadlock prevention) |
| `reliable.js` | Browser reliability layer — identical wire format to b3-reliable (0xB3, CRC32, ACK/Resume) |
| `boot.js` | Dashboard init, EC2 signaling, daemon connection, voice picker, agent settings |
| `ws.js` | WebSocket to daemon via E2E encrypted relay, terminal data routing, password auth |
| `rtc.js` | WebRTC data channels (secondary transport, P2P when available) |
| `gpu.js` | GPU credential fetching, local GPU health, job submission (local-first → RunPod) |
| `voice-record.js` | Continuous recording, silence detection, streaming STT, garbage filtering, diarization |
| `voice-play.js` | GaplessStreamPlayer for zero-gap TTS via Web Audio API + HTML audio fallback (iOS Chrome) |
| `tts.js` | Serial TTS queue, streaming playback, history/replay, mini player UI |
| `terminal.js` | xterm.js terminal, keyboard input, copy/paste, touch scroll, virtual keyboard resize |
| `layout.js` | Configurable fullscreen layout, show/hide UI elements, persist to EC2 settings |
| `led.js` | 300-LED strip canvas renderer, emotion-driven color mapping, animation registration |
| `animations.js` | Animation manager, built-in + custom animations, toggle enable/disable |
| `files.js` | File browser with Shiki syntax highlighting, CodeMirror 6 editing, markdown preview |
| `daemon-ws.js` | WebSocket connection to daemon via relay, reconnection, auth handshake |
| `info.js` | HTML info panel (agent-pushed via voice_share_info), history persisted by daemon |

**Animation patterns** in `browser/js/animations/`: solid, breathing, wave, chase, sparkle, fire, gradient, aurora (8 patterns).

### When modifying the GPU worker

Edit `gpu-worker/handler.py` for ML actions, `gpu-worker/local_server.py` for the FastAPI server.

**10 ML actions** (4 billable):

| Action | Type | Billable | Purpose |
|--------|------|----------|---------|
| `transcribe` | Generator | Yes | STT via faster-whisper |
| `diarize` | Generator | Yes | Speaker ID via WhisperX + PyAnnote |
| `synthesize` | Regular | Yes | TTS full audio |
| `synthesize_stream` | Generator | Yes | TTS chunked streaming |
| `wake` | Generator | No | Wake word detection |
| `embed` | Regular | No | Semantic embeddings (nomic-embed-text-v1.5, 768D) |
| `enroll` | Regular | No | Speaker embedding enrollment |
| `compute_conditionals` | Regular | No | Voice conditional synthesis |
| `extract_voice` | Regular | No | Audio source extraction/cleanup |
| `logs` | Regular | No | GPU worker log retrieval |

**GPU relay sidecar** (`crates/b3-gpu-relay/`): Rust binary at port 5126 that wraps the GPU worker WebSocket with `b3-reliable` for per-browser session multiplexing and msg_id-based response routing.

Build with: `cd gpu-worker && ./build.sh v55`

### How MCP registration works

MCP registration is handled directly by `b3 setup` and `b3 update` — no Claude Code plugin required. `register_voice_mcp()` writes `{"mcpServers": {"b3": {"command": "b3", "args": ["mcp", "voice"]}}}` into the project's `.mcp.json`. Skills ship embedded in the `b3` binary and are extracted to `~/.claude/skills/` where Claude Code auto-discovers them.

### Building

```bash
cargo build -p b3-cli                    # Debug build
cargo build -p b3-cli --release          # Release build (use --profile ci on WSL)
cargo build -p b3-gpu-relay --release    # GPU relay sidecar
cargo test                               # Run tests (b3-reliable has comprehensive suite)
cargo run -p b3-cli -- start             # Run daemon
```

## Troubleshooting

- **say returns "no_listeners"** — No browser is connected. Open your dashboard URL.
- **Voice sounds robotic or cuts off** — GPU worker may be overloaded. Check `voice_health()`.
- **"Connection refused" on any tool** — Daemon isn't running. Run `b3 start`.
- **WebRTC not connecting** — Check TURN server reachability. Browser console: `browser_console(filter="ICE")`.
- **Terminal freezes on cellular** — Reliability layer should handle this. Check `browser_console(filter="reliable")` for ACK/Resume activity.
- **Streaming STT drops transcription** — Check garbage filter threshold. Browser console: `browser_console(filter="Filtered")`.
- **TLS panic / delta freeze** — The `init_safe_logging()` fix in b3-webrtc should prevent this. If it recurs, check that `B3Peer::new()` calls `init_safe_logging()` immediately after peer creation.

## Known Limitations

This skill covers the open-source components: daemon, browser UI, GPU worker, reliability crates, WebRTC crates, and shared types. It has limited knowledge of the proprietary server (`b3-server`). For server-side architecture, route maps, and database schemas, refer to the b3-dev internal plugin.

## Additional Resources

### Reference Files

- **`references/mcp-tools.md`** — Full parameter schemas, usage examples, and error handling for all 19 MCP tools
- **`references/daemon-architecture.md`** — Daemon module layout, startup sequence, WebRTC, bridge internals, MCP server lifecycle
- **`references/gpu-worker.md`** — GPU actions, Chatterbox pool, transcription pipeline, WebRTC sidecar, VRAM budget, Docker image
- **`references/reliability-layer.md`** — b3-reliable wire format, ReliableChannel/MultiTransport/SessionManager API, auto-chunking, reconnection scenarios, browser counterpart
- **`references/webrtc-transport.md`** — b3-webrtc API, B3Peer, data channel framing, signaling flow, TURN credentials, TLS panic fix
- **`references/app-proxy.md`** — App proxy guide for browser app developers: how the daemon handles `http_request` frames in `ec2_proxy.rs`, base64 body encoding, `/app/a/` URL scheme, `--app`/`--app-public` flags, building and iterating on a browser app via the tunnel
- **`references/child-agents.md`** — Child agent CLI (`b3 child spawn/list/kill`), `b3 start --detached`, `fork_state.rs` OnceLock statics (replacing B3_* env vars), hive ACL scoping for child agents

### Examples

- **`examples/voice-round-trip.md`** — Complete trace: user speaks → STT → Claude → TTS → audio plays (dual transport with reliability layer)

## Patent Notice

Babel3 includes patent-pending technology licensed under Apache 2.0 (royalty-free patent grant). See [PATENTS](../../../../PATENTS) for details.

- Chromatophore emotion-to-animation dispatch — *Patent pending: US 64/010,742*
- Dual-model transcription with disagreement preservation — *Patent pending: US 64/010,891*
- Embedding-based rejection databases with collaborative growth — *Patent pending: US 64/011,207*

---

This skill is part of the Babel3 plugin, distributed via the [babel3-com/b3-plugins](https://github.com/babel3-com/b3-plugins) marketplace. Run `claude plugin marketplace list` to browse available skills.
