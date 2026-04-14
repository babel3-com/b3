# GPU Worker — Unified Docker Container

Combined GPU worker running Whisper + WhisperX + Chatterbox TTS + Embeddings on a single GPU.
**Same image runs everywhere:** RunPod cloud (48GB A40, 3x concurrency) and WSL local (24GB 4090, 1x concurrency).

**Docker Image:** `$DOCKER_ORG/gpu-worker:$VERSION`
**Configuration:** Set `DOCKER_ORG` and `HF_TOKEN` in `gpu-worker/.env` (never committed). Version is set at build time via `./build.sh <version>`.
**Cost (RunPod):** ~$0.00034/s (A40 tier)

## Modes

The entrypoint (`entrypoint.sh`) auto-detects the environment:

| Mode | Detection | What runs | Port | Concurrency |
|------|-----------|-----------|------|-------------|
| **RunPod** | `RUNPOD_POD_ID` or `RUNPOD_ENDPOINT_ID` env var set | `handler.py` via `runpod.serverless.start()` | RunPod internal | 3 (48GB A40) |
| **Local** | Neither env var set | `local_server.py` (FastAPI + uvicorn) | 5125 | 1 (24GB 4090) |

RunPod mode uses `return_aggregate_stream=True` in `runpod.serverless.start()`, which wraps yielded results in a list for `/runsync` responses. Streaming results are available via RunPod's `/stream/{job_id}` endpoint.

## Local Usage

```bash
# Run locally (voices mounted at runtime, not baked into image)
docker run -d \
  --name gpu-worker \
  --gpus all \
  -p 5125:5125 \
  -v /path/to/voices:/voices:ro \
  -e HF_TOKEN=$HF_TOKEN \
  $DOCKER_ORG/gpu-worker:$VERSION

# Health check
curl localhost:5125/health

# TTS test (synchronous)
curl -X POST localhost:5125/runsync \
  -H "Content-Type: application/json" \
  -d '{"input": {"action": "synthesize", "text": "Hello world", "voice": "zara"}}'

# Transcribe test
curl -X POST localhost:5125/runsync \
  -H "Content-Type: application/json" \
  -d '{"input": {"action": "transcribe", "audio_b64": "...", "language": "en"}}'

# Streaming TTS (async — poll for chunks)
JOB=$(curl -s -X POST localhost:5125/run \
  -H "Content-Type: application/json" \
  -d '{"input": {"action": "synthesize_stream", "text": "Hello world. This is a test.", "voice": "zara"}}' | jq -r '.id')
curl localhost:5125/stream/$JOB
```

Voice samples are volume-mounted from the host — new voice clones added to the host are immediately available without rebuilding.

## Local Server API (local_server.py)

The local server provides a RunPod-compatible API for local GPU usage:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/runsync` | POST | Synchronous request — blocks until complete, returns result |
| `/run` | POST | Async job submission — returns `{id}` immediately |
| `/stream/{job_id}` | GET | Poll for new streaming chunks (returns only unseen chunks) |
| `/status/{job_id}` | GET | Job status check |
| `/health` | GET | GPU info, VRAM usage, available voices |

Jobs are stored in memory with a 5-minute TTL. The `/stream` endpoint tracks which chunks each client has already seen, returning only new ones on each poll.

## Actions

All requests use the same payload format. Route by `action` field.

### `synthesize` — Chatterbox TTS

```json
{"input": {"action": "synthesize", "text": "Hello world", "voice": "zara", "exaggeration": 0.5, "cfg_weight": 0.5}}
```

Response: `{"audio_b64": "<base64 WAV>", "sample_rate": 24000, "duration_sec": 3.56, "generation_sec": 2.43}`

Optional fields:
- `voice` — name of .wav file in `/voices/` (default: `"zara"`)
- `conditionals_b64` — pre-computed voice conditionals (.pt file, base64). Overrides `voice` if provided.
- `exaggeration` — voice expressiveness, 0.0-1.0 (default: 0.5)
- `cfg_weight` — classifier-free guidance weight (default: 0.5)
- `msg_id` + `chunk_index` — when provided, routes through the priority SynthesisQueue (lower chunk_index = higher priority)

### `synthesize_stream` — Streaming TTS (sentence-level)

```json
{"input": {"action": "synthesize_stream", "text": "First sentence. Second sentence. Third!", "voice": "zara"}}
```

Yields one chunk per sentence:
```json
{"chunk_index": 0, "audio_b64": "...", "sample_rate": 24000, "duration_sec": 1.2, "generation_sec": 0.8, "rtf": 0.667, "msg_id": ""}
{"chunk_index": 1, "audio_b64": "...", ...}
{"done": true, "total_chunks": 3, "msg_id": ""}
```

**How it works:** Text is split at natural speech boundaries (`.!?;,` followed by space). Each sentence is generated using regular `model.generate()` — this is NOT token-level streaming. Each chunk is a complete, clean utterance with no artifacts. Voice conditionals (`prepare_conditionals()`) are computed once before the sentence loop, not per-sentence (CUDA fix for consistency and speed).

**Client-side polling:** Use `POST /run` to submit, then poll `GET /stream/{job_id}` to receive chunks as they generate. Browser clients can decode `audio_b64` and play immediately for low-latency first-chunk audio.

### `transcribe` — Dual Transcription

```json
{"input": {"action": "transcribe", "audio_b64": "<base64 audio>", "language": "en", "agent_id": "voice-web", "diarize": true}}
```

Response:
```json
{
  "text": "Hello world",
  "segments": [{"start": 0, "end": 1.5, "text": "Hello world"}],
  "medium_elapsed": 0.68,
  "whisperx_text": "[YANIV] Hello world",
  "whisperx_elapsed": 2.1,
  "speakers": {"SPEAKER_00": {"name": "YANIV", "score": 0.85}},
  "embedding_similarity": 0.9842,
  "preset": "english-fast"
}
```

- `diarize: false` skips WhisperX/PyAnnote (faster, no speaker labels)
- `agent_id` selects which enrolled speaker set to match against
- `preset` selects transcription model pair (see Presets below)
- `speakers` — inline speaker enrollment (dict of `name: base64_npy`). Ensures the same worker that transcribes has the embeddings.
- Audio can be any format (webm, wav, mp3) — ffmpeg converts internally

**Presets:**

| Preset | faster-whisper model | WhisperX model | Use case |
|--------|---------------------|---------------|----------|
| `english-fast` (default) | medium.en | small.en | Fast English transcription |
| `english-quality` | large-v3 | medium.en | High-quality English |
| `multilingual` | large-v3 | large-v3-turbo | Non-English languages |

### `embed` — Sentence Embeddings (nomic-embed-text, 768D)

```json
{"input": {"action": "embed", "texts": ["Hello world"]}}
```

Response: `{"embeddings": [[...768 floats...]], "dimensions": 768, "count": 1, "processing_sec": 0.02}`

Optional: `task_type` (default `"search_document"`, use `"search_query"` for queries).

### `enroll` — Speaker Enrollment (per-agent)

```json
{"input": {"action": "enroll", "agent_id": "voice-web", "speakers": {"YANIV": "<base64 .npy>", "RACHEL": "<base64 .npy>"}}}
```

Response: `{"enrolled": 2, "agent_id": "voice-web", "speakers": ["YANIV", "RACHEL"]}`

Speaker embeddings are stored in memory, keyed by `agent_id`. Each daemon enrolls its own speaker set on first connection. Enrollment is also supported inline within `transcribe` requests.

## Handler Architecture

`handler.py` is a **sync generator** that yields results. This supports two handler types:

- **Regular handlers** (`synthesize`, `embed`, `enroll`): yield a single result dict.
- **Generator handlers** (`synthesize_stream`): yield multiple result dicts. On RunPod, these accumulate at the `/stream/{job_id}` endpoint.

```python
# Router in handler.py
HANDLERS = {
    "synthesize": handle_synthesize,
    "embed": handle_embed,
    "enroll": handle_enroll,
}

GENERATOR_HANDLERS = {
    "synthesize_stream": handle_synthesize_stream,
}
```

All handlers attach pass-through metadata (`msg_id`, `chunk`, `total`, `agent_id`) to results.

## Chatterbox TTS Pool

The handler runs a pool of Chatterbox model instances for concurrent TTS. Each instance has its own lock because `model.conds` is mutable state — concurrent `generate()` calls on the same instance with different conditionals causes voice corruption.

- **Pool size:** auto-detected or set via `CHATTERBOX_POOL_SIZE` env var. Defaults to 2 (CPU prep overlaps GPU inference).
- **Priority scheduling:** When `msg_id` is provided, requests go through `SynthesisQueue` which orders by `(chunk_index, arrival_time)` — earlier chunks generate first, with natural cross-message interleaving.
- **Round-robin fallback:** Requests without `msg_id` use direct round-robin across pool instances.

## Client Routing (runpod_client.py)

The `runpod_client.py` in voice-web handles routing automatically:
1. Try local Docker container (localhost:5125, 2s connect timeout)
2. If unreachable, fall back to RunPod cloud

This is NOT a dual code path — the handler code is identical in both locations. Same Docker image, same handler.py. `runpod_client.py` unwraps single-item lists for backward compatibility with `return_aggregate_stream=True`.

## Architecture

```
              ┌───────────────────────┐
              │    handler.py         │  Same code everywhere
              │  (sync generator)     │
              │  6 actions            │
              └──────────┬────────────┘
                         │
         ┌───────────────┼────────────────┐
         │               │                │
  ┌──────┴──────┐  ┌─────┴──────┐  ┌─────┴──────┐
  │ RunPod A40  │  │  WSL 4090  │  │ Future GPU │
  │ runpod.     │  │ FastAPI +  │  │            │
  │ serverless  │  │  uvicorn   │  │            │
  │ 3x concurr │  │ 1x concurr │  │            │
  └─────────────┘  └────────────┘  └────────────┘
```

## Models & VRAM

| Model | VRAM | Action |
|-------|------|--------|
| Whisper medium.en (faster-whisper) | ~2GB | transcribe |
| WhisperX small.en | ~1GB | transcribe (diarize) |
| PyAnnote diarization 3.1 | ~1.5GB | transcribe (diarize) |
| PyAnnote embedding | ~0.5GB | transcribe (speaker match) |
| Chatterbox TTS (per pool instance) | ~3.5GB | synthesize, synthesize_stream |
| nomic-embed-text-v1.5 | ~0.5GB | embed |
| **Total (2 TTS instances)** | **~12.5GB** | |

Fits on 24GB 4090 with ~11.5GB headroom. On 48GB A40, supports 3x concurrency. All models load lazily on first request to avoid startup crashes.

## Building & Deploying

```bash
# Configure (create .env — never committed)
cd gpu-worker
echo 'DOCKER_ORG=your-dockerhub-org' >> .env
echo 'HF_TOKEN=hf_xxx' >> .env

# Build (HF_TOKEN needed for PyAnnote gated models)
./build.sh v42

# Push (use versioned tags — RunPod caches by tag)
docker push $DOCKER_ORG/gpu-worker:$VERSION

# Update RunPod endpoint to use new tag in console
# NEVER use :latest — RunPod won't pull the new image
```

Only PyAnnote is pre-downloaded at build time (gated model). All other models lazy-load on first request. Voices are mounted at runtime via `-v /path/to/voices:/voices:ro`.

## Systemd Service (WSL local)

```bash
# Copy the gpu-worker service file to systemd
sudo cp gpu-worker.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable gpu-worker
sudo systemctl start gpu-worker
```

## Files

| File | Purpose |
|------|---------|
| `handler.py` | Core handler — all actions, model loading, synthesis queue, speaker matching |
| `local_server.py` | FastAPI wrapper around handler.py for local GPU mode |
| `entrypoint.sh` | Auto-detects RunPod vs local, launches the right server |
| `Dockerfile` | Build with PyAnnote pre-download (other models lazy-load) |
| `voices/` | WAV reference samples for Chatterbox TTS voice cloning (mount at runtime, not in image) |

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `HF_TOKEN` | Build time | HuggingFace token for PyAnnote gated models (pass as `--build-arg`) |
| `RUNPOD_POD_ID` | Auto | Set by RunPod — triggers RunPod mode |
| `RUNPOD_ENDPOINT_ID` | Auto | Set by RunPod — triggers RunPod mode |
| `PORT` | No | Override local server port (default: 5125) |
| `CHATTERBOX_POOL_SIZE` | No | Override TTS pool size (default: auto-detect, capped at 2) |

## Adding Voice Samples

Voice reference WAVs for Chatterbox TTS are mounted at runtime via `-v`, not baked into the image:

```bash
docker run -d --gpus all -p 5125:5125 \
  -v /path/to/voices:/voices:ro \
  $DOCKER_ORG/gpu-worker:$VERSION
```

1. Add `NAME.wav` (clean speech, 5-15 seconds) to your voices directory
2. Use `"voice": "NAME"` in synthesize/synthesize_stream requests
3. Pre-baked conditionals (`.pt` files) can go in `voices/.cache/NAME.pt` for faster startup

## Key Fixes (version history)

- **v43:** Current — restored compute_conditionals action (mistakenly removed in v42, needed by browser Voices tab)
- **v42:** Open-source cleanup: removed callback infrastructure, health_check, voice-cloning-frontend.js; voices mounted at runtime instead of baked in, models lazy-load instead of pre-downloaded (except gated PyAnnote)
- **v25:** Sentence-level streaming TTS, priority synthesis queue, Chatterbox pool, inline speaker enrollment, transcription presets, trailing noise trimming
- **v22:** Async generator handler (fixed `return value` SyntaxError in generator), streaming TTS, `return_aggregate_stream=True`
- **v14:** Unified container — same image runs on RunPod and locally, auto-detection entrypoint
- **v6:** Added local mode (FastAPI + uvicorn), entrypoint auto-detection, `__main__` guard on runpod.serverless.start()
- **v5:** Added `libcudnn8` for GPU-accelerated Whisper (was falling back to CPU)
- **v4:** Fixed `total_mem` -> `total_memory` typo in startup diagnostics
- **v3:** Monkey-patched `torch.load` for PyTorch 2.6 `weights_only=True` compatibility with WhisperX/PyAnnote
- **v2:** Lazy model loading (models load on first request, not at import time)
- **v1:** Initial — eager loading crashed on startup
