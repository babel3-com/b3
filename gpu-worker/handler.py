"""
RunPod Serverless GPU Worker — Full voice pipeline on one 48GB GPU.

Models (configurable via preset):
  - faster-whisper (content transcription — the WHAT)
  - WhisperX + PyAnnote (speaker diarization — the WHO)
  - Chatterbox TTS (voice synthesis)
  - nomic-embed-text-v1.5 (semantic embeddings, 768D)

Presets:
  - english-fast (default): medium.en + small.en
  - english-quality:         large-v3 + medium.en
  - multilingual:            large-v3 + large-v3-turbo

Routes by "action" field: "transcribe", "diarize", "synthesize", "synthesize_stream", "embed", "enroll".
Concurrency via Chatterbox pool: multiple model instances, each with
its own lock, round-robin request distribution. Pool size auto-detected
from VRAM or set via CHATTERBOX_POOL_SIZE env var.

Voice conditionals (.pt files) can be pre-baked in /voices/.cache/ for known voices.
This produces cleaner audio than computing from WAV reference on each request.
Voices are mounted at runtime via -v, not baked into the Docker image.
Speaker embeddings are NOT baked in — sent dynamically via "enroll" action.

VRAM budget (48GB A40):
  - Chatterbox TTS pool:        ~3.5GB × N instances
  - All whisper models cached:  ~11.5GB
  - PyAnnote diarization+emb:   ~1.5GB
  - nomic-embed-text-v1.5:      ~0.5GB
  - Headroom:                   ~4GB reserved
  - Auto-detect: (total - 4GB headroom) / 3.5GB / 2 (compute margin), max 6
"""

import base64
import io
import os
import queue
import subprocess
import sys
import tempfile
import time
import traceback
import threading
from concurrent.futures import ThreadPoolExecutor

import numpy as np
import requests
import runpod
import scipy.io.wavfile as wavfile
from scipy.spatial.distance import cosine
import torch

# PyTorch 2.6 changed torch.load default to weights_only=True.
# WhisperX/PyAnnote use omegaconf which isn't in the safe globals list.
# Fix: force weights_only=False for all torch.load calls.
_original_torch_load = torch.load
def _patched_torch_load(*args, **kwargs):
    kwargs["weights_only"] = False
    return _original_torch_load(*args, **kwargs)
torch.load = _patched_torch_load

print(f"Python {sys.version}", flush=True)
print(f"PyTorch {torch.__version__}, CUDA available: {torch.cuda.is_available()}", flush=True)
if torch.cuda.is_available():
    print(f"GPU: {torch.cuda.get_device_name(0)}, VRAM: {torch.cuda.get_device_properties(0).total_memory / 1e9:.1f}GB", flush=True)

# ─────────────────────────────────────────────
# HuggingFace token (needed for PyAnnote gated models)
# ─────────────────────────────────────────────
HF_TOKEN = os.environ.get("HF_TOKEN", "")
print(f"HF_TOKEN set: {bool(HF_TOKEN)}, length: {len(HF_TOKEN)}", flush=True)

# ─────────────────────────────────────────────
# Billing: shared module — credit check/deduct via EC2
# ─────────────────────────────────────────────
from billing import check_credits as _check_credits, deduct_credit as _deduct_credit, \
    report_job_checkpoint as _report_job_checkpoint, BILLABLE_ACTIONS, DEV_MODE, BILLING_URL
import billing
billing.init()

GPU_WORKER_ID = os.environ.get("GPU_WORKER_ID", "") or os.environ.get("RUNPOD_ENDPOINT_ID", "") or "unknown"

# Relay registration probe — called before yielding worker_id to browsers.
# Polls localhost:{RELAY_PORT}/health until the relay reports EC2-connected,
# or until the timeout expires. Prevents browsers from receiving worker_id
# coords before the relay has registered with EC2 (registration race on cold start).
_RELAY_PORT = int(os.environ.get("GPU_RELAY_PORT", "5131"))
_RELAY_PROBE_TIMEOUT = 30.0   # seconds — relay typically registers within 2-5s; 30s closes startup race
_RELAY_PROBE_INTERVAL = 0.5   # seconds

def _wait_for_relay_registration():
    """Block until the relay reports ec2_registered=true in /health, or timeout.

    The relay registers with EC2 via an outbound WS and sets ec2_registered=true
    once that WS connects. Polling /health for that flag ensures browsers won't
    receive gpu_worker_id coords before the relay is known to EC2.

    Without this, a browser can receive gpu_worker_id coords before the relay is
    known to EC2, causing a "no GPU worker registered" error on connect.
    """
    import urllib.request
    import json as _json
    health_url = f"http://localhost:{_RELAY_PORT}/health"
    deadline = time.time() + _RELAY_PROBE_TIMEOUT
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(health_url, timeout=1) as resp:
                body = _json.loads(resp.read())
                if body.get("ec2_registered"):
                    print(f"[Relay] EC2 registration confirmed on port {_RELAY_PORT}", flush=True)
                    return
        except Exception:
            pass
        time.sleep(_RELAY_PROBE_INTERVAL)
    print(f"[Relay] EC2 registration probe timed out after {_RELAY_PROBE_TIMEOUT}s — proceeding anyway", flush=True)

# ─────────────────────────────────────────────
# Transcription presets: name → (faster-whisper model, whisperx model)
# ─────────────────────────────────────────────
PRESETS = {
    "english-fast":    ("medium.en",  "small.en"),
    "english-quality": ("large-v3",   "medium.en"),
    "multilingual":    ("large-v3",   "large-v3-turbo"),
}
DEFAULT_PRESET = "english-fast"

# ─────────────────────────────────────────────
# All models load lazily on first request to avoid startup crashes.
# Models are cached by name so multiple presets share the same instance.
# ─────────────────────────────────────────────
_whisper_models = {}  # model_name → WhisperModel instance
_whisper_lock = threading.Lock()

_whisperx_models = {}  # model_name → whisperx model instance
_whisperx_lock = threading.Lock()
WHISPERX_ALIGN_MODEL = None
WHISPERX_ALIGN_META = None

_diarize_pipeline = None
_diarize_lock = threading.Lock()

_embedding_model = None
_embedding_lock = threading.Lock()

# ─────────────────────────────────────────────
# Chatterbox TTS pool: multiple instances for true concurrency.
# Each instance has its own lock to prevent conds corruption.
# model.conds is mutable instance state — concurrent generate() calls
# on the SAME instance with different conds = voice corruption.
# ─────────────────────────────────────────────
_chatterbox_pool = []       # list of (model, lock) tuples
_chatterbox_pool_lock = threading.Lock()
_chatterbox_pool_index = 0  # round-robin counter
CHATTERBOX_POOL_SIZE = int(os.environ.get("CHATTERBOX_POOL_SIZE", "0"))  # 0 = auto-detect
SAMPLE_RATE = 24000
VOICES_DIR = "/voices"
VOICES_CACHE_DIR = "/voices/.cache"

EMBEDDER = None
_embedder_lock = threading.Lock()


def get_whisper(model_name="medium.en"):
    """Get a faster-whisper model by name, loading lazily and caching."""
    if model_name not in _whisper_models:
        with _whisper_lock:
            if model_name not in _whisper_models:
                print(f"Loading Whisper {model_name}...", flush=True)
                t = time.time()
                from faster_whisper import WhisperModel
                _whisper_models[model_name] = WhisperModel(model_name, device="cuda", compute_type="float16")
                print(f"Whisper {model_name} loaded in {time.time() - t:.1f}s", flush=True)
    return _whisper_models[model_name]


def get_whisperx(model_name="small.en"):
    """Get a WhisperX model by name, loading lazily and caching."""
    if model_name not in _whisperx_models:
        with _whisperx_lock:
            if model_name not in _whisperx_models:
                print(f"Loading WhisperX {model_name}...", flush=True)
                t = time.time()
                import torch, whisperx
                # PyTorch 2.6: weights_only=True by default breaks WhisperX (omegaconf.ListConfig)
                _orig_load = torch.load
                torch.load = lambda *a, **kw: _orig_load(*a, **{**kw, 'weights_only': False})
                try:
                    _whisperx_models[model_name] = whisperx.load_model(model_name, device="cuda", compute_type="float16")
                finally:
                    torch.load = _orig_load
                print(f"WhisperX {model_name} loaded in {time.time() - t:.1f}s", flush=True)
    return _whisperx_models[model_name]


def get_diarize_pipeline():
    global _diarize_pipeline
    if _diarize_pipeline is None:
        with _diarize_lock:
            if _diarize_pipeline is None:
                print("Loading PyAnnote diarization pipeline...", flush=True)
                from pyannote.audio import Pipeline
                _diarize_pipeline = Pipeline.from_pretrained(
                    "pyannote/speaker-diarization-3.1",
                    use_auth_token=HF_TOKEN,
                )
                _diarize_pipeline.to(torch.device("cuda"))
                print("PyAnnote diarization loaded", flush=True)
    return _diarize_pipeline


def get_embedding_model():
    global _embedding_model
    if _embedding_model is None:
        with _embedding_lock:
            if _embedding_model is None:
                print("Loading PyAnnote embedding model...", flush=True)
                from pyannote.audio import Inference
                _embedding_model = Inference(
                    "pyannote/embedding",
                    use_auth_token=HF_TOKEN,
                    window="whole",
                )
                _embedding_model.to(torch.device("cuda"))
                print("PyAnnote embedding model loaded", flush=True)
    return _embedding_model


_pool_size_cache = None

def _detect_pool_size():
    """How many Chatterbox instances to run.
    Default 1 — concurrency didn't show measurable benefit and each instance
    costs ~3.5GB VRAM + 10s cold-start time.
    Override via CHATTERBOX_POOL_SIZE env var."""
    global _pool_size_cache
    if _pool_size_cache is not None:
        return _pool_size_cache
    if CHATTERBOX_POOL_SIZE > 0:
        _pool_size_cache = CHATTERBOX_POOL_SIZE
        return _pool_size_cache
    _pool_size_cache = 1
    return _pool_size_cache


def _init_chatterbox_pool():
    """Initialize the Chatterbox pool (called once, lazily)."""
    global _chatterbox_pool
    with _chatterbox_pool_lock:
        if _chatterbox_pool:
            return  # already initialized
        pool_size = _detect_pool_size()
        print(f"Initializing Chatterbox pool: {pool_size} instances...", flush=True)
        from chatterbox.tts import ChatterboxTTS
        for i in range(pool_size):
            t = time.time()
            model = ChatterboxTTS.from_pretrained(device="cuda")
            lock = threading.Lock()
            _chatterbox_pool.append((model, lock))
            print(f"  Chatterbox instance {i+1}/{pool_size} loaded in {time.time() - t:.1f}s", flush=True)
        if torch.cuda.is_available():
            vram_gb = torch.cuda.memory_allocated() / 1e9
            print(f"Chatterbox pool ready: {pool_size} instances, {vram_gb:.1f}GB VRAM allocated", flush=True)


def acquire_chatterbox():
    """Get a (model, lock) tuple from the pool using round-robin.
    Caller MUST hold the lock during the entire synthesize operation."""
    global _chatterbox_pool_index
    if not _chatterbox_pool:
        _init_chatterbox_pool()
    with _chatterbox_pool_lock:
        idx = _chatterbox_pool_index % len(_chatterbox_pool)
        _chatterbox_pool_index += 1
    return _chatterbox_pool[idx]


# ─────────────────────────────────────────────
# Priority-aware synthesis queue.
# Orders TTS jobs by (chunk_index, arrival_time) so earlier chunks
# generate first, with natural cross-message interleaving.
# ─────────────────────────────────────────────

class PendingJob:
    """A synthesis job waiting for a Chatterbox instance."""
    __slots__ = ('priority', 'fn', 'args', 'kwargs', 'result', 'error', 'done')

    def __init__(self, chunk_index, fn, args, kwargs):
        self.priority = (chunk_index, time.monotonic())
        self.fn = fn
        self.args = args
        self.kwargs = kwargs
        self.result = None
        self.error = None
        self.done = threading.Event()

    def __lt__(self, other):
        return self.priority < other.priority


class SynthesisQueue:
    """Priority queue that schedules TTS jobs across the Chatterbox pool.

    Lower chunk_index = higher priority. Ties broken by arrival time.
    When a pool instance finishes, the highest-priority waiting job runs next.
    """

    def __init__(self):
        self._queue = queue.PriorityQueue()
        self._started = False
        self._start_lock = threading.Lock()
        self._workers = []

    def _ensure_started(self):
        """Start worker threads (one per pool instance) on first use."""
        if self._started:
            return
        with self._start_lock:
            if self._started:  # double-check after acquiring lock
                return
            if not _chatterbox_pool:
                _init_chatterbox_pool()
            for idx, (model, lock) in enumerate(_chatterbox_pool):
                t = threading.Thread(
                    target=self._worker_loop, args=(idx, model, lock),
                    daemon=True, name=f"synth-worker-{idx}"
                )
                t.start()
                self._workers.append(t)
            self._started = True
            print(f"SynthesisQueue started with {len(self._workers)} workers", flush=True)

    def _worker_loop(self, idx, model, lock):
        """Each worker owns one Chatterbox instance and pulls from the shared queue."""
        while True:
            job = self._queue.get()  # blocks until a job is available
            try:
                with lock:
                    job.result = job.fn(model, *job.args, **job.kwargs)
            except Exception as e:
                job.error = e
            finally:
                job.done.set()
                self._queue.task_done()

    def submit(self, chunk_index, fn, *args, **kwargs):
        """Submit a job with priority. Blocks until complete. Returns result."""
        self._ensure_started()
        job = PendingJob(chunk_index, fn, args, kwargs)
        self._queue.put(job)
        job.done.wait()
        if job.error is not None:
            raise job.error
        return job.result


_synthesis_queue = SynthesisQueue()


def get_embedder():
    global EMBEDDER
    if EMBEDDER is None:
        with _embedder_lock:
            if EMBEDDER is None:
                print("Loading embedding model...", flush=True)
                t = time.time()
                from sentence_transformers import SentenceTransformer
                EMBEDDER = SentenceTransformer("nomic-ai/nomic-embed-text-v1.5", trust_remote_code=True, device="cuda")
                print(f"Embedding model loaded in {time.time() - t:.1f}s", flush=True)
    return EMBEDDER


# ─────────────────────────────────────────────
# Speaker enrollment (dynamic — sent by daemon)
# Keyed by agent_id so each daemon has its own speakers.
# ─────────────────────────────────────────────
# { "warm-spark-101": {"YANIV": np.array(...), "RACHEL": np.array(...)}, ... }
ENROLLED_SPEAKERS = {}
_enroll_lock = threading.Lock()
SPEAKER_MATCH_THRESHOLD = 0.3


def match_speaker(embedding, agent_id=""):
    """Match a speaker embedding against enrolled speakers for a specific agent."""
    speakers = ENROLLED_SPEAKERS.get(agent_id, {})
    best_name = "UNKNOWN"
    best_score = -1.0
    for name, enrolled_emb in speakers.items():
        try:
            sim = 1.0 - cosine(embedding, enrolled_emb)
            if sim > best_score:
                best_score = sim
                best_name = name
        except Exception:
            continue
    return best_name, round(best_score, 3)


# ─────────────────────────────────────────────
# Audio utilities
# ─────────────────────────────────────────────

def convert_to_wav_16k(input_path):
    """Convert any audio format to 16kHz mono WAV for WhisperX."""
    out_path = input_path + ".16k.wav"
    subprocess.run(
        ["ffmpeg", "-y", "-i", input_path, "-ar", "16000", "-ac", "1", out_path],
        capture_output=True, timeout=30,
    )
    return out_path


# ─────────────────────────────────────────────
# Handlers
# ─────────────────────────────────────────────

def _run_whisper_medium(audio_path, language, model_name="medium.en", progress_q=None):
    """Run faster-whisper transcription (thread-safe).

    If progress_q is provided, puts per-segment progress events as segments
    are decoded. Each event: {"stage": "transcribing", "segments": N, "tokens": T, ...}
    """
    model = get_whisper(model_name)
    with _whisper_lock:
        start = time.time()
        if progress_q:
            progress_q.put({"stage": "whisper_transcribing", "model": model_name})
        segments, info = model.transcribe(
            audio_path,
            language=language,
            beam_size=5,
            vad_filter=True,
            vad_parameters=dict(min_silence_duration_ms=500, onset=0.15),
        )
        segment_list = []
        total_tokens = 0
        for seg in segments:
            total_tokens += len(seg.tokens)
            segment_list.append({
                "start": round(seg.start, 2),
                "end": round(seg.end, 2),
                "text": seg.text.strip(),
            })
            if progress_q:
                progress_q.put({
                    "stage": "transcribing",
                    "segments": len(segment_list),
                    "tokens": total_tokens,
                    "latest_text": seg.text.strip()[:80],
                    "audio_pos": round(seg.end, 1),
                })
        full_text = " ".join(s["text"] for s in segment_list)
        elapsed = time.time() - start
        if progress_q:
            progress_q.put({"stage": "whisper_done", "segments": len(segment_list), "tokens": total_tokens, "elapsed": round(elapsed, 2)})
    return {
        "text": full_text,
        "segments": segment_list,
        "elapsed": round(elapsed, 2),
    }


def _run_whisperx_diarize(audio_path, agent_id="", model_name="small.en", progress_q=None):
    """Run WhisperX + PyAnnote diarization (speaker identification).

    If progress_q is provided, puts stage progress events as each pipeline
    step starts: whisperx_transcribing, whisperx_aligning, diarizing, matching_speakers.
    """
    global WHISPERX_ALIGN_MODEL, WHISPERX_ALIGN_META

    start = time.time()
    wav_path = convert_to_wav_16k(audio_path)

    try:
        import whisperx
        audio = whisperx.load_audio(wav_path)

        # Step 1: WhisperX transcription
        if progress_q:
            progress_q.put({"stage": "whisperx_transcribing", "model": model_name})
        wx_model = get_whisperx(model_name)
        result = wx_model.transcribe(audio, batch_size=8)

        # Step 2: Word-level alignment
        if progress_q:
            progress_q.put({"stage": "whisperx_aligning"})
        import whisperx
        if WHISPERX_ALIGN_MODEL is None:
            WHISPERX_ALIGN_MODEL, WHISPERX_ALIGN_META = whisperx.load_align_model(
                language_code="en", device="cuda"
            )
        result = whisperx.align(
            result["segments"], WHISPERX_ALIGN_MODEL, WHISPERX_ALIGN_META,
            audio, device="cuda", return_char_alignments=False,
        )

        # Step 3: Diarization
        # PyAnnote 3.x returns Annotation objects, but WhisperX expects DataFrames.
        # Convert before passing to assign_word_speakers.
        if progress_q:
            progress_q.put({"stage": "diarizing"})
        diarize_error = None
        try:
            import pandas as pd
            diarize_pipeline = get_diarize_pipeline()
            diarize_result = diarize_pipeline(wav_path)

            # Convert Annotation → DataFrame if needed
            if hasattr(diarize_result, 'itertracks'):
                segments_list = []
                for segment, _, speaker in diarize_result.itertracks(yield_label=True):
                    segments_list.append({
                        "start": segment.start,
                        "end": segment.end,
                        "speaker": speaker,
                    })
                diarize_segments = pd.DataFrame(segments_list)
                print(f"Diarization found {len(segments_list)} segments", flush=True)
            else:
                diarize_segments = diarize_result

            result = whisperx.assign_word_speakers(diarize_segments, result)
        except Exception as e:
            diarize_error = f"{type(e).__name__}: {e}"
            print(f"Diarization failed (non-fatal): {diarize_error}", flush=True)
            traceback.print_exc()

        # Step 4: Speaker matching via embeddings
        speakers_found = set()
        for seg in result.get("segments", []):
            spk = seg.get("speaker", "")
            if spk:
                speakers_found.add(spk)

        # Map SPEAKER_00 etc. to enrolled names for this agent
        agent_speakers = ENROLLED_SPEAKERS.get(agent_id, {})
        speaker_map = {}
        if speakers_found and agent_speakers:
            if progress_q:
                progress_q.put({"stage": "matching_speakers", "speakers_found": len(speakers_found)})
            try:
                import torchaudio
                emb_model = get_embedding_model()
                waveform, sr = torchaudio.load(wav_path)

                for seg in result.get("segments", []):
                    spk = seg.get("speaker", "")
                    if spk and spk not in speaker_map:
                        seg_start = seg.get("start", 0)
                        seg_end = seg.get("end", 0)
                        if seg_end - seg_start < 0.3:
                            continue
                        # Extract segment audio
                        start_sample = int(seg_start * sr)
                        end_sample = int(seg_end * sr)
                        segment_wav = waveform[:, start_sample:end_sample]

                        # Get embedding
                        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
                            torchaudio.save(tmp.name, segment_wav, sr)
                            embedding = emb_model(tmp.name)
                            os.unlink(tmp.name)

                        # PyAnnote Inference returns SlidingWindowFeature
                        # whose .data is a numpy array (not a torch tensor)
                        if hasattr(embedding, 'data'):
                            raw = embedding.data
                            if isinstance(raw, np.ndarray):
                                embedding = raw.flatten()
                            elif isinstance(raw, memoryview):
                                embedding = np.frombuffer(raw, dtype=np.float32).flatten()
                            elif hasattr(raw, 'cpu'):
                                embedding = raw.cpu().numpy().flatten()
                            else:
                                embedding = np.array(raw).flatten()
                        elif isinstance(embedding, np.ndarray):
                            embedding = embedding.flatten()
                        else:
                            embedding = np.array(embedding).flatten()

                        name, score = match_speaker(embedding, agent_id)
                        speaker_map[spk] = {"name": name, "score": score}
            except Exception as e:
                print(f"Speaker matching failed (non-fatal): {e}")

        # Build output text with speaker tags (deduplicate consecutive same-speaker tags)
        parts = []
        prev_tag = ""
        for seg in result.get("segments", []):
            text = seg.get("text", "").strip()
            spk = seg.get("speaker", "")
            if spk and spk in speaker_map:
                tag = speaker_map[spk]["name"]
            elif spk:
                tag = spk
            else:
                tag = ""

            if tag and tag != prev_tag:
                parts.append(f"[{tag}] {text}")
            else:
                parts.append(text)
            prev_tag = tag

        full_text = " ".join(parts)
        elapsed = time.time() - start

        result_out = {
            "text": full_text,
            "speakers": speaker_map,
            "elapsed": round(elapsed, 2),
        }
        if diarize_error:
            result_out["diarize_error"] = diarize_error
        return result_out
    finally:
        if os.path.exists(wav_path):
            os.unlink(wav_path)


def _run_whisperx_align(audio_path, model_name="small.en", progress_q=None, skip_align=False):
    """Run WhisperX transcription + optional word-level alignment.

    When skip_align=True, only runs transcription (for self-similarity comparison).
    When skip_align=False, also runs word-level alignment for diarization.
    Returns text + word-level timestamps for later diarization by handle_diarize().
    """
    global WHISPERX_ALIGN_MODEL, WHISPERX_ALIGN_META

    start = time.time()
    wav_path = convert_to_wav_16k(audio_path)

    try:
        import whisperx
        audio = whisperx.load_audio(wav_path)

        # Step 1: WhisperX transcription
        if progress_q:
            progress_q.put({"stage": "whisperx_transcribing", "model": model_name})
        wx_model = get_whisperx(model_name)
        result = wx_model.transcribe(audio, batch_size=8)

        # Step 2: Word-level alignment (skip when diarization is off)
        if not skip_align:
            if progress_q:
                progress_q.put({"stage": "whisperx_aligning"})
            if WHISPERX_ALIGN_MODEL is None:
                WHISPERX_ALIGN_MODEL, WHISPERX_ALIGN_META = whisperx.load_align_model(
                    language_code="en", device="cuda"
                )
            result = whisperx.align(
                result["segments"], WHISPERX_ALIGN_MODEL, WHISPERX_ALIGN_META,
                audio, device="cuda", return_char_alignments=False,
            )

        # Build text from segments
        parts = []
        for seg in result.get("segments", []):
            text = seg.get("text", "").strip()
            if text:
                parts.append(text)
        full_text = " ".join(parts)

        # Extract word-level timestamps for diarization
        word_segments = []
        for seg in result.get("segments", []):
            for word in seg.get("words", []):
                word_segments.append({
                    "word": word.get("word", ""),
                    "start": round(word.get("start", 0), 3),
                    "end": round(word.get("end", 0), 3),
                })

        elapsed = time.time() - start
        return {
            "text": full_text,
            "word_segments": word_segments,
            "elapsed": round(elapsed, 2),
        }
    finally:
        if os.path.exists(wav_path):
            os.unlink(wav_path)


def handle_transcribe(input_data):
    """Dual transcription: faster-whisper (content) + WhisperX (similarity check) in parallel.

    No diarization — that's a separate action (handle_diarize).
    When skip_align=True, WhisperX runs transcription only (no word alignment),
    saving ~30-50% of WhisperX time. Alignment is only needed for diarization.
    Generator handler: yields progress dicts, then the final result dict.
    """
    audio_b64 = input_data.get("audio_b64", "")
    language = input_data.get("language", "en")
    agent_id = input_data.get("agent_id", "")
    skip_align = input_data.get("skip_align", False)

    # Resolve preset to model names
    preset_name = input_data.get("preset", DEFAULT_PRESET)
    if preset_name not in PRESETS:
        print(f"Unknown preset '{preset_name}', falling back to {DEFAULT_PRESET}", flush=True)
        preset_name = DEFAULT_PRESET
    whisper_model, whisperx_model = PRESETS[preset_name]
    print(f"Transcribe preset={preset_name} → whisper={whisper_model}, whisperx={whisperx_model}, skip_align={skip_align}", flush=True)

    # Inline speaker enrollment: if speakers dict is provided, enroll them now.
    # This ensures the same worker that transcribes also has the embeddings.
    speakers_b64 = input_data.get("speakers")
    if speakers_b64 and agent_id:
        loaded = {}
        for name, emb_b64 in speakers_b64.items():
            try:
                npy_bytes = base64.b64decode(emb_b64)
                arr = np.load(io.BytesIO(npy_bytes))
                loaded[name.upper()] = arr
            except Exception as e:
                print(f"Inline enroll: failed to load {name}: {e}")
        if loaded:
            with _enroll_lock:
                ENROLLED_SPEAKERS[agent_id] = loaded
            print(f"Inline enrolled {len(loaded)} speakers for '{agent_id}': {list(loaded.keys())}")

    if not audio_b64:
        yield {"error": "Missing audio_b64 field"}
        return

    audio_bytes = base64.b64decode(audio_b64)
    if len(audio_bytes) == 0:
        yield {"error": "Empty audio data"}
        return

    # Ensure relay has registered with EC2 before advertising gpu_worker_id to browsers.
    _wait_for_relay_registration()

    # Report GPU state: warm if whisper model already cached, cold otherwise
    _gpu_state = "warm" if whisper_model in _whisper_models else "cold"
    yield {"status": "accepted", "state": _gpu_state, "gpu_worker_id": GPU_WORKER_ID}

    with tempfile.NamedTemporaryFile(suffix=".webm", delete=False) as f:
        f.write(audio_bytes)
        audio_path = f.name

    try:
        yield {"progress": {"stage": "started", "audio_size": len(audio_bytes)}}

        progress_q = queue.Queue()

        # Run both models in parallel: faster-whisper (content) + WhisperX (similarity + optional alignment)
        with ThreadPoolExecutor(max_workers=2) as pool:
            medium_future = pool.submit(_run_whisper_medium, audio_path, language, whisper_model, progress_q)
            wx_future = pool.submit(_run_whisperx_align, audio_path, whisperx_model, progress_q, skip_align)

            # Poll progress queue while workers run
            while not (medium_future.done() and wx_future.done()):
                try:
                    evt = progress_q.get(timeout=0.5)
                    yield {"progress": evt}
                except queue.Empty:
                    pass

            # Drain any remaining events
            while not progress_q.empty():
                try:
                    yield {"progress": progress_q.get_nowait()}
                except queue.Empty:
                    break

            medium_result = medium_future.result(timeout=120)
            try:
                wx_result = wx_future.result(timeout=120)
            except Exception as e:
                print(f"WhisperX failed, using medium.en only: {e}", flush=True)
                traceback.print_exc()
                wx_result = {"text": "", "word_segments": [], "elapsed": 0, "error": str(e)}

        # Compute embedding similarity between the two transcriptions
        yield {"progress": {"stage": "computing_similarity"}}
        medium_text = medium_result["text"].strip()
        wx_text = wx_result["text"].strip()
        embedding_sim = None
        if medium_text and wx_text:
            try:
                embedder = get_embedder()
                vecs = embedder.encode([
                    f"search_document: {medium_text}",
                    f"search_document: {wx_text}",
                ], convert_to_numpy=True)
                embedding_sim = round(1.0 - float(cosine(vecs[0], vecs[1])), 4)
            except Exception as e:
                print(f"Embedding similarity failed: {e}", flush=True)

        result = {
            "text": medium_text,  # primary content (trust this)
            "segments": medium_result.get("segments", []),
            "medium_elapsed": medium_result["elapsed"],
            "whisperx_text": wx_text,  # aligned text (no speaker tags)
            "word_segments": wx_result.get("word_segments", []),  # for diarization
            "whisperx_elapsed": wx_result.get("elapsed", 0),
            "embedding_similarity": embedding_sim,
            "preset": preset_name,
        }
        if "error" in wx_result:
            result["whisperx_error"] = wx_result["error"]
        yield result
    finally:
        os.unlink(audio_path)


def handle_diarize(input_data):
    """Run PyAnnote diarization + speaker matching on full audio.

    Separate from transcription — called by browser after all chunks are transcribed.
    Takes the full concatenated audio + pre-computed word segments (with absolute timestamps)
    from prior transcribe calls.

    Input:
        audio_b64: str — base64-encoded full audio (all chunks concatenated)
        agent_id: str — for enrolled speaker matching
        word_segments: list — [{word, start, end}, ...] with absolute timestamps
        speakers: dict (optional) — inline speaker enrollment {name: emb_b64}

    Output:
        speaker_segments: [{start, end, speaker}, ...] — raw PyAnnote segments
        speaker_map: {SPEAKER_00: {name, score}, ...} — enrolled name matches
        word_speakers: [{word, start, end, speaker}, ...] — words with speaker labels
    """
    audio_b64 = input_data.get("audio_b64", "")
    agent_id = input_data.get("agent_id", "")
    word_segments = input_data.get("word_segments", [])

    # Inline speaker enrollment
    speakers_b64 = input_data.get("speakers")
    if speakers_b64 and agent_id:
        loaded = {}
        for name, emb_b64 in speakers_b64.items():
            try:
                npy_bytes = base64.b64decode(emb_b64)
                arr = np.load(io.BytesIO(npy_bytes))
                loaded[name.upper()] = arr
            except Exception as e:
                print(f"Inline enroll: failed to load {name}: {e}")
        if loaded:
            with _enroll_lock:
                ENROLLED_SPEAKERS[agent_id] = loaded
            print(f"Diarize: inline enrolled {len(loaded)} speakers for '{agent_id}'")

    if not audio_b64:
        yield {"error": "Missing audio_b64 field"}
        return

    audio_bytes = base64.b64decode(audio_b64)
    if len(audio_bytes) == 0:
        yield {"error": "Empty audio data"}
        return

    # Ensure relay has registered with EC2 before advertising gpu_worker_id to browsers.
    _wait_for_relay_registration()

    # Report GPU state — diarization uses PyAnnote which is always loaded at startup
    yield {"status": "accepted", "state": "warm", "gpu_worker_id": GPU_WORKER_ID}

    with tempfile.NamedTemporaryFile(suffix=".webm", delete=False) as f:
        f.write(audio_bytes)
        audio_path = f.name

    try:
        yield {"progress": {"stage": "diarize_started", "audio_size": len(audio_bytes),
                             "word_segments": len(word_segments)}}

        start = time.time()
        wav_path = convert_to_wav_16k(audio_path)

        try:
            import pandas as pd

            # Step 1: Run PyAnnote diarization on full audio
            yield {"progress": {"stage": "diarizing"}}
            diarize_pipeline = get_diarize_pipeline()
            diarize_result = diarize_pipeline(wav_path)

            # Convert Annotation → list of segments
            raw_segments = []
            if hasattr(diarize_result, 'itertracks'):
                for segment, _, speaker in diarize_result.itertracks(yield_label=True):
                    raw_segments.append({
                        "start": round(segment.start, 3),
                        "end": round(segment.end, 3),
                        "speaker": speaker,
                    })
                print(f"Diarize: PyAnnote found {len(raw_segments)} segments", flush=True)

            # Step 2: Assign speakers to word segments using timestamps
            yield {"progress": {"stage": "assigning_speakers"}}
            diarize_df = pd.DataFrame(raw_segments) if raw_segments else pd.DataFrame(columns=["start", "end", "speaker"])

            # Build word→speaker mapping using overlap
            word_speakers = []
            for ws in word_segments:
                w_start = ws.get("start", 0)
                w_end = ws.get("end", 0)
                w_mid = (w_start + w_end) / 2
                best_speaker = ""
                # Find the diarization segment that contains the word midpoint
                for seg in raw_segments:
                    if seg["start"] <= w_mid <= seg["end"]:
                        best_speaker = seg["speaker"]
                        break
                word_speakers.append({
                    "word": ws.get("word", ""),
                    "start": ws.get("start", 0),
                    "end": ws.get("end", 0),
                    "speaker": best_speaker,
                })

            # Step 3: Match speakers against enrolled embeddings
            speakers_found = set(s["speaker"] for s in raw_segments if s["speaker"])
            agent_speakers = ENROLLED_SPEAKERS.get(agent_id, {})
            speaker_map = {}

            if speakers_found and agent_speakers:
                yield {"progress": {"stage": "matching_speakers", "speakers_found": len(speakers_found)}}
                try:
                    import torchaudio
                    emb_model = get_embedding_model()
                    waveform, sr = torchaudio.load(wav_path)

                    for seg in raw_segments:
                        spk = seg.get("speaker", "")
                        if spk and spk not in speaker_map:
                            seg_start = seg.get("start", 0)
                            seg_end = seg.get("end", 0)
                            if seg_end - seg_start < 0.3:
                                continue
                            start_sample = int(seg_start * sr)
                            end_sample = int(seg_end * sr)
                            segment_wav = waveform[:, start_sample:end_sample]

                            with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
                                torchaudio.save(tmp.name, segment_wav, sr)
                                embedding = emb_model(tmp.name)
                                os.unlink(tmp.name)

                            if hasattr(embedding, 'data'):
                                raw = embedding.data
                                if isinstance(raw, np.ndarray):
                                    embedding = raw.flatten()
                                elif isinstance(raw, memoryview):
                                    embedding = np.frombuffer(raw, dtype=np.float32).flatten()
                                elif hasattr(raw, 'cpu'):
                                    embedding = raw.cpu().numpy().flatten()
                                else:
                                    embedding = np.array(raw).flatten()
                            elif isinstance(embedding, np.ndarray):
                                embedding = embedding.flatten()
                            else:
                                embedding = np.array(embedding).flatten()

                            name, score = match_speaker(embedding, agent_id)
                            speaker_map[spk] = {"name": name, "score": score}
                except Exception as e:
                    print(f"Speaker matching failed (non-fatal): {e}", flush=True)

            elapsed = time.time() - start
            yield {
                "speaker_segments": raw_segments,
                "speaker_map": speaker_map,
                "word_speakers": word_speakers,
                "elapsed": round(elapsed, 2),
            }
        finally:
            if os.path.exists(wav_path):
                os.unlink(wav_path)
    finally:
        os.unlink(audio_path)


def _trim_trailing_noise(wav_array, sample_rate=24000, frame_ms=20, threshold_db=-35,
                         min_silence_ms=150):
    """Trim trailing silence/noise from audio — only when there's a real silent gap.

    Only trims if there's a contiguous silent region at the end lasting at least
    min_silence_ms. This prevents clipping the natural fade-out of speech
    (e.g., the tail of "operational") while still removing actual trailing artifacts.

    Returns trimmed array (or original if no significant trailing silence found).
    """
    frame_size = int(sample_rate * frame_ms / 1000)
    min_silent_frames = int(min_silence_ms / frame_ms)  # 150ms = ~7-8 frames

    if len(wav_array) < frame_size * 2:
        return wav_array

    # Convert threshold from dB to linear
    threshold = 10 ** (threshold_db / 20)

    # Count consecutive silent frames from the end
    silent_frames = 0
    for i in range(len(wav_array) - frame_size, 0, -frame_size):
        frame = wav_array[i:i + frame_size]
        rms = np.sqrt(np.mean(frame ** 2))
        if rms > threshold:
            break
        silent_frames += 1

    # Only trim if the trailing silence is long enough to be an artifact
    if silent_frames >= min_silent_frames:
        # Keep audio up to the last active frame + 2 frames (40ms) for natural decay
        trim_point = len(wav_array) - (silent_frames * frame_size) + (frame_size * 2)
        trim_point = min(trim_point, len(wav_array))
        return wav_array[:trim_point]

    return wav_array


def _tensor_to_wav(audio_tensor, trim_noise=False):
    """Convert torch.Tensor or numpy audio to WAV bytes. Returns (wav_bytes, n_samples)."""
    if isinstance(audio_tensor, torch.Tensor):
        wav_array = audio_tensor.cpu().numpy()
    else:
        wav_array = audio_tensor
    if wav_array.ndim > 1:
        wav_array = wav_array.squeeze()
    wav_array = np.clip(wav_array, -1.0, 1.0)
    if trim_noise:
        wav_array = _trim_trailing_noise(wav_array, SAMPLE_RATE)
    wav_int16 = (wav_array * 32767).astype(np.int16)
    buf = io.BytesIO()
    wavfile.write(buf, SAMPLE_RATE, wav_int16)
    return buf.getvalue(), len(wav_int16)


def _load_cached_conditionals(model, voice):
    """Load pre-baked conditionals from /voices/.cache/{voice}.pt if available.

    Returns True if cached conditionals were loaded onto model.conds,
    False if no cache exists (caller should fall back to WAV).
    """
    cache_path = f"{VOICES_CACHE_DIR}/{voice}.pt"
    if not os.path.exists(cache_path):
        return False
    from chatterbox.tts import Conditionals
    conds = Conditionals.load(cache_path, map_location=model.device)
    model.conds = conds
    return True


def _do_synthesize(model, text, voice, conditionals_b64, exaggeration, cfg_weight,
                   temperature=0.8, repetition_penalty=1.2, min_p=0.05, top_p=1.0):
    """Core synthesis on a specific Chatterbox model instance.

    Called either by SynthesisQueue workers (priority path) or directly
    via acquire_chatterbox() (backward-compat path). The model lock is
    already held by the caller.
    """
    gen_kwargs = dict(
        cfg_weight=cfg_weight,
        temperature=temperature,
        repetition_penalty=repetition_penalty,
        min_p=min_p,
        top_p=top_p,
    )
    if conditionals_b64:
        pt_bytes = base64.b64decode(conditionals_b64)
        with tempfile.NamedTemporaryFile(suffix=".pt", delete=False) as tmp:
            tmp.write(pt_bytes)
            tmp_path = tmp.name
        try:
            from chatterbox.tts import Conditionals
            conds = Conditionals.load(tmp_path, map_location=model.device)
            model.conds = conds
            wav_array = model.generate(text, **gen_kwargs)
        finally:
            os.unlink(tmp_path)
    elif _load_cached_conditionals(model, voice):
        # Pre-baked conditionals: cleaner audio than WAV reference
        wav_array = model.generate(text, **gen_kwargs)
    else:
        voice_path = f"{VOICES_DIR}/{voice}.wav"
        if not os.path.exists(voice_path):
            voice_path = f"{VOICES_DIR}/zara.wav"
        wav_array = model.generate(
            text,
            audio_prompt_path=voice_path,
            exaggeration=exaggeration,
            **gen_kwargs,
        )

    return _tensor_to_wav(wav_array, trim_noise=False)  # trimming moved to browser


def handle_synthesize(input_data):
    """Chatterbox TTS: text → audio_b64.

    Supports voice cloning via conditionals and priority scheduling via
    msg_id + chunk_index. When msg_id is present, requests are routed
    through SynthesisQueue which orders by chunk_index (lower = higher
    priority) so earlier chunks generate first.
    """
    text = input_data.get("text", "")
    voice = input_data.get("voice", "zara")
    conditionals_b64 = input_data.get("conditionals_b64", "")
    exaggeration = input_data.get("exaggeration", 0.5)
    cfg_weight = input_data.get("cfg_weight", 0.5)
    temperature = input_data.get("temperature", 0.8)
    repetition_penalty = input_data.get("repetition_penalty", 1.2)
    min_p = input_data.get("min_p", 0.05)
    top_p = input_data.get("top_p", 1.0)
    msg_id = input_data.get("msg_id", "")
    chunk_index = input_data.get("chunk", input_data.get("chunk_index", 0))

    if not text:
        return {"error": "Missing text field"}

    start = time.time()

    try:
        if msg_id:
            # Priority queue path: ordered by chunk_index within msg_id
            print(f"[SYNTH] msg={msg_id} chunk={chunk_index} queued", flush=True)
            wav_bytes, n_samples = _synthesis_queue.submit(
                chunk_index,
                _do_synthesize, text, voice, conditionals_b64,
                exaggeration, cfg_weight,
                temperature, repetition_penalty, min_p, top_p,
            )
            print(f"[SYNTH] msg={msg_id} chunk={chunk_index} done ({time.time() - start:.1f}s)", flush=True)
        else:
            # Backward-compat: direct round-robin for non-chunked requests
            model, model_lock = acquire_chatterbox()
            with model_lock:
                wav_bytes, n_samples = _do_synthesize(
                    model, text, voice, conditionals_b64,
                    exaggeration, cfg_weight,
                    temperature, repetition_penalty, min_p, top_p,
                )
    except Exception as e:
        traceback.print_exc()
        return {"error": f"Synthesis failed: {str(e)}"}

    elapsed = time.time() - start
    duration_sec = n_samples / SAMPLE_RATE

    return {
        "audio_b64": base64.b64encode(wav_bytes).decode("ascii"),
        "sample_rate": SAMPLE_RATE,
        "duration_sec": round(duration_sec, 2),
        "generation_sec": round(elapsed, 2),
    }


def _split_sentences(text, split_chars=".!?", min_chars=80):
    """Split text into sentences at natural boundaries.

    Args:
        text: Input text to split
        split_chars: Characters to split on (each followed by space).
                     Default ".!?" = sentence endings only.
                     Add ",;—" for clause-level splitting.
        min_chars: Minimum characters per chunk. Short sentences get merged
                   with the next one until this threshold is reached.
                   Default 80. Set to 0 to disable merging.
    """
    import re
    # Build regex from split_chars — split after any of these chars followed by space
    escaped = re.escape(split_chars)
    parts = re.split(r'(?<=[' + escaped + r'])\s+', text.strip())

    # Merge short fragments to reduce chunk boundary artifacts
    merged = []
    for part in parts:
        part = part.strip()
        if not part:
            continue
        if merged and min_chars > 0 and len(merged[-1]) < min_chars:
            merged[-1] = merged[-1] + " " + part
        else:
            merged.append(part)

    # Final pass: merge last chunk if too short
    if len(merged) > 1 and min_chars > 0 and len(merged[-1]) < min_chars:
        merged[-2] = merged[-2] + " " + merged[-1]
        merged.pop()

    return merged if merged else [text]


def handle_synthesize_stream(input_data):
    """Streaming TTS via sentence-level chunking. Yields audio per sentence.

    Splits text at natural speech boundaries (sentences, clauses), then
    generates each segment using regular model.generate(). This produces
    clean, artifact-free audio because each chunk is a complete utterance.

    This is a generator handler: RunPod accumulates yields at /stream/{job_id}.
    """
    text = input_data.get("text", "")
    voice = input_data.get("voice", "zara")
    conditionals_b64 = input_data.get("conditionals_b64", "")
    exaggeration = input_data.get("exaggeration", 0.5)
    cfg_weight = input_data.get("cfg_weight", 0.5)
    temperature = input_data.get("temperature", 0.8)
    repetition_penalty = input_data.get("repetition_penalty", 1.2)
    min_p = input_data.get("min_p", 0.05)
    top_p = input_data.get("top_p", 1.0)
    msg_id = input_data.get("msg_id", "")
    split_chars = input_data.get("split_chars", ".!?")
    min_chunk_chars = input_data.get("min_chunk_chars", 80)

    if not text:
        yield {"error": "Missing text field"}
        return

    # Ensure relay has registered with EC2 before advertising gpu_worker_id to browsers.
    _wait_for_relay_registration()

    # Report GPU state: warm if chatterbox pool already initialized, cold otherwise
    _gpu_state = "warm" if _chatterbox_pool else "cold"
    yield {"status": "accepted", "state": _gpu_state, "gpu_worker_id": GPU_WORKER_ID}

    print(f"[STREAM] msg={msg_id} params: temp={temperature} rep_pen={repetition_penalty} min_p={min_p} top_p={top_p} cfg={cfg_weight} exag={exaggeration}", flush=True)
    yield {"progress": {"stage": "splitting", "msg_id": msg_id}}
    sentences = _split_sentences(text, split_chars=split_chars, min_chars=min_chunk_chars)
    print(f"[STREAM] Split into {len(sentences)} sentences for msg={msg_id}", flush=True)
    for i, s in enumerate(sentences):
        print(f"[STREAM]   {i}: ({len(s)} chars) {s[:60]}...", flush=True)
    yield {"progress": {"stage": "split_done", "chunks": len(sentences), "msg_id": msg_id}}

    pool_cold = not _chatterbox_pool  # True if pool needs initialization
    if pool_cold:
        yield {"progress": {"stage": "loading_tts_model", "msg_id": msg_id}}
    model, model_lock = acquire_chatterbox()

    tmp_path = None
    try:
        # Set up voice/conditionals once
        if conditionals_b64:
            yield {"progress": {"stage": "loading_voice", "voice": voice, "method": "conditionals", "msg_id": msg_id}}
            pt_bytes = base64.b64decode(conditionals_b64)
            with tempfile.NamedTemporaryFile(suffix=".pt", delete=False) as tmp:
                tmp.write(pt_bytes)
                tmp_path = tmp.name
            from chatterbox.tts import Conditionals
            conds = Conditionals.load(tmp_path, map_location=model.device)

        start_time = time.time()
        total_audio_sec = 0

        yield {"progress": {"stage": "waiting_for_model", "msg_id": msg_id}}
        with model_lock:
            # Prepare voice conditionals ONCE before the loop
            if conditionals_b64:
                model.conds = conds
            elif _load_cached_conditionals(model, voice):
                yield {"progress": {"stage": "voice_ready", "voice": voice, "method": "cached", "msg_id": msg_id}}
            else:
                yield {"progress": {"stage": "loading_voice", "voice": voice, "method": "wav", "msg_id": msg_id}}
                voice_path = f"{VOICES_DIR}/{voice}.wav"
                if not os.path.exists(voice_path):
                    voice_path = f"{VOICES_DIR}/zara.wav"
                model.prepare_conditionals(voice_path, exaggeration=exaggeration)

            for i, sentence in enumerate(sentences):
                yield {"progress": {"stage": "generating", "chunk": i + 1, "total": len(sentences), "text_preview": sentence[:60]}}
                chunk_start = time.time()
                wav_array = model.generate(
                    sentence, cfg_weight=cfg_weight,
                    temperature=temperature, repetition_penalty=repetition_penalty,
                    min_p=min_p, top_p=top_p,
                )

                wav_bytes, n_samples = _tensor_to_wav(wav_array, trim_noise=False)  # trimming moved to browser
                chunk_dur = n_samples / SAMPLE_RATE
                chunk_elapsed = time.time() - chunk_start
                total_audio_sec += chunk_dur

                print(f"[STREAM] Sentence {i+1}/{len(sentences)}: "
                      f"{chunk_dur:.1f}s audio in {chunk_elapsed:.1f}s "
                      f"(RTF={chunk_elapsed/chunk_dur:.2f})", flush=True)

                yield {
                    "chunk_index": i,
                    "audio_b64": base64.b64encode(wav_bytes).decode("ascii"),
                    "sample_rate": SAMPLE_RATE,
                    "duration_sec": round(chunk_dur, 2),
                    "generation_sec": round(chunk_elapsed, 2),
                    "rtf": round(chunk_elapsed / chunk_dur, 3) if chunk_dur > 0 else 0,
                    "msg_id": msg_id,
                }

                if i == 0:
                    print(f"Latency to first chunk: {time.time() - start_time:.3f}s", flush=True)

        total_elapsed = time.time() - start_time
        print(f"Total generation time: {total_elapsed:.3f}s", flush=True)
        print(f"Total audio duration: {total_audio_sec:.3f}s", flush=True)
        print(f"RTF (Real-Time Factor): {total_elapsed/total_audio_sec:.3f}", flush=True)
        print(f"Total chunks yielded: {len(sentences)}", flush=True)

        yield {"done": True, "total_chunks": len(sentences), "msg_id": msg_id}
    except Exception as e:
        traceback.print_exc()
        yield {"error": f"Streaming synthesis failed: {str(e)}", "msg_id": msg_id}
    finally:
        if tmp_path:
            try:
                os.unlink(tmp_path)
            except Exception:
                pass


def handle_embed(input_data):
    """Sentence embeddings: texts → vectors (nomic-embed-text, 768D)."""
    texts = input_data.get("texts", [])
    task_type = input_data.get("task_type", "search_document")  # or "search_query"

    if not texts:
        return {"error": "Missing texts field"}

    if isinstance(texts, str):
        texts = [texts]

    # nomic-embed-text requires task prefix
    prefixed = [f"{task_type}: {t}" for t in texts]

    start = time.time()
    model = get_embedder()
    embeddings = model.encode(prefixed, convert_to_numpy=True)
    elapsed = time.time() - start

    return {
        "embeddings": embeddings.tolist(),
        "dimensions": embeddings.shape[1],
        "count": len(texts),
        "processing_sec": round(elapsed, 2),
    }


def handle_enroll(input_data):
    """Receive speaker embeddings from a daemon. Keyed by agent_id.

    Input:
        agent_id: str — the agent this enrollment belongs to
        speakers: dict — { "YANIV": "<base64 .npy>", "RACHEL": "<base64 .npy>", ... }

    The daemon reads its .npy files, base64-encodes them, and sends them here.
    The GPU worker stores them in memory for diarization matching.
    """
    agent_id = input_data.get("agent_id", "")
    speakers_b64 = input_data.get("speakers", {})

    if not agent_id:
        return {"error": "Missing agent_id field"}
    if not speakers_b64:
        return {"error": "Missing speakers field"}

    loaded = {}
    for name, emb_b64 in speakers_b64.items():
        try:
            npy_bytes = base64.b64decode(emb_b64)
            arr = np.load(io.BytesIO(npy_bytes))
            loaded[name.upper()] = arr
        except Exception as e:
            print(f"Failed to load embedding for {name}: {e}")

    with _enroll_lock:
        ENROLLED_SPEAKERS[agent_id] = loaded

    print(f"Enrolled {len(loaded)} speakers for agent '{agent_id}': {list(loaded.keys())}")
    return {
        "enrolled": len(loaded),
        "agent_id": agent_id,
        "speakers": list(loaded.keys()),
    }


def _audio_to_wav(audio_bytes, suffix=".wav"):
    """Convert audio bytes (WAV, MP3, etc.) to WAV using ffmpeg if needed.

    Returns path to a temp WAV file. Caller must clean up.
    """
    with tempfile.NamedTemporaryFile(suffix=suffix, delete=False) as tmp:
        tmp.write(audio_bytes)
        src_path = tmp.name

    # If already WAV, return as-is
    if suffix == ".wav":
        return src_path

    # Convert to WAV via ffmpeg
    wav_path = src_path.rsplit(".", 1)[0] + ".wav"
    try:
        import subprocess
        result = subprocess.run(
            ["ffmpeg", "-y", "-i", src_path, "-ar", "24000", "-ac", "1", wav_path],
            capture_output=True, timeout=30,
        )
        if result.returncode != 0:
            raise RuntimeError(f"ffmpeg failed: {result.stderr.decode()[:200]}")
    finally:
        os.unlink(src_path)
    return wav_path


def _compute_conditionals_from_wav(wav_path, exaggeration=0.5):
    """Compute voice conditionals from a WAV file.

    Returns (conditionals_b64, processing_sec) or raises.
    """
    start = time.time()
    model, model_lock = acquire_chatterbox()

    with model_lock:
        model.prepare_conditionals(wav_path, exaggeration=exaggeration)

        with tempfile.NamedTemporaryFile(suffix=".pt", delete=False) as pt_tmp:
            pt_path = pt_tmp.name

        model.conds.save(pt_path)

    with open(pt_path, "rb") as f:
        pt_bytes = f.read()

    conditionals_b64 = base64.b64encode(pt_bytes).decode("ascii")
    os.unlink(pt_path)

    elapsed = time.time() - start
    return conditionals_b64, round(elapsed, 2)


def handle_compute_conditionals(input_data):
    """Compute voice conditionals from audio sample for voice cloning.

    Input:
        wav_b64: str — base64-encoded WAV audio (or use audio_b64 + format)
        audio_b64: str — base64-encoded audio in any format (WAV, MP3, etc.)
        format: str — audio format when using audio_b64 (default "wav")
        exaggeration: float — voice expressiveness (default 0.5)

    Returns:
        conditionals_b64: str — base64-encoded .pt file
        processing_sec: float — time to compute conditionals
    """
    wav_b64 = input_data.get("wav_b64", "")
    audio_b64 = input_data.get("audio_b64", "")
    audio_format = input_data.get("format", "wav")
    exaggeration = input_data.get("exaggeration", 0.5)

    if not wav_b64 and not audio_b64:
        return {"error": "Missing wav_b64 or audio_b64 field"}

    try:
        if wav_b64:
            audio_bytes = base64.b64decode(wav_b64)
            suffix = ".wav"
        else:
            audio_bytes = base64.b64decode(audio_b64)
            suffix = f".{audio_format}"

        wav_path = _audio_to_wav(audio_bytes, suffix=suffix)
        try:
            conditionals_b64, processing_sec = _compute_conditionals_from_wav(
                wav_path, exaggeration=exaggeration
            )
        finally:
            if os.path.exists(wav_path):
                os.unlink(wav_path)

        return {
            "conditionals_b64": conditionals_b64,
            "processing_sec": processing_sec,
        }

    except Exception as e:
        traceback.print_exc()
        return {"error": f"Failed to compute conditionals: {str(e)}"}


def handle_extract_voice(input_data):
    """Extract a voice sample from a YouTube URL and compute conditionals.

    Downloads up to max_seconds of audio from the URL, converts to WAV,
    and computes voice conditionals for use with Chatterbox TTS.

    Input:
        url: str — YouTube URL
        max_seconds: int — max audio duration to extract (default 30)
        exaggeration: float — voice expressiveness (default 0.5)

    Returns:
        conditionals_b64: str — base64-encoded .pt file
        sample_b64: str — base64-encoded WAV sample (for playback/storage)
        duration_sec: float — actual duration of extracted sample
        processing_sec: float — total processing time
    """
    url = input_data.get("url", "")
    max_seconds = input_data.get("max_seconds", 30)
    exaggeration = input_data.get("exaggeration", 0.5)

    if not url:
        return {"error": "Missing url field"}

    start = time.time()

    try:
        import subprocess

        # Download audio with yt-dlp
        with tempfile.TemporaryDirectory() as tmpdir:
            raw_path = os.path.join(tmpdir, "audio")
            result = subprocess.run(
                [
                    "yt-dlp",
                    "--no-playlist",
                    "-x",                          # extract audio only
                    "--audio-format", "wav",
                    "--postprocessor-args", f"-ac 1 -ar 24000 -t {max_seconds}",
                    "-o", raw_path,
                    url,
                ],
                capture_output=True, timeout=120,
            )
            if result.returncode != 0:
                stderr = result.stderr.decode()[:300]
                return {"error": f"yt-dlp failed: {stderr}"}

            # yt-dlp may add extension
            wav_path = raw_path + ".wav"
            if not os.path.exists(wav_path):
                # Try finding the output file
                files = [f for f in os.listdir(tmpdir) if f.endswith(".wav")]
                if files:
                    wav_path = os.path.join(tmpdir, files[0])
                else:
                    return {"error": "yt-dlp produced no WAV output"}

            # Get duration
            probe = subprocess.run(
                ["ffprobe", "-v", "quiet", "-show_entries", "format=duration",
                 "-of", "csv=p=0", wav_path],
                capture_output=True, timeout=10,
            )
            duration = float(probe.stdout.decode().strip()) if probe.returncode == 0 else 0

            # Compute conditionals
            conditionals_b64, _ = _compute_conditionals_from_wav(
                wav_path, exaggeration=exaggeration
            )

            # Read the WAV sample for return
            with open(wav_path, "rb") as f:
                sample_b64 = base64.b64encode(f.read()).decode("ascii")

            elapsed = time.time() - start

            return {
                "conditionals_b64": conditionals_b64,
                "sample_b64": sample_b64,
                "duration_sec": round(duration, 2),
                "processing_sec": round(elapsed, 2),
            }

    except Exception as e:
        traceback.print_exc()
        return {"error": f"Failed to extract voice: {str(e)}"}


# ─────────────────────────────────────────────
# Router
# ─────────────────────────────────────────────

def handle_logs(input_data):
    """Return recent log lines from the in-memory ring buffer.

    Reads from local_server._log_buffer (populated by _TeeWriter which
    intercepts all stdout/stderr). No model needed — pure memory read.
    """
    lines = input_data.get("lines", 100)
    try:
        lines = max(1, min(int(lines), 2000))
    except (TypeError, ValueError):
        lines = 100
    from local_server import _log_buffer
    recent = list(_log_buffer)[-lines:]
    return {"text": "\n".join(recent)}


HANDLERS = {
    "synthesize": handle_synthesize,
    "embed": handle_embed,
    "enroll": handle_enroll,
    "compute_conditionals": handle_compute_conditionals,
    "extract_voice": handle_extract_voice,
    "logs": handle_logs,
}

# Generator handlers yield multiple results (for RunPod /stream endpoint)
GENERATOR_HANDLERS = {
    "synthesize_stream": handle_synthesize_stream,
    "transcribe": handle_transcribe,
    "diarize": handle_diarize,
    "wake": lambda _: iter([]),  # Wake probe: yields WS coords (from outer handler) then exits immediately
}


def handler(event):
    """Route to the right model based on 'action' field.

    Sync generator handler. Supports two handler types:
    - Regular handlers: yield a single result dict
    - Generator handlers: yield multiple result dicts (for RunPod /stream endpoint)

    Using sync generator (not async) for RunPod compatibility.
    GPU work is blocking anyway (single model lock), so async provides no benefit.

    Billing: checks credits before billable actions, deducts after success.
    """
    input_data = event.get("input", {})
    action = input_data.get("action", "")
    agent_id = input_data.get("agent_id", "")

    # Yield WebSocket connection coordinates as first output (RunPod only).
    # Browser sees these in /stream and opens a direct WebSocket via EC2 proxy.
    # Skip when called via WS — the browser is already connected, coords not needed.
    if not event.get("_via_ws"):
        worker_id = os.environ.get("B3_WORKER_ID", "")
        pod_id = os.environ.get("RUNPOD_POD_ID", "")
        if worker_id:
            yield {"worker_id": worker_id, "pod_id": pod_id}
            print(f"[WS] Yielded EC2 proxy coords: worker_id={worker_id} pod={pod_id}", flush=True)

    # Credit gate — check before billable actions (skipped in dev mode)
    if action in BILLABLE_ACTIONS and not DEV_MODE:
        if not agent_id:
            print(f"[BILLING] REJECTED: billable action {action!r} with no agent_id", flush=True)
            yield {"error": "agent_id required for billable operations", "code": "missing_agent_id"}
            return
        credit_check = _check_credits(agent_id, action)
        print(f"[BILLING] {action} agent={agent_id} check={credit_check}", flush=True)
        if not credit_check.get("allowed", True):
            msg = credit_check.get("message", "Credits depleted")
            yield {"error": msg, "code": "credits_depleted"}
            return

    voice_job_id = input_data.get("voice_job_id", "")

    # Generator handler path — yield from streaming handlers
    if action in GENERATOR_HANDLERS:
        gen_fn = GENERATOR_HANDLERS[action]
        yield from gen_fn(input_data)
        # Deduct after successful streaming completion
        if action in BILLABLE_ACTIONS:
            threading.Thread(target=_deduct_credit, args=(agent_id, action), daemon=True).start()
        # Report job completion to EC2
        if voice_job_id:
            threading.Thread(target=_report_job_checkpoint, args=(voice_job_id, "transcribed" if action == "transcribe" else "generating"), daemon=True).start()
        return

    # Regular handler path
    if action not in HANDLERS:
        all_actions = list(HANDLERS.keys()) + list(GENERATOR_HANDLERS.keys())
        yield {"error": f"Unknown action '{action}'. Use: {', '.join(all_actions)}"}
        return

    result = HANDLERS[action](input_data)

    # Deduct after successful regular handler completion
    if action in BILLABLE_ACTIONS and "error" not in result:
        threading.Thread(target=_deduct_credit, args=(agent_id, action), daemon=True).start()
    # Report job completion/failure to EC2
    if voice_job_id:
        if "error" in result:
            threading.Thread(target=_report_job_checkpoint, args=(voice_job_id, "failed", result["error"]), daemon=True).start()
        else:
            threading.Thread(target=_report_job_checkpoint, args=(voice_job_id, "transcribed" if action == "transcribe" else "generating"), daemon=True).start()

    # Attach pass-through metadata (msg_id, chunk, total, etc.)
    meta = {}
    for key in ("msg_id", "chunk", "total", "agent_id"):
        if key in input_data:
            meta[key] = input_data[key]
    callback_payload = {**result, **meta}

    yield callback_payload


def _background_chatterbox_warmup():
    """Load Chatterbox pool + run warmup inference in a background thread.
    Called after transcription models are loaded and the RunPod worker has
    started, so transcription requests can be served immediately."""
    import time as _t
    try:
        print("Background: loading Chatterbox pool...", flush=True)
        _t0 = _t.time()
        _init_chatterbox_pool()
        print(f"Background: Chatterbox pool warm in {_t.time() - _t0:.1f}s", flush=True)

        # Preload embedding model — avoids 4s lazy load on first embed call
        try:
            print("Background: preloading embedding model...", flush=True)
            _te = _t.time()
            get_embedder()
            print(f"Background: embedding model ready in {_t.time() - _te:.1f}s", flush=True)
        except Exception as e:
            print(f"Background: embedding model preload failed: {e}", flush=True)

        # Warmup inference — triggers CUDA kernel compilation so first
        # real TTS request doesn't pay the penalty.
        _tw = _t.time()
        print("Background: running warmup inference...", flush=True)
        model, lock = acquire_chatterbox()
        with lock:
            _dummy = model.generate("Hello.", audio_prompt_path=None)
        del _dummy
        if torch.cuda.is_available():
            torch.cuda.empty_cache()
        print(f"Background: warmup inference done in {_t.time() - _tw:.1f}s", flush=True)
    except Exception as e:
        print(f"Background: Chatterbox warmup failed: {e}", flush=True)


if __name__ == "__main__":
    import time as _time

    # 1. Pre-initialize CUDA context.
    if torch.cuda.is_available():
        print("Pre-initializing CUDA context...", flush=True)
        _cuda_t = _time.time()
        torch.cuda.init()
        torch.zeros(1, device="cuda")
        print(f"CUDA context ready in {_time.time() - _cuda_t:.1f}s", flush=True)

    # 2. Load transcription models FIRST — the interaction always starts
    #    with the user speaking, so STT readiness is more important than TTS.
    #    Load the most common presets so first transcription is fast.
    print("Pre-loading transcription models...", flush=True)
    _stt_t = _time.time()
    # multilingual preset (most common from voice pipeline)
    get_whisper("large-v3")
    get_whisperx("large-v3-turbo")
    # english-fast preset (default)
    get_whisper("medium.en")
    get_whisperx("small.en")
    print(f"Transcription models ready in {_time.time() - _stt_t:.1f}s", flush=True)

    # 3. Start RunPod worker NOW — transcription requests can be served
    #    immediately. Chatterbox loads in background.
    _bg = threading.Thread(target=_background_chatterbox_warmup, daemon=True)
    _bg.start()

    # 4. Start WebSocket server in background thread (same process, shared models)
    #    Allows browsers to connect directly for lower latency than HTTP polling.
    try:
        # local_server.py imports `from handler import handler` at module level.
        # When handler.py runs as __main__, sys.modules['handler'] doesn't exist.
        # Inject it so the circular import resolves against the already-loaded module.
        import sys as _sys
        if 'handler' not in _sys.modules:
            _sys.modules['handler'] = _sys.modules['__main__']
        # Install log capture BEFORE importing local_server so model-loading
        # output (which precedes start_ws_server()) is captured in _log_buffer.
        from local_server import _install_log_capture
        _install_log_capture()
        from local_server import start_ws_server
        start_ws_server()
    except Exception as e:
        print(f"[WS] Failed to start WebSocket server (non-fatal): {e}", flush=True)

    # 5. Start RunPod serverless worker (blocks forever, serving requests)
    runpod.serverless.start({
        "handler": handler,
        "return_aggregate_stream": True,
    })
