# Forking Guide

*The whole thing is yours to change. Here's where to start.*

Babel3 is designed to be forked. The browser, daemon, and GPU worker are all customizable. Apache 2.0 means you can build anything — proprietary features, commercial products, closed-source modifications. The license asks for attribution, nothing more.

## Fork the Browser

The most common fork. Redesign the dashboard, add panels, change the layout, build a completely different phone experience.

### What you're working with

14 standalone JavaScript files in `browser/js/` plus CSS in `browser/css/`. No framework, no build step. Each file is a self-contained module.

### How to start

```bash
# Fork the repo on GitHub, then:
git clone https://github.com/YOUR-USERNAME/b3.git
cd b3

# Serve your local browser during development
b3 start --browser-dir ./browser
```

Changes to `browser/js/` and `browser/css/` are picked up immediately — just refresh the page.

### Key files to customize

| What you want to change | File |
|------------------------|------|
| Terminal look and feel | `terminal.js` |
| Voice recording behavior | `voice-record.js` |
| TTS playback | `voice-play.js` |
| LED animations | `led.js`, `animations.js` |
| Dashboard layout | `layout.js`, `dashboard.css` |
| File browser | `files.js` |
| Info panel | `info.js` |

### Rogue browser mode

Users can switch their dashboard to any public GitHub fork:

1. Your fork must be a public GitHub fork of the Babel3 repo
2. Users select your fork in their dashboard settings
3. The server fetches and caches your browser assets from your repo
4. Your fork replaces the official dashboard entirely

When a user switches to your fork, their API key is rotated immediately for security. When they switch back, it rotates again.

## Fork the Daemon

This is where it gets interesting. Add custom MCP servers, new voice pipelines, integrations with your own tools.

### What you're working with

The daemon source is at `crates/b3-cli/src/`. It's Rust.

### Adding MCP tools

Edit `mcp/voice.rs`:

1. Add your tool definition to `tool_definitions()`:

```rust
{
    "name": "my_custom_tool",
    "description": "What your tool does",
    "inputSchema": {
        "type": "object",
        "properties": {
            "param1": {
                "type": "string",
                "description": "What this parameter is"
            }
        },
        "required": ["param1"]
    }
}
```

2. Add the handler in the `match` block (same file):

```rust
"my_custom_tool" => {
    let param1 = params.get("param1").as_str().unwrap_or("");
    // Your logic here
    make_response(id, Some(json!({"result": "success"})), None)
}
```

3. Build and test:

```bash
cargo build -p b3-cli
cargo run -p b3-cli -- start
```

Claude Code will discover your new tool automatically via the MCP protocol.

### Modifying the web server

The daemon's web server (`daemon/web.rs`) handles the HTTP API. Add new routes here for functionality that scripts and browsers can access directly.

### Modifying the bridge

The bridge (`bridge/pusher.rs` and `bridge/puller.rs`) handles communication with the babel3.com server. The puller processes incoming SSE events — add new event types here if you need custom server-to-daemon communication.

## Fork the GPU Worker

Add new ML capabilities — avatar rendering, custom TTS models, video processing, image generation.

### What you're working with

A Python-based Docker container at `gpu-worker/`. Core logic in `handler.py` (~1,500 lines).

### Adding a new action

Edit `handler.py`:

1. Add your handler function:

```python
def handle_my_action(job_input):
    """Your custom GPU action."""
    data = job_input.get("data", "")
    # Your ML logic here
    return {"result": "processed"}
```

2. Register it in the `handler()` dispatch:

```python
elif action == "my_action":
    result = handle_my_action(job_input)
```

3. Build and test:

```bash
cd gpu-worker
./build.sh v1
docker run -d --gpus all -p 5125:5125 $DOCKER_ORG/gpu-worker:v1
curl -X POST localhost:5125/runsync -d '{"input": {"action": "my_action", "data": "test"}}'
```

### VRAM budget

On a 24 GB GPU (4090), the default models use ~16 GB. You have ~8 GB of headroom for custom models. On a 48 GB GPU (A40), you have ~32 GB of headroom.

## Publishing your fork

### Browser forks

Push to a public GitHub fork. Users can select it in their dashboard settings. The server caches your assets automatically.

### Daemon and GPU worker forks

Build and distribute your own binaries/containers. Users configure their setup to point at your daemon or GPU worker.

### Contributing upstream

Want your changes in the main Babel3 repo? Open a PR. See [CONTRIBUTING.md](../CONTRIBUTING.md) for guidelines. If it improves the experience for everyone, we'll merge it.

## License

Apache 2.0 applies to all forks. You must:
- Include the LICENSE file
- Include the NOTICE file (attribution)
- Note any modifications you made

You may:
- Use commercially
- Modify without restriction
- Distribute modified versions
- Use patented technologies (royalty-free grant via Apache 2.0 Section 3)

You may not:
- Use the "Babel3" trademark to market your fork (Apache 2.0 Section 6)
- Remove the patent grant or copyright notices
