/* test-multi-transport.js — Node.js tests for ReliableMultiTransport */
'use strict';

var R = require('../browser/js/reliable.js');
var ReliableMultiTransport = R.ReliableMultiTransport;
var ReliableChannel = R.ReliableChannel;
var PRIORITY_CRITICAL = R.PRIORITY_CRITICAL;
var PRIORITY_BEST_EFFORT = R.PRIORITY_BEST_EFFORT;

var passed = 0;
var failed = 0;

function assert(cond, msg) {
    if (!cond) {
        failed++;
        console.error('  FAIL: ' + msg);
        console.trace();
    } else {
        passed++;
    }
}

function assertEq(a, b, msg) {
    if (a !== b) {
        failed++;
        console.error('  FAIL: ' + msg + ' — expected ' + JSON.stringify(b) + ', got ' + JSON.stringify(a));
    } else {
        passed++;
    }
}

// ── Mock Transport ────────────────────────────────────────────────

function MockTransport(opts) {
    opts = opts || {};
    this.readyState = opts.openImmediately !== false ? 'open' : 'connecting';
    this.sent = [];
    this.onmessage = null;
    this.onclose = null;
    this.closed = false;
}

MockTransport.prototype.send = function(data) {
    this.sent.push(data);
};

MockTransport.prototype.close = function() {
    this.readyState = 'closed';
    this.closed = true;
    if (this.onclose) this.onclose();
};

MockTransport.prototype.simulateOpen = function() {
    this.readyState = 'open';
};

MockTransport.prototype.simulateClose = function() {
    this.readyState = 'closed';
    if (this.onclose) this.onclose();
};

MockTransport.prototype.simulateMessage = function(data) {
    if (this.onmessage) this.onmessage(data);
};

// ── Tests ─────────────────────────────────────────────────────────

console.log('test_single_transport');
(function() {
    var transport = null;
    var received = [];

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    transport = new MockTransport();
                    return transport;
                },
                priority: 1
            }
        },
        onmessage: function(payload, seq, priority) {
            received.push({ payload: new TextDecoder().decode(payload), seq: seq });
        }
    });

    assert(transport !== null, 'WS transport should be connected');
    assertEq(transport.readyState, 'open', 'WS should be open');

    multi.send('hello', PRIORITY_CRITICAL);
    assertEq(transport.sent.length, 1, 'One frame sent via WS');

    // Feed the frame back to a receiver
    var receiverTransport = null;
    var receiverMsgs = [];
    var receiver = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    receiverTransport = new MockTransport();
                    return receiverTransport;
                },
                priority: 1
            }
        },
        onmessage: function(payload, seq) {
            receiverMsgs.push({ payload: new TextDecoder().decode(payload), seq: seq });
        }
    });

    receiver.receive(transport.sent[0]);
    assertEq(receiverMsgs.length, 1, 'Receiver got one message');
    assertEq(receiverMsgs[0].payload, 'hello', 'Payload matches');
    assertEq(receiverMsgs[0].seq, 1, 'Seq is 1');

    multi.destroy();
    receiver.destroy();
})();

console.log('test_priority_routing');
(function() {
    var wsTransport = null;
    var rtcTransport = null;

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            },
            rtc: {
                connect: function() {
                    rtcTransport = new MockTransport();
                    return rtcTransport;
                },
                priority: 0  // RTC preferred
            }
        }
    });

    multi.send('goes to rtc', PRIORITY_CRITICAL);
    assert(rtcTransport.sent.length >= 1, 'RTC gets the data frame');
    // WS may get a resume frame from initial connect, but not the data
    var wsDataFrames = wsTransport.sent.filter(function(f) {
        var u8 = f instanceof Uint8Array ? f : new Uint8Array(f);
        return u8[1] === 0x01; // Data frame
    });
    assertEq(wsDataFrames.length, 0, 'WS should not get data frames');

    multi.destroy();
})();

console.log('test_failover_on_close');
(function() {
    var wsTransport = null;
    var rtcTransport = null;

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            },
            rtc: {
                connect: function() {
                    rtcTransport = new MockTransport();
                    return rtcTransport;
                },
                priority: 0
            }
        }
    });

    // Send via RTC
    multi.send('via rtc', PRIORITY_CRITICAL);
    var rtcCount = rtcTransport.sent.length;
    assert(rtcCount >= 1, 'RTC got data');

    // RTC dies
    rtcTransport.simulateClose();

    // Next send should go to WS
    var wsCountBefore = wsTransport.sent.length;
    multi.send('via ws', PRIORITY_CRITICAL);
    assert(wsTransport.sent.length > wsCountBefore, 'WS gets data after RTC close');

    multi.destroy();
})();

console.log('test_continuous_seq_across_switch');
(function() {
    var wsTransport = null;
    var rtcTransport = null;
    var received = [];

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            },
            rtc: {
                connect: function() {
                    rtcTransport = new MockTransport();
                    return rtcTransport;
                },
                priority: 0
            }
        }
    });

    // Send seq 1, 2 via RTC
    multi.send('one', PRIORITY_CRITICAL);
    multi.send('two', PRIORITY_CRITICAL);

    // RTC dies → failover to WS
    rtcTransport.simulateClose();

    // Send seq 3 via WS
    multi.send('three', PRIORITY_CRITICAL);

    // Receiver decodes all
    var receiver = new ReliableMultiTransport({
        transports: { ws: { connect: function() { return new MockTransport(); }, priority: 1 } },
        onmessage: function(payload, seq) {
            received.push({ text: new TextDecoder().decode(payload), seq: seq });
        }
    });

    // Feed RTC frames
    var rtcDataFrames = rtcTransport.sent.filter(function(f) {
        var u8 = f instanceof Uint8Array ? f : new Uint8Array(f);
        return u8[1] === 0x01;
    });
    for (var i = 0; i < rtcDataFrames.length; i++) {
        receiver.receive(rtcDataFrames[i]);
    }

    // Feed WS frames
    var wsDataFrames = wsTransport.sent.filter(function(f) {
        var u8 = f instanceof Uint8Array ? f : new Uint8Array(f);
        return u8[1] === 0x01;
    });
    for (var j = 0; j < wsDataFrames.length; j++) {
        receiver.receive(wsDataFrames[j]);
    }

    assertEq(received.length, 3, 'All 3 messages received');
    assertEq(received[0].seq, 1, 'seq 1');
    assertEq(received[1].seq, 2, 'seq 2');
    assertEq(received[2].seq, 3, 'seq 3 — continuous across switch');

    multi.destroy();
    receiver.destroy();
})();

console.log('test_setAllowedTransports');
(function() {
    var wsTransport = null;
    var rtcTransport = null;

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            },
            rtc: {
                connect: function() {
                    rtcTransport = new MockTransport();
                    return rtcTransport;
                },
                priority: 0
            }
        }
    });

    assert(rtcTransport !== null, 'RTC connected in auto mode');
    assert(wsTransport !== null, 'WS connected in auto mode');

    // Switch to WS only
    multi.setAllowedTransports('ws');
    assert(rtcTransport.closed, 'RTC closed when switched to WS-only');

    // Send goes to WS
    var wsBefore = wsTransport.sent.length;
    multi.send('ws only', PRIORITY_CRITICAL);
    assert(wsTransport.sent.length > wsBefore, 'WS gets data in WS-only mode');

    multi.destroy();
})();

console.log('test_rtc_block_check');
(function() {
    var blocked = true;
    var wsTransport = null;
    var rtcConnectCount = 0;

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            },
            rtc: {
                connect: function() {
                    rtcConnectCount++;
                    return new MockTransport();
                },
                priority: 0
            }
        },
        rtcBlockCheck: function() { return blocked; }
    });

    assertEq(rtcConnectCount, 0, 'RTC not connected when blocked');
    assert(wsTransport !== null, 'WS connected even when RTC blocked');

    // Unblock — the _checkRtcBlock timer would fire, but we call it manually
    blocked = false;
    multi._checkRtcBlock();
    assertEq(rtcConnectCount, 1, 'RTC connected after block cleared');

    multi.destroy();
})();

console.log('test_onconnect_callback');
(function() {
    var connectEvents = [];

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() { return new MockTransport(); },
                priority: 1
            }
        },
        onconnect: function(name, handle) {
            connectEvents.push(name);
        }
    });

    assertEq(connectEvents.length, 1, 'onconnect fired once');
    assertEq(connectEvents[0], 'ws', 'onconnect fired for ws');

    multi.destroy();
})();

console.log('test_status_reporting');
(function() {
    var blocked = false;
    var wsTransport = null;

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            },
            rtc: {
                connect: function() { return new MockTransport(); },
                priority: 0
            }
        },
        rtcBlockCheck: function() { return blocked; }
    });

    var s = multi.status();
    assertEq(s.ws, 'connected', 'WS connected');
    assertEq(s.rtc, 'connected', 'RTC connected');
    assertEq(s.active, 'rtc', 'RTC is active (lower priority number)');

    // Switch to WS only
    multi.setAllowedTransports('ws');
    s = multi.status();
    assertEq(s.ws, 'connected', 'WS still connected');
    assertEq(s.rtc, 'disabled', 'RTC disabled');
    assertEq(s.active, 'ws', 'WS is now active');

    multi.destroy();
})();

console.log('test_destroy_cleanup');
(function() {
    var wsTransport = null;

    var multi = new ReliableMultiTransport({
        transports: {
            ws: {
                connect: function() {
                    wsTransport = new MockTransport();
                    return wsTransport;
                },
                priority: 1
            }
        }
    });

    multi.destroy();
    assert(wsTransport.closed, 'WS closed on destroy');
})();

// ── Results ───────────────────────────────────────────────────────
console.log('\n' + passed + ' passed, ' + failed + ' failed');
if (failed > 0) process.exit(1);
