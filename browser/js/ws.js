/* ws.js — Babel3 WebSocket connection to daemon (open-source)
 *
 * WebSocket connection to daemon via E2E encrypted relay, MCP tool call reflow,
 * daemon password authentication, terminal data routing.
 *
 * Depends on: core.js (HC namespace), terminal.js (HC.term)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

var ws = null;
var wsReconnectTimer = null;
var _wsReconnectDelay = 3000;
var _wsConsecutiveFailures = 0;
var _wsOpenedOnce = false; // true after first onopen; reset each new connectWS()

// ── Delta coalescing ──
// WS messages arrive as individual events. When a tab is backgrounded, the
// browser throttles JS — deltas queue up and all fire at once on focus. Without
// coalescing, HC.term.write() is called N times in a tight loop with no paints
// between calls, causing a visible freeze until the queue drains.
//
// Fix: buffer the terminal write across requestAnimationFrame. Byte accumulation
// (allTerminalBytes etc.) stays synchronous so the buffer is always current.
// Only HC.term.write() is deferred — at most once per animation frame.
//
// Coalescing rules:
//   rebuild + anything  → rebuild wins (reset + write full buffer)
//   delta   + delta     → concat bytes, one incremental write
//   offsetFromBottom    → captured at first delta in frame; scroll restore fires after write
var _pendingTermFlush = null; // { isRebuild, bytes, offsetFromBottom } | null
var _termFlushScheduled = false;

function _scheduleDeltaFlush(isRebuild, bytes, offsetFromBottom) {
    if (!_pendingTermFlush) {
        _pendingTermFlush = { isRebuild: isRebuild, bytes: bytes, offsetFromBottom: offsetFromBottom };
    } else if (isRebuild) {
        // New rebuild supersedes pending — use latest full-buffer bytes, keep earliest scroll offset
        _pendingTermFlush = { isRebuild: true, bytes: bytes, offsetFromBottom: _pendingTermFlush.offsetFromBottom };
    } else if (_pendingTermFlush.isRebuild) {
        // Incremental delta after pending rebuild: rebuild still wins.
        // bytes here is a displayDelta, not the full buffer — ignore it.
        // The rebuild will write allTerminalBytes which already includes this delta
        // (byte accumulation is synchronous, runs before _scheduleDeltaFlush is called).
        // Just update the rebuild bytes to the current full buffer.
        _pendingTermFlush = { isRebuild: true, bytes: allTerminalBytes, offsetFromBottom: _pendingTermFlush.offsetFromBottom };
    } else {
        // Coalesce incremental deltas
        var cat = new Uint8Array(_pendingTermFlush.bytes.length + bytes.length);
        cat.set(_pendingTermFlush.bytes);
        cat.set(bytes, _pendingTermFlush.bytes.length);
        _pendingTermFlush = { isRebuild: false, bytes: cat, offsetFromBottom: _pendingTermFlush.offsetFromBottom };
    }
    if (!_termFlushScheduled) {
        _termFlushScheduled = true;
        requestAnimationFrame(function() {
            _termFlushScheduled = false;
            var f = _pendingTermFlush;
            _pendingTermFlush = null;
            if (!f || !HC.term) return;
            if (f.isRebuild) {
                HC.term.reset();
                HC.term.write(clampCursorUp(f.bytes));
            } else {
                HC.term.write(clampCursorUp(f.bytes));
            }
            if (f.offsetFromBottom > 0) {
                requestAnimationFrame(function() {
                    var newBase = HC.term.buffer.active.baseY;
                    var targetY = Math.max(0, newBase - f.offsetFromBottom);
                    HC.term.scrollToLine(targetY);
                });
            }
        });
    }
}

// ── WebSocket to Daemon (peer-to-peer via tunnel) ──
let allTerminalBytes = new Uint8Array(0);
let allTerminalBytesRaw = new Uint8Array(0);      // unmodified bytes for full rebuilds
let allTerminalBytesReflowed = new Uint8Array(0); // reflowed version — always maintained regardless of toggle state


// ── MCP Reflow Toggle ──
// Persisted in localStorage. Toggle button in right-hand button stack (show_reflow_toggle layout flag).
HC._reflowEnabled = HC._prefs ? (HC._prefs.reflow_enabled !== false) : false; // default: false (from LAYOUT_DEFAULTS)

HC._updateReflowBtn = function() {
    var btn = document.getElementById('fs-reflow-toggle-btn');
    if (!btn) return;
    btn.textContent = HC._reflowEnabled ? '\xb6' : '\xb6';
    btn.title = HC._reflowEnabled ? 'MCP Reflow ON \u2014 click to disable' : 'MCP Reflow OFF \u2014 click to enable';
    btn.style.color = HC._reflowEnabled ? '#aaffaa' : 'rgba(255,255,255,0.4)';
};

HC.cycleReflowMode = function() {
    HC._reflowEnabled = !HC._reflowEnabled;
    if (HC.saveLayoutSetting) HC.saveLayoutSetting('reflow_enabled', HC._reflowEnabled);
    HC._updateReflowBtn();
    log('[Reflow] ' + (HC._reflowEnabled ? 'enabled' : 'disabled') + ' \u2014 rebuilding terminal from ' + (HC._reflowEnabled ? 'reflowed' : 'raw') + ' buffer');
    if (!HC.term) return;
    var src = HC._reflowEnabled ? allTerminalBytesReflowed : allTerminalBytesRaw;
    allTerminalBytes = src;
    HC.term.reset();
    HC.term.write(clampCursorUp(src));
};

// Expose both buffers for download/diff analysis
HC._getTerminalBuffers = function() {
    return { raw: allTerminalBytesRaw, reflowed: allTerminalBytesReflowed };
};

// Sync button state once DOM is ready
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', HC._updateReflowBtn);
} else {
    HC._updateReflowBtn();
}

function b64ToBytes(b64) {
    const raw = atob(b64);
    const bytes = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
    return bytes;
}

// WS message tracking (for logging/diagnostics only — no transport management)
let _wsLastMsg = 0;
let _wsMsgCount = 0;
// Stale detection REMOVED — reconnection handled exclusively by
// ReliableMultiTransport (daemon heartbeats + exponential backoff).
function _wsStartStaleDetector() {} // no-op, kept for call sites
function _wsStopStaleDetector() {} // no-op

// ── Cursor-Up Clamp ──
// Ink (React-for-CLI) emits \e[NA cursor-up sequences where N can exceed
// the viewport height during re-renders. This causes every terminal (browser
// xterm.js AND native) to follow the cursor into scrollback — jumping to top.
// Fix: clamp N to viewport rows. See vadimdemedes/ink#917.
function clampCursorUp(data) {
    if (!HC.term) return data;
    var rows = HC.term.rows;
    if (!rows || rows < 1) return data;

    // Work on string for regex. Convert if needed.
    var isBytes = data instanceof Uint8Array;
    var text = isBytes ? new TextDecoder().decode(data) : data;

    // Quick check: does it contain any cursor-up sequence?
    if (text.indexOf('\x1b[') === -1) return data;

    // Match \e[NA (cursor up) where N > rows
    var changed = false;
    var result = text.replace(/\x1b\[(\d+)A/g, function(match, n) {
        var num = parseInt(n, 10);
        if (num > rows) {
            changed = true;
            return '\x1b[' + rows + 'A';
        }
        return match;
    });

    if (!changed) return data;
    return isBytes ? new TextEncoder().encode(result) : result;
}

// ── MCP Tool Call Reflow ──
// On narrow terminals (< 80 cols), Ink aligns MCP tool call parameters
// to the column after the opening parenthesis (e.g., col 38 for a 38-char
// tool name). This wastes the left half of the screen. The reflow function
// detects MCP parameter regions and re-wraps text to use the full width.
//
// Two rendering modes in the byte stream:
// 1. Full redraws: lines separated by \r\r\n, CUF for indentation
// 2. Incremental deltas: \r + CUF + CUD for cursor positioning
// Both use \x1b[NC (Cursor Forward N) for parameter indentation.
function reflowMcpBytes(data) {
    const cols = HC.term.cols;
    if (cols >= 80) return data;

    let text;
    if (data instanceof Uint8Array) {
        text = new TextDecoder().decode(data);
    } else {
        text = String(data);
    }

    // Only process chunks that contain a real Ink-rendered MCP tool call.
    // Ink renders: \e[1m...tool_name\e[1C(MCP)\e[22m then params then ⎿
    // Skip if text only mentions (MCP) in conversation (no bold styling).
    if (!text.includes('(MCP)')) return data;
    // Check for bold marker before (MCP) — real Ink calls have \e[1m
    if (!text.includes('\x1b[1m')) return data;

    const INDENT = 4;
    const wrapWidth = cols - INDENT;
    const indentStr = ' '.repeat(INDENT);

    // Process all MCP blocks in the text.
    // Match only Ink-rendered calls: \e[22m immediately follows (MCP)
    let result = '';
    let pos = 0;
    while (pos < text.length) {
        // Find (MCP) followed by \e[22m (unbold) — Ink's signature
        const mcpIdx = text.indexOf('(MCP)', pos);
        if (mcpIdx === -1) { result += text.substring(pos); break; }

        const afterMCP = mcpIdx + 5;
        // Verify this is an Ink-rendered call: \e[22m within 10 chars
        const unboldCheck = text.substring(afterMCP, afterMCP + 20);
        if (!unboldCheck.includes('\x1b[22m') && !unboldCheck.includes('\x1b[0m') && !unboldCheck.startsWith('\r')) {
            // Not an Ink call — skip past this (MCP)
            result += text.substring(pos, afterMCP);
            pos = afterMCP;
            continue;
        }

        let resultIdx = text.indexOf('\u23bf', afterMCP);
        // No ⎿ yet — tool call is in-progress. Reflow to end of buffer so
        // the params appear wide immediately rather than waiting for completion.
        if (resultIdx === -1) { resultIdx = text.length; }

        // Copy everything before the parameter region
        result += text.substring(pos, afterMCP);

        // Extract parameter region between (MCP)... and ⎿
        const paramRegion = text.substring(afterMCP, resultIdx);

        // Two-pass cleaning:
        // Pass 1: Handle cursor movements context-sensitively.
        // When CUF follows \r or \n, it's indentation for a continuation
        // line — join without space (word was split at column boundary).
        // When CUF follows visible text, it's a word separator — use space.
        let pass1 = paramRegion
            .replace(/\x1b\[\?2026[hl]/g, '')  // sync markers → strip
            .replace(/\x1b\[\d*[JKPL]/g, '')   // erase sequences → strip
            .replace(/\x1b\[\d+;\d+[Hf]/g, '') // absolute cursor → strip
            .replace(/\x1b\[\d*[GH]/g, '')     // CHA/CUP → strip
            .replace(/\x1b\[2K/g, '');          // erase line → strip

        // Strip cursor movement sequences context-sensitively:
        // - CUD/CUU/CUB → strip (not content)
        // - Linebreak + CUF = continuation line. Ink encodes word boundaries
        //   by using CUF(N+1) instead of CUF(N): the extra column IS the space.
        //   CUF(N) = mid-word split → join without space.
        //   CUF(N+1) or higher = word boundary → insert space.
        //   We encode the CUF value in the placeholder for resolution.
        // - CUF alone (between visible chars) = word separator → space
        pass1 = pass1.replace(/\x1b\[\d+[ABD]/g, '');
        // Replace linebreak+CUF (including delta mode: \r + spaces + CUF)
        // with placeholder that encodes the LAST CUF value.
        // Delta mode: \r\x1b[1C<spaces>\x1b[27C → total CUF = 1+27 = 28...
        // but actually the spaces+CUF are ALL indentation. We need the sum.
        // Full mode: \r\r\n\x1b[38C or \r\r\n\x1b[39C
        // Delta mode: \r\x1b[NNC<spaces>\x1b[NNC (multiple CUFs + spaces)
        pass1 = pass1.replace(/[\r\n]+(?:\x1b\[(\d+)C| )+/g, function(m) {
            // Sum all CUF values + count literal spaces to get total columns
            var total = 0;
            var cufRe = /\x1b\[(\d+)C/g;
            var cm;
            while ((cm = cufRe.exec(m)) !== null) total += parseInt(cm[1]);
            // Count literal spaces
            for (var ci = 0; ci < m.length; ci++) {
                if (m[ci] === ' ') total++;
            }
            return '\x07LB' + total + '\x07';
        });
        // Inline CUF (word separator) → single space
        pass1 = pass1.replace(/\x1b\[\d+C/g, ' ');
        // Resolve placeholders using CUF values and segment lengths.
        // Split by placeholders: [seg0, cuf1, seg1, cuf2, seg2, ...]
        // CUF > baseline → word boundary → insert space.
        // CUF == baseline → check previous segment visible length:
        //   if shorter than line width → word boundary (word-wrapped short)
        //   if equal to line width → mid-word split → join.
        var lbParts = pass1.split(/\x07LB(\d+)\x07/);
        // Find the baseline CUF (minimum among content continuations).
        // Exclude very small values (< 10) — those are transitions to the
        // result line (\r\n\x1b[2C before ⎿), not content continuations.
        var minCuf = 9999;
        for (var lbi = 1; lbi < lbParts.length; lbi += 2) {
            var lbv = parseInt(lbParts[lbi]);
            if (lbv >= 10 && lbv < minCuf) minCuf = lbv;
        }
        if (minCuf === 9999) minCuf = 0; // fallback: no valid continuations
        // Determine the original line width (visible chars per line).
        // Find the MAX segment visible length — full lines = max width.
        var maxSegLen = 0;
        for (var lbi2 = 0; lbi2 < lbParts.length; lbi2 += 2) {
            var segVis = lbParts[lbi2].replace(/\x1b\[\d*[a-zA-Z]/g, '').length;
            if (segVis > maxSegLen) maxSegLen = segVis;
        }
        var origLineWidth = maxSegLen; // max visible chars on any line = full width

        pass1 = lbParts[0];
        for (var lbi3 = 1; lbi3 < lbParts.length; lbi3 += 2) {
            var lbCuf = parseInt(lbParts[lbi3]);
            var lbNext = lbParts[lbi3 + 1] || '';

            if (lbCuf < 10) {
                // Small CUF = transition to result line → end of content
                pass1 += ' ' + lbNext;
            } else if (lbCuf > minCuf) {
                // CUF > baseline → word boundary → space
                pass1 += ' ' + lbNext;
            } else {
                // CUF == baseline → check previous segment length
                var prevCuf = (lbi3 >= 3) ? parseInt(lbParts[lbi3 - 2]) : minCuf;
                var prevSeg = lbParts[lbi3 - 1] || lbParts[0];
                var prevVisLen = prevSeg.replace(/\x1b\[\d*[a-zA-Z]/g, '').length;
                // Line width depends on which CUF started that line
                var prevLineW = origLineWidth - (prevCuf - minCuf);

                if (prevVisLen < prevLineW) {
                    pass1 += ' ' + lbNext; // Short line → word boundary
                } else {
                    pass1 += lbNext; // Full line → mid-word join
                }
            }
        }

        // Join any remaining mid-word wraps (bare \r\n without CUF)
        pass1 = pass1.replace(/(\w)\r?\n[ ]*(\w)/g, '$1$2');
        let cleaned = pass1
            .replace(/\r\n/g, ' ')
            .replace(/\r/g, ' ')
            .replace(/\n/g, ' ')
            .replace(/\u2500+/g, '')            // strip box-drawing ─ separators
            .replace(/ {2,}/g, ' ')
            .trim();

        // Strip leading ) that bleeds from the tool name line
        cleaned = cleaned.replace(/^\)+\s*/, '');

        // Word-wrap the cleaned parameter text
        const words = cleaned.split(' ').filter(function(w) { return w.length > 0; });
        const lines = [];
        let curLine = '';
        for (let wi = 0; wi < words.length; wi++) {
            const word = words[wi];
            if (curLine.length + word.length + 1 > wrapWidth && curLine.length > 0) {
                lines.push(curLine);
                curLine = word;
            } else {
                curLine += (curLine ? ' ' : '') + word;
            }
        }
        if (curLine) lines.push(curLine);

        // Emit reflowed parameters on new lines with indent
        for (let li = 0; li < lines.length; li++) {
            result += '\r\n' + indentStr + lines[li];
        }
        result += '\r\n  ';

        pos = resultIdx;
    }

    if (data instanceof Uint8Array) {
        return new TextEncoder().encode(result);
    }
    return result;
}

// ── Daemon password authentication ──
var _daemonPwCookieName = 'b3-daemon-pw-' + (typeof HC.config.agentName !== 'undefined' ? HC.config.agentName : 'default');
var _daemonAuthPending = false;

// Auto-auth from URL fragment: #pw=<password> (used by hosted session "Open" button)
// Fragment never hits the server — stays client-side only.
(function() {
    var hash = window.location.hash;
    if (hash && hash.indexOf('#pw=') === 0) {
        var pw = decodeURIComponent(hash.substring(4));
        if (pw) {
            document.cookie = _daemonPwCookieName + '=' + encodeURIComponent(pw) + '; path=/; max-age=31536000; SameSite=Strict; Secure';
        }
        // Clear the fragment so password isn't visible in address bar or history
        history.replaceState(null, '', window.location.pathname + window.location.search);
    }
})();

function _getDaemonPw() {
    var m = document.cookie.match(new RegExp('(^| )' + _daemonPwCookieName + '=([^;]+)'));
    return m ? decodeURIComponent(m[2]) : null;
}
// Expose for rtc.js auth
HC._getDaemonPw = _getDaemonPw;
function _saveDaemonPw(pw) {
    document.cookie = _daemonPwCookieName + '=' + encodeURIComponent(pw) + '; path=/; max-age=31536000; SameSite=Strict; Secure';
}
function _clearDaemonPw() {
    document.cookie = _daemonPwCookieName + '=; path=/; max-age=0';
}

function _showDaemonPwPrompt() {
    // If prompt already visible, don't recreate (prevents stomping user input on reconnect)
    if (document.getElementById('daemon-pw-overlay')) return;
    // Create overlay
    var overlay = document.createElement('div');
    overlay.id = 'daemon-pw-overlay';
    overlay.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.8);z-index:9999;display:flex;align-items:center;justify-content:center;pointer-events:none;';
    var box = document.createElement('div');
    box.style.cssText = 'background:#161b22;border:1px solid #30363d;border-radius:8px;padding:1.5rem;max-width:360px;width:90%;pointer-events:auto;';
    box.innerHTML = '<h3 style="color:#fff;margin-bottom:0.5rem;font-size:1rem;">Daemon Password</h3>' +
        '<p style="color:#8b949e;font-size:0.85rem;margin-bottom:1rem;">Enter the password shown when this agent started.</p>' +
        '<div style="position:relative;margin-bottom:0.75rem;">' +
        '<input type="password" id="daemon-pw-input" style="width:100%;padding:8px 10px;padding-right:40px;background:#0d1117;border:1px solid #30363d;border-radius:4px;color:#c9d1d9;font-size:0.9rem;font-family:monospace;outline:none;box-sizing:border-box;" placeholder="Password" autocomplete="off">' +
        '<button id="daemon-pw-eye" type="button" style="position:absolute;right:6px;top:50%;transform:translateY(-50%);background:none;border:none;color:#8b949e;cursor:pointer;font-size:1.1rem;padding:4px;line-height:1;" title="Show password">&#128065;</button>' +
        '</div>' +
        '<div id="daemon-pw-error" style="color:#f85149;font-size:0.8rem;margin-bottom:0.5rem;display:none;"></div>' +
        '<button id="daemon-pw-submit" style="width:100%;padding:8px;background:#58a6ff;border:none;border-radius:4px;color:#fff;font-size:0.9rem;cursor:pointer;">Connect</button>';
    overlay.appendChild(box);
    document.body.appendChild(overlay);
    var inp = document.getElementById('daemon-pw-input');
    var btn = document.getElementById('daemon-pw-submit');
    var eye = document.getElementById('daemon-pw-eye');
    inp.focus();
    eye.addEventListener('click', function() {
        if (inp.type === 'password') {
            inp.type = 'text';
            eye.style.color = '#58a6ff';
        } else {
            inp.type = 'password';
            eye.style.color = '#8b949e';
        }
        inp.focus();
    });
    inp.addEventListener('keydown', function(e) { if (e.key === 'Enter') btn.click(); });
    btn.addEventListener('click', function() {
        var pw = inp.value.trim();
        if (!pw) return;
        _saveDaemonPw(pw);
        // Send auth — overlay stays until auth_ok confirms success
        if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: 'auth', password: pw }));
        }
        btn.disabled = true;
        btn.textContent = 'Verifying...';
    });
}

function _showDaemonPwError() {
    _clearDaemonPw();
    var el = document.getElementById('daemon-pw-overlay');
    if (el) {
        var err = document.getElementById('daemon-pw-error');
        if (err) { err.textContent = 'Invalid password. Try again.'; err.style.display = 'block'; }
        var inp = document.getElementById('daemon-pw-input');
        if (inp) { inp.value = ''; inp.focus(); }
        var btn = document.getElementById('daemon-pw-submit');
        if (btn) { btn.disabled = false; btn.textContent = 'Connect'; }
        return;
    }
    // No overlay visible — show it
    _showDaemonPwPrompt();
}

function handleWsMessage(e) {
    try {
        _wsLastMsg = Date.now();
        _wsMsgCount++;

        // ── Reliable frame check ──
        // Binary data with 0xB3 prefix = reliable frame from daemon.
        // Feed to ReliableChannel which calls onmessage with decoded payload.
        if (e.data instanceof ArrayBuffer || e.data instanceof Blob) {
            var _reliableTarget = HC.daemonChannel || HC._wsReliable;
            if (_reliableTarget) {
                var process = function(buf) {
                    var u8 = new Uint8Array(buf);
                    if (HC.reliableIsReliable && HC.reliableIsReliable(u8)) {
                        if (_reliableTarget.noteRecv) _reliableTarget.noteRecv('ws');
                        _reliableTarget.receive(u8);
                    }
                    // else: unknown binary, ignore
                };
                if (e.data instanceof Blob) {
                    e.data.arrayBuffer().then(process);
                } else {
                    process(e.data);
                }
            }
            return;
        }

        const msg = JSON.parse(e.data);

        // ── Auth protocol ──
        if (msg.type === 'auth_required') {
            _daemonAuthPending = true;
            var savedPw = _getDaemonPw();
            if (savedPw) {
                // Try saved password automatically
                ws.send(JSON.stringify({ type: 'auth', password: savedPw }));
            } else {
                _showDaemonPwPrompt();
            }
            return;
        }
        if (msg.type === 'auth_ok') {
            _daemonAuthPending = false;
            var el = document.getElementById('daemon-pw-overlay');
            if (el) el.remove();
            log('[WS] Daemon password accepted');
            // Re-send version and browser_info — the auth loop skipped them
            if (HC._serverVersion) {
                ws.send(JSON.stringify({ type: 'version', server_version: HC._serverVersion, js_version: HC._jsVersion }));
            }
            var _hasAC = typeof gaplessPlayer !== 'undefined' && HC.gaplessPlayer._ctx && HC.gaplessPlayer._ctx.state === 'running';
            ws.send(JSON.stringify({
                type: 'browser_info',
                user_agent: navigator.userAgent,
                volume: HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100,
                audio_context: !!_hasAC
            }));
            return;
        }
        if (msg.type === 'auth_failed') {
            _showDaemonPwError();
            return;
        }

        if (msg.type === 'heartbeat') {
            // Respond with pong so daemon knows this client is alive (prevents stale reaping)
            try { ws.send(JSON.stringify({ type: 'pong' })); } catch(e) {}
            return;
        }
        if (msg.type === 'welcome' && msg.session_id !== undefined) {
            HC._clientId = msg.session_id;
            log('[WS] Assigned session_id=' + msg.session_id);
            return;
        }
        if (msg.type !== 'delta' && msg.type !== 'init' && msg.type !== 'full') {
            log('[WS] Message: type=' + msg.type + (msg.msg_id ? ' msg=' + msg.msg_id : ''));
        }
        if (msg.type === 'init' || msg.type === 'full') {
            // Reset reliable channel — daemon also resets on init.
            // Both sides start fresh at seq 1 after init.
            var _channel = HC.daemonChannel || HC._wsReliable;
            var _prevSeq = _channel && _channel._reliable ? _channel._reliable._lastContiguous : -1;
            if (_channel && _channel._reliable && _channel._reliable.reset) {
                _channel._reliable.reset();
            }
            log('[WS] Received init — reset reliable channel (prev lastContiguous=' + _prevSeq + ', bytes=' + (msg.terminal_data||'').length + ')');
            let bytes = b64ToBytes(msg.terminal_data);
            allTerminalBytesRaw = bytes;
            allTerminalBytesReflowed = reflowMcpBytes(bytes); // always maintain reflowed buffer
            allTerminalBytes = HC._reflowEnabled ? allTerminalBytesReflowed : allTerminalBytesRaw;
            // Save scroll offset from bottom (survives buffer rebuild)
            const buf = HC.term.buffer.active;
            const wasScrolledBack = buf.viewportY < buf.baseY;
            const offsetFromBottom = buf.baseY - buf.viewportY;
            HC.term.reset();
            HC.term.write(clampCursorUp(allTerminalBytes));
            if (wasScrolledBack) {
                requestAnimationFrame(() => {
                    const newBase = HC.term.buffer.active.baseY;
                    const targetY = Math.max(0, newBase - offsetFromBottom);
                    HC.term.scrollToLine(targetY);
                });
            }

            document.getElementById('status-dot').className = 'status-dot online';
            // Re-fit terminal and send resize — server's reported size may be stale
            // Skip scroll restore in fitTerminal since we handle it above
            try { HC.fitAddon.fit(); } catch(e) {}
            HC.sendResize();
        } else if (msg.type === 'delta') {
            const rawDeltaBytes = b64ToBytes(msg.data);
            // Always compute reflowedDelta to keep allTerminalBytesReflowed accurate even
            // when reflow is off — toggling reflow on later needs a correct reflowed buffer.
            const reflowedDelta = reflowMcpBytes(rawDeltaBytes);
            const MAX_BROWSER_BYTES = 256 * 1024;

            // Accumulate raw bytes (always — needed for full rebuilds and reflow-off mode)
            const rawCombined = new Uint8Array(allTerminalBytesRaw.length + rawDeltaBytes.length);
            rawCombined.set(allTerminalBytesRaw);
            rawCombined.set(rawDeltaBytes, allTerminalBytesRaw.length);
            allTerminalBytesRaw = rawCombined.length > MAX_BROWSER_BYTES
                ? rawCombined.slice(rawCombined.length - MAX_BROWSER_BYTES)
                : rawCombined;

            const buf = HC.term.buffer.active;
            const atBottom = buf.viewportY >= buf.baseY;
            const offsetFromBottom = buf.baseY - buf.viewportY;

            // Full rebuild only needed when reflow is enabled AND the delta changes layout.
            //
            // When reflow is off (default): never rebuild on MCP deltas. Ink's cursor-up
            // in-place updates apply cleanly as incremental writes — no reset needed.
            // This eliminates the ~3s black screen on iOS (256KB term.write blocks main thread).
            //
            // When reflow is on: rebuild if (a) reflow actually transformed the delta
            // (⎿ present → reflowedDelta !== rawDeltaBytes), or (b) delta has MCP header
            // without ⎿ — cursor positioning in the delta targets the narrow format and
            // would misalign after reflow.
            const deltaText = new TextDecoder().decode(rawDeltaBytes);
            const deltaNeedsRebuild = HC._reflowEnabled && (
                reflowedDelta !== rawDeltaBytes ||
                (deltaText.includes('(MCP)') && deltaText.includes('\x1b[1m'))
            );

            if (deltaNeedsRebuild) {
                // Delta touches an MCP tool call block (with or without ⎿).
                // Cursor-positioning in the delta is calculated for the original
                // narrow format. Applying it incrementally would produce cursor-
                // position mismatches (wrong line count after reflow).
                // Full rebuild from raw bytes — ⎿ is present in allTerminalBytesRaw
                // even if absent from this delta, so reflow completes correctly.
                allTerminalBytesReflowed = reflowMcpBytes(allTerminalBytesRaw); // always update reflowed buffer
                allTerminalBytes = HC._reflowEnabled ? allTerminalBytesReflowed : allTerminalBytesRaw;
                _scheduleDeltaFlush(true, allTerminalBytes, offsetFromBottom);
            } else {
                // Normal delta — no MCP reflow needed. Accumulate reflowed buffer too.
                const reflowedCombined = new Uint8Array(allTerminalBytesReflowed.length + reflowedDelta.length);
                reflowedCombined.set(allTerminalBytesReflowed);
                reflowedCombined.set(reflowedDelta, allTerminalBytesReflowed.length);
                allTerminalBytesReflowed = reflowedCombined.length > MAX_BROWSER_BYTES
                    ? reflowedCombined.slice(reflowedCombined.length - MAX_BROWSER_BYTES)
                    : reflowedCombined;
                // Accumulate display buffer — write is deferred to rAF
                const displayDelta = HC._reflowEnabled ? reflowedDelta : rawDeltaBytes;
                const combined = new Uint8Array(allTerminalBytes.length + displayDelta.length);
                combined.set(allTerminalBytes);
                combined.set(displayDelta, allTerminalBytes.length);
                allTerminalBytes = combined.length > MAX_BROWSER_BYTES
                    ? combined.slice(combined.length - MAX_BROWSER_BYTES)
                    : combined;
                _scheduleDeltaFlush(false, displayDelta, offsetFromBottom);
            }
        } else if (msg.type === 'tts_stream') {
            // Deduplicate: daemon replays buffered tts_stream events on every WS reconnect.
            // _ttsSeenMsgIds is module-level and never cleared — entries persist across
            // reconnects so replayed events for already-completed messages are dropped.
            if (!HC._ttsSeenMsgIds) HC._ttsSeenMsgIds = {};
            if (HC._ttsSeenMsgIds[msg.msg_id]) {
                log('[TTS] [msg=' + msg.msg_id + '] Duplicate (seen) — skipping');
                return;
            }
            HC._ttsSeenMsgIds[msg.msg_id] = true;
            log('[TTS] [msg=' + msg.msg_id + '] Stream cmd: text=' + (msg.text||'').substring(0,40) + '...');
            if (msg.emotion) HC.setLedEmotion(msg.emotion);
            HC.enqueueTts(msg);
        } else if (msg.type === 'tts_generating') {
            log('[TTS] [msg=' + msg.msg_id + '] Generating');
            HC.setVoiceStatus('info', 'Generating audio...');
        } else if (msg.type === 'led') {
            HC.setLedEmotion(msg.emotion);
        } else if (msg.type === 'echo_pong') {
            // Upstream health echo — forward to reliable transport
            if (HC._daemonSession && HC._daemonSession.handleEchoPong) {
                HC._daemonSession.handleEchoPong(msg.id);
            }
        } else if (msg.type === 'hive_dm' || msg.type === 'hive_room') {
            // Store hive messages for terminal overlay (same pattern as transcription history)
            if (!HC._hiveHistory) HC._hiveHistory = [];
            HC._hiveHistory.push({
                type: msg.type,
                sender: msg.sender || 'unknown',
                text: msg.text || '',
                roomId: msg.room_id || '',
                roomTopic: msg.room_topic || '',
                members: msg.members || [],
                ts: Date.now(),
            });
            while (HC._hiveHistory.length > 50) HC._hiveHistory.shift();
            log('[Hive] ' + msg.type + ' from ' + (msg.sender || '?') + ': ' + (msg.text || '').substring(0, 60));
        } else if (msg.type === 'info') {
            if (msg.html && HC.showInfoContent) {
                HC.showInfoContent(msg.html);
            }
        } else if (msg.type === 'browser_eval') {
            // If targeted to a specific client, skip if we're not the target.
            // Daemon WS path sends target_session_id, EC2 SSE relay sends session_id (serde field).
            var _tid = msg.target_session_id !== undefined ? msg.target_session_id : msg.session_id;
            if (_tid !== undefined && HC._clientId !== undefined && _tid !== HC._clientId) {
                return;
            }
            let result;
            try {
                result = String(eval(msg.code));
            } catch(evalErr) {
                result = 'ERROR: ' + evalErr.message;
            }
            if (ws && ws.readyState === 1) {
                ws.send(JSON.stringify({ type: 'eval_result', eval_id: msg.eval_id, result: result }));
            }
        }
    } catch(err) {}
}

function connectWS() {
    if (!HC._daemonBase && !HC._pendingWs) {
        HC.setTerminalStatus('Daemon offline — waiting...');
        document.getElementById('status-dot').className = 'status-dot connecting';
        setTimeout(resolveAndConnect, 5000);
        return;
    }
    // Strip handlers before closing to prevent the onclose from scheduling
    // a rogue reconnect timer that would kill the new connection ~3s later.
    if (ws) { ws.onclose = null; ws.onerror = null; try { ws.close(); } catch(_) {} }
    _wsOpenedOnce = false;
    // Stable session ID for reconnect — shared with GPU relay
    if (!HC._daemonSessionId) {
        HC._daemonSessionId = sessionStorage.getItem('b3-daemon-session-id');
        if (!HC._daemonSessionId) {
            HC._daemonSessionId = String(Math.floor(Math.random() * 0x7FFFFFFF));
            sessionStorage.setItem('b3-daemon-session-id', HC._daemonSessionId);
        }
    }
    // Pre-seed dedup set from in-memory history before opening the socket.
    // Daemon replays tts_stream events synchronously on connect — this ensures
    // the seen set is populated before any replayed events arrive, preventing
    // GPU resubmission for already-completed messages.
    if (HC._ttsHistory && HC._ttsHistory.length) {
        if (!HC._ttsSeenMsgIds) HC._ttsSeenMsgIds = {};
        for (var _i = 0; _i < HC._ttsHistory.length; _i++) {
            var _e = HC._ttsHistory[_i];
            if (_e.msgId) HC._ttsSeenMsgIds[_e.msgId] = true;
        }
    }
    // Use a pre-created DaemonWS (E2E encrypted EC2 proxy) if boot.js set one,
    // otherwise fall back to a plain WebSocket via tunnel URL.
    if (HC._pendingWs) {
        ws = HC._pendingWs;
        HC._pendingWs = null;
    } else {
        const wsUrl = HC._daemonBase.replace(/^http/, 'ws') + '/ws?token=' + encodeURIComponent(HC.config.daemonToken) + '&session_id=' + HC._daemonSessionId;
        ws = new WebSocket(wsUrl);
    }
    HC.ws = ws; // Keep HC.ws in sync
    ws.binaryType = 'arraybuffer'; // needed for reliable binary frames
    var _wsOnOpen = () => {
        _wsOpenedOnce = true;
        // EC2 path confirmed working
        document.getElementById('status-dot').className = 'status-dot online';
        if (HC._fetchDaemonVersion) HC._fetchDaemonVersion();
        if (HC.refreshStatusBar) HC.refreshStatusBar();
        else HC.setTerminalStatus(HC.term.cols + 'x' + HC.term.rows, true);
        if (HC.textInput) {
            HC.textInput.disabled = false;
            HC.textInput.placeholder = 'Type a message...';
        }
        if (HC.fitTerminal) HC.fitTerminal();

        // ── Daemon channel setup ──
        // HC.daemonChannel is a ReliableMultiTransport that manages WS + RTC
        // with continuous sequence numbers. Created once, transports hot-added.
        if (HC.ReliableMultiTransport) {
            if (!HC.daemonChannel) {
                // First WS connect — create the channel
                HC.daemonChannel = new HC.ReliableMultiTransport({
                    transports: {},  // transports added dynamically below
                    maxBuffer: 1000,
                    ackInterval: 200,
                    onmessage: function(payload, seq, priority) {
                        try {
                            var text = (typeof payload === 'string') ? payload : new TextDecoder().decode(payload);
                            // Re-enter handleWsMessage with decoded JSON text
                            handleWsMessage({ data: text });
                        } catch(err) {}
                    },
                    onconnect: function(name) {
                        log('[Daemon] Transport connected: ' + name);
                    },
                });
                HC.daemonChannel.ontransportchange = function(active) {
                    log('[Daemon] Active transport: ' + (active || 'none'));
                };
                // Start telemetry heartbeat — ships structured JSON every 10s
                // via the raw WS text path (not the reliable channel). Must use
                // sendRaw so telemetry arrives at the daemon as Message::Text,
                // not as encrypted binary PTY data through the reliable bridge.
                HC.daemonChannel.startTelemetry('daemon', function(jsonStr) {
                    if (HC.ws && HC.ws.readyState === 1) {
                        if (HC.ws.sendRaw) HC.ws.sendRaw(jsonStr);
                        else HC.ws.send(jsonStr);
                    }
                });
                // On the EC2 encrypted path, terminal deltas flow through DaemonWS._innerRC
                // (not through the outer daemonChannel ReliableChannel). The default snapshot
                // reads daemonChannel._reliable._lastContiguous which is always 0 on EC2.
                // Augment the snapshot with inner RC stats when available so frames_in/frames_out
                // reflect actual terminal data throughput regardless of transport path.
                (function() {
                    var origSnapshot = HC.daemonChannel.telemetrySnapshot.bind(HC.daemonChannel);
                    HC.daemonChannel.telemetrySnapshot = function() {
                        var snap = origSnapshot();
                        var innerRC = HC.ws && HC.ws._innerRC;
                        if (innerRC && innerRC._lastContiguous > 0) {
                            // EC2 path: inner RC has actual frame counts
                            snap.frames_in = innerRC._lastContiguous;
                            snap.frames_out = innerRC._nextSeq - 1;
                            snap.last_contiguous = innerRC._lastContiguous;
                            snap.inner_rc = true;
                        }
                        return snap;
                    };
                })();
                // Start upstream health tracking — detects stale upstream + auto-reconnect
                HC.daemonChannel.startUpstreamHealth();
                HC._daemonSession = HC.daemonChannel;
            }
            // Register (or re-register) WS transport sink.
            // Construct handle first so it's available in the catch block.
            var wsHandle = {
                readyState: 'open',
                send: function(data) { if (ws.readyState === 1) ws.send(data); },
                // sendRaw: bypass DaemonWS._innerRC (encrypted reliable channel) and send
                // a plain text frame directly on the underlying WebSocket. Required for
                // echo_ping — daemon handles it as Message::Text, not as a reliable binary
                // frame delivered to the PTY bridge. Normal send() goes through _innerRC
                // and arrives at the daemon as binary PTY data, never reaching the text handler.
                sendRaw: function(text) { if (ws.readyState === 1 && ws.sendRaw) ws.sendRaw(text); },
                close: function() {},
                onmessage: null,
                onclose: null,
                _lastRecv: Date.now(),
            };
            try {
                // connect() returns null (handle is registered externally by _wsOnOpen),
                // but triggers real WS recovery so _scheduleReconnect doesn't loop forever
                // with no-op null returns (upstream health disconnect → _scheduleReconnect
                // → _connectTransport → null → infinite loop).
                HC.daemonChannel._transportDefs['ws'] = {
                    connect: function() {
                        // If DaemonWS is still open, re-register it as the transport handle
                        // (handles the case where upstream health called _disconnectTransport
                        // but the underlying socket is still alive).
                        // Otherwise trigger a full WS reconnect.
                        if (ws && ws.readyState === 1) {
                            setTimeout(function() { _wsOnOpen({}); }, 0);
                        } else if (HC.connectWS) {
                            HC.connectWS();
                        }
                        return null;
                    },
                    priority: 1,
                };
                HC.daemonChannel._disconnectTransport('ws');
                log('[WS] Setting _handles[ws]...');
                HC.daemonChannel._handles['ws'] = wsHandle;
                log('[WS] _handles[ws] set, calling _onTransportOpen...');
                HC.daemonChannel._onTransportOpen('ws', wsHandle, HC.daemonChannel._activeName());
                log('[WS] Daemon channel WS transport registered (resume seq=' + HC.daemonChannel._reliable.lastContiguousReceived() + ')');
            } catch (regErr) {
                log('[WS] ERROR during daemon channel WS registration: ' + regErr.message);
                // Ensure handle is set even if _onTransportOpen throws
                if (!HC.daemonChannel._handles['ws']) {
                    HC.daemonChannel._handles['ws'] = wsHandle;
                }
            }
        } else if (HC.ReliableChannel) {
            // Fallback: old standalone ReliableChannel
            if (HC._wsReliable) HC._wsReliable.destroy();
            HC._wsReliable = new HC.ReliableChannel({
                send: function(data) { if (ws.readyState === WebSocket.OPEN) ws.send(data); },
                maxBuffer: 1000, ackInterval: 200,
            });
            HC._wsReliable.onmessage = function(payload) {
                try {
                    var text = new TextDecoder().decode(payload);
                    handleWsMessage({ data: text });
                } catch(err) {}
            };
            HC._wsReliable.sendResume();
            log('[WS] Reliable channel active (resume seq=' + HC._wsReliable.lastContiguousReceived() + ')');
        }

        // Announce server version to daemon so it can track connected browsers
        if (HC._serverVersion) {
            ws.send(JSON.stringify({ type: 'version', server_version: HC._serverVersion, js_version: HC._jsVersion }));
        }
        // Send browser info (user-agent, volume, audio context) so daemon can distinguish browsers from terminals
        var _hasAudioCtx = typeof gaplessPlayer !== 'undefined' && HC.gaplessPlayer._ctx && HC.gaplessPlayer._ctx.state === 'running';
        ws.send(JSON.stringify({
            type: 'browser_info',
            user_agent: navigator.userAgent,
            volume: HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100,
            audio_context: !!_hasAudioCtx
        }));
        log('[WS] Connected to ' + (HC._daemonBase || (HC.EC2_BASE ? 'EC2 proxy (' + HC.EC2_BASE + ')' : 'unknown')));
        // Reset backoff after a stable connection (survived > 10s)
        setTimeout(() => {
            if (ws && ws.readyState === WebSocket.OPEN) {
                _wsConsecutiveFailures = 0;
                _wsReconnectDelay = 3000;
            }
        }, 10000);
        _wsStartStaleDetector();

        // Recover orphaned audio recordings from IndexedDB
        if (typeof HC._getOrphanedRecordings !== 'function') { log('[IDB] _getOrphanedRecordings not loaded yet — skipping orphan recovery'); } else HC._getOrphanedRecordings().then(orphans => {
            if (orphans.length === 0) return;
            log('[IDB] Found ' + orphans.length + ' orphaned recording(s) — re-uploading');
            for (const rec of orphans) {
                log('[IDB] Orphan seq=' + rec.seq + ' dur=' + rec.durSec + ' size=' + (rec.blob ? rec.blob.size : 0));
                if (rec.blob && rec.blob.size > 0) {
                    HC.submitTranscription(rec.blob, rec.durSec || '?', 0);
                    HC._deleteAudioBlob(rec.seq).catch(() => {});
                }
            }
        }).catch(e => log('[IDB] Orphan recovery error: ' + e.message));

        // Hydrate TTS history + info archive + GPU config in a single bundled request.
        // This replaces 2 separate daemon HTTP fetches with 1, reducing
        // relay contention that caused WS disconnect loops.
        HC.daemonFetch('/api/init-bundle').then(function(r) { return r.json(); }).then(function(bundle) {
            // TTS history — sync from daemon archive (source of truth)
            var entries = bundle.tts_history || [];
            if (entries.length) {
                entries.reverse(); // daemon returns newest-first; we want oldest-first
                HC._ttsHistory.length = 0;
                if (!HC._ttsSeenMsgIds) HC._ttsSeenMsgIds = {};
                for (var i = 0; i < entries.length; i++) {
                    var e = entries[i];
                    HC._ttsHistory.push({
                        msgId: e.msg_id,
                        textPreview: e.text || '',
                        emotion: e.emotion || '',
                        audioUrl: '/api/tts-archive/' + e.msg_id,
                        fromArchive: true
                    });
                    // Pre-seed dedup set: archive entries are already completed.
                    // Prevents replayed tts_stream events from resubmitting GPU jobs.
                    HC._ttsSeenMsgIds[e.msg_id] = true;
                }
                HC._updateHistoryControls();
                log('[TTS-Archive] Loaded ' + entries.length + ' archived messages');
            }
            // Info archive
            if (bundle.info_archive && HC._applyInfoArchive) {
                HC._applyInfoArchive(bundle.info_archive);
            } else if (bundle.info_archive && bundle.info_archive.length) {
                log('[Info] Loaded ' + bundle.info_archive.length + ' archived entries');
            }
            // gpu_config from daemon: only RunPod serverless credentials used now.
            // local_gpu_url/token are ignored — tunnel path removed in #417.
            if (bundle.gpu_config) {
                var gc = bundle.gpu_config;
                if (gc.gpu_url) {
                    HC.GPU_URL = gc.gpu_url;
                    HC.GPU_TOKEN = gc.gpu_token || '';
                }
            }
        }).catch(function(e) { log('[InitBundle] ' + e.message); });
    };
    ws.onopen = _wsOnOpen;
    // DaemonWS may already be READY by the time ws.js sets onopen — call immediately.
    // Also defer a check: DaemonWS async connect may resolve during microtask gap
    // between constructor and connectWS(), firing onopen(null) before we set it.
    if (ws.readyState === 1 || ws.readyState === WebSocket.OPEN) {
        _wsOnOpen({});
    }
    setTimeout(function() {
        if ((ws.readyState === 1 || ws.readyState === WebSocket.OPEN) && HC.daemonChannel && Object.keys(HC.daemonChannel._handles).length === 0) {
            log('[WS] Deferred onopen — daemon channel has no transport, calling _wsOnOpen');
            _wsOnOpen({});
        }
    }, 2000);
    ws.onmessage = handleWsMessage;
    ws.onerror = (ev) => {
        log('[WS] Connection error after ' + _wsMsgCount + ' messages, readyState=' + (ws ? ws.readyState : '?'));
        HC.setTerminalStatus('Connection error — retrying...');
        document.getElementById('status-dot').className = 'status-dot connecting';
    };
    ws.onclose = (ev) => {
        _wsStopStaleDetector();
        // Tag disconnect reason for telemetry
        var reason = 'unknown';
        if (ev.code === 1000 || ev.code === 1001) reason = 'clean_close';
        else if (ev.code === 1006) reason = 'tunnel_flap';  // abnormal closure, no Close frame
        else if (ev.code === 1011) reason = 'server_error';
        else if (ev.code === 1012 || ev.code === 1013) reason = 'server_restart';
        else if (!ev.wasClean) reason = 'abnormal_close';
        // Propagate reason to the reliable transport handle for structured logging
        if (HC.daemonChannel && HC.daemonChannel._handles && HC.daemonChannel._handles['ws']) {
            HC.daemonChannel._handles['ws']._disconnectReason = reason;
        }
        log('[WS] Closed code=' + (ev.code || '?') + ' reason=' + reason + ' clean=' + ev.wasClean + ' msgs=' + _wsMsgCount);
        document.getElementById('status-dot').className = 'status-dot connecting';
        // EC2 path failed — no tunnel fallback, retry EC2
        if (!_wsOpenedOnce) {
            log('[WS] EC2 path failed to connect — retrying');
        }
        // Exponential backoff on repeated rapid failures
        _wsConsecutiveFailures++;
        _wsReconnectDelay = Math.min(3000 * Math.pow(1.5, _wsConsecutiveFailures - 1), 30000);
        const delaySec = (_wsReconnectDelay / 1000).toFixed(1);
        HC.setTerminalStatus('Disconnected — reconnecting in ' + delaySec + 's...');
        if (wsReconnectTimer) clearTimeout(wsReconnectTimer);
        wsReconnectTimer = setTimeout(() => resolveAndConnect(), _wsReconnectDelay);
    };
}


// Expose to HC namespace
HC.ws = ws;
HC.connectWS = connectWS;
HC.wsSend = function(data) { if (ws && ws.readyState === WebSocket.OPEN) ws.send(data); };
HC.wsReadyState = function() { return ws ? ws.readyState : 3; };
HC._saveDaemonPw = _saveDaemonPw;
