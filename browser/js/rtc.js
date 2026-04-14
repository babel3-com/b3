/* rtc.js — Babel3 WebRTC data channel to daemon (open-source)
 *
 * P2P connection to daemon via WebRTC data channels. Falls back to
 * WebSocket (E2E encrypted relay) if WebRTC is unavailable.
 * Same message protocol as ws.js.
 *
 * Depends on: core.js (HC namespace), ws.js (fallback)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

HC._daemonRtc = null;         // RTCPeerConnection
HC._daemonRtcFailCount = 0;   // Consecutive failures for exponential backoff
HC._daemonRtcControl = null;  // DataChannel: control (JSON RPC)
HC._daemonRtcTerminal = null; // DataChannel: terminal (binary PTY)
HC._daemonRtcReady = false;
HC._daemonRtcSessionId = null;

/**
 * Attempt WebRTC connection to daemon. On success, routes all daemon
 * communication through data channels. On failure, silently falls back
 * to WebSocket (ws.js handles its own connection).
 */
HC.daemonRtcConnect = async function() {
    if (!HC.EC2_BASE || !HC.config || !HC.config.agentId || !HC.config.daemonToken) return;
    if (HC._daemonRtcReady || HC._daemonRtc) return;
    if (!HC.rtcEnabled()) return; // WS-only mode — skip RTC

    try {
        // 1. Fetch TURN credentials
        var credResp = await fetch(HC.EC2_BASE + '/api/webrtc/turn-credentials', {
            headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
        });
        if (!credResp.ok) { log('[DAEMON-RTC] TURN credentials failed'); return; }
        var creds = await credResp.json();

        // 2. Create peer connection
        var pc = new RTCPeerConnection({ iceServers: creds.ice_servers });
        HC._daemonRtc = pc;

        // 3. Create data channels
        var controlDc = pc.createDataChannel('control', { ordered: true });
        var terminalDc = pc.createDataChannel('terminal', { ordered: true });
        terminalDc.binaryType = 'arraybuffer';
        HC._daemonRtcControl = controlDc;
        HC._daemonRtcTerminal = terminalDc;

        var channelsOpen = 0;
        function onChannelOpen() {
            channelsOpen++;
            if (channelsOpen >= 2) {
                HC._daemonRtcReady = true;
                log('[DAEMON-RTC] Both channels open — P2P to daemon active');

                // Update status bar
                var dot = document.getElementById('status-dot');
                if (dot) dot.className = 'status-dot online';
                var connType = document.getElementById('rtc-conn-type');
                if (connType) connType.textContent = 'P2P';

                // Send initial handshake (same as ws.js onopen)
                _rtcSendControl({ type: 'browser_info',
                    user_agent: navigator.userAgent,
                    volume: HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100,
                    audio_context: !!(typeof gaplessPlayer !== 'undefined' && HC.gaplessPlayer && HC.gaplessPlayer._ctx)
                });
                if (HC._serverVersion) {
                    _rtcSendControl({ type: 'version', server_version: HC._serverVersion });
                }

                // Fetch init bundle via RPC
                _rtcSendControl({ id: 'init-1', method: 'GET', path: '/api/init-bundle' });
            }
        }

        controlDc.onopen = onChannelOpen;
        terminalDc.onopen = onChannelOpen;

        controlDc.onclose = function() {
            log('[DAEMON-RTC] Control channel closed');
            _daemonRtcCleanup('channel_closed');
        };
        terminalDc.onclose = function() {
            log('[DAEMON-RTC] Terminal channel closed');
            _daemonRtcCleanup('channel_closed');
        };

        // 4. Control channel messages — binary-framed protocol from daemon
        //    Wire format: [type:u8][length:u32LE][payload]
        //    Type 0x01 = JSON, 0x03 = Ping, 0x04 = Pong
        controlDc.onmessage = function(event) {
            var data = event.data;

            // Binary frame (ArrayBuffer) — decode framing protocol
            if (data instanceof ArrayBuffer) {
                if (data.byteLength < 5) return;
                var view = new DataView(data);
                var type = view.getUint8(0);
                var len = view.getUint32(1, true);
                if (type === 0x01) {
                    // JSON payload
                    try {
                        var payload = data.slice(5, 5 + len);
                        var msg = JSON.parse(new TextDecoder().decode(payload));
                        _handleDaemonEvent(msg);
                    } catch (e) {
                        log('[DAEMON-RTC] Control JSON decode error: ' + e.message);
                    }
                } else if (type === 0x03) {
                    // Ping — respond with pong
                    _rtcSendPong();
                }
                // 0x04 Pong — ignore
                return;
            }

            // String fallback (shouldn't happen with current daemon, but safe)
            if (typeof data === 'string') {
                try {
                    var msg = JSON.parse(data);
                    _handleDaemonEvent(msg);
                } catch (e) {
                    log('[DAEMON-RTC] Control message parse error: ' + e.message);
                }
            }
        };

        // 5. Terminal channel messages — framed protocol from daemon
        var _termChunks = null; // chunk reassembly state for terminal channel

        terminalDc.onmessage = function(event) {
            var data = event.data;
            if (typeof data === 'string') {
                try {
                    var msg = JSON.parse(data);
                    _handleDaemonMessage(msg);
                } catch(e) {}
                return;
            }
            if (!(data instanceof ArrayBuffer) || data.byteLength < 2) return;
            var u8raw = new Uint8Array(data);
            // Check for raw reliable frame (no b3-webrtc framing) — can happen
            // when datachannel delivers SCTP payload directly or framing is stripped
            if (u8raw[0] === 0xB3 && u8raw[1] >= 0x01 && u8raw[1] <= 0x03) {
                if (HC.daemonChannel) {
                    HC.daemonChannel.receive(u8raw);
                }
                return;
            }
            if (data.byteLength < 5) return;
            var view = new DataView(data);
            var type = view.getUint8(0);
            var len = view.getUint32(1, true);
            var payload = data.slice(5, 5 + len);
            if (type === 0x01) {
                // JSON — init/delta/full terminal messages
                try {
                    var msg = JSON.parse(new TextDecoder().decode(payload));
                    _handleDaemonMessage(msg);
                } catch(e) {}
            } else if (type === 0x02) {
                // Binary payload — check for reliable frame (0xB3 prefix)
                var binU8 = new Uint8Array(payload);
                if (HC.reliableIsReliable && HC.reliableIsReliable(binU8)) {
                    // Reliable frame from daemon — route to shared daemonChannel
                    if (HC.daemonChannel) {
                        // Register RTC transport sink if not already registered
                        if (!HC.daemonChannel._handles['rtc']) {
                            HC.daemonChannel._transportDefs['rtc'] = {
                                connect: function() { return null; },
                                priority: 0, // RTC preferred over WS
                            };
                            var rtcHandle = {
                                readyState: 'open',
                                send: function(d) {
                                    // Wrap in RTC binary framing: [0x02][len:u32LE][payload]
                                    // The daemon's b3-webrtc decoder expects ALL data on the
                                    // channel to use this framing. Raw bytes get misinterpreted.
                                    if (terminalDc.readyState !== 'open') return;
                                    var payload = (d instanceof Uint8Array) ? d : new Uint8Array(d);
                                    var frame = new ArrayBuffer(5 + payload.length);
                                    var view = new DataView(frame);
                                    view.setUint8(0, 0x02); // TYPE_BINARY
                                    view.setUint32(1, payload.length, true);
                                    new Uint8Array(frame, 5).set(payload);
                                    terminalDc.send(frame);
                                },
                                close: function() {},
                                onmessage: null,
                                onclose: null,
                                _lastRecv: Date.now(), // Initialize so heartbeat doesn't kill before first frame
                            };
                            HC.daemonChannel._handles['rtc'] = rtcHandle;
                            HC.daemonChannel._onTransportOpen('rtc', rtcHandle, HC.daemonChannel._activeName());
                            HC.log('[DAEMON-RTC] Terminal registered with daemonChannel (priority 0)');
                        }
                        HC.daemonChannel.noteRecv('rtc');
                        HC.daemonChannel.receive(binU8);
                    } else if (HC._rtcTermReliable) {
                        // Fallback: old standalone channel
                        HC._rtcTermReliable.receive(binU8);
                    }
                } else {
                    // Legacy raw binary PTY bytes
                    if (HC.term) HC.term.write(binU8);
                }
            } else if (type === 0x05) {
                // Chunked message — reassemble then process
                var cv = new DataView(payload);
                if (payload.byteLength < 9) return;
                var seq = cv.getUint32(0, true);
                var total = cv.getUint32(4, true);
                var originalType = cv.getUint8(8);
                var chunkData = payload.slice(9);
                if (!_termChunks || _termChunks.total !== total) {
                    _termChunks = { total: total, originalType: originalType, parts: new Array(total), received: 0 };
                }
                if (!_termChunks.parts[seq]) {
                    _termChunks.parts[seq] = chunkData;
                    _termChunks.received++;
                }
                if (_termChunks.received >= _termChunks.total) {
                    var totalLen = 0;
                    for (var i = 0; i < _termChunks.parts.length; i++) totalLen += _termChunks.parts[i].byteLength;
                    var assembled = new Uint8Array(totalLen);
                    var off = 0;
                    for (var i = 0; i < _termChunks.parts.length; i++) {
                        assembled.set(new Uint8Array(_termChunks.parts[i]), off);
                        off += _termChunks.parts[i].byteLength;
                    }
                    var ot = _termChunks.originalType;
                    _termChunks = null;
                    if (ot === 0x01) {
                        try {
                            var msg = JSON.parse(new TextDecoder().decode(assembled));
                            _handleDaemonMessage(msg);
                        } catch(e) {}
                    } else if (ot === 0x02) {
                        // Check for reliable frame after reassembly
                        if (HC.reliableIsReliable && HC.reliableIsReliable(assembled) && (HC.daemonChannel || HC._rtcTermReliable)) {
                            (HC.daemonChannel || HC._rtcTermReliable).receive(assembled);
                        } else {
                            if (HC.term) HC.term.write(assembled);
                        }
                    }
                }
            }
        };

        // 6. ICE candidates
        pc.onicecandidate = function(event) {
            if (event.candidate && HC._daemonRtcSessionId) {
                fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/webrtc/ice', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + HC.config.daemonToken },
                    body: JSON.stringify({
                        session_id: HC._daemonRtcSessionId,
                        candidate: event.candidate.candidate,
                        mid: event.candidate.sdpMid || '',
                        target: 'daemon',
                    }),
                }).catch(function() {});
            }
        };

        // 7. Connection state
        pc.onconnectionstatechange = function() {
            log('[DAEMON-RTC] State: ' + pc.connectionState);
            if (pc.connectionState === 'connected') {
                HC._daemonRtcFailCount = 0; // Reset backoff on success
                log('[DAEMON-RTC] Connected — P2P active');
            } else if (pc.connectionState === 'failed') {
                _daemonRtcCleanup('ice_failed');
                HC._daemonRtcFailCount++;
                // Exponential backoff: 30s → 60s → 120s → 300s max
                var backoffMs = Math.min(30000 * Math.pow(2, HC._daemonRtcFailCount - 1), 300000);
                log('[DAEMON-RTC] Failed (attempt ' + HC._daemonRtcFailCount + ') — retrying in ' + (backoffMs/1000) + 's');
                setTimeout(function() {
                    if (!HC._daemonRtcReady && HC.daemonRtcConnect) {
                        log('[DAEMON-RTC] Retrying WebRTC connection');
                        HC.daemonRtcConnect().catch(function() {});
                    }
                }, backoffMs);
            } else if (pc.connectionState === 'disconnected') {
                setTimeout(function() {
                    if (pc.connectionState === 'disconnected') {
                        log('[DAEMON-RTC] Still disconnected — ICE restart');
                        var doRestart = function() {
                            pc.restartIce();
                            pc.createOffer({ iceRestart: true }).then(function(offer) {
                                return pc.setLocalDescription(offer);
                        }).then(function() {
                            return _waitForGathering(pc, 3000);
                        }).then(function() {
                            return fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/webrtc/offer', {
                                method: 'POST',
                                headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + HC.config.daemonToken },
                                body: JSON.stringify({ sdp: pc.localDescription.sdp, target: 'daemon', session_id: parseInt(HC._daemonSessionId || HC._clientId, 10) }),
                            });
                        }).then(function(resp) {
                            if (!resp.ok) throw new Error('ICE restart rejected');
                            return resp.json();
                        }).then(function(answer) {
                            HC._daemonRtcSessionId = answer.session_id;
                            return pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
                        }).catch(function(e) {
                            log('[DAEMON-RTC] ICE restart failed: ' + e.message);
                            _daemonRtcCleanup('ice_restart_failed');
                        });
                        };
                        // Defer ICE restart if audio session active
                        if (HC.audioLocked && HC.audioLocked()) {
                            HC.audioDefer(doRestart);
                        } else {
                            doRestart();
                        }
                    }
                }, 3000);
            }
        };

        // 8. Create offer and signal
        var offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        await _waitForGathering(pc, 5000);

        var offerResp = await fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/webrtc/offer', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + HC.config.daemonToken },
            body: JSON.stringify({ sdp: pc.localDescription.sdp, target: 'daemon', session_id: parseInt(HC._daemonSessionId || HC._clientId, 10) }),
            signal: AbortSignal.timeout(15000),
        });

        if (!offerResp.ok) {
            log('[DAEMON-RTC] Offer rejected: ' + offerResp.status);
            _daemonRtcCleanup('offer_rejected');
            return;
        }

        var answer = await offerResp.json();
        HC._daemonRtcSessionId = answer.session_id;
        await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
        log('[DAEMON-RTC] Remote description set, waiting for channels...');

    } catch (e) {
        log('[DAEMON-RTC] Connection failed: ' + e.message);
        _daemonRtcCleanup('connection_error');
    }
};

// ── Control message dispatch ──────────────────────────────────────

function _dispatchControlMessage(msg) {
    // RPC responses (have an 'id' field)
    if (msg.id && msg.status !== undefined) {
        _handleRpcResponse(msg);
        return;
    }
    // Server-push events (same as ws.js handleWsMessage)
    if (msg.event) {
        _handleDaemonEvent(msg);
        return;
    }
    // Direct messages (auth protocol, heartbeat, etc.)
    _handleDaemonMessage(msg);
}

function _rtcSendPong() {
    var dc = HC._daemonRtcControl;
    if (!dc || dc.readyState !== 'open') return;
    var frame = new ArrayBuffer(5);
    var view = new DataView(frame);
    view.setUint8(0, 0x04); // TYPE_PONG
    view.setUint32(1, 0, true); // length = 0
    dc.send(frame);
}

// ── RPC over control channel ──────────────────────────────────────

var _rtcRpcCallbacks = {};

function _rtcSendFramed(obj) {
    var dc = HC._daemonRtcControl;
    if (!dc || dc.readyState !== 'open') return;
    if (dc.bufferedAmount > 256 * 1024) {
        log('[DAEMON-RTC] backpressure — skipping send (buffered=' + dc.bufferedAmount + ')');
        return;
    }
    var payload = new TextEncoder().encode(JSON.stringify(obj));
    var frame = new ArrayBuffer(5 + payload.length);
    var view = new DataView(frame);
    view.setUint8(0, 0x01); // TYPE_JSON
    view.setUint32(1, payload.length, true); // little-endian length
    new Uint8Array(frame, 5).set(payload);
    dc.send(frame);
}

function _rtcSendControl(msg) {
    _rtcSendFramed(msg);
}

function _handleRpcResponse(msg) {
    var cb = _rtcRpcCallbacks[msg.id];
    if (cb) {
        delete _rtcRpcCallbacks[msg.id];
        cb(msg.status, msg.body);
    }
}

// RPC call — returns promise
HC.rtcRpc = function(method, path, body) {
    return new Promise(function(resolve, reject) {
        var id = 'rpc-' + Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
        var msg = { id: id, method: method, path: path };
        if (body !== undefined) msg.body = body;

        _rtcRpcCallbacks[id] = function(status, respBody) {
            if (status >= 200 && status < 300) resolve(respBody);
            else reject(new Error('RPC ' + path + ' failed: HTTP ' + status));
        };

        _rtcSendControl(msg);

        // Timeout
        setTimeout(function() {
            if (_rtcRpcCallbacks[id]) {
                delete _rtcRpcCallbacks[id];
                reject(new Error('RPC timeout: ' + path));
            }
        }, 30000);
    });
};

// ── Daemon event handlers (same logic as ws.js handleWsMessage) ──

function _handleDaemonEvent(msg) {
    // Server-push events: { event: "tts_stream", ... }
    var event = msg.event;

    if (event === 'tts_stream') {
        // Reformat to match WS message format (type instead of event)
        msg.type = 'tts_stream';
        if (typeof handleWsMessage === 'function') {
            // Reuse ws.js handler if available (it has full TTS logic)
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'tts_audio_data') {
        msg.type = 'tts_audio_data';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'tts_generating') {
        msg.type = 'tts_generating';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'led') {
        msg.type = 'led';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'info') {
        msg.type = 'info';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'browser_eval') {
        msg.type = 'browser_eval';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'transcription') {
        msg.type = 'transcription';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'hive' || event === 'hive_room') {
        msg.type = event;
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    } else if (event === 'notification') {
        msg.type = 'notification';
        if (typeof handleWsMessage === 'function') {
            handleWsMessage({ data: JSON.stringify(msg) });
        }
    }
}

function _handleDaemonMessage(msg) {
    // Direct messages (auth, heartbeat, terminal data)
    if (msg.type === 'heartbeat') {
        _rtcSendControl({ type: 'pong' });
        return;
    }
    if (msg.type === 'auth_required') {
        var savedPw = HC._getDaemonPw ? HC._getDaemonPw() : null;
        if (savedPw) {
            _rtcSendControl({ type: 'auth', password: savedPw });
        } else if (typeof _showDaemonPwPrompt === 'function') {
            _showDaemonPwPrompt();
        }
        return;
    }
    if (msg.type === 'auth_ok') {
        log('[DAEMON-RTC] Auth accepted');
        var el = document.getElementById('daemon-pw-overlay');
        if (el) el.remove();
        // Re-send handshake after auth
        if (HC._serverVersion) {
            _rtcSendControl({ type: 'version', server_version: HC._serverVersion });
        }
        var _hasAC = typeof gaplessPlayer !== 'undefined' && HC.gaplessPlayer && HC.gaplessPlayer._ctx;
        _rtcSendControl({
            type: 'browser_info',
            user_agent: navigator.userAgent,
            volume: HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100,
            audio_context: !!(_hasAC && HC.gaplessPlayer._ctx.state === 'running')
        });
        return;
    }
    if (msg.type === 'auth_failed') {
        if (typeof _showDaemonPwError === 'function') _showDaemonPwError();
        return;
    }
    // Forward all message types to ws.js handler — single code path for
    // terminal rendering (init/delta/full), TTS, transcription, etc.
    if (typeof handleWsMessage === 'function' && msg.type !== 'heartbeat' && msg.type !== 'auth_required' && msg.type !== 'auth_ok' && msg.type !== 'auth_failed') {
        handleWsMessage({ data: JSON.stringify(msg) });
    }
}

// ── Terminal input over data channel ──────────────────────────────

HC.rtcSendTerminal = function(data) {
    var dc = HC._daemonRtcTerminal;
    if (!dc || dc.readyState !== 'open') return false;

    if (typeof data === 'string') {
        dc.send(new TextEncoder().encode(data));
    } else {
        dc.send(data);
    }
    return true;
};

// ── Unified send — routes to WebRTC or WebSocket ─────────────────

HC.daemonSend = function(data) {
    // Prefer WebRTC if available
    if (HC._daemonRtcReady) {
        if (typeof data === 'string') {
            _rtcSendControl(JSON.parse(data));
        } else {
            HC.rtcSendTerminal(data);
        }
        return;
    }
    // Fall back to WebSocket
    HC.wsSend(data);
};

// ── Helpers ──────────────────────────────────────────────────────────

function _waitForGathering(pc, timeoutMs) {
    return new Promise(function(resolve) {
        if (pc.iceGatheringState === 'complete') { resolve(); return; }
        var done = false;
        var check = function() {
            if (done) return;
            if (pc.iceGatheringState === 'complete') {
                done = true;
                pc.removeEventListener('icegatheringstatechange', check);
                resolve();
            }
        };
        pc.addEventListener('icegatheringstatechange', check);
        setTimeout(function() { if (!done) { done = true; resolve(); } }, timeoutMs);
    });
}

function _daemonRtcCleanup(reason) {
    HC._daemonRtcReady = false;
    HC._daemonRtcSessionId = null;
    // Tag disconnect reason on the handle before removing it (for telemetry)
    if (HC.daemonChannel && HC.daemonChannel._handles && HC.daemonChannel._handles['rtc']) {
        HC.daemonChannel._handles['rtc']._disconnectReason = reason || 'unknown';
        HC.daemonChannel._disconnectTransport('rtc');
    }
    if (HC._daemonRtcControl) { try { HC._daemonRtcControl.close(); } catch(e) {} HC._daemonRtcControl = null; }
    if (HC._daemonRtcTerminal) { try { HC._daemonRtcTerminal.close(); } catch(e) {} HC._daemonRtcTerminal = null; }
    if (HC._daemonRtc) { try { HC._daemonRtc.close(); } catch(e) {} HC._daemonRtc = null; }
    _rtcRpcCallbacks = {};

    // Update UI — show tunnel fallback
    var connType = document.getElementById('rtc-conn-type');
    if (connType) connType.textContent = 'Tunnel';
}

// ── GPU RTC Connect ────────────────────────────────────────────────
// Same pattern as daemonRtcConnect but for the GPU relay:
// - Single data channel (no control/terminal split)
// - Signaling goes directly to relay tunnel URL (not EC2)
// - Registers on gpuChannel (not daemonChannel)

HC._gpuRtc = null;
HC._gpuRtcDc = null;
HC._gpuRtcReady = false;
HC._gpuRtcFailCount = 0;
HC._gpuRtcBackoffUntil = 0;

HC.gpuRtcConnect = async function() {
    if (!HC.gpuChannel) return;
    if (HC._gpuRtcReady || HC._gpuRtc) return;
    if (!HC.rtcEnabled()) return;
    // Exponential backoff: after 3 consecutive failures, wait 5 minutes
    if (HC._gpuRtcFailCount >= 3 && Date.now() < HC._gpuRtcBackoffUntil) {
        return;
    }

    try {
        // 1. Fetch TURN credentials (same as daemon)
        var turnUrl = HC.EC2_BASE + '/api/webrtc/turn-credentials';
        var credResp = await fetch(turnUrl, {
            headers: HC.config.daemonToken ? { 'Authorization': 'Bearer ' + HC.config.daemonToken } : {},
        });
        var iceServers = [{ urls: 'stun:stun.l.google.com:19302' }];
        if (credResp.ok) {
            var creds = await credResp.json();
            if (creds.ice_servers) iceServers = creds.ice_servers;
        }

        // 2. Create peer connection
        var pc = new RTCPeerConnection({ iceServers: iceServers });
        HC._gpuRtc = pc;

        // 3. Create single data channel
        var dc = pc.createDataChannel('data', { ordered: true });
        dc.binaryType = 'arraybuffer';
        HC._gpuRtcDc = dc;

        dc.onopen = function() {
            HC._gpuRtcReady = true;
            HC._gpuRtcFailCount = 0; // Reset on success
            log('[GPU-RTC] Data channel open');

            // Register RTC transport on gpuChannel (priority 0 — preferred)
            if (HC.gpuChannel) {
                HC.gpuChannel._transportDefs['rtc'] = {
                    connect: function() { return null; }, // managed externally
                    priority: 0,
                };
                var rtcHandle = {
                    readyState: 'open',
                    send: function(data) { if (dc.readyState === 'open') dc.send(data); },
                    close: function() {},
                    onmessage: null,
                    onclose: null,
                };
                HC.gpuChannel._disconnectTransport('rtc');
                HC.gpuChannel._handles['rtc'] = rtcHandle;
                HC.gpuChannel._onTransportOpen('rtc', rtcHandle, HC.gpuChannel._activeName());
                log('[GPU-RTC] Registered on gpuChannel (priority 0)');
            }
        };

        dc.onclose = function() {
            log('[GPU-RTC] Data channel closed');
            _gpuRtcCleanup();
        };

        dc.onmessage = function(event) {
            // Feed to gpuChannel reliable layer
            if (event.data instanceof ArrayBuffer && HC.gpuChannel) {
                var u8 = new Uint8Array(event.data);
                if (HC.reliableIsReliable && HC.reliableIsReliable(u8)) {
                    HC.gpuChannel.noteRecv('rtc');
                    HC.gpuChannel.receive(u8);
                }
            }
        };

        // 4. Connection state
        pc.onconnectionstatechange = function() {
            log('[GPU-RTC] State: ' + pc.connectionState);
            if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
                _gpuRtcCleanup();
            }
        };

        // Tunnel path removed in #417 — no signaling endpoint available.
        log('[GPU-RTC] No signaling endpoint (tunnel removed) — aborting');
        _gpuRtcCleanup();
        return;

    } catch (e) {
        log('[GPU-RTC] Connect failed: ' + e.message);
        _gpuRtcCleanup();
    }
};

function _gpuRtcCleanup() {
    HC._gpuRtcReady = false;
    HC._gpuRtcFailCount++;
    if (HC._gpuRtcFailCount >= 3) {
        var backoffMin = Math.min(5 * Math.pow(2, HC._gpuRtcFailCount - 3), 30); // 5, 10, 20, 30 min
        HC._gpuRtcBackoffUntil = Date.now() + backoffMin * 60 * 1000;
        log('[GPU-RTC] ' + HC._gpuRtcFailCount + ' consecutive failures — backing off ' + backoffMin + 'min');
    }
    if (HC.gpuChannel && HC.gpuChannel._handles['rtc']) {
        HC.gpuChannel._disconnectTransport('rtc');
    }
    if (HC._gpuRtcDc) { try { HC._gpuRtcDc.close(); } catch(e) {} HC._gpuRtcDc = null; }
    if (HC._gpuRtc) { try { HC._gpuRtc.close(); } catch(e) {} HC._gpuRtc = null; }
}
