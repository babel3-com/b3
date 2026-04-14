"""Local GPU server — same handler.py as RunPod, served via FastAPI.

Accepts the same API contract as RunPod runsync AND streaming:
  POST /runsync  {"input": {"action": "...", ...}}  → synchronous result
  POST /run      {"input": {"action": "...", ...}}  → async job, returns {id}
  GET  /stream/{job_id}                              → poll for streaming chunks
  GET  /status/{job_id}                              → job status
  GET  /health

Port 5125 (same as old chatterbox-api.py for backward compatibility).
Concurrency managed by SynthesisQueue in handler.py — multiple requests
can be accepted concurrently; the queue handles priority scheduling.
"""

import collections
import os
import sys
import uuid
import asyncio
import threading
import time
import billing
import torch

# ── In-memory log capture ─────────────────────────────────────────────
# Intercept sys.stdout / sys.stderr so all print() output is captured.
# Rolling buffer: last 2000 lines. Thread-safe via lock.
# Mirrors ws_server.py — handler.py's `logs` action imports _log_buffer.
_log_lock = threading.Lock()
_log_buffer = collections.deque(maxlen=2000)


class _TeeWriter:
    """Wraps an existing stream, copies each write to _log_buffer."""
    def __init__(self, original):
        self._original = original
        self._pending = ""

    def write(self, data):
        self._original.write(data)
        self._pending += data
        while "\n" in self._pending:
            line, self._pending = self._pending.split("\n", 1)
            with _log_lock:
                _log_buffer.append(line)

    def flush(self):
        self._original.flush()

    def __getattr__(self, name):
        return getattr(self._original, name)


def _install_log_capture():
    """Redirect stdout/stderr through TeeWriter once. Idempotent."""
    if not isinstance(sys.stdout, _TeeWriter):
        sys.stdout = _TeeWriter(sys.stdout)
    if not isinstance(sys.stderr, _TeeWriter):
        sys.stderr = _TeeWriter(sys.stderr)


from fastapi import FastAPI, Request, WebSocket, WebSocketDisconnect
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from handler import handler

app = FastAPI(title="GPU Worker (Local)")

# Allow browser to call local GPU directly (CORS open for local development)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)

# Bearer token auth — GPU_AUTH_TOKEN env var
_AUTH_TOKEN = os.environ.get("GPU_AUTH_TOKEN", "")


@app.middleware("http")
async def verify_auth(request: Request, call_next):
    if not _AUTH_TOKEN:
        return await call_next(request)
    # Allow CORS preflight through (CORSMiddleware handles it after us)
    if request.method == "OPTIONS":
        return await call_next(request)
    auth = request.headers.get("Authorization", "")
    if auth == f"Bearer {_AUTH_TOKEN}":
        return await call_next(request)
    return JSONResponse(status_code=401, content={"error": "unauthorized"})

# In-memory job store for async /run + /stream pattern
_jobs = {}  # job_id -> {"status": str, "chunks": list, "started": float}
_jobs_lock = threading.Lock()

# Clean up old jobs after 5 minutes
JOB_TTL = 300


def _cleanup_old_jobs():
    now = time.time()
    with _jobs_lock:
        stale = [jid for jid, j in _jobs.items() if now - j["started"] > JOB_TTL]
        for jid in stale:
            del _jobs[jid]


def _run_job(job_id: str, event: dict):
    """Run handler in background thread, accumulating yielded chunks."""
    try:
        for chunk in handler(event):
            with _jobs_lock:
                if job_id in _jobs:
                    _jobs[job_id]["chunks"].append(chunk)
    finally:
        with _jobs_lock:
            if job_id in _jobs:
                _jobs[job_id]["status"] = "COMPLETED"


@app.post("/runsync")
async def runsync(request: dict):
    """Synchronous — collects all yielded results."""
    # Suppress EC2 proxy coord frame (handler.py:1563 gates on _via_ws).
    # HTTP callers don't need worker coordinates — only WS streaming needs them suppressed per-call.
    request["_via_ws"] = True
    results = list(handler(request))
    if len(results) == 1:
        return {"status": "COMPLETED", "output": results[0]}
    return {"status": "COMPLETED", "output": results}


@app.post("/run")
async def run_async(request: dict):
    """Async job submission — returns job_id immediately."""
    _cleanup_old_jobs()
    job_id = str(uuid.uuid4())[:12]
    with _jobs_lock:
        _jobs[job_id] = {
            "status": "IN_PROGRESS",
            "chunks": [],
            "started": time.time(),
            "delivered": 0,  # How many chunks the client has already seen
        }
    t = threading.Thread(target=_run_job, args=(job_id, request), daemon=True)
    t.start()
    return {"id": job_id, "status": "IN_QUEUE"}


@app.get("/stream/{job_id}")
async def stream_poll(job_id: str):
    """Poll for new streaming chunks. Returns only NEW chunks since last poll.
    RunPod contract: returns list of {output: ...} items."""
    with _jobs_lock:
        job = _jobs.get(job_id)
        if not job:
            return {"stream": [], "status": "NOT_FOUND"}
        new_chunks = job["chunks"][job["delivered"]:]
        job["delivered"] = len(job["chunks"])
        status = job["status"]
    return {"stream": [{"output": c} for c in new_chunks], "status": status}


@app.get("/status/{job_id}")
async def status(job_id: str):
    """Job status check."""
    with _jobs_lock:
        job = _jobs.get(job_id)
        if not job:
            return {"status": "NOT_FOUND"}
        return {"status": job["status"], "chunks_generated": len(job["chunks"])}


@app.get("/health")
async def health():
    gpu_name = torch.cuda.get_device_name(0) if torch.cuda.is_available() else "cpu"
    vram_total = torch.cuda.get_device_properties(0).total_memory / 1e9 if torch.cuda.is_available() else 0
    vram_used = torch.cuda.memory_allocated(0) / 1e9 if torch.cuda.is_available() else 0

    voices = []
    if os.path.isdir("/voices"):
        voices = [f.replace(".wav", "") for f in os.listdir("/voices") if f.endswith(".wav")]

    return {
        "status": "ok",
        "mode": "local",
        "version": os.environ.get("GPU_WORKER_VERSION", "unknown"),
        "billing": "disabled (dev mode)" if billing.DEV_MODE else billing.BILLING_URL,
        "gpu": gpu_name,
        "vram_total_gb": round(vram_total, 1),
        "vram_used_gb": round(vram_used, 1),
        "voices": voices,
    }


def _classify_chunk(chunk: dict, job_id: str) -> dict:
    """Convert handler yield format to typed WebSocket message."""
    if chunk.get("error"):
        return {"type": "error", "job_id": job_id, "error": chunk["error"],
                "code": chunk.get("error_code", "handler_error")}
    if chunk.get("status") == "accepted":
        return {"type": "accepted", "job_id": job_id,
                "state": chunk.get("state", "unknown"),
                "gpu_worker_id": chunk.get("gpu_worker_id", "")}
    if chunk.get("progress"):
        return {"type": "progress", "job_id": job_id, "progress": chunk["progress"]}
    if chunk.get("done"):
        return {"type": "done", "job_id": job_id,
                "total_chunks": chunk.get("total_chunks", 0)}
    if chunk.get("chunk_index") is not None:
        return {"type": "chunk", "job_id": job_id,
                "chunk_index": chunk["chunk_index"],
                "audio_b64": chunk.get("audio_b64", ""),
                "duration_sec": chunk.get("duration_sec", 0)}
    # Default: full result (STT, diarize, embed, etc.)
    return {"type": "result", "job_id": job_id, "output": chunk}


# Module-level stream state — survives WebSocket reconnects
_audio_streams: dict[str, list] = {}   # stream_id -> [b64_chunks]
_stream_prerolls: dict[str, str] = {}  # stream_id -> preroll WAV b64
_stream_consumed: dict[str, int] = {}  # stream_id -> PCM samples already transcribed
_stream_agents: dict[str, str] = {}   # stream_id -> agent_id (for billing)


async def _ffmpeg_pcm(path: str = None, data: bytes = None, timeout: int = 15) -> bytes | None:
    """Decode audio to 16kHz mono s16le PCM via ffmpeg (non-blocking async subprocess).

    Pass `path` for file input or `data` for piped bytes — not both.
    Returns raw PCM bytes on success, None on failure.

    Uses asyncio.create_subprocess_exec so the event loop stays responsive
    during decode. The old subprocess.run approach blocked all WS message
    processing for 2-5s on a 20s audio buffer.
    """
    if path:
        cmd = ["ffmpeg", "-i", path, "-f", "s16le", "-ac", "1", "-ar", "16000", "-"]
        stdin_mode = None
    else:
        cmd = ["ffmpeg", "-i", "pipe:0", "-f", "s16le", "-ac", "1", "-ar", "16000", "-"]
        stdin_mode = asyncio.subprocess.PIPE

    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdin=stdin_mode,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        stdout, _ = await asyncio.wait_for(proc.communicate(input=data), timeout=timeout)
    except asyncio.TimeoutError:
        proc.kill()
        await proc.communicate()  # reap the process
        raise
    return stdout if proc.returncode == 0 and stdout else None


@app.websocket("/ws")
async def ws_endpoint(websocket: WebSocket):
    """Persistent WebSocket for GPU job submission + streaming results.

    Multiplexes concurrent jobs over a single connection.
    Client sends {type: "submit", job_id, input} messages.
    Server pushes {type: "progress|chunk|result|done|error", job_id, ...} messages.
    """
    # Auth check from query param
    token = websocket.query_params.get("token", "")
    if _AUTH_TOKEN and token != _AUTH_TOKEN:
        await websocket.close(code=4001, reason="unauthorized")
        return

    await websocket.accept()
    active_tasks: dict[str, asyncio.Task] = {}
    print("[WS] Client connected", flush=True)

    async def _run_job_ws(job_id: str, event: dict):
        """Run sync handler in thread, forward chunks over WebSocket."""
        q: asyncio.Queue = asyncio.Queue()

        def _thread_target():
            try:
                for chunk in handler(event):
                    q.put_nowait(chunk)
            except Exception as e:
                q.put_nowait({"error": str(e)})
            finally:
                q.put_nowait(None)  # sentinel

        thread = threading.Thread(target=_thread_target, daemon=True)
        thread.start()

        try:
            while True:
                chunk = await q.get()
                if chunk is None:
                    break
                msg = _classify_chunk(chunk, job_id)
                await websocket.send_json(msg)
        except (WebSocketDisconnect, RuntimeError):
            pass  # client gone, thread will finish naturally
        finally:
            active_tasks.pop(job_id, None)

    # Heartbeat: ping every 5s (tolerates cellular packet loss)
    async def _heartbeat():
        try:
            while True:
                await asyncio.sleep(5)
                await websocket.send_json({"type": "ping"})
        except (WebSocketDisconnect, RuntimeError):
            pass

    heartbeat_task = asyncio.create_task(_heartbeat())

    try:
        while True:
            data = await websocket.receive_json()
            msg_type = data.get("type", "")

            if msg_type == "submit":
                job_id = data.get("job_id", str(uuid.uuid4())[:12])
                input_data = data.get("input", {})
                action = input_data.get("action", "")
                agent_id = input_data.get("agent_id", "")

                # Credit gate — check before billable actions
                if action in billing.BILLABLE_ACTIONS and not billing.DEV_MODE:
                    if not agent_id:
                        await websocket.send_json({"type": "error", "job_id": job_id, "error": "agent_id required"})
                        continue
                    credit_check = billing.check_credits(agent_id, action)
                    print(f"[BILLING] {action} agent={agent_id} check={credit_check}", flush=True)
                    if not credit_check.get("allowed", True):
                        await websocket.send_json({"type": "error", "job_id": job_id, "error": credit_check.get("message", "Credits depleted")})
                        continue

                event = {"input": input_data, "_via_ws": True}
                task = asyncio.create_task(_run_job_ws(job_id, event))
                active_tasks[job_id] = task
                print(f"[WS] Job {job_id} submitted: {action}", flush=True)

                # Deduct credit after job completes (fire-and-forget)
                if action in billing.BILLABLE_ACTIONS:
                    async def _deduct_after(t, aid, act):
                        try:
                            await t
                            threading.Thread(target=billing.deduct_credit, args=(aid, act), daemon=True).start()
                        except Exception:
                            pass
                    asyncio.create_task(_deduct_after(task, agent_id, action))

            elif msg_type == "cancel":
                job_id = data.get("job_id", "")
                task = active_tasks.pop(job_id, None)
                if task:
                    task.cancel()
                    print(f"[WS] Job {job_id} cancelled", flush=True)

            elif msg_type == "stream_start":
                sid = data.get("stream_id", "")
                _audio_streams[sid] = []
                _stream_agents[sid] = data.get("agent_id", "")
                print(f"[WS] Stream {sid} started (agent={_stream_agents[sid][:8]}...)", flush=True)

            elif msg_type == "stream_preroll":
                sid = data.get("stream_id", "")
                _stream_prerolls[sid] = data.get("audio_b64", "")
                print(f"[WS] Stream {sid} preroll received ({len(data.get('audio_b64', ''))} b64 chars)", flush=True)

            elif msg_type == "stream_chunk":
                sid = data.get("stream_id", "")
                if sid in _audio_streams:
                    _audio_streams[sid].append(data.get("audio_b64", ""))

            elif msg_type == "stream_transcribe_partial":
                sid = data.get("stream_id", "")
                chunks = _audio_streams.get(sid, [])
                if chunks:
                    import base64, tempfile, numpy as np, os, struct, io
                    # Decode FULL webm (headers are only at start), then slice PCM.
                    # NOTE: base64 decode happens on the event loop but is fast (<1ms for
                    # typical chunk counts). The ffmpeg decodes below are the real bottleneck
                    # and are now async.
                    all_bytes = b"".join(base64.b64decode(c) for c in chunks)
                    consumed_samples = _stream_consumed.get(sid, 0)
                    if len(all_bytes) > 1000:
                        try:
                            with tempfile.NamedTemporaryFile(suffix=".webm", delete=False) as f:
                                f.write(all_bytes)  # FULL webm with headers
                                tmp_path = f.name
                            try:
                                # On first partial, decode preroll and main WebM concurrently
                                # so neither blocks while the other runs.
                                should_preroll = consumed_samples == 0 and sid in _stream_prerolls
                                if should_preroll:
                                    preroll_wav = base64.b64decode(_stream_prerolls[sid])
                                    pcm_raw, preroll_raw = await asyncio.gather(
                                        _ffmpeg_pcm(path=tmp_path, timeout=10),
                                        _ffmpeg_pcm(data=preroll_wav, timeout=5),
                                        return_exceptions=True,
                                    )
                                else:
                                    pcm_raw = await _ffmpeg_pcm(path=tmp_path, timeout=10)
                                    preroll_raw = None
                            finally:
                                os.unlink(tmp_path)

                            if not isinstance(pcm_raw, bytes) or len(pcm_raw) <= 3200:
                                print(f"[WS] Partial transcribe: WebM decode failed for {sid}", flush=True)
                            else:
                                sr = 16000
                                full_pcm = np.frombuffer(pcm_raw, dtype=np.int16).astype(np.float32) / 32768.0
                                if should_preroll and isinstance(preroll_raw, bytes) and preroll_raw:
                                    preroll_pcm = np.frombuffer(preroll_raw, dtype=np.int16).astype(np.float32) / 32768.0
                                    full_pcm = np.concatenate([preroll_pcm, full_pcm])
                                    print(f"[WS] Prepended {len(preroll_pcm)/sr:.1f}s preroll to stream {sid}", flush=True)
                                elif should_preroll:
                                    print(f"[WS] Preroll decode failed for {sid} (non-fatal)", flush=True)
                                new_pcm = full_pcm[consumed_samples:]
                                if len(new_pcm) > sr:  # at least 1 second
                                    # Find quietest 300ms window in last 10s of new audio
                                    search_start = max(0, len(new_pcm) - sr * 10)
                                    window = int(sr * 0.3)
                                    step = max(1, window // 3)
                                    best_rms, best_mid = float('inf'), search_start + window // 2
                                    for i in range(search_start, len(new_pcm) - window, step):
                                        rms = float(np.sqrt(np.mean(new_pcm[i:i+window] ** 2)))
                                        if rms < best_rms:
                                            best_rms = rms
                                            best_mid = i + window // 2
                                    cut_sample = best_mid
                                    cut_sec = cut_sample / sr
                                    total_new_sec = len(new_pcm) / sr
                                    print(f"[WS] Quiet cut at {cut_sec:.1f}s/{total_new_sec:.1f}s (rms={best_rms:.4f})", flush=True)
                                    send_pcm = new_pcm[:cut_sample]
                                    wav_buf = io.BytesIO()
                                    send_i16 = (send_pcm * 32767).astype(np.int16)
                                    raw = send_i16.tobytes()
                                    wav_buf.write(b'RIFF')
                                    wav_buf.write(struct.pack('<I', 36 + len(raw)))
                                    wav_buf.write(b'WAVEfmt ')
                                    wav_buf.write(struct.pack('<IHHIIHH', 16, 1, 1, sr, sr*2, 2, 16))
                                    wav_buf.write(b'data')
                                    wav_buf.write(struct.pack('<I', len(raw)))
                                    wav_buf.write(raw)
                                    audio_b64 = base64.b64encode(wav_buf.getvalue()).decode()
                                    _stream_consumed[sid] = consumed_samples + cut_sample
                                    job_id = sid + "-partial-" + str(len(chunks))
                                    event = {"input": {
                                        "action": "transcribe",
                                        "audio_b64": audio_b64,
                                        "agent_id": _stream_agents.get(sid, ""),
                                        "language": data.get("language", "en"),
                                        "preset": data.get("preset", "multilingual"),
                                        "skip_align": data.get("skip_align", False),
                                    }, "_via_ws": True}
                                    task = asyncio.create_task(_run_job_ws(job_id, event))
                                    active_tasks[job_id] = task
                                    print(f"[WS] Stream {sid} partial: {cut_sec:.1f}s of {total_new_sec:.1f}s new audio", flush=True)
                        except Exception as e:
                            print(f"[WS] Partial transcribe failed: {e}", flush=True)

            elif msg_type == "stream_finalize":
                sid = data.get("stream_id", "")
                chunks = _audio_streams.pop(sid, [])
                preroll_b64 = _stream_prerolls.pop(sid, None)
                stream_agent_id = _stream_agents.pop(sid, "")
                consumed_samples = _stream_consumed.pop(sid, 0)
                if chunks:
                    import base64
                    all_bytes = b"".join(base64.b64decode(c) for c in chunks)
                    # If partials consumed some audio, decode full webm and slice remaining PCM
                    if consumed_samples > 0:
                        try:
                            import tempfile, numpy as np, os, struct, io
                            with tempfile.NamedTemporaryFile(suffix=".webm", delete=False) as f:
                                f.write(all_bytes)
                                tmp_path = f.name
                            try:
                                pcm_raw = await _ffmpeg_pcm(path=tmp_path, timeout=15)
                            finally:
                                os.unlink(tmp_path)
                            if pcm_raw:
                                full_pcm = np.frombuffer(pcm_raw, dtype=np.int16).astype(np.float32) / 32768.0
                                remaining = full_pcm[consumed_samples:]
                                if len(remaining) < 1600:  # less than 0.1s
                                    print(f"[WS] Stream {sid} finalized: no remaining audio after partials", flush=True)
                                    await websocket.send_json({"type": "result", "job_id": sid, "output": {"text": "", "whisperx_text": ""}})
                                    continue
                                # Encode remaining PCM as WAV
                                sr = 16000
                                send_i16 = (remaining * 32767).astype(np.int16)
                                raw = send_i16.tobytes()
                                wav_buf = io.BytesIO()
                                wav_buf.write(b'RIFF')
                                wav_buf.write(struct.pack('<I', 36 + len(raw)))
                                wav_buf.write(b'WAVEfmt ')
                                wav_buf.write(struct.pack('<IHHIIHH', 16, 1, 1, sr, sr*2, 2, 16))
                                wav_buf.write(b'data')
                                wav_buf.write(struct.pack('<I', len(raw)))
                                wav_buf.write(raw)
                                audio_b64 = base64.b64encode(wav_buf.getvalue()).decode()
                                remaining_sec = len(remaining) / sr
                                print(f"[WS] Stream {sid} finalized: {remaining_sec:.1f}s remaining after partials", flush=True)
                            else:
                                audio_b64 = base64.b64encode(all_bytes).decode()
                                print(f"[WS] Stream {sid} finalized: ffmpeg failed, sending full audio", flush=True)
                        except Exception as e:
                            audio_b64 = base64.b64encode(all_bytes).decode()
                            print(f"[WS] Stream {sid} finalized: decode failed ({e}), sending full audio", flush=True)
                    else:
                        # No partials — prepend preroll if available
                        if preroll_b64:
                            try:
                                import tempfile, numpy as np, os, struct, io
                                # Decode preroll WAV and main WebM concurrently (non-blocking)
                                preroll_wav = base64.b64decode(preroll_b64)
                                with tempfile.NamedTemporaryFile(suffix=".webm", delete=False) as f:
                                    f.write(all_bytes)
                                    tmp_path = f.name
                                try:
                                    preroll_raw, webm_raw = await asyncio.gather(
                                        _ffmpeg_pcm(data=preroll_wav, timeout=5),
                                        _ffmpeg_pcm(path=tmp_path, timeout=15),
                                        return_exceptions=True,
                                    )
                                finally:
                                    os.unlink(tmp_path)
                                if isinstance(preroll_raw, bytes) and preroll_raw and isinstance(webm_raw, bytes) and webm_raw:
                                    preroll_pcm = np.frombuffer(preroll_raw, dtype=np.int16).astype(np.float32) / 32768.0
                                    webm_pcm = np.frombuffer(webm_raw, dtype=np.int16).astype(np.float32) / 32768.0
                                    combined = np.concatenate([preroll_pcm, webm_pcm])
                                    sr = 16000
                                    send_i16 = (combined * 32767).astype(np.int16)
                                    raw = send_i16.tobytes()
                                    wav_buf = io.BytesIO()
                                    wav_buf.write(b'RIFF')
                                    wav_buf.write(struct.pack('<I', 36 + len(raw)))
                                    wav_buf.write(b'WAVEfmt ')
                                    wav_buf.write(struct.pack('<IHHIIHH', 16, 1, 1, sr, sr*2, 2, 16))
                                    wav_buf.write(b'data')
                                    wav_buf.write(struct.pack('<I', len(raw)))
                                    wav_buf.write(raw)
                                    audio_b64 = base64.b64encode(wav_buf.getvalue()).decode()
                                    print(f"[WS] Stream {sid} finalized: {len(preroll_pcm)/sr:.1f}s preroll + {len(webm_pcm)/sr:.1f}s main", flush=True)
                                else:
                                    audio_b64 = base64.b64encode(all_bytes).decode()
                                    print(f"[WS] Stream {sid} finalized: preroll decode failed, sending webm only", flush=True)
                            except Exception as e:
                                audio_b64 = base64.b64encode(all_bytes).decode()
                                print(f"[WS] Stream {sid} finalized: preroll prepend failed ({e}), sending webm only", flush=True)
                        else:
                            audio_b64 = base64.b64encode(all_bytes).decode()
                            print(f"[WS] Stream {sid} finalized: {len(all_bytes)}B (no partials, no preroll)", flush=True)
                    job_id = sid
                    event = {"input": {
                        "action": "transcribe",
                        "audio_b64": audio_b64,
                        "agent_id": stream_agent_id,
                        "language": data.get("language", "en"),
                        "preset": data.get("preset", "multilingual"),
                        "skip_align": data.get("skip_align", False),
                    }, "_via_ws": True}
                    task = asyncio.create_task(_run_job_ws(job_id, event))
                    active_tasks[job_id] = task
                    print(f"[WS] Stream {sid} finalized: {len(chunks)} chunks → transcribe", flush=True)
                else:
                    # No chunks — stream state was lost (GPU container restart while stream was active).
                    # Send an empty result so the relay can route it and the browser gets a
                    # response instead of hanging until the liveness probe fires.
                    print(f"[WS] Stream {sid} finalized but no chunks (state lost — container restart?)", flush=True)
                    await websocket.send_json({"type": "result", "job_id": sid, "output": {"text": "", "whisperx_text": ""}})

            elif msg_type == "pong":
                pass  # heartbeat response

    except (WebSocketDisconnect, RuntimeError):
        pass
    finally:
        heartbeat_task.cancel()
        for task in active_tasks.values():
            task.cancel()
        print("[WS] Client disconnected", flush=True)


def _prewarm():
    """Pre-warm all models at startup so first request is fast."""
    import torch
    from handler import (
        _init_chatterbox_pool, acquire_chatterbox,
        get_whisper, get_whisperx, get_embedder,
    )

    # Pre-init CUDA context
    if torch.cuda.is_available():
        print("Pre-initializing CUDA context...", flush=True)
        t0 = time.time()
        torch.cuda.init()
        torch.zeros(1, device="cuda")
        print(f"CUDA context ready in {time.time() - t0:.1f}s", flush=True)

    # Transcription models first
    print("Pre-loading transcription models...", flush=True)
    t = time.time()
    get_whisper("large-v3")
    get_whisperx("large-v3-turbo")
    get_whisper("medium.en")
    get_whisperx("small.en")
    print(f"Transcription models ready in {time.time() - t:.1f}s", flush=True)

    # Embedder (used for transcription similarity check)
    print("Pre-loading embedder...", flush=True)
    t = time.time()
    get_embedder()
    print(f"Embedder ready in {time.time() - t:.1f}s", flush=True)

    # Then Chatterbox
    print("Pre-warming Chatterbox pool...", flush=True)
    t = time.time()
    _init_chatterbox_pool()
    print(f"Chatterbox pool warm in {time.time() - t:.1f}s", flush=True)

    # Warmup inference
    try:
        t2 = time.time()
        print("Running warmup inference...", flush=True)
        model, lock = acquire_chatterbox()
        with lock:
            _dummy = model.generate("Hello.", audio_prompt_path=None)
        del _dummy
        if torch.cuda.is_available():
            torch.cuda.empty_cache()
        print(f"Warmup inference done in {time.time() - t2:.1f}s", flush=True)
    except Exception as e:
        print(f"Warmup inference failed (non-fatal): {e}", flush=True)


def start_ws_server():
    """Start local_server FastAPI app in a background thread. Non-blocking.

    Used by handler.py on RunPod to serve the full WS endpoint (including
    streaming protocol) in the same process as the loaded models.
    Same pattern as ws_server.start_ws_server() but backed by this app
    which already has stream_start/chunk/finalize/partial support.
    """
    import threading
    import uvicorn as _uvicorn

    port = int(os.environ.get("PORT", 5125))

    def _run():
        config = _uvicorn.Config(app, host="0.0.0.0", port=port,
                                 log_level="warning",
                                 ws_ping_interval=None, ws_ping_timeout=None)
        server = _uvicorn.Server(config)
        server.run()

    t = threading.Thread(target=_run, daemon=True, name="ws-server")
    t.start()
    print(f"[WS] local_server WebSocket server starting on port {port}", flush=True)
    return t


if __name__ == "__main__":
    import uvicorn
    port = int(os.environ.get("PORT", 5125))
    print(f"Starting local GPU server on port {port}", flush=True)
    if not os.environ.get("RUNPOD_POD_ID") and not os.environ.get("RUNPOD_ENDPOINT_ID"):
        _prewarm()
    else:
        print("RunPod mode: skipping prewarm (handler.py loads models)", flush=True)
    # EC2 registration is handled by the b3-gpu-relay sidecar (entrypoint.sh)
    uvicorn.run(app, host="0.0.0.0", port=port,
                ws_ping_interval=None, ws_ping_timeout=None)
