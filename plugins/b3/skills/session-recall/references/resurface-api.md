# Resurface API Reference

## `list_sessions`

List all Claude session files with metadata, sorted by size descending.

### Parameters

| Parameter | Type | Default | Description |
|---|---|---|---|
| `min_size_mb` | float | 0 | Minimum file size in MB. Use 1.0 to skip tiny sub-sessions. |
| `limit` | int | 50 | Max sessions to return. Use 0 for no limit. |
| `after` | string | "" | ISO date filter — only sessions after this date (e.g. "2026-03-01") |
| `before` | string | "" | ISO date filter — only sessions before this date |

### Response

```json
{
  "total": 47,
  "returned": 10,
  "sessions": [
    {
      "session_id": "e01ff420-...",
      "source": "current",
      "size_mb": 535.48,
      "line_count": 165145,
      "first_timestamp": null,
      "last_timestamp": "2026-04-02T02:00:06.165Z",
      "file_path": "/home/agent-alice/.claude/projects/.../e01ff420-....jsonl"
    }
  ]
}
```

**Notes:**
- `source: "current"` = local sessions, `source: "remote"` = remote sessions
- `first_timestamp` is often null (not all sessions have it indexed)
- Sessions are sorted by size descending — the biggest sessions are the main working sessions

## `resurface`

Extract moments from a session JSONL file with sampling, filtering, and output redirection.

### Parameters

| Parameter | Type | Default | Description |
|---|---|---|---|
| `start_line` | int | **required** | Starting line number (1-based) |
| `end_line` | int | **required** | Ending line number |
| `session_file` | string | "" | Path to session JSONL. If empty, uses active session. |
| `record_type` | string | "conversation" | What to extract (see below) |
| `frac_sample` | float | null | Random sample fraction 0.0-1.0. null = no sampling. |
| `max_word_length` | int | null | Truncate each line to N words. null = no truncation. |
| `include_timestamps` | bool | true | Include ISO timestamps in output |
| `seed` | int | null | Random seed for reproducible sampling |
| `output_file` | string | "" | Write to file instead of returning (saves tokens) |
| `tool_result_limit` | int | 100 | Max chars for tool_result content. 0 = unlimited. |

### Record Types

| Type | Extracts |
|---|---|
| `conversation` | User messages, assistant text, voice_say calls |
| `tool_use` | All tool calls with name and input |
| `bash` | Bash commands only (command + description) |
| `grep` | Grep commands specifically |
| `write` | Write tool calls (files created/written) |
| `read` | Read tool calls (files read) |
| `all` | Everything including tool results |

### Sampling Strategy Guide

| Goal | frac_sample | max_word_length | Notes |
|---|---|---|---|
| Session overview/index | 0.01-0.03 | 60 | ~150-500 moments from 15k conversation lines |
| Topic scanning | 0.1-0.2 | 80 | Broader coverage, still manageable |
| Targeted extraction | 1.0 | 100-150 | Full content from a narrowed range |
| Full read for context | 1.0 | null | Everything, use only on small ranges (<2k lines) |

### Output Formats

**Inline (no output_file):** Returns moments as text directly in the tool result.

```
[Session: 67039 total lines | Showing L1-L165145]

L1606: 2026-03-23T03:17:15.633Z | Zara [voice]: Good catch. The stop button wasn't...
L1961: 2026-03-23T03:52:17.689Z | Zara: The simulator build has both extensions...
```

**File redirect (output_file set):** Writes to file, returns metadata:

```json
{"resurfaced": "/path/to/file.txt", "moments": 15454, "total_lines": 84323}
```

The file contains the same format as inline output. Use Grep to search it.

### Example Workflows

**Quick session overview:**
```
resurface(start_line=1, end_line=165145, session_file="...",
          record_type="conversation", frac_sample=0.01, max_word_length=60)
```

**Dump for search:**
```
resurface(start_line=12000, end_line=18000, session_file="...",
          record_type="conversation", frac_sample=1.0, max_word_length=150,
          output_file="/home/agent-alice/tmp/recall/conv-12k-18k.txt")
```

**Find all files written:**
```
resurface(start_line=1, end_line=165145, session_file="...",
          record_type="write", frac_sample=1.0,
          output_file="/home/agent-alice/tmp/recall/writes.txt")
```

**Then grep the output:**
```
Grep(pattern="deploy", path="/home/agent-alice/tmp/recall/conv-12k-18k.txt")
```
