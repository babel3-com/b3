/* gpu.js — Babel3 GPU job submission and config (open-source)
 *
 * Fetches GPU credentials from EC2, monitors local GPU health,
 * submits jobs with local-first/RunPod-fallback routing.
 * Also: garbage embeddings for STT filtering, voice conditionals.
 *
 * Depends on: core.js (HC namespace)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

// Local aliases
var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// GPU state (shared via HC namespace)
HC.GPU_URL = null;
HC.GPU_TOKEN = null;
// EC2-registered GPU worker — kept for backward compat (read by gpuRun local lane guard,
// status bar, and external code). Mirrors HC._gpuActiveWorkerId.
HC._localWorkerId = null;
HC._localWorkerCapabilities = null;
HC._garbageEntries = [];
HC._garbageThreshold = 0.88;
HC._pendingChannelFinalize = null;      // stream_finalize queued when channel was inactive
HC._pendingChannelFinalizeWorkerId = null; // which worker's channel this finalize belongs to

// ── GPU Worker Pool ─────────────────────────────────────────────────
// Maintains simultaneous connections to all registered EC2 workers.
// Each entry: { channel, capabilities, version, category, status }
//   category: 'dedicated' | 'serverless'
//   status:   'connecting' | 'active' | 'failed'
HC._gpuWorkerPool = {};       // { [worker_id]: poolEntry }
HC._gpuActiveWorkerId = null; // worker_id currently routing jobs to

// Recompute active worker: dedicated+active > serverless+active > connecting.
// Called after any pool entry status change.
function _recomputeActiveWorker() {
    var best = null;
    var bestScore = -1;
    Object.keys(HC._gpuWorkerPool).forEach(function(wid) {
        var e = HC._gpuWorkerPool[wid];
        if (e.status === 'failed') return;
        // In runpod-only mode, dedicated workers are excluded from routing.
        // Applies on every recompute — covers mode switches mid-session.
        if (HC._gpuMode === 'runpod-only' && e.category === 'dedicated') return;
        // Score: dedicated+active=3, serverless+active=2, dedicated+connecting=1, serverless+connecting=0
        var score = (e.status === 'active' ? 2 : 0) + (e.category === 'dedicated' ? 1 : 0);
        if (score > bestScore) { bestScore = score; best = wid; }
    });
    var prev = HC._gpuActiveWorkerId;
    HC._gpuActiveWorkerId = best;
    HC._localWorkerId = best; // keep legacy field in sync
    if (best) {
        var entry = HC._gpuWorkerPool[best];
        HC._localWorkerCapabilities = entry.capabilities || {};
        HC._localGpuHealthy = entry.status === 'active';
    }
    if (prev !== best) {
        log('[GPU] Active worker: ' + (best ? best.slice(0, 8) + ' (' + (HC._gpuWorkerPool[best] && HC._gpuWorkerPool[best].category) + ')' : 'none'));
        // Discard pending finalize if it belonged to the old worker — stream context doesn't transfer
        if (HC._pendingChannelFinalize && HC._pendingChannelFinalizeWorkerId && HC._pendingChannelFinalizeWorkerId !== best) {
            log('[GPU] Finalize dropped — worker disconnected (stream_id=' + HC._pendingChannelFinalize.stream_id + ')');
            HC._pendingChannelFinalize = null;
            HC._pendingChannelFinalizeWorkerId = null;
        }
    }
}

// Create and register a pool entry for an EC2 worker. Idempotent by worker_id.
// If entry already exists and is not failed, returns existing entry.
function _poolAddWorker(workerId, capabilities, version) {
    var category = (capabilities && capabilities.category) || 'serverless';
    var existing = HC._gpuWorkerPool[workerId];
    if (existing && existing.status !== 'failed') {
        // Update metadata only
        existing.capabilities = capabilities || existing.capabilities;
        existing.version = version || existing.version;
        existing.category = category;
        return existing;
    }

    if (!HC.GpuWorkerWS || !HC.ReliableMultiTransport) {
        log('[GPU] Pool: cannot add worker=' + workerId + ' — GpuWorkerWS or ReliableMultiTransport not loaded');
        return null;
    }

    var ec2Base = HC.EC2_BASE || window.location.origin;
    if (!HC._gpuSessionId) {
        HC._gpuSessionId = sessionStorage.getItem('b3-gpu-session-id');
        if (!HC._gpuSessionId) {
            HC._gpuSessionId = String(Math.floor(Math.random() * 0x7FFFFFFF));
            sessionStorage.setItem('b3-gpu-session-id', HC._gpuSessionId);
        }
    }

    var entry = {
        workerId: workerId,
        category: category,
        capabilities: capabilities || {},
        version: version || null,
        status: 'connecting',
        connectedAt: Date.now(),
        channel: null,
        registryMissCount: 0,
    };
    HC._gpuWorkerPool[workerId] = entry;

    // Create a dedicated WS transport factory for this worker
    function _createWorkerTransport() {
        var gws = new HC.GpuWorkerWS(workerId, HC._gpuSessionId, ec2Base);
        var handle = {
            readyState: 'connecting',
            _workerId: workerId,
            _gws: gws,
            send: function(data) { gws.send(data); },
            close: function() { gws.close(); },
            onmessage: null,
            onclose: null,
        };
        gws.onopen = function() {
            handle.readyState = 'open';
            log('[GPU-Pool] Worker connected: ' + workerId.slice(0, 8) + ' (' + category + ')');
        };
        gws.onclose = function() {
            handle.readyState = 'closed';
            if (handle.onclose) handle.onclose();
        };
        gws.onmessage = function(event) {
            if (event.data instanceof ArrayBuffer) {
                if (handle.onmessage) handle.onmessage(new Uint8Array(event.data));
            } else {
                _gpuHandleMessage(event.data);
            }
        };
        return handle;
    }

    var channel = new HC.ReliableMultiTransport({
        transports: { ws: { connect: _createWorkerTransport, priority: 1 } },
        allowedTransports: 'auto',
        rtcBlockCheck: function() { return HC.audioLocked ? HC.audioLocked() : false; },
        maxBuffer: 500,
        ackInterval: 200,
        onconnect: function(name) { log('[GPU-Pool] ' + workerId.slice(0, 8) + ' transport: ' + name); },
        onmessage: function(payload) {
            var text = (typeof payload === 'string') ? payload : new TextDecoder().decode(payload);
            _gpuHandleMessage(text);
        },
    });

    channel.ontransportchange = function(active) {
        entry.status = active ? 'active' : 'connecting';
        _recomputeActiveWorker();
        // Flush pending finalize if it belongs to this worker and channel is now active
        if (active && HC._pendingChannelFinalize && HC._pendingChannelFinalizeWorkerId === workerId) {
            var msg = HC._pendingChannelFinalize;
            HC._pendingChannelFinalize = null;
            HC._pendingChannelFinalizeWorkerId = null;
            try {
                channel.send(JSON.stringify(msg));
                log('[GPU-Pool] Flushed pending finalize for stream: ' + msg.stream_id + ' worker=' + workerId.slice(0, 8));
            } catch(e) {
                log('[GPU-Pool] Failed to flush finalize: ' + e.message);
            }
        }
    };

    channel.onconnectfail = function(transport, count) {
        log('[GPU-Pool] ' + workerId.slice(0, 8) + ' ' + transport + ': ' + count + ' consecutive failures — marking failed, stopping reconnect');
        entry.status = 'failed';
        _recomputeActiveWorker();
        try { channel.destroy(); } catch(e) {}
    };

    channel._extraTelemetry = { jobs_submitted: 0, jobs_completed: 0 };
    entry.channel = channel;

    // Start telemetry only if this becomes the active worker
    _recomputeActiveWorker();
    if (HC._gpuActiveWorkerId === workerId) {
        channel.startTelemetry('gpu', function(jsonStr) {
            if (HC.ws && HC.ws.readyState === 1) HC.ws.send(jsonStr);
        });
    }

    log('[GPU-Pool] Added worker: ' + workerId.slice(0, 8) + ' (' + category + (version ? ' v' + version : '') + ')');
    return entry;
}

// Remove a worker from the pool (no longer in EC2 registry and not active).
function _poolRemoveWorker(workerId) {
    var entry = HC._gpuWorkerPool[workerId];
    if (!entry) return;
    if (entry.channel) {
        try { entry.channel.destroy ? entry.channel.destroy() : entry.channel.close ? entry.channel.close() : null; } catch(e) {}
    }
    delete HC._gpuWorkerPool[workerId];
    log('[GPU-Pool] Removed worker: ' + workerId.slice(0, 8));
    _recomputeActiveWorker();
}

// GPU routing mode — 'local+runpod' (default) or 'runpod-only'
// Persisted in HC._prefs cookie (set by layout.js). Toggle button in right-hand button stack.
HC._gpuMode = (HC._prefs && HC._prefs.gpu_mode) || 'local+runpod';

HC._updateGpuModeBtn = function() {
    var btn = document.getElementById('fs-gpu-toggle-btn');
    if (!btn) return;
    var isRunpodOnly = HC._gpuMode === 'runpod-only';
    btn.textContent = isRunpodOnly ? '☁' : '▲';
    btn.title = isRunpodOnly ? 'RunPod Only — click for Local+RunPod' : 'Local+RunPod — click for RunPod Only';
    btn.style.color = isRunpodOnly ? '#7ec8e3' : '#aaffaa';
};

HC.cycleGpuMode = function() {
    HC._gpuMode = HC._gpuMode === 'local+runpod' ? 'runpod-only' : 'local+runpod';
    if (HC.saveLayoutSetting) HC.saveLayoutSetting('gpu_mode', HC._gpuMode);
    HC._updateGpuModeBtn();
    // Recompute immediately so the active worker flips to reflect the new mode
    // without waiting for the next ontransportchange or _refreshGpuConfig.
    _recomputeActiveWorker();
    log('[GPU] Mode switched to: ' + HC._gpuMode);
};

// Sync button state once DOM is ready
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', HC._updateGpuModeBtn);
} else {
    HC._updateGpuModeBtn();
}

HC.fetchGpuConfig = async function() {
    // Fetch RunPod config + EC2-registered workers (b3-gpu-relay).
    // Uses session cookie OR Bearer token auth — no agent_id param needed (server filters by session.sub).
    try {
        var resp = await fetch(HC.EC2_BASE + '/api/gpu/config');
        if (resp.ok) {
            var data = await resp.json();
            if (data.configured) {
                HC.GPU_URL = data.gpu_url;
                HC.GPU_TOKEN = data.gpu_token;
                log('[GPU] RunPod config loaded, endpoint=' + (HC.GPU_URL || '').substring(0, 50));
            } else {
                log('[GPU] RunPod not configured on EC2');
            }
        }
    } catch(e) { log('[GPU] Config fetch failed: ' + e.message); }

    try {
        var _gpuWorkerHeaders = {};
        if (HC.config && HC.config.daemonToken) _gpuWorkerHeaders['Authorization'] = 'Bearer ' + HC.config.daemonToken;
        var wr = await fetch(HC.EC2_BASE + '/api/gpu-workers', { credentials: 'include', headers: _gpuWorkerHeaders });
        if (wr.ok) {
            var workers = await wr.json();
            // In runpod-only mode, skip dedicated local workers — only use serverless RunPod workers.
            if (HC._gpuMode === 'runpod-only') {
                workers = (workers || []).filter(function(w) {
                    return !w.capabilities || w.capabilities.category !== 'dedicated';
                });
            }
            if (workers && workers.length > 0) {
                // Populate pool with all registered workers
                workers.forEach(function(w) {
                    _poolAddWorker(w.worker_id, w.capabilities || {}, w.version || null);
                });
                // Pick up version from first worker for status bar
                if (workers[0].version) {
                    HC._gpuVersion = workers[0].version;
                    if (HC.refreshStatusBar) HC.refreshStatusBar();
                }
                log('[GPU] EC2-registered workers added to pool: ' + workers.length);
            } else {
                log('[GPU] No EC2-registered workers');
            }
        } else {
            log('[GPU] /api/gpu-workers: HTTP ' + wr.status);
        }
    } catch(e2) { log('[GPU] Worker list fetch failed: ' + e2.message); }
};

// Periodically refresh GPU config from EC2 (detects new/removed workers)
setInterval(_refreshGpuConfig, 60000);

// ── GPU Channel (ReliableMultiTransport) ────────────────────────────
// Single channel for all GPU communication. Application code calls
// HC.gpuChannel.send() — transport selection handled internally.
// Currently WS only; RTC added when merged sidecar ships (Increment 4).

HC._gpuPending = {};   // unified job_id → {onChunk, onProgress, onDone, onError, onAccepted}

// Unified message handler — called for all incoming GPU messages regardless of transport.
function _gpuHandleMessage(text) {
    var msg;
    try { msg = JSON.parse(text); } catch (e) { return; }

    if (msg.type === 'ping') {
        // Reply pong via the active channel
        var _pongChannel = (HC._gpuActiveWorkerId && HC._gpuWorkerPool[HC._gpuActiveWorkerId] && HC._gpuWorkerPool[HC._gpuActiveWorkerId].channel) || null;
        if (_pongChannel) _pongChannel.send(JSON.stringify({type: 'pong'}), HC.PRIORITY_BEST_EFFORT);
        return;
    }


    var jobId = msg.job_id;

    // Partial transcription results from streaming upload
    if (jobId && jobId.indexOf('-partial') > 0 && msg.type === 'result') {
        var streamId = jobId.replace(/-partial.*$/, '');
        log('[GPU] Partial transcription for stream ' + streamId + ': ' + (msg.output.text || '').substring(0, 40) + '...');
        if (HC._onStreamPartial) HC._onStreamPartial(streamId, msg.output);
        return;
    }

    // Retransmit requests from GPU sidecar
    if (msg.type === 'retransmit_request' && msg.stream_id && msg.missing) {
        if (HC._handleRetransmitRequest) HC._handleRetransmitRequest(msg.stream_id, msg.missing);
        return;
    }

    var pending = HC._gpuPending[jobId];
    if (!pending) return;

    if (msg.type === 'accepted' && pending.onAccepted) pending.onAccepted(msg);
    else if (msg.type === 'progress' && pending.onProgress) pending.onProgress(msg.progress);
    else if (msg.type === 'chunk' && pending.onChunk) pending.onChunk(msg);
    else if (msg.type === 'result') {
        if (pending.onResult) pending.onResult(msg.output);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuPending[jobId];
        _gpuTelemetryJobComplete();
    }
    else if (msg.type === 'done') {
        if (pending.onDone) pending.onDone(msg);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuPending[jobId];
        _gpuTelemetryJobComplete();
    }
    else if (msg.type === 'error') {
        if (pending.onError) pending.onError(msg.error);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuPending[jobId];
        _gpuTelemetryJobComplete();
    }
    else if (msg.type === 'echo') {
        if (pending.onResult) pending.onResult(msg);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuPending[jobId];
        _gpuTelemetryJobComplete();
    }
}

// Increment jobs_completed on the active channel's telemetry.
// In pool mode: active pool entry's channel. In tunnel mode: HC.gpuChannel.
function _gpuTelemetryJobComplete() {
    var activeId = HC._gpuActiveWorkerId;
    if (activeId && HC._gpuWorkerPool[activeId] && HC._gpuWorkerPool[activeId].channel) {
        var ch = HC._gpuWorkerPool[activeId].channel;
        if (ch._extraTelemetry) ch._extraTelemetry.jobs_completed++;
    } else if (HC.gpuChannel && HC.gpuChannel._extraTelemetry) {
        HC.gpuChannel._extraTelemetry.jobs_completed++;
    }
}

// Refetch EC2 worker registry — called on WS reconnect and every 60s.
// Fire-and-forget: syncs pool with EC2 worker registry.
// Adds new workers via _poolAddWorker, removes stale ones via _poolRemoveWorker.
function _refreshGpuConfig() {
    if (!HC.EC2_BASE) return;
    var hadEc2Worker = Object.keys(HC._gpuWorkerPool).length > 0;
    var _refreshWorkerHeaders = {};
    if (HC.config && HC.config.daemonToken) _refreshWorkerHeaders['Authorization'] = 'Bearer ' + HC.config.daemonToken;
    fetch(HC.EC2_BASE + '/api/gpu-workers', { credentials: 'include', headers: _refreshWorkerHeaders })
    .then(function(r) { return r.ok ? r.json() : null; })
    .then(function(workers) {
        workers = workers || [];
        // In runpod-only mode, exclude dedicated (local EC2) workers from the pool.
        if (HC._gpuMode === 'runpod-only') {
            workers = workers.filter(function(w) {
                return !(w.capabilities && w.capabilities.category === 'dedicated');
            });
        }
        // Add new workers / update existing ones
        var activeIds = {};
        workers.forEach(function(w) {
            activeIds[w.worker_id] = true;
            _poolAddWorker(w.worker_id, w.capabilities || {}, w.version || null);
            // Update version for status bar (use first worker that reports a version)
            if (w.version && !HC._gpuVersion) {
                HC._gpuVersion = w.version;
                if (HC.refreshStatusBar) HC.refreshStatusBar();
            }
        });
        // Reset registry miss counter for workers that are present this cycle.
        Object.keys(HC._gpuWorkerPool).forEach(function(wid) {
            if (activeIds[wid]) HC._gpuWorkerPool[wid].registryMissCount = 0;
        });
        // Remove workers that are no longer in the registry and are not actively routing.
        // Three eviction cases:
        // - Non-active, non-connecting (failed/unknown): evict immediately
        // - Connecting > 60s (EXITED workers that never completed handshake): evict
        // - Active but not in EC2 registry AND channel reports not connected:
        //   ReliableMultiTransport can stay 'active' while reconnect-looping,
        //   so ontransportchange(false) may not fire before the next refresh cycle.
        //   Require 2 consecutive registry misses before evicting — prevents false
        //   eviction when the channel is mid-reconnect-tick or the registry briefly
        //   returns a stale list (worker restarting, transient health check gap).
        Object.keys(HC._gpuWorkerPool).forEach(function(wid) {
            var e = HC._gpuWorkerPool[wid];
            if (activeIds[wid]) return;
            e.registryMissCount = (e.registryMissCount || 0) + 1;
            var channelInactive = e.channel && !e.channel.status().active;
            var staleActive = e.status === 'active' && channelInactive && e.registryMissCount >= 2;
            var staleConnecting = e.status === 'connecting' && e.connectedAt && (Date.now() - e.connectedAt) > 60000;
            if (staleActive || (e.status !== 'active' && (e.status !== 'connecting' || staleConnecting))) {
                log('[GPU] EC2 worker gone: ' + wid.slice(0, 8) + (staleActive ? ' (stale active, ' + e.registryMissCount + ' misses)' : staleConnecting ? ' (stale connecting)' : ''));
                _poolRemoveWorker(wid);
            }
        });
        // First time workers appear: drain queued transcriptions
        var wasEmpty = !hadEc2Worker;
        if (wasEmpty && workers.length > 0) {
            if (HC._drainTranscribeQueue) HC._drainTranscribeQueue();
        }
    })
    .catch(function() {});
}

// Create GPU WS transport adapter for ReliableMultiTransport
function _createGpuWsTransport() {
    // Snapshot EC2 state before the async refresh so the guard below is consistent.
    // _refreshGpuConfig is fire-and-forget — its /api/gpu-workers fetch can null out
    // _localWorkerId mid-flight, causing a subsequent call to pass the guard incorrectly.
    var wasEc2 = !!HC._localWorkerId;

    // Refetch GPU config on every reconnect — detects tunnel URL changes and new EC2 workers
    _refreshGpuConfig();

    // Guard: if any GPU WS is already connecting or open, return it directly.
    //
    // Two bugs fixed here:
    // 1. The old guard had `wasEc2 &&` prefix — bypassed when _localWorkerId was null
    //    at snapshot time (async _refreshGpuConfig not yet resolved). Six concurrent
    //    calls each saw wasEc2=false, bypassed the guard, and created 6 sessions.
    // 2. Returning null caused ReliableMultiTransport to schedule a reconnect (it
    //    interprets null as "connect failed") — infinite reconnect loop even when live.
    //
    // Fix: drop the wasEc2 prefix and return the existing handle so ReliableMultiTransport
    // sees readyState 'open'/'connecting' and stops scheduling further reconnects.
    if (HC._gpuWs && (HC._gpuWs.readyState === 'connecting' || HC._gpuWs.readyState === 'open')) {
        // Guard against stale handles after a worker switch: if the handle's worker ID no
        // longer matches HC._localWorkerId, close the stale connection and fall through to
        // create a new one. This closes the race window where _disconnectTransport fires
        // gws.close() but ECDH completes first and onopen re-sets the handle to the old
        // worker before the close takes effect.
        if (HC._localWorkerId && HC._gpuWs._workerId && HC._gpuWs._workerId !== HC._localWorkerId) {
            log('[GPU-WS] Worker changed — closing stale connection to ' + HC._gpuWs._workerId + ', reconnecting to ' + HC._localWorkerId);
            try { if (HC._gpuWs._gws) HC._gpuWs._gws.close(); } catch(e) {}
            HC._gpuWs = null;
        } else {
            log('[GPU-WS] GPU WS already connecting/READY — returning existing handle');
            return HC._gpuWs;
        }
    }

    // Stable session ID for reconnect — used on both EC2-proxy and tunnel paths
    if (!HC._gpuSessionId) {
        HC._gpuSessionId = sessionStorage.getItem('b3-gpu-session-id');
        if (!HC._gpuSessionId) {
            HC._gpuSessionId = String(Math.floor(Math.random() * 0x7FFFFFFF));
            sessionStorage.setItem('b3-gpu-session-id', HC._gpuSessionId);
        }
    }

    if (HC._localWorkerId) {
        // EC2-registered worker path — E2E encrypted via GpuWorkerWS (ECDH + AES-256-GCM).
        if (!HC.GpuWorkerWS) {
            log('[GPU-WS] GpuWorkerWS not loaded — cannot connect to EC2 worker');
            return null;
        }
        var ec2Base = HC.EC2_BASE || window.location.origin;
        // Snapshot worker ID at connection time — _refreshGpuConfig() runs fire-and-forget
        // and may update HC._localWorkerId before onopen fires, causing stale log output.
        var connectedWorkerId = HC._localWorkerId;
        var gws = new HC.GpuWorkerWS(connectedWorkerId, HC._gpuSessionId, ec2Base);

        var handle = {
            readyState: 'connecting',
            _workerId: connectedWorkerId, // for stale-handle detection in guard above
            _gws: gws,                   // for explicit close in guard above
            send: function(data) { gws.send(data); },
            close: function() { gws.close(); },
            onmessage: null,
            onclose: null,
        };
        HC._gpuWs = handle;

        gws.onopen = function() {
            handle.readyState = 'open';
            log('[GPU-WS] EC2 encrypted connected (worker=' + connectedWorkerId + ')');
        };
        gws.onclose = function() {
            handle.readyState = 'closed';
            HC._gpuWs = null;
            if (handle.onclose) handle.onclose();
        };
        gws.onmessage = function(event) {
            if (event.data instanceof ArrayBuffer) {
                if (handle.onmessage) handle.onmessage(new Uint8Array(event.data));
            } else {
                // Text control frames from relay (JSON)
                _gpuHandleMessage(event.data);
            }
        };

        log('[GPU-WS] EC2 worker path: ECDH handshake → worker=' + connectedWorkerId);
        return handle;
    }

    // No EC2 worker registered — no transport available
    return null;
}

// Stale detection removed — reconnection handled exclusively by the
// ReliableMultiTransport layer (exponential backoff on onclose).

// Submit a job via the GPU channel. Returns a controller with callback hooks.
// Routes via active pool entry (EC2 workers).
HC.gpuSubmit = function(input) {
    var channel = null;
    var workerLabel = null;
    var activeId = HC._gpuActiveWorkerId;
    if (activeId && HC._gpuWorkerPool[activeId]) {
        var poolEntry = HC._gpuWorkerPool[activeId];
        if (poolEntry.status === 'active' && poolEntry.channel) {
            channel = poolEntry.channel;
            workerLabel = activeId.slice(0, 8);
        }
    }
    if (!channel) {
        return null;
    }

    var jobId = (crypto.randomUUID ? crypto.randomUUID() : (Date.now().toString(36) + Math.random().toString(36).slice(2))).substring(0, 12);
    // Capture channel reference at submit time — active worker may change mid-flight
    var submitChannel = channel;
    var controller = {
        jobId: jobId,
        backend: 'dedicated-gpu',
        onAccepted: null,
        onChunk: null,
        onProgress: null,
        onResult: null,
        onDone: null,
        onError: null,
        onBinaryChunk: null,
        cancel: function() {
            submitChannel.send(JSON.stringify({type: 'cancel', job_id: jobId}));
            if (controller._expireTimer) clearTimeout(controller._expireTimer);
            delete HC._gpuPending[jobId];
        }
    };
    HC._gpuPending[jobId] = controller;

    // Expire pending job after 90s
    controller._expireTimer = setTimeout(function() {
        if (HC._gpuPending[jobId]) {
            log('[GPU] Job expired (90s timeout): ' + jobId);
            if (controller.onError) controller.onError('Job timed out');
            delete HC._gpuPending[jobId];
        }
    }, 90000);

    try {
        submitChannel.send(JSON.stringify({type: 'submit', job_id: jobId, input: input}));
        if (submitChannel._extraTelemetry) submitChannel._extraTelemetry.jobs_submitted++;
        log('[GPU] Job submitted: ' + jobId + ' action=' + (input.action || '?') + ' worker=' + workerLabel);
    } catch(e) {
        clearTimeout(controller._expireTimer);
        delete HC._gpuPending[jobId];
        log('[GPU] Submit failed: ' + e.message);
        return null;
    }
    return controller;
};

// ── Legacy compatibility ────────────────────────────────────────────
// gpuWsSubmit and gpuWsConnect delegate to the new channel
HC._gpuWsPending = HC._gpuPending;  // alias for any remaining references
HC._gpuRtcPending = HC._gpuPending; // alias for any remaining references
HC._gpuWsReady = false;
HC._gpuWs = null;

HC.gpuWsConnect = function() {
    // Legacy: now handled by HC._initGpuChannel
    if (HC.gpuChannel) return;
    HC._initGpuChannel();
};

HC.gpuWsSubmit = function(input) {
    return HC.gpuSubmit(input);
};

// ── RunPod WebSocket Discovery + Connection ──────────────────────────
// Extract WS coordinates from any RunPod stream/status response.
// Called during HTTP polling — if coords found, upgrade to WebSocket mid-job.

// Extract RunPod WS coordinates from stream/status data.
// Checks for:
//   1) progress_update with ws_ip/ws_port (legacy RunPod workers)
//   2) progress_update with worker_id (EC2-registered b3-gpu-relay workers)
//   3) workerId at top level (RunPod status response)
HC.extractRunpodWsCoords = function(data) {
    var sources = [];
    if (data.stream && Array.isArray(data.stream)) sources = sources.concat(data.stream);
    if (data.output && Array.isArray(data.output)) sources = sources.concat(data.output);
    for (var i = 0; i < sources.length; i++) {
        var ev = sources[i].output || sources[i];
        if (ev && ev.ws_ip && ev.ws_port) {
            return { ip: ev.ws_ip, port: parseInt(ev.ws_port), podId: ev.pod_id || '' };
        }
        // EC2-registered b3-gpu-relay worker: handler.py yields worker_id instead of ws_ip/ws_port
        if (ev && ev.worker_id) {
            return { workerId: ev.worker_id, ip: '', port: 0, podId: ev.pod_id || '' };
        }
    }
    // Check workerId at top level of status response (set when worker picks up job)
    if (data.workerId) {
        return { ip: '', port: 5125, podId: data.workerId };
    }
    return null;
};



// Shared RunPod wake+connect helper.
// Phase 1: POST /run with action='wake' (no GPU work, just yields WS coords).
// Phase 2: poll /stream until ws_ip/ws_port appear.
// Phase 3: _poolAddWorker + wait for ontransportchange active (8s timeout).
// Returns { workerId, jobId } on success, null on timeout/failure/cancellation.
// Callers use jobId to cancel the wake probe if it's no longer needed.
// isCancelled: function() → bool. gpuRun() passes function(){return resolved;}.
//              gpuWarmRunpod() passes a check for any active pool worker.
// onJobId: optional callback(jobId) fired immediately after Phase 1 — lets the
//          caller track the probe for cancellation before the helper returns.
async function _runpodWakeAndConnect(agentId, isCancelled, onJobId) {
    try {
        var rpHeaders = { 'Content-Type': 'application/json' };
        if (HC.GPU_TOKEN) rpHeaders['Authorization'] = HC.GPU_TOKEN;
        var runUrl = HC.GPU_URL.replace('/runsync', '/run');
        // Phase 1: wake probe
        var r = await fetch(runUrl, { method: 'POST', headers: rpHeaders,
            body: JSON.stringify({ input: { action: 'wake', agent_id: agentId || '' } }) });
        if (isCancelled()) return null;
        if (!r.ok) { log('[GPU] RunPod wake failed: HTTP ' + r.status); return null; }
        var data = await r.json();
        var rpJobId = data.id;
        log('[GPU] RunPod wake probe submitted: ' + rpJobId);
        if (onJobId) onJobId(rpJobId);
        // Phase 2: poll /stream until ws_ip/ws_port (first output from handler)
        var streamUrl = HC.GPU_URL.replace('/runsync', '/stream/' + rpJobId);
        var statusUrl = HC.GPU_URL.replace('/runsync', '/status/' + rpJobId);
        var coords = null;
        for (var pi = 0; pi < 240; pi++) {
            if (isCancelled()) return null;
            await new Promise(function(res) { setTimeout(res, 500); });
            if (isCancelled()) return null;
            try {
                var sr = await fetch(streamUrl, { headers: rpHeaders });
                if (sr.ok) {
                    var sd = await sr.json();
                    coords = HC.extractRunpodWsCoords(sd);
                    if (coords && (coords.ip || coords.workerId)) break;
                    // /stream may not include workerId — fetch /status once when stream has output
                    if (!coords && (sd.stream || []).length > 0) {
                        try {
                            var sr2 = await fetch(statusUrl, { headers: rpHeaders });
                            if (sr2.ok) coords = HC.extractRunpodWsCoords(await sr2.json());
                            if (coords && (coords.ip || coords.workerId)) break;
                        } catch(e2) {}
                    }
                }
            } catch(e) {}
        }
        if (!coords || (!coords.ip && !coords.workerId)) {
            log('[GPU] RunPod wake: no WS coords after polling');
            return null;
        }
        if (isCancelled()) return null;
        if (!coords.workerId) {
            // Legacy ip:port path — no pool, can't pre-warm via pool
            log('[GPU] RunPod wake: legacy ip:port coords, no pool entry created');
            return null;
        }
        // Phase 3: add to pool and wait for active
        log('[GPU] RunPod WS coords: worker=' + coords.workerId + ' — connecting');
        var targetWorkerId = coords.workerId;
        var existingEntry = HC._gpuWorkerPool[targetWorkerId];
        var alreadyActive = existingEntry && existingEntry.status === 'active';
        if (!alreadyActive) {
            var rpCaps = { category: 'serverless' };
            _poolAddWorker(targetWorkerId, rpCaps, null);
            log('[GPU] RunPod: added worker=' + targetWorkerId.slice(0, 8) + ' to pool');
        } else {
            log('[GPU] RunPod: reusing active pool entry for worker=' + targetWorkerId.slice(0, 8));
        }
        if (isCancelled()) return null;
        var channelReady = await new Promise(function(resolve) {
            var entry = HC._gpuWorkerPool[targetWorkerId];
            if (!entry) { resolve(false); return; }
            if (entry.status === 'active') { resolve(true); return; }
            var timer = setTimeout(function() { resolve(false); }, 8000);
            var prevOntransportchange = entry.channel && entry.channel.ontransportchange;
            if (entry.channel) {
                entry.channel.ontransportchange = function(active) {
                    if (prevOntransportchange) prevOntransportchange(active);
                    if (active) {
                        clearTimeout(timer);
                        if (entry.channel) entry.channel.ontransportchange = prevOntransportchange;
                        resolve(true);
                    }
                };
            } else {
                resolve(false);
            }
        });
        if (!channelReady || isCancelled()) {
            if (!channelReady) log('[GPU] RunPod: pool entry did not become active for worker=' + targetWorkerId.slice(0, 8));
            return null;
        }
        return { workerId: targetWorkerId, jobId: rpJobId };
    } catch(e) {
        log('[GPU] RunPod wake+connect failed: ' + e.message);
        return null;
    }
}

// Pre-warm a RunPod worker when speech is detected and no pool worker is active.
// Idempotent: no-op if any pool worker is already active or no GPU_URL configured.
// Fire-and-forget from voice-record.js — result is pool readiness when job arrives.
HC.gpuWarmRunpod = function() {
    if (!HC.GPU_URL) return;
    var anyActive = Object.keys(HC._gpuWorkerPool).some(function(wid) {
        var e = HC._gpuWorkerPool[wid];
        if (HC._gpuMode === 'runpod-only' && e.category === 'dedicated') return false;
        return e.status === 'active';
    });
    if (anyActive) return;
    var agentId = (HC.config && HC.config.agentId) ? HC.config.agentId : '';
    log('[GPU] gpuWarmRunpod: warming RunPod worker (no active pool entry)');
    _runpodWakeAndConnect(agentId, function() {
        return Object.keys(HC._gpuWorkerPool).some(function(wid) {
            var e = HC._gpuWorkerPool[wid];
            if (HC._gpuMode === 'runpod-only' && e.category === 'dedicated') return false;
            return e.status === 'active';
        });
    });
};

HC.gpuRun = async function(input) {
    // Inject agent_id for billing — GPU worker uses this for credit checks
    if (HC.config && HC.config.agentId && !input.agent_id) {
        input.agent_id = HC.config.agentId;
    }
    // Race design: fire local GPU immediately. After 500ms head start,
    // fire RunPod too. First healthy response wins. Cancel the loser.

    var LOCAL_HEAD_START = 500; // ms — local gets a head start before RunPod joins
    var resolved = false;

    return new Promise(function(resolve, reject) {
        var rpJobId = null; // Track RunPod job for cancellation

        function win(result) {
            if (resolved) return;
            resolved = true;
            // Cancel the other backend's job if it was submitted
            if (result.backend === 'dedicated-gpu' && rpJobId && HC.GPU_URL) {
                var cancelUrl = HC.GPU_URL.replace('/runsync', '/cancel/' + rpJobId);
                fetch(cancelUrl, { method: 'POST', headers: HC.gpuPollHeaders(HC.GPU_TOKEN) }).catch(function() {});
                log('[GPU] Cancelled RunPod job ' + rpJobId + ' (dedicated-gpu won race)');
            }
            resolve(result);
        }

        // --- Local GPU lane (via pool channel) ---
        // Skipped in 'runpod-only' mode — all jobs route to RunPod directly.
        if (HC._localWorkerId && HC._gpuMode !== 'runpod-only') {
            (async function localLane() {
                // gpuSubmit() returns null during the _handles registration gap: the WS
                // transport exists but readyState isn't 'open' yet so _onTransportOpen
                // hasn't fired and the handle isn't in _handles. status().active returns
                // null → gpuSubmit returns null. Two short retries (300ms + 500ms) poll
                // until the handle registers and readyState flips open.
                if (HC._gpuActiveWorkerId) {
                    var ctrl = HC.gpuSubmit(input);
                    if (!ctrl && !resolved) {
                        await new Promise(function(r) { setTimeout(r, 300); });
                        ctrl = HC.gpuSubmit(input);
                    }
                    if (!ctrl && !resolved) {
                        await new Promise(function(r) { setTimeout(r, 500); });
                        ctrl = HC.gpuSubmit(input);
                    }
                    if (ctrl) {
                        win({ data: { id: ctrl.jobId }, backend: 'dedicated-gpu', wsController: ctrl });
                        return;
                    }
                }
            })();
        }

        // --- RunPod lane (wake probe → WebSocket) ---
        // Phase 1: submit a wake probe (action='wake') — no GPU work, just yields WS coords.
        // Phase 2: poll /stream until ws_ip/ws_port appear (typically <1s once worker is live).
        // Phase 3: open WebSocket to worker via EC2 proxy, submit real job.
        // This avoids the double-submit bug where HTTP /run held the GPU while a second
        // job came in via WS, causing the WS job to starve.
        // In 'runpod-only' mode: no delay (local lane skipped, RunPod fires immediately).
        if (HC.GPU_URL) {
            var _rpDelay = (HC._gpuMode === 'runpod-only') ? 0 : LOCAL_HEAD_START;
            setTimeout(function() {
                if (resolved) return; // Local already won
                (async function runpodLane() {
                    try {
                        // Fast path: if a pool worker is already active, skip the wake probe
                        // entirely and submit directly. In runpod-only mode this avoids a
                        // wake probe round-trip on every TTS call when a warm worker exists.
                        var activePoolWorker = (function() {
                            var wids = Object.keys(HC._gpuWorkerPool);
                            for (var i = 0; i < wids.length; i++) {
                                var e = HC._gpuWorkerPool[wids[i]];
                                if (e.status === 'active' && e.category === 'serverless') return wids[i];
                            }
                            return null;
                        })();
                        if (activePoolWorker && !resolved) {
                            HC._gpuActiveWorkerId = activePoolWorker;
                            HC._localWorkerId = activePoolWorker;
                            var poolCtrl = HC.gpuSubmit(input);
                            _recomputeActiveWorker();
                            if (poolCtrl) {
                                log('[GPU] RunPod: reusing active pool worker=' + activePoolWorker.slice(0, 8) + ' (skipped wake probe)');
                                win({ data: { id: poolCtrl.jobId }, backend: 'serverless-gpu',
                                      wsController: poolCtrl,
                                      baseUrl: HC.GPU_URL, token: HC.GPU_TOKEN || '' });
                                return;
                            }
                            log('[GPU] RunPod: pool worker active but gpuSubmit failed, falling through to wake probe');
                        }

                        // Phase 1-3: wake probe, poll, pool-add — extracted to shared helper.
                        // gpuRun() passes resolved as isCancelled so helper aborts if local wins.
                        // onJobId sets rpJobId immediately after Phase 1 so win() can cancel the
                        // probe if local GPU wins the race while the poll loop is still running.
                        var rpResult = await _runpodWakeAndConnect(
                            input.agent_id || '',
                            function() { return resolved; },
                            function(jobId) { rpJobId = jobId; }
                        );

                        var ctrl;
                        if (rpResult) {
                            var targetWorkerId = rpResult.workerId;
                            // Route this job to the RunPod worker by temporarily pointing the
                            // active worker at it. Restore via _recomputeActiveWorker immediately
                            // after submit so concurrent jobs (e.g. TTS) don't route to RunPod.
                            if (resolved) return;
                            HC._gpuActiveWorkerId = targetWorkerId;
                            HC._localWorkerId = targetWorkerId;
                            ctrl = HC.gpuSubmit(input);
                            _recomputeActiveWorker(); // restore correct priority (dedicated > serverless)
                            if (!ctrl) {
                                log('[GPU] RunPod: gpuSubmit failed for worker=' + targetWorkerId.slice(0, 8));
                                return;
                            }
                        } else {
                            // Helper returned null: timeout, cancelled, or legacy ip:port coords.
                            if (!resolved) log('[GPU] RunPod: wake+connect returned null, skipping submit');
                            return;
                        }

                        if (!ctrl || resolved) return;
                        // Cancel the wake probe — it's already completed but clean up RunPod queue
                        var cancelHeaders = { 'Content-Type': 'application/json' };
                        if (HC.GPU_TOKEN) cancelHeaders['Authorization'] = HC.GPU_TOKEN;
                        fetch(HC.GPU_URL.replace('/runsync', '/cancel/' + rpJobId),
                            { method: 'POST', headers: cancelHeaders }).catch(function() {});
                        rpJobId = null; // Don't cancel again in win() if local wins after this
                        // For the pool path, pass the gpuSubmit controller as wsController so
                        // voice-record.js uses push-based WS callbacks instead of HTTP polling.
                        win({ data: { id: ctrl.jobId }, backend: 'serverless-gpu',
                              wsController: ctrl,
                              baseUrl: HC.GPU_URL, token: HC.GPU_TOKEN || '' });
                    } catch(e) {
                        if (resolved) return;
                        log('[GPU] RunPod failed: ' + e.message);
                    }
                })();
            }, _rpDelay);
        }

        // --- Safety net: if neither responds in 120s, reject ---
        // RunPod cold start can take 60-120s. Local GPU via tunnel is fast
        // but network issues can cause delays. Be generous here — the race
        // ensures we get the fastest result; the timeout is only for total failure.
        setTimeout(function() {
            if (!resolved) {
                resolved = true;
                reject(new Error('No GPU backend responded within 120s'));
            }
        }, 120000);

        // --- If no backends configured, reject immediately ---
        if (!HC._localWorkerId && !HC.GPU_URL) {
            reject(new Error('No GPU backend available'));
        }
    });
};

HC.gpuStreamUrl = function(baseUrl, jobId) {
    if (baseUrl.includes('/runsync')) return baseUrl.replace('/runsync', '/stream/' + jobId);
    return baseUrl + '/stream/' + jobId;
};

HC.gpuStatusUrl = function(baseUrl, jobId) {
    if (baseUrl.includes('/runsync')) return baseUrl.replace('/runsync', '/status/' + jobId);
    return baseUrl + '/status/' + jobId;
};

HC.gpuPollHeaders = function(token) {
    var h = {};
    if (token) h['Authorization'] = token;
    return h;
};

// ── GPU Job Timer + State Tracking (Stage 1 Marketplace) ──
// Count-up timer shown in toast during STT/TTS jobs.
// GPU worker reports its state (warm/cold/queued) in the first stream event.

HC._gpuJobTimers = {};  // jobId → { interval, t0, state, workerId, jobType }
var _GPU_TIMER_MAX_MS = 120000;  // Safety net: kill orphaned timers after 2 minutes

HC.gpuJobStart = function(jobId, jobType) {
    // Kill any existing timers for this job type to prevent orphans
    // (e.g., local GPU fails → falls back to RunPod with new job ID)
    HC._gpuJobEndAll(jobType);
    var t0 = performance.now();
    var timer = { t0: t0, state: null, workerId: null, jobType: jobType, interval: null, _countSec: 0 };
    timer.interval = setInterval(function() {
        timer._countSec = Math.round((performance.now() - t0) / 1000);
        // Safety net: kill orphaned timers
        if ((performance.now() - t0) > _GPU_TIMER_MAX_MS) {
            log('[GPU] Timer orphan killed: ' + jobId + ' after ' + timer._countSec + 's');
            HC.gpuJobEnd(jobId, 'orphan_timeout');
            return;
        }
        var label = jobType === 'transcribe' ? 'Transcribing' : 'Generating audio';
        var suffix = '';
        if (timer.state === 'cold') suffix = ' (GPU warming up)';
        else if (timer.state === 'queued') suffix = ' (queued' + (timer.queuePos ? ' #' + timer.queuePos : '') + ')';
        if (timer._countSec > 0) {
            HC.setVoiceStatus('sending', label + '... ' + timer._countSec + 's' + suffix);
        }
    }, 1000);
    HC._gpuJobTimers[jobId] = timer;
    return timer;
};

// Kill all timers for a given job type (used when retrying or falling back)
HC._gpuJobEndAll = function(jobType) {
    var ids = Object.keys(HC._gpuJobTimers);
    for (var i = 0; i < ids.length; i++) {
        var timer = HC._gpuJobTimers[ids[i]];
        if (timer && timer.jobType === jobType) {
            if (timer.interval) clearInterval(timer.interval);
            delete HC._gpuJobTimers[ids[i]];
            log('[GPU] Cleared stale timer: ' + ids[i] + ' (' + jobType + ')');
        }
    }
};

HC.gpuJobAccepted = function(jobId, state, workerId, queuePos) {
    var timer = HC._gpuJobTimers[jobId];
    if (!timer) return;
    timer.state = state;
    timer.workerId = workerId || null;
    timer.queuePos = queuePos || null;
    log('[GPU] Job ' + jobId + ' accepted: state=' + state + ' worker=' + (workerId || 'unknown'));
};

HC.gpuJobEnd = function(jobId, outcome) {
    var timer = HC._gpuJobTimers[jobId];
    if (!timer) return null;
    if (timer.interval) clearInterval(timer.interval);
    var elapsed = Math.round(performance.now() - timer.t0);
    var stats = {
        job_type: timer.jobType,
        gpu_worker_id: timer.workerId || 'unknown',
        initial_state: timer.state || 'unknown',
        round_trip_ms: elapsed,
        outcome: outcome || 'success',
    };
    delete HC._gpuJobTimers[jobId];
    // Fire-and-forget stats report to EC2
    HC.reportGpuStats(stats);
    return stats;
};

HC.reportGpuStats = function(stats) {
    if (!HC.EC2_BASE) return;
    var agentId = (HC.config && HC.config.agentId) || '';
    var body = Object.assign({}, stats, { agent_id: agentId });
    try {
        fetch(HC.EC2_BASE + '/api/gpu-stats', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
        }).catch(function(e) { log('[GPU-Stats] Report failed: ' + e.message); });
    } catch(e) { /* non-blocking */ }
};

HC.fetchGarbageEmbeddings = async function() {
    try {
        var resp = await fetch(HC.EC2_BASE + '/api/garbage/embeddings');
        if (!resp.ok) return;
        var data = await resp.json();
        HC._garbageEntries = (data.entries || []).filter(function(e) { return e.embedding && e.embedding.length > 0; });
        HC._garbageThreshold = data.threshold || 0.88;
        log('[GARBAGE] Loaded ' + HC._garbageEntries.length + ' entries (threshold=' + HC._garbageThreshold + ')');
    } catch(e) { log('[GARBAGE] Fetch failed (non-blocking): ' + e.message); }
};

HC.fetchVoiceConditionals = async function(voiceName) {
    var cacheKey = 'voice-cond-' + voiceName;
    var cached = sessionStorage.getItem(cacheKey);
    if (cached) return cached;
    try {
        var resp = await fetch(HC.EC2_BASE + '/api/default-voices/' + encodeURIComponent(voiceName));
        if (resp.ok) {
            var data = await resp.json();
            if (data.conditionals_b64) {
                sessionStorage.setItem(cacheKey, data.conditionals_b64);
                return data.conditionals_b64;
            }
        }
        var userResp = await fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/user-voices/' + encodeURIComponent(voiceName), {
            headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
        });
        if (userResp.ok) {
            var data2 = await userResp.json();
            if (data2.conditionals_b64) {
                sessionStorage.setItem(cacheKey, data2.conditionals_b64);
                return data2.conditionals_b64;
            }
        }
    } catch(e) { log('[Voice] Failed to fetch conditionals for ' + voiceName + ': ' + e.message); }
    return '';
};

// ── WebRTC Data Channel to GPU Worker ───────────────────────────────
// Replaces WebSocket/HTTP with peer-to-peer data channel.
// Falls back to WebSocket if WebRTC fails.

HC._gpuRtc = null;       // RTCPeerConnection
HC._gpuRtcDc = null;     // RTCDataChannel
HC._gpuRtcReady = false;
HC._gpuRtcSessionId = null;
HC._gpuRtcBackoff = 2000;
HC._gpuRtcTimer = null;
HC._gpuRtcPending = {};  // job_id -> {onChunk, onProgress, onDone, onError, onAccepted}
var _gpuRtcIceRestartCount = 0;
var _dcChunks = null;  // chunk reassembly state for incoming chunked messages
var _GPU_RTC_MAX_ICE_RESTARTS = 3;

HC.gpuRtcConnect = async function() {
    if (HC._gpuRtcReady || HC._gpuRtc) return;
    if (!HC.EC2_BASE) return;
    if (!HC.rtcEnabled()) return; // WS-only mode — skip RTC

    try {
        // 1. Fetch TURN credentials
        var credResp = await fetch(HC.EC2_BASE + '/api/webrtc/turn-credentials', {
            headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
        });
        if (!credResp.ok) { log('[GPU-RTC] TURN credentials failed: ' + credResp.status); _gpuRtcScheduleReconnect(); return; }
        var creds = await credResp.json();

        // 2. Create RTCPeerConnection
        var pc = new RTCPeerConnection({ iceServers: creds.ice_servers });
        HC._gpuRtc = pc;

        // 3. Create data channel
        var dc = pc.createDataChannel('gpu', { ordered: true });
        dc.binaryType = 'arraybuffer';
        HC._gpuRtcDc = dc;

        dc.onopen = function() {
            HC._gpuRtcReady = true;
            HC._gpuRtcBackoff = 2000;
            _gpuRtcIceRestartCount = 0;
            HC._gpuRtcLastPong = Date.now(); // connection just opened = alive
            log('[GPU-RTC] Data channel open — P2P to GPU worker');

            // Resubmit any stream that was interrupted by a connection drop.
            // All chunks were retained in _streamSentChunks — resend the entire stream.
            if (HC._pendingStreamResubmit) {
                var pending = HC._pendingStreamResubmit;
                HC._pendingStreamResubmit = null;
                log('[GPU-RTC] Resubmitting stream ' + pending.streamId + ' (' + pending.chunkCount + ' chunks)');
                try {
                    // Start new stream with same ID
                    _gpuRtcSendJson({ type: 'stream_start', stream_id: pending.streamId });
                    // Resend all chunks in order
                    for (var i = 0; i < pending.chunkCount; i++) {
                        var b64 = pending.chunks[i];
                        if (b64) {
                            _gpuRtcSendJson({
                                type: 'stream_chunk',
                                stream_id: pending.streamId,
                                chunk_index: i,
                                audio_b64: b64,
                            });
                        }
                    }
                    log('[GPU-RTC] Resubmitted ' + pending.chunkCount + ' chunks for ' + pending.streamId);
                    // If recording already stopped, send finalize too
                    if (pending.finalized && pending.finalizeData) {
                        _gpuRtcSendJson(pending.finalizeData);
                        log('[GPU-RTC] Sent deferred finalize for ' + pending.streamId);
                    }
                } catch(e) {
                    log('[GPU-RTC] Resubmit failed: ' + e.message);
                }
            }

            // Resubmit active stream that had chunks buffered during connection drop
            if (HC._streamNeedsResubmit && !HC._pendingStreamResubmit) {
                var sid = HC._streamNeedsResubmit;
                var chunks = window._streamSentChunks ? window._streamSentChunks[sid] : null;
                if (chunks) {
                    var count = HC._streamResubmitChunkCount || 0;
                    log('[GPU-RTC] Resubmitting active stream ' + sid + ' (' + count + ' buffered chunks)');
                    try {
                        _gpuRtcSendJson({ type: 'stream_start', stream_id: sid, mime_type: 'audio/webm' });
                        for (var j = 0; j < count; j++) {
                            if (chunks[j]) {
                                _gpuRtcSendJson({
                                    type: 'stream_chunk',
                                    stream_id: sid,
                                    chunk_index: j,
                                    audio_b64: chunks[j],
                                });
                            }
                        }
                        log('[GPU-RTC] Active stream resubmitted — ' + count + ' chunks');
                        if (typeof setVoiceStatus === 'function') setVoiceStatus('recording', '\ud83c\udfa4 Recording...');
                    } catch(e) {
                        log('[GPU-RTC] Active stream resubmit failed: ' + e.message);
                    }
                }
                HC._streamNeedsResubmit = null;
                HC._streamResubmitChunkCount = 0;
            }

            // Lightweight liveness ping every 10s (5 bytes, not a keepalive — just for freshness detection)
            if (HC._gpuRtcPingTimer) clearInterval(HC._gpuRtcPingTimer);
            HC._gpuRtcPingTimer = setInterval(function() {
                if (dc.readyState === 'open') {
                    try {
                        var ping = new Uint8Array(5);
                        ping[0] = 0x03; // TYPE_PING
                        dc.send(ping.buffer);
                    } catch(e) {}
                } else {
                    clearInterval(HC._gpuRtcPingTimer);
                }
            }, 10000);
        };

        dc.onclose = function() {
            log('[GPU-RTC] Data channel closed');
            _gpuRtcCleanup();
            _gpuRtcScheduleReconnect();
        };

        // Chunk reassembly uses module-scope _dcChunks (reset in _gpuRtcCleanup)

        dc.onmessage = function(event) {
            // Messages are framed: [type:u8][length:u32LE][payload]
            var data = event.data;
            if (typeof data === 'string') {
                _gpuRtcHandleJson(data);
                return;
            }
            var view = new DataView(data);
            if (data.byteLength < 5) return;
            var type = view.getUint8(0);
            var len = view.getUint32(1, true);
            var payload = data.slice(5, 5 + len);

            if (type === 0x01) {
                var text = new TextDecoder().decode(payload);
                _gpuRtcHandleJson(text);
            } else if (type === 0x02) {
                _gpuRtcHandleBinary(payload);
            } else if (type === 0x03) {
                var pong = new Uint8Array(5);
                pong[0] = 0x04;
                dc.send(pong.buffer);
            } else if (type === 0x04) {
                // Pong — update liveness timestamp
                HC._gpuRtcLastPong = Date.now();
            } else if (type === 0x05) {
                // Chunked message — reassemble
                var chunkView = new DataView(payload);
                if (payload.byteLength < 9) return;
                var seq = chunkView.getUint32(0, true);
                var total = chunkView.getUint32(4, true);
                var originalType = chunkView.getUint8(8);
                var chunkData = payload.slice(9);

                if (!_dcChunks || _dcChunks.total !== total) {
                    _dcChunks = { total: total, originalType: originalType, parts: new Array(total), received: 0 };
                }
                if (!_dcChunks.parts[seq]) {
                    _dcChunks.parts[seq] = chunkData;
                    _dcChunks.received++;
                }
                if (_dcChunks.received >= _dcChunks.total) {
                    // All chunks received — reassemble
                    var totalLen = 0;
                    for (var i = 0; i < _dcChunks.parts.length; i++) totalLen += _dcChunks.parts[i].byteLength;
                    var assembled = new Uint8Array(totalLen);
                    var offset = 0;
                    for (var i = 0; i < _dcChunks.parts.length; i++) {
                        assembled.set(new Uint8Array(_dcChunks.parts[i]), offset);
                        offset += _dcChunks.parts[i].byteLength;
                    }
                    var ot = _dcChunks.originalType;
                    _dcChunks = null;
                    if (ot === 0x01) {
                        _gpuRtcHandleJson(new TextDecoder().decode(assembled));
                    } else if (ot === 0x02) {
                        _gpuRtcHandleBinary(assembled.buffer);
                    }
                }
            }
        };

        // 4. ICE candidate handling
        pc.onicecandidate = function(event) {
            if (event.candidate && HC._gpuRtcSessionId) {
                fetch(HC.EC2_BASE + '/api/gpu/ice', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        session_id: HC._gpuRtcSessionId,
                        candidate: event.candidate.candidate,
                        mid: event.candidate.sdpMid || '',
                        source: 'browser',
                    }),
                }).catch(function(e) { log('[GPU-RTC] ICE candidate send failed: ' + e.message); });
            }
        };

        // 5. Connection state monitoring
        pc.onconnectionstatechange = function() {
            log('[GPU-RTC] Connection state: ' + pc.connectionState);
            if (pc.connectionState === 'connected') {
                _gpuRtcIceRestartCount = 0; // Reset on success
            } else if (pc.connectionState === 'failed') {
                _gpuRtcIceRestartCount++;
                if (_gpuRtcIceRestartCount > _GPU_RTC_MAX_ICE_RESTARTS) {
                    log('[GPU-RTC] ICE restart limit reached (' + _gpuRtcIceRestartCount + ') — creating fresh connection');
                    _gpuRtcCleanup();
                    _gpuRtcScheduleReconnect();
                } else {
                    log('[GPU-RTC] Connection failed, ICE restart ' + _gpuRtcIceRestartCount + '/' + _GPU_RTC_MAX_ICE_RESTARTS);
                    _gpuRtcIceRestart(pc);
                }
            } else if (pc.connectionState === 'disconnected') {
                // On cellular, IP changes make the old connection unrecoverable.
                // Skip ICE restarts (they rarely work on cellular) — create fresh
                // connection after 1 second. This reduces reconnect from ~15s to ~3s.
                setTimeout(function() {
                    if (pc.connectionState === 'disconnected') {
                        log('[GPU-RTC] Disconnected for 1s — creating fresh connection');
                        _gpuRtcCleanup();
                        HC.gpuRtcConnect();
                    }
                }, 1000);
            }
        };

        // 6. Create offer and signal via EC2
        var offer = await pc.createOffer();
        await pc.setLocalDescription(offer);

        // Wait for ICE gathering to complete (simpler for v1)
        await new Promise(function(resolve) {
            if (pc.iceGatheringState === 'complete') { resolve(); return; }
            var check = function() {
                if (pc.iceGatheringState === 'complete') {
                    pc.removeEventListener('icegatheringstatechange', check);
                    resolve();
                }
            };
            pc.addEventListener('icegatheringstatechange', check);
            // Safety timeout — don't block forever
            setTimeout(resolve, 5000);
        });

        // 7. Send offer to EC2 signaling
        var offerResp = await fetch(HC.EC2_BASE + '/api/gpu/offer', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ sdp: pc.localDescription.sdp }),
            signal: AbortSignal.timeout(15000),
        });

        if (!offerResp.ok) {
            log('[GPU-RTC] Offer rejected: ' + offerResp.status);
            _gpuRtcCleanup();
            _gpuRtcScheduleReconnect();
            return;
        }

        var answer = await offerResp.json();
        HC._gpuRtcSessionId = answer.session_id;

        await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
        log('[GPU-RTC] Remote description set, waiting for data channel open...');

    } catch (e) {
        log('[GPU-RTC] Connection failed: ' + e.message);
        _gpuRtcCleanup();
        _gpuRtcScheduleReconnect();
    }
};

function _gpuRtcHandleJson(text) {
    var msg;
    try { msg = JSON.parse(text); } catch (e) { return; }

    // Handle retransmit requests from GPU sidecar (no job_id)
    if (msg.type === 'retransmit_request' && msg.stream_id && msg.missing) {
        if (HC._handleRetransmitRequest) HC._handleRetransmitRequest(msg.stream_id, msg.missing);
        return;
    }

    var jobId = msg.job_id;
    if (!jobId) return;

    // Handle partial transcription results
    if (jobId.indexOf('-partial') > 0 && msg.type === 'result') {
        var streamId = jobId.replace(/-partial.*$/, '');
        if (HC._onStreamPartial) HC._onStreamPartial(streamId, msg.output);
        return;
    }

    var pending = HC._gpuRtcPending[jobId];
    if (!pending) return;

    if (msg.type === 'accepted' && pending.onAccepted) pending.onAccepted(msg);
    else if (msg.type === 'progress' && pending.onProgress) pending.onProgress(msg.progress);
    else if (msg.type === 'chunk' && pending.onChunk) pending.onChunk(msg);
    else if (msg.type === 'result') {
        if (pending.onResult) pending.onResult(msg.output);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuRtcPending[jobId];
    }
    else if (msg.type === 'done') {
        if (pending.onDone) pending.onDone(msg);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuRtcPending[jobId];
    }
    else if (msg.type === 'error') {
        if (pending.onError) pending.onError(msg.error);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuRtcPending[jobId];
    }
    else if (msg.type === 'echo') {
        if (pending.onResult) pending.onResult(msg);
        if (pending._expireTimer) clearTimeout(pending._expireTimer);
        delete HC._gpuRtcPending[jobId];
    }
}

function _gpuRtcHandleBinary(payload) {
    // Binary audio chunks include a job_id prefix for routing
    // Format: [job_id_len:u8][job_id:ascii][audio_data]
    if (payload.byteLength < 2) return;
    var view = new DataView(payload);
    var idLen = view.getUint8(0);
    if (payload.byteLength < 1 + idLen) return;
    var jobId = new TextDecoder().decode(payload.slice(1, 1 + idLen));
    var audioData = payload.slice(1 + idLen);
    var pending = HC._gpuRtcPending[jobId];
    if (pending && pending.onBinaryChunk) pending.onBinaryChunk(audioData);
}

// Submit a job over WebRTC data channel
HC.gpuRtcSubmit = function(input) {
    if (!HC._gpuRtcReady || !HC._gpuRtcDc) return null;

    var jobId = (crypto.randomUUID ? crypto.randomUUID() : (Date.now().toString(36) + Math.random().toString(36).slice(2))).substring(0, 12);
    var controller = {
        jobId: jobId,
        backend: 'local-gpu-rtc',
        onAccepted: null, onChunk: null, onProgress: null, onResult: null,
        onDone: null, onError: null, onBinaryChunk: null,
        cancel: function() {
            _gpuRtcSendJson({ type: 'cancel', job_id: jobId });
            delete HC._gpuRtcPending[jobId];
        }
    };
    HC._gpuRtcPending[jobId] = controller;

    // Expire pending job after 90s if no response — prevents leak when
    // responses are lost during brief RTC disconnects.
    controller._expireTimer = setTimeout(function() {
        if (HC._gpuRtcPending[jobId]) {
            log('[GPU-RTC] Job expired (90s timeout): ' + jobId);
            if (controller.onError) controller.onError('Job timed out');
            delete HC._gpuRtcPending[jobId];
        }
    }, 90000);

    try {
        _gpuRtcSendJson({ type: 'submit', job_id: jobId, input: input });
        log('[GPU-RTC] Job submitted: ' + jobId + ' action=' + (input.action || '?'));
    } catch (e) {
        clearTimeout(controller._expireTimer);
        delete HC._gpuRtcPending[jobId];
        log('[GPU-RTC] Submit failed: ' + e.message);
        return null;
    }
    return controller;
};

// Backpressure: wait until bufferedAmount drops below threshold
var _DC_BACKPRESSURE_THRESHOLD = 1024 * 1024; // 1MB
function _dcWaitForDrain(dc) {
    if (!dc || dc.bufferedAmount <= _DC_BACKPRESSURE_THRESHOLD) return Promise.resolve();
    return new Promise(function(resolve) {
        var prev = dc.bufferedAmountLowThreshold;
        dc.bufferedAmountLowThreshold = _DC_BACKPRESSURE_THRESHOLD;
        var onLow = function() {
            dc.removeEventListener('bufferedamountlow', onLow);
            dc.bufferedAmountLowThreshold = prev;
            resolve();
        };
        dc.addEventListener('bufferedamountlow', onLow);
        // Safety timeout — don't block forever
        setTimeout(function() {
            dc.removeEventListener('bufferedamountlow', onLow);
            dc.bufferedAmountLowThreshold = prev;
            resolve();
        }, 5000);
    });
}

// Send JSON message over data channel with framing
async function _gpuRtcSendJson(obj) {
    var dc = HC._gpuRtcDc;
    if (!dc || dc.readyState !== 'open') throw new Error('data channel not open');
    if (dc.bufferedAmount > 256 * 1024) {
        console.warn('[GPU-RTC] backpressure — skipping JSON send (buffered=' + dc.bufferedAmount + ')');
        return;
    }

    var payload = new TextEncoder().encode(JSON.stringify(obj));
    var CHUNK_SIZE = 16 * 1024;

    if (5 + payload.length <= CHUNK_SIZE) {
        // Single frame
        await _dcWaitForDrain(dc);
        var frame = new ArrayBuffer(5 + payload.length);
        var view = new DataView(frame);
        view.setUint8(0, 0x01); // TYPE_JSON
        view.setUint32(1, payload.length, true);
        new Uint8Array(frame, 5).set(payload);
        dc.send(frame);
    } else {
        // Chunk the payload. Each frame = 5 (header) + 9 (chunk header) + data.
        // Max frame must be <= CHUNK_SIZE (16KB) for cross-browser safety.
        var CHUNK_DATA_SIZE = CHUNK_SIZE - 5 - 9; // 16370 bytes per chunk
        var totalChunks = Math.ceil(payload.length / CHUNK_DATA_SIZE);
        for (var i = 0; i < totalChunks; i++) {
            await _dcWaitForDrain(dc);
            var start = i * CHUNK_DATA_SIZE;
            var end = Math.min(start + CHUNK_DATA_SIZE, payload.length);
            var chunkData = payload.slice(start, end);
            var chunkFrame = new ArrayBuffer(5 + 9 + chunkData.length);
            var cv = new DataView(chunkFrame);
            cv.setUint8(0, 0x05); // TYPE_CHUNK
            cv.setUint32(1, 9 + chunkData.length, true);
            cv.setUint32(5, i, true);           // seq
            cv.setUint32(9, totalChunks, true);  // total
            cv.setUint8(13, 0x01);               // original_type = JSON
            new Uint8Array(chunkFrame, 14).set(chunkData);
            dc.send(chunkFrame);
        }
    }
}

// Send binary data over data channel
async function _gpuRtcSendBinary(data) {
    var dc = HC._gpuRtcDc;
    if (!dc || dc.readyState !== 'open') throw new Error('data channel not open');
    if (dc.bufferedAmount > 256 * 1024) {
        console.warn('[GPU-RTC] backpressure — skipping binary send (buffered=' + dc.bufferedAmount + ')');
        return;
    }

    var CHUNK_SIZE = 16 * 1024;
    if (5 + data.byteLength <= CHUNK_SIZE) {
        await _dcWaitForDrain(dc);
        var frame = new ArrayBuffer(5 + data.byteLength);
        var view = new DataView(frame);
        view.setUint8(0, 0x02); // TYPE_BINARY
        view.setUint32(1, data.byteLength, true);
        new Uint8Array(frame, 5).set(new Uint8Array(data));
        dc.send(frame);
    } else {
        var CHUNK_DATA_SIZE = CHUNK_SIZE - 5 - 9;
        var totalChunks = Math.ceil(data.byteLength / CHUNK_DATA_SIZE);
        for (var i = 0; i < totalChunks; i++) {
            await _dcWaitForDrain(dc);
            var start = i * CHUNK_DATA_SIZE;
            var end = Math.min(start + CHUNK_DATA_SIZE, data.byteLength);
            var chunkData = new Uint8Array(data, start, end - start);
            var chunkFrame = new ArrayBuffer(5 + 9 + chunkData.length);
            var cv = new DataView(chunkFrame);
            cv.setUint8(0, 0x05);
            cv.setUint32(1, 9 + chunkData.length, true);
            cv.setUint32(5, i, true);
            cv.setUint32(9, totalChunks, true);
            cv.setUint8(13, 0x02);
            new Uint8Array(chunkFrame, 14).set(chunkData);
            dc.send(chunkFrame);
        }
    }
}

function _gpuRtcIceRestart(pc) {
    if (!pc || pc.connectionState === 'closed') return;
    pc.restartIce();
    pc.createOffer({ iceRestart: true }).then(function(offer) {
        return pc.setLocalDescription(offer);
    }).then(function() {
        // Wait for gathering, then re-signal
        var checkGather = function() {
            if (pc.iceGatheringState === 'complete') {
                pc.removeEventListener('icegatheringstatechange', checkGather);
                // Re-send offer to EC2
                fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/webrtc/offer', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + HC.config.daemonToken },
                    body: JSON.stringify({ sdp: pc.localDescription.sdp, target: 'gpu-worker' }),
                }).then(function(resp) {
                    if (!resp.ok) throw new Error('ICE restart offer rejected');
                    return resp.json();
                }).then(function(answer) {
                    HC._gpuRtcSessionId = answer.session_id;
                    return pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
                }).catch(function(e) {
                    log('[GPU-RTC] ICE restart failed: ' + e.message);
                    _gpuRtcCleanup();
                    _gpuRtcScheduleReconnect();
                });
            }
        };
        pc.addEventListener('icegatheringstatechange', checkGather);
        setTimeout(function() { checkGather(); }, 3000);
    }).catch(function(e) {
        log('[GPU-RTC] ICE restart offer failed: ' + e.message);
    });
}

function _gpuRtcCleanup() {
    HC._gpuRtcReady = false;
    HC._gpuRtcSessionId = null;
    HC._gpuRtcLastPong = 0;
    _dcChunks = null;
    if (HC._gpuRtcPingTimer) { clearInterval(HC._gpuRtcPingTimer); HC._gpuRtcPingTimer = null; }
    if (HC._gpuRtcDc) { try { HC._gpuRtcDc.close(); } catch(e) {} HC._gpuRtcDc = null; }
    if (HC._gpuRtc) { try { HC._gpuRtc.close(); } catch(e) {} HC._gpuRtc = null; }
    // Reject all pending RTC jobs
    var ids = Object.keys(HC._gpuRtcPending);
    for (var i = 0; i < ids.length; i++) {
        var p = HC._gpuRtcPending[ids[i]];
        if (p && p._expireTimer) clearTimeout(p._expireTimer);
        if (p && p.onError) p.onError('GPU WebRTC disconnected');
        delete HC._gpuRtcPending[ids[i]];
    }
}

function _gpuRtcScheduleReconnect() {
    if (HC._gpuRtcTimer) return;
    HC._gpuRtcTimer = setTimeout(function() {
        HC._gpuRtcTimer = null;
        HC.gpuRtcConnect();
    }, HC._gpuRtcBackoff);
    HC._gpuRtcBackoff = Math.min(HC._gpuRtcBackoff * 2, 30000);
}
