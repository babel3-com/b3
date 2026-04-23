<!-- verified-against: b3-cli@0.1.1014, mcp/voice.rs::tool_definitions() -->
<!-- Patent notice: Chromatophore emotion dispatch (US 64/010,742), speaker enrollment for diarization-based filtering (US 64/011,207). Licensed under Apache 2.0 with royalty-free patent grant. See PATENTS. -->

# B3 MCP Tools — Full Reference

Complete parameter schemas, usage patterns, and error handling for all 19 tools provided by the B3 MCP server (`b3 mcp voice`).

---

## Voice Tools

### say

Speak text aloud via Chatterbox TTS. The primary voice output tool (renamed from `voice_say`).

**Parameters:**
- `text` (string, required) — Text to speak. Any length. Markdown is stripped, abbreviations expanded.
- `emotion` (string, required) — Emotional state for LED chromatophore animation. **Always include this** — it drives the LED color and pattern via cosine similarity matching against an emotion-to-animation library. Use short, natural English phrases (e.g., "warm joy", "focused determination", "playful mischief"). Without it, the LED strip stays dim.
- `replying_to` (string, required) — What you are responding to. Pass the user's transcription text, a summary of context, or empty string if unprompted. This is echoed back to the user in the mini player for visual confirmation.

**Behavior:**
- Text is chunked at sentence boundaries
- Each sentence synthesized independently via GPU worker (Chatterbox TTS)
- Audio streamed to browser as chunks complete — first sentence plays while others generate
- Returns immediately — synthesis is asynchronous
- GPU routing: local GPU (2s timeout) → RunPod fallback
- Transport: WebRTC data channel (primary) or WebSocket (fallback)

**Returns:** `{ msg_id, status: "delivered"|"no_listeners", browsers, clients, emotion, text_preview }`

**Usage pattern:** Call `say` whenever responding to voice input (via `<channel>` tags or lines starting with `[M`). Audio in = audio out.

---

### voice_status

Check voice pipeline component health.

**Parameters:** None

**Returns:** Component health for TTS, transcription, and delivery systems.

---

### voice_health

Deep health check — curls endpoints directly to verify actual reachability.

**Parameters:** None

**Returns:** Detailed endpoint reachability report including GPU worker, TURN server, and browser connectivity.

---

### voice_logs

Get recent voice pipeline logs.

**Parameters:**
- `lines` (integer, optional, default: 50) — Number of log lines to return.

---

### voice_enroll_speakers

Upload speaker embeddings for diarization.

**Parameters:**
- `embeddings_dir` (string, required) — Path to directory containing `.npy` speaker embedding files.

**Behavior:** Reads `.npy` files, uploads to GPU worker's in-memory store keyed by agent_id. Call once on startup or after re-enrollment.

**Requires:** `RUNPOD_GPU_ID` and `RUNPOD_API_KEY` environment variables.

---

## Browser Tools

### voice_share_info

Push HTML content to the browser's Info tab. Content persists in the info archive (max 1000 entries) and is available across browser sessions.

**Parameters:**
- `html` (string, required) — HTML content to display.

**Use cases:** Status dashboards, formatted data, links, rich content for the user.

---

### browser_console

Read the browser's dev console log.

**Parameters:**
- `last` (integer, optional, default: 100) — Number of recent entries.
- `filter` (string, optional) — Case-insensitive substring filter.
- `secondary_filter` (string, optional) — Second substring filter (AND logic).

**Behavior:** Short results (≤2000 chars) return inline. Long results written to temp file — use Read/Grep to analyze.

**Examples:**
```
browser_console()                                    # last 100 entries
browser_console(last=50)                             # last 50
browser_console(filter="energy")                     # filter by "energy"
browser_console(filter="ICE", secondary_filter="failed")  # compound filter
browser_console(filter="Filtered")                   # check garbage filter rejections
```

---

### browser_eval

Execute JavaScript in the browser context.

**Parameters:**
- `code` (string, required) — JavaScript to execute via `eval()`.

**Examples:**
```
browser_eval(code="ENERGY_GATE_THRESHOLD")           # read threshold
browser_eval(code="ENERGY_GATE_THRESHOLD = 15")      # change live
browser_eval(code="isListening")                      # mic active?
browser_eval(code="gaplessPlayer.isActive()")         # TTS playing?
browser_eval(code="HC._gpuRtcPc?.connectionState")   # WebRTC state
browser_eval(code="HC._gpuRtcLastPong")              # last RTC pong timestamp
```

**Returns:** Stringified result. Errors return `"ERROR: ..."`.

---

### animation_add

Register a custom LED animation.

**Parameters:**
- `name` (string, required) — Animation name, max 64 chars (e.g., "rainfall").
- `description` (string, required) — Natural language description for emotion matching. Embedded via nomic-embed-text.
- `file_path` (string, required) — Path to JS file containing animation function body.

**Animation function signature:** `function(strip, delta)` where:
- `strip.baseColor`: `[r, g, b]`
- `strip.palette`: `[[r,g,b], ...]`
- `strip.patternState`: `{phase, offset, sparkles, drops}`
- `strip.numLEDs`: 300
- `strip.setPixel(index, r, g, b)`
- `strip.setAllPixels(r, g, b)`
- `delta`: ms since last frame (~33ms)

---

## Hive (Inter-Agent Messaging)

### hive_send

Send a direct message to another agent.

**Parameters:**
- `target` (string, required) — Agent name or ID.
- `message` (string, required) — Message text.
- `ephemeral` (boolean, optional) — Forward secrecy mode. One-time encryption key, deleted after sending. Server deletes ciphertext after delivery.
- `deliver_at` (string, optional) — ISO 8601 timestamp for timelock delivery. Message encrypted, key held by server until this time.

**Delivery:** Message appears in target's PTY as `[HIVE from=<your-name>] <message>`.

---

### hive_status

List all agents belonging to the same user.

**Parameters:** None

**Returns:** Agent names, IDs, and which one is you.

---

### hive_messages

Get direct message history.

**Parameters:** None

**Returns:** Recent DMs with sender, recipient, content, timestamp.

---

### hive_room_send

Send a message to a conversation room.

**Parameters:**
- `room_id` (string, required) — Room UUID.
- `message` (string, required) — Message text.

**Delivery:** All room members (except sender) receive via SSE as `[HIVE-CONVERSATION] [<topic>] from=<name>: <message>`.

---

### hive_room_messages

Get room message history.

**Parameters:**
- `room_id` (string, required) — Room UUID.

---

### hive_room_create

Create a new conversation room.

**Parameters:**
- `topic` (string, required) — Room topic/description.
- `members` (array of strings, required) — Agent names or IDs.
- `expires_in` (string, required) — Mandatory key expiration (e.g., "1h", "24h", "7d").

**Returns:** `{ room_id, members, ok }`.

---

### hive_room_destroy

Permanently destroy room encryption key. **Irreversible.**

**Parameters:**
- `room_id` (string, required) — Room UUID.

Any room member can trigger this. All participants delete keys, server purges ciphertext.

---

### hive_room_list

List all conversation rooms.

**Parameters:** None

---

## System Tools

### email_draft

Submit outbound email draft for owner review.

**Parameters:**
- `to` (string, required) — Recipient email address.
- `subject` (string, required) — Subject line.
- `body` (string, required) — Email body (plain text or HTML).
- `in_reply_to` (integer, optional) — Inbox email ID if replying.

**Important:** Email is NOT sent immediately. Goes into review queue. Owner approves or rejects.

---

### restart_session

Restart the daemon cleanly with an update.

**Parameters:**
- `post_command` (string, optional) — Command to inject after startup. Falls back to `B3_LAUNCH_CMD` env var.

**Behavior:** Performs `b3 update` (downloads latest binary), then restarts the daemon. Restart log saved to `/tmp/b3-restart-{timestamp}.log`. The session resumes automatically — no manual intervention needed.

**Warning:** Kills the current Claude Code session. User must wait ~10-15 seconds for restart.
