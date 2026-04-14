/* reliable.js — Mosh-like reliability layer for bidirectional communication.
 *
 * Transport-agnostic: wraps any send(bytes) / onmessage(bytes) pair.
 * Same wire format as the Rust b3-reliable crate.
 *
 * Wire format:
 *   Data:   [0xB3][0x01][seq:u32LE][ts:u32LE][crc:u32LE][pri:u8][len:u32LE][payload]
 *   ACK:    [0xB3][0x02][ack_seq:u32LE][window:u16LE]
 *   Resume: [0xB3][0x03][last_ack_seq:u32LE]
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

// ── CRC32 Lookup Table (ISO 3309) ──────────────────────────────────
var _crc32Table = (function() {
    var t = new Uint32Array(256);
    for (var i = 0; i < 256; i++) {
        var c = i;
        for (var j = 0; j < 8; j++) {
            c = (c & 1) ? (0xEDB88320 ^ (c >>> 1)) : (c >>> 1);
        }
        t[i] = c;
    }
    return t;
})();

function crc32(data) {
    var crc = 0xFFFFFFFF;
    for (var i = 0; i < data.length; i++) {
        crc = _crc32Table[(crc ^ data[i]) & 0xFF] ^ (crc >>> 8);
    }
    return (crc ^ 0xFFFFFFFF) >>> 0;
}

// ── Constants ──────────────────────────────────────────────────────
var MAGIC_DATA   = 0x01;  // [0xB3][0x01]
var MAGIC_ACK    = 0x02;  // [0xB3][0x02]
var MAGIC_RESUME = 0x03;  // [0xB3][0x03]
var DATA_HEADER_SIZE = 19; // 2 + 4 + 4 + 4 + 1 + 4
var ACK_SIZE = 8;          // 2 + 4 + 2
var RESUME_SIZE = 6;       // 2 + 4

var PRIORITY_CRITICAL = 0;
var PRIORITY_BEST_EFFORT = 1;

// ── Frame Encode/Decode ────────────────────────────────────────────

function encodeData(seq, timestamp, priority, payload) {
    // payload: Uint8Array
    var checksum = crc32(payload);
    var buf = new ArrayBuffer(DATA_HEADER_SIZE + payload.length);
    var view = new DataView(buf);
    var u8 = new Uint8Array(buf);
    view.setUint8(0, 0xB3);
    view.setUint8(1, MAGIC_DATA);
    view.setUint32(2, seq, true);
    view.setUint32(6, timestamp, true);
    view.setUint32(10, checksum, true);
    view.setUint8(14, priority);
    view.setUint32(15, payload.length, true);
    u8.set(payload, DATA_HEADER_SIZE);
    return u8;
}

function encodeAck(ackSeq, window) {
    var buf = new ArrayBuffer(ACK_SIZE);
    var view = new DataView(buf);
    view.setUint8(0, 0xB3);
    view.setUint8(1, MAGIC_ACK);
    view.setUint32(2, ackSeq, true);
    view.setUint16(6, window, true);
    return new Uint8Array(buf);
}

function encodeResume(lastAckSeq) {
    var buf = new ArrayBuffer(RESUME_SIZE);
    var view = new DataView(buf);
    view.setUint8(0, 0xB3);
    view.setUint8(1, MAGIC_RESUME);
    view.setUint32(2, lastAckSeq, true);
    return new Uint8Array(buf);
}

function isReliable(data) {
    // data: Uint8Array or ArrayBuffer
    var u8 = data instanceof Uint8Array ? data : new Uint8Array(data);
    return u8.length >= 2 && u8[0] === 0xB3 && u8[1] >= 0x01 && u8[1] <= 0x03;
}

function decode(data) {
    var u8 = data instanceof Uint8Array ? data : new Uint8Array(data);
    if (u8.length < 2 || u8[0] !== 0xB3) return null;
    var view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);

    if (u8[1] === MAGIC_DATA && u8.length >= DATA_HEADER_SIZE) {
        var seq = view.getUint32(2, true);
        var timestamp = view.getUint32(6, true);
        var checksum = view.getUint32(10, true);
        var priority = view.getUint8(14);
        var payloadLen = view.getUint32(15, true);
        if (u8.length < DATA_HEADER_SIZE + payloadLen) return null;
        var payload = u8.slice(DATA_HEADER_SIZE, DATA_HEADER_SIZE + payloadLen);
        // Verify checksum
        if (crc32(payload) !== checksum) return null;
        return { type: 'data', seq: seq, timestamp: timestamp, checksum: checksum, priority: priority, payload: payload };
    }
    if (u8[1] === MAGIC_ACK && u8.length >= ACK_SIZE) {
        return { type: 'ack', ackSeq: view.getUint32(2, true), window: view.getUint16(6, true) };
    }
    if (u8[1] === MAGIC_RESUME && u8.length >= RESUME_SIZE) {
        return { type: 'resume', lastAckSeq: view.getUint32(2, true) };
    }
    return null;
}

// ── ReliableChannel ────────────────────────────────────────────────

function ReliableChannel(config) {
    config = config || {};
    this._send = config.send || function() {};
    this._maxBuffer = config.maxBuffer || 1000;
    this._ackInterval = config.ackInterval || 100;
    this._startTime = Date.now();

    // Sending
    this._nextSeq = 1;
    this._sendBuffer = [];     // [{seq, priority, encoded}]
    this._lastAckReceived = 0;

    // Receiving
    this._lastContiguous = 0;
    this._reorderBuffer = {};  // seq → {payload, seq, priority}
    this._ackPending = false;

    // Callback
    this.onmessage = null;     // (payload: Uint8Array, seq: number, priority: number)

    // Chunk reassembly
    this._chunkAssembly = [];
    this._maxFramePayload = config.maxFramePayload || 32768; // 32KB

    // Auto-tick for ACKs
    var self = this;
    this._tickTimer = setInterval(function() { self.tick(); }, this._ackInterval);
}

ReliableChannel.prototype.send = function(payload, priority) {
    if (typeof payload === 'string') {
        payload = new TextEncoder().encode(payload);
    } else if (!(payload instanceof Uint8Array)) {
        payload = new Uint8Array(payload);
    }
    if (priority === undefined) priority = PRIORITY_CRITICAL;

    var maxChunk = this._maxFramePayload || 32768; // 32KB default
    if (payload.length <= maxChunk) {
        // Standalone — prepend 0x00 tag
        var framed = new Uint8Array(1 + payload.length);
        framed[0] = 0x00;
        framed.set(payload, 1);
        this._sendOne(framed, priority);
    } else {
        // Chunk large payload
        var offset = 0;
        var first = true;
        while (offset < payload.length) {
            var end = Math.min(offset + maxChunk, payload.length);
            var isLast = (end >= payload.length);
            var tag = first ? 0x01 : (isLast ? 0x03 : 0x02);
            var chunk = payload.slice(offset, end);
            var framed = new Uint8Array(1 + chunk.length);
            framed[0] = tag;
            framed.set(chunk, 1);
            this._sendOne(framed, priority);
            offset = end;
            first = false;
        }
    }
};

ReliableChannel.prototype._sendOne = function(payload, priority) {
    var seq = this._nextSeq++;
    var ts = (Date.now() - this._startTime) & 0xFFFFFFFF;
    var encoded = encodeData(seq, ts, priority, payload);

    this._sendBuffer.push({ seq: seq, priority: priority, encoded: encoded });

    while (this._sendBuffer.length > this._maxBuffer) {
        var front = this._sendBuffer[0];
        if (front.priority === PRIORITY_BEST_EFFORT || front.seq <= this._lastAckReceived) {
            this._sendBuffer.shift();
        } else {
            break;
        }
    }

    this._send(encoded);
};

ReliableChannel.prototype.receive = function(data) {
    var frame = decode(data);
    if (!frame) return; // Not a reliable frame or corrupt

    if (frame.type === 'data') {
        this._receiveData(frame);
    } else if (frame.type === 'ack') {
        this._receiveAck(frame.ackSeq);
    } else if (frame.type === 'resume') {
        this._handleResume(frame.lastAckSeq);
    }
};

ReliableChannel.prototype._receiveData = function(frame) {
    this._ackPending = true;

    // Duplicate
    if (frame.seq <= this._lastContiguous) return;

    // Next expected
    if (frame.seq === this._lastContiguous + 1) {
        this._lastContiguous = frame.seq;
        this._processChunk(frame.payload, frame.seq, frame.priority);

        // Drain reorder buffer
        while (this._reorderBuffer[this._lastContiguous + 1]) {
            var next = this._reorderBuffer[this._lastContiguous + 1];
            delete this._reorderBuffer[this._lastContiguous + 1];
            this._lastContiguous = next.seq;
            this._processChunk(next.payload, next.seq, next.priority);
        }
        return;
    }

    // Out of order — buffer (raw payload with chunk tag intact)
    if (!this._reorderBuffer[frame.seq]) {
        this._reorderBuffer[frame.seq] = { payload: frame.payload, seq: frame.seq, priority: frame.priority };
    }
};

// Process chunk tag: 0x00=standalone, 0x01=first, 0x02=middle, 0x03=last
ReliableChannel.prototype._processChunk = function(payload, seq, priority) {
    if (!payload || payload.length === 0) return;
    var tag = payload[0];
    var data = payload.slice(1);
    if (tag === 0x00) {
        // Standalone
        if (this.onmessage) this.onmessage(data, seq, priority);
    } else if (tag === 0x01) {
        // First chunk
        this._chunkAssembly = [data];
    } else if (tag === 0x02) {
        // Middle chunk
        this._chunkAssembly.push(data);
    } else if (tag === 0x03) {
        // Last chunk — assemble and deliver
        this._chunkAssembly.push(data);
        var totalLen = 0;
        for (var i = 0; i < this._chunkAssembly.length; i++) totalLen += this._chunkAssembly[i].length;
        var assembled = new Uint8Array(totalLen);
        var off = 0;
        for (var i = 0; i < this._chunkAssembly.length; i++) {
            assembled.set(this._chunkAssembly[i], off);
            off += this._chunkAssembly[i].length;
        }
        this._chunkAssembly = [];
        if (this.onmessage) this.onmessage(assembled, seq, priority);
    } else {
        // Unknown tag — deliver as-is (backward compat)
        if (this.onmessage) this.onmessage(payload, seq, priority);
    }
};

ReliableChannel.prototype._receiveAck = function(ackSeq) {
    if (ackSeq > this._lastAckReceived) {
        this._lastAckReceived = ackSeq;
        while (this._sendBuffer.length > 0 && this._sendBuffer[0].seq <= ackSeq) {
            this._sendBuffer.shift();
        }
        this._dupAckCount = 0;
    } else if (ackSeq === this._lastAckReceived && this._sendBuffer.length > 0) {
        // Duplicate ACK — remote side stuck. After 3, retransmit unacked critical frames.
        this._dupAckCount = (this._dupAckCount || 0) + 1;
        if (this._dupAckCount >= 3) {
            this._dupAckCount = 0;
            for (var i = 0; i < this._sendBuffer.length; i++) {
                var m = this._sendBuffer[i];
                if (m.seq > ackSeq && m.priority === PRIORITY_CRITICAL) {
                    this._send(m.encoded);
                }
            }
        }
    }
};

ReliableChannel.prototype._handleResume = function(lastAckSeq) {
    this._lastAckReceived = lastAckSeq;

    // Case 1: Remote side is ahead (e.g. relay restarted, browser remembers high seq).
    // Jump outbound seq past remote's last_ack. Also reset inbound — remote's
    // outbound seq will restart from 1, we must accept it.
    if (lastAckSeq >= this._nextSeq) {
        if (typeof HC !== 'undefined' && HC.log) HC.log('[Reliable] Resume seq ahead (' + lastAckSeq + ' >= ' + this._nextSeq + ') — resetting both directions');
        this._nextSeq = lastAckSeq + 1;
        this._sendBuffer = [];
        this._lastContiguous = 0;
        this._reorderBuffer = {};
        this._chunkBuffer = null;
        return;
    }

    // Case 2: Remote side is behind and our send buffer can't satisfy it.
    // The frames it needs are gone — reset outbound to remote's state.
    // Also reset inbound — remote is fresh, its outbound seq restarts.
    var oldestBuffered = this._sendBuffer.length > 0 ? this._sendBuffer[0].seq : null;
    if (lastAckSeq < this._nextSeq && (oldestBuffered === null || oldestBuffered > lastAckSeq + 1)) {
        if (typeof HC !== 'undefined' && HC.log) HC.log('[Reliable] Resume from stale remote (' + lastAckSeq + ' << ' + this._nextSeq + ') — resetting both directions');
        this._nextSeq = lastAckSeq + 1;
        this._lastContiguous = lastAckSeq;
        this._sendBuffer = [];
        this._reorderBuffer = {};
        this._chunkBuffer = null;
        return;
    }

    // Normal case: replay unacked critical messages
    for (var i = 0; i < this._sendBuffer.length; i++) {
        var m = this._sendBuffer[i];
        if (m.seq > lastAckSeq && m.priority === PRIORITY_CRITICAL) {
            this._send(m.encoded);
        }
    }
    // Clean acked
    while (this._sendBuffer.length > 0 && this._sendBuffer[0].seq <= lastAckSeq) {
        this._sendBuffer.shift();
    }
};

ReliableChannel.prototype.setTransport = function(sendFn) {
    this._send = sendFn;
};

ReliableChannel.prototype.sendResume = function() {
    this._send(encodeResume(this._lastContiguous));
};

ReliableChannel.prototype.lastContiguousReceived = function() {
    return this._lastContiguous;
};

ReliableChannel.prototype.tick = function() {
    if (this._ackPending) {
        var window = Math.max(0, this._maxBuffer - Object.keys(this._reorderBuffer).length);
        this._send(encodeAck(this._lastContiguous, window));
        this._ackPending = false;
    }
};

ReliableChannel.prototype.unackedCount = function() {
    return this._sendBuffer.length;
};

ReliableChannel.prototype.reorderCount = function() {
    return Object.keys(this._reorderBuffer).length;
};

ReliableChannel.prototype.reset = function() {
    this._nextSeq = 1;
    this._sendBuffer = [];
    this._lastAckReceived = 0;
    this._lastContiguous = 0;
    this._reorderBuffer = {};
    this._ackPending = false;
    this._chunkAssembly = [];
};

ReliableChannel.prototype.destroy = function() {
    if (this._tickTimer) clearInterval(this._tickTimer);
    this._tickTimer = null;
};

// ── ReliableMultiTransport ─────────────────────────────────────────
// Multi-transport management over a single ReliableChannel.
// Mirrors the Rust MultiTransport in b3-reliable/src/multi.rs.
//
// Each transport adapter returns a TransportHandle:
//   { send(bytes), close(), onmessage, onclose, readyState }
//
// The channel manages connect/reconnect, priority routing, failover,
// and transport toggling. Application code just calls send()/onmessage.

/**
 * @param {Object} config
 * @param {Object} config.transports — { name: { connect: fn() → TransportHandle, priority: number } }
 * @param {string} config.allowedTransports — 'auto', 'ws', 'rtc', or transport name (default: 'auto')
 * @param {Function} config.rtcBlockCheck — returns true if RTC should be deferred (e.g. audio locked)
 * @param {number} config.maxBuffer — max unacked messages (default: 1000)
 * @param {number} config.ackInterval — ACK tick interval in ms (default: 200)
 * @param {Function} config.onconnect — called when any transport connects (for auth handshake)
 * @param {Function} config.onmessage — called with (payload, seq, priority) for received data
 */
function ReliableMultiTransport(config) {
    config = config || {};
    var self = this;

    // Transport definitions (immutable after construction)
    this._transportDefs = {};  // name → { connect, priority }
    var defs = config.transports || {};
    for (var name in defs) {
        if (defs.hasOwnProperty(name)) {
            this._transportDefs[name] = {
                connect: defs[name].connect,
                priority: defs[name].priority || 0
            };
        }
    }

    // Transport state
    this._handles = {};        // name → TransportHandle (when connected)
    this._reconnectTimers = {}; // name → timeout ID
    this._reconnectBackoff = {}; // name → current backoff ms
    this._connectFailures = {};  // name → consecutive connect failure count (reset after 5s stable)
    this._allowed = config.allowedTransports || 'auto';
    this._rtcBlockCheck = config.rtcBlockCheck || function() { return false; };
    this._rtcBlockTimer = null;

    // Callbacks
    this.onconnect = config.onconnect || null;
    this.onmessage = config.onmessage || null;
    this.ondisconnect = null;   // (transportName)
    this.ontransportchange = null; // (newActiveName)

    // Inner ReliableChannel — one channel, continuous seq numbers
    this._reliable = new ReliableChannel({
        send: function(encoded) { self._routeFrame(encoded); },
        maxBuffer: config.maxBuffer || 1000,
        ackInterval: config.ackInterval || 200
    });

    // Wire up inner channel's onmessage
    this._reliable.onmessage = function(payload, seq, priority) {
        if (self.onmessage) self.onmessage(payload, seq, priority);
    };

    // Start connecting allowed transports
    this._connectAllowed();

    // RTC block check timer — recheck every 2s
    this._rtcBlockTimer = setInterval(function() { self._checkRtcBlock(); }, 2000);

    // Transport-level heartbeat — kills zombie transports.
    // Any transport that hasn't received data in 10 seconds is dead.
    // Catches: SCTP masking, tunnel zombies, silent disconnects, future unknowns.
    this._healthInterval = setInterval(function() {
        for (var name in self._handles) {
            var h = self._handles[name];
            // Monitor transports that haven't received data in 15 seconds.
            // DO NOT disconnect — protocol-level liveness (WS ping/pong, ICE state)
            // handles cleanup. This log is observability only.
            if (h._lastRecv && (Date.now() - h._lastRecv > 15000)) {
                // Protocol-level keepalives handle liveness — no log needed
            }
            // Send keepalive ACK on ALL transports — keeps idle fallback transports
            // alive by ensuring the remote side has something to ACK back.
            try { self._reliable.tick(); } catch(e) {}
        }
    }, 5000);
}

// ── Public API ────────────────────────────────────────────────────

/** Send payload via best transport. */
ReliableMultiTransport.prototype.send = function(payload, priority) {
    this._reliable.send(payload, priority);
};

/** Note that data was received on a transport — resets heartbeat timer.
 *  Call this from external receive handlers (e.g. rtc.js, ws.js) that
 *  bypass the internal handle.onmessage path. */
ReliableMultiTransport.prototype.noteRecv = function(transportName) {
    var h = this._handles[transportName];
    if (h) h._lastRecv = Date.now();
};

/** Feed incoming data from any transport. */
ReliableMultiTransport.prototype.receive = function(data) {
    this._reliable.receive(data);
};

/** Change allowed transports at runtime. */
ReliableMultiTransport.prototype.setAllowedTransports = function(mode) {
    var prev = this._allowed;
    this._allowed = mode;
    if (prev !== mode) {
        // Disconnect disallowed transports
        for (var name in this._handles) {
            if (!this._isAllowed(name)) {
                this._disconnectTransport(name);
            }
        }
        // Connect newly allowed ones
        this._connectAllowed();
        // Resync with remote on the surviving transport. The remote may have
        // reset its inbound channel when the other transport disconnected.
        // Resume tells the remote our current state so both sides can resync.
        this._reliable.sendResume();
    }
};

/** Get status of all transports. */
ReliableMultiTransport.prototype.status = function() {
    var result = {};
    for (var name in this._transportDefs) {
        var handle = this._handles[name];
        if (handle && handle.readyState === 'open') {
            result[name] = 'connected';
        } else if (!this._isAllowed(name)) {
            result[name] = 'disabled';
        } else if (name === 'rtc' && this._rtcBlockCheck()) {
            result[name] = 'blocked';
        } else if (this._reconnectTimers[name]) {
            result[name] = 'reconnecting';
        } else {
            result[name] = 'disconnected';
        }
    }
    result.active = this._activeName();
    return result;
};

/** Clean up all transports and timers. */
ReliableMultiTransport.prototype.destroy = function() {
    for (var name in this._handles) {
        this._disconnectTransport(name);
    }
    for (var name2 in this._reconnectTimers) {
        clearTimeout(this._reconnectTimers[name2]);
    }
    this._reconnectTimers = {};
    if (this._rtcBlockTimer) clearInterval(this._rtcBlockTimer);
    this._rtcBlockTimer = null;
    if (this._healthInterval) clearInterval(this._healthInterval);
    this._healthInterval = null;
    this._reliable.destroy();
};

// ── Internal: Transport Management ────────────────────────────────

/** Route a reliable frame to the best active transport. */
ReliableMultiTransport.prototype._routeFrame = function(encoded) {
    // Track that user is actively trying to send (for upstream health)
    if (this._lastUpstreamSendTried !== undefined) this._lastUpstreamSendTried = Date.now();
    var name = this._activeName();
    if (name && this._handles[name]) {
        this._handles[name].send(encoded);
        // Mark upstream healthy on successful send
        if (this._lastUpstreamSendOk !== undefined) this._lastUpstreamSendOk = Date.now();
    }
    // If no transport, frame stays in send buffer for replay on reconnect
};

/** Find the highest-priority connected transport name.
 *  A transport must have received at least one frame (_lastRecv set)
 *  before it can be promoted to active. This prevents hollow transports
 *  (connected but not delivering data) from stealing traffic. */
ReliableMultiTransport.prototype._activeName = function() {
    var best = null;
    var bestPri = Infinity;
    for (var name in this._handles) {
        var handle = this._handles[name];
        if (handle && handle.readyState === 'open') {
            // Don't promote a transport that has never received data —
            // it may be a black hole (e.g. RTC connected but datachannel broken).
            // Fall back to a lower-priority transport that IS delivering.
            if (!handle._lastRecv) continue;
            var pri = this._transportDefs[name] ? this._transportDefs[name].priority : 99;
            if (pri < bestPri) {
                bestPri = pri;
                best = name;
            }
        }
    }
    return best;
};

/** Check if a transport is allowed by current mode. */
ReliableMultiTransport.prototype._isAllowed = function(name) {
    if (this._allowed === 'auto') return true;
    return this._allowed === name;
};

/** Connect all allowed transports that aren't already connected. */
ReliableMultiTransport.prototype._connectAllowed = function() {
    for (var name in this._transportDefs) {
        if (this._isAllowed(name) && !this._handles[name] && !this._reconnectTimers[name]) {
            this._connectTransport(name);
        }
    }
};

/** Connect a single transport by name. */
ReliableMultiTransport.prototype._connectTransport = function(name) {
    var self = this;

    // RTC block check
    if (name === 'rtc' && this._rtcBlockCheck()) {
        return; // Will retry on _checkRtcBlock timer
    }

    if (!this._isAllowed(name)) return;

    var def = this._transportDefs[name];
    if (!def || !def.connect) return;

    var handle;
    try {
        handle = def.connect();
    } catch (e) {
        this._scheduleReconnect(name);
        return;
    }

    if (!handle) {
        this._scheduleReconnect(name);
        return;
    }

    // Wire up handle callbacks
    var prevActive = this._activeName();

    handle._lastRecv = Date.now(); // Initialize — heartbeat won't kill on first connect

    handle.onmessage = function(data) {
        handle._lastRecv = Date.now();
        self._reliable.receive(data);
    };

    handle.onclose = function(reason) {
        if (handle._stabilityTimer) clearTimeout(handle._stabilityTimer);
        var duration = handle._lastRecv ? Math.round((Date.now() - (handle._connectedAt || handle._lastRecv)) / 1000) : 0;
        var framesIn = self._reliable._lastContiguous;
        var framesOut = self._reliable._nextSeq - 1;
        var disconnectReason = reason || handle._disconnectReason || 'unknown';

        // Emit structured disconnect event via telemetry path.
        // Rate-limited: at most 1 disconnect event per 5 seconds per channel.
        // Without this, a GPU reconnect storm (15+ channel reinitializations in 1s)
        // floods the daemon WS with disconnect telemetry, killing the daemon connection.
        var now = Date.now();
        if (self._telemetrySend && (!self._lastDisconnectReport || (now - self._lastDisconnectReport >= 5000))) {
            self._lastDisconnectReport = now;
            try {
                self._telemetrySend(JSON.stringify({
                    type: 'disconnect',
                    ts: Math.floor(now / 1000),
                    transport: name,
                    reason: disconnectReason,
                    duration_s: duration,
                    frames_in: framesIn,
                    frames_out: framesOut
                }));
            } catch(e) {}
        }

        delete self._handles[name];

        // Track consecutive connect failures for escalation.
        // A flash-connect (connected but died within the 5s stability window) counts
        // as a failure — the stability timer never fired, so the counter was not reset.
        // Reset only happens after 5s stable (see _onTransportOpen stability timer).
        self._connectFailures[name] = (self._connectFailures[name] || 0) + 1;
        // Fire onconnectfail exactly at threshold — not on every subsequent failure.
        // Prevents repeated user-visible warnings for the same persistent dead tunnel.
        if (self._connectFailures[name] === 3 && self.onconnectfail) {
            self.onconnectfail(name, self._connectFailures[name]);
        }

        if (self.ondisconnect) self.ondisconnect(name, disconnectReason);
        var newActive = self._activeName();
        if (newActive !== prevActive && self.ontransportchange) {
            self.ontransportchange(newActive);
        }
        // Schedule reconnect if still allowed
        if (self._isAllowed(name)) {
            self._scheduleReconnect(name);
        }
    };

    // Wait for readyState to become 'open' if transport reports it
    if (handle.readyState === 'open') {
        this._onTransportOpen(name, handle, prevActive);
    } else {
        // Transport is connecting — watch for open via polling or callback
        handle._openCheck = setInterval(function() {
            if (handle.readyState === 'open') {
                clearInterval(handle._openCheck);
                handle._openCheck = null;
                self._onTransportOpen(name, handle, prevActive);
            } else if (handle.readyState === 'closed') {
                clearInterval(handle._openCheck);
                handle._openCheck = null;
                // Will be handled by onclose
            }
        }, 50);
    }

    this._handles[name] = handle;
};

/** Called when a transport transitions to 'open'. */
ReliableMultiTransport.prototype._onTransportOpen = function(name, handle, prevActive) {
    // Ensure _lastRecv exists — heartbeat checks this to determine liveness.
    // Some code paths (rtc.js manual registration) set it in the handle literal,
    // others (_connectTransport) set it before calling us. This catches any path
    // that missed it (e.g., after iOS tab eviction + page reload).
    handle._lastRecv = handle._lastRecv || Date.now();
    handle._connectedAt = Date.now();

    // Don't reset backoff immediately — wait for stability.
    // If the connection dies within 5s, backoff stays elevated (exponential).
    // Only a stable connection (>5s) resets backoff to 0.
    var self = this;
    if (handle._stabilityTimer) clearTimeout(handle._stabilityTimer);
    handle._stabilityTimer = setTimeout(function() {
        self._reconnectBackoff[name] = 0;
        self._connectFailures[name] = 0; // Stable connection — reset failure counter
    }, 5000);

    // Fire onconnect (for auth handshake)
    if (this.onconnect) this.onconnect(name, handle);

    // Reset ALL seq state on WS transport open (full reconnect) BEFORE sending Resume.
    // Without this, data sent between our Resume and the remote's Resume-back
    // goes out at old-seq (e.g., seq=105) while the remote expects seq=1 after
    // its stale reset. The remote's reorder buffer fills with unreachable frames,
    // then the Resume clears them — TTS jobs lost in the gap.
    // Must also reset _lastContiguous and _reorderBuffer (inbound state) — otherwise
    // sendResume() sends Resume(old_lastContiguous), the daemon's handle_resume jumps
    // next_seq past where we can receive, and frames pile up in our reorder buffer
    // forever (deadlock).
    // Only WS — RTC is a transport upgrade within an existing session, seq preserved.
    if (name === 'ws') {
        this._reliable._nextSeq = 1;
        this._reliable._sendBuffer = [];
        this._reliable._lastAckReceived = 0;
        this._reliable._lastContiguous = 0;
        this._reliable._reorderBuffer = {};
    }

    // Send resume to remote side
    this._reliable.sendResume();

    // Check if active transport changed
    var newActive = this._activeName();
    if (newActive !== prevActive && this.ontransportchange) {
        this.ontransportchange(newActive);
    }
};

/** Disconnect a transport by name. */
ReliableMultiTransport.prototype._disconnectTransport = function(name) {
    var handle = this._handles[name];
    if (handle) {
        if (handle._openCheck) clearInterval(handle._openCheck);
        if (handle._stabilityTimer) clearTimeout(handle._stabilityTimer);
        try { handle.close(); } catch (e) {}
        delete this._handles[name];
    }
    if (this._reconnectTimers[name]) {
        clearTimeout(this._reconnectTimers[name]);
        delete this._reconnectTimers[name];
    }
};

/** Schedule reconnect with exponential backoff. */
ReliableMultiTransport.prototype._scheduleReconnect = function(name) {
    var self = this;
    if (this._reconnectTimers[name]) return; // Already scheduled

    var backoff = this._reconnectBackoff[name] || 1000;
    this._reconnectBackoff[name] = Math.min(backoff * 2, 30000); // Max 30s

    this._reconnectTimers[name] = setTimeout(function() {
        delete self._reconnectTimers[name];
        if (self._isAllowed(name)) {
            self._connectTransport(name);
        }
    }, backoff);
};

/** Periodic check: if RTC block cleared, try connecting. */
ReliableMultiTransport.prototype._checkRtcBlock = function() {
    if (!this._rtcBlockCheck() && this._isAllowed('rtc') && !this._handles['rtc'] && !this._reconnectTimers['rtc']) {
        this._connectTransport('rtc');
    }
};

// ── Upstream Health Tracking ──────────────────────────────────────
// Detects upstream stalls (browser → daemon path dead while daemon → browser works).
// Uses application-level echo ping to test full round-trip, auto-reconnects on stall.

ReliableMultiTransport.prototype.startUpstreamHealth = function() {
    var self = this;
    this._lastUpstreamSendOk = Date.now();
    this._lastUpstreamAttempt = 0;
    this._lastUpstreamSendTried = 0;  // last time user tried to send (not just idle)
    this._upstreamFailCount = 0;
    this._pendingEchoPing = null;

    this._upstreamHealthTimer = setInterval(function() {
        var now = Date.now();
        var staleSec = (now - self._lastUpstreamSendOk) / 1000;
        var wsHandle = self._handles['ws'];
        var wsState = wsHandle ? wsHandle.readyState : 'none';

        // Only probe upstream if user has actually tried to send recently.
        // Idle sessions (no upstream sends) should NOT trigger reconnects —
        // downstream working + no upstream attempts = healthy idle connection.
        var hasRecentSendAttempt = self._lastUpstreamSendTried > 0 &&
            (now - self._lastUpstreamSendTried < 60000);

        // Check pending echo ping timeout (10s — generous to handle slow daemons)
        if (self._pendingEchoPing && (now - self._pendingEchoPing.sent > 10000)) {
            var log = (typeof HC !== 'undefined' && HC.log) ? HC.log : console.log;
            log('[Upstream] Echo ping timeout (10s) — triggering reconnect');
            self._pendingEchoPing = null;
            self._upstreamFailCount++;
            if (wsHandle) {
                self._disconnectTransport('ws');
                self._scheduleReconnect('ws');
            }
            return;
        }

        // If user tried to send and upstream stale for >15s, probe with echo ping.
        // Must use sendRaw (plain text WS frame) — daemon handles echo_ping as
        // Message::Text. Sending via wsHandle.send() routes through DaemonWS._innerRC
        // (encrypted reliable binary), delivering as PTY data, not to the text handler.
        if (hasRecentSendAttempt && staleSec > 15 && !self._pendingEchoPing && wsHandle && wsHandle.readyState === 'open') {
            var pingId = now.toString(36);
            try {
                var pingJson = JSON.stringify({ type: 'echo_ping', id: pingId });
                if (wsHandle.sendRaw) {
                    wsHandle.sendRaw(pingJson);
                } else {
                    wsHandle.send(new TextEncoder().encode(pingJson));
                }
                self._pendingEchoPing = { id: pingId, sent: now };
                self._lastUpstreamAttempt = now;
            } catch(e) {
                self._upstreamFailCount++;
            }
        }

        // Log upstream health when stale AND user is trying to send
        if (hasRecentSendAttempt && staleSec > 10) {
            var log2 = (typeof HC !== 'undefined' && HC.log) ? HC.log : console.log;
            log2('[Upstream] health: stale=' + staleSec.toFixed(0) + 's' +
                ' fails=' + self._upstreamFailCount +
                ' wsState=' + wsState +
                ' pendingPing=' + JSON.stringify(self._pendingEchoPing));
        }

        // Auto-reconnect if user tried to send and upstream stale for >30s
        if (hasRecentSendAttempt && staleSec > 30 && self._lastUpstreamAttempt < self._lastUpstreamSendOk) {
            var log3 = (typeof HC !== 'undefined' && HC.log) ? HC.log : console.log;
            log3('[Upstream] Stale for ' + staleSec.toFixed(0) + 's — triggering reconnect');
            self._upstreamFailCount++;
            if (wsHandle) {
                self._disconnectTransport('ws');
                self._scheduleReconnect('ws');
            }
        }
    }, 10000);
};

/** Call when upstream send succeeds (e.g. after reliable ACK or WS send completes). */
ReliableMultiTransport.prototype.noteUpstreamSendOk = function() {
    this._lastUpstreamSendOk = Date.now();
};

/** Handle echo_pong from daemon — clears pending ping, updates upstream health. */
ReliableMultiTransport.prototype.handleEchoPong = function(id) {
    if (this._pendingEchoPing && this._pendingEchoPing.id === id) {
        var rtt = Date.now() - this._pendingEchoPing.sent;
        this._pendingEchoPing = null;
        this._lastUpstreamSendOk = Date.now();
        var log = (typeof HC !== 'undefined' && HC.log) ? HC.log : console.log;
        log('[Upstream] Echo pong received: rtt=' + rtt + 'ms');
    }
};

/** Get upstream health diagnostics for browser_eval inspection. */
ReliableMultiTransport.prototype.upstreamHealth = function() {
    return {
        lastSendOk: this._lastUpstreamSendOk || 0,
        lastSendTried: this._lastUpstreamSendTried || 0,
        staleSec: ((Date.now() - (this._lastUpstreamSendOk || Date.now())) / 1000).toFixed(1),
        failCount: this._upstreamFailCount || 0,
        pendingPing: this._pendingEchoPing,
        wsState: this._handles['ws'] ? this._handles['ws'].readyState : 'none'
    };
};

// ── Telemetry ─────────────────────────────────────────────────────
// Periodic structured heartbeat for pipeline observability.
// Ships via the raw WS text path (NOT the reliable channel) so it
// doesn't consume seq numbers or get buffered/replayed.

/** Get a telemetry snapshot of the channel state.
 *  Extra counters (e.g. jobs_submitted, jobs_completed) can be set on
 *  the instance as this._extraTelemetry = { key: value, ... }.
 */
ReliableMultiTransport.prototype.telemetrySnapshot = function() {
    var r = this._reliable;
    var active = this._activeName();
    var handle = active ? this._handles[active] : null;
    var lastRecvAge = handle && handle._lastRecv ? (Date.now() - handle._lastRecv) : -1;

    var snap = {
        transport: active || 'none',
        frames_in: r._lastContiguous,
        frames_out: r._nextSeq - 1,
        last_contiguous: r._lastContiguous,
        reorder_buf: Object.keys(r._reorderBuffer).length,
        send_buf: r._sendBuffer.length,
        reconnects: this._reconnectCount || 0,
        last_recv_age_ms: lastRecvAge
    };

    // Merge any extra counters set by the consumer (e.g. GPU job tracking)
    if (this._extraTelemetry) {
        for (var k in this._extraTelemetry) {
            if (this._extraTelemetry.hasOwnProperty(k)) snap[k] = this._extraTelemetry[k];
        }
    }

    return snap;
};

/** Start periodic telemetry emission (call once after construction).
 *  @param {string} channelName — 'daemon' or 'gpu'
 *  @param {Function} sendFn — function(jsonString) to ship telemetry out-of-band
 */
ReliableMultiTransport.prototype.startTelemetry = function(channelName, sendFn) {
    var self = this;
    this._telemetrySend = sendFn;  // Also used by disconnect event logging
    if (this._telemetryTimer) clearInterval(this._telemetryTimer);

    this._telemetryTimer = setInterval(function() {
        try {
            var snap = self.telemetrySnapshot();
            var msg = {};
            msg[channelName] = snap;
            msg.type = 'telemetry';
            msg.ts = Math.floor(Date.now() / 1000);
            sendFn(JSON.stringify(msg));
        } catch(e) {}
    }, 10000);
};

// Track reconnects — increment on every transport open
(function() {
    var origOnOpen = ReliableMultiTransport.prototype._onTransportOpen;
    ReliableMultiTransport.prototype._onTransportOpen = function(name, handle, prevActive) {
        this._reconnectCount = (this._reconnectCount || 0) + 1;
        return origOnOpen.call(this, name, handle, prevActive);
    };
})();

// Clean up telemetry and upstream health timers on destroy
(function() {
    var origDestroy = ReliableMultiTransport.prototype.destroy;
    ReliableMultiTransport.prototype.destroy = function() {
        if (this._telemetryTimer) clearInterval(this._telemetryTimer);
        this._telemetryTimer = null;
        if (this._upstreamHealthTimer) clearInterval(this._upstreamHealthTimer);
        this._upstreamHealthTimer = null;
        return origDestroy.call(this);
    };
})();

// ── Exports ────────────────────────────────────────────────────────
// Attach to HC namespace for browser, or export for Node.js testing
if (typeof HC !== 'undefined') {
    HC.ReliableChannel = ReliableChannel;
    HC.ReliableMultiTransport = ReliableMultiTransport;
    HC.reliableCrc32 = crc32;
    HC.reliableIsReliable = isReliable;
    HC.reliableDecode = decode;
    HC.PRIORITY_CRITICAL = PRIORITY_CRITICAL;
    HC.PRIORITY_BEST_EFFORT = PRIORITY_BEST_EFFORT;
} else if (typeof module !== 'undefined') {
    module.exports = { ReliableChannel: ReliableChannel, ReliableMultiTransport: ReliableMultiTransport, crc32: crc32, isReliable: isReliable, decode: decode, encodeData: encodeData, encodeAck: encodeAck, encodeResume: encodeResume, PRIORITY_CRITICAL: PRIORITY_CRITICAL, PRIORITY_BEST_EFFORT: PRIORITY_BEST_EFFORT };
}
