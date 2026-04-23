<!-- verified-against: gpu-worker v54, b3-gpu-rtc -->
<!-- Patent notice: Dual-model transcription pipeline (US 64/010,891), embedding-based rejection (US 64/011,207). Licensed under Apache 2.0 with royalty-free patent grant. See PATENTS. -->

# GPU Worker Reference

The GPU worker (`gpu-worker/`) is a unified Docker container running all ML models on a single GPU. The same image runs on RunPod cloud (serverless) and locally on WSL.

---

## Architecture

```
gpu-worker container (32.2GB Docker image)
├── entrypoint.sh
│   ├── Start b3-gpu-rtc sidecar (WebRTC, port 5126)
│   ├── Optional: relay tunnel (if GPU_TUNNEL_TOKEN set)
│   ├── RUNPOD_POD_ID set? → handler.py (runpod.serverless.start)
│   └── else → local_server.py (FastAPI on port 5125)
├── handler.py          # All GPU actions (8 handlers)
├── local_server.py     # FastAPI + WebSocket wrapper
├── b3-gpu-rtc          # Rust WebRTC sidecar binary
├── /voices/            # WAV reference samples (mounted :ro)
└── /voices/.cache/     # Pre-baked .pt conditionals
```

**Platform detection:** `RUNPOD_POD_ID` or `RUNPOD_ENDPOINT_ID` → RunPod cloud mode (handler.py). Neither → local GPU mode (local_server.py).

---

## Actions (8 handlers in handler.py)

### Generator handlers (yield progress, then result)

| Action | Input | Output | Model |
|--------|-------|--------|-------|
| `transcribe` | audio_b64, language, preset, skip_align? | text, segments, words | faster-whisper + WhisperX |
| `diarize` | audio_b64, word_segments, agent_id, speakers? | speaker_segments, speaker_map | PyAnnote |
| `synthesize_stream` | text, voice, conditionals_b64?, TTS params, split_chars | audio_b64 chunks (per sentence) | Chatterbox TTS |

### Sync handlers (return single result)

| Action | Input | Output | Model |
|--------|-------|--------|-------|
| `synthesize` | text, voice, conditionals_b64?, TTS params | audio_b64 (WAV) | Chatterbox TTS |
| `compute_conditionals` | wav_b64, exaggeration | conditionals_b64 (.pt) | Chatterbox |
| `embed` | texts[], task_type? | embeddings[] (768D) | nomic-embed-text-v1.5 |
| `enroll` | agent_id, speakers (name → npy_b64) | enrolled count | PyAnnote |
| `extract_voice` | url (YouTube) | conditionals_b64, wav_b64 | yt-dlp + Chatterbox |

---

## Transcription Pipeline (Dual Model)

Two models run in parallel via `ThreadPoolExecutor(max_workers=2)`:

```
Audio (WebM/MP3/WAV)
├── Thread 1: faster-whisper (WHAT — content)
│   medium.en (fast) or large-v3 (quality)
│   → segments with timestamps
└── Thread 2: WhisperX (word alignment + similarity check)
    → words with precise timestamps
```

**Presets:**

| Preset | faster-whisper | WhisperX | VRAM |
|--------|---------------|----------|------|
| `english-fast` (default) | medium.en | small.en | ~3 GB |
| `english-quality` | large-v3 | medium.en | ~6 GB |
| `multilingual` | large-v3 | large-v3-turbo | ~7 GB |

**Parameters:**
- `language` — ISO language code (default: "en")
- `skip_align` — Skip WhisperX alignment stage (saves 30-50% time when diarization off)
- `speakers` — Inline speaker enrollment: dict of `{name: base64_emb_npy}`

---

## TTS: Chatterbox Pool

**Problem:** Chatterbox model instances have mutable state (`model.conds`). Concurrent calls with different voices corrupt output.

**Solution:** Pool of isolated instances, each with its own `threading.Lock()`:

```python
_chatterbox_pool = []  # List of (model, lock) tuples

def acquire_chatterbox():
    idx = _pool_index % len(_chatterbox_pool)
    _pool_index += 1
    return _chatterbox_pool[idx]
```

**Pool size:** Auto-detected: `(VRAM - 4GB) / 3.5GB / 2`, max 6.
- RunPod 48GB A40: 3 instances
- Local 24GB 4090: 1 instance

**TTS Parameters** (chatterbox-tts ≥0.1.6):

| Parameter | Default | Description |
|-----------|---------|-------------|
| `temperature` | 0.8 | Sampling randomness |
| `repetition_penalty` | 1.2 | Penalize repeated tokens |
| `min_p` | 0.05 | Minimum probability threshold |
| `top_p` | 1.0 | Nucleus sampling |
| `exaggeration` | 0.5 | Voice expressiveness (0-1) |
| `cfg_weight` | 0.5 | Classifier-free guidance weight |

**Streaming TTS** (`synthesize_stream`):
1. Split text at sentence boundaries (`split_chars`, default: `.!?`)
2. Merge short fragments (below `min_chunk_chars`, default: 80)
3. Generate each sentence independently
4. Yield audio chunks as they complete — low latency on first chunk
5. Priority queue: lower chunk_index = higher priority (first sentences generated first)

**Voice Conditionals** (`.pt` files):
- Pre-computed Chatterbox embeddings (voice identity + prosody)
- Load in <100ms vs ~2s from WAV
- Fallback chain: `conditionals_b64` param → cached `.pt` → compute from `.wav` → `zara.wav`

---

## WebRTC Sidecar (b3-gpu-rtc)

Rust binary that bridges browser WebRTC data channels to the GPU's HTTP/WS API. Runs alongside the GPU worker in the same container.

```
Browser ←── WebRTC (datachannel-rs) ──→ b3-gpu-rtc (port 5126)
                                            │
                                   HTTP POST /run + GET /stream
                                   WebSocket /ws (streaming STT)
                                            │
                                      local_server.py (port 5125)
```

**Key features:**
- Receives WebRTC offers, negotiates ICE (STUN/TURN)
- Routes `submit` messages → POST `/run` → poll `/stream/{id}` → forward chunks back
- Routes `stream_start/chunk/finalize` → GPU WebSocket relay for streaming STT
- Chunk-count synchronization: finalize waits until expected chunks arrive
- Auto-creates GPU WS relay if `stream_start` was missed
- Heartbeat ping every 5s
- Log rotation: `/tmp/b3-gpu-rtc.log` with `.prev`

**Environment variables:**
- `GPU_RTC_PORT` — Listen port (default: 5126)
- `LOCAL_GPU_URL` — GPU endpoint (default: http://localhost:5125)
- `TURN_HOST` — TURN server IP
- `TURN_SECRET` — TURN shared secret

---

## Local Server (local_server.py)

FastAPI wrapper matching RunPod's API contract:

### HTTP Endpoints (port 5125)

| Method | Route | Description |
|--------|-------|-------------|
| POST | `/runsync` | Synchronous — blocks until complete |
| POST | `/run` | Async — returns `{id}`, poll `/stream/{id}` |
| GET | `/stream/{job_id}` | Poll for streaming chunks |
| GET | `/status/{job_id}` | Job status |
| GET | `/health` | GPU name, VRAM free, voices |

### WebSocket Endpoint (`/ws`)

Persistent multiplexed connection for concurrent jobs + streaming STT:

**Client → Server:**
- `submit` — `{type, job_id, input}` → runs handler
- `stream_start` — `{type, stream_id}` → begin STT stream
- `stream_chunk` — `{type, stream_id, audio_b64}` → webm audio chunk
- `stream_transcribe_partial` — transcribe accumulated audio at quiet point
- `stream_finalize` — transcribe remaining audio after partials
- `cancel` — cancel job
- `pong` — heartbeat response

**Server → Client:**
- `accepted` — `{type, job_id, state: "warm"|"cold"}`
- `progress` — per-stage metrics
- `chunk` — `{type, job_id, chunk_index, audio_b64, duration_sec}`
- `result` — `{type, job_id, output}`
- `done` — `{type, job_id, total_chunks}`
- `error` — `{type, job_id, error, code}`
- `ping` — heartbeat (5s interval)

### Prewarm

On startup (local mode only), pre-loads all models from disk to GPU:
- CUDA context initialization
- All whisper models (medium.en, large-v3)
- WhisperX variants (small.en, large-v3-turbo)
- Embedder (nomic-embed-text)
- Chatterbox pool (N instances based on VRAM)
- Warmup inference for cache population

First request latency equals any subsequent request.

---

## Models: Baked Into Image

All models pre-downloaded at build time. Zero network calls at runtime.

| Model | Size | Purpose |
|-------|------|---------|
| faster-whisper medium.en | 1.5GB | STT (english-fast preset) |
| faster-whisper large-v3 | 2.9GB | STT (english-quality, multilingual) |
| faster-whisper large-v3-turbo | 1.6GB | WhisperX alignment (multilingual) |
| faster-whisper small.en | 464MB | WhisperX alignment (english-fast) |
| Chatterbox TTS | 3.0GB | Voice synthesis |
| nomic-embed-text-v1.5 | 523MB | Semantic embeddings (768D) |
| PyAnnote diarization-3.1 | ~350MB | Speaker identification |
| PyAnnote embedding | ~130MB | Speaker vectors |

**Total models: ~10.5GB** baked into the image.

---

## VRAM Budget

### 24 GB 4090 (local)

```
Chatterbox TTS (1 instance):   ~3.5 GB
faster-whisper (medium.en):    ~2 GB
faster-whisper (large-v3):     ~5 GB
WhisperX + PyAnnote:           ~5 GB
nomic-embed-text:              ~0.5 GB
Headroom:                      ~8 GB
```

### 48 GB A40 (RunPod)

```
Chatterbox TTS (3 instances):  ~10.5 GB
All whisper models cached:     ~11.5 GB
PyAnnote diarization+emb:     ~1.5 GB
nomic-embed-text:              ~0.5 GB
Headroom:                      ~24 GB
```

---

## Docker Image

**Current size: 32.2GB** (v54). Known bloat: 2.8GB CUDA static libs, 685MB triton, 194MB gradio — see docker plugin's `image-size-management.md` reference for reduction plan.

**Build:**
```bash
cd gpu-worker
./build.sh v55
```

**Run locally:**
```bash
docker run -d --gpus all -p 5125:5125 -p 5126:5126 \
  -v /path/to/voices:/voices:ro \
  -e TURN_HOST=<your-turn-server-ip> -e TURN_SECRET=<your-turn-secret> \
  babel3/gpu-worker:<tag>
```

**Voices:** Mounted at runtime via `-v`, not baked into image.
**Speaker embeddings:** Sent dynamically via `enroll` action, not baked in.
