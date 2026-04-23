/* daemon-ws.js — E2E encrypted daemon WebSocket via EC2 proxy
 *
 * Provides a WebSocket-compatible interface. Always uses EC2 encrypted proxy
 * (ECDH + AES-256-GCM). No LAN direct-connect attempt.
 *
 * # EC2 encrypted path
 * 1. GET /api/daemon-key?agent_id=X → { pubkey (base64 x25519) }
 * 2. Generate ephemeral x25519 keypair (Web Crypto)
 * 3. ECDH → shared bits → HKDF-SHA256 → 64 bytes
 *    [0..32] = browser send / daemon recv key (AES-256-GCM)
 *    [32..64] = daemon send / browser recv key (AES-256-GCM)
 * 4. Connect wss://{ec2Base}/api/daemon-proxy?agent_id=X (session cookie auth)
 * 5. Send plaintext: {"type":"handshake","pubkey":"<b64 ephemeral>","session_id":N}
 * 6. Await plaintext: {"type":"handshake_ok"} (5s timeout)
 * 7. All subsequent frames: [12-byte nonce][ciphertext + AES-GCM tag]
 *    Nonce layout: [0x00,0x00,0x00,0x00, counter_u64_LE]
 *
 * # Reliability layering (EC2 path only)
 * An inner ReliableChannel (_innerRC) wraps all outbound frames before encryption.
 * This is the browser-side peer of the daemon's EncryptedChannel.inner (b3-reliable).
 * The outer HC.daemonChannel ↔ handle_terminal_ws RC provides end-to-end session
 * reliability with Resume-on-reconnect. The inner layer handles EC2-hop reliability.
 *
 * # Browser compatibility
 * Requires Web Crypto x25519 (Chrome 113+, Safari 17.4+, Firefox 130+).
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

(function() {

    var HANDSHAKE_TIMEOUT_MS = 5000;
    var HKDF_SALT = new TextEncoder().encode('b3-daemon-proxy');
    var HKDF_INFO = new TextEncoder().encode('b3-daemon-proxy-v1');

    // ── DaemonWS ──────────────────────────────────────────────────────
    //
    // Drop-in for `new WebSocket(url)` in ws.js.
    // Presents the same interface: onopen, onmessage, onerror, onclose,
    // send(), close(), readyState, binaryType.

    function DaemonWS(agentId, sessionId, ec2Base, daemonToken) {
        this._agentId    = agentId;
        this._sessionId  = sessionId;
        this._ec2Base    = ec2Base;     // e.g. 'https://babel3.com'
        this._daemonToken = daemonToken;

        // WebSocket-compatible public API
        this.readyState  = 0; // CONNECTING
        this.binaryType  = 'arraybuffer';
        this.onopen      = null;
        this.onmessage   = null;
        this.onerror     = null;
        this.onclose     = null;

        // Internal
        this._ws         = null;  // underlying WebSocket (EC2 path)
        this._encryptKey = null;  // CryptoKey — browser→daemon
        this._decryptKey = null;  // CryptoKey — daemon→browser
        this._sendCounter = 0;
        this._innerRC    = null;  // ReliableChannel (EC2 path only)
        this._sendQueue  = [];    // outbound enc queue (handles async crypto)
        this._flushing   = false;
        this._closed     = false;
        this._pendingHttpRequests = {}; // id → {resolve, reject, timer}

        this._connect();
    }

    // ── Connection logic ──────────────────────────────────────────────

    DaemonWS.prototype._connect = async function() {
        var self = this;
        self._connectT0 = performance.now();
        _log('[DaemonWS] connect start agent=' + self._agentId);

        // Step 1: Fetch daemon pubkey from EC2.
        var ec2Url = this._ec2Base + '/api/daemon-key?agent_id=' + encodeURIComponent(this._agentId);
        var keyInfo;
        try {
            var headers = {};
            if (this._daemonToken) headers['Authorization'] = 'Bearer ' + this._daemonToken;
            var resp = await fetch(ec2Url, {
                credentials: 'include',
                cache: 'no-store',
                headers: headers,
            });
            if (!resp.ok) throw new Error('HTTP ' + resp.status);
            keyInfo = await resp.json();
        } catch(e) {
            _log('[DaemonWS] daemon-key fetch failed: ' + e.message);
            self._fail(new Error('daemon-key fetch failed: ' + e.message));
            return;
        }
        _log('[DaemonWS] daemon-key +' + Math.round(performance.now() - self._connectT0) + 'ms');

        var pubkeyB64 = keyInfo.pubkey;

        // Step 2: EC2 encrypted path.
        await self._connectEc2(pubkeyB64);
    };

    // EC2 encrypted path: ECDH + AES-256-GCM.
    DaemonWS.prototype._connectEc2 = async function(pubkeyB64) {
        var self = this;
        var _t0 = self._connectT0 || performance.now();

        // Step 3a: ECDH key derivation.
        var keys;
        try {
            keys = await _deriveKeys(pubkeyB64);
        } catch(e) {
            _log('[DaemonWS] Key derivation failed: ' + e.message);
            self._fail(e);
            return;
        }

        self._encryptKey  = keys.encryptKey;  // browser→daemon
        self._decryptKey  = keys.decryptKey;  // daemon→browser
        self._sendCounter = 0;
        _log('[DaemonWS] ECDH +' + Math.round(performance.now() - _t0) + 'ms');

        // Step 3b: Connect EC2 proxy WS.
        var wsBase = self._ec2Base.replace(/^https?:\/\//, '');
        var isHttps = self._ec2Base.startsWith('https');
        var wsUrl = (isHttps ? 'wss://' : 'ws://') + wsBase +
                    '/api/daemon-proxy?agent_id=' + encodeURIComponent(self._agentId);
        // WebSocket API cannot set custom headers — pass token as query param instead.
        if (self._daemonToken) wsUrl += '&token=' + encodeURIComponent(self._daemonToken);

        var ws;
        try {
            ws = new WebSocket(wsUrl);
            ws.binaryType = 'arraybuffer';
        } catch(e) {
            _log('[DaemonWS] EC2 WS create failed: ' + e.message);
            self._fail(e);
            return;
        }
        self._ws = ws;

        // Step 3c: Send handshake frame once WS opens.
        var handshakeOkReceived = false;
        var handshakeTimer = null;
        var handshakeResolve, handshakeReject;
        var handshakePromise = new Promise(function(res, rej) {
            handshakeResolve = res;
            handshakeReject  = rej;
        });

        ws.onopen = async function() {
            _log('[DaemonWS] WS open (TCP+TLS) +' + Math.round(performance.now() - _t0) + 'ms');
            var handshakeFrame = JSON.stringify({
                type:       'handshake',
                pubkey:     keys.myPubB64,
                session_id: Number(self._sessionId),
            });
            ws.send(handshakeFrame);

            handshakeTimer = setTimeout(function() {
                if (!handshakeOkReceived) {
                    handshakeReject(new Error('handshake_ok timeout'));
                }
            }, HANDSHAKE_TIMEOUT_MS);
        };

        ws.onmessage = function(e) {
            if (!handshakeOkReceived) {
                // Expecting plaintext handshake_ok
                try {
                    var msg = (typeof e.data === 'string') ? JSON.parse(e.data) :
                              JSON.parse(new TextDecoder().decode(e.data));
                    if (msg.type === 'handshake_ok') {
                        handshakeOkReceived = true;
                        clearTimeout(handshakeTimer);
                        _log('[DaemonWS] handshake_ok (dial-back) +' + Math.round(performance.now() - _t0) + 'ms');
                        if (msg.version && typeof HC !== 'undefined') {
                            HC._daemonVersion = msg.version;
                        }
                        handshakeResolve();
                        return;
                    }
                } catch(_) {}
                _log('[DaemonWS] Unexpected pre-handshake frame, ignoring');
                return;
            }
            // READY — decrypt and feed to _innerRC
            if (!(e.data instanceof ArrayBuffer)) {
                // Text frames after handshake are out-of-band daemon messages
                // (e.g. echo_pong) sent directly via msg_tx, not through the
                // encrypted reliable channel. Deliver directly to onmessage.
                if (typeof e.data === 'string' && self.onmessage) {
                    self.onmessage({ data: e.data });
                }
                return;
            }
            var data = new Uint8Array(e.data);
            self._decrypt(data).then(function(plaintext) {
                if (self._innerRC) self._innerRC.receive(plaintext);
            }).catch(function(err) {
                _log('[DaemonWS] Decrypt failed: ' + err.message);
            });
        };

        ws.onerror = function(e) {
            if (!handshakeOkReceived) handshakeReject(new Error('WS error during handshake'));
            if (self.onerror) self.onerror(e);
        };

        ws.onclose = function(e) {
            self.readyState = 3;
            if (!handshakeOkReceived) handshakeReject(new Error('WS closed during handshake'));
            if (self.onclose) self.onclose(e);
        };

        // Step 3d: Wait for handshake_ok.
        try {
            await handshakePromise;
        } catch(e) {
            _log('[DaemonWS] Handshake failed: ' + e.message);
            self._fail(e);
            return;
        }

        // Step 3e: Set up inner ReliableChannel (peer of daemon's enc.inner).
        // This layer handles EC2-hop reliability. Frames from HC.daemonChannel
        // (end-to-end session reliability) pass through as payloads.
        if (HC.ReliableChannel) {
            self._innerRC = new HC.ReliableChannel({
                send: function(data) {
                    self._sendQueue.push(data);
                    self._flushSendQueue();
                },
                maxBuffer: 1000,
                ackInterval: 200,
            });
            self._innerRC.onmessage = function(payload) {
                // payload = raw bytes from handle_terminal_ws (binary reliable frame or JSON text)

                // HTTP-over-WS: intercept http_response frames before delivering to ws.js.
                // Daemon sends {"type":"http_response","id":"...","status":N,"body":"..."}
                // in response to {"type":"http_request",...} frames sent via send().
                if (payload.length > 2 && payload[0] !== 0xB3) {
                    var str = new TextDecoder().decode(payload);
                    if (str.indexOf('"http_response"') !== -1) {
                        try {
                            var frame = JSON.parse(str);
                            if (frame.type === 'http_response' && frame.id &&
                                    self._pendingHttpRequests[frame.id]) {
                                var pending = self._pendingHttpRequests[frame.id];
                                delete self._pendingHttpRequests[frame.id];
                                clearTimeout(pending.timer);
                                var status = frame.status || 200;
                                var bodyRaw = frame.body || '';
                                var contentType = frame.content_type || '';
                                // body_encoding: "base64" is the authoritative signal (new daemons).
                                // Fall back to content-type sniff for old daemons during rollout.
                                var isBase64 = frame.body_encoding === 'base64' ||
                                    contentType.indexOf('audio/') === 0 ||
                                    contentType.indexOf('image/') === 0 ||
                                    contentType === 'application/octet-stream';
                                var bodyText = isBase64 ? (function() {
                                    try {
                                        return new TextDecoder().decode((function() {
                                            var bin = atob(bodyRaw);
                                            var bytes = new Uint8Array(bin.length);
                                            for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
                                            return bytes;
                                        })());
                                    } catch(_) { return bodyRaw; }
                                })() : bodyRaw;
                                var bodyBytes = isBase64 ? (function() {
                                    try {
                                        var bin = atob(bodyRaw);
                                        var bytes = new Uint8Array(bin.length);
                                        for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
                                        return bytes;
                                    } catch(_) { return new Uint8Array(0); }
                                })() : null;
                                pending.resolve({
                                    ok: status >= 200 && status < 300,
                                    status: status,
                                    headers: { get: function(h) { return h.toLowerCase() === 'content-type' ? contentType : null; } },
                                    json: function() {
                                        try { return Promise.resolve(JSON.parse(bodyText)); }
                                        catch(_) { return Promise.resolve(null); }
                                    },
                                    text: function() { return Promise.resolve(bodyText); },
                                    arrayBuffer: function() {
                                        if (bodyBytes) return Promise.resolve(bodyBytes.buffer);
                                        return Promise.resolve(new TextEncoder().encode(bodyText).buffer);
                                    },
                                    blob: function() {
                                        return this.arrayBuffer().then(function(buf) {
                                            return new Blob([buf], { type: contentType || 'application/octet-stream' });
                                        });
                                    },
                                });
                                return;
                            }
                        } catch(_) {}
                    }
                }

                if (self.onmessage) {
                    // Detect binary vs text:
                    // Binary reliable frames start with 0xB3 — deliver as ArrayBuffer.
                    // JSON text (welcome, heartbeat, init, delta) — deliver as string.
                    if (payload.length >= 2 && payload[0] === 0xB3) {
                        self.onmessage({ data: payload.buffer.slice(payload.byteOffset, payload.byteOffset + payload.byteLength) });
                    } else {
                        self.onmessage({ data: new TextDecoder().decode(payload) });
                    }
                }
            };
        } else {
            // Fallback: no ReliableChannel available — raw encrypted pass-through.
            // The daemon's enc.inner won't have a JS peer, so reliability
            // is degraded. Log a warning — this shouldn't happen in prod.
            _log('[DaemonWS] WARNING: HC.ReliableChannel not found — inner RC unavailable');
            ws.onmessage = function(e) {
                if (!(e.data instanceof ArrayBuffer)) return;
                self._decrypt(new Uint8Array(e.data)).then(function(plain) {
                    if (self.onmessage) self.onmessage({ data: plain.buffer });
                }).catch(function(err) {
                    _log('[DaemonWS] Decrypt failed: ' + err.message);
                });
            };
        }

        self.readyState = 1;
        _log('[DaemonWS] READY total=+' + Math.round(performance.now() - _t0) + 'ms agent=' + self._agentId);
        if (self.onopen) self.onopen({});
    };

    // ── Public API ────────────────────────────────────────────────────

    DaemonWS.prototype.send = function(data) {
        if (this.readyState !== 1) return;

        // EC2 path: route through inner ReliableChannel
        if (this._innerRC) {
            this._innerRC.send(data);
        }
        // else: fallback (no innerRC) — encrypt and send directly
        // (degraded mode — innerRC should always be present)
    };

    /**
     * Send a raw text frame directly on the underlying WebSocket, bypassing
     * the encrypted reliable channel (_innerRC). Used for out-of-band control
     * messages (e.g. echo_ping) that must arrive at the daemon as Message::Text
     * so the daemon's text-message handler can process them. Messages sent via
     * send() go through _innerRC as encrypted binary frames and are delivered
     * to the session bridge (PTY), not the text handler.
     */
    DaemonWS.prototype.sendRaw = function(text) {
        if (this.readyState !== 1) return;
        if (this._ws && this._ws.readyState === WebSocket.OPEN) {
            this._ws.send(text);
        }
    };

    DaemonWS.prototype.close = function() {
        this._closed = true;
        this.readyState = 3;
        if (this._ws) {
            this._ws.onclose = null;
            try { this._ws.close(); } catch(_) {}
        }
        if (this._innerRC) {
            clearInterval(this._innerRC._tickTimer);
            this._innerRC = null;
        }
    };

    // ── Encryption ────────────────────────────────────────────────────

    DaemonWS.prototype._encrypt = async function(data) {
        this._sendCounter++;
        var nonce = _makeNonce(this._sendCounter);
        var ciphertext = await crypto.subtle.encrypt(
            { name: 'AES-GCM', iv: nonce, tagLength: 128 },
            this._encryptKey,
            data
        );
        var wire = new Uint8Array(12 + ciphertext.byteLength);
        wire.set(nonce, 0);
        wire.set(new Uint8Array(ciphertext), 12);
        return wire;
    };

    DaemonWS.prototype._decrypt = async function(data) {
        // data: Uint8Array — [12-byte nonce][ciphertext+tag]
        if (data.length < 12 + 16) throw new Error('Frame too short: ' + data.length);
        var nonce = data.slice(0, 12);
        var ciphertext = data.slice(12);
        var plaintext = await crypto.subtle.decrypt(
            { name: 'AES-GCM', iv: nonce, tagLength: 128 },
            this._decryptKey,
            ciphertext
        );
        return new Uint8Array(plaintext);
    };

    // Flush the async send queue — called after each enqueue.
    DaemonWS.prototype._flushSendQueue = async function() {
        if (this._flushing) return;
        this._flushing = true;
        while (this._sendQueue.length > 0) {
            var data = this._sendQueue.shift();
            try {
                var u8;
                if (typeof data === 'string') {
                    u8 = new TextEncoder().encode(data);
                } else if (data instanceof Uint8Array) {
                    u8 = data;
                } else {
                    u8 = new Uint8Array(data);
                }
                var wire = await this._encrypt(u8);
                if (this._ws && this._ws.readyState === WebSocket.OPEN) {
                    this._ws.send(wire);
                }
            } catch(e) {
                _log('[DaemonWS] Encrypt/send failed: ' + e.message);
            }
        }
        this._flushing = false;
    };

    // ── HTTP-over-WS ──────────────────────────────────────────────────
    //
    // Used by HC.daemonFetch when _daemonBase is null (EC2 encrypted path, no tunnel).
    // Serializes an HTTP request as a WS frame and returns a Promise<Response-like>.
    // The daemon intercepts the frame in handle_ec2_frame and proxies it to its local
    // HTTP server, returning {"type":"http_response","id":"...","status":N,"body":"..."}.
    //
    // Usage: HC.ws.sendHttpRequest('POST', '/api/transcription', headers, body)

    DaemonWS.prototype.sendHttpRequest = function(method, path, headers, body) {
        var self = this;
        return new Promise(function(resolve, reject) {
            if (self.readyState !== 1) {
                reject(new Error('DaemonWS not open'));
                return;
            }
            var id = _randomId();
            var timer = setTimeout(function() {
                delete self._pendingHttpRequests[id];
                reject(new Error('HTTP-over-WS timeout: ' + method + ' ' + path));
            }, 30000);
            self._pendingHttpRequests[id] = { resolve: resolve, reject: reject, timer: timer };
            var frame = JSON.stringify({
                type: 'http_request',
                id: id,
                method: method || 'GET',
                path: path,
                headers: headers || {},
                body: (body !== null && body !== undefined) ? String(body) : '',
            });
            self.send(frame);
        });
    };

    // ── Error handling ────────────────────────────────────────────────

    DaemonWS.prototype._fail = function(err) {
        this.readyState = 3;
        if (this.onerror) this.onerror({ error: err, message: err.message });
        if (this.onclose) this.onclose({ code: 4000, reason: err.message, wasClean: false });
    };

    // ── Crypto helpers ────────────────────────────────────────────────

    // ECDH key derivation given the daemon's base64 x25519 public key.
    // Returns { myPubB64, encryptKey, decryptKey } (async).
    async function _deriveKeys(serverPubB64) {
        // Decode server's long-term public key
        var serverPubBytes = _b64ToBytes(serverPubB64);
        if (serverPubBytes.length !== 32) {
            throw new Error('server pubkey must be 32 bytes, got ' + serverPubBytes.length);
        }

        var serverPubKey = await crypto.subtle.importKey(
            'raw',
            serverPubBytes,
            { name: 'X25519' },
            false,
            []
        );

        // Generate ephemeral keypair
        var myKeyPair = await crypto.subtle.generateKey(
            { name: 'X25519' },
            true,
            ['deriveBits']
        );

        // ECDH: shared secret (32 bytes)
        var sharedBits = await crypto.subtle.deriveBits(
            { name: 'X25519', public: serverPubKey },
            myKeyPair.privateKey,
            256
        );

        // HKDF-SHA256: shared_bits → 64 bytes
        var hkdfKey = await crypto.subtle.importKey(
            'raw',
            sharedBits,
            'HKDF',
            false,
            ['deriveBits']
        );
        var derivedBits = await crypto.subtle.deriveBits(
            { name: 'HKDF', hash: 'SHA-256', salt: HKDF_SALT, info: HKDF_INFO },
            hkdfKey,
            512  // 64 bytes
        );

        var derived = new Uint8Array(derivedBits);
        // [0..32]  = browser send / daemon recv key
        // [32..64] = daemon send  / browser recv key
        var browserSendBytes = derived.slice(0, 32);
        var browserRecvBytes = derived.slice(32, 64);

        var encryptKey = await crypto.subtle.importKey(
            'raw', browserSendBytes, 'AES-GCM', false, ['encrypt']);
        var decryptKey = await crypto.subtle.importKey(
            'raw', browserRecvBytes, 'AES-GCM', false, ['decrypt']);

        // Export our ephemeral public key for the handshake frame
        var myPubRaw = await crypto.subtle.exportKey('raw', myKeyPair.publicKey);
        var myPubB64 = _bytesToB64(new Uint8Array(myPubRaw));

        return { myPubB64: myPubB64, encryptKey: encryptKey, decryptKey: decryptKey };
    }

    // Nonce: [0x00,0x00,0x00,0x00, counter_u64_LE]
    // Matches Rust counter_nonce() in b3-reliable/src/crypto.rs.
    function _makeNonce(counter) {
        var nonce = new Uint8Array(12);
        var view = new DataView(nonce.buffer);
        // bytes 0-3 = 0 (already zero)
        // bytes 4-11 = counter as LE u64
        var lo = counter >>> 0;
        var hi = Math.floor(counter / 0x100000000) >>> 0;
        view.setUint32(4, lo, true);   // little-endian low 32 bits
        view.setUint32(8, hi, true);   // little-endian high 32 bits
        return nonce;
    }

    function _b64ToBytes(b64) {
        var raw = atob(b64);
        var bytes = new Uint8Array(raw.length);
        for (var i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
        return bytes;
    }

    function _bytesToB64(bytes) {
        var raw = '';
        for (var i = 0; i < bytes.length; i++) raw += String.fromCharCode(bytes[i]);
        return btoa(raw);
    }

    function _log(msg) {
        if (typeof HC !== 'undefined' && HC.log) HC.log(msg);
        else console.log('[Babel3] ' + msg);
    }

    function _randomId() {
        return Math.random().toString(36).slice(2) + Date.now().toString(36);
    }

    // ── Exports ───────────────────────────────────────────────────────

    // HC.DaemonWS — the class itself, used by ws.js via HC._pendingWs hook.
    //
    // Integration pattern (ws.js / boot.js):
    //
    //   When boot.js detects that the daemon is registered on the EC2 proxy
    //   (info.online && !info.tunnel_url), it does:
    //
    //     HC._pendingWs = new HC.DaemonWS(
    //         HC.config.agentId, HC._daemonSessionId, HC.EC2_BASE, HC.config.daemonToken);
    //     HC.connectWS();  // ws.js picks up HC._pendingWs instead of new WebSocket()
    //
    //   ws.js's connectWS() checks HC._pendingWs at the WebSocket creation point and
    //   uses it directly, preserving all existing onopen/onmessage/onclose handler logic.
    //   DaemonWS.onopen fires once EC2 handshake completes — transparent to ws.js.
    //
    //   On DaemonWS failure (daemon-key 404 or handshake timeout), onerror/onclose fires.
    //   ws.js's onclose handler calls resolveAndConnect() which retries; if the daemon
    //   is reachable, it falls back to the plain WebSocket path.

    if (typeof HC !== 'undefined') {
        HC.DaemonWS = DaemonWS;
    }

    // ── GpuWorkerWS ──────────────────────────────────────────────────────
    //
    // Drop-in for `new WebSocket(url)` in gpu.js _createGpuWsTransport.
    // Presents the same WebSocket-compatible interface as DaemonWS.
    //
    // # EC2 encrypted path
    // 1. GET /api/gpu-key?worker_id=X → { pubkey (base64 x25519), capabilities }
    // 2. Generate ephemeral x25519 keypair (Web Crypto)
    // 3. ECDH → HKDF-SHA256 → 64 bytes (same key derivation as DaemonWS)
    // 4. Connect wss://{ec2Base}/api/gpu-proxy-ws?worker_id=X&session_id=N
    // 5. Send plaintext: {"type":"handshake","pubkey":"<b64 ephemeral>","session_id":N}
    // 6. Await plaintext: {"type":"handshake_ok"} (5s timeout)
    // 7. All subsequent frames: [12-byte nonce][ciphertext + AES-GCM tag]
    //    Inner ReliableChannel (b3-reliable) handles EC2-hop reliability.
    //    Outer HC.gpuChannel (ReliableMultiTransport) handles end-to-end reliability.

    function GpuWorkerWS(workerId, sessionId, ec2Base) {
        this._workerId    = workerId;
        this._sessionId   = sessionId;
        this._ec2Base     = ec2Base;

        // WebSocket-compatible public API
        this.readyState   = 0; // CONNECTING
        this.binaryType   = 'arraybuffer';
        this.onopen       = null;
        this.onmessage    = null;
        this.onerror      = null;
        this.onclose      = null;

        // Internal
        this._ws          = null;
        this._encryptKey  = null;
        this._decryptKey  = null;
        this._sendCounter = 0;
        this._innerRC     = null;
        this._sendQueue   = [];
        this._flushing    = false;
        this._closed      = false;

        this._connect();
    }

    GpuWorkerWS.prototype._connect = async function() {
        var self = this;
        var _t0 = performance.now();
        _log('[GpuWorkerWS] connect start worker=' + self._workerId);

        // Step 1: Fetch relay pubkey.
        var keyUrl = this._ec2Base + '/api/gpu-key?worker_id=' + encodeURIComponent(this._workerId);
        var keyInfo;
        try {
            var resp = await fetch(keyUrl, { credentials: 'include', cache: 'no-store' });
            if (!resp.ok) throw new Error('HTTP ' + resp.status);
            keyInfo = await resp.json();
        } catch(e) {
            _log('[GpuWorkerWS] gpu-key fetch failed: ' + e.message);
            self._fail(new Error('gpu-key fetch failed: ' + e.message));
            return;
        }
        _log('[GpuWorkerWS] gpu-key +' + Math.round(performance.now() - _t0) + 'ms');

        // Step 2: ECDH key derivation.
        var keys;
        try {
            keys = await _deriveKeys(keyInfo.pubkey);
        } catch(e) {
            _log('[GpuWorkerWS] ECDH failed: ' + e.message);
            self._fail(e);
            return;
        }
        self._encryptKey = keys.encryptKey;
        self._decryptKey = keys.decryptKey;
        _log('[GpuWorkerWS] ECDH +' + Math.round(performance.now() - _t0) + 'ms');

        // Step 3: Connect to EC2 proxy WS.
        var wsBase = self._ec2Base.replace(/^https/, 'wss').replace(/^http(?!s)/, 'ws');
        var wsUrl = wsBase + '/api/gpu-proxy-ws?worker_id=' + encodeURIComponent(self._workerId)
                  + '&session_id=' + encodeURIComponent(self._sessionId);

        var ws;
        try {
            ws = new WebSocket(wsUrl);
            ws.binaryType = 'arraybuffer';
        } catch(e) {
            _log('[GpuWorkerWS] WS create failed: ' + e.message);
            self._fail(e);
            return;
        }
        self._ws = ws;

        // Step 4: Handshake.
        var handshakeOkReceived = false;
        var handshakeTimer = null;
        var handshakeResolve, handshakeReject;
        var handshakePromise = new Promise(function(res, rej) {
            handshakeResolve = res; handshakeReject = rej;
        });

        ws.onopen = function() {
            _log('[GpuWorkerWS] WS open (TCP+TLS) +' + Math.round(performance.now() - _t0) + 'ms');
            ws.send(JSON.stringify({
                type:       'handshake',
                pubkey:     keys.myPubB64,
                session_id: Number(self._sessionId),
            }));
            handshakeTimer = setTimeout(function() {
                if (!handshakeOkReceived) handshakeReject(new Error('handshake_ok timeout'));
            }, HANDSHAKE_TIMEOUT_MS);
        };

        ws.onmessage = function(e) {
            if (!handshakeOkReceived) {
                try {
                    var msg = (typeof e.data === 'string') ? JSON.parse(e.data)
                            : JSON.parse(new TextDecoder().decode(e.data));
                    if (msg.type === 'handshake_ok') {
                        handshakeOkReceived = true;
                        clearTimeout(handshakeTimer);
                        _log('[GpuWorkerWS] handshake_ok (dial-back) +' + Math.round(performance.now() - _t0) + 'ms');
                        handshakeResolve();
                        return;
                    }
                } catch(_) {}
                _log('[GpuWorkerWS] Unexpected pre-handshake frame, ignoring');
                return;
            }
            // READY — decrypt and feed to _innerRC.
            if (!(e.data instanceof ArrayBuffer)) {
                _log('[GpuWorkerWS] Unexpected text frame after handshake — ignored');
                return;
            }
            self._decrypt(new Uint8Array(e.data)).then(function(plaintext) {
                if (self._innerRC) self._innerRC.receive(plaintext);
            }).catch(function(err) {
                _log('[GpuWorkerWS] Decrypt failed: ' + err.message);
            });
        };

        ws.onerror = function(e) {
            if (!handshakeOkReceived) handshakeReject(new Error('WS error during handshake'));
            if (self.onerror) self.onerror(e);
        };

        ws.onclose = function(e) {
            self.readyState = 3;
            if (!handshakeOkReceived) handshakeReject(new Error('WS closed during handshake'));
            if (self.onclose) self.onclose(e);
        };

        try {
            await handshakePromise;
        } catch(e) {
            _log('[GpuWorkerWS] Handshake failed: ' + e.message);
            self._fail(e);
            return;
        }

        // Step 5: Set up inner ReliableChannel (peer of relay's EncryptedChannel.inner).
        if (HC.ReliableChannel) {
            self._innerRC = new HC.ReliableChannel({
                send: function(data) {
                    self._sendQueue.push(data);
                    self._flushSendQueue();
                },
                maxBuffer: 1000,
                ackInterval: 200,
            });
            self._innerRC.onmessage = function(payload) {
                if (self.onmessage) {
                    // Binary reliable frames (0xB3 prefix) → deliver as ArrayBuffer.
                    // Text (JSON control frames from relay) → deliver as string.
                    if (payload.length >= 2 && payload[0] === 0xB3) {
                        self.onmessage({ data: payload.buffer.slice(payload.byteOffset, payload.byteOffset + payload.byteLength) });
                    } else {
                        self.onmessage({ data: new TextDecoder().decode(payload) });
                    }
                }
            };
        } else {
            _log('[GpuWorkerWS] WARNING: HC.ReliableChannel not found — inner RC unavailable');
        }

        self.readyState = 1;
        _log('[GpuWorkerWS] READY total=+' + Math.round(performance.now() - _t0) + 'ms worker=' + self._workerId);
        if (self.onopen) self.onopen({});
    };

    // send, close, _encrypt, _decrypt, _flushSendQueue, _fail — identical to DaemonWS.
    GpuWorkerWS.prototype.send            = DaemonWS.prototype.send;
    GpuWorkerWS.prototype.close           = DaemonWS.prototype.close;
    GpuWorkerWS.prototype._encrypt        = DaemonWS.prototype._encrypt;
    GpuWorkerWS.prototype._decrypt        = DaemonWS.prototype._decrypt;
    GpuWorkerWS.prototype._flushSendQueue = DaemonWS.prototype._flushSendQueue;
    GpuWorkerWS.prototype._fail           = DaemonWS.prototype._fail;

    if (typeof HC !== 'undefined') {
        HC.GpuWorkerWS = GpuWorkerWS;
    }

})();
