<h1 align="center">Babel3</h1>

<h3 align="center">Build something amazing from somewhere beautiful.</h3>

<p align="center">
  A Claude Code voice interface that unglues you from your chair.<br>
  Open source. Hands-free. Professional-grade vibe coding.
</p>

<!-- Alternative visual tagline for ads/hero images:
     Build something amazing ~~or~~ **and** be somewhere beautiful.
     Use where markdown strikethrough is supported. -->


---

You're on a trail. The sun is on your face. You say, "add error handling to the auth module." Claude asks for clarification — in voice. You have a conversation about it. It builds and deploys it, and you test it on your phone. You continue your walk.

This is not a metaphor. This is a Tuesday.

Babel3 dissolves the glue that keeps you in a chair and releases you into the world. Every coding tool made the code smarter but kept you at the desk. This is suffering that is no longer required. Babel3 is the first that lets you leave.

One open source repo, cloned. You talk to your agent on your phone. It answers back in voice. You can put the phone in your pocket and continue the conversation.

Works with Claude Code, Codex CLI, or any other command line tool. Babel3 is CLI-agnostic — it's a voice-enabled tunnel to your terminal.

## Quick Start

```bash
git clone https://github.com/babel3-com/b3.git
cd b3 && ./install.sh
```

The installer will walk you through it: sign up at babel3.com, grab your token, paste it in. Pick a name for your agent. That's it — you're in a session.

Open the dashboard URL on your phone. Press the mic button. Start talking.

This is your actual CLI — the same interface, just on your phone. Claude Code by default, or any CLI you choose.

## How It Works

When you speak, your voice is transcribed and injected into the session. When Claude answers in voice, it sends text to an MCP server that generates the audio and plays it on your phone. It's all open source and your agent knows how to modify it, so you can customize it however you like — just ask your agent for the changes you want.

```
                       babel3.com
                      ┌─────────────┐
                      │ Auth        │
                      │ Billing     │
                      │ Tunneling   │
                      │ TURN relay  │
                      └──────┬──────┘
                             │
Your Phone ◀════════════════════════════════▶ Your Machine
┌───────────────────┐   Dual transport       ┌───────────────────┐
│ Terminal + file   │   ┌ WebSocket (CF)     │ Persistent session│
│ browsing web app  │◀──┤                   ▶│ that wraps around │──▶ GPU Worker
│ with a microphone │   └ WebRTC (P2P/TURN)  │ your CLI          │
└───────────────────┘   Reliability layer    └───────────────────┘
                        (seq + CRC + ACK)
```

**Your privacy is protected.** Terminal traffic flows encrypted P2P between your phone and your machine — babel3.com never sees your code or conversations. Voice goes through a GPU worker for transcription and generation, but the GPU worker is ephemeral — it doesn't keep anything.

## Built for Cellular

You're not always on wifi. You're on LTE walking through a park, and the connection drops silently. Your terminal freezes. You tap the screen — nothing. You wait. Still nothing. You close the app and reopen. Half the output is gone.

That was the problem. Babel3 was built for the walk, the park bench, the coffee shop with one bar of signal — and that meant solving it for real: keeping a live terminal session alive over cellular networks where connections drop every 10 to 20 minutes.

**Dual transport.** Every session runs two paths simultaneously — a WebSocket through an E2E encrypted relay (primary) and a WebRTC data channel that goes peer-to-peer when available (secondary). The system prefers the relay for reliability, but when WebRTC is available it provides lower latency. If either dies, the other takes over instantly. You never notice the switch.

**Reliability layer.** Underneath both transports sits a Mosh-inspired reliability protocol. Every frame gets a sequence number and a CRC32 checksum. The sender keeps a buffer of unacknowledged frames. If the connection drops and comes back — even on a different transport — the other side sends a resume frame saying "last I got was sequence 4,217" and everything after that replays automatically. No data lost.

**Priority-aware.** Not all data is equal. Terminal deltas and voice audio are critical — they survive buffer pressure. LED animations and notifications are best-effort — they get evicted first when the buffer fills. You lose the light show before you lose the code.

The reliability layer is its own crate (`b3-reliable`) with a matching JavaScript implementation in the browser. Same wire format on both sides, same CRC32 polynomial, same frame layout. It's transport-agnostic — it doesn't care if the bytes flow over WebSocket, WebRTC, or carrier pigeon.

**Why not just use Mosh?** Mosh solves a similar problem but it's built around a single UDP transport for a single terminal. Babel3 sessions carry terminal, voice, and inter-agent data over multiple simultaneous transports (WebSocket + WebRTC), with per-browser session isolation, and need to work inside a browser — not a native client. We took Mosh's core insight (sequence + ACK + replay) and rebuilt it for our world.

## Multi-Agent

You want to run multiple agents. One reason: LLMs are better at finding mistakes in someone else's code than in their own. (Sound familiar?)

**Run multiple sessions, each with its own agent.** Switch between them from your phone — they're all in the menu.

**Your agents talk to each other.** One agent writes the feature and submits a PR. Another reviews security. Another reviews UX implications. Rejections come in, they get fixed, another round starts — a real conversation. LLMs were trained on code reviews. They feel at home doing this, and they're good at it. You're enjoying the breeze on a park bench while the quality of the PR climbs.

**Encrypted built-in messaging lets your agents talk among themselves.** They can message each other on a two-way channel, or they can start multi-agent persistent conversations. All encrypted end-to-end. They coordinate without you in the loop, and you step in when you want to.

## MCP Tools — Use from Any Program

Babel3 ships an MCP server that gives your agent voice, inter-agent messaging, and on-demand HTML delivery to the browser. But these aren't locked to Claude Code — the daemon exposes a local HTTP API that any program can call.

Three examples:

**Speak text aloud to your phone:**

```bash
curl -s -X POST http://127.0.0.1:3100/api/tts \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)" \
  -d '{"text": "Build is passing. All 47 tests green.", "emotion": "quiet satisfaction"}'
```

Your phone speaks it. Any script, any language, any tool — if it can make an HTTP request, it can talk to your phone.

**Send a message to another agent:**

```bash
curl -s -X POST http://127.0.0.1:3100/api/hive/send \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)" \
  -d '{"target": "pr-reviewer", "message": "Tests passed. Ready for review."}'
```

Your agents can message each other — or you can message them from a script. Build a CI pipeline that talks to your agents when deploys finish.

**Push HTML content to your phone's info tab:**

```bash
curl -s -X POST http://127.0.0.1:3100/api/inject \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.b3/config.json | jq -r .api_key)" \
  -d '{"type": "hive", "sender": "info", "text": "<h2>Deploy Status</h2><p>Production: ✓ healthy</p>"}'
```

The full MCP includes 20 tools — voice, inter-agent messaging, HTML delivery, and more. Your agent gets them automatically. Your scripts get them via curl. Install the [B3 plugin](https://github.com/babel3-com/b3-plugins) for the full tool set with a codebase skill that makes your agent proficient in developing Babel3 itself.

**The chromatophore.** Notice the `emotion` parameter in voice_say. Your agent describes how it's feeling in plain text — "quiet satisfaction," "playful mischief," anything — and the dashboard renders it as a subtle LED color animation. Like the colors on an octopus. The agent doesn't pick the colors. It just expresses an emotion, and the chromatophore interprets it differently each time. A small thing that makes your agent come alive without getting in the way. The animations are compatible with real LED strips — ask your agent to build the integration, and every time it speaks, the lights in your room change. The whole emotion-to-animation algorithm is open source. Play with it, create your own patterns, share them with friends.

## Customize Your Dashboard

The dashboard is yours to shape. Every panel and button can be toggled off from the menu — strip it down to just the terminal if that's what you want. The goal is to give your entire screen to the terminal and get everything else out of the way. Less noise, more code.

## Innovation Marketplace

Babel3 is designed to be forked. The browser, the daemon, the GPU worker — all of it.

**Fork the browser.** This is the one most people will do. Redesign the dashboard. Add panels, change the layout, build a completely different experience on your phone. Your fork serves as the dashboard for anyone who chooses it. Make Babel3 look and feel exactly the way you want.

**Fork the daemon.** This is where it gets wild. Add custom MCP servers, new voice pipelines, integrations with your own tools. You control both ends of the communication — the daemon and the browser — so you can bring whatever tooling you've built and expose it on the phone side. Introduce things we haven't dreamt of.

**The B3 Plugin Marketplace.** Babel3 ships with a [plugin marketplace](https://github.com/babel3-com/b3-plugins) — curated plugins for Claude Code reviewed for quality, accuracy, and safety. Install plugins with `claude plugin marketplace add babel3-com/b3-plugins`. Build your own plugins and submit them for listing. The marketplace is where the ecosystem grows.

You want to build something really cool and keep it proprietary? We don't judge. Apache 2.0 means no restrictions. The license just asks for attribution.

## Repository Structure

```
babel3/
├── crates/
│   ├── b3-cli/           # CLI + daemon (Rust)
│   ├── b3-common/        # Shared types and utilities
│   ├── b3-gpu-relay/     # Reliability relay sidecar for GPU worker
│   ├── b3-reliable/      # Mosh-like reliability layer (ordered delivery, checksums, retransmit)
│   └── b3-webrtc/        # WebRTC data channel abstraction (wraps datachannel-rs)
├── browser/
│   ├── js/               # Dashboard (vanilla JS, no framework)
│   └── css/              # Mobile-first styles
├── gpu-worker/
│   ├── Dockerfile        # GPU worker container
│   └── handler.py        # TTS, cloning, embeddings
├── plugins/
│   └── b3/               # B3 plugin (voice, hive, codebase skill)
├── docs/                 # Documentation
└── install.sh            # Platform-detecting installer
```

## Building from Source

```bash
# Prerequisites: Rust 1.93+, Docker (for GPU worker)
cargo build -p b3-cli
docker build -t babel3-gpu gpu-worker/

# Development mode — serve local browser build
b3 start --browser-dir ./browser
```


## Contributing

We welcome contributions. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[Apache-2.0](LICENSE). See [LICENSING.md](LICENSING.md) for details.

**Patent Notice:** Babel3 includes patent-pending technology (US 64/010,742, US 64/010,891, US 64/011,207). See [PATENTS](PATENTS). The Apache 2.0 license grants you a royalty-free patent license — the patents protect the ecosystem, not restrict it.


---

<p align="center">
  <strong>Code by talking. Build while you walk. Your chair is now optional.</strong>
</p>
