/* tts.js — Babel3 TTS queue and streaming playback (open-source)
 *
 * Serial TTS playback queue, streaming TTS from GPU worker,
 * playback history with replay, mini player UI.
 *
 * Depends on: core.js (HC namespace), voice-play.js (HC.GaplessStreamPlayer),
 *             gpu.js (HC.gpuRun, HC.gpuStreamUrl, etc.)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// ── Streaming TTS (GaplessStreamPlayer) ─────────────────
// Ported from voice-web. Browser calls GPU /run, polls /stream/{jobId},
// plays sub-chunks via Web Audio API with zero-gap scheduling.
// Voice playback extracted to static/open/js/voice-play.js
var gaplessPlayer = new HC.GaplessStreamPlayer();

// iOS: ensure AudioContext is created/resumed on any user gesture
document.addEventListener('click', function() { gaplessPlayer.ensureContext(); });

// ── User interaction tracking (for AudioContext autoplay policy) ──
var _markInteracted = function() {
    HC._setUserInteracted();
    // Pre-unlock HTML audio element from user gesture
    if (typeof gaplessPlayer !== 'undefined') gaplessPlayer.unlockHtmlAudio();
    // Notify daemon that this browser can now play audio
    if (HC.ws && HC.ws.readyState === WebSocket.OPEN) {
        HC.ws.send(JSON.stringify({ type: 'audio_context_change', audio_context: true }));
    }
};
document.addEventListener('click', _markInteracted, { once: true });
document.addEventListener('touchend', _markInteracted, { once: true });
document.addEventListener('keydown', _markInteracted, { once: true });

// ── Serial TTS playback queue ───────────────────────────
// Generation (GPU fetch) starts immediately for all queued messages;
// playback serializes — message N+1 waits for message N to finish playing.
const _ttsQueue = [];
let _ttsRunning = false;

// ── TTS playback history ────────────────────────────────
// Daemon archive is the source of truth. Browser keeps a local mirror
// that syncs from daemon on connect and on every prev/next navigation.
const _ttsHistory = [];
let _historyIndex = -1;  // -1 = live; >=0 = replaying history entry
let _historyRefreshing = false;

// Sync history from daemon archive. Replaces local array with daemon's
// ordered list (oldest-first). Preserves cursor position by msg_id.
async function _refreshHistoryFromDaemon() {
    if (_historyRefreshing) return;
    _historyRefreshing = true;
    try {
        var resp = await HC.daemonFetch('/api/tts-history');
        if (!resp.ok) return;
        var entries = await resp.json();
        if (!entries || !entries.length) return;
        // Daemon returns newest-first; reverse to oldest-first for array indexing
        entries.reverse();
        // Remember current cursor position by msg_id
        var cursorMsgId = HC._prevCursor >= 0 && _ttsHistory[HC._prevCursor]
            ? _ttsHistory[HC._prevCursor].msgId : null;
        // Replace local history with daemon's version
        _ttsHistory.length = 0;
        for (var i = 0; i < entries.length; i++) {
            var e = entries[i];
            _ttsHistory.push({
                msgId: e.msg_id,
                textPreview: e.text || '',
                originalText: e.original_text || e.text || '',
                emotion: e.emotion || '',
                replyingTo: e.replying_to || '',
                audioUrl: '/api/tts-archive/' + e.msg_id,
                fromArchive: true
            });
        }
        // Restore cursor position
        if (cursorMsgId) {
            for (var j = 0; j < _ttsHistory.length; j++) {
                if (_ttsHistory[j].msgId === cursorMsgId) {
                    HC._prevCursor = j;
                    break;
                }
            }
        }
        _updateHistoryControls();
        log('[History] Synced ' + _ttsHistory.length + ' entries from daemon archive');
    } catch (e) {
        log('[History] Daemon sync failed: ' + e.message);
    } finally {
        _historyRefreshing = false;
    }
}

function _updateHistoryControls() {
    const prevBtn = document.getElementById('fs-player-prev');
    const stopBtn = document.getElementById('fs-player-stop');
    const pauseBtn = document.getElementById('fs-player-pause');
    const nextBtn = document.getElementById('fs-player-next');
    const hasHistory = _ttsHistory.length > 0;
    const hasQueue = _ttsQueue.length > 0 || _ttsRunning;
    if (prevBtn) prevBtn.disabled = !hasHistory;
    if (stopBtn) stopBtn.disabled = !hasHistory && !hasQueue;
    if (pauseBtn) {
        pauseBtn.disabled = !hasHistory && !hasQueue;
        if (!gaplessPlayer.isPaused()) { pauseBtn.innerHTML = '&#x23F8;&#xFE0E;'; pauseBtn.title = 'Pause'; }
    }
    if (nextBtn) nextBtn.disabled = HC._prevCursor < 0 || HC._prevCursor >= _ttsHistory.length - 1;
}
// Show transport buttons immediately on page load for balanced layout
_updateHistoryControls();

async function _replayHistoryEntry(entry) {
    if (!entry || !entry.audioUrl) return;
    // Stop any currently playing audio first (prevents overlap)
    gaplessPlayer.cleanup();
    log('[History] Replaying ' + entry.msgId);
    HC.setVoiceStatus('info', 'Replaying: ' + (entry.textPreview || '').substring(0, 30) + '...');
    if (entry.emotion) HC.setLedEmotion(entry.emotion);
    try {
        const resp = entry.fromArchive
            ? await HC.daemonFetch(entry.audioUrl)
            : await fetch(entry.audioUrl);
        if (!resp.ok) { HC.setVoiceStatus('error', 'Replay fetch failed'); return; }
        const wavBytes = await resp.arrayBuffer();
        gaplessPlayer.ensureContext();
        gaplessPlayer.start(entry.msgId + '-replay');
        gaplessPlayer._textPreview = entry.textPreview || '';
        gaplessPlayer._replyingTo = entry.replyingTo || '';
        // Update popup text immediately with full text
        var popupText = document.getElementById('fs-player-text');
        if (popupText) {
            if (entry.replyingTo) {
                popupText.innerHTML = '<span class="replying-to">\u25B6 ' + entry.replyingTo.replace(/</g, '&lt;').substring(0, 200) + '</span><br>' + (entry.textPreview || 'Replaying...');
            } else {
                popupText.textContent = entry.textPreview || 'Replaying...';
            }
        }
        _updateHistoryControls();
        await gaplessPlayer.scheduleChunk(wavBytes, 0);
        await gaplessPlayer.finalize(1, null, null, /*skipUpload=*/true);
    } catch (e) {
        log('[History] Replay error: ' + e.message);
        HC.setVoiceStatus('error', 'Replay failed');
    }
    _updateHistoryControls();
    // Keep cursor position — don't reset. User can continue navigating
    // with prev/next from current position. Cursor resets on stop or
    // when a new live TTS message starts playing.
}

function enqueueTts(msg) {
    _ttsQueue.push(msg);
    HC._prevCursor = -1; // Reset history cursor — new live message takes priority
    _updateHistoryControls();
    if (!_ttsRunning) _drainTtsQueue();
}

async function _drainTtsQueue() {
    if (_ttsRunning || _ttsQueue.length === 0) return;
    _ttsRunning = true;
    _updateHistoryControls();
    while (_ttsQueue.length > 0) {
        const next = _ttsQueue.shift();
        try {
            // Timeout: if streamFromGpu hangs for >120s, force-release the queue
            await Promise.race([
                streamFromGpu(next),
                new Promise((_, reject) => setTimeout(() => reject(new Error('TTS timeout (120s)')), 120000))
            ]);
        } catch (e) { log('[TTS-Queue] [msg=' + (next.msg_id||'?') + '] Error: ' + e.message); }
    }
    _ttsRunning = false;
    _updateHistoryControls();
}

// ── Streaming TTS from GPU ──────────────────────────────
async function streamFromGpu(msg) {
    const { msg_id, text, original_text, voice, emotion, replying_to,
            split_chars, min_chunk_chars, conditionals_b64 } = msg;

    const t0 = performance.now();
    HC.setVoiceStatus('info', 'Streaming audio...');

    // Register voice job for reliability tracking
    const _vjId = crypto.randomUUID ? crypto.randomUUID() : (Date.now().toString(36) + Math.random().toString(36).slice(2));
    HC.registerJob({ id: _vjId, agent_id: HC.config.agentId || '', job_type: 'tts', msg_id: msg_id, text_preview: (text || '').substring(0, 200), voice: voice });

    // 1. Submit async job to GPU via gpuRun (local-first + RunPod fallback)
    const preferredVoice = localStorage.getItem('b3-voice-' + HC.config.agentId) || 'arabella-chase';
    const effectiveVoice = (!voice || voice === 'default') ? preferredVoice : voice;
    const effectiveCond = conditionals_b64 || await HC.fetchVoiceConditionals(effectiveVoice);

    // TTS generation parameters (stored in localStorage for A/B testing)
    const ttsTemp = parseFloat(localStorage.getItem('b3-tts-temperature-' + HC.config.agentId) || '0.8');
    const ttsRepPen = parseFloat(localStorage.getItem('b3-tts-repetition-penalty-' + HC.config.agentId) || '1.2');
    const ttsMinP = parseFloat(localStorage.getItem('b3-tts-min-p-' + HC.config.agentId) || '0.05');
    const ttsTopP = parseFloat(localStorage.getItem('b3-tts-top-p-' + HC.config.agentId) || '1.0');

    const input = {
        action: 'synthesize_stream',
        text, voice: effectiveVoice,
        exaggeration: 0.5, cfg_weight: 0.5,
        temperature: ttsTemp, repetition_penalty: ttsRepPen,
        min_p: ttsMinP, top_p: ttsTopP,
        msg_id,
        voice_job_id: _vjId,
    };
    if (split_chars) input.split_chars = split_chars;
    if (min_chunk_chars) input.min_chunk_chars = min_chunk_chars;
    if (effectiveCond) input.conditionals_b64 = effectiveCond;
    log('[TTS-Stream] [msg=' + msg_id + '] params: temp=' + ttsTemp + ' rep_pen=' + ttsRepPen + ' min_p=' + ttsMinP + ' top_p=' + ttsTopP);

    let jobId, backend, baseUrl, token, wsController;
    for (let attempt = 0; attempt < 2; attempt++) {
        try {
            const result = await HC.gpuRun(input);
            jobId = result.data.id;
            backend = result.backend;
            baseUrl = result.baseUrl;
            token = result.token;
            wsController = result.wsController || null;
            log('[TTS-Stream] [msg=' + msg_id + '] Job submitted: ' + jobId + ' (' + backend + ', voice=' + effectiveVoice + ')');
            HC.jobCheckpoint(_vjId, { status: 'gpu_submitted', gpu_job_id: jobId, gpu_backend: backend });
            HC.gpuJobStart(jobId, 'synthesize_stream');
            break;
        } catch (err) {
            if (attempt === 0) {
                log('[TTS-Stream] [msg=' + msg_id + '] Submit failed (retrying in 2s): ' + err.message);
                await new Promise(r => setTimeout(r, 2000));
                continue;
            }
            log('[TTS-Stream] [msg=' + msg_id + '] Submit failed: ' + err.message);
            if (jobId) HC.gpuJobEnd(jobId, 'error');
            HC.jobCheckpoint(_vjId, { status: 'failed', error: err.message, error_stage: 'gpu_submit' });
            HC._handleTtsLost(msg_id, text, err.message, 'submit');
            return;
        }
    }

    HC.setVoiceStatus('info', 'Streaming audio (' + backend + ')...');

    // 2. Start gapless player
    gaplessPlayer.ensureContext();
    gaplessPlayer.start(msg_id);
    gaplessPlayer._textPreview = text || '';
    gaplessPlayer._voice = effectiveVoice || '';
    gaplessPlayer._voiceJobId = _vjId;
    gaplessPlayer._emotion = emotion || '';
    gaplessPlayer._replyingTo = replying_to || '';
    gaplessPlayer._originalText = original_text || text || '';
    // Pre-compute chunk→text mapping for trim logs
    gaplessPlayer._chunkTexts = gaplessPlayer._splitSentences(text || '', split_chars || '.!?', min_chunk_chars || 80);
    const streamGen = gaplessPlayer._generation;
    // Estimate chunk count from text length for progress bar segmentation
    const effectiveMinChars = min_chunk_chars || 80;
    const estChunks = Math.max(1, Math.ceil((text || '').length / effectiveMinChars));
    gaplessPlayer.setEstimatedChunks(estChunks);

    // 3. Stream results — WebSocket (push) or HTTP polling (pull)
    let done = false;
    let lastChunkIndex = -1;
    let totalChunks = null;

    if (wsController) {
        // ── WebSocket path: event-driven, no polling ──
        await new Promise(function(resolve, reject) {
            wsController.onAccepted = function(msg) {
                HC.gpuJobAccepted(jobId, msg.state, msg.gpu_worker_id);
            };
            wsController.onProgress = function(p) {
                var stage = p.stage;
                if (stage === 'splitting') HC.setVoiceStatus('info', 'Splitting text...');
                else if (stage === 'split_done') {
                    HC.setVoiceStatus('info', 'Split into ' + p.chunks + ' chunks');
                    if (gaplessPlayer._generation === streamGen) gaplessPlayer.setEstimatedChunks(p.chunks);
                }
                else if (stage === 'loading_tts_model') HC.setVoiceStatus('info', 'Loading TTS model (cold start)...');
                else if (stage === 'waiting_for_model') HC.setVoiceStatus('info', 'Waiting for model...');
                else if (stage === 'loading_voice') HC.setVoiceStatus('info', 'Loading voice \'' + (p.voice||'') + '\'...');
                else if (stage === 'voice_ready') HC.setVoiceStatus('info', 'Voice ready');
                else if (stage === 'generating') HC.setVoiceStatus('info', 'Generating ' + p.chunk + '/' + p.total + '...');
                else HC.setVoiceStatus('info', stage);
                log('[TTS-Stream] [msg=' + msg_id + '] Progress: ' + stage + (p.chunk ? ' ' + p.chunk + '/' + p.total : ''));
            };
            wsController.onChunk = function(chunk) {
                if (chunk.chunk_index === undefined || chunk.chunk_index <= lastChunkIndex) return;
                lastChunkIndex = chunk.chunk_index;
                if (chunk.total_chunks && gaplessPlayer._generation === streamGen) {
                    gaplessPlayer.setEstimatedChunks(chunk.total_chunks);
                }
                var chunkMs = Math.round(performance.now() - t0);
                log('[TTS-Stream] [msg=' + msg_id + '] Chunk ' + (chunk.chunk_index + 1) + ' received: ' + chunkMs + 'ms dur=' + (chunk.duration_sec||'?') + 's');
                if (chunk.chunk_index === 0) log('[TTS-Stream] [msg=' + msg_id + '] First audio at ' + chunkMs + 'ms');
                var raw = atob(chunk.audio_b64);
                var wavBytes = new ArrayBuffer(raw.length);
                var view = new Uint8Array(wavBytes);
                for (var i = 0; i < raw.length; i++) view[i] = raw.charCodeAt(i);
                HC.reportVoiceEvent(msg_id, chunk.chunk_index, 'delivered');
                if (gaplessPlayer._generation === streamGen) {
                    gaplessPlayer.scheduleChunk(wavBytes, chunk.chunk_index);
                }
            };
            wsController.onDone = function(msg) {
                totalChunks = msg.total_chunks;
                done = true;
                log('[TTS-Stream] [msg=' + msg_id + '] Complete: ' + totalChunks + ' chunks in ' + Math.round(performance.now() - t0) + 'ms');
                resolve();
            };
            wsController.onError = function(err) {
                log('[TTS-Stream] [msg=' + msg_id + '] WS error: ' + err + ' — marking WS unreliable');
                HC._gpuWsReady = false; // Force next TTS to use HTTP/RunPod
                HC._handleTtsLost(msg_id, text, err, 'ws_error');
                done = true;
                resolve(); // Don't reject — let finalize run
            };

            // RTC demotion: if no progress within 10s and RTC is active,
            // the RTC data channel is likely zombie. Demote to WS.
            var _rtcDemotionTimer = setTimeout(function() {
                if (!done && HC.gpuChannel) {
                    var st = HC.gpuChannel.status();
                    if (st.active === 'rtc') {
                        log('[TTS-Stream] [msg=' + msg_id + '] No progress in 10s with RTC active — demoting to WS');
                        HC.gpuChannel._disconnectTransport('rtc');
                        // Resubmit through WS
                        wsController.cancel();
                        var newCtrl = HC.gpuSubmit(input);
                        if (newCtrl) {
                            jobId = newCtrl.jobId;
                            log('[TTS-Stream] [msg=' + msg_id + '] Resubmitted via WS: ' + jobId);
                            newCtrl.onAccepted = wsController.onAccepted;
                            newCtrl.onProgress = wsController.onProgress;
                            newCtrl.onChunk = wsController.onChunk;
                            newCtrl.onDone = wsController.onDone;
                            newCtrl.onError = wsController.onError;
                        }
                    }
                }
            }, 10000);
            wsController._origOnDone = wsController.onDone;
            wsController.onDone = function(msg) {
                clearTimeout(_rtcDemotionTimer);
                wsController._origOnDone(msg);
            };
            wsController._origOnChunk = wsController.onChunk;
            wsController.onChunk = function(chunk) {
                clearTimeout(_rtcDemotionTimer);
                wsController._origOnChunk(chunk);
            };
        });
        HC.gpuJobEnd(jobId, 'success');
    } else {
    // ── HTTP polling path (RunPod or WS unavailable) ──
    const streamUrl = HC.gpuStreamUrl(baseUrl, jobId);
    const statusUrl = HC.gpuStatusUrl(baseUrl, jobId);
    const pollHeaders = HC.gpuPollHeaders(token);

    let warmupShown = false;
    let warmupInterval = null;
    let warmupCountdown = 30;

    while (!done) {
        await new Promise(r => setTimeout(r, 400));

        try {
            const resp = await fetch(streamUrl, { headers: pollHeaders });
            if (!resp.ok) {
                if (resp.status === 404) {
                    // Container still booting — start countdown after 3s
                    const elapsed = Math.round((performance.now() - t0) / 1000);
                    if (elapsed >= 3 && !warmupShown) {
                        warmupShown = true;
                        warmupCountdown = 30;
                        HC.setVoiceStatus('info', 'Warming up... ~' + warmupCountdown + 's');
                        log('[TTS-Stream] [msg=' + msg_id + '] Cold start detected — container booting');
                        warmupInterval = setInterval(() => {
                            if (warmupCountdown > 1) warmupCountdown--;
                            HC.setVoiceStatus('info', 'Warming up... ~' + warmupCountdown + 's');
                        }, 1000);
                    }
                    continue;
                }
                log('[TTS-Stream] [msg=' + msg_id + '] /stream error: HTTP ' + resp.status);
                continue;
            }
            if (warmupShown) {
                if (warmupInterval) { clearInterval(warmupInterval); warmupInterval = null; }
                const warmupDuration = Math.round((performance.now() - t0) / 1000);
                log('[TTS-Stream] [msg=' + msg_id + '] Warmup complete after ' + warmupDuration + 's');
                HC.setVoiceStatus('info', 'Streaming audio...');
            }

            const data = await resp.json();
            const outputs = data.stream || (Array.isArray(data) ? data : []);

            for (const item of outputs) {
                const chunk = item.output || item;
                if (chunk.error) {
                    log('[TTS-Stream] [msg=' + msg_id + '] GPU error: ' + chunk.error);
                    HC._handleTtsLost(msg_id, text, chunk.error, 'gpu_error');
                    done = true; break;
                }
                if (chunk.done) {
                    done = true;
                    totalChunks = chunk.total_chunks;
                    log('[TTS-Stream] [msg=' + msg_id + '] Complete: ' + totalChunks + ' chunks in ' + Math.round(performance.now() - t0) + 'ms');
                    break;
                }
                // GPU state report (first event from worker)
                if (chunk.status === 'accepted' && chunk.state) {
                    HC.gpuJobAccepted(jobId, chunk.state, chunk.gpu_worker_id);
                    continue;
                }
                // GPU worker progress events (no audio, just status)
                if (chunk.progress) {
                    const p = chunk.progress;
                    const stage = p.stage;
                    if (stage === 'splitting') HC.setVoiceStatus('info', 'Splitting text...');
                    else if (stage === 'split_done') {
                        HC.setVoiceStatus('info', 'Split into ' + p.chunks + ' chunks');
                        if (gaplessPlayer._generation === streamGen) gaplessPlayer.setEstimatedChunks(p.chunks);
                    }
                    else if (stage === 'loading_tts_model') HC.setVoiceStatus('info', 'Loading TTS model (cold start)...');
                    else if (stage === 'waiting_for_model') HC.setVoiceStatus('info', 'Waiting for model...');
                    else if (stage === 'loading_voice') HC.setVoiceStatus('info', 'Loading voice \'' + (p.voice||'') + '\'...');
                    else if (stage === 'voice_ready') HC.setVoiceStatus('info', 'Voice ready');
                    else if (stage === 'generating') HC.setVoiceStatus('info', 'Generating ' + p.chunk + '/' + p.total + '...');
                    else HC.setVoiceStatus('info', stage);
                    log('[TTS-Stream] [msg=' + msg_id + '] Progress: ' + stage + (p.chunk ? ' ' + p.chunk + '/' + p.total : ''));
                    continue;
                }
                if (chunk.chunk_index === undefined || chunk.chunk_index <= lastChunkIndex) continue;
                lastChunkIndex = chunk.chunk_index;

                // Update estimated total from GPU-provided total_chunks
                if (chunk.total_chunks && gaplessPlayer._generation === streamGen) {
                    gaplessPlayer.setEstimatedChunks(chunk.total_chunks);
                }

                const chunkMs = Math.round(performance.now() - t0);
                log('[TTS-Stream] [msg=' + msg_id + '] Chunk ' + (chunk.chunk_index + 1) + ' received: ' + chunkMs + 'ms dur=' + (chunk.duration_sec||'?') + 's');
                if (chunk.chunk_index === 0) log('[TTS-Stream] [msg=' + msg_id + '] First audio at ' + chunkMs + 'ms');

                // Decode base64 → ArrayBuffer
                const raw = atob(chunk.audio_b64);
                const wavBytes = new ArrayBuffer(raw.length);
                const view = new Uint8Array(wavBytes);
                for (let i = 0; i < raw.length; i++) view[i] = raw.charCodeAt(i);

                HC.reportVoiceEvent(msg_id, chunk.chunk_index, 'delivered');
                if (gaplessPlayer._generation === streamGen) {
                    await gaplessPlayer.scheduleChunk(wavBytes, chunk.chunk_index);
                }
            }

            if (data.status === 'FAILED') {
                HC._handleTtsLost(msg_id, text, 'RunPod job FAILED', 'runpod_failed');
                done = true;
            } else if (data.status === 'COMPLETED') {
                done = true;
            }
        } catch (e) {
            log('[TTS-Stream] [msg=' + msg_id + '] Poll error: ' + e.message);
        }

        // Check status endpoint if stream gave nothing
        if (!done && !streamHadData) {
            try {
                const sResp = await fetch(statusUrl, { headers: pollHeaders });
                if (sResp.ok) {
                    const sData = await sResp.json();
                    if (sData.status === 'FAILED') {
                        HC._handleTtsLost(msg_id, text, 'RunPod job FAILED (status check)', 'runpod_failed');
                        done = true;
                    } else if (sData.status === 'COMPLETED') {
                        done = true;
                    }
                }
            } catch(e) {}
        }
    }

    if (warmupInterval) { clearInterval(warmupInterval); warmupInterval = null; }
    HC.gpuJobEnd(jobId, 'success');

    // Drain: if loop exited via COMPLETED status without chunk.done,
    // there may be un-fetched chunks. Do one final /stream poll.
    if (totalChunks === null) {
        try {
            const drainResp = await fetch(streamUrl, { headers: pollHeaders });
            if (drainResp.ok) {
                const drainData = await drainResp.json();
                const drainOutputs = drainData.stream || (Array.isArray(drainData) ? drainData : []);
                for (const item of drainOutputs) {
                    const chunk = item.output || item;
                    if (chunk.done) {
                        totalChunks = chunk.total_chunks;
                        log('[TTS-Stream] [msg=' + msg_id + '] Drain: Complete marker found, ' + totalChunks + ' total chunks');
                        continue;
                    }
                    if (chunk.progress || chunk.chunk_index === undefined || chunk.chunk_index <= lastChunkIndex) continue;
                    lastChunkIndex = chunk.chunk_index;
                    const raw = atob(chunk.audio_b64);
                    const wavBytes = new ArrayBuffer(raw.length);
                    const view = new Uint8Array(wavBytes);
                    for (let i = 0; i < raw.length; i++) view[i] = raw.charCodeAt(i);
                    log('[TTS-Stream] [msg=' + msg_id + '] Drain: Chunk ' + (chunk.chunk_index + 1) + ' recovered dur=' + (chunk.duration_sec||'?') + 's');
                    HC.reportVoiceEvent(msg_id, chunk.chunk_index, 'delivered');
                    if (gaplessPlayer._generation === streamGen) {
                        await gaplessPlayer.scheduleChunk(wavBytes, chunk.chunk_index);
                    }
                }
            }
        } catch(e) { log('[TTS-Stream] [msg=' + msg_id + '] Drain poll error: ' + e.message); }
    }
    } // end HTTP polling else block

    // 4. Finalize — wait for all audio to finish playing
    const networkMs = Math.round(performance.now() - t0);
    log('[TTS-Stream] [msg=' + msg_id + '] Network done: ' + networkMs + 'ms, waiting for playback...');
    HC.jobCheckpoint(_vjId, { status: 'generating', chunks_total: totalChunks });
    if (gaplessPlayer.isActive() && gaplessPlayer._generation === streamGen) {
        const textPreview = text || '';
        await gaplessPlayer.finalize(totalChunks, () => {
            const playbackMs = Math.round(performance.now() - t0);
            const metrics = gaplessPlayer.getMetrics();
            log('[TTS-Stream] [msg=' + msg_id + '] Playback complete: ' + playbackMs + 'ms (network=' + networkMs + 'ms)' +
                (metrics ? ' gaps=' + metrics.gap_count + ' dur=' + metrics.total_duration_sec + 's' : ''));
            if (HC._ttsPlayedMsgIds) HC._ttsPlayedMsgIds[msg_id] = true;
            HC.jobCheckpoint(_vjId, { status: 'played', chunks_played: totalChunks });
            // LED stays on until next voice message replaces it
        }, textPreview);
        HC.jobCheckpoint(_vjId, { status: 'done' });
    }
}

function formatTime(sec) {
    if (!sec || sec < 0) return '0:00';
    const m = Math.floor(sec / 60);
    const s = Math.floor(sec % 60);
    return m + ':' + (s < 10 ? '0' : '') + s;
}

// Playback owns the strip when active — monitorAudio yields to it
let _playbackStripActive = false;

// Update playback progress on mic-level-strip (dual-purpose: mic level during recording, playback progress during TTS)
function updatePlaybackStrip(pct) {
    const strip = document.getElementById('mic-level-strip');
    const fill = document.getElementById('mic-level-fill');
    if (pct > 0) {
        _playbackStripActive = true;
        if (strip) { strip.className = 'mic-level-strip active playback'; }
        if (fill) fill.style.transform = 'scaleX(' + (Math.min(100, pct) / 100) + ')';
    } else {
        _playbackStripActive = false;
        if (strip) strip.className = (HC.isRecording && HC.isRecording()) ? 'mic-level-strip active' : 'mic-level-strip';
        if (fill) fill.style.width = '0%';
    }
}

// Mini player UI updates
function updateMiniPlayer(active, text, time, opts) {
    var collapse = opts && opts.collapse;
    const el = document.getElementById('mini-player');
    const textEl = document.getElementById('now-playing-text');
    const timeEl = document.getElementById('playback-time');
    if (!el) return;
    if (active) {
        el.classList.add('active');
        if (textEl) textEl.textContent = text;
        if (timeEl) timeEl.textContent = time;
    } else if (collapse) {
        el.classList.remove('active');
    }
    // else: inactive but not collapsed — keep showing last text
    // Sync popup player
    var popupText = document.getElementById('fs-player-text');
    var playerBtn = document.getElementById('fs-player-btn');
    var preview = (typeof gaplessPlayer !== 'undefined' && gaplessPlayer._textPreview) ? gaplessPlayer._textPreview : '';
    var replyCtx = (typeof gaplessPlayer !== 'undefined' && gaplessPlayer._replyingTo) ? gaplessPlayer._replyingTo : '';
    if (popupText) {
        if (active) {
            if (replyCtx) {
                popupText.innerHTML = '<span class="replying-to">\u25B6 ' + replyCtx.replace(/</g, '&lt;').substring(0, 200) + '</span><br>' + (preview || text || 'Playing...');
            } else {
                popupText.textContent = preview || text || 'Playing...';
            }
        }
        else if (collapse) popupText.textContent = '';
        // else: keep existing text
    }
    if (playerBtn) playerBtn.style.opacity = active ? '1' : '';
}
HC.updateMiniPlayer = updateMiniPlayer;


// Expose to HC namespace
// Stop all TTS playback — clears queue, resets drain loop state
function stopTts() {
    _ttsQueue.length = 0;
    _ttsRunning = false;
    gaplessPlayer.cleanup();
}

HC.gaplessPlayer = gaplessPlayer;
HC.enqueueTts = enqueueTts;
HC.stopTts = stopTts;
HC._ttsHistory = _ttsHistory;
HC._ttsQueue = _ttsQueue;
HC._replayHistoryEntry = _replayHistoryEntry;
HC._updateHistoryControls = _updateHistoryControls;
HC._refreshHistoryFromDaemon = _refreshHistoryFromDaemon;
HC.updateMiniPlayer = updateMiniPlayer;
HC.updatePlaybackStrip = updatePlaybackStrip;
