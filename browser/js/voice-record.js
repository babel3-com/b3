/* voice-record.js — Babel3 voice recording and STT (open-source)
 *
 * Continuous voice recording with silence detection, streaming segmentation,
 * IndexedDB audio persistence, GPU-based transcription, dual-model STT,
 * garbage filtering, diarization, daemon injection.
 *
 * Depends on: core.js (HC namespace), gpu.js (HC.gpuRun), tts.js
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// GPU stream send — routes through the active GPU channel.
// Pool mode: active pool entry's channel (EC2 worker).
// Relay mode: HC.gpuChannel (E2E encrypted relay / local HTTP).
// Transport selection, failover, and reconnect handled by the channel.

// Returns the active GPU channel: pool entry if EC2 workers are active, else HC.gpuChannel (tunnel).
function _gpuActiveChannel() {
    var activeId = HC._gpuActiveWorkerId;
    if (activeId && HC._gpuWorkerPool && HC._gpuWorkerPool[activeId]) {
        var e = HC._gpuWorkerPool[activeId];
        if (e.status === 'active' && e.channel) return e.channel;
    }
    return HC.gpuChannel || null; // tunnel fallback
}

function _gpuStreamSend(msg) {
    var ch = _gpuActiveChannel();
    if (!ch) {
        log('[STT-Stream] _gpuStreamSend: no channel (activeId=' + HC._gpuActiveWorkerId +
            ' pool=' + JSON.stringify(Object.keys(HC._gpuWorkerPool || {})) + ')');
        return false;
    }
    var status = ch.status();
    if (!status.active) return false;
    try {
        ch.send(JSON.stringify(msg));
        return true;
    } catch(e) {
        log('[STT-Stream] Send failed: ' + e.message);
        return false;
    }
}
function _gpuStreamReady() {
    var ch = _gpuActiveChannel();
    if (!ch) return false;
    var status = ch.status();
    return !!status.active;
}

// ── Continuous Voice Recording with Silence Detection ──
// Ported from voice-web app: mic stays open, silence triggers send.
const micBtn = document.getElementById('mic-btn');
const micIcon = document.getElementById('mic-icon');
let micStream = null;
let audioContext = null;
let analyser = null;
let prerollNode = null;
let mediaRecorder = null;
let audioChunks = [];
let _streamUploadId = null;     // Active streaming upload session ID
let _streamChunkIndex = 0;      // Chunk counter for current stream
var _streamSentChunks = {};     // stream_id → { chunk_index: base64 } for retransmission
var _streamChunkTimes = {};     // stream_id → { chunk_index: ms_offset_from_stream_start } for replay timing
var _streamStartTimes = {};     // stream_id → Date.now() at stream_start send
var _streamPendingFinalize = null;  // { streamId, expected, data } — deferred until last chunk sent
var _streamChunksSent = 0;      // Count of chunks whose FileReader.onload completed
let isListening = false;
let isSpeaking = false;
let silenceStart = null;
let speechStart = null;
let _gateStart = null;  // sustained energy gate: when energy first exceeded threshold
const GATE_DURATION = 500; // ms of sustained energy required before recording starts
let _filesVisitedDuringRecording = [];  // tracks files browsed while recording
let prerollBlob = null;
let recorderDest = null;  // Promoted to module scope for zero-gap restart

// In-flight transcription tracking
let _sttSeq = 0;
const _sttInFlight = new Map();  // seq → { timer, retries }
const STT_TIMEOUT_MS = 60000;  // 60s — covers RunPod cloud (45s typical)
const STT_COLDSTART_TIMEOUT_MS = 90000;  // Extended timeout when GPU is warming up
const STT_MAX_RETRIES = 1;

// RunPod cold-start queue: holds pending transcriptions while waiting for
// a serverless worker to connect. Drained by HC._drainTranscribeQueue()
// which gpu.js calls when a new EC2 worker first appears (wasNull path).
// Entries stay in _sttInFlight — existing timeout handles expiry if worker never connects.
const _pendingTranscribeQueue = [];  // [{ seq, audioBlob, durSec, avgEnergy }]

// ── Garbage filter: shared threshold + match logic ──
// Threshold = 95% of self-similarity between models.
// Allows minor capitalization/punctuation differences in junk samples.
function garbageThreshold(embSim) {
    return typeof embSim === 'number' ? embSim * 0.95 : 0.40;
}
// Check embedding vectors against garbage entries. Returns { text, sim, model } or null.
function garbageCheck(vecs, textsToCheck, threshold, label) {
    for (var vi = 0; vi < vecs.length; vi++) {
        var bestSim = 0, bestMatch = '';
        for (var j = 0; j < HC._garbageEntries.length; j++) {
            var sim = HC.cosineSim(vecs[vi], HC._garbageEntries[j].embedding);
            if (sim > bestSim) { bestSim = sim; bestMatch = HC._garbageEntries[j].text; }
        }
        log('[STT] Garbage check' + (label ? ' (' + label + ')' : '') + ': text="' + textsToCheck[vi].substring(0,30) + '" best="' + bestMatch.substring(0,30) + '" sim=' + bestSim.toFixed(3) + ' thresh=' + threshold.toFixed(3));
        if (bestSim >= threshold) {
            return { text: bestMatch, sim: bestSim, model: vi === 0 ? 'M' : 'X' };
        }
    }
    return null;
}

// Detection thresholds (matching voice-web defaults)
// SPEECH_THRESHOLD is user-adjustable via the mic level strip slider
let SPEECH_THRESHOLD = (HC._prefs && HC._prefs.speech_threshold !== undefined ? HC._prefs.speech_threshold : 20);
let SILENCE_THRESHOLD = Math.max(2, SPEECH_THRESHOLD);
const SILENCE_DURATION = 3000;

const MIN_SPEECH_DURATION = 300;
function getPrerollSeconds() { return Math.max(2, (MIN_SPEECH_DURATION * 5 + 500) / 1000); }
let currentMicLevel = 0;
let lastTranscriptionTime = 0;
const VAD_GATE_ENABLED = true;

// Energy gate: track average energy during speech to filter non-speech recordings
let ENERGY_GATE_THRESHOLD = 10;  // Average level must exceed this to send for transcription
let _speechEnergySum = 0;
let _speechEnergySamples = 0;
let _lastSpeechDurSec = '?';
const VAD_GATE_SILENCE_WINDOW = 4000;
const VAD_GATE_MAX_DELAY = 60000;

// Voice status — displayed as text input placeholder (like voice-web)
const LISTENING_HTML = 'Listening...';
const _defaultPlaceholder = 'Type a message...';
let _voiceStatusGen = 0;
function setVoiceStatus(cls, text) {
    const ti = document.getElementById('text-input');
    if (!ti) return;
    // Strip HTML tags for placeholder (plain text only)
    const plain = text.replace(/<[^>]*>/g, '').trim();
    if (!ti.value) ti.placeholder = plain || _defaultPlaceholder;
    HC.flashFsToast(plain);
}
function revertVoiceStatusAfter(ms, cls, text) {
    const gen = ++_voiceStatusGen;
    setTimeout(() => {
        if (_voiceStatusGen === gen) setVoiceStatus(cls, text);
    }, ms);
}
function hideVoiceStatus() {
    const ti = document.getElementById('text-input');
    if (ti && !ti.value) ti.placeholder = _defaultPlaceholder;
}
function updatePendingDisplay() {
    if (_sttInFlight.size > 0) {
        setVoiceStatus('sending', '\u23f3 Waiting for ' + _sttInFlight.size + ' transcription' + (_sttInFlight.size > 1 ? 's' : '') + '...');
    }
}

// ── IndexedDB Audio Persistence ──
// Saves audio blobs to IndexedDB immediately on recording, survives GC + page refresh
const IDB_NAME = 'Babel3Audio';
const IDB_STORE = 'recordings';
const IDB_VERSION = 2;  // v2: added stt-fixtures store

// ── STT Fixture Capture ──
// Captures transcription jobs (audio + metadata + result) for offline replay.
// Enables reproducing intermittent RunPod empty-result failures.
// Enable: sessionStorage.setItem('b3_stt_capture', '1')
// Export: HC._exportSttFixtures() — downloads .jsonl for CLI worker-isolation replay
const STT_FIXTURE_STORE = 'stt-fixtures';
const STT_FIXTURE_MAX = 50;  // keep last 50 fixtures, drop oldest
var _sttCapturePreroll = null;  // b64 WAV preroll for current stream (cleared after capture)

function _shouldCaptureFixture() {
    try { return sessionStorage.getItem('b3_stt_capture') === '1'; } catch(e) { return false; }
}

async function _saveFixtureToIdb(fixture) {
    try {
        const db = await _openAudioDb();
        const tx = db.transaction(STT_FIXTURE_STORE, 'readwrite');
        tx.objectStore(STT_FIXTURE_STORE).add(fixture);
        await new Promise((res, rej) => { tx.oncomplete = res; tx.onerror = () => rej(tx.error); });
        // Trim oldest if over limit
        const tx2 = db.transaction(STT_FIXTURE_STORE, 'readwrite');
        const store2 = tx2.objectStore(STT_FIXTURE_STORE);
        const allKeys = await new Promise((res, rej) => {
            const r = store2.getAllKeys(); r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
        });
        if (allKeys.length > STT_FIXTURE_MAX) {
            const toDelete = allKeys.slice(0, allKeys.length - STT_FIXTURE_MAX);
            toDelete.forEach(k => store2.delete(k));
        }
        await new Promise((res, rej) => { tx2.oncomplete = res; tx2.onerror = () => rej(tx2.error); });
        db.close();
        log('[STT-Fixture] Saved ' + fixture.id + ' (' + fixture.type + ' ' + (fixture.result.empty ? 'EMPTY' : 'ok') + ')');
    } catch(e) { log('[STT-Fixture] Save error: ' + e.message); }
}

// Update a stored fixture by fixture.id (not IDB autoIncrement key).
// Applies patch fields to the fixture object and writes back via cursor scan.
HC._updateSttFixture = async function(fixtureId, patch) {
    try {
        var db = await _openAudioDb();
        var tx = db.transaction(STT_FIXTURE_STORE, 'readwrite');
        var store = tx.objectStore(STT_FIXTURE_STORE);
        await new Promise(function(resolve, reject) {
            var req = store.openCursor();
            req.onsuccess = function(e) {
                var cursor = e.target.result;
                if (!cursor) { resolve(); return; }
                if (cursor.value.id === fixtureId) {
                    var updated = Object.assign({}, cursor.value, patch);
                    var putReq = cursor.update(updated);
                    putReq.onsuccess = function() { resolve(); };
                    putReq.onerror = function() { reject(putReq.error); };
                } else {
                    cursor.continue();
                }
            };
            req.onerror = function() { reject(req.error); };
        });
        db.close();
    } catch(e) { log('[STT-Fixture] _updateSttFixture error: ' + e.message); }
};

// Export all fixtures as a newline-delimited JSON file download.
// Usage: HC._exportSttFixtures() from browser console or UI.
HC._exportSttFixtures = async function() {
    try {
        const db = await _openAudioDb();
        const tx = db.transaction(STT_FIXTURE_STORE, 'readonly');
        const fixtures = await new Promise((res, rej) => {
            const r = tx.objectStore(STT_FIXTURE_STORE).getAll();
            r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
        });
        db.close();
        if (!fixtures.length) { log('[STT-Fixture] No fixtures to export'); return; }
        const lines = fixtures.map(f => JSON.stringify(f)).join('\n');
        const blob = new Blob([lines], { type: 'application/x-ndjson' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = 'stt-fixtures-' + new Date().toISOString().slice(0,19).replace(/:/g,'-') + '.jsonl';
        document.body.appendChild(a);
        a.click();
        setTimeout(() => { document.body.removeChild(a); URL.revokeObjectURL(url); }, 1000);
        log('[STT-Fixture] Exported ' + fixtures.length + ' fixtures');
    } catch(e) { log('[STT-Fixture] Export error: ' + e.message); }
};

// Push all fixtures (or a filtered subset) to the daemon via /api/file-upload.
// No download dialog — the daemon writes the file and injects a [file-upload] notice.
// Returns { path, count } on success, or throws on error.
//
// Usage:
//   HC._pushSttFixturesToDaemon()                    — push all fixtures
//   HC._pushSttFixturesToDaemon({ filter: 'empty' }) — push only empty-result fixtures
//   HC._pushSttFixturesToDaemon({ dest_dir: '/home/me/my-fixtures' })
HC._pushSttFixturesToDaemon = async function(opts) {
    opts = opts || {};
    if (!HC.hasDaemonConnection()) { throw new Error('No daemon connection — daemon not connected'); }
    if (!HC.config || !HC.config.daemonToken) { throw new Error('HC.config.daemonToken not available'); }

    const db = await _openAudioDb();
    const tx = db.transaction(STT_FIXTURE_STORE, 'readonly');
    var fixtures = await new Promise((res, rej) => {
        const r = tx.objectStore(STT_FIXTURE_STORE).getAll();
        r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
    });
    db.close();

    if (opts.filter === 'empty') {
        fixtures = fixtures.filter(function(f) { return f.result && f.result.empty; });
    }
    if (!fixtures.length) {
        log('[STT-Fixture] No fixtures to push' + (opts.filter ? ' (filter=' + opts.filter + ')' : ''));
        return { path: null, count: 0 };
    }

    const lines = fixtures.map(f => JSON.stringify(f)).join('\n');
    const ts = new Date().toISOString().slice(0, 19).replace(/:/g, '-');
    const filename = 'stt-fixtures-' + ts + '.jsonl';
    const blob = new Blob([lines], { type: 'application/x-ndjson' });

    const form = new FormData();
    form.append('file', blob, filename);
    // dest_dir must already exist on disk — daemon returns 400 if not.
    // Default: omit dest_dir → daemon saves to ~/uploads/ (always exists).
    if (opts.dest_dir) form.append('dest_dir', opts.dest_dir);

    const resp = await fetch(HC._daemonBase + '/api/file-upload', {
        method: 'POST',
        headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
        body: form,
    });
    if (!resp.ok) {
        const text = await resp.text().catch(() => '');
        throw new Error('file-upload failed: HTTP ' + resp.status + ' ' + text.slice(0, 100));
    }
    const json = await resp.json();
    const savedPath = json.path || filename;
    log('[STT-Fixture] Pushed ' + fixtures.length + ' fixture(s) to daemon: ' + savedPath);
    return { path: savedPath, count: fixtures.length };
};

// Replay a single fixture through the real browser transport path (gpuChannel).
// For stream fixtures: sends stream_start → preroll → chunks (with original timing)
// → stream_finalize through _gpuStreamSend, exercising the full WS/RTC stack.
// For segment fixtures: calls HC.gpuRun() directly with the captured audio_b64.
//
// Parse a .jsonl string and replay all fixtures sequentially.
// Used by the daemon-side replay script via /api/browser-eval injection.
// Returns array of result objects.
HC._replaySttFixturesFromText = async function(text, opts) {
    opts = opts || {};
    var fixtures = [];
    text.split('\n').forEach(function(line) {
        line = line.trim();
        if (!line) return;
        try { fixtures.push(JSON.parse(line)); } catch(e) { log('[Replay] Bad JSON line: ' + e.message); }
    });
    log('[Replay] Loaded ' + fixtures.length + ' fixtures from injected data');
    var results = [];
    var speedup = opts.speedup || 1.0;
    for (var i = 0; i < fixtures.length; i++) {
        var fix = fixtures[i];
        var entry = { id: fix.id, type: fix.type,
                      original_empty: fix.result && fix.result.empty,
                      original_backend: fix.result && fix.result.backend,
                      expected_text: fix.expected_text || null };
        await new Promise(function(done) {
            HC._replaySttFixture(fix, {
                speedup: speedup,
                onResult: function(out, ms) {
                    var text = ((out.text || '') + ' ' + (out.whisperx_text || '')).trim();
                    entry.replay_empty = !text; entry.replay_text = text.substring(0, 80); entry.replay_ms = ms;
                    var hasExpected = !!fix.expected_text;
                    if (entry.original_empty && !entry.replay_empty) {
                        entry.verdict = 'REPRODUCED_BUG';
                    } else if (entry.original_empty && entry.replay_empty && hasExpected) {
                        entry.verdict = 'REGRESSION';  // confirmed-content fixture went empty again
                    } else if (!entry.original_empty && entry.replay_empty) {
                        entry.verdict = 'REGRESSION';
                    } else {
                        entry.verdict = 'CONSISTENT';
                    }
                    var logLine = '[Replay] ' + fix.id + ' → ' + entry.verdict + ' (' + ms + 'ms)';
                    if (entry.verdict === 'REPRODUCED_BUG' && fix.expected_text) {
                        logLine += ' expected: "' + fix.expected_text.substring(0, 50) + '"';
                    }
                    log(logLine);
                    done();
                },
                onError: function(e) {
                    entry.replay_empty = true; entry.error = e; entry.verdict = 'ERROR';
                    log('[Replay] ' + fix.id + ' → ERROR: ' + e);
                    done();
                },
            });
        });
        results.push(entry);
    }
    return results;
};

// Usage:
//   HC._replaySttFixture(fixture)
//   HC._replaySttFixture(fixture, { onResult: (out, ms) => console.log(out), speedup: 2.0 })
HC._replaySttFixture = async function(fixture, opts) {
    opts = opts || {};
    var speedup = opts.speedup || 1.0;  // 1.0 = original timing, 2.0 = double speed
    var onResult = opts.onResult || function(out, ms) {
        var text = (out.text || out.whisperx_text || '').trim();
        log('[Replay] Done in ' + ms + 'ms — ' + (text ? '"' + text.substring(0, 60) + '"' : 'EMPTY'));
    };
    var onError = opts.onError || function(e) { log('[Replay] Error: ' + e); };

    log('[Replay] Starting fixture ' + fixture.id + ' type=' + fixture.type);

    if (fixture.type === 'segment') {
        var req = fixture.request || {};
        var payload = {
            action: 'transcribe',
            audio_b64: fixture.audio.audio_b64,
            language: req.language || 'en',
            preset: req.preset || 'multilingual',
            skip_align: req.skip_align || false,
            agent_id: req.agent_id || (HC.config && HC.config.agentId) || '',
        };
        try {
            var t0seg = performance.now();
            var gpu = await HC.gpuRun(payload);
            var jobId = gpu.data.id;
            log('[Replay] Segment job=' + jobId + ' backend=' + gpu.backend);
            // Poll for result — reuse existing HTTP polling pattern
            var output = {};
            var done = false;
            var baseUrl = gpu.baseUrl;
            var pollHeaders = HC.gpuPollHeaders(gpu.token);
            var streamUrl = HC.gpuStreamUrl(baseUrl, jobId);
            if (gpu.wsController) {
                await new Promise(function(resolve) {
                    gpu.wsController.onResult = function(r) { output = r; done = true; resolve(); };
                    gpu.wsController.onError = function(e) { done = true; resolve(); };
                    gpu.wsController.onDone = function() { if (!done) { done = true; resolve(); } };
                });
            } else {
                for (var pi = 0; pi < 300 && !done; pi++) {
                    await new Promise(function(r) { setTimeout(r, 500); });
                    try {
                        var sr = await fetch(streamUrl, { headers: pollHeaders });
                        if (!sr.ok) continue;
                        var sd = await sr.json();
                        var items = sd.stream || (Array.isArray(sd) ? sd : []);
                        for (var ii = 0; ii < items.length; ii++) {
                            var ev = items[ii].output || items[ii];
                            if (ev.text !== undefined || ev.medium_elapsed !== undefined) { output = ev; done = true; break; }
                        }
                        if (!done && (sd.status === 'COMPLETED' || sd.status === 'FAILED')) done = true;
                    } catch(pe) {}
                }
            }
            onResult(output, Math.round(performance.now() - t0seg));
        } catch(e) { onError(e.message); }
        return;
    }

    // ── Stream replay ──
    if (!_gpuStreamReady()) { onError('GPU stream not ready'); return; }

    var audio = fixture.audio || {};
    var req = fixture.request || {};
    var chunks = audio.chunks || {};
    var chunkTimes = audio.chunk_times || {};
    var chunkKeys = Object.keys(chunks).sort(function(a, b) { return parseInt(a) - parseInt(b); });
    // Use UUID format — relay may validate stream_id format against live stream pattern.
    var replayId = (typeof crypto !== 'undefined' && crypto.randomUUID)
        ? crypto.randomUUID()
        : 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, function(c) {
              var r = Math.random() * 16 | 0;
              return (c === 'x' ? r : (r & 0x3 | 0x8)).toString(16);
          });

    var t0stream = Date.now();
    var resultPromise = new Promise(function(resolve) {
        var timer = setTimeout(function() {
            delete HC._gpuPending[replayId];
            delete HC._gpuPending[replayId + '-finalize'];
            resolve({ error: 'timeout 90s' });
        }, 90000);
        var handler = {
            onResult: function(out) {
                clearTimeout(timer);
                delete HC._gpuPending[replayId]; delete HC._gpuPending[replayId + '-finalize'];
                resolve({ output: out });
            },
            onError: function(e) {
                clearTimeout(timer);
                delete HC._gpuPending[replayId]; delete HC._gpuPending[replayId + '-finalize'];
                resolve({ error: e });
            },
            onDone: function() {}, onProgress: function() {},
        };
        handler._expireTimer = timer;  // expose so _replaySend can cancel on early failure
        HC._gpuPending[replayId] = HC._gpuPending[replayId + '-finalize'] = handler;
    });

    // Helper: send and surface failures immediately rather than silently timing out.
    function _replaySend(msg) {
        var ok = _gpuStreamSend(msg);
        if (!ok) {
            var pending = HC._gpuPending[replayId];
            if (pending) clearTimeout(pending._expireTimer);
            delete HC._gpuPending[replayId]; delete HC._gpuPending[replayId + '-finalize'];
            onError('send failed (channel inactive) at ' + msg.type);
        }
        return ok;
    }

    // stream_start
    if (!_replaySend({ type: 'stream_start', stream_id: replayId,
                       mime_type: audio.mime_type || 'audio/webm',
                       agent_id: req.agent_id || (HC.config && HC.config.agentId) || '' })) return;
    log('[Replay] stream_start sent id=' + replayId + ' chunks=' + chunkKeys.length);

    // preroll
    if (audio.preroll_b64) {
        if (!_replaySend({ type: 'stream_preroll', stream_id: replayId,
                           audio_b64: audio.preroll_b64, mime_type: audio.preroll_mime || 'audio/wav' })) return;
    }

    // chunks with original timing
    var prevTime = 0;
    for (var ci = 0; ci < chunkKeys.length; ci++) {
        var idx = chunkKeys[ci];
        var absTime = (chunkTimes[idx] != null) ? chunkTimes[idx] : ci * 250;
        var delay = Math.round((absTime - prevTime) / speedup);
        if (delay > 0) await new Promise(function(r) { setTimeout(r, delay); });
        prevTime = absTime;
        if (!_replaySend({ type: 'stream_chunk', stream_id: replayId,
                           chunk_index: parseInt(idx), audio_b64: chunks[idx] })) return;
    }

    // finalize
    if (!_replaySend({ type: 'stream_finalize', stream_id: replayId,
                       expected_chunks: chunkKeys.length,
                       preset: req.preset || 'multilingual',
                       skip_align: req.skip_align || false,
                       language: req.language || 'en' })) return;
    log('[Replay] stream_finalize sent — waiting for result');

    var res = await resultPromise;
    if (res.error) { onError(res.error); }
    else { onResult(res.output, Date.now() - t0stream); }
};

// Load fixtures from a .jsonl string (e.g. from a file upload) and replay all.
// Run all stored fixtures as a regression test — reads from IDB, replays each
// one through the real transport path, reports pass/fail per fixture.
// Usage: HC._runSttReplayTests(opts)
//   opts.filter = 'empty' | 'all' (default: 'all')
//   opts.speedup = number (default: 1.0 — original timing)
//   opts.onDone = function(results) — called with array of result objects
HC._runSttReplayTests = async function(opts) {
    opts = opts || {};
    var filterEmpty = opts.filter === 'empty';
    var speedup = opts.speedup || 1.0;
    var results = [];

    var fixtures;
    try {
        var db = await _openAudioDb();
        var tx = db.transaction(STT_FIXTURE_STORE, 'readonly');
        fixtures = await new Promise(function(res, rej) {
            var r = tx.objectStore(STT_FIXTURE_STORE).getAll();
            r.onsuccess = function() { res(r.result); }; r.onerror = function() { rej(r.error); };
        });
        db.close();
    } catch(e) { log('[Replay] IDB read error: ' + e.message); return; }

    if (filterEmpty) fixtures = fixtures.filter(function(f) { return f.result && f.result.empty; });
    if (!fixtures.length) { log('[Replay] No fixtures' + (filterEmpty ? ' (empty results)' : '') + ' in IDB'); return; }

    log('[Replay] Running ' + fixtures.length + ' fixture(s) speedup=' + speedup + ' ...');

    for (var i = 0; i < fixtures.length; i++) {
        var fix = fixtures[i];
        var entry = { id: fix.id, type: fix.type, original_empty: fix.result && fix.result.empty,
                      original_backend: fix.result && fix.result.backend,
                      expected_text: fix.expected_text || null };
        await new Promise(function(done) {
            HC._replaySttFixture(fix, {
                speedup: speedup,
                onResult: function(out, ms) {
                    var text = ((out.text || '') + ' ' + (out.whisperx_text || '')).trim();
                    entry.replay_empty = !text;
                    entry.replay_text = text.substring(0, 80);
                    entry.replay_ms = ms;
                    var hasExpected = !!fix.expected_text;
                    if (entry.original_empty && !entry.replay_empty) {
                        entry.verdict = 'REPRODUCED_BUG';
                        // Promote: first successful replay of an empty fixture sets expected_text
                        if (!hasExpected) {
                            HC._updateSttFixture(fix.id, { expected_text: text });
                            entry.expected_text = text;
                            log('[Replay] ' + fix.id + ' → promoted expected_text: "' + text.substring(0, 60) + '"');
                        }
                    } else if (entry.original_empty && entry.replay_empty && hasExpected) {
                        entry.verdict = 'REGRESSION';  // confirmed-content fixture went empty again
                    } else if (!entry.original_empty && entry.replay_empty) {
                        entry.verdict = 'REGRESSION';
                    } else {
                        entry.verdict = 'CONSISTENT';
                    }
                    var logParts = '[Replay] ' + fix.id + ' → ' + entry.verdict +
                        ' (' + ms + 'ms) ' + (text ? '"' + text.substring(0, 50) + '"' : 'EMPTY');
                    if (entry.verdict === 'REPRODUCED_BUG' && fix.expected_text) {
                        logParts += '\n  expected: "' + fix.expected_text.substring(0, 50) + '"';
                    }
                    log(logParts);
                    done();
                },
                onError: function(e) {
                    entry.replay_empty = true; entry.error = e; entry.verdict = 'ERROR';
                    log('[Replay] ' + fix.id + ' → ERROR: ' + e);
                    done();
                },
            });
        });
        results.push(entry);
    }

    var pass = results.filter(function(r) { return r.verdict === 'CONSISTENT'; }).length;
    var bugs = results.filter(function(r) { return r.verdict === 'REPRODUCED_BUG'; }).length;
    var regressions = results.filter(function(r) { return r.verdict === 'REGRESSION'; }).length;
    var errors = results.filter(function(r) { return r.verdict === 'ERROR'; }).length;
    log('[Replay] Done: ' + pass + ' consistent, ' + bugs + ' reproduced bugs, ' +
        regressions + ' regressions, ' + errors + ' errors');

    if (opts.onDone) opts.onDone(results);
    return results;
};

function _openAudioDb() {
    return new Promise((resolve, reject) => {
        const req = indexedDB.open(IDB_NAME, IDB_VERSION);
        req.onupgradeneeded = (e) => {
            const db = e.target.result;
            if (!db.objectStoreNames.contains(IDB_STORE)) {
                db.createObjectStore(IDB_STORE, { keyPath: 'seq' });
            }
            if (!db.objectStoreNames.contains(STT_FIXTURE_STORE)) {
                db.createObjectStore(STT_FIXTURE_STORE, { autoIncrement: true });
            }
        };
        req.onsuccess = () => resolve(req.result);
        req.onerror = () => reject(req.error);
    });
}

async function _saveAudioBlob(seq, blob, durSec) {
    try {
        const db = await _openAudioDb();
        const tx = db.transaction(IDB_STORE, 'readwrite');
        tx.objectStore(IDB_STORE).put({ seq, blob, durSec, ts: Date.now() });
        await new Promise((res, rej) => { tx.oncomplete = res; tx.onerror = rej; });
        db.close();
    } catch (e) { log('[IDB] save error: ' + e.message); }
}

async function _deleteAudioBlob(seq) {
    try {
        const db = await _openAudioDb();
        const tx = db.transaction(IDB_STORE, 'readwrite');
        tx.objectStore(IDB_STORE).delete(seq);
        await new Promise((res, rej) => { tx.oncomplete = res; tx.onerror = rej; });
        db.close();
    } catch (e) { log('[IDB] delete error: ' + e.message); }
}

async function _getOrphanedRecordings() {
    try {
        const db = await _openAudioDb();
        const tx = db.transaction(IDB_STORE, 'readonly');
        const store = tx.objectStore(IDB_STORE);
        return new Promise((resolve, reject) => {
            const req = store.getAll();
            req.onsuccess = () => {
                db.close();
                const tenMinAgo = Date.now() - 600000;
                resolve((req.result || []).filter(r => r.ts > tenMinAgo));
            };
            req.onerror = () => { db.close(); reject(req.error); };
        });
    } catch (e) { log('[IDB] getOrphans error: ' + e.message); return []; }
}

// ── Streaming Partial Transcription Handler ──
HC._onStreamPartial = function(streamId, output) {
    var text = (output.text || '').trim();
    if (text) {
        setVoiceStatus('transcribing', '\ud83c\udfa4 ' + text);
        log('[STT] Partial: "' + text.substring(0, 50) + '..."');
        // Store partial result for combining with final
        _partialResults.push({
            text: text,
            wxText: (output.whisperx_text || '').trim(),
            segments: output.segments || [],
            wordSegments: output.word_segments || [],
            chunkOffset: _cumulativeOffset,
            mediumElapsed: output.medium_elapsed || 0,
            wxElapsed: output.whisperx_elapsed || 0,
            embSim: output.embedding_similarity,
            energy: typeof avgEnergy === 'number' ? avgEnergy : undefined,
        });
    }
};

// ── Streaming Audio Upload ──
// Send compressed webm chunks to GPU WebSocket during recording.
// GPU accumulates chunks and transcribes when browser sends "finalize".
function _streamAudioChunk(blob) {
    // Only stream when actively recording speech (not just listening)
    if (!isSpeaking && !_streamUploadId) return;
    // Start a new stream session on first chunk
    if (!_streamUploadId) {
        if (!_gpuStreamReady()) {
            // GPU not ready — buffer chunk, show status, reconnect gracefully.
            if (!HC._streamBufferedChunks) HC._streamBufferedChunks = [];
            HC._streamBufferedChunks.push(blob);
            if (HC._streamBufferedChunks.length > 60) HC._streamBufferedChunks.shift();
            // Status update — user sees what's happening
            var bufSec = HC._streamBufferedChunks.length;
            setVoiceStatus('sending', '\u26a0\ufe0f GPU reconnecting... ' + bufSec + 's buffered');
            // Throttled logging
            if (bufSec <= 3 || bufSec % 10 === 0) {
                log('[STT-Stream] GPU not ready — buffered chunk (' + bufSec + ')');
            }
            // Let the channel's built-in reconnect handle recovery.
            // DO NOT call fetchGpuConfig()/gpuInit() here — that creates a new
            // ReliableMultiTransport on every retry, flooding the daemon WS with
            // disconnect events and killing the connection (see incident 2026-04-02).
            // The channel's _scheduleReconnect has exponential backoff built in.
            // Buffer chunks and poll for channel recovery.
            if (!HC._gpuStreamReconnecting) {
                HC._gpuStreamReconnecting = true;
                var _flushAttempt = function() {
                    if (_gpuStreamReady()) {
                        HC._gpuStreamReconnecting = false;
                        var buffered = HC._streamBufferedChunks || [];
                        HC._streamBufferedChunks = [];
                        if (buffered.length > 0) {
                            _streamUploadId = null;
                            _streamChunkIndex = 0;
                            log('[STT-Stream] GPU recovered — flushing ' + buffered.length + ' buffered chunks');
                            for (var i = 0; i < buffered.length; i++) _streamAudioChunk(buffered[i]);
                        }
                    } else {
                        // Still not ready — poll again in 2s. Channel reconnects itself.
                        setTimeout(_flushAttempt, 2000);
                    }
                };
                setTimeout(_flushAttempt, 2000);
            }
            return;
        }
        // Pre-flight probe: if RTC is active, send a quick echo to verify liveness.
        // If no response in 500ms, demote RTC so the stream goes through WS.
        // Non-blocking: uses callback, not await (this function isn't async).
        if (HC.gpuChannel) {
            var _pfStatus = HC.gpuChannel.status();
            if (_pfStatus.active === 'rtc') {
                var _pfEchoId = 'pf-' + Date.now();
                var _pfDone = false;
                var _pfOldOnMsg = HC.gpuChannel.onmessage;
                var _pfTimer = setTimeout(function() {
                    if (_pfDone) return;
                    _pfDone = true;
                    HC.gpuChannel.onmessage = _pfOldOnMsg;
                    log('[STT-Stream] Pre-flight probe: RTC dead (2s timeout) — demoting to WS');
                    HC.gpuChannel._disconnectTransport('rtc');
                }, 2000);
                HC.gpuChannel.onmessage = function(payload) {
                    try {
                        var text = (typeof payload === 'string') ? payload : new TextDecoder().decode(payload);
                        var msg = JSON.parse(text);
                        if (msg.type === 'echo' && msg.echo_id === _pfEchoId) {
                            if (!_pfDone) {
                                _pfDone = true;
                                clearTimeout(_pfTimer);
                                HC.gpuChannel.onmessage = _pfOldOnMsg;
                                log('[STT-Stream] Pre-flight probe: RTC alive');
                            }
                            return;
                        }
                    } catch(e) {}
                    if (_pfOldOnMsg) _pfOldOnMsg(payload);
                };
                HC.gpuChannel.send(JSON.stringify({ type: 'echo', echo_id: _pfEchoId }));
            }
        }

        // Generate stream ID but don't commit it until stream_start send succeeds.
        // If _gpuStreamSend() fails (channel null or not active), _streamUploadId
        // stays null so the NEXT chunk re-enters this block and retries.
        // Previously: _streamUploadId was set before send — a failed send left it
        // set, so all subsequent chunks skipped this block entirely (orphaned stream).
        var _pendingUploadId = (crypto.randomUUID ? crypto.randomUUID() : Date.now().toString(36)).substring(0, 12);
        _streamChunkIndex = 0;
        // Clear any stale resubmit flag from a previous stream — a new stream
        // supersedes any pending resubmit. Without this, "⚠️ Reconnecting..." persists
        // across streams even after the issue is resolved.
        if (HC._streamNeedsResubmit) {
            log('[STT-Stream] Clearing stale resubmit flag (' + HC._streamNeedsResubmit + ') — new stream starting');
            HC._streamNeedsResubmit = null;
        }
        try {
            var _startSent = _gpuStreamSend({
                type: 'stream_start',
                stream_id: _pendingUploadId,
                mime_type: mediaRecorder ? mediaRecorder.mimeType : 'audio/webm',
                agent_id: HC.config.agentId || '',
            });
            if (!_startSent) {
                // Channel not ready — leave _streamUploadId null so next chunk retries
                log('[STT-Stream] stream_start send failed — will retry on next chunk');
                return;
            }
            // Commit stream ID only after successful send
            _streamUploadId = _pendingUploadId;
            _streamStartTimes[_streamUploadId] = Date.now();
            log('[STT-Stream] Started upload stream ' + _streamUploadId);
            // Send preroll WAV as the first data in the stream (captured during gate period)
            if (prerollBlob && prerollBlob.size > 0) {
                var _prerollReader = new FileReader();
                var _prerollStreamId = _streamUploadId;
                _prerollReader.onload = function() {
                    var bytes = new Uint8Array(_prerollReader.result);
                    var binary = '';
                    for (var i = 0; i < bytes.length; i += 8192) {
                        binary += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + 8192, bytes.length)));
                    }
                    var prerollB64 = btoa(binary);
                    _sttCapturePreroll = prerollB64;  // stash for fixture capture
                    try {
                        _gpuStreamSend({
                            type: 'stream_preroll',
                            stream_id: _prerollStreamId,
                            audio_b64: prerollB64,
                            mime_type: 'audio/wav',
                        });
                        log('[STT-Stream] Sent preroll WAV (' + bytes.length + ' bytes)');
                    } catch(e) { log('[STT-Stream] Preroll send failed: ' + e.message); }
                };
                _prerollReader.readAsArrayBuffer(prerollBlob);
                prerollBlob = null;
            }
        } catch(e) { /* transport managed by channel */ return; }
    }
    // Stream already active — send chunk even if WS reconnected (same stream_id, GPU has headers)
    // Send chunk with retry on failure
    var chunkIdx = _streamChunkIndex++;
    var streamId = _streamUploadId;

    // Tenacious chunk sending: try once, if fail → buffer for resubmit.
    // Never mark chunks as LOST. The resubmit flow will send ALL buffered
    // chunks when the connection recovers.
    var reader = new FileReader();
    reader.onload = function() {
        // Chunked base64 encoding — String.fromCharCode.apply has argument limit (~32K on Safari)
        var bytes = new Uint8Array(reader.result);
        var binary = '';
        for (var i = 0; i < bytes.length; i += 8192) {
            binary += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + 8192, bytes.length)));
        }
        var b64 = btoa(binary);
        // Always save for retransmission/resubmit
        if (!_streamSentChunks[streamId]) _streamSentChunks[streamId] = {};
        _streamSentChunks[streamId][chunkIdx] = b64;
        if (!_streamChunkTimes[streamId]) _streamChunkTimes[streamId] = {};
        _streamChunkTimes[streamId][chunkIdx] = _streamStartTimes[streamId]
            ? Date.now() - _streamStartTimes[streamId] : chunkIdx * 250;

        var sent = false;
        try {
            if (_gpuStreamReady()) {
                sent = _gpuStreamSend({
                    type: 'stream_chunk',
                    stream_id: streamId,
                    chunk_index: chunkIdx,
                    audio_b64: b64,
                });
            }
        } catch(e) {}

        if (sent) {
            _streamChunksSent++;
            // If finalize is waiting for this chunk, send it after a tick
            if (_streamPendingFinalize && _streamPendingFinalize.streamId === streamId
                && _streamChunksSent >= _streamPendingFinalize.expected) {
                var _pf = _streamPendingFinalize;
                _streamPendingFinalize = null;
                _streamChunksSent = 0;
                setTimeout(function() {
                    log('[STT-Stream] Last chunk flushed — sending deferred finalize (' + _pf.expected + ' chunks)');
                    _gpuStreamSend(_pf.data);
                }, 0);
            }
        } else {
            // Chunk buffered but not sent — connection down.
            // Flag for resubmit when connection recovers.
            if (!HC._streamNeedsResubmit) {
                HC._streamNeedsResubmit = streamId;
                HC._streamResubmitChunkCount = _streamChunkIndex;
                log('[STT-Stream] Chunk ' + chunkIdx + ' buffered — connection down, will resubmit on reconnect');
                setVoiceStatus('recording', '\u26a0\ufe0f Reconnecting...');
                // Trigger reconnect — via setTimeout to avoid Safari iOS audio callback deadlock
                /* reconnection handled by gpuChannel */
            }
        }
    };
    reader.readAsArrayBuffer(blob);
}

/// Handle retransmit_request from GPU sidecar — resend missing chunks
function _handleRetransmitRequest(streamId, missing) {
    var saved = _streamSentChunks[streamId];
    if (!saved) {
        log('[STT-Stream] Retransmit request for unknown stream ' + streamId);
        return;
    }
    log('[STT-Stream] Retransmitting ' + missing.length + ' chunks for ' + streamId + ': [' + missing.join(',') + ']');
    for (var i = 0; i < missing.length; i++) {
        var idx = missing[i];
        var b64 = saved[idx];
        if (b64) {
            // Use _gpuStreamSend — transport lock stays alive until result callback
            _gpuStreamSend({
                type: 'stream_chunk',
                stream_id: streamId,
                chunk_index: idx,
                audio_b64: b64,
            });
        } else {
            log('[STT-Stream] Chunk ' + idx + ' not in retransmit buffer');
        }
    }
}
// Expose for gpu.js to call
HC._handleRetransmitRequest = _handleRetransmitRequest;


// ── Streaming Segmentation ──
// Cut every ~30s at the quietest moment, send each chunk for transcription
const SEGMENT_TARGET_SEC = 20;
const SEARCH_ZONE_SEC = 5;
const CUT_WINDOW_MS = 300;
let _segmentStart = null;
let _allWebmChunks = [];       // ALL raw webm chunks from entire recording
let _processedSamples = 0;     // PCM samples already sent for transcription
let _segmentProcessing = false;
let _partialResults = [];
let _segmentBlobs = [];
let _cumulativeOffset = 0;
let _segmentPromises = [];   // Promise[] — tracks in-flight segment transcriptions

// Reset all segmentation state — shared across all paths
function _resetSegmentationState() {
    _partialResults = [];
    _segmentBlobs = [];
    _segmentPromises = [];
    _filesVisitedDuringRecording = [];
    _cumulativeOffset = 0;
    _allWebmChunks = [];
    _processedSamples = 0;
}

// Find quietest 300ms window in the last SEARCH_ZONE_SEC of decoded PCM
function _findQuietestCutSample(samples, sampleRate) {
    const searchStart = Math.max(0, samples.length - Math.floor(sampleRate * SEARCH_ZONE_SEC));
    const windowSamples = Math.floor(sampleRate * CUT_WINDOW_MS / 1000);
    const stepSamples = Math.max(1, Math.floor(windowSamples / 3));
    let bestRms = Infinity, bestMid = searchStart + Math.floor(windowSamples / 2);
    for (let i = searchStart; i + windowSamples <= samples.length; i += stepSamples) {
        let sumSq = 0;
        for (let j = i; j < i + windowSamples; j++) { sumSq += samples[j] * samples[j]; }
        const rms = Math.sqrt(sumSq / windowSamples);
        if (rms < bestRms) { bestRms = rms; bestMid = i + Math.floor(windowSamples / 2); }
    }
    return bestMid;
}

// Encode Float32Array to WAV Blob (16-bit PCM)
function _float32ToWavBlob(samples, sampleRate) {
    const buf = new ArrayBuffer(44 + samples.length * 2);
    const v = new DataView(buf);
    const w = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
    w(0, 'RIFF'); v.setUint32(4, 36 + samples.length * 2, true);
    w(8, 'WAVE'); w(12, 'fmt ');
    v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
    v.setUint32(24, sampleRate, true); v.setUint32(28, sampleRate * 2, true);
    v.setUint16(32, 2, true); v.setUint16(34, 16, true);
    w(36, 'data'); v.setUint32(40, samples.length * 2, true);
    let off = 44;
    for (let i = 0; i < samples.length; i++) {
        const s = Math.max(-1, Math.min(1, samples[i]));
        v.setInt16(off, s < 0 ? s * 0x8000 : s * 0x7FFF, true);
        off += 2;
    }
    return new Blob([buf], { type: 'audio/wav' });
}

// Flush accumulated chunks, decode FULL webm, slice new audio, find quiet point, split, send
async function _flushAndSplitSegment() {
    try {
        // Grab flushed chunks and ADD to cumulative webm (don't clear — keep for full decode)
        const chunks = audioChunks.slice();
        audioChunks = [];
        _allWebmChunks.push(...chunks);
        if (_allWebmChunks.length === 0) { _segmentProcessing = false; return; }

        // Decode the FULL webm blob (all chunks since recording started)
        const decodeCtx = new (window.AudioContext || window.webkitAudioContext)();
        const fullBlob = new Blob(_allWebmChunks, { type: (_allWebmChunks[0] && _allWebmChunks[0].type) || 'audio/webm' });
        const decoded = await decodeCtx.decodeAudioData(await fullBlob.arrayBuffer());
        const allSamples = decoded.getChannelData(0);
        const sr = decoded.sampleRate;

        // Slice out only the NEW samples (after what we've already processed)
        const newSamples = allSamples.slice(_processedSamples);
        if (newSamples.length === 0) { _segmentProcessing = false; decodeCtx.close(); return; }

        const totalDurSec = newSamples.length / sr;
        log('[STT] Segment: ' + totalDurSec.toFixed(1) + 's new PCM (' +
            newSamples.length + ' samples @ ' + sr + 'Hz, ' +
            _processedSamples + ' already processed)');

        // Find quietest 300ms window in last 10s of the new samples
        const cutSample = _findQuietestCutSample(newSamples, sr);
        const cutTimeSec = cutSample / sr;
        const carryDurSec = (newSamples.length - cutSample) / sr;

        log('[STT] Segment split at ' + cutTimeSec.toFixed(2) + 's — sending ' +
            cutTimeSec.toFixed(1) + 's, retaining ' + carryDurSec.toFixed(1) + 's carryover');

        // Split: encode first part to WAV for GPU, advance processed counter
        const sendBlob = _float32ToWavBlob(newSamples.slice(0, cutSample), sr);
        _processedSamples += cutSample;  // mark sent samples as processed (carryover stays unprocessed)

        // Save for full-audio diarization later
        _segmentBlobs.push(sendBlob);

        // Send to transcription (no IDB save for intermediate segments — only final audio persists)
        const seq = ++_sttSeq;
        _segmentPromises.push(_doSegmentTranscribe(seq, sendBlob, cutTimeSec.toFixed(1)));

        decodeCtx.close();
    } catch (err) {
        log('[STT] Segment split failed: ' + err.message);
    } finally {
        _segmentProcessing = false;
    }
}

// Transcribe a segment chunk (reuses GPU pipeline but accumulates instead of injecting)
async function _doSegmentTranscribe(seq, audioBlob, durSec, _retryAttempt) {
    if (!HC.GPU_URL && !LOCAL_HC.GPU_URL) { log('[STT] Segment: GPU not configured'); return; }
    var retryAttempt = _retryAttempt || 0;
    const segT0 = performance.now();
    try {
        const buf = await audioBlob.arrayBuffer();
        const bytes = new Uint8Array(buf);
        let binary = '';
        for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        const b64 = btoa(binary);

        const preset = document.getElementById('stt-preset').value || 'multilingual';

        log('[STT] Segment transcribe: ' + durSec + 's chunk (' + (audioBlob.size/1024).toFixed(0) + 'KB ' + audioBlob.type + ') preset=' + preset + (retryAttempt ? ' retry=' + retryAttempt : ''));
        const _segDiarizeOn = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
        const _segFixT0 = performance.now();
        const gpu = await HC.gpuRun({ action: 'transcribe', audio_b64: b64, language: 'en', preset: preset, skip_align: !_segDiarizeOn });
        const jobId = gpu.data.id;
        const segWsCtrl = gpu.wsController || null;
        log('[STT] Segment job=' + jobId + ' backend=' + gpu.backend + ' submit=' + Math.round(performance.now() - segT0) + 'ms');
        HC.gpuJobStart(jobId, 'transcribe');

        let output = {};
        let done = false;
        let resultSource = 'none';
        const t0 = performance.now();

        if (segWsCtrl) {
            // ── WebSocket path ──
            await new Promise(function(resolve) {
                segWsCtrl.onAccepted = function(msg) { HC.gpuJobAccepted(jobId, msg.state, msg.gpu_worker_id); };
                segWsCtrl.onProgress = function(p) { log('[STT] Segment progress: ' + p.stage); };
                segWsCtrl.onResult = function(result) { output = result; done = true; resultSource = 'ws'; resolve(); };
                segWsCtrl.onError = function(err) { log('[STT] Segment WS error: ' + err); done = true; resolve(); };
                segWsCtrl.onDone = function() { if (!done) { done = true; resolve(); } };
            });
            HC.gpuJobEnd(jobId, 'success');
        } else {
        // ── HTTP polling path ──
        const streamUrl = HC.gpuStreamUrl(gpu.baseUrl, jobId);
        const statusUrl = HC.gpuStatusUrl(gpu.baseUrl, jobId);
        const pollHeaders = HC.gpuPollHeaders(gpu.token);
        let pollCount = 0;

        while (!done) {
            await new Promise(r => setTimeout(r, 300));
            pollCount++;
            try {
                const sResp = await fetch(streamUrl, { headers: pollHeaders });
                if (!sResp.ok) {
                    if (sResp.status === 404) {
                        const segElapsed = Math.round((performance.now() - t0) / 1000);
                        if (segElapsed >= 3) setVoiceStatus('sending', '\u2615 GPU warming up... ' + segElapsed + 's');
                        continue;
                    }
                    continue;
                }
                const sData = await sResp.json();
                const items = sData.stream || (Array.isArray(sData) ? sData : []);
                for (const item of items) {
                    const ev = item.output || item;
                    if (ev.status === 'accepted' && ev.state) {
                        HC.gpuJobAccepted(jobId, ev.state, ev.gpu_worker_id);
                        continue;
                    }
                    if (ev.progress) continue;
                    if (ev.text !== undefined || ev.medium_elapsed !== undefined) {
                        output = ev; done = true; resultSource = 'stream';
                        break;
                    }
                }
                if (!done && (sData.status === 'COMPLETED' || sData.status === 'FAILED')) {
                    log('[STT] Segment: stream ' + sData.status + ' items=' + items.length +
                        ' output_empty=' + (Object.keys(output).length === 0) + ' polls=' + pollCount);
                    done = true;
                }
            } catch(e) { log('[STT] Segment poll error: ' + e.message); }
            if (!done && (performance.now() - t0) > 30000) {
                log('[STT] Segment: stream timeout 30s, polls=' + pollCount);
                done = true;
            }
        }

        // Fallback: if stream didn't yield result, fetch from /status
        if (Object.keys(output).length === 0) {
            log('[STT] Segment: no result from stream after ' + pollCount + ' polls — fetching /status/' + jobId);
            try {
                const stResp = await fetch(statusUrl, { headers: pollHeaders });
                if (stResp.ok) {
                    const stData = await stResp.json();
                    log('[STT] Segment /status: status=' + stData.status + ' hasOutput=' + !!stData.output +
                        ' outputType=' + (Array.isArray(stData.output) ? 'array[' + stData.output.length + ']' : typeof stData.output));
                    if (stData.status === 'COMPLETED' && stData.output) {
                        output = stData.output;
                        if (Array.isArray(output)) output = output[output.length - 1] || {};
                        resultSource = 'status';
                    } else if (stData.status === 'FAILED') {
                        log('[STT] Segment: GPU job FAILED — ' + JSON.stringify(stData.error || '').substring(0, 200));
                    }
                }
            } catch(e) { log('[STT] Segment status error: ' + e.message); }
        }

        HC.gpuJobEnd(jobId, 'success');
        } // end HTTP polling else block
        const pollMs = Math.round(performance.now() - t0);
        const totalMs = Math.round(performance.now() - segT0);
        log('[STT] Segment poll done: source=' + resultSource + ' polls=' + (typeof pollCount !== 'undefined' ? pollCount : 0) + ' poll=' + pollMs + 'ms total=' + totalMs + 'ms' +
            ' text=' + (output.text ? output.text.length + 'ch' : 'EMPTY') +
            ' wx=' + (output.whisperx_text ? output.whisperx_text.length + 'ch' : 'EMPTY') +
            ' m_elapsed=' + (output.medium_elapsed || 0) + ' wx_elapsed=' + (output.whisperx_elapsed || 0));

        const mediumText = (output.text || '').trim();
        const wxText = (output.whisperx_text || '').trim();
        const segments = output.segments || [];
        const wordSegs = output.word_segments || [];
        if (!mediumText && !wxText) {
            if (retryAttempt < 1) {
                log('[STT] Segment: both empty — retrying on alternate backend (attempt ' + (retryAttempt + 1) + ')');
                return _doSegmentTranscribe(seq, new Blob([buf], { type: audioBlob.type }), durSec, retryAttempt + 1);
            }
            // Auto-capture on empty result after retry — enables offline replay for debugging
            _saveFixtureToIdb({
                id: 'seg-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 6),
                version: 1, type: 'segment', captured_at: Date.now(),
                trigger: 'empty_result',
                audio: { mime_type: audioBlob.type, audio_b64: b64 },
                request: { action: 'transcribe', language: 'en', preset: preset, skip_align: !_segDiarizeOn,
                           agent_id: (HC.config && HC.config.agentId) || '' },
                result: { text: '', whisperx_text: '', backend: gpu.backend, timing_ms: Math.round(performance.now() - _segFixT0), empty: true },
                meta: { gpu_url: HC.GPU_URL || '',
                        dur_sec: durSec, seq: seq },
            });
            log('[STT] Segment: both empty after retry — notifying daemon as lost');
            _handleSttLost(seq, new Blob([buf], { type: audioBlob.type }), durSec, 0);
            return;
        }
        // Capture all results when debug flag is set
        if (_shouldCaptureFixture()) {
            _saveFixtureToIdb({
                id: 'seg-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 6),
                version: 1, type: 'segment', captured_at: Date.now(),
                trigger: 'all',
                audio: { mime_type: audioBlob.type, audio_b64: b64 },
                request: { action: 'transcribe', language: 'en', preset: preset, skip_align: !_segDiarizeOn,
                           agent_id: (HC.config && HC.config.agentId) || '' },
                result: { text: mediumText, whisperx_text: wxText, backend: gpu.backend,
                          timing_ms: Math.round(performance.now() - _segFixT0), empty: false },
                meta: { gpu_url: HC.GPU_URL || '',
                        dur_sec: durSec, seq: seq },
            });
        }

        // Adjust word-level timestamps from chunk-relative to full-audio-absolute
        const chunkOffset = _cumulativeOffset;
        const adjustedWordSegs = wordSegs.map(ws => ({
            word: ws.word,
            start: Math.round((ws.start + chunkOffset) * 1000) / 1000,
            end: Math.round((ws.end + chunkOffset) * 1000) / 1000,
        }));
        _cumulativeOffset += parseFloat(durSec) || 0;

        // Accumulate partial result
        _partialResults.push({
            text: mediumText,
            wxText: wxText,
            segments: segments,
            wordSegments: adjustedWordSegs,
            chunkOffset: chunkOffset,
            mediumElapsed: output.medium_elapsed || 0,
            wxElapsed: output.whisperx_elapsed || 0,
            embSim: output.embedding_similarity,
            energy: _speechEnergySamples > 0 ? _speechEnergySum / _speechEnergySamples : undefined,
        });

        // Show accumulated text in status area (textarea placeholder)
        const allText = _partialResults.map(r => r.text).join(' ');
        setVoiceStatus('transcribing', '\ud83c\udfa4 ' + allText);
        log('[STT] Segment result: "' + mediumText.substring(0,40) + '" (' + _partialResults.length +
            ' chunks, ' + adjustedWordSegs.length + ' words accumulated)');
    } catch (e) {
        log('[STT] Segment transcribe error: ' + e.message);
    }
}

// Diarize full audio using accumulated word segments — returns {text, elapsed, speakerMap} or null on failure
async function _diarizeFullAudio() {
    const allWordSegments = _partialResults.flatMap(r => r.wordSegments || []);
    if ((!HC.GPU_URL && !LOCAL_HC.GPU_URL) || _segmentBlobs.length === 0 || allWordSegments.length === 0) return null;

    setVoiceStatus('transcribing', '\ud83d\udd0a Diarizing speakers...');
    try {
        // Use the original compressed webm recording (much smaller than concatenated WAV segments)
        const fullBlob = _allWebmChunks.length > 0
            ? new Blob(_allWebmChunks, { type: 'audio/webm' })
            : new Blob(_segmentBlobs, { type: _segmentBlobs[0]?.type || 'audio/wav' });
        const buf = await fullBlob.arrayBuffer();
        const bytes = new Uint8Array(buf);
        let binary = '';
        for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        const fullB64 = btoa(binary);

        log('[STT] Diarize: sending ' + (fullBlob.size/1024).toFixed(0) + 'KB audio + ' +
            allWordSegments.length + ' word segments to GPU');

        const gpu = await HC.gpuRun({
            action: 'diarize',
            audio_b64: fullB64,
            agent_id: HC.config.agentId,
            word_segments: allWordSegments,
        });
        const jobId = gpu.data.id;
        log('[STT] Diarize job=' + jobId + ' backend=' + gpu.backend);

        // Poll for diarization result
        const streamUrl = HC.gpuStreamUrl(gpu.baseUrl, jobId);
        const statusUrl = HC.gpuStatusUrl(gpu.baseUrl, jobId);
        const pollHeaders = HC.gpuPollHeaders(gpu.token);
        let diarizeOutput = {};
        let done = false;
        const t0 = performance.now();
        let consecutiveFails = 0;

        while (!done) {
            await new Promise(r => setTimeout(r, 500));
            // Hard timeout: 2 minutes max
            if ((performance.now() - t0) > 120000) {
                log('[STT] Diarize poll timeout (2 min)');
                break;
            }
            try {
                const sResp = await fetch(streamUrl, { headers: pollHeaders });
                if (!sResp.ok) { if (sResp.status === 404) continue; continue; }
                consecutiveFails = 0; // successful fetch resets counter
                const sData = await sResp.json();
                const items = sData.stream || (Array.isArray(sData) ? sData : []);
                for (const item of items) {
                    const ev = item.output || item;
                    if (ev.progress) {
                        const stage = ev.progress.stage || '';
                        setVoiceStatus('transcribing', '\ud83d\udd0a ' + stage.replace(/_/g, ' '));
                        continue;
                    }
                    if (ev.speaker_segments !== undefined || ev.word_speakers !== undefined) {
                        diarizeOutput = ev; done = true; break;
                    }
                }
                if (!done && (sData.status === 'COMPLETED' || sData.status === 'FAILED')) done = true;
            } catch(e) {
                consecutiveFails++;
                if (consecutiveFails <= 3) log('[STT] Diarize poll error: ' + e.message);
                if (consecutiveFails >= 10) {
                    log('[STT] Diarize poll: 10 consecutive failures, giving up');
                    break;
                }
            }
        }

        const speakerMap = diarizeOutput.speaker_map || {};
        const wordSpeakers = diarizeOutput.word_speakers || [];
        const mapEntries = Object.entries(speakerMap).map(([k,v]) => k + '=' + v.name + '(' + (v.score*100).toFixed(0) + '%)');
        log('[STT] Diarize complete: ' + (diarizeOutput.speaker_segments || []).length +
            ' segments, speakers: ' + (mapEntries.join(', ') || 'none'));

        return {
            text: _mergeSpeakerLabels(wordSpeakers, speakerMap),
            elapsed: diarizeOutput.elapsed || 0,
            speakerMap: speakerMap,
        };
    } catch (err) {
        log('[STT] Diarization failed: ' + err.message);
        return null;
    }
}

// Inject finalized transcription (called after diarization or directly if diarize disabled)
var _injectionInProgress = false;
async function _injectFinalTranscription(diarizeResult) {
    if (_injectionInProgress) {
        log('[STT] Injection already in progress — skipping duplicate');
        return;
    }
    _injectionInProgress = true;
    try {
    log('[STT] Injecting: ' + _partialResults.length + ' chunks');

    const combinedMedium = _partialResults.map(r => r.text).join(' ');
    const combinedWx = _partialResults.map(r => r.wxText).join(' ');

    // Guard: don't inject if both models returned empty text
    if (!combinedMedium.trim() && !combinedWx.trim() && !(diarizeResult && diarizeResult.text && diarizeResult.text.trim())) {
        log('[STT] Discarded: all models returned empty text (0 content)');
        _resetSegmentationState();
        return;
    }

    // Garbage filter — same check as the non-streaming path
    if (HC._garbageEntries && HC._garbageEntries.length > 0) {
        try {
            setVoiceStatus('sending', '\ud83d\udd0d Checking garbage filter...');
            const avgEmbSim = _partialResults.filter(r => typeof r.embSim === 'number');
            const effectiveEmbSim = avgEmbSim.length > 0
                ? avgEmbSim.reduce((s, r) => s + r.embSim, 0) / avgEmbSim.length : 0.40;
            const dynamicThreshold = garbageThreshold(effectiveEmbSim);
            const textsToCheck = [combinedMedium, combinedWx].filter(t => t.trim().length >= 3);
            if (textsToCheck.length > 0) {
                setVoiceStatus('sending', '\ud83e\udde0 Computing embeddings...');
                const submitResult = await HC.gpuRun({ action: 'embed', texts: textsToCheck, task_type: 'search_query' });
                if (submitResult && submitResult.wsController) {
                    const embedOutput = await new Promise(function(resolve) {
                        var timer = setTimeout(function() { resolve(null); }, 10000);
                        submitResult.wsController.onResult = function(output) { clearTimeout(timer); resolve(output); };
                        submitResult.wsController.onError = function() { clearTimeout(timer); resolve(null); };
                    });
                    if (embedOutput) setVoiceStatus('sending', '\ud83d\udd0d Comparing similarity...');
                    const vecs = embedOutput && embedOutput.embeddings;
                    if (vecs && vecs.length > 0) {
                        var match = garbageCheck(vecs, textsToCheck, dynamicThreshold, 'stream');
                        if (match) {
                            log('[STT] Filtered: "' + match.text.substring(0,40) + '" (sim=' + match.sim.toFixed(2) + ', thresh=' + dynamicThreshold.toFixed(2) + ')');
                            HC.setVoiceStatus('idle', '🗑️ Filtered: "' + match.text.substring(0,25) + '…"');
                            setTimeout(function() { HC.setVoiceStatus('idle', ''); }, 3000);
                            _resetSegmentationState();
                            return;
                        }
                    }
                }
            }
        } catch(e) {
            if (e.message && !e.message.includes('No GPU')) log('[STT] Garbage check (stream) error: ' + e.message);
        }
    }

    const totalElapsedM = _partialResults.reduce((s, r) => s + r.mediumElapsed, 0);
    const totalElapsedX = _partialResults.reduce((s, r) => s + r.wxElapsed, 0);
    const totalDur = _cumulativeOffset.toFixed(1);
    const avgEmbSim = _partialResults.filter(r => typeof r.embSim === 'number');
    const embSimStr = avgEmbSim.length > 0
        ? Math.round(avgEmbSim.reduce((s, r) => s + r.embSim, 0) / avgEmbSim.length * 100) + '%'
        : 'n/a';

    const diarizedText = diarizeResult ? diarizeResult.text : '';
    const diarizeElapsed = diarizeResult ? diarizeResult.elapsed : 0;

    // Text model = clean text (no speaker tags), Voices model = speaker-tagged text
    let injection = '';
    const avgEnergyParts = _partialResults.filter(r => typeof r.energy === 'number');
    const avgEnergyStr = avgEnergyParts.length > 0
        ? ', energy: ' + Math.round(avgEnergyParts.reduce((s, r) => s + r.energy, 0) / avgEnergyParts.length)
        : '';
    const closingTag = '[audio length: ' + totalDur + 's, self-similarity: ' + embSimStr +
        avgEnergyStr +
        (diarizeElapsed ? ', diarize: ' + diarizeElapsed.toFixed(1) + 's' : '') + ']';

    // Append files visited during recording
    const filesTag = _filesVisitedDuringRecording.length > 0
        ? ' [files viewed: ' + _filesVisitedDuringRecording.join(', ') + ']'
        : '';

    const voicesText = diarizedText || combinedWx;
    // Injection format: clean text first, metadata at end in square brackets.
    // User sees the first few words (clean text). Agent sees both models.
    // Overlay popup shows full metadata on demand.
    const metaParts = [
        totalElapsedM.toFixed(1) + 's',
        voicesText ? totalElapsedX.toFixed(1) + 's' : null,
        totalDur + 's audio',
        embSimStr + ' sim',
    ].filter(Boolean).join(', ');
    if (voicesText) {
        injection = (combinedMedium || '(empty)') +
            ' | ' + voicesText +
            ' [' + metaParts + ']' + filesTag;
    } else {
        injection = (combinedMedium || '(empty)') + ' [' + metaParts + ']' + filesTag;
    }

    // Store full transcription metadata for overlay popup
    if (!HC._transcriptionHistory) HC._transcriptionHistory = [];
    HC._transcriptionHistory.push({
        textModel: combinedMedium || '',
        voicesModel: voicesText || '',
        elapsedM: totalElapsedM,
        elapsedX: totalElapsedX,
        audioDur: totalDur,
        similarity: embSimStr,
        injection: injection,
        ts: Date.now(),
    });
    // Ring buffer: keep last 30
    while (HC._transcriptionHistory.length > 30) HC._transcriptionHistory.shift();

    if (injection.trim()) {
        lastTranscriptionTime = Date.now();
        setVoiceStatus('sending', '\ud83d\udce5 Injecting transcription...');
        log('[STT] Injecting segmented transcription (' + _partialResults.length + ' chunks): ' + injection.substring(0, 80));
        await HC.daemonFetch('/api/transcription', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ text: injection }),
        });
        setVoiceStatus('success', '\u2705 Injecting: ' + (combinedMedium || combinedWx));
        revertVoiceStatusAfter(2000, 'success', '\u2705 Transcribed');
        revertVoiceStatusAfter(2000, '', isListening ? LISTENING_HTML : '');
        HC.spendCredit(); // instant local decrement + background server sync
    }

    // Clean up
    _resetSegmentationState();
    } finally {
        _injectionInProgress = false;
    }
}

// Merge speaker labels onto accumulated word-level text using diarization results
function _mergeSpeakerLabels(wordSpeakers, speakerMap) {
    if (!wordSpeakers || wordSpeakers.length === 0) return '';
    let parts = [];
    let prevSpeaker = '';
    for (const ws of wordSpeakers) {
        const rawSpk = ws.speaker || '';
        const resolved = (rawSpk && speakerMap[rawSpk]) ? speakerMap[rawSpk].name : rawSpk;
        if (resolved && resolved !== prevSpeaker) {
            parts.push('[' + resolved + '] ' + ws.word);
            prevSpeaker = resolved;
        } else {
            parts.push(ws.word);
        }
    }
    return parts.join(' ').replace(/\s+/g, ' ').trim();
}

// Enable mic button if getUserMedia is available OR we're inside a native container
if ((navigator.mediaDevices && navigator.mediaDevices.getUserMedia) ||
    (window.NativeBridge && window.NativeBridge.isAvailable)) {
    micBtn.disabled = false;
}

var _micToggleBusy = false;
micBtn.addEventListener('click', async () => {
    // Debounce: prevent rapid toggling while async start/stop is in progress
    if (_micToggleBusy) {
        log('[Mic] Toggle debounced — previous toggle still in progress');
        return;
    }
    _micToggleBusy = true;
    try {
        if (isListening) {
            stopListening();
        } else {
            await startListening();
        }
    } finally {
        // Release after a brief delay to prevent double-tap issues
        setTimeout(function() { _micToggleBusy = false; }, 500);
    }
});

// AudioWorklet processor as inline Blob URL
const workletCode = `
class PrerollProcessor extends AudioWorkletProcessor {
    constructor() {
super();
this.buffer = [];
this.maxSamples = 48000 * 2;
this.port.onmessage = (event) => {
    if (event.data.maxSamples) this.maxSamples = event.data.maxSamples;
    if (event.data.getBuffer) this.port.postMessage({ buffer: this.buffer.slice() });
    if (event.data.clear) this.buffer = [];
};
    }
    process(inputs, outputs) {
const input = inputs[0];
const output = outputs[0];
if (input && input[0]) {
    for (let i = 0; i < input[0].length; i++) this.buffer.push(input[0][i]);
    if (this.buffer.length > this.maxSamples)
        this.buffer = this.buffer.slice(-this.maxSamples);
    // Pass through to output so browser keeps calling process()
    if (output && output[0]) output[0].set(input[0]);
}
return true;
    }
}
registerProcessor('preroll-processor', PrerollProcessor);`;

async function startListening() {
    const t0 = performance.now();
    const useNativeBridge = window.NativeBridge && window.NativeBridge.isAvailable;

    if (useNativeBridge) {
        // Native app path: request mic from NativeBridge (no getUserMedia)
        log('[Mic] Using NativeBridge (native mic ownership)');
        window.NativeBridge.requestMicStream();
    } else {
        // Browser path: standard getUserMedia
        try {
            micStream = await navigator.mediaDevices.getUserMedia({
                audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: false }
            });
            log('[Mic] getUserMedia: ' + Math.round(performance.now() - t0) + 'ms');
        } catch(e) {
            log('[Mic] Permission denied: ' + e.message);
            setVoiceStatus('recording', '\u274c Microphone access denied');
            revertVoiceStatusAfter(4000, '', '');
            return;
        }
    }
    try {
        audioContext = new (window.AudioContext || window.webkitAudioContext)({
            sampleRate: useNativeBridge ? 16000 : undefined
        });
        // iOS Safari: AudioContext may start suspended even from a user gesture.
        // Explicit resume ensures the analyser receives data and the level strip works.
        if (audioContext.state === 'suspended') {
            await audioContext.resume();
            log('[Mic] AudioContext resumed from suspended (state=' + audioContext.state + ')');
        }

        const gainNode = audioContext.createGain();
        gainNode.gain.value = 1;  // No ramp — immediate

        analyser = audioContext.createAnalyser();
        analyser.fftSize = 512;

        if (useNativeBridge) {
            // Create a ScriptProcessorNode that receives audio from NativeBridge
            // Buffer size 4096 at 16kHz = 256ms chunks
            var _nativeBridgeNode = audioContext.createScriptProcessor(4096, 1, 1);
            var _nativeBridgeBuffer = [];

            window.NativeBridge.onAudioData(function(float32) {
                // Accumulate incoming audio chunks
                for (var i = 0; i < float32.length; i++) _nativeBridgeBuffer.push(float32[i]);
            });

            _nativeBridgeNode.onaudioprocess = function(e) {
                var output = e.outputBuffer.getChannelData(0);
                var available = Math.min(output.length, _nativeBridgeBuffer.length);
                for (var i = 0; i < available; i++) {
                    output[i] = _nativeBridgeBuffer[i];
                }
                // Fill remainder with silence
                for (var j = available; j < output.length; j++) {
                    output[j] = 0;
                }
                // Remove consumed samples
                _nativeBridgeBuffer.splice(0, available);
            };

            _nativeBridgeNode.connect(gainNode);
            gainNode.connect(analyser);
            // Store reference for cleanup
            window._nativeBridgeNode = _nativeBridgeNode;
        } else {
            const microphone = audioContext.createMediaStreamSource(micStream);
            microphone.connect(gainNode);
            gainNode.connect(analyser);
        }

        // AudioWorklet for pre-roll buffer
        const prerollSecs = getPrerollSeconds();
        const maxSamples = audioContext.sampleRate * prerollSecs;
        const blob = new Blob([workletCode], { type: 'application/javascript' });
        const url = URL.createObjectURL(blob);
        await audioContext.audioWorklet.addModule(url);
        URL.revokeObjectURL(url);
        prerollNode = new AudioWorkletNode(audioContext, 'preroll-processor');
        prerollNode.port.postMessage({ maxSamples });
        gainNode.connect(prerollNode);

        // Recorder destination — module scope for zero-gap restart
        recorderDest = audioContext.createMediaStreamDestination();
        gainNode.connect(recorderDest);
        mediaRecorder = new MediaRecorder(recorderDest.stream, {
            mimeType: MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
                ? 'audio/webm;codecs=opus' : 'audio/webm'
        });
        _streamUploadId = null;
        _streamChunkIndex = 0;
        _streamChunksSent = 0;
        _streamPendingFinalize = null;
        mediaRecorder.ondataavailable = (e) => {
            if (e.data.size > 0) {
                audioChunks.push(e.data);
                // Stream compressed webm chunks to GPU during recording
                _streamAudioChunk(e.data);
            }
        };
        mediaRecorder.onstop = handleRecorderStop;

        isListening = true;
        micBtn.classList.add('recording');
        const fsMicOn = document.getElementById('fs-mic');
        if (fsMicOn) fsMicOn.classList.add('recording');
        micIcon.textContent = '\u23f9';
        setVoiceStatus('', LISTENING_HTML);
        const strip = document.getElementById('mic-level-strip');
        if (strip) strip.className = 'mic-level-strip active';
        // Add threshold marker if not present
        // The marker is a draggable circle on the mic level strip.
        // Drag it right for noisy environments (raise silence threshold).
        // Log scale with floor at 2: usable range 2-80, most of the strip is the useful range.
        if (strip && !document.getElementById('mic-threshold-marker')) {
            var marker = document.createElement('div');
            marker.id = 'mic-threshold-marker';
            // Tall touch target (30px) centered on the strip, visible circle is 10x10
            marker.style.cssText = 'position:absolute;width:10px;height:10px;background:rgba(255,255,255,0.9);border:1px solid rgba(0,0,0,0.3);border-radius:50%;top:50%;cursor:ew-resize;z-index:10;transform:translate(-50%,-50%);touch-action:none;box-shadow:0 0 3px rgba(0,0,0,0.3);';
            // Invisible touch area (wider + taller for fat fingers)
            var touchArea = document.createElement('div');
            touchArea.style.cssText = 'position:absolute;width:40px;height:40px;top:50%;left:50%;transform:translate(-50%,-50%);cursor:ew-resize;touch-action:none;';
            marker.appendChild(touchArea);
            // Log scale with floor at 2, ceiling at 200: pct = log(threshold/2) / log(100) * 100
            function threshToPct(t) { return Math.min(100, t / 2); }
            function pctToThresh(p) { return Math.max(2, Math.min(200, Math.round(p * 2))); }
            marker.style.left = threshToPct(SPEECH_THRESHOLD) + '%';
            strip.style.position = 'relative';
            strip.style.overflow = 'visible';
            strip.appendChild(marker);
            // Drag handling — listen on the strip itself for easier targeting
            var dragging = false;
            function onMove(clientX) {
                var rect = strip.getBoundingClientRect();
                var pct = Math.max(0, Math.min(100, (clientX - rect.left) / rect.width * 100));
                var newThreshold = pctToThresh(pct);
                SPEECH_THRESHOLD = newThreshold;
                SILENCE_THRESHOLD = newThreshold;
                if (HC.saveLayoutSetting) HC.saveLayoutSetting('speech_threshold', newThreshold);
                marker.style.left = threshToPct(newThreshold) + '%';
            }
            // Touch + mouse on both marker AND strip (tap anywhere on strip to set threshold)
            strip.addEventListener('touchstart', function(e) {
                dragging = true;
                onMove(e.touches[0].clientX);
                e.preventDefault();
            }, { passive: false });
            strip.addEventListener('mousedown', function(e) {
                dragging = true;
                onMove(e.clientX);
                e.preventDefault();
            });
            document.addEventListener('touchmove', function(e) { if (dragging) onMove(e.touches[0].clientX); }, { passive: true });
            document.addEventListener('mousemove', function(e) { if (dragging) onMove(e.clientX); });
            document.addEventListener('touchend', function() { dragging = false; });
            document.addEventListener('mouseup', function() { dragging = false; });
        }
        log('[Mic] Ready: ' + Math.round(performance.now() - t0) + 'ms total');
        monitorAudio();
    } catch(e) { log('[Mic] Error: ' + e.message); }
}

function stopListening() {
    isListening = false;
    // Save duration before async stop — same fix as silence timeout path
    if (isSpeaking && speechStart) {
        _lastSpeechDurSec = ((Date.now() - speechStart) / 1000).toFixed(1);
    }
    if (mediaRecorder && mediaRecorder.state === 'recording') {
        mediaRecorder.stop();
        HC.audioUnlock('recorder');
    }
    // NativeBridge path: stop native mic instead of getUserMedia tracks
    if (window.NativeBridge && window.NativeBridge.isAvailable) {
        window.NativeBridge.stopMicStream();
        if (window._nativeBridgeNode) { window._nativeBridgeNode.disconnect(); window._nativeBridgeNode = null; }
    } else {
        if (micStream) micStream.getTracks().forEach(t => t.stop());
    }
    if (audioContext) audioContext.close().catch(() => {});
    micBtn.classList.remove('recording');
    const fsMicOff = document.getElementById('fs-mic');
    if (fsMicOff) fsMicOff.classList.remove('recording');
    micIcon.textContent = '\ud83c\udfa4';
    isSpeaking = false;
    silenceStart = null;
    recorderDest = null;
    // Show pending count if any in-flight, else hide
    if (_sttInFlight.size > 0) {
        updatePendingDisplay();
    } else {
        hideVoiceStatus();
    }
    // Update level meters to 0 — but don't reset if playback strip is active
    if (!_playbackStripActive) {
        const strip = document.getElementById('mic-level-strip');
        if (strip) strip.className = 'mic-level-strip';
        const micFill = document.getElementById('mic-level-fill');
        if (micFill) micFill.style.width = '0%';
    }
}

// CPU health monitor — tracks monitorAudio frame timing
var _cpuMon = { frames: 0, totalMs: 0, maxMs: 0, slow: 0, lastReport: Date.now(), start: Date.now() };

function monitorAudio() {
    if (!isListening) return;
    // Resume AudioContext if browser suspended it (tab switch, background policy).
    // startMic() checks state at mic-open time, but suspension can happen after.
    // getByteFrequencyData returns all zeros on a suspended context — level=0,
    // isSpeaking never flips, recording silently stops working.
    // resume() is fire-and-forget — next rAF frame picks up live data once resumed.
    if (audioContext && audioContext.state === 'suspended') {
        audioContext.resume().catch(function(){});
    }
    var _frameT0 = performance.now();
    const dataArray = new Uint8Array(analyser.frequencyBinCount);
    analyser.getByteFrequencyData(dataArray);
    let sum = 0;
    for (let i = 0; i < dataArray.length; i++) sum += dataArray[i];
    const level = sum / dataArray.length;
    currentMicLevel = level;
    const now = Date.now();

    // Accumulate energy during speech for energy gate
    if (isSpeaking) {
        _speechEnergySum += level;
        _speechEnergySamples++;
    }

    // Update mic level strip (yield to playback strip when TTS is playing)
    // Log scale with floor at 2, ceiling at 200 (same as threshold slider)
    const pct = Math.min(100, Math.round(level / 2));
    if (!_playbackStripActive) {
        const micFill = document.getElementById('mic-level-fill');
        if (micFill) micFill.style.width = pct + '%';
    }

    const strip = document.getElementById('mic-level-strip');
    if (level > SPEECH_THRESHOLD) {
        if (strip && !_playbackStripActive) strip.className = 'mic-level-strip active speaking';
        if (!isSpeaking) {
            // Sustained energy gate: require GATE_DURATION ms above threshold
            if (!_gateStart) _gateStart = now;
            if ((now - _gateStart) < GATE_DURATION) {
                // Still in gate period — don't start recording yet.
                // DON'T return — must reach requestAnimationFrame at end.
            } else {
                // Gate passed — start recording
                _gateStart = null;
                isSpeaking = true;
                if (!_gpuStreamReady() && HC.gpuWarmRunpod) HC.gpuWarmRunpod();
                speechStart = now - GATE_DURATION; // backdate to include gate period
                _segmentStart = now;
                _resetSegmentationState();
                audioChunks = [];
                _speechEnergySum = 0;
                _speechEnergySamples = 0;
                var fp = document.getElementById('files-panel');
                var curFilePath = HC.getCurrentFilePath ? HC.getCurrentFilePath() : '';
                if (curFilePath && fp && fp.classList.contains('open')) {
                    _filesVisitedDuringRecording.push(curFilePath);
                }
                setVoiceStatus('recording', '\ud83c\udfa4 Recording...');
                (async () => {
                    await capturePreroll();
                    if (mediaRecorder && mediaRecorder.state !== 'recording') {
                        HC.audioLock('recorder');
                        mediaRecorder.start(1000);
                    }
                })();
            }
        }
        silenceStart = null;

        // ── Streaming segmentation: flush at 30s boundaries ──
        if (speechStart && !_segmentProcessing) {
            const segStart = _segmentStart || speechStart;
            const segElapsed = (now - segStart) / 1000;
            if (segElapsed >= SEGMENT_TARGET_SEC) {
                // If streaming to GPU, request partial transcription instead of browser-side segmentation
                if (_streamUploadId && _gpuStreamReady()) {
                    log('[STT] Segment at ' + segElapsed.toFixed(1) + 's — requesting partial transcription via stream');
                    _segmentStart = now;
                    try {
                        var preset = document.getElementById('stt-preset') ? document.getElementById('stt-preset').value : 'multilingual';
                        var diarizeOn = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
                        _gpuStreamSend({
                            type: 'stream_transcribe_partial',
                            stream_id: _streamUploadId,
                            preset: preset,
                            skip_align: !diarizeOn,
                            language: 'en',
                        });
                    } catch(e) { log('[STT] Partial request failed: ' + e.message); }
                } else {
                    // Fallback: old browser-side segmentation path
                    log('[STT] Segment flush at ' + segElapsed.toFixed(1) + 's — requestData (recorder keeps running)');
                    _segmentProcessing = true;
                    _segmentStart = now;
                    if (mediaRecorder && mediaRecorder.state === 'recording') {
                        mediaRecorder.requestData();
                        setTimeout(() => _flushAndSplitSegment(), 200);
                    } else {
                        _segmentProcessing = false;
                    }
                }
            }

            // Show duration + partial transcription for long recordings
            const totalElapsed = (now - speechStart) / 1000;
            if (totalElapsed > 3) {
                if (_partialResults.length > 0) {
                    const partialText = _partialResults.map(r => r.text).join(' ');
                    setVoiceStatus('recording', '\ud83c\udfa4 ' + Math.floor(totalElapsed) + 's — ' + partialText);
                } else {
                    setVoiceStatus('recording', '\ud83c\udfa4 ' + Math.floor(totalElapsed) + 's');
                }
            }
        }
    } else if (level <= SPEECH_THRESHOLD && !isSpeaking) {
        // Energy dropped before gate completed — reset gate
        _gateStart = null;
    } else if (level < SILENCE_THRESHOLD && isSpeaking) {
        if (strip && !_playbackStripActive) strip.className = 'mic-level-strip active silence';
        if (!silenceStart) {
            silenceStart = now;
        } else {
            const elapsed = now - silenceStart;
            // Show countdown after 1s grace
            if (elapsed > 1000) {
                const secsElapsed = (elapsed / 1000).toFixed(1);
                const secsTotal = (SILENCE_DURATION / 1000).toFixed(1);
                setVoiceStatus('', '\ud83d\udd07 Silence ' + secsElapsed + 's / ' + secsTotal + 's');
            }
            if (elapsed > SILENCE_DURATION) {
                // Save speech duration BEFORE nulling — mediaRecorder.stop() is async
                // and handleRecorderStop() needs speechStart to compute duration
                _lastSpeechDurSec = speechStart ? ((now - speechStart - SILENCE_DURATION) / 1000).toFixed(1) : '?';
                if (mediaRecorder && mediaRecorder.state === 'recording') {
                    mediaRecorder.stop();
                    HC.audioUnlock('recorder');
                }
                isSpeaking = false;
                silenceStart = null;
                speechStart = null;
                if (strip && !_playbackStripActive) strip.className = 'mic-level-strip active';
            }
        }
    }
    // CPU health tracking
    var _frameDur = performance.now() - _frameT0;
    _cpuMon.frames++;
    _cpuMon.totalMs += _frameDur;
    if (_frameDur > _cpuMon.maxMs) _cpuMon.maxMs = _frameDur;
    if (_frameDur > 50) _cpuMon.slow++;
    var _sinceReport = Date.now() - _cpuMon.lastReport;
    if (_sinceReport > 30000) {
        var _elapsed = _sinceReport / 1000;
        var _avg = _cpuMon.frames > 0 ? (_cpuMon.totalMs / _cpuMon.frames).toFixed(1) : '0';
        var _fps = (_cpuMon.frames / _elapsed).toFixed(0);
        var _totalMin = ((Date.now() - _cpuMon.start) / 60000).toFixed(1);
        log('[CPU] ' + _totalMin + 'min: ' + _fps + 'fps avg=' + _avg + 'ms max=' + _cpuMon.maxMs.toFixed(0) + 'ms slow=' + _cpuMon.slow);
        _cpuMon.frames = 0; _cpuMon.totalMs = 0; _cpuMon.maxMs = 0; _cpuMon.slow = 0;
        _cpuMon.lastReport = Date.now();
    }
    requestAnimationFrame(monitorAudio);
}

async function capturePreroll() {
    if (!prerollNode) { prerollBlob = null; return; }
    return new Promise((resolve) => {
        prerollNode.port.onmessage = (event) => {
            if (event.data.buffer) {
                const samples = new Float32Array(event.data.buffer);
                if (samples.length === 0) { prerollBlob = null; resolve(); return; }
                prerollBlob = new Blob([encodeWAV(samples, audioContext.sampleRate)], { type: 'audio/wav' });
                resolve();
            }
        };
        prerollNode.port.postMessage({ getBuffer: true });
    });
}

function encodeWAV(samples, sampleRate) {
    const buf = new ArrayBuffer(44 + samples.length * 2);
    const v = new DataView(buf);
    const w = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
    w(0, 'RIFF'); v.setUint32(4, 36 + samples.length * 2, true);
    w(8, 'WAVE'); w(12, 'fmt ');
    v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
    v.setUint32(24, sampleRate, true); v.setUint32(28, sampleRate * 2, true);
    v.setUint16(32, 2, true); v.setUint16(34, 16, true);
    w(36, 'data'); v.setUint32(40, samples.length * 2, true);
    let off = 44;
    for (let i = 0; i < samples.length; i++) {
        const s = Math.max(-1, Math.min(1, samples[i]));
        v.setInt16(off, s < 0 ? s * 0x8000 : s * 0x7FFF, true);
        off += 2;
    }
    return buf;
}

function handleRecorderStop() {
    // Send stream finalize after a brief delay to let the last chunk's
    // FileReader complete (ondataavailable fires the final partial chunk,
    // but _streamAudioChunk uses async FileReader)
    var _finalizeStreamId = _streamUploadId;
    var _finalizeChunkCount = _streamChunkIndex;
    _streamUploadId = null;
    _streamChunkIndex = 0;
    // NOTE: _streamTransport stays locked until GPU acknowledges all chunks.
    // Retransmit requests need the same transport. Cleared in result/error callback.

    // If stream was queued for resubmit (RTC died mid-stream), attach finalize info.
    // The dc.onopen handler will send finalize after resubmitting all chunks.
    if (HC._pendingStreamResubmit && HC._pendingStreamResubmit.streamId === _finalizeStreamId) {
        var preset = document.getElementById('stt-preset') ? document.getElementById('stt-preset').value : 'multilingual';
        var diarizeOn = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
        HC._pendingStreamResubmit.finalized = true;
        HC._pendingStreamResubmit.chunkCount = _finalizeChunkCount;
        HC._pendingStreamResubmit.finalizeData = {
            type: 'stream_finalize',
            stream_id: _finalizeStreamId,
            expected_chunks: _finalizeChunkCount,
            preset: preset,
            skip_align: !diarizeOn,
            language: 'en',
        };
        log('[STT-Stream] Finalize queued for resubmit: ' + _finalizeStreamId + ' (' + _finalizeChunkCount + ' chunks)');
        return;  // Don't try to send finalize now — wait for reconnect + resubmit
    }

    if (_finalizeStreamId) {
        // Register result handler and start elapsed timer BEFORE deciding whether to send
        // now (channel active) or queue for retry (channel inactive). Both paths need
        // HC._gpuPending registered so the GPU's result can be routed.
        var _cleanupStreamId = _finalizeStreamId;
        var _finalizeT0 = Date.now();
        var _probeState = 'waiting'; // waiting → probing → alive/dead
        var _probeInterval = 10; // first probe at 10s
        var _finalizeStreamIdForTimer = _finalizeStreamId;
        var _finalizeHandler = {
            onResult: function(output) {
                clearInterval(_finalizeTimer);
                if (_finalizeHandler._expireTimer) clearTimeout(_finalizeHandler._expireTimer);
                // Clear resubmit flag — result arrived, stream is done.
                // Prevents "Reconnecting..." banner from persisting into future streams.
                if (HC._streamNeedsResubmit === _cleanupStreamId) {
                    HC._streamNeedsResubmit = null;
                }
                /* transport managed by channel */ // release transport lock
                // Capture stream fixture before chunks are freed — chunks deleted next line
                var _streamText = (output.text || '').trim();
                var _streamWxText = (output.whisperx_text || '').trim();
                var _streamEmpty = !_streamText && !_streamWxText;
                if (_streamEmpty || _shouldCaptureFixture()) {
                    var _fixChunks = {};
                    var _fixSaved = _streamSentChunks[_cleanupStreamId];
                    if (_fixSaved) { Object.keys(_fixSaved).forEach(function(k) { _fixChunks[k] = _fixSaved[k]; }); }
                    var _fixChunkTimes = {};
                    var _fixTimes = _streamChunkTimes[_cleanupStreamId];
                    if (_fixTimes) { Object.keys(_fixTimes).forEach(function(k) { _fixChunkTimes[k] = _fixTimes[k]; }); }
                    var _fixPreroll = _sttCapturePreroll;
                    _sttCapturePreroll = null;
                    var _fixPreset = (document.getElementById('stt-preset') ? document.getElementById('stt-preset').value : 'multilingual') || 'multilingual';
                    var _fixDiarize = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
                    _saveFixtureToIdb({
                        id: 'str-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 6),
                        version: 1, type: 'stream', captured_at: Date.now(),
                        trigger: _streamEmpty ? 'empty_result' : 'all',
                        audio: { mime_type: mediaRecorder ? mediaRecorder.mimeType : 'audio/webm',
                                 chunks: _fixChunks, chunk_times: _fixChunkTimes,
                                 preroll_b64: _fixPreroll || null, preroll_mime: _fixPreroll ? 'audio/wav' : null },
                        request: { stream_id: _cleanupStreamId, preset: _fixPreset,
                                   skip_align: !_fixDiarize, language: 'en',
                                   agent_id: (HC.config && HC.config.agentId) || '' },
                        result: { text: _streamText, whisperx_text: _streamWxText, empty: _streamEmpty },
                        meta: (function() {
                            var ckeys = Object.keys(_fixChunks).sort(function(a,b){return parseInt(a)-parseInt(b);});
                            var span = ckeys.length > 1
                                ? (_fixChunkTimes[ckeys[ckeys.length-1]] || 0) - (_fixChunkTimes[ckeys[0]] || 0)
                                : null;
                            return { gpu_url: HC.GPU_URL || '',
                                     chunks_count: ckeys.length, chunk_time_span_ms: span };
                        })(),
                    });
                }
                delete _streamSentChunks[_cleanupStreamId]; delete _streamChunkTimes[_cleanupStreamId]; delete _streamStartTimes[_cleanupStreamId]; // free retransmit buffer
                setVoiceStatus('sending', '\ud83c\udfa4 Processing transcription...');
                var text = _streamText;
                var wxText = _streamWxText;
                if (text || wxText) {
                    _partialResults.push({
                        text: text,
                        wxText: wxText,
                        segments: output.segments || [],
                        wordSegments: output.word_segments || [],
                        mediumElapsed: output.medium_elapsed || 0,
                        wxElapsed: output.whisperx_elapsed || 0,
                        embSim: output.embedding_similarity,
                        energy: typeof avgEnergy === 'number' ? avgEnergy : undefined,
                    });
                }
                // Update cumulative offset from speech duration
                if (_lastSpeechDurSec) _cumulativeOffset = parseFloat(_lastSpeechDurSec) || 0;
                // Combine all partials and inject
                var combined = _partialResults.map(function(r) { return r.text; }).join(' ').trim();
                var combinedWx = _partialResults.map(function(r) { return r.wxText || r.text; }).join(' ').trim();
                if (combined) {
                    log('[STT-Stream] Final injection: ' + combined.substring(0, 50) + '...');
                    _injectFinalTranscription(null);
                } else {
                    log('[STT-Stream] Final: empty result');
                    _resetSegmentationState();
                }
            },
            onError: function(err) {
                log('[STT-Stream] Finalize error: ' + err);
                // Tenacious: GPU connection lost → resubmit, don't give up
                if (err && err.indexOf('connection lost') >= 0 && _streamSentChunks[_cleanupStreamId]) {
                    log('[STT-Stream] GPU connection lost — triggering resubmit');
                    HC._streamNeedsResubmit = _cleanupStreamId;
                    HC._streamResubmitChunkCount = _finalizeChunkCount;
                    setVoiceStatus('sending', '\u26a0\ufe0f Reconnecting...');
                    if (typeof _gpuRtcCleanup === 'function') _gpuRtcCleanup();
                    /* reconnection handled by gpuChannel */
                    return; // Don't clear timer — keep the master timeout running
                }
                clearInterval(_finalizeTimer);
                if (_finalizeHandler._expireTimer) clearTimeout(_finalizeHandler._expireTimer);
                /* transport managed by channel */ // release transport lock
                delete _streamSentChunks[_cleanupStreamId]; delete _streamChunkTimes[_cleanupStreamId]; delete _streamStartTimes[_cleanupStreamId]; _sttCapturePreroll = null; // free retransmit buffer + fixture state
                if (_partialResults.length > 0) _injectFinalTranscription(null);
                else _resetSegmentationState();
            },
            onDone: function() {
                clearInterval(_finalizeTimer);
                if (_finalizeHandler._expireTimer) clearTimeout(_finalizeHandler._expireTimer);
            },
            onProgress: function() {},
        };
        // Finalize jobs bypass HC.gpuJob() and have no expiry timer by default.
        // Add a 90s safety-net timer to prevent permanent leaks.
        _finalizeHandler._expireTimer = setTimeout(function() {
            if (HC._gpuPending[_cleanupStreamId]) {
                log('[STT-Stream] Finalize job expired (90s) — GPU never sent done/result: ' + _cleanupStreamId);
                delete HC._gpuPending[_cleanupStreamId];
            }
            if (HC._gpuPending[_cleanupStreamId + '-finalize']) {
                delete HC._gpuPending[_cleanupStreamId + '-finalize'];
            }
        }, 90000);
        // Register under BOTH stream_id (GPU WS) and stream_id-finalize (HTTP path)
        HC._gpuPending[_finalizeStreamId] = HC._gpuPending[_finalizeStreamId + '-finalize'] = _finalizeHandler;

        // Elapsed timer + liveness probe + 3-minute master timeout.
        // Started here so both the immediate and deferred paths show UI feedback.
        // The ONLY exit from this wait is: GPU result, GPU error, or 3-min timeout.
        var _finalizeTimer = setInterval(function() {
            var sec = Math.round((Date.now() - _finalizeT0) / 1000);
            if (HC._streamNeedsResubmit) {
                setVoiceStatus('sending', '\u26a0\ufe0f Reconnecting... ' + sec + 's');
            } else {
                setVoiceStatus('sending', '\ud83c\udfa4 Transcribing... ' + sec + 's');
            }

            // Liveness probe: every _probeInterval seconds, send echo to test connection.
            // If echo returns → connection alive, GPU processing.
            // If echo doesn't return → connection dead, force reconnect + resubmit.
            if (sec >= _probeInterval && _probeState === 'waiting' && !HC._streamNeedsResubmit) {
                var echoId = 'echo-' + Date.now();
                log('[STT-Stream] Liveness probe at ' + sec + 's (' + echoId + ')');
                HC._gpuPending[echoId] = {
                    onResult: function() {
                        log('[STT-Stream] Probe returned — connection alive');
                        _probeState = 'waiting';
                        _probeInterval = sec + 15; // next probe in 15s
                    },
                    onError: function() { _probeState = 'waiting'; _probeInterval = sec + 15; },
                    onDone: function() {}, onProgress: function() {},
                };
                // Check return value BEFORE entering probing state.
                // _gpuStreamSend returns false silently when the channel is in the
                // _handles registration gap (transport exists but readyState not 'open'
                // yet). If we enter 'probing' without sending, the 3s dead-check fires
                // and falsely flags the connection dead → unnecessary resubmit + 30s delay.
                var probeSent;
                try { probeSent = _gpuStreamSend({ type: 'echo', job_id: echoId }); } catch(e) { probeSent = false; }
                if (probeSent) {
                    _probeState = 'probing';
                } else {
                    // Channel not active — skip this probe, clean up pending entry, retry soon
                    delete HC._gpuPending[echoId];
                    _probeInterval = sec + 5;
                    log('[STT-Stream] Liveness probe skipped — channel not active, retry in 5s');
                }
            }
            // 3s after probe: if no echo, connection may be dead → flag resubmit
            // Do NOT close transports — the reliability layer manages reconnection.
            if (_probeState === 'probing' && sec >= _probeInterval + 3) {
                _probeState = 'dead';
                log('[STT-Stream] Probe FAILED — flagging for resubmit');
                HC._streamNeedsResubmit = _finalizeStreamIdForTimer;
                HC._streamResubmitChunkCount = _finalizeChunkCount;
                setVoiceStatus('sending', '\u26a0\ufe0f Waiting for GPU... ' + sec + 's');
                // If RTC is active but not delivering, demote to WS
                if (HC.gpuChannel) {
                    var st = HC.gpuChannel.status();
                    if (st.active === 'rtc') {
                        log('[STT-Stream] Demoting dead RTC — switching to WS');
                        HC.gpuChannel._disconnectTransport('rtc');
                    }
                }
                _probeState = 'waiting';
                _probeInterval = sec + 15;
            }

            // 3-minute master timeout — the only non-GPU exit
            if (sec >= 180) {
                log('[STT-Stream] Master timeout (3 min) — giving up');
                clearInterval(_finalizeTimer);
                /* transport managed by channel */
                HC._streamNeedsResubmit = null;
                delete _streamSentChunks[_finalizeStreamIdForTimer]; delete _streamChunkTimes[_finalizeStreamIdForTimer]; delete _streamStartTimes[_finalizeStreamIdForTimer]; _sttCapturePreroll = null;
                if (_partialResults.length > 0) _injectFinalTranscription(null);
                else {
                    setVoiceStatus('recording', '\u274c Transcription timed out');
                    revertVoiceStatusAfter(4000, '', '');
                    _resetSegmentationState();
                }
            }
        }, 1000);
    }

    if (_finalizeStreamId && _gpuStreamReady()) {
        try {
            var preset = document.getElementById('stt-preset') ? document.getElementById('stt-preset').value : 'multilingual';
            var diarizeOn = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
            var finalizeMsg = {
                type: 'stream_finalize',
                stream_id: _finalizeStreamId,
                expected_chunks: _finalizeChunkCount,
                preset: preset,
                skip_align: !diarizeOn,
                language: 'en',
            };
            // If all chunks already sent (FileReader completed), send finalize immediately.
            // Otherwise defer — the chunk sender will send finalize after the last chunk.
            if (_streamChunksSent >= _finalizeChunkCount) {
                _gpuStreamSend(finalizeMsg);
                log('[STT-Stream] Finalized stream ' + _finalizeStreamId + ' (' + _finalizeChunkCount + ' chunks, all sent)');
                _streamChunksSent = 0; // reset for next stream
            } else {
                _streamPendingFinalize = {
                    streamId: _finalizeStreamId,
                    expected: _finalizeChunkCount,
                    data: finalizeMsg,
                };
                log('[STT-Stream] Deferring finalize — ' + _streamChunksSent + '/' + _finalizeChunkCount + ' chunks sent');
                // DON'T reset _streamChunksSent — the chunk sender needs it to reach expected
            }
        } catch(e) { log('[STT-Stream] Finalize failed: ' + e.message); }
    } else if (_finalizeStreamId) {
        // GPU channel inactive at finalize time — queue and retry when channel becomes active.
        // This covers the window between channel reset and first heartbeat received (_lastRecv).
        // Handler and timer are already set up above; ontransportchange in gpu.js flushes this.
        var preset = document.getElementById('stt-preset') ? document.getElementById('stt-preset').value : 'multilingual';
        var diarizeOn = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
        HC._pendingChannelFinalize = {
            type: 'stream_finalize',
            stream_id: _finalizeStreamId,
            expected_chunks: _finalizeChunkCount,
            preset: preset,
            skip_align: !diarizeOn,
            language: 'en',
        };
        log('[STT-Stream] Channel inactive at finalize — queued for retry: ' + _finalizeStreamId);
    }

    // Grab chunks + preroll, clear immediately
    const chunks = audioChunks.slice();
    const preroll = prerollBlob;
    audioChunks = [];
    prerollBlob = null;

    // Add final chunks to cumulative webm array
    _allWebmChunks.push(...chunks);
    if (_allWebmChunks.length === 0 && _partialResults.length === 0) return;
    const mimeType = mediaRecorder ? mediaRecorder.mimeType : 'audio/webm';

    // Credit gate — skip GPU work, inject notice instead
    if (HC._userState.credits === 0) {
        log('[STT] Credits depleted — skipping GPU, injecting notice');
        setVoiceStatus('error', 'Credits depleted');
        revertVoiceStatusAfter(3000, '', isListening ? LISTENING_HTML : '');
        const injection = '[credits depleted] The user attempted to use speech-to-text but has no credits remaining. They will need to subscribe or buy a credit pack at {{DOMAIN}}/billing.';
        HC.daemonFetch('/api/transcription', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ text: injection }),
        }).catch(() => {});
        _resetSegmentationState();
        return;
    }

    if (_finalizeStreamId) {
        // ── Streaming flow: GPU already has all chunks + partials, finalize sent ──
        // The finalize result handler will combine partials + final segment and inject.
        // Do NOT reset segmentation state here — _partialResults contains the partials
        // that need to be combined with the finalize result.
        log('[STT] Streaming path active — skipping browser-side segmentation (' + _partialResults.length + ' partials waiting)');
        setVoiceStatus('sending', '\ud83d\udce4 Finalizing recording...');
        return;
    }

    if (_partialResults.length > 0 || _processedSamples > 0) {
        // ── Segmented flow: decode full webm, slice unprocessed remainder ──
        const finalDur = _lastSpeechDurSec;
        log('[STT] Final segment: ' + finalDur + 's (' + _partialResults.length + ' prior chunks)');
        setVoiceStatus('sending', '\ud83d\udce4 Transcribing final segment...');

        (async () => {
            try {
                // Decode FULL webm to get all PCM, then slice from _processedSamples
                let finalSamples = new Float32Array(0);
                let sr = 48000;
                if (_allWebmChunks.length > 0) {
                    const fullBlob = new Blob(_allWebmChunks, { type: mimeType });
                    const decodeCtx = new (window.AudioContext || window.webkitAudioContext)();
                    const decoded = await decodeCtx.decodeAudioData(await fullBlob.arrayBuffer());
                    const allSamples = decoded.getChannelData(0);
                    sr = decoded.sampleRate;
                    finalSamples = allSamples.slice(_processedSamples);
                    decodeCtx.close();
                }
                // Encode remaining PCM as WAV
                const audioBlob = _float32ToWavBlob(finalSamples, sr);
                const actualDur = (finalSamples.length / sr).toFixed(1);
                log('[STT] Final segment WAV: ' + actualDur + 's (' + finalSamples.length + ' samples)');

                const idbSeq = ++_sttSeq;
                _saveAudioBlob(idbSeq, audioBlob, actualDur);
                _segmentBlobs.push(audioBlob);

                await _doSegmentTranscribe(idbSeq, audioBlob, actualDur);
                _deleteAudioBlob(idbSeq).catch(() => {});
                // Wait for all intermediate segments to finish before injecting
                if (_segmentPromises.length > 0) {
                    log('[STT] Waiting for ' + _segmentPromises.length + ' in-flight segments...');
                    await Promise.allSettled(_segmentPromises);
                    _segmentPromises = [];
                }
                // Diarize if enabled, then inject
                const diarizeToggle = document.getElementById('stt-diarize');
                const diarizationEnabled = diarizeToggle ? diarizeToggle.checked : true;
                const diarizeResult = diarizationEnabled ? await _diarizeFullAudio() : null;
                await _injectFinalTranscription(diarizeResult);
            } catch (e) {
                log('[STT] Final segment/inject error: ' + e.message);
                // Fallback: inject whatever we have.
                // Guard on !_injectionInProgress: prevents double injection if
                // _injectFinalTranscription threw after injecting (tunnel path).
                // Guard on daemon availability: mirrors daemonFetch's own readiness
                // check so the fallback works on EC2 path (_daemonBase=null, WS path).
                if (!_injectionInProgress && _partialResults.length > 0) {
                    const plainText = _partialResults.map(r => r.text).join(' ');
                    if (plainText.trim()) {
                        lastTranscriptionTime = Date.now();
                        const fallbackFilesTag = _filesVisitedDuringRecording.length > 0
                            ? ' [files viewed: ' + _filesVisitedDuringRecording.join(', ') + ']'
                            : '';
                        const injection = plainText + ' [' + _cumulativeOffset.toFixed(1) + 's audio]' + fallbackFilesTag;
                        if (HC._daemonBase || (HC.ws && typeof HC.ws.sendHttpRequest === 'function' && HC.ws.readyState === 1)) {
                            await HC.daemonFetch('/api/transcription', {
                                method: 'POST',
                                headers: { 'Content-Type': 'application/json' },
                                body: JSON.stringify({ text: injection }),
                            });
                            HC.spendCredit(); // instant local decrement + background server sync
                        }
                    }
                    _resetSegmentationState();
                }
            }
            if (_sttInFlight.size > 0) updatePendingDisplay();
            else if (!isListening) setTimeout(hideVoiceStatus, 2000);
        })();
    } else {
        // ── Short utterance (< 30s): existing flow, unchanged ──
        const audioBlob = new Blob(chunks, { type: mimeType });
        const idbSeq = ++_sttSeq;
        _saveAudioBlob(idbSeq, audioBlob, _lastSpeechDurSec);

        // Discard if too short
        if (parseFloat(_lastSpeechDurSec) < MIN_SPEECH_DURATION / 1000) {
            log('[STT] Discarded: too short (' + _lastSpeechDurSec + 's)');
            _deleteAudioBlob(idbSeq).catch(() => {});
            return;
        }

        // Energy gate
        const avgEnergy = _speechEnergySamples > 0 ? _speechEnergySum / _speechEnergySamples : 0;
        if (avgEnergy < ENERGY_GATE_THRESHOLD) {
            log('[STT] Discarded: energy gate avgLevel=' + avgEnergy.toFixed(1) + ' < ' + ENERGY_GATE_THRESHOLD);
            setVoiceStatus('recording', '\ud83d\udd07 Too quiet (energy ' + avgEnergy.toFixed(0) + ')');
            revertVoiceStatusAfter(2000, '', isListening ? LISTENING_HTML : '');
            _deleteAudioBlob(idbSeq).catch(() => {});
            return;
        }

        const durSec = _lastSpeechDurSec;
        log('[STT] Sending ' + durSec + 's, avgEnergy=' + avgEnergy.toFixed(1));
        setVoiceStatus('sending', '\ud83d\udce4 Sent ' + durSec + 's to transcription');
        revertVoiceStatusAfter(2000, '', LISTENING_HTML);

        // Fire-and-forget transcription (full pipeline with diarization)
        submitTranscription(audioBlob, durSec, avgEnergy);
        // Delete IDB blob — it's in memory for retries, IDB is only crash recovery
        _deleteAudioBlob(idbSeq).catch(() => {});
    }

    // Zero-gap: re-create MediaRecorder on same recorderDest immediately
    if (isListening && recorderDest) {
        try {
            mediaRecorder = new MediaRecorder(recorderDest.stream, {
                mimeType: MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
                    ? 'audio/webm;codecs=opus' : 'audio/webm'
            });
            mediaRecorder.ondataavailable = (e) => { if (e.data.size > 0) audioChunks.push(e.data); };
            mediaRecorder.onstop = handleRecorderStop;
        } catch(e) { log('[Mic] Recorder re-create failed: ' + e.message); }
    }
}

// Handle lost transcription: inject notification + upload audio to daemon for recovery
function _handleSttLost(seq, audioBlob, durSec, avgEnergy) {
    durSec = parseFloat(durSec) || 0;
    avgEnergy = parseFloat(avgEnergy) || 0;
    log('[STT] _handleSttLost called: seq=' + seq + ' blob=' + (audioBlob ? audioBlob.size + 'B' : 'NULL') + ' dur=' + durSec + ' ws=' + (HC.ws ? HC.ws.readyState : 'null'));
    try {
        setVoiceStatus('recording', '\u274c Transcription lost');
        revertVoiceStatusAfter(3000, '', isListening ? LISTENING_HTML : '');
        if (_sttInFlight.size === 0 && !isListening) hideVoiceStatus();

        // Inject a notification into the PTY so the agent knows
        if (HC.ws && HC.ws.readyState === 1) {
            HC.wsSend(JSON.stringify({
                type: 'stt_lost',
                seq: seq,
                duration: durSec,
                energy: avgEnergy,
            }));
            log('[STT] Notified daemon of lost seq=' + seq + ' (' + durSec.toFixed(1) + 's)');
        } else {
            log('[STT] Cannot notify daemon: ws=' + (HC.ws ? 'state=' + HC.ws.readyState : 'null'));
        }

        // Upload the audio blob to daemon for recovery with retry
        // If blob is null (GC'd), try to recover from IndexedDB
        if (!audioBlob) {
            log('[STT] Blob is null for seq=' + seq + ' — checking IndexedDB');
            (async () => {
                try {
                    const db = await _openAudioDb();
                    const tx = db.transaction(IDB_STORE, 'readonly');
                    const req = tx.objectStore(IDB_STORE).get(seq);
                    req.onsuccess = () => {
                        db.close();
                        if (req.result && req.result.blob) {
                            log('[STT] Recovered blob from IDB for seq=' + seq + ' (' + req.result.blob.size + 'B)');
                            _handleSttLost(seq, req.result.blob, durSec, avgEnergy);
                        } else {
                            log('[STT] No IDB entry for seq=' + seq);
                        }
                    };
                    req.onerror = () => { db.close(); log('[STT] IDB get error for seq=' + seq); };
                } catch (e) { log('[STT] IDB recovery error: ' + e.message); }
            })();
            return;
        }
        if (audioBlob) {
            log('[STT] Uploading lost audio seq=' + seq + ' (' + audioBlob.size + ' bytes)');
            const maxRetries = 3;
            const doUpload = async (attempt) => {
                try {
                    // Send JSON+base64 instead of FormData — FormData does not serialize
                    // correctly through the EC2 proxy sendHttpRequest path.
                    const buf = await audioBlob.arrayBuffer();
                    const bytes = new Uint8Array(buf);
                    let binary = '';
                    for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
                    const audio_b64 = btoa(binary);
                    HC.daemonFetch('/api/lost-audio', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ audio_b64, seq, duration: durSec, energy: avgEnergy }),
                    }).then(r => {
                        if (r.ok) log('[STT] Uploaded lost audio seq=' + seq + ' to daemon');
                        else {
                            log('[STT] Lost audio upload HTTP ' + r.status + ' (attempt ' + attempt + '/' + maxRetries + ')');
                            if (attempt < maxRetries) setTimeout(() => doUpload(attempt + 1), attempt * 2000);
                        }
                    }).catch(e => {
                        log('[STT] Lost audio upload error: ' + e.message + ' (attempt ' + attempt + '/' + maxRetries + ')');
                        if (attempt < maxRetries) setTimeout(() => doUpload(attempt + 1), attempt * 2000);
                        else log('[STT] Lost audio upload FAILED after ' + maxRetries + ' attempts');
                    });
                } catch (e) {
                    log('[STT] Lost audio encode error: ' + e.message + ' (attempt ' + attempt + '/' + maxRetries + ')');
                    if (attempt < maxRetries) setTimeout(() => doUpload(attempt + 1), attempt * 2000);
                }
            };
            doUpload(1);
        } else {
            log('[STT] No audio blob to upload for seq=' + seq);
        }
    } catch(e) {
        log('[STT] _handleSttLost EXCEPTION: ' + e.message);
    }
}

// Handle lost TTS: notify daemon so agent can retry
// Track msg_ids that have been successfully played — suppress false loss reports
if (!HC._ttsPlayedMsgIds) HC._ttsPlayedMsgIds = {};

function _handleTtsLost(msgId, text, error, stage) {
    // If this TTS already played successfully (possibly on another browser),
    // don't report it as lost.
    if (HC._ttsPlayedMsgIds[msgId]) {
        log('[TTS] [msg=' + msgId + '] Suppressed false loss report — already played');
        return;
    }

    setVoiceStatus('error', '\u274c TTS failed');
    revertVoiceStatusAfter(4000, '', '');

    // Notify daemon via WebSocket
    if (HC.ws && HC.ws.readyState === 1) {
        HC.wsSend(JSON.stringify({
            type: 'tts_lost',
            msg_id: msgId,
            text: text,
            error: error,
            stage: stage,
        }));
        log('[TTS] [msg=' + msgId + '] Notified daemon of lost TTS stage=' + stage + ' error=' + error);
    }
    // Report error to daemon (always POST, injection gated daemon-side)
    HC.daemonFetch('/api/voice-report', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            msg_id: msgId,
            status: 'error',
            error: error,
            stage: stage,
            session_id: clientId,
            volume: HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100,
            inject: !!HC._voiceReportEnabled,
        }),
    }).catch(() => {});
}

function submitTranscription(audioBlob, durSec, avgEnergy) {
    const seq = ++_sttSeq;
    const entry = { retries: 0, timer: null };
    _sttInFlight.set(seq, entry);

    if (_sttInFlight.size > 1) updatePendingDisplay();

    // Timeout → retry once → give up
    entry.timer = setTimeout(() => {
        if (!_sttInFlight.has(seq)) return;
        if (entry.retries < STT_MAX_RETRIES) {
            entry.retries++;
            log('[STT] Timeout seq=' + seq + ' — retry ' + entry.retries);
            _doGpuTranscribe(seq, audioBlob, durSec, avgEnergy);
            entry.timer = setTimeout(() => {
                if (_sttInFlight.has(seq)) {
                    _sttInFlight.delete(seq);
                    log('[STT] Lost seq=' + seq);
                    _handleSttLost(seq, audioBlob, durSec, avgEnergy);
                }
            }, STT_TIMEOUT_MS);
        } else {
            _sttInFlight.delete(seq);
            log('[STT] Lost seq=' + seq);
            _handleSttLost(seq, audioBlob, durSec, avgEnergy);
        }
    }, STT_TIMEOUT_MS);

    _doGpuTranscribe(seq, audioBlob, durSec, avgEnergy);
}

async function _doGpuTranscribe(seq, audioBlob, durSec, avgEnergy) {
    if (!HC.GPU_URL && !LOCAL_HC.GPU_URL) {
        log('[GPU] Not configured — cannot transcribe');
        _sttInFlight.delete(seq);
        setVoiceStatus('recording', '\u26a0\ufe0f Voice server unavailable');
        revertVoiceStatusAfter(3000, '', isListening ? 'Listening...' : '');
        return;
    }
    const hasDaemon = HC._daemonBase ||
        (HC.ws && typeof HC.ws.sendHttpRequest === 'function' && HC.ws.readyState === 1);
    if (!hasDaemon) {
        log('[STT] No daemon path available — skipping transcription (seq=' + seq + ')');
        _sttInFlight.delete(seq);
        return;
    }

    // RunPod cold-start: if in runpod-only mode and no EC2 worker is connected yet,
    // queue the request. gpu.js will call HC._drainTranscribeQueue() when a worker
    // first appears (_refreshGpuConfig wasNull path). The existing STT_TIMEOUT_MS timer
    // stays alive — if the worker never connects, the timeout fires and calls _handleSttLost.
    if (HC._gpuMode === 'runpod-only' && !HC._localWorkerId) {
        log('[STT] runpod-only + no worker yet — queuing transcription (seq=' + seq + ')');
        _pendingTranscribeQueue.push({ seq, audioBlob, durSec, avgEnergy });
        return;
    }

    const preset = document.getElementById('stt-preset').value || 'multilingual';

    try {
        const buf = await audioBlob.arrayBuffer();
        const bytes = new Uint8Array(buf);
        let binary = '';
        for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        const b64 = btoa(binary);

        // Register voice job for reliability tracking
        const _vjId = crypto.randomUUID ? crypto.randomUUID() : (Date.now().toString(36) + Math.random().toString(36).slice(2));
        // Awaited so the row exists before the first jobCheckpoint PATCH fires.
        // gpuRun fires on the next line — audio path is not blocked.
        await HC.registerJob({ id: _vjId, agent_id: HC.config.agentId || '', job_type: 'stt', duration_sec: parseFloat(durSec) || 0 });

        // Submit async job via gpuRun (local-first + RunPod fallback)
        const sttT0 = performance.now();
        const _diarizeOn = (function() { var cb = document.getElementById('stt-diarize'); return cb ? cb.checked : true; })();
        const gpuResult = await HC.gpuRun(
            { action: 'transcribe', audio_b64: b64, language: 'en', preset: preset, skip_align: !_diarizeOn, voice_job_id: _vjId }
        );
        const runData = gpuResult.data, backend = gpuResult.backend, baseUrl = gpuResult.baseUrl, token = gpuResult.token;
        const wsCtrl = gpuResult.wsController || null;
        const jobId = runData.id;
        log('[STT] Job submitted: ' + jobId + ' (' + backend + ')');
        setVoiceStatus('sending', 'Transcribing...');
        HC.gpuJobStart(jobId, 'transcribe');
        HC.jobCheckpoint(_vjId, { status: 'gpu_submitted', gpu_job_id: jobId, gpu_backend: backend });

        let output = {};
        let sttDone = false;

        if (wsCtrl) {
            // ── WebSocket path: push-based ──
            await new Promise(function(resolve) {
                wsCtrl.onAccepted = function(msg) { HC.gpuJobAccepted(jobId, msg.state, msg.gpu_worker_id); };
                wsCtrl.onProgress = function(p) {
                    var stage = p.stage;
                    if (stage === 'started') setVoiceStatus('sending', 'Audio received...');
                    else if (stage === 'whisper_transcribing') setVoiceStatus('sending', 'Transcribing...');
                    else if (stage === 'transcribing') setVoiceStatus('sending', 'Transcribing... ' + (p.segments||0) + ' segments');
                    else if (stage === 'computing_similarity') setVoiceStatus('sending', 'Computing self-similarity...');
                    else setVoiceStatus('sending', stage);
                    log('[STT] Progress: ' + stage);
                };
                wsCtrl.onResult = function(result) { output = result; sttDone = true; resolve(); };
                wsCtrl.onError = function(err) {
                    log('[STT] WS error: ' + err + ' — marking WS unreliable for retry');
                    // Channel will handle reconnection
                    sttDone = true;
                    resolve();
                };
                wsCtrl.onDone = function() { if (!sttDone) { sttDone = true; resolve(); } };
            });
            HC.gpuJobEnd(jobId, 'success');
        } else {
        // ── HTTP polling path ──
        const streamUrl = HC.gpuStreamUrl(baseUrl, jobId);
        const pollHeaders = HC.gpuPollHeaders(token);

        while (!sttDone) {
            await new Promise(r => setTimeout(r, 300));
            try {
                const sResp = await fetch(streamUrl, { headers: pollHeaders });
                if (!sResp.ok) {
                    if (sResp.status === 404) {
                        const elapsed = Math.round((performance.now() - sttT0) / 1000);
                        if (elapsed >= 3) {
                            setVoiceStatus('sending', '\u2615 GPU warming up... ' + elapsed + 's');
                            // Extend the outer timeout on first cold-start detection
                            const entry = _sttInFlight.get(seq);
                            if (entry && !entry._coldStartExtended) {
                                entry._coldStartExtended = true;
                                if (entry.timer) clearTimeout(entry.timer);
                                entry.timer = setTimeout(() => {
                                    if (_sttInFlight.has(seq)) {
                                        _sttInFlight.delete(seq);
                                        log('[STT] Lost seq=' + seq + ' (cold start timeout)');
                                        _handleSttLost(seq, audioBlob, durSec, avgEnergy);
                                    }
                                }, STT_COLDSTART_TIMEOUT_MS);
                                log('[STT] Cold start detected — extended timeout to ' + STT_COLDSTART_TIMEOUT_MS + 'ms');
                            }
                        }
                        continue;
                    }
                    continue;
                }
                const sData = await sResp.json();

                const items = sData.stream || (Array.isArray(sData) ? sData : []);
                if (items.length > 0) log('[STT] Poll: status=' + sData.status + ' items=' + items.length);
                for (const item of items) {
                    const ev = item.output || item;
                    // GPU state report (first event from worker)
                    if (ev.status === 'accepted' && ev.state) {
                        HC.gpuJobAccepted(jobId, ev.state, ev.gpu_worker_id, ev.queue_position);
                        continue;
                    }
                    if (ev.progress) {
                        const p = ev.progress;
                        const stage = p.stage;
                        if (stage === 'started') setVoiceStatus('sending', 'Audio received...');
                        else if (stage === 'whisper_transcribing') setVoiceStatus('sending', 'Transcribing...');
                        else if (stage === 'transcribing') setVoiceStatus('sending', 'Transcribing... ' + (p.segments||0) + ' segments');
                        else if (stage === 'whisper_done') setVoiceStatus('sending', 'Transcription done (' + (p.tokens||0) + ' tokens)');
                        else if (stage === 'whisperx_transcribing') setVoiceStatus('sending', 'Verifying transcription...');
                        else if (stage === 'whisperx_aligning') setVoiceStatus('sending', 'Aligning words...');
                        else if (stage === 'computing_similarity') setVoiceStatus('sending', 'Computing self-similarity...');
                        else setVoiceStatus('sending', stage);
                        log('[STT] Progress: ' + stage + (p.segments ? ' segs=' + p.segments : '') + (p.tokens ? ' tok=' + p.tokens : ''));
                        continue;
                    }
                    // Final result (has text field, no progress field)
                    if (ev.text !== undefined || ev.medium_elapsed !== undefined) {
                        output = ev;
                        sttDone = true;
                        break;
                    }
                }
                if (!sttDone && (sData.status === 'COMPLETED' || sData.status === 'FAILED')) {
                    sttDone = true;
                }
            } catch(e) {
                log('[STT] Poll error: ' + e.message);
            }
            // Fallback: check /status if stream gave nothing after 20s
            if (!sttDone && (performance.now() - sttT0) > 20000) {
                try {
                    const stResp = await fetch(statusUrl, { headers: pollHeaders });
                    if (stResp.ok) {
                        const stData = await stResp.json();
                        if (stData.status === 'COMPLETED') {
                            output = stData.output || {};
                            if (Array.isArray(output)) output = output[output.length - 1] || {};
                            sttDone = true;
                        } else if (stData.status === 'FAILED') {
                            throw new Error('RunPod job failed');
                        }
                    }
                } catch(e) { log('[STT] Status check error: ' + e.message); }
            }
        }
        const sttElapsed = Math.round(performance.now() - sttT0);
        log('[STT] Complete in ' + sttElapsed + 'ms');
        HC.gpuJobEnd(jobId, 'success');
        } // end HTTP polling else block
        HC.jobCheckpoint(_vjId, { status: 'transcribed' });

        // Clear from in-flight tracking
        // Don't bail if entry is missing — recovery resubmits may arrive after
        // the loss handler already deleted the entry. The result is still valid.
        const entry = _sttInFlight.get(seq);
        if (entry) {
            if (entry.timer) clearTimeout(entry.timer);
            _sttInFlight.delete(seq);
        }

        // ── Filter helper (DRY the repeated pattern) ──
        function filterOut(reason) {
            log('[STT] Filtered: ' + reason + ' seq=' + seq);
            var displayText = mediumText || wxText || '';
            setVoiceStatus('', '🗑️ Filtered: "' + displayText.substring(0, 25) + (displayText.length > 25 ? '…' : '') + '" (' + reason.split(' ')[0] + ')');
            revertVoiceStatusAfter(3000, '', isListening ? LISTENING_HTML : '');
            if (_sttInFlight.size > 0) updatePendingDisplay();
            else if (!isListening) setTimeout(hideVoiceStatus, 2000);
        }

        // ── Garbage filter: GPU worker flags hallucinated transcriptions ──
        if (output.garbage === true) { filterOut('gpu-garbage'); return; }

        // ── Parse dual-model results ──
        const mediumText = (output.text || '').trim();
        const wxText = (output.whisperx_text || '').trim();
        const mediumElapsed = output.medium_elapsed || 0;
        const wxElapsed = output.whisperx_elapsed || 0;
        const embSim = output.embedding_similarity;

        // ── Local quick filters (ported from voice-web tmux-bridge.py) ──

        // 1. Both models empty
        if (!mediumText && !wxText) { filterOut('both-empty'); return; }

        // 2. Zero-length audio (0.0s) — nothing to transcribe
        if (output.duration_sec !== undefined && output.duration_sec < 0.1) {
            filterOut('zero-length-audio'); return;
        }

        // 3. Low embedding similarity (models disagree → likely noise)
        if (typeof embSim === 'number' && embSim < 0.6) {
            filterOut('low-similarity emb=' + embSim.toFixed(2));
            return;
        }

        // 4. One model empty or similarity not computed → use 0.40 baseline.
        //    This covers: only one model available, one model returned empty,
        //    or the embedding similarity wasn't computed.
        //    Note: embSim is const, so we use a new variable for the effective value.
        var effectiveEmbSim = embSim;
        if (typeof embSim !== 'number') {
            effectiveEmbSim = 0.40;
            if (!mediumText || !wxText) {
                log('[STT] One model empty — using embSim baseline 0.40');
            }
        }

        // ── Confidence tier (same thresholds as voice-web) ──
        let tier = 'medium';
        if (typeof effectiveEmbSim === 'number') {
            if (effectiveEmbSim >= 0.85) tier = 'high';
            else if (effectiveEmbSim < 0.6) tier = 'noise';
            else tier = 'medium';
        } else {
            tier = 'low';
        }

        // ── Embedding-based garbage check (all client-side) ──
        // Dynamic threshold = embedding similarity between models.
        // High agreement → high threshold (hard to filter real speech).
        const dynamicThreshold = garbageThreshold(effectiveEmbSim);
        const textsToCheck = [mediumText, wxText].filter(t => t.length >= 3);
        if (textsToCheck.length > 0 && HC._garbageEntries.length > 0) {
            try {
                // Embed via gpuRun — same path as streaming garbage check
                const submitResult = await HC.gpuRun({ action: 'embed', texts: textsToCheck, task_type: 'search_query' });
                let vecs = null;
                if (submitResult && submitResult.wsController) {
                    const embedOutput = await new Promise(function(resolve) {
                        var timer = setTimeout(function() { resolve(null); }, 10000);
                        submitResult.wsController.onResult = function(output) { clearTimeout(timer); resolve(output); };
                        submitResult.wsController.onError = function() { clearTimeout(timer); resolve(null); };
                    });
                    vecs = embedOutput && embedOutput.embeddings;
                }
                if (vecs && vecs.length > 0) {
                    var garbageMatch = garbageCheck(vecs, textsToCheck, dynamicThreshold);
                    if (garbageMatch) {
                        filterOut('garbage-db match="' + garbageMatch.text.substring(0,30) + '" sim=' + garbageMatch.sim.toFixed(2) + ' thresh=' + dynamicThreshold.toFixed(2) + ' via=' + garbageMatch.model);
                        return;
                    }
                } else {
                    log('[STT] Garbage check: no embeddings returned');
                }
            } catch(garbErr) {
                log('[STT] Garbage check failed (non-blocking): ' + garbErr.message);
            }
        } else {
            const reason = !HC.GPU_URL ? 'no GPU configured' : HC._garbageEntries.length === 0 ? 'no garbage entries loaded' : 'no texts to check';
            log('[STT] Garbage check skipped: ' + reason);
        }

        // ── Optionally diarize short utterance ──
        const diarizeToggle = document.getElementById('stt-diarize');
        const diarizationEnabled = diarizeToggle ? diarizeToggle.checked : true;
        const wordSegs = output.word_segments || [];
        let diarizedText = '';
        let diarizeElapsed = 0;

        if (diarizationEnabled && wordSegs.length > 0 && audioBlob) {
            try {
                // Temporarily populate state for _diarizeFullAudio
                _partialResults = [{ text: mediumText, wxText: wxText, wordSegments: wordSegs, chunkOffset: 0,
                    mediumElapsed: mediumElapsed, wxElapsed: wxElapsed, embSim: effectiveEmbSim }];
                _segmentBlobs = [audioBlob];
                _cumulativeOffset = parseFloat(durSec) || 0;
                const diarizeResult = await _diarizeFullAudio();
                if (diarizeResult) {
                    diarizedText = diarizeResult.text;
                    diarizeElapsed = diarizeResult.elapsed;
                }
                _resetSegmentationState();
            } catch(dErr) {
                log('[STT] Short diarize failed: ' + dErr.message);
                _resetSegmentationState();
            }
        }

        // ── Format dual-model injection string ──
        // Text model = clean text (no speaker tags), Voices model = speaker-tagged text
        let injection = '';
        const simPct = typeof effectiveEmbSim === 'number' ? Math.round(effectiveEmbSim * 100) + '%' : 'n/a';
        const energyStr = typeof avgEnergy === 'number' ? ', energy: ' + avgEnergy.toFixed(0) : '';
        const diarizeStr = diarizeElapsed ? ', diarize: ' + diarizeElapsed.toFixed(1) + 's' : '';
        const closingTag = `[audio length: ${durSec}s, self-similarity: ${simPct}${energyStr}${diarizeStr}]`;
        const filesTagShort = _filesVisitedDuringRecording.length > 0
            ? ' [files viewed: ' + _filesVisitedDuringRecording.join(', ') + ']'
            : '';
        const voicesText = diarizedText || wxText;
        const metaParts = [
            mediumElapsed.toFixed(1) + 's',
            voicesText ? wxElapsed.toFixed(1) + 's' : null,
            durSec + 's audio',
            simPct + ' sim',
        ].filter(Boolean).join(', ');
        if (voicesText) {
            injection = `${mediumText || '(empty)'} | ${voicesText || '(empty)'} [${metaParts}]${filesTagShort}`;
        } else {
            injection = `${mediumText || '(empty)'} [${metaParts}]${filesTagShort}`;
        }

        // Store full transcription metadata for overlay popup
        if (!HC._transcriptionHistory) HC._transcriptionHistory = [];
        HC._transcriptionHistory.push({
            textModel: mediumText || '',
            voicesModel: voicesText || '',
            elapsedM: mediumElapsed,
            elapsedX: wxElapsed,
            audioDur: durSec,
            similarity: simPct,
            injection: injection,
            ts: Date.now(),
        });
        while (HC._transcriptionHistory.length > 30) HC._transcriptionHistory.shift();

        if (injection.trim()) {
            lastTranscriptionTime = Date.now();
            log('[STT] Result: M=\'' + (mediumText||'').substring(0,40) + '\' X=\'' + (wxText||'').substring(0,40) + '\' emb=' + (typeof effectiveEmbSim==='number'?effectiveEmbSim.toFixed(2):'n/a') + ' tier=' + tier + ' seq=' + seq);
            await HC.daemonFetch('/api/transcription', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ text: injection, voice_job_id: _vjId }),
            });
            log('[STT] Injected to PTY seq=' + seq);
            HC.jobCheckpoint(_vjId, { status: 'injected' });
            HC.jobCheckpoint(_vjId, { status: 'done' });
            setVoiceStatus('success', '\ud83c\udfa4 ' + (mediumText || wxText));
            revertVoiceStatusAfter(1500, 'success', '\u2705 Transcribed');
            revertVoiceStatusAfter(2000, '', isListening ? LISTENING_HTML : '');
            HC.spendCredit(); // instant local decrement + background server sync
            _filesVisitedDuringRecording = [];
        }

        if (_sttInFlight.size > 0) updatePendingDisplay();
        else if (!isListening) setTimeout(hideVoiceStatus, 2000);
    } catch(e) {
        log('[STT] Browser-direct failed: ' + e.message + ' seq=' + seq);
        if (typeof jobId !== 'undefined') HC.gpuJobEnd(jobId, 'error');
        if (typeof _vjId !== 'undefined') HC.jobCheckpoint(_vjId, { status: 'failed', error: e.message, error_stage: 'gpu_transcribe' });
        // Don't clear from in-flight — let timeout handle retry
        if (!_sttInFlight.has(seq)) {
            // Already removed (timed out), show error
            setVoiceStatus('recording', '\u274c Transcription failed');
            revertVoiceStatusAfter(3000, '', isListening ? 'Listening...' : '');
        }
    }
}


// Expose to HC namespace
HC.startListening = startListening;
HC.stopListening = stopListening;
HC.submitTranscription = submitTranscription;
HC._getOrphanedRecordings = _getOrphanedRecordings;
HC.isRecording = function() { return isSpeaking; };
HC.trackFileVisit = function(path) {
    if (path && _filesVisitedDuringRecording.indexOf(path) === -1) {
        _filesVisitedDuringRecording.push(path);
    }
};
HC.setVoiceStatus = setVoiceStatus;
HC._handleTtsLost = _handleTtsLost;
HC._deleteAudioBlob = _deleteAudioBlob;
HC._VAD_GATE_ENABLED = VAD_GATE_ENABLED;

// Called by gpu.js when a RunPod worker first connects (wasNull path in _refreshGpuConfig).
// Drains _pendingTranscribeQueue — fires _doGpuTranscribe for each entry that is still
// in-flight (not yet timed out). Safe to call if queue is empty.
HC._drainTranscribeQueue = function() {
    if (_pendingTranscribeQueue.length === 0) return;
    var entries = _pendingTranscribeQueue.splice(0);
    log('[STT] Worker connected — draining ' + entries.length + ' queued transcription(s)');
    for (var i = 0; i < entries.length; i++) {
        var e = entries[i];
        if (_sttInFlight.has(e.seq)) {
            _doGpuTranscribe(e.seq, e.audioBlob, e.durSec, e.avgEnergy);
        } else {
            log('[STT] Queue entry seq=' + e.seq + ' already expired — skipping');
        }
    }
};
