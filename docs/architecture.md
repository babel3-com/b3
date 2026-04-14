# Architecture

*Your phone is a window into your terminal, with a microphone and speakers attached.*

Babel3 has three open-source components that work together to give Claude Code a voice, a dashboard, and inter-agent communication.

## System Overview

```
                       babel3.com
                      ┌─────────────┐
                      │ Auth        │
                      │ Billing     │
                      │ Tunneling   │
                      └──────┬──────┘
                             │
                             │
Your Phone ◀════════════════════════════════▶ Your Machine
┌───────────────────┐    P2P tunnel          ┌───────────────────┐
│ Terminal + file   │    (encrypted)         │ Persistent session│
│ browsing web app  │◀──────────────────────▶│ that wraps around │──▶ GPU Worker
│ with a microphone │ voice, terminal, files │ your CLI          │
└───────────────────┘                        └───────────────────┘
```

**Key principle:** Your phone connects directly to your machine via an E2E encrypted relay. babel3.com is only the signaling server — it tells your phone where your machine is, then steps out. Terminal content, voice audio, and files flow through an encrypted channel. babel3.com never sees your code or conversations.

## The Daemon (`crates/b3-cli/`)

The `b3` binary runs as both a CLI tool and a long-running daemon.

### CLI commands

| Command | What it does |
|---------|-------------|
| `b3 setup` | First-run wizard (sign up, token, agent name) |
| `b3 start` | Launch daemon + Claude Code session |
| `b3 stop` | Stop the daemon |
| `b3 attach` | Attach to a running session |
| `b3 status` | Check daemon health |
| `b3 hive` | Send messages between agents |
| `b3 update` | Self-update the binary |
| `b3 login` | Re-authenticate |

### Daemon subsystems

When `b3 start` runs, it launches these subsystems:

1. **PTY Manager** — Spawns Claude Code in a pseudo-terminal. Reads/writes bytes to the PTY master fd.

2. **Bridge** — Two components:
   - **Pusher** sends terminal output to the server every 100ms (for persistence and analytics)
   - **Puller** receives SSE events from the server and injects them into the PTY (transcriptions, hive messages, tunnel tokens)

3. **Web Server** — localhost:3100, exposed via E2E encrypted relay. Handles WebSocket terminal relay, voice (STT upload, TTS dispatch), hive message proxying, and file browsing.

4. **MCP Server** — Spawned by Claude Code as `b3 mcp voice`. Communicates over stdio using JSON-RPC. Provides 20 tools (voice, hive, browser, system).

5. **Tunnel Manager** — Manages the E2E encrypted relay tunnel. The primary tunnel starts at boot. Additional tunnels are provisioned on demand when browsers on other domains connect.

### Startup sequence

The daemon starts in numbered steps (see `daemon/server.rs`):

1. Load config from `~/.b3/config.json`
2. Register with server (`POST /api/agents/register`)
3. Register MCP in `.mcp.json` (or detect B3 plugin and skip)
4. Start primary E2E encrypted relay tunnel
   - 4d. Restore dynamic tunnels from server
5. Spawn Claude Code in PTY
6. Start bridge (pusher + puller)
7. Start web server (localhost:3100)
8. Start IPC listener (Unix socket for `b3 attach`, `b3 stop`)

## The Browser (`browser/`)

14 standalone JavaScript files + CSS. No framework, no build step. Served as static files by the server.

| File | Purpose |
|------|---------|
| `boot.js` | Initialization, config injection, version checking |
| `core.js` | Core dashboard logic, settings panel |
| `terminal.js` | xterm.js terminal (v5.5.0 + FitAddon) |
| `ws.js` | WebSocket to daemon, terminal data handling |
| `voice-record.js` | Mic capture, voice activity detection, audio upload |
| `voice-play.js` | TTS playback queue (gapless, sentence-level) |
| `tts.js` | TTS request handling |
| `led.js` | LED chromatophore — emotion to color/pattern via cosine similarity (see [PATENTS](../PATENTS)) |
| `animations.js` | Custom LED animation management |
| `layout.js` | Responsive layout, panel toggling |
| `files.js` | File browser, PDF viewer |
| `info.js` | Info panel (HTML pushed from agents) |
| `gpu.js` | GPU marketplace UI |

The browser connects to the daemon via WebSocket through E2E encrypted relay (primary) or WebRTC peer-to-peer when available (secondary). All terminal, voice, and file data flows between phone and machine over an encrypted channel.

## The GPU Worker (`gpu-worker/`)

A Docker container running all ML models on a single GPU. Same image for RunPod cloud and local development.

### Models

| Model | Purpose | VRAM |
|-------|---------|------|
| faster-whisper (medium.en / large-v3) | Speech-to-text | 2-5 GB |
| WhisperX | Word-level alignment | ~2 GB |
| Chatterbox TTS | Text-to-speech | ~3.5 GB per instance |
| PyAnnote | Speaker diarization | ~2 GB |
| nomic-embed-text-v1.5 | Semantic embeddings (768D) | ~0.5 GB |

All models are pre-downloaded into the Docker image at build time. Zero network calls at runtime. First request is as fast as any subsequent request.

### Actions

| Action | Input | Output |
|--------|-------|--------|
| `transcribe` | Audio | Text + word timestamps (dual-model — see [PATENTS](../PATENTS)) |
| `synthesize_stream` | Text + voice | Audio chunks (per sentence) |
| `synthesize` | Text + voice | Single audio WAV |
| `diarize` | Audio + word segments | Speaker labels |
| `enroll` | Speaker embeddings (.npy) | Enrollment confirmation |
| `embed` | Text strings | 768D embedding vectors |
| `compute_conditionals` | Voice WAV | Pre-baked voice identity (.pt) |

## Shared Types (`crates/b3-common/`)

Data types used by both daemon and server. The most important is `AgentEvent` — an enum with ~16 variants representing every event that flows between server and daemon:

- `Input` — Keystroke injection
- `Transcription` — Voice transcription result
- `Hive` / `HiveRoom` — Inter-agent messages
- `ConfigUpdate` — Runtime configuration changes
- `BrowserEval` — Execute JS in browser
- `PtyResize` — Terminal resize

## Data flow: Voice round-trip

```
1. You speak → browser captures audio (voice-record.js)
2. Audio uploaded to daemon via E2E encrypted relay
3. Daemon dispatches to GPU worker → Whisper transcribes
4. Daemon injects "[MIC] your-name: what you said" into PTY
5. Claude Code reads it, calls voice_say() MCP tool
6. MCP → daemon → GPU worker → Chatterbox TTS → audio chunks
7. Audio streamed to browser via WebSocket → plays on your phone
8. LED chromatophore animates based on the emotion parameter
```

Total: ~3-4 seconds from speech to first audio response.

## Privacy architecture

- **Terminal traffic:** Encrypted via E2E relay (WebSocket primary) or WebRTC peer-to-peer (when available). babel3.com never sees it.
- **Voice audio:** Processed by GPU worker (ephemeral — nothing stored). Transcripts are session-only.
- **Hive messages:** End-to-end encrypted (X25519 + ChaCha20-Poly1305). Server is a dumb encrypted blob router.
- **File browsing:** Files served directly from daemon to browser (P2P). Never touch the server.
