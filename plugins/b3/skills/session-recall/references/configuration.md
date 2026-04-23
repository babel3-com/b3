# Session Recall Configuration Guide

## MCP Server Setup

Session recall tools ship in the b3 binary. They are served by the unified `b3 mcp` process alongside voice, hive, and browser tools — no separate server entry needed.

### Standard .mcp.json Entry

```json
{
  "b3": {
    "command": "b3",
    "args": ["mcp", "serve"]
  }
}
```

This is written automatically by `b3 setup` and refreshed on every `b3 start`. No manual configuration needed for basic use.

### Environment Variables

Pass these in the `env` block of the `b3` `.mcp.json` entry:

| Variable | Default | Description |
|---|---|---|
| `SESSION_DIRS` | *(auto-discover)* | Colon-separated list of directories containing session JSONL files. Overrides auto-discovery. |
| `SESSION_EXTRA_DIRS` | `""` | Additional colon-separated dirs to search. Appended to auto-discovered dirs. |
| `SESSION_INDEX_FILE` | `""` | Path to session index JSON file (for tag enrichment). Optional. |
| `SESSION_AGENT_NAME` | *(from $USER)* | Override agent display name. Default: capitalize after `agent-` prefix. |

## Auto-Discovery

When `SESSION_DIRS` is not set, the server scans `~/.claude/projects/` for subdirectories containing `.jsonl` files. This covers all projects the agent has worked on.

## Remote Sessions

For remote sessions (e.g. from `.claude-remote/projects/`), pass `SESSION_EXTRA_DIRS` to the `b3` entry:

```json
{
  "b3": {
    "command": "b3",
    "args": ["mcp", "serve"],
    "env": {
      "SESSION_EXTRA_DIRS": "/path/to/project/.claude-remote/projects/-project-path"
    }
  }
}
```

## Multi-Agent Setup

Each agent instance runs its own `b3 mcp` process. `SESSION_AGENT_NAME` controls the display name in resurfaced output.

To cross-reference another agent's sessions, add their session directory to `SESSION_EXTRA_DIRS`:

```json
{
  "b3": {
    "command": "b3",
    "args": ["mcp", "serve"],
    "env": {
      "SESSION_EXTRA_DIRS": "/home/agent-alice/.claude/projects/-home-agent-alice-my-other-project"
    }
  }
}
```

## Troubleshooting

If `list_sessions` or `resurface` are unavailable, the b3 binary may be outdated or `.mcp.json` may have a stale `session-memory` entry. Fix:

```bash
b3 update --force
# then restart the Claude Code session
```

`b3 update --force` refreshes the binary, rewrites `.mcp.json` to the unified entry, and removes any stale `session-memory` key.
