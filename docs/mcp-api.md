# MCP & HTTP API Reference

*Twenty tools your agent gets automatically. Your scripts get them via curl.*

The B3 daemon exposes two interfaces: an **HTTP API** at `localhost:3100` (curlable from any program) and an **MCP server** (JSON-RPC over stdio, used by Claude Code). This doc covers both.

## Authentication

All HTTP API calls require a Bearer token:

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
```

## Part 1: HTTP API (Curlable Endpoints)

These are real HTTP routes on the daemon's local web server. Any program that can make HTTP requests can use them.

### Voice

**Speak text aloud:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/tts \
  -H "Content-Type: application/json" \
  -H "$AUTH" \
  -d '{
    "text": "Build is passing. All 47 tests green.",
    "emotion": "quiet satisfaction"
  }'
```

Text is chunked at sentence boundaries, synthesized via the GPU worker (Chatterbox TTS), and streamed to connected browsers as audio chunks.

**Upload audio for transcription:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/audio-upload \
  -H "$AUTH" \
  -F "audio=@recording.webm"
```

### Hive (Inter-Agent Messaging)

**Send a direct message:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/hive/send \
  -H "Content-Type: application/json" \
  -H "$AUTH" \
  -d '{"target": "reviewer", "message": "PR ready for review"}'
```

The message appears in the target agent's terminal as `[HIVE from=your-name] PR ready for review`.

**List agents:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s http://127.0.0.1:3100/api/hive/agents -H "$AUTH"
```

**Create a conversation room:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/hive/rooms \
  -H "Content-Type: application/json" \
  -H "$AUTH" \
  -d '{"topic": "Code review for PR #42", "members": ["reviewer", "security-auditor"], "expires_in": "24h"}'
```

**Send to a room:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/hive/rooms/{room_id}/send \
  -H "Content-Type: application/json" \
  -H "$AUTH" \
  -d '{"message": "LGTM, approve"}'
```

**Get message history:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
# Direct messages
curl -s http://127.0.0.1:3100/api/hive/messages -H "$AUTH"

# Room messages
curl -s http://127.0.0.1:3100/api/hive/rooms/{room_id}/messages -H "$AUTH"
```

**Destroy room encryption key (irreversible):**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/hive/rooms/{room_id}/destroy-key \
  -H "$AUTH"
```

### Browser

**Read browser console log:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s "http://127.0.0.1:3100/api/browser-console?last=50&filter=energy" -H "$AUTH"
```

Note the path: `/api/browser-console` (hyphen, not slash).

**Execute JavaScript in the browser:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/browser-eval \
  -H "Content-Type: application/json" \
  -H "$AUTH" \
  -d '{"code": "ENERGY_GATE_THRESHOLD"}'
```

> **Security:** `browser_eval` executes arbitrary JavaScript in connected browsers. The daemon binds to localhost by default — do not expose port 3100 to untrusted networks. If you need remote access, use the E2E encrypted relay (which authenticates via the babel3.com dashboard).

**Push HTML to the info tab:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s -X POST http://127.0.0.1:3100/api/inject \
  -H "Content-Type: application/json" \
  -H "$AUTH" \
  -d '{"type": "info", "html": "<h2>Deploy Status</h2><p>Production: healthy</p>"}'
```

### System

**Health check (no auth required):**

```bash
curl -s http://127.0.0.1:3100/health
```

**Diagnostics:**

```bash
AUTH="Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)"
curl -s http://127.0.0.1:3100/api/diagnostics -H "$AUTH"
```

### File Management

The daemon exposes file management endpoints for the dashboard's file browser. These are internal to the dashboard UI — for file operations from scripts, use the filesystem directly.

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/api/files` | GET | List directory |
| `/api/file` | GET | Read file |
| `/api/file` | PUT | Write file |
| `/api/file-raw` | GET | Read raw file (binary) |
| `/api/file-rename` | POST | Rename file |
| `/api/file-copy` | POST | Copy file |
| `/api/file-delete` | POST | Delete file |
| `/api/file-upload` | POST | Upload file |

## Part 2: MCP Tools (via Claude Code)

These 20 tools are available through the MCP JSON-RPC protocol (`b3 mcp voice`). Claude Code accesses them automatically. They are **not** HTTP endpoints — you cannot curl them directly.

The MCP server translates tool calls into HTTP requests against the daemon's local API where applicable, or handles them directly.

### Voice (6 tools)

| Tool | What it does |
|------|-------------|
| `voice_say` | Speak text aloud (calls `/api/tts` internally) |
| `voice_status` | Voice pipeline component health |
| `voice_health` | Deep health check — curls endpoints directly |
| `voice_logs` | Recent voice pipeline logs |
| `voice_share_info` | Push HTML to browser's info tab |
| `voice_enroll_speakers` | Upload speaker embeddings for diarization |

### Browser (2 tools)

| Tool | What it does |
|------|-------------|
| `browser_console` | Read browser dev console log (calls `/api/browser-console`) |
| `browser_eval` | Execute JS in browser context (calls `/api/browser-eval`) |

### Hive (8 tools)

| Tool | What it does |
|------|-------------|
| `hive_send` | Send DM to another agent. Supports forward secrecy and timelock. |
| `hive_status` | List all agents belonging to the same user |
| `hive_messages` | Direct message history |
| `hive_room_send` | Send message to a conversation room |
| `hive_room_messages` | Get room message history |
| `hive_room_create` | Create room with mandatory key expiration |
| `hive_room_destroy` | Permanently destroy room encryption key |
| `hive_room_list` | List all conversation rooms |

### System (4 tools)

| Tool | What it does |
|------|-------------|
| `animation_add` | Register custom LED animation (name + description + JS pattern) |
| `email_draft` | Submit outbound email for owner review (not sent immediately) |
| `restart_session` | Restart daemon with update (kills current session) |
| `voice_show_image` | Push an image to connected browsers |

For full parameter schemas, usage examples, and error handling, install the [B3 plugin](https://github.com/babel3-com/b3-plugins) — the `references/mcp-tools.md` file has complete documentation for all 20 tools.

## Port Configuration

The daemon defaults to port 3100. To use a different port:

```bash
b3 start --web-port 3200
```

All API URLs change accordingly (`localhost:3200/api/...`).
