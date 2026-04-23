/* core.js — Babel3 shared namespace and config (open-source)
 *
 * Reads server-injected config from window.__HC__ and exposes the
 * global HC namespace used by all other modules.
 *
 * Load order: this file MUST load before any other HC module.
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

window.HC = window.HC || {};

/* ── Audio Lock ─────────────────────────────────────────────────────
 * Global lock that defers RTCPeerConnection operations while the audio
 * session is active (recording or playing). Safari iOS deadlocks when
 * RTCPeerConnection init triggers AVAudioSession.setCategory while the
 * audio thread is busy. This lock prevents that by queuing RTC ops
 * and flushing them when audio goes idle.
 */
(function() {
    var _locked = false;
    var _queue = [];
    var _reasons = {}; // track what's holding the lock (e.g., 'recorder', 'playback')

    HC.audioLock = function(reason) {
        _reasons[reason] = true;
        _locked = true;
    };

    HC.audioUnlock = function(reason) {
        delete _reasons[reason];
        if (Object.keys(_reasons).length === 0) {
            _locked = false;
            // Flush queued RTC operations
            var pending = _queue.splice(0);
            for (var i = 0; i < pending.length; i++) {
                try { pending[i](); } catch(e) { console.error('[AudioLock] flush error:', e); }
            }
            if (pending.length > 0) {
                HC.log('[AudioLock] Flushed ' + pending.length + ' deferred RTC ops');
            }
        }
    };

    // Run fn immediately if unlocked, or queue it for later
    HC.audioDefer = function(fn) {
        if (_locked) {
            _queue.push(fn);
            HC.log('[AudioLock] Deferred RTC op (queue=' + _queue.length + ', reasons=' + Object.keys(_reasons).join(',') + ')');
        } else {
            fn();
        }
    };

    HC.audioLocked = function() { return _locked; };
})();

/* ── Transport Mode ─────────────────────────────────────────────────
 * Controls which transport is used for daemon communication:
 *   'ws'  — WebSocket only (default — stable)
 *   'rtc' — WebRTC primary, WS fallback (experimental)
 * Persisted in an agent-scoped cookie so each agent tab remembers its own
 * transport independently (RTC experiments on one agent don't bleed to others).
 * Cookie key: 'b3-transport-{agentId}' — agentId read from window.__HC__
 * (server-injected before JS runs; HC.config is not set yet at file load time).
 * Toggle via HC.cycleTransport().
 */
HC._transportCookieKey = function() {
    // window.__HC__ is available synchronously — server injects it before this
    // script runs. HC.config is NOT available here (set later in boot IIFE).
    var agentId = (window.__HC__ && window.__HC__.agentId) || '';
    return 'b3-transport-' + agentId;
};
HC._getTransportCookie = function() {
    var key = HC._transportCookieKey();
    var escaped = key.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    var match = document.cookie.match(new RegExp('(?:^|;\\s*)' + escaped + '=(\\w+)'));
    return match ? match[1] : null;
};
HC._setTransportCookie = function(mode) {
    var key = HC._transportCookieKey();
    document.cookie = key + '=' + mode + '; path=/; max-age=31536000; SameSite=Lax';
};
HC.transportMode = HC._getTransportCookie() || 'ws';
// Migrate legacy 'auto' → 'ws'
if (HC.transportMode === 'auto') { HC.transportMode = 'ws'; }
// Persist to agent-scoped cookie
HC._setTransportCookie(HC.transportMode);

// Update transport button: text + split background showing connection state
// Top half = GPU connection, bottom half = daemon connection
HC._updateTransportBtn = function() {
    var btn = document.getElementById('fs-transport-btn');
    if (!btn) return;
    var m = HC.transportMode;
    btn.textContent = m === 'ws' ? 'WS' : 'RTC';

    var green = 'rgba(76,175,80,0.7)';
    var red = 'rgba(244,67,54,0.6)';
    var blue = 'rgba(79,195,247,0.5)';

    // Resolve active GPU channel: pool entry (EC2 worker) or tunnel fallback.
    // HC.gpuChannel is null in pool mode — must check pool first or button shows red
    // even when a dedicated worker is active and routing correctly.
    var _gpuCh = (HC._gpuActiveWorkerId && HC._gpuWorkerPool && HC._gpuWorkerPool[HC._gpuActiveWorkerId] && HC._gpuWorkerPool[HC._gpuActiveWorkerId].channel)
        ? HC._gpuWorkerPool[HC._gpuActiveWorkerId].channel
        : (HC.gpuChannel || null);

    var gpuColor, daemonColor;
    if (m === 'ws') {
        // Top half = GPU WS (through E2E encrypted relay to relay sidecar)
        // Bottom half = daemon WS (through E2E encrypted relay to daemon)
        var gpuWsOpen = _gpuCh ? !!_gpuCh.status().active : (HC._gpuWsReady && HC._gpuWs && HC._gpuWs.readyState === WebSocket.OPEN);
        var daemonWsOpen = HC.daemonChannel ? !!HC.daemonChannel.status().active : (HC.ws && HC.ws.readyState === WebSocket.OPEN);
        gpuColor = gpuWsOpen ? blue : red;
        daemonColor = daemonWsOpen ? blue : red;
    } else {
        var gpuOk = _gpuCh ? !!_gpuCh.status().active : false;
        var daemonOk = HC.daemonChannel ? !!HC.daemonChannel.status().active : (
            (HC._daemonRtc && HC._daemonRtc.connectionState === 'connected') ||
            (HC._daemonRtcTerminal && HC._daemonRtcTerminal.readyState === 'open') ||
            (HC._daemonRtcControl && HC._daemonRtcControl.readyState === 'open'));
        gpuColor = gpuOk ? green : red;
        daemonColor = daemonOk ? green : red;
    }

    btn.style.background = 'linear-gradient(to bottom, ' + gpuColor + ' 50%, ' + daemonColor + ' 50%)';
    btn.style.color = '#fff';
};

setInterval(function() { HC._updateTransportBtn(); }, 2000);
document.addEventListener('DOMContentLoaded', function() { HC._updateTransportBtn(); });

HC.cycleTransport = function() {
    // Toggle between RTC (default, with WS fallback) and WS-only (safe mode)
    HC.transportMode = HC.transportMode === 'ws' ? 'rtc' : 'ws';
    HC._setTransportCookie(HC.transportMode);
    HC.log('[Transport] Mode: ' + HC.transportMode);
    if (HC.transportMode === 'ws') {
        // Close ALL RTC objects — peer connections AND individual data channels
        if (HC._daemonRtcTerminal) { try { HC._daemonRtcTerminal.close(); } catch(e) {} HC._daemonRtcTerminal = null; }
        if (HC._daemonRtcControl) { try { HC._daemonRtcControl.close(); } catch(e) {} HC._daemonRtcControl = null; }
        if (HC._daemonRtc) { try { HC._daemonRtc.close(); } catch(e) {} }
        HC._daemonRtc = null;
        HC._daemonRtcReady = false;
        if (HC._gpuRtcDc) { try { HC._gpuRtcDc.close(); } catch(e) {} HC._gpuRtcDc = null; }
        if (HC._gpuRtc) { try { HC._gpuRtc.close(); } catch(e) {} }
        HC._gpuRtc = null;
        HC._gpuRtcReady = false;
        // Tell daemon to clear rtc_active — resume WS delivery for everything
        if (HC.ws && HC.ws.readyState === WebSocket.OPEN) {
            HC.ws.send(JSON.stringify({ type: 'rtc_inactive' }));
        }
        // Channels handle transport selection
        if (HC.gpuChannel) HC.gpuChannel.setAllowedTransports('ws');
        if (HC.daemonChannel) HC.daemonChannel.setAllowedTransports('ws');
    } else {
        // Switching to RTC — initiate RTC connections for both daemon and GPU
        if (HC.daemonRtcConnect) HC.daemonRtcConnect().catch(function(){});
        if (HC.gpuRtcConnect) HC.gpuRtcConnect().catch(function(){});
        if (HC.gpuChannel) HC.gpuChannel.setAllowedTransports('auto');
        if (HC.daemonChannel) HC.daemonChannel.setAllowedTransports('auto');
    }
    HC._updateTransportBtn();
    return HC.transportMode;
};

// Check if RTC should be used
HC.rtcEnabled = function() {
    return HC.transportMode !== 'ws';
};

/* ── Config ── */
HC.EC2_BASE = window.location.origin;

// HC.ready resolves when HC.config is fully populated.
// If window.__HC__ has a bootstrapUrl, fetch from the server first.
// If window.__HC__ already has direct fields (old cached page or daemon mode),
// bootstrapUrl is absent and we populate HC.config synchronously from cfg.
HC.ready = (async function() {
    var cfg = window.__HC__ || {};
    if (cfg.bootstrapUrl) {
        var url = cfg.bootstrapUrl;
        // Derive agent name from URL path when using bare '/api/bootstrap'.
        // Matches /a/:name and /app/a/:name/* (forward-compat with PR 2).
        if (url === '/api/bootstrap') {
            // Match /a/:parent/:child or /a/:name (also /app/a/... variants).
            // Two-segment match: m[1]=parent, m[2]=child (child must not contain '.' — guards against manifest.json).
            var m = window.location.pathname.match(/^\/a(?:pp\/a)?\/([^\/]+)\/([^\/\.]+)$/);
            if (m) {
                url = '/api/bootstrap/' + encodeURIComponent(m[1]) + '/' + encodeURIComponent(m[2]);
            } else {
                var m1 = window.location.pathname.match(/^\/a(?:pp\/a)?\/([^\/]+)/);
                if (m1) url = '/api/bootstrap/' + encodeURIComponent(m1[1]);
            }
        }
        try {
            var r = await fetch(url, { credentials: 'include' });
            if (!r.ok) throw new Error('bootstrap HTTP ' + r.status);
            var data = await r.json();
            cfg = Object.assign({}, cfg, data);
        } catch(e) {
            console.error('[HC] Bootstrap failed:', e);
            document.body.innerHTML = '<div style="color:#f85149;padding:2rem;font-family:monospace;">Dashboard bootstrap failed. <a href="/login" style="color:#58a6ff">Log in</a> and refresh.</div>';
            throw e;
        }
    }
    HC.config = {
        agentName:       cfg.agentName       || '',
        agentId:         cfg.agentId         || '',
        agentEmail:      cfg.agentEmail      || '',
        daemonToken:     cfg.daemonToken     || '',
        hostedSessionId: cfg.hostedSessionId || '',
        version:         cfg.version         || '0.0.0',
        domain:          cfg.domain          || window.location.hostname,
    };
    return HC.config;
})();

/* ── Math utilities ── */

/**
 * Cosine similarity between two equal-length numeric arrays.
 * @param {number[]} a
 * @param {number[]} b
 * @returns {number}
 */
HC.cosineSim = function(a, b) {
    var dot = 0, na = 0, nb = 0;
    for (var i = 0; i < a.length; i++) {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    var denom = Math.sqrt(na) * Math.sqrt(nb);
    return denom > 0 ? dot / denom : 0;
};

/* ── Daemon state ── */
HC._daemonBase = null;

/** Check if we have any daemon connection (tunnel URL or HTTP-over-WS relay). */
HC.hasDaemonConnection = function() {
    if (HC._daemonBase) return true;
    return !!(HC.ws && typeof HC.ws.sendHttpRequest === 'function' && HC.ws.readyState === 1);
};

/**
 * Authenticated fetch to the local daemon (adds Bearer token).
 * @param {string} path - URL path (e.g. '/api/health')
 * @param {Object} [options] - fetch options
 * @returns {Promise<Response>}
 */
HC.daemonFetch = function(path, options) {
    if (!HC._daemonBase) {
        // EC2 encrypted path: route through DaemonWS channel (HTTP-over-WS).
        var dws = HC.ws;
        if (dws && typeof dws.sendHttpRequest === 'function' && dws.readyState === 1) {
            return dws.sendHttpRequest(
                (options && options.method) || 'GET',
                path,
                (options && options.headers) || {},
                (options && options.body) || null
            );
        }
        console.warn('[daemonFetch] No daemon connection for:', path);
        return Promise.reject('no daemon');
    }
    var headers = Object.assign({}, (options && options.headers) || {},
        { 'Authorization': 'Bearer ' + HC.config.daemonToken });
    return fetch(HC._daemonBase + path, Object.assign({}, options, { headers: headers, credentials: 'include' }));
};

/* ── Voice Job Tracking (reliability) ── */

/**
 * Register a new voice job with EC2 for tracking.
 * Returns a Promise — callers should await before firing the first jobCheckpoint
 * so the row exists when the PATCH arrives. gpuRun fires concurrently on the
 * next line, so the audio path is not blocked.
 * @param {Object} job - { id, agent_id, job_type, msg_id?, text_preview?, voice?, duration_sec? }
 * @returns {Promise<string>} job ID
 */
HC.registerJob = function(job) {
    if (!HC.EC2_BASE || !HC.config) return Promise.resolve(job.id);
    job.reporter_version = 'browser:' + (HC._jsVersion || 'unknown');
    return fetch(HC.EC2_BASE + '/api/voice-jobs', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + HC.config.daemonToken },
        body: JSON.stringify(job),
    }).then(function() { return job.id; }).catch(function(e) {
        if (HC.log) HC.log('[VoiceJob] Register failed: ' + e.message);
        return job.id;
    });
};

/**
 * Update a voice job checkpoint. Fire-and-forget — non-fatal on failure.
 * @param {string} jobId - UUID of the job
 * @param {Object} patch - { status, gpu_job_id?, gpu_backend?, error?, ... }
 */
HC.jobCheckpoint = function(jobId, patch) {
    if (!jobId || !HC.EC2_BASE) return;
    patch.reporter_version = 'browser:' + (HC._jsVersion || 'unknown');
    fetch(HC.EC2_BASE + '/api/voice-jobs/' + jobId, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + HC.config.daemonToken },
        body: JSON.stringify(patch),
    }).catch(function(e) {
        if (HC.log) HC.log('[VoiceJob] Checkpoint failed: ' + e.message);
    });
};

/* ── Logging (console + EC2 + daemon WebSocket) ── */
(function() {
    var _logBuffer = [];
    var _daemonLogBuffer = [];
    var _logFlushTimer = null;
    var _daemonLogFlushTimer = null;

    function _flushDaemonLogs() {
        _daemonLogFlushTimer = null;
        if (_daemonLogBuffer.length === 0) return;
        var ws = HC.ws;
        if (!ws || ws.readyState !== 1) return;
        var batch = _daemonLogBuffer.splice(0);
        try { ws.send(JSON.stringify({ type: 'browser_log', logs: batch })); } catch(e) {}
    }

    function _flushLogs() {
        _logFlushTimer = null;
        if (_logBuffer.length === 0) return;
        var batch = _logBuffer.splice(0);
        fetch(HC.EC2_BASE + '/api/browser-log', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ agent: HC.config.agentName || 'unknown', logs: batch }),
        }).catch(function() {});
    }

    /**
     * Log to console AND forward to daemon (for MCP browser_console tool) + EC2.
     * @param {string} msg
     */
    HC.log = function(msg) {
        var ts = new Date().toLocaleTimeString();
        console.log('[Babel3] ' + ts + ' ' + msg);
        _logBuffer.push(ts + ' ' + msg);
        _daemonLogBuffer.push(ts + ' ' + msg);
        if (!_logFlushTimer) {
            _logFlushTimer = setTimeout(_flushLogs, 2000);
        }
        if (!_daemonLogFlushTimer) {
            _daemonLogFlushTimer = setTimeout(_flushDaemonLogs, 500);
        }
    };
})();

/* ── Global FPS Monitor ─────────────────────────────────────────────
 * Counts requestAnimationFrame callbacks per 60s window.
 * Healthy = ~60fps. Degradation over time = leak.
 * Reports to browser console (visible via browser_console filter="FPS").
 */
(function() {
    var _frames = 0, _start = Date.now(), _lastReport = Date.now();
    function tick() {
        _frames++;
        var now = Date.now();
        if (now - _lastReport >= 60000) {
            var elapsed = (now - _lastReport) / 1000;
            var fps = (_frames / elapsed).toFixed(0);
            var totalMin = ((now - _start) / 60000).toFixed(1);
            var mem = performance.memory ? ' heap=' + Math.round(performance.memory.usedJSHeapSize / 1048576) + 'MB' : '';
            HC.log('[FPS] ' + totalMin + 'min: ' + fps + 'fps' + mem);
            _frames = 0;
            _lastReport = now;
        }
        requestAnimationFrame(tick);
    }
    requestAnimationFrame(tick);
})();

// ── Upstream Health Diagnostics ───────────────────────────────────
// Exposed for browser_eval inspection when upstream stalls.
HC.upstreamHealth = function() {
    if (HC._daemonSession && HC._daemonSession.upstreamHealth) {
        return HC._daemonSession.upstreamHealth();
    }
    return { error: 'no daemon session' };
};
