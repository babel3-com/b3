"""GPU worker billing — credit check/deduct via EC2.

Production: BILLING_URL + GPU_SECRET env vars required.
Dev mode: omit both → no billing, local use only.
Fail open: if billing check fails, allow the job (don't block on errors).
"""

import os
import requests

BILLING_URL = os.environ.get("BILLING_URL", "https://babel3.com")
GPU_SECRET = os.environ.get("GPU_SECRET", "")
# Dev mode: no GPU_SECRET means billing is disabled (can't authenticate)
DEV_MODE = not GPU_SECRET

# Actions that cost credits
BILLABLE_ACTIONS = {"synthesize", "synthesize_stream", "diarize"}

# TTS accumulator: deduct 1 credit per 4 TTS chunks (0.25 per chunk)
_tts_chunk_counts: dict[str, int] = {}  # agent_id → accumulated chunks
TTS_CHUNKS_PER_CREDIT = 4


def check_credits(agent_id: str, action: str) -> dict:
    """Check if agent has credits. Returns {"allowed": bool, "balance": int}."""
    if DEV_MODE:
        return {"allowed": True, "balance": -1, "dev_mode": True}
    try:
        resp = requests.post(
            f"{BILLING_URL}/api/gpu/credit-check",
            json={"agent_id": agent_id, "action": action},
            headers={"x-gpu-secret": GPU_SECRET},
            timeout=5,
        )
        if resp.status_code == 200:
            return resp.json()
        print(f"[BILLING] check failed: {resp.status_code} {resp.text}", flush=True)
        # Fail open — don't block on billing errors
        return {"allowed": True, "balance": -1}
    except Exception as e:
        print(f"[BILLING] check error: {e}", flush=True)
        return {"allowed": True, "balance": -1}


def deduct_credit(agent_id: str, action: str) -> dict:
    """Deduct credit after successful job. TTS: 1 credit per 4 chunks. Others: 1 credit."""
    if DEV_MODE:
        return {}

    # TTS accumulator: only deduct when 4 chunks accumulate
    if action in ("synthesize_stream", "synthesize"):
        count = _tts_chunk_counts.get(agent_id, 0) + 1
        if count < TTS_CHUNKS_PER_CREDIT:
            _tts_chunk_counts[agent_id] = count
            print(f"[BILLING] TTS chunk {count}/{TTS_CHUNKS_PER_CREDIT} for {agent_id} (no deduct yet)", flush=True)
            return {}
        # Reset counter and deduct 1 credit
        _tts_chunk_counts[agent_id] = 0

    try:
        resp = requests.post(
            f"{BILLING_URL}/api/gpu/credit-deduct",
            json={"agent_id": agent_id, "operation": action},
            headers={"x-gpu-secret": GPU_SECRET},
            timeout=5,
        )
        if resp.status_code == 200:
            result = resp.json()
            print(f"[BILLING] deducted 1 credit for {agent_id}/{action}, balance={result.get('balance')}", flush=True)
            return result
        print(f"[BILLING] deduct failed: {resp.status_code} {resp.text}", flush=True)
        return {}
    except Exception as e:
        print(f"[BILLING] deduct error: {e}", flush=True)
        return {}


def report_job_checkpoint(voice_job_id: str, status: str, error: str = None) -> None:
    """Report job completion to EC2. Fire-and-forget."""
    if not BILLING_URL or not voice_job_id:
        return
    try:
        patch = {"status": status, "reporter_version": "gpu-worker:" + os.environ.get("GPU_WORKER_VERSION", "unknown")}
        if error:
            patch["error"] = error[:200]
            patch["error_stage"] = "gpu_worker"
        requests.patch(
            f"{BILLING_URL}/api/voice-jobs/{voice_job_id}",
            json=patch,
            headers={"x-gpu-secret": GPU_SECRET},
            timeout=5,
        )
    except Exception as e:
        print(f"[JOB] checkpoint failed for {voice_job_id}: {e}", flush=True)


def init():
    """Print billing mode at startup."""
    if DEV_MODE:
        print("BILLING: dev mode (no GPU_SECRET — billing disabled)", flush=True)
    else:
        print(f"BILLING: active (url={BILLING_URL})", flush=True)
