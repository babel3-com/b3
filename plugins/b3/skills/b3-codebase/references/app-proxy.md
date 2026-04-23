# App Proxy — Browser App Development Guide

The app proxy lets a browser load a web app served by the daemon's local HTTP server, routed through the existing EC2 reverse tunnel. No inbound ports needed. The daemon side lives in `open-source/crates/b3-cli/src/daemon/ec2_proxy.rs`.

---

## What It Is

Every b3 daemon runs an embedded Axum HTTP server (`daemon/web.rs`) on a local port. Normally only reachable locally. The app proxy extends the reverse tunnel — already used for PTY I/O — to forward HTTP requests from a browser to that local server.

**Developer use case:** Visit `/app/a/<agent>/` to get the agent's dashboard with JS/CSS served from your local build instead of the hosted static files. This is the main loop for iterating on browser-side code.

**URL scheme (handled server-side):**
```
/app/a/<agent>/           → agent dashboard, /static/open/ assets rewritten to proxy
/app/a/<agent>/<path>     → proxied to daemon's local HTTP server at /<path>
/app/a/<parent>/<child>/  → child agent dashboard
/app/a/<parent>/<child>/<path> → proxied to child's daemon at /<path>
```

---

## How the Daemon Handles It (Open-Source Side)

### 1. Registration

When the daemon starts, it registers on the EC2 server and announces `app_enabled` and `app_public` flags based on CLI args (`--app`, `--app-public`). These reset on every restart.

### 2. Session request / dial-back

The server never opens connections to the daemon — the daemon always dials out. For each proxied HTTP request, the server:
1. Generates a `session_id`
2. Sends `{"type":"session_request","session_id":"..."}` on the daemon's existing control WebSocket
3. Waits up to 5s for the daemon to dial back

The daemon's `tunnel_client()` loop in `ec2_proxy.rs` receives the `session_request` and dials back:
```
GET /api/daemon-session?session_id=<id>   Bearer <api_key>
```

This creates a fresh WebSocket for the single HTTP exchange.

### 3. http_request frame (daemon side)

Inside `run_daemon_session()`, frames arrive from the server. The daemon checks for `"type": "http_request"` before routing to the PTY:

```rust
// ec2_proxy.rs — inside run_daemon_session()
if let Some(obj) = payload.as_object() {
    if obj.get("type").and_then(|v| v.as_str()) == Some("http_request") {
        handle_http_request(obj, web_port, api_key).await;
        continue;
    }
}
// otherwise: forward to PTY stdin
```

The handler:
1. Checks the `target` field: for `target=api` (PTY tunnel default), rejects paths not starting with `/api/`; for `target=app` (app proxy), there is no path validation — any path is forwarded to the daemon's local web server
2. Decodes base64 body (all bodies travel as base64 inside the JSON frame)
3. POSTs/GETs to `http://127.0.0.1:<web_port><path>` with `Bearer <api_key>`
4. Base64-encodes **all** response bodies unconditionally — handles binary and avoids JSON string escaping issues with arbitrary text content
5. Sends back `{"type":"http_response","status":N,"body":"<base64>","body_encoding":"base64","content_type":"..."}`

### Frame format

**http_request** (server → daemon):
```json
{
  "type": "http_request",
  "id": "<session_id>",
  "target": "app",
  "method": "GET",
  "path": "/api/status",
  "body": "<base64>",
  "body_encoding": "base64",
  "headers": { "content-type": "application/json" }
}
```

**http_response** (daemon → server):
```json
{
  "type": "http_response",
  "id": "<session_id>",
  "status": 200,
  "body": "<base64>",
  "body_encoding": "base64",
  "content_type": "application/json"
}
```

**Why base64 unconditionally?** The frames travel as JSON text over WebSocket. Encoding all bodies — not just binary ones — avoids both binary corruption and JSON string escaping issues with arbitrary text content (e.g. HTML with quotes, control characters in log output).

---

## Building a Browser App for the Proxy

Your app runs inside the daemon's embedded web server (`daemon/web.rs`). When accessed via `/app/a/<agent>/`, the dashboard HTML has all `/static/open/` URLs rewritten to go through the proxy — so your JS/CSS loads from your local build.

### Steps

1. **Start daemon with `--app`:**
   ```bash
   b3 start --app              # auth required (owner only)
   b3 start --app --app-public # no auth (anyone can access)
   ```

2. **Serve your app from the daemon's web server.** Add routes to `daemon/web.rs`. The server is a standard Axum router — add `GET "/api/myapp/..."` handlers there.

3. **Access via proxy:**
   ```
   https://babel3.com/app/a/<your-agent>/api/myapp/...
   ```

4. **Iterate locally:**
   Visit `https://babel3.com/app/a/<agent>/` to get the dashboard with `/static/open/` assets fetched from your daemon. Edit `browser/js/` files, reload — no deploy needed.

### Auth model

| Flag | Behavior |
|------|----------|
| No `--app` | Proxy disabled (404) |
| `--app` | Owner session cookie required; unauthenticated → 401; non-owner → 404 |
| `--app --app-public` | No auth — anyone can access |

---

## Key Files (Open-Source)

| File | Role |
|------|------|
| `open-source/crates/b3-cli/src/daemon/ec2_proxy.rs` | `run_daemon_session()` — receives http_request frames, proxies to local web server |
| `open-source/crates/b3-cli/src/daemon/web.rs` | Embedded daemon HTTP server — add app routes here |
| `open-source/browser/js/core.js:188` | Bootstrap URL derivation from `/app/a/` path |
