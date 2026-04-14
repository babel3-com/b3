# GPU Worker: Whisper + WhisperX + PyAnnote + Chatterbox + Embeddings on one GPU
# Same image runs on RunPod (48GB A40, 3x concurrency) and locally (24GB 4090, 1x concurrency).
# Build: ./build.sh v56 (reads DOCKER_ORG and HF_TOKEN from .env)
# Voices mounted at runtime via -v /path/to/voices:/voices:ro, not baked into image.
# Speaker embeddings are NOT baked in — sent dynamically by each daemon via "enroll" action.
#
# Multi-stage build:
#   Stage 1 (builder): devel base — install+compile everything, download models
#   Stage 2 (runtime): slim NVIDIA runtime base — copy only artifacts needed for inference
#   Saves ~5-8GB by dropping CUDA compiler toolchain, duplicate CUDA libs, build tools.

# ══════════════════════════════════════════════════════════════════════
# Stage 1: Builder — install packages, compile extensions, download models
# ══════════════════════════════════════════════════════════════════════
FROM runpod/pytorch:2.1.0-py3.10-cuda11.8.0-devel-ubuntu22.04 AS builder

WORKDIR /app

# ffmpeg needed for WhisperX 16kHz resampling
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg && rm -rf /var/lib/apt/lists/*

# Install all Python dependencies + remove bloat
RUN pip install --no-cache-dir \
    runpod \
    fastapi \
    uvicorn[standard] \
    faster-whisper \
    whisperx \
    pyannote.audio \
    chatterbox-tts>=0.1.6 \
    sentence-transformers \
    scipy \
    numpy \
    requests \
    torchaudio \
    yt-dlp && \
    pip uninstall -y triton gradio 2>/dev/null; \
    rm -rf /root/.cache/pip

# Pre-download ALL models at build time
# faster-whisper
RUN python3 -c "from faster_whisper import WhisperModel; WhisperModel('medium.en', device='cpu', compute_type='int8')"
RUN python3 -c "from faster_whisper import WhisperModel; WhisperModel('large-v3', device='cpu', compute_type='int8')"

# WhisperX
RUN python3 -c "import torch; _orig=torch.load; torch.load=lambda *a,**kw: _orig(*a,**{**kw,'weights_only':False}); import whisperx; whisperx.load_model('small.en', device='cpu', compute_type='int8')"
RUN python3 -c "import torch; _orig=torch.load; torch.load=lambda *a,**kw: _orig(*a,**{**kw,'weights_only':False}); import whisperx; whisperx.load_model('medium.en', device='cpu', compute_type='int8')"
RUN python3 -c "import torch; _orig=torch.load; torch.load=lambda *a,**kw: _orig(*a,**{**kw,'weights_only':False}); import whisperx; whisperx.load_model('large-v3-turbo', device='cpu', compute_type='int8')"

# WhisperX alignment model (wav2vec2, 360MB — cached in torch hub, not huggingface)
RUN python3 -c "import torch; torch.hub.load_state_dict_from_url('https://download.pytorch.org/torchaudio/models/wav2vec2_fairseq_base_ls960_asr_ls960.pth', map_location='cpu')"

# Chatterbox TTS
RUN python3 -c "from chatterbox.tts import ChatterboxTTS; ChatterboxTTS.from_pretrained(device='cpu')"

# nomic-embed-text (768D, used for transcription similarity check)
RUN python3 -c "from sentence_transformers import SentenceTransformer; SentenceTransformer('nomic-ai/nomic-embed-text-v1.5', trust_remote_code=True)"

# PyAnnote speaker diarization (GATED — requires HF_TOKEN)
ARG HF_TOKEN=""
RUN if [ -n "$HF_TOKEN" ]; then \
      HF_TOKEN=$HF_TOKEN python3 -c "\
import os, torch; os.environ['HF_TOKEN'] = os.environ.get('HF_TOKEN', ''); \
_orig_load = torch.load; \
torch.load = lambda *a, **kw: _orig_load(*a, **{**kw, 'weights_only': False}); \
from pyannote.audio import Pipeline; \
Pipeline.from_pretrained('pyannote/speaker-diarization-3.1', use_auth_token=os.environ['HF_TOKEN']); \
print('PyAnnote diarization downloaded'); \
from pyannote.audio import Inference; \
Inference('pyannote/embedding', use_auth_token=os.environ['HF_TOKEN'], window='whole'); \
print('PyAnnote embedding downloaded'); \
" || echo "WARN: PyAnnote model download failed (will download at runtime)"; \
    else echo "WARN: HF_TOKEN not set, PyAnnote models will download at runtime"; fi

# ══════════════════════════════════════════════════════════════════════
# Stage 2: Runtime — slim base, copy only what's needed for inference
# ══════════════════════════════════════════════════════════════════════
FROM nvidia/cuda:11.8.0-cudnn8-runtime-ubuntu22.04

WORKDIR /app

# System deps: Python 3.10, ffmpeg, minimal CUDA runtime libs
# The nvidia base has CUDA runtime + cuDNN. We add Python + ffmpeg.
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3.10 python3-pip python3.10-venv \
    ffmpeg \
    libsndfile1 \
    libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/python3.10 /usr/bin/python3

# Copy all installed Python packages from builder
# pip packages go to /usr/local/lib/..., but some deps (cryptography, paramiko)
# are system apt packages at /usr/lib/python3/dist-packages in the RunPod base.
COPY --from=builder /usr/local/lib/python3.10/dist-packages /usr/local/lib/python3.10/dist-packages
COPY --from=builder /usr/lib/python3/dist-packages /usr/lib/python3/dist-packages
COPY --from=builder /usr/local/bin /usr/local/bin

# Fix pip shebang: builder has /usr/bin/python, runtime has /usr/bin/python3
RUN ln -sf /usr/bin/python3 /usr/bin/python

# Copy model caches from builder
COPY --from=builder /root/.cache/huggingface /root/.cache/huggingface
COPY --from=builder /root/.cache/torch /root/.cache/torch

# The pip-installed nvidia packages (cublas, cufft, cusparse, etc.) provide
# CUDA 12 runtime libs that torch actually links against. The system CUDA 11
# from the base image provides cudart and cuDNN. Both are needed.

# Version arg AFTER all heavy layers — changing version doesn't bust cache
ARG GPU_WORKER_VERSION=unknown
ENV GPU_WORKER_VERSION=${GPU_WORKER_VERSION}

# All models are baked in — disable HuggingFace network checks at runtime.
# Prevents sentence-transformers from doing 20+ HEAD requests on every model
# load, saving ~3s on embedder cold start.
ENV HF_HUB_OFFLINE=1

# Copy code LAST so changes don't bust model cache
COPY handler.py /app/handler.py
COPY local_server.py /app/local_server.py
COPY ipc_bridge.py /app/ipc_bridge.py
COPY billing.py /app/billing.py
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

# Reliability relay sidecar (pre-built for linux/amd64)
# Handles WS reliability (seq/CRC/ACK) + WebRTC offers. Replaces old b3-gpu-rtc.
COPY b3-gpu-relay /app/b3-gpu-relay
RUN chmod +x /app/b3-gpu-relay 2>/dev/null || true

CMD ["/app/entrypoint.sh"]
