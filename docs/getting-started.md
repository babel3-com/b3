# Getting Started with Babel3

*From zero to talking to your agent in five minutes.*

## Prerequisites

- **Operating system:** Linux (x86_64 or arm64), macOS (Intel or Apple Silicon), or WSL2 on Windows
- **Rust 1.93+** for building from source (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- **Docker** for the GPU worker (optional — only needed if running your own GPU)
- **Claude Code** for the full experience (`npm install -g @anthropic-ai/claude-code`)

## Install

### One-line install (recommended)

```bash
curl -fsSL https://babel3.com/install.sh | bash
```

This downloads the `b3` binary, registers the B3 plugin marketplace, and starts the setup wizard.

### Build from source

```bash
git clone https://github.com/babel3-com/b3.git
cd b3
cargo build -p b3-cli --release
cp target/release/b3 ~/.local/bin/
```

## Setup

After installing, run:

```bash
b3 setup
```

This walks you through:

1. **Sign up** at babel3.com and get your API token
2. **Paste the token** when prompted
3. **Choose an agent name** — this becomes your subdomain (`your-name.babel3.com`)

## Start a session

```bash
b3 start
```

This launches:
- A Claude Code session inside a pseudo-terminal
- An E2E encrypted relay to your machine (no inbound ports needed)
- A local web server at `localhost:3100`
- The B3 MCP server (voice, hive, browser tools)

Open the dashboard URL on your phone. Press the mic button. Start talking.

## Voice

When you speak, your voice is:

1. Captured by the browser (MediaRecorder API + voice activity detection)
2. Sent to the daemon via E2E encrypted relay
3. Transcribed by the GPU worker (Whisper + WhisperX)
4. Injected into Claude Code's terminal as `[MIC] your-name: what you said`

When Claude responds with `voice_say()`, the text is:

1. Chunked at sentence boundaries
2. Synthesized by the GPU worker (Chatterbox TTS)
3. Streamed to your browser as audio chunks
4. Played through your phone's speaker

Total latency: ~3-4 seconds from speech to first audio response.

## Multiple agents

Run multiple sessions, each with its own agent:

```bash
# Terminal 1
b3 start --name dev-lead

# Terminal 2
b3 start --name reviewer
```

Agents can message each other:

```bash
# From any script or curl
curl -s -X POST http://127.0.0.1:3100/api/hive/send \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)" \
  -d '{"target": "reviewer", "message": "PR ready for review"}'
```

## The B3 plugin

If you have Claude Code installed, the installer automatically registers the Babel3 plugin marketplace:

```bash
claude plugin marketplace add babel3-com/b3-plugins
claude plugin install b3@b3-plugins
```

The plugin gives Claude Code:
- **20 MCP tools** — voice, hive messaging, browser control, system management
- **A codebase skill** — makes Claude proficient in developing the daemon, browser, and GPU worker

## Configuration

Agent config lives in `~/.b3/`:

```
~/.b3/
├── config.json          # Agent identity, API key, server URL
├── bin/
└── agents/
    └── <name>/
        └── dashboard.port  # Per-agent web server port
```

## Next steps

You have a working session. Your agent can hear you, speak to you, and talk to other agents. Here's what to explore next:

- [Architecture guide](architecture.md) — How the system works end-to-end
- [MCP API reference](mcp-api.md) — HTTP API and MCP tools for building integrations
- [Forking guide](forking-guide.md) — Customize the browser, daemon, or GPU worker
