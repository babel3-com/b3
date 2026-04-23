<!-- verified-against: b3-cli@0.1.1409, b3-reliable@0.1.0 -->

# Example: Voice Round-Trip (Dual Transport + Reliability Layer)

What happens when a user says "Hello" and Claude responds with speech. Shows both transport paths and the reliability layer that wraps them.

## The Flow

```
1. CAPTURE — Browser captures audio (voice-record.js)
   └── Audio available for transcription

2. STT — Audio sent to GPU for transcription
   ├── Primary: WebRTC data channel → b3-gpu-relay sidecar (port 5126)
   │   └── Sidecar wraps with b3-reliable, bridges to local_server.py
   └── Fallback: HTTP POST to GPU worker (local 2s timeout → RunPod)
   └── GPU: faster-whisper + WhisperX → transcription + garbage filter

3. INJECT — Transcription injected into daemon PTY
   ├── Primary: WebRTC control channel → daemon/rtc.rs → PTY stdin
   └── Fallback: WebSocket → daemon/web.rs → PTY stdin
   └── Claude sees: [MIC] alice: Hello

4. RESPOND — Claude calls voice_say() MCP tool (mcp/voice.rs)
   └── MCP → daemon web server → TtsStream event

5. DELIVER — TtsStream event reaches browser
   ├── ReliableChannel wraps event (seq + CRC32 + Critical priority)
   ├── MultiTransport routes to best sink:
   │   ├── WebRTC data channel (priority 0, preferred) → browser
   │   └── WebSocket via EC2 relay (priority 1, fallback) → browser
   └── Browser ACKs received frames (every 100ms)

6. TTS — Browser dispatches to GPU (gpu.js)
   ├── Text chunked at sentence boundaries
   ├── Each chunk → GPU worker (Chatterbox TTS)
   └── Audio streamed back as chunks complete

7. PLAY — Browser plays audio (voice-play.js)
   ├── GaplessStreamPlayer (Web Audio API, zero-gap)
   ├── iOS Chrome: HTML <audio> fallback
   └── LED chromatophore animates based on emotion (led.js)
```

## Reliability Layer in Action

The reliability layer wraps steps 5-7 (daemon → browser delivery):

```
Daemon                              Browser
┌─────────────┐                    ┌─────────────┐
│ voice_say() │                    │ reliable.js  │
│      │      │                    │      │       │
│      ▼      │                    │      ▼       │
│ ReliableChannel                  │ ReliableChannel
│  seq=4217   │    0xB3 frame      │  ACK 4217    │
│  CRC32      │ ──────────────►    │  deliver     │
│  Critical   │                    │      │       │
│             │    ACK frame       │      ▼       │
│  evict 4217 │ ◄──────────────    │ TTS dispatch │
└─────────────┘                    └─────────────┘
```

**If the transport drops mid-delivery:**

```
Daemon sends seq 4217, 4218, 4219 over WebRTC
WebRTC dies at seq 4218 (cellular handoff)
  │
  ├── MultiTransport detects dead sink (15s heartbeat timeout)
  ├── Removes WebRTC, routes to WebSocket automatically
  ├── Browser reconnects WebRTC moments later
  ├── Browser sends Resume(last_ack=4217)
  ├── Daemon replays seq 4218, 4219 (Critical) over new WebRTC
  └── Browser receives, plays audio — no data lost
```

## Timeline (WebRTC Path)

```
0.0s    User says "Hello"
0.8s    VAD detects end of speech
0.9s    Audio sent over WebRTC data channel to GPU sidecar
1.5s    GPU returns transcription (peer-to-peer, no server hop)
1.6s    "[MIC] alice: Hello" injected into PTY
2.2s    Claude calls voice_say()
2.3s    TtsStream wrapped by ReliableChannel (seq, CRC32, Critical)
2.4s    MultiTransport routes via WebRTC (priority 0)
3.0s    First sentence audio arrives at browser
3.1s    Browser plays: "Hey! I can hear you."
3.2s    Browser ACKs → daemon evicts from send buffer
```

## Streaming STT (Long Recordings)

For recordings longer than ~20 seconds, the browser streams partial transcriptions:

```
0.0s     Recording starts, audio chunks sent continuously
20.0s    Segment boundary: browser finds quiet point (3s search zone)
20.5s    First segment sent to GPU for transcription
21.5s    Partial result injected as marquee text in voice bar
22.0s    Recording continues, new audio accumulates
40.0s    Second segment boundary
42.0s    Second partial injected
...
N.0s     User stops recording
N.1s     Finalize: remaining audio sent with expected_chunks count
N+1.0s   GPU waits for all chunks, transcribes final segment
N+1.5s   Full transcription assembled + garbage filter applied
N+2.0s   "[MIC] alice: <full transcription>" injected into PTY
```

**Key details:**
- `SEGMENT_TARGET_SEC = 20` — target segment length
- `SEARCH_ZONE_SEC = 3` — how far to search for a quiet point
- Chunk-count synchronization prevents race conditions on finalize
- Garbage filter (embedding-based) removes noise transcriptions on both short and streaming paths
- Transport locking prevents mixed WS/RTC corruption during a single stream

## GPU Routing

```
voice_say() called
    │
    ▼
Daemon sends TtsStream event to browser
    │ (wrapped by ReliableChannel, routed by MultiTransport)
    ▼
Browser checks GPU WebRTC connection
    ├── Connected? → Send over data channel to b3-gpu-relay
    │                 (relay wraps with b3-reliable per-session)
    └── Not connected? → HTTP POST to GPU worker
                          (local 2s timeout → RunPod fallback)
```

**Key:** Audio never routes through the EC2 relay in plaintext. All voice traffic is E2E encrypted: browser ↔ daemon (via WebRTC or encrypted relay) and browser ↔ GPU (via WebRTC sidecar or direct HTTP). The EC2 relay is a signaling relay for WebRTC offers/answers/ICE candidates and a forwarding relay for encrypted WebSocket frames — it never sees plaintext.

## Transport Priority

| Transport | Priority | Latency | Reliability |
|-----------|----------|---------|-------------|
| WebRTC data channel | 0 (preferred) | Low (P2P) | b3-reliable wraps |
| WebSocket via EC2 relay | 1 (fallback) | Medium (relay hop) | b3-reliable wraps |

Both are wrapped by the same `ReliableChannel` with continuous sequence numbers. A frame sent as seq 100 over WebRTC and replayed as seq 100 over WebSocket after reconnect is deduplicated by the browser.
