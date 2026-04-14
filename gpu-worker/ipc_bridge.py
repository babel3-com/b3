"""IPC bridge for b3-gpu-rtc sidecar.

Listens on a Unix socket for requests from the Rust WebRTC sidecar,
translates them to handler() calls, and sends results back.

Protocol: length-prefixed JSON
  [length: u32 LE][json payload]
"""

import asyncio
import base64
import json
import os
import struct
import sys
import traceback

# The handler module — imported from the same directory
import handler as gpu_handler

SOCKET_PATH = "/tmp/b3-gpu.sock"
MAX_MESSAGE_BYTES = 64 * 1024 * 1024  # 64 MB

# Streaming audio state (mirrors local_server.py _audio_streams)
_audio_streams = {}      # stream_id -> [webm_chunks]
_stream_consumed = {}    # stream_id -> pcm_samples_consumed


async def read_message(reader: asyncio.StreamReader) -> dict:
    """Read a length-prefixed JSON message."""
    len_bytes = await reader.readexactly(4)
    length = struct.unpack("<I", len_bytes)[0]
    if length > MAX_MESSAGE_BYTES:
        raise ValueError(f"IPC message too large: {length} bytes (max {MAX_MESSAGE_BYTES})")
    payload = await reader.readexactly(length)
    return json.loads(payload)


async def write_message(writer: asyncio.StreamWriter, msg: dict):
    """Write a length-prefixed JSON message."""
    payload = json.dumps(msg).encode()
    writer.write(struct.pack("<I", len(payload)))
    writer.write(payload)
    await writer.drain()


async def handle_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    """Handle a single sidecar connection."""
    print("[IPC] Sidecar connected", flush=True)
    client_stream_ids = set()  # track streams opened by this connection

    try:
        while True:
            msg = await read_message(reader)
            msg_type = msg.get("type", "")

            if msg_type == "submit":
                job_id = msg.get("job_id", "unknown")
                input_data = msg.get("input", {})
                event = {"input": input_data}
                asyncio.create_task(_process_job(writer, job_id, event))

            elif msg_type == "cancel":
                job_id = msg.get("job_id", "unknown")
                print(f"[IPC] Cancel requested for {job_id}", flush=True)

            elif msg_type == "stream_start":
                stream_id = msg.get("stream_id", "")
                mime_type = msg.get("mime_type", "audio/webm")
                _audio_streams[stream_id] = []
                _stream_consumed[stream_id] = 0
                client_stream_ids.add(stream_id)
                print(f"[IPC] Stream started: {stream_id} ({mime_type})", flush=True)
                await write_message(writer, {
                    "type": "accepted",
                    "stream_id": stream_id,
                })

            elif msg_type == "stream_chunk":
                stream_id = msg.get("stream_id", "")
                audio_b64 = msg.get("audio_b64", "")
                if stream_id in _audio_streams and audio_b64:
                    chunk_bytes = base64.b64decode(audio_b64)
                    _audio_streams[stream_id].append(chunk_bytes)

            elif msg_type == "stream_transcribe_partial":
                stream_id = msg.get("stream_id", "")
                language = msg.get("language", "en")
                preset = msg.get("preset", "english-fast")
                asyncio.create_task(
                    _process_partial(writer, stream_id, language, preset)
                )

            elif msg_type == "stream_finalize":
                stream_id = msg.get("stream_id", "")
                language = msg.get("language", "en")
                preset = msg.get("preset", "english-fast")
                asyncio.create_task(
                    _process_finalize(writer, stream_id, language, preset)
                )

            else:
                print(f"[IPC] Unknown message type: {msg_type}", flush=True)

    except asyncio.IncompleteReadError:
        print("[IPC] Sidecar disconnected", flush=True)
    except Exception as e:
        print(f"[IPC] Error: {e}", flush=True)
        traceback.print_exc()
    finally:
        # Clean up any audio streams opened by this client
        for sid in client_stream_ids:
            _audio_streams.pop(sid, None)
            _stream_consumed.pop(sid, None)
        if client_stream_ids:
            print(f"[IPC] Cleaned up {len(client_stream_ids)} stream(s) on disconnect", flush=True)
        writer.close()
        await writer.wait_closed()


async def _process_job(writer: asyncio.StreamWriter, job_id: str, event: dict):
    """Process a GPU job and stream results back."""
    try:
        await write_message(writer, {
            "type": "accepted",
            "job_id": job_id,
            "state": "warm",
            "gpu_worker_id": os.environ.get("HOSTNAME", "local"),
        })

        result_gen = gpu_handler.handler(event)

        if hasattr(result_gen, "__anext__"):
            async for output in result_gen:
                await _send_output(writer, job_id, output)
        elif hasattr(result_gen, "__next__"):
            for output in result_gen:
                await _send_output(writer, job_id, output)
        else:
            await _send_output(writer, job_id, result_gen)

        await write_message(writer, {"type": "done", "job_id": job_id})

    except Exception as e:
        print(f"[IPC] Job {job_id} failed: {e}", flush=True)
        traceback.print_exc()
        try:
            await write_message(writer, {
                "type": "error", "job_id": job_id, "error": str(e),
            })
        except Exception:
            pass


async def _process_partial(
    writer: asyncio.StreamWriter,
    stream_id: str,
    language: str,
    preset: str,
):
    """Process a partial transcription from accumulated stream chunks."""
    try:
        chunks = _audio_streams.get(stream_id, [])
        if not chunks:
            return

        # Concatenate all webm chunks (full blob with headers)
        full_blob = b"".join(chunks)
        audio_b64 = base64.b64encode(full_blob).decode()

        # Build handler event
        consumed = _stream_consumed.get(stream_id, 0)
        event = {"input": {
            "action": "transcribe",
            "audio_b64": audio_b64,
            "language": language,
            "preset": preset,
            "skip_align": True,
            "_stream_consumed_samples": consumed,
        }}

        partial_job_id = f"{stream_id}-partial-{consumed}"

        result_gen = gpu_handler.handler(event)
        if hasattr(result_gen, "__next__"):
            for output in result_gen:
                if isinstance(output, dict) and "text" in output:
                    # Update consumed count
                    new_consumed = output.get("_consumed_samples", consumed)
                    _stream_consumed[stream_id] = new_consumed

                    await write_message(writer, {
                        "type": "result",
                        "job_id": partial_job_id,
                        "output": output,
                    })

    except Exception as e:
        print(f"[IPC] Partial transcription failed for {stream_id}: {e}", flush=True)


async def _process_finalize(
    writer: asyncio.StreamWriter,
    stream_id: str,
    language: str,
    preset: str,
):
    """Finalize streaming transcription — transcribe remaining audio."""
    try:
        chunks = _audio_streams.get(stream_id, [])
        if not chunks:
            await write_message(writer, {
                "type": "result",
                "job_id": f"{stream_id}-finalize",
                "output": {"text": "", "segments": []},
            })
            return

        full_blob = b"".join(chunks)
        audio_b64 = base64.b64encode(full_blob).decode()

        consumed = _stream_consumed.get(stream_id, 0)
        event = {"input": {
            "action": "transcribe",
            "audio_b64": audio_b64,
            "language": language,
            "preset": preset,
            "_stream_consumed_samples": consumed,
            "_finalize": True,
        }}

        finalize_job_id = f"{stream_id}-finalize"

        result_gen = gpu_handler.handler(event)
        if hasattr(result_gen, "__next__"):
            for output in result_gen:
                await _send_output(writer, finalize_job_id, output)

        await write_message(writer, {"type": "done", "job_id": finalize_job_id})

        # Cleanup stream state
        _audio_streams.pop(stream_id, None)
        _stream_consumed.pop(stream_id, None)

    except Exception as e:
        print(f"[IPC] Finalize failed for {stream_id}: {e}", flush=True)
        try:
            await write_message(writer, {
                "type": "error",
                "job_id": f"{stream_id}-finalize",
                "error": str(e),
            })
        except Exception:
            pass


async def _send_output(writer: asyncio.StreamWriter, job_id: str, output):
    """Classify and send a handler output."""
    if isinstance(output, dict):
        if "error" in output:
            await write_message(writer, {
                "type": "error", "job_id": job_id, "error": output["error"],
            })
        elif "progress" in output:
            await write_message(writer, {
                "type": "progress", "job_id": job_id, "progress": output["progress"],
            })
        elif "audio_b64" in output:
            chunk_index = output.get("chunk_index", output.get("chunk", 0))
            await write_message(writer, {
                "type": "chunk",
                "job_id": job_id,
                "chunk_index": chunk_index,
                "audio_b64": output["audio_b64"],
                "duration_sec": output.get("duration_sec", 0),
                "generation_sec": output.get("generation_sec", 0),
            })
        else:
            await write_message(writer, {
                "type": "result", "job_id": job_id, "output": output,
            })
    else:
        await write_message(writer, {
            "type": "result", "job_id": job_id, "output": output,
        })


async def main():
    try:
        os.unlink(SOCKET_PATH)
    except FileNotFoundError:
        pass

    server = await asyncio.start_unix_server(handle_client, SOCKET_PATH)
    print(f"[IPC] Listening on {SOCKET_PATH}", flush=True)

    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
