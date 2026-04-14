/* voice-play.js — Babel3 gapless TTS playback (open-source)
 *
 * GaplessStreamPlayer: streams audio chunks from GPU worker
 * via Web Audio API with zero-gap scheduling. Supports HTML
 * <audio> fallback for iOS Chrome compatibility.
 *
 * Depends on: core.js (HC namespace)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

HC._clientId = (typeof crypto !== 'undefined' && crypto.randomUUID ? crypto.randomUUID() : Math.random().toString(36).slice(2) + Math.random().toString(36).slice(2)).slice(0, 8);

HC.reportVoiceEvent = function(msgId, chunk, event) {
    if (!HC.daemonFetch) return;
    HC.daemonFetch('/api/voice-event', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ msg_id: msgId, chunk: chunk, event: event, session_id: HC._clientId }),
    }).catch(function() {});
};

// iOS Chrome breaks Web Audio API AudioBufferSourceNode playback
// (AudioContext reports "running" but no sound reaches speakers).
// HTML <audio> elements work fine — used as default for all platforms.

// Local aliases for HC utilities (avoids touching 100+ call sites)
var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };
var reportVoiceEvent = HC.reportVoiceEvent;
var clientId = HC._clientId;
var daemonFetch = function(path, opts) { return HC.daemonFetch(path, opts); };

// Hooks set by the main IIFE (optional — graceful degradation)
var _userHasInteracted = false;
HC._setUserInteracted = function() { _userHasInteracted = true; };
var spendCredit = function() { if (HC.spendCredit) HC.spendCredit(); };
var updateMiniPlayer = function() { if (HC.updateMiniPlayer) HC.updateMiniPlayer.apply(null, arguments); };

HC.GaplessStreamPlayer = class GaplessStreamPlayer {
    constructor() {
        this._ctx = null;
        this._nextStartTime = 0;
        this._sources = [];
        this._totalScheduled = 0;
        this._totalEnded = 0;
        this._onAllDone = null;
        this._msgId = null;
        this._gainNode = null;
        this._pendingBuffers = [];
        this._playbackStarted = false;
        this._bufferCount = 1;
        this._generation = 0;
        this._chunkMetrics = [];
        this._playbackStartTime = 0;
        this._totalDuration = 0;
        this._chunkTimeline = [];
        this._estimatedTotalChunks = 1;
        this._rafId = null;
        this._chunkBytes = [];  // Raw WAV bytes per chunk, for EC2 upload after playback
        this._paused = false;
        // HTML <audio> playback state
        this._htmlAudioQueue = [];  // [{chunkIndex, duration, url}]
        this._htmlAudioCurrent = null;
        this._htmlAudioStartTime = 0;
        this._htmlAudioEl = null;  // persistent, pre-unlocked <audio> element
    }

    // Get or create the persistent <audio> element for HTML playback.
    // Must be unlocked via a user gesture before it can play.
    _getAudioEl() {
        if (!this._htmlAudioEl) {
            this._htmlAudioEl = new Audio();
            this._htmlAudioEl.setAttribute('playsinline', '');
            log('[GaplessStream-HTML] Created persistent audio element');
        }
        return this._htmlAudioEl;
    }

    // Pre-unlock the HTML audio element from a user gesture context.
    // Plays a tiny silent WAV so iOS allows subsequent play() calls.
    unlockHtmlAudio() {
        const el = this._getAudioEl();
        // Generate 1 sample of silence as WAV
        const buf = new ArrayBuffer(46);
        const v = new DataView(buf);
        const w = (off, s) => { for(let i=0;i<s.length;i++) v.setUint8(off+i, s.charCodeAt(i)); };
        w(0,'RIFF'); v.setUint32(4, 38, true);
        w(8,'WAVEfmt ');
        v.setUint32(16,16,true); v.setUint16(20,1,true); v.setUint16(22,1,true);
        v.setUint32(24,22050,true); v.setUint32(28,44100,true);
        v.setUint16(32,2,true); v.setUint16(34,16,true);
        w(36,'data'); v.setUint32(40, 2, true);
        v.setInt16(44, 0, true);
        const blob = new Blob([buf], {type:'audio/wav'});
        const url = URL.createObjectURL(blob);
        el.src = url;
        el.play().then(() => {
            URL.revokeObjectURL(url);
            log('[GaplessStream-HTML] Audio element unlocked');
        }).catch(e => {
            URL.revokeObjectURL(url);
            log('[GaplessStream-HTML] Unlock failed: ' + e.message);
        });
    }

    ensureContext() {
        // iOS Safari has a third state "interrupted" — the OS reclaimed the audio session.
        // An interrupted context cannot be resumed; we must create a fresh one.
        if (this._ctx && this._ctx.state === 'interrupted') {
            log('[GaplessStream] AudioContext interrupted by iOS — recreating');
            try { this._ctx.close(); } catch (e) {}
            this._mediaElSource = null;  // Must recreate after new context
            this._ctx = null;
        }
        if (!this._ctx || this._ctx.state === 'closed') {
            this._ctx = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: 24000 });
            this._gainNode = this._ctx.createGain();
            this._gainNode.connect(this._ctx.destination);
            this._paused = false;
            this._ctx.onstatechange = () => {
                log('[GaplessStream] AudioContext state changed: ' + this._ctx.state);
                if (this._ctx.state === 'interrupted') {
                    log('[GaplessStream] iOS interrupted audio — will recreate on next playback');
                }
            };
            log('[GaplessStream] Created AudioContext (state=' + this._ctx.state + ')');
        }
        // Don't auto-resume here — let _ensureUnlocked handle it with toast UI.
        // Only resume if user has interacted (mic click etc sets _userHasInteracted).
        if (this._ctx.state === 'suspended' && !this._paused && _userHasInteracted) {
            this._ctx.resume().catch(() => {});
        }
    }

    // Show audio-unlock toast if AudioContext is suspended or interrupted.
    // Returns a promise that resolves once playback is allowed.
    // Idempotent — if already showing or resolved, returns the same promise.
    _ensureUnlocked() {
        // Recreate interrupted context first (iOS-specific state)
        if (this._ctx && this._ctx.state === 'interrupted') {
            this.ensureContext();  // Handles interrupted → fresh context
        }
        const needsUnlock = this._ctx && (this._ctx.state === 'suspended') && !this._paused;
        if (!needsUnlock) {
            this._unlockPromise = null;
            return Promise.resolve();
        }
        if (this._unlockPromise) return this._unlockPromise;
        this._unlockPromise = (async () => {
            // Try resume with a short timeout — on fresh pages, resume() may
            // hang forever (Chrome returns a pending promise until user gesture).
            try {
                await Promise.race([
                    this._ctx.resume(),
                    new Promise(r => setTimeout(r, 200))
                ]);
            } catch (e) {}
            if (this._ctx.state !== 'suspended') { this._unlockPromise = null; return; }
            log('[GaplessStream] AudioContext suspended — showing audio-unlock toast');
            HC.setVoiceStatus('warning', 'Audio blocked — tap anywhere to enable');
            await new Promise((resolve) => {
                // Remove stale toast if any
                const old = document.getElementById('audio-unlock-toast');
                if (old) old.remove();
                const toast = document.createElement('div');
                toast.id = 'audio-unlock-toast';
                toast.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;' +
                    'z-index:99999;cursor:pointer;display:flex;align-items:center;justify-content:center;' +
                    'background:rgba(0,0,0,0.6);backdrop-filter:blur(4px);-webkit-backdrop-filter:blur(4px);';
                const inner = document.createElement('div');
                inner.style.cssText = 'background:rgba(20,20,40,0.95);color:rgba(255,255,255,0.95);' +
                    'border:1px solid rgba(255,255,255,0.2);border-radius:16px;' +
                    'padding:24px 32px;font-size:18px;font-family:system-ui;text-align:center;' +
                    'box-shadow:0 4px 24px rgba(0,0,0,0.5);animation:audioToastPulse 2s ease-in-out infinite;';
                inner.innerHTML = '\uD83D\uDD0A<br><b>Tap to enable audio</b><br>' +
                    '<span style="font-size:13px;opacity:0.7;">Browser requires interaction before playing sound</span>';
                toast.appendChild(inner);
                if (!document.getElementById('audio-toast-style')) {
                    const style = document.createElement('style');
                    style.id = 'audio-toast-style';
                    style.textContent = '@keyframes audioToastPulse{0%,100%{opacity:0.85}50%{opacity:1}}';
                    document.head.appendChild(style);
                }
                const handler = async () => {
                    toast.removeEventListener('click', handler);
                    toast.removeEventListener('touchend', handler);
                    _userHasInteracted = true;
                    try { await this._ctx.resume(); } catch (e) {}
                    this.ensureContext();
                    // Also unlock HTML audio element from this user gesture
                    this.unlockHtmlAudio();
                    log('[GaplessStream] User tapped toast — state=' + this._ctx.state);
                    toast.remove();
                    HC.setVoiceStatus('info', 'Audio enabled');
                    setTimeout(() => HC.setVoiceStatus('', ''), 2000);
                    resolve();
                };
                toast.addEventListener('click', handler);
                toast.addEventListener('touchend', handler);
                document.body.appendChild(toast);
            });
            this._unlockPromise = null;
        })();
        return this._unlockPromise;
    }

    start(msgId) {
        this._generation++;
        const gen = this._generation;
        this._onAllDone = null;
        this.ensureContext();
        // Show unlock toast immediately if audio can't play yet
        if (this._ctx && this._ctx.state === 'suspended') {
            this._ensureUnlocked();
        }
        // Apply user's volume preference (from prefs cookie)
        const volPref = HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100;
        this._gainNode.gain.value = volPref / 100;
        this._nextStartTime = 0;
        this._sources = [];
        this._totalScheduled = 0;
        this._totalEnded = 0;
        this._msgId = msgId;
        this._pendingBuffers = [];
        this._playbackStarted = false;
        this._chunkMetrics = [];
        this._playbackStartTime = 0;
        this._totalDuration = 0;
        this._chunkTimeline = [];
        this._estimatedTotalChunks = 1;
        this._chunkBytes = [];
        // Reset HTML audio state
        if (this._htmlAudioEl) {
            try { this._htmlAudioEl.pause(); this._htmlAudioEl.onended = null; this._htmlAudioEl.onerror = null; } catch(e) {}
        }
        for (const entry of this._htmlAudioQueue) {
            if (entry.url) URL.revokeObjectURL(entry.url);
        }
        this._htmlAudioQueue = [];
        this._htmlAudioCurrent = null;
        this._htmlAudioStartTime = 0;
        if (this._rafId) { cancelAnimationFrame(this._rafId); this._rafId = null; }
        log('[GaplessStream] [msg=' + msgId + '] Started (gen=' + gen + ')');
        updateMiniPlayer(true, 'Generating...', '');
    }

    setEstimatedChunks(n) {
        if (n > 0) this._estimatedTotalChunks = n;
    }

    // ── Mini FFT (Cooley-Tukey radix-2) for spectral flatness ──
    _fft(re, im) {
        const N = re.length;
        // Bit-reversal permutation
        for (let i = 1, j = 0; i < N; i++) {
            let bit = N >> 1;
            for (; j & bit; bit >>= 1) j ^= bit;
            j ^= bit;
            if (i < j) {
                [re[i], re[j]] = [re[j], re[i]];
                [im[i], im[j]] = [im[j], im[i]];
            }
        }
        // FFT butterfly
        for (let len = 2; len <= N; len <<= 1) {
            const halfLen = len >> 1;
            const angle = -2 * Math.PI / len;
            const wRe = Math.cos(angle), wIm = Math.sin(angle);
            for (let i = 0; i < N; i += len) {
                let curRe = 1, curIm = 0;
                for (let j = 0; j < halfLen; j++) {
                    const a = i + j, b = a + halfLen;
                    const tRe = re[b] * curRe - im[b] * curIm;
                    const tIm = re[b] * curIm + im[b] * curRe;
                    re[b] = re[a] - tRe; im[b] = im[a] - tIm;
                    re[a] += tRe; im[a] += tIm;
                    const nextRe = curRe * wRe - curIm * wIm;
                    curIm = curRe * wIm + curIm * wRe;
                    curRe = nextRe;
                }
            }
        }
    }

    // Spectral flatness: geometric_mean / arithmetic_mean of magnitude spectrum
    _spectralFlatness(samples, fftSize) {
        const re = new Float32Array(fftSize);
        const im = new Float32Array(fftSize);
        for (let i = 0; i < samples.length && i < fftSize; i++) re[i] = samples[i];
        this._fft(re, im);
        const halfN = fftSize >> 1;
        let logSum = 0, linSum = 0;
        for (let i = 1; i < halfN; i++) {
            const mag = Math.sqrt(re[i] * re[i] + im[i] * im[i]) + 1e-12;
            logSum += Math.log(mag);
            linSum += mag;
        }
        const n = halfN - 1;
        return Math.exp(logSum / n) / (linSum / n + 1e-12);
    }

    // Estimate pitch (F0) via autocorrelation. Returns Hz or 0 if unvoiced.
    _estimatePitch(samples, sampleRate) {
        const n = samples.length;
        // Search range: 80Hz - 600Hz
        const minLag = Math.floor(sampleRate / 600);
        const maxLag = Math.min(Math.floor(sampleRate / 80), n - 1);
        if (maxLag <= minLag) return 0;
        // Normalized autocorrelation
        let energy = 0;
        for (let i = 0; i < n; i++) energy += samples[i] * samples[i];
        if (energy < 1e-10) return 0;
        let bestLag = 0, bestCorr = 0;
        for (let lag = minLag; lag <= maxLag; lag++) {
            let corr = 0, e1 = 0, e2 = 0;
            for (let i = 0; i < n - lag; i++) {
                corr += samples[i] * samples[i + lag];
                e1 += samples[i] * samples[i];
                e2 += samples[i + lag] * samples[i + lag];
            }
            const norm = Math.sqrt(e1 * e2) + 1e-12;
            const nCorr = corr / norm;
            if (nCorr > bestCorr) { bestCorr = nCorr; bestLag = lag; }
        }
        if (bestCorr < 0.3 || bestLag === 0) return 0; // unvoiced
        return sampleRate / bestLag;
    }

    // NxN matrix inversion via Gauss-Jordan elimination
    // NxN matrix inversion via Gauss-Jordan elimination
    _invertMatrix(m) {
        const n = m.length;
        // Augmented matrix [m | I]
        const aug = [];
        for (let i = 0; i < n; i++) {
            aug[i] = new Float64Array(2 * n);
            for (let j = 0; j < n; j++) aug[i][j] = m[i][j];
            aug[i][n + i] = 1;
        }
        for (let col = 0; col < n; col++) {
            // Partial pivot
            let maxRow = col, maxVal = Math.abs(aug[col][col]);
            for (let r = col + 1; r < n; r++) {
                if (Math.abs(aug[r][col]) > maxVal) { maxVal = Math.abs(aug[r][col]); maxRow = r; }
            }
            if (maxVal < 1e-15) return null;
            if (maxRow !== col) { const tmp = aug[col]; aug[col] = aug[maxRow]; aug[maxRow] = tmp; }
            const pivot = aug[col][col];
            for (let j = 0; j < 2 * n; j++) aug[col][j] /= pivot;
            for (let r = 0; r < n; r++) {
                if (r === col) continue;
                const factor = aug[r][col];
                for (let j = 0; j < 2 * n; j++) aug[r][j] -= factor * aug[col][j];
            }
        }
        const inv = [];
        for (let i = 0; i < n; i++) {
            inv[i] = [];
            for (let j = 0; j < n; j++) inv[i][j] = aug[i][n + j];
        }
        return inv;
    }

    // Mahalanobis distance: sqrt((x-μ)ᵀ Σ⁻¹ (x-μ)) — N-dimensional
    _mahalDist(x, mean, invCov) {
        const n = x.length;
        const d = [];
        for (let i = 0; i < n; i++) d[i] = x[i] - mean[i];
        let val = 0;
        for (let i = 0; i < n; i++)
            for (let j = 0; j < n; j++)
                val += d[i] * invCov[i][j] * d[j];
        return Math.sqrt(Math.max(0, val));
    }

    // Anomaly-detection trim using Mahalanobis distance.
    // The body of the chunk defines "normal" (speech). Tail frames that
    // deviate significantly from the body distribution are artifacts.
    // Mirror GPU worker's _split_sentences for chunk→text mapping
    _splitSentences(text, splitChars, minChars) {
        splitChars = splitChars || '.!?';
        minChars = minChars || 80;
        const escaped = splitChars.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
        const re = new RegExp('(?<=[' + escaped + '])\\s+');
        const parts = text.trim().split(re).filter(p => p.trim());
        const merged = [];
        for (const part of parts) {
            const p = part.trim();
            if (!p) continue;
            if (merged.length && minChars > 0 && merged[merged.length-1].length < minChars) {
                merged[merged.length-1] += ' ' + p;
            } else {
                merged.push(p);
            }
        }
        if (merged.length > 1 && minChars > 0 && merged[merged.length-1].length < minChars) {
            merged[merged.length-2] += ' ' + merged.pop();
        }
        return merged.length ? merged : [text];
    }

    _trimTrailingAudio(audioBuffer, chunkIndex, chunkText) {
        const TRIM_LOG = false; // set true to re-enable trim diagnostics
        const sampleRate = audioBuffer.sampleRate;
        const channelData = audioBuffer.getChannelData(0);
        const frameSize = Math.floor(sampleRate * 0.020); // 20ms frames
        const totalFrames = Math.floor(channelData.length / frameSize);
        const originalMs = (channelData.length / sampleRate * 1000).toFixed(0);
        const textLabel = chunkText ? ' "' + chunkText + '"' : '';
        if (totalFrames < 10) {
            if (TRIM_LOG) log('[Trim] [msg=' + this._msgId + '] Chunk ' + chunkIndex + textLabel + ': too short (' + totalFrames + ' frames) — NO TRIM. ' + originalMs + 'ms');
            return { trimmed: audioBuffer, trimmedMs: 0, originalMs: parseFloat(originalMs) };
        }

        // ── Config ──
        const ANOMALY_THRESHOLD = 2.2;    // Mahalanobis distance (2.2 sigma)
        const SAFETY_MS = 80;             // ms padding after last normal frame
        const MIN_ARTIFACT_MS = 100;      // don't trim less than this
        const VARIANCE_WINDOW = 8;        // frames for rolling energy variance
        const BODY_RATIO = 0.7;           // first 70% of frames = speech reference
        const fftSize = 1024;
        const DIM = 4;                    // feature dimensions

        // ── Forward pass: compute 4 features per frame ──
        // Features: [rmsDb, energyVariance, spectralFlatness, pitch]
        const features = [];
        const rmsValues = [];
        for (let f = 0; f < totalFrames; f++) {
            const offset = f * frameSize;
            const slice = channelData.subarray(offset, offset + frameSize);
            let sumSq = 0;
            for (let i = 0; i < slice.length; i++) sumSq += slice[i] * slice[i];
            const rms = Math.sqrt(sumSq / slice.length);
            const rmsDb = rms > 0 ? 20 * Math.log10(rms) : -100;
            const sf = this._spectralFlatness(slice, fftSize);
            const pitch = this._estimatePitch(slice, sampleRate);
            rmsValues.push(rms);
            features.push({ rmsDb, sf, rms, pitch });
        }

        // Rolling energy variance
        for (let i = 0; i < features.length; i++) {
            const start = Math.max(0, i - VARIANCE_WINDOW + 1);
            let sum = 0, sum2 = 0, n = 0;
            for (let j = start; j <= i; j++) { sum += rmsValues[j]; sum2 += rmsValues[j]*rmsValues[j]; n++; }
            const mean = sum / n;
            features[i].eVar = Math.sqrt(sum2/n - mean*mean);
        }

        // ── Compute pitch p-values (handles bimodal distribution) ──
        // Trimodal pitch model: atom at 0 (unvoiced), atom at >500Hz
        // (artifact), continuous voiced distribution in between.
        // Each atom gets p-value = fraction of body at that atom.
        // Continuous part gets two-sided normal p-value.
        const PITCH_HIGH_ATOM = 500; // Hz threshold for high-pitch atom
        const bodyEnd = Math.floor(totalFrames * BODY_RATIO);
        const bodyPitches = features.slice(0, bodyEnd).map(f => f.pitch);
        const nBody = bodyPitches.length;
        const bodyZeros = bodyPitches.filter(p => p === 0).length;
        const bodyHighs = bodyPitches.filter(p => p >= PITCH_HIGH_ATOM).length;
        const pZero = bodyZeros / nBody;
        const pHigh = bodyHighs / nBody;
        const bodyContinuous = bodyPitches.filter(p => p > 0 && p < PITCH_HIGH_ATOM);
        let pitchMean = 0, pitchStd = 1;
        if (bodyContinuous.length > 1) {
            pitchMean = bodyContinuous.reduce((a, b) => a + b, 0) / bodyContinuous.length;
            const sqDiffs = bodyContinuous.map(p => (p - pitchMean) * (p - pitchMean));
            pitchStd = Math.sqrt(sqDiffs.reduce((a, b) => a + b, 0) / bodyContinuous.length) || 1;
        }
        // Approximate normal CDF via Abramowitz & Stegun (|error| < 1.5e-7)
        function normCdf(x) {
            if (x < -8) return 0; if (x > 8) return 1;
            const t = 1 / (1 + 0.2316419 * Math.abs(x));
            const d = 0.3989422804014327; // 1/sqrt(2π)
            const p = d * Math.exp(-x * x / 2) * t * (0.3193815 + t * (-0.3565638 + t * (1.781478 + t * (-1.8212560 + t * 1.3302744))));
            return x > 0 ? 1 - p : p;
        }
        // Compute p-value for each frame's pitch
        for (let i = 0; i < totalFrames; i++) {
            const p = features[i].pitch;
            if (p === 0) {
                features[i].pitchPval = pZero;
            } else if (p >= PITCH_HIGH_ATOM) {
                features[i].pitchPval = pHigh;
            } else {
                const z = Math.abs(p - pitchMean) / pitchStd;
                features[i].pitchPval = 2 * (1 - normCdf(z)); // two-sided
            }
        }

        // ── Build feature vectors [rmsDb, eVar, sf, log(pitchPval)] ──
        // Log transform: p-values are bounded [0,1] and skewed — bad metric
        // for covariance. log(p) spreads the scale properly. Clamp at -10 to
        // avoid -Infinity for extremely small p-values.
        const vecs = features.map(f => [f.rmsDb, f.eVar, f.sf, Math.max(-10, Math.log(f.pitchPval + 1e-15))]);

        // ── K-means (k=2) on body frames: vowel vs consonant clusters ──
        const bodyVecs = vecs.slice(0, bodyEnd);
        // Initialize centroids: first and frame at 1/3 (diverse start)
        const K = 2;
        const centroids = [bodyVecs[0].slice(), bodyVecs[Math.floor(bodyVecs.length / 3)].slice()];
        let assignments = new Array(bodyVecs.length).fill(0);
        for (let iter = 0; iter < 20; iter++) {
            // Assign each body frame to nearest centroid
            let changed = false;
            for (let n = 0; n < bodyVecs.length; n++) {
                let bestK = 0, bestD = Infinity;
                for (let c = 0; c < K; c++) {
                    let d = 0;
                    for (let i = 0; i < DIM; i++) d += (bodyVecs[n][i] - centroids[c][i]) ** 2;
                    if (d < bestD) { bestD = d; bestK = c; }
                }
                if (assignments[n] !== bestK) { assignments[n] = bestK; changed = true; }
            }
            if (!changed) break;
            // Recompute centroids
            for (let c = 0; c < K; c++) {
                const members = bodyVecs.filter((_, n) => assignments[n] === c);
                if (members.length === 0) continue;
                for (let i = 0; i < DIM; i++) {
                    centroids[c][i] = members.reduce((s, v) => s + v[i], 0) / members.length;
                }
            }
        }
        // Compute per-cluster mean, covariance, inverse
        const clusters = [];
        let clusterOk = true;
        for (let c = 0; c < K; c++) {
            const members = bodyVecs.filter((_, n) => assignments[n] === c);
            if (members.length < DIM + 1) { clusterOk = false; break; }
            const mean = new Array(DIM).fill(0);
            for (const v of members) for (let i = 0; i < DIM; i++) mean[i] += v[i];
            for (let i = 0; i < DIM; i++) mean[i] /= members.length;
            const cov = Array.from({length: DIM}, () => new Array(DIM).fill(0));
            for (const v of members) {
                const d = [];
                for (let i = 0; i < DIM; i++) d[i] = v[i] - mean[i];
                for (let i = 0; i < DIM; i++)
                    for (let j = 0; j < DIM; j++)
                        cov[i][j] += d[i] * d[j];
            }
            for (let i = 0; i < DIM; i++)
                for (let j = 0; j < DIM; j++)
                    cov[i][j] /= members.length;
            for (let i = 0; i < DIM; i++) cov[i][i] += 1e-6;
            const invCov = this._invertMatrix(cov);
            if (!invCov) { clusterOk = false; break; }
            clusters.push({ mean, invCov, size: members.length });
        }
        if (!clusterOk || clusters.length < K) {
            if (TRIM_LOG) log('[Trim] [msg=' + this._msgId + '] Chunk ' + chunkIndex + textLabel + ': cluster covariance singular — NO TRIM. ' + originalMs + 'ms');
            return { trimmed: audioBuffer, trimmedMs: 0, originalMs: parseFloat(originalMs) };
        }
        // ── Backward scan: find last "normal" frame ──
        // Multi-scale majority vote: run 3 window sizes to catch
        // short, medium, and long artifacts. Take the biggest trim.
        const VOTE_WINDOWS = [5, 10, 20];
        let lastNormalFrame = totalFrames - 1;
        const tailLog = [];
        // Compute all tail distances + cluster assignments for logging
        const tailDists = [];
        const tailCluster = [];
        for (let i = bodyEnd; i < totalFrames; i++) {
            let bestD = Infinity, bestC = 0;
            for (let c = 0; c < clusters.length; c++) {
                const d = this._mahalDist(vecs[i], clusters[c].mean, clusters[c].invCov);
                if (d < bestD) { bestD = d; bestC = c; }
            }
            tailDists[i] = bestD;
            tailCluster[i] = bestC;
        }
        // Build per-frame anomaly flags
        const isAnom = [];
        for (let i = bodyEnd; i < totalFrames; i++) {
            isAnom[i] = tailDists[i] > ANOMALY_THRESHOLD;
        }
        // Run backward scan at each scale, keep the deepest trim
        let bestTrimFrame = totalFrames - 1;
        let bestWindow = 0;
        for (const W of VOTE_WINDOWS) {
            let found = false;
            for (let i = totalFrames - 1; i >= bodyEnd; i--) {
                if (i - W + 1 < bodyEnd) break;
                let normalCount = 0;
                for (let j = 0; j < W; j++) {
                    if (!isAnom[i - j]) normalCount++;
                }
                if (normalCount > W / 2) {
                    const trimAt = i - Math.floor(W / 2);
                    if (trimAt < bestTrimFrame) {
                        bestTrimFrame = trimAt;
                        bestWindow = W;
                    }
                    found = true;
                    break;
                }
            }
            if (!found && bodyEnd < bestTrimFrame) {
                bestTrimFrame = bodyEnd;
                bestWindow = W;
            }
        }
        lastNormalFrame = bestTrimFrame;

        // Log all tail frames with trim point marked
        if (TRIM_LOG) for (let i = totalFrames - 1; i >= bodyEnd; i--) {
            const f = features[i];
            const marker = (i === lastNormalFrame) ? ' <<< TRIM' : '';
            tailLog.push('f' + i + ' c' + tailCluster[i] + ' σ=' + tailDists[i].toFixed(1) + (isAnom[i] ? '!' : '') + ' [' + f.rmsDb.toFixed(0) + 'dB v' + f.eVar.toFixed(4) + ' sf' + f.sf.toFixed(2) + ' p' + f.pitch.toFixed(0) + 'Hz(pv=' + f.pitchPval.toFixed(2) + ')]' + marker);
        }

        // ── Compute trim point ──
        const safetySamples = Math.floor(sampleRate * SAFETY_MS / 1000);
        const trimSample = Math.min((lastNormalFrame + 1) * frameSize + safetySamples, channelData.length);
        const artifactMs = ((channelData.length - trimSample) / sampleRate * 1000);

        if (artifactMs < MIN_ARTIFACT_MS) {
            const clSizes2 = clusters.map((c, i) => 'c' + i + '=' + c.size).join(' ');
            if (TRIM_LOG) log('[Trim] [msg=' + this._msgId + '] Chunk ' + chunkIndex + textLabel + ': NO TRIM (' + artifactMs.toFixed(0) + 'ms < ' + MIN_ARTIFACT_MS + 'ms min, best vote=' + bestWindow + ' of [' + VOTE_WINDOWS + '], ' + clSizes2 + '). ' + originalMs + 'ms. Tail (newest→oldest):\n' + tailLog.join('\n'));
            return { trimmed: audioBuffer, trimmedMs: 0, originalMs: parseFloat(originalMs) };
        }

        // ── Fade-out instead of hard trim ──
        // Apply a cosine gain ramp from the trim point, fading to zero.
        // This makes over-trims inaudible — even if we catch the tail of
        // a word, it decays naturally instead of chopping.
        const FADE_MS = 150;  // max fade duration (ms)
        const fadeSamples = Math.min(
            Math.floor(sampleRate * FADE_MS / 1000),
            channelData.length - trimSample  // don't fade past end
        );
        const fadeEndSample = trimSample + fadeSamples;

        const trimmedBuffer = this._ctx.createBuffer(1, fadeEndSample, sampleRate);
        const trimmedData = trimmedBuffer.getChannelData(0);
        trimmedData.set(channelData.subarray(0, trimSample));
        // Cosine fade: 1.0 → 0.0 over fadeSamples
        for (let i = 0; i < fadeSamples; i++) {
            const gain = 0.5 * (1 + Math.cos(Math.PI * i / fadeSamples));
            trimmedData[trimSample + i] = channelData[trimSample + i] * gain;
        }
        const trimmedMs = artifactMs.toFixed(0);
        const fadeMs = (fadeSamples / sampleRate * 1000).toFixed(0);
        const newMs = (fadeEndSample / sampleRate * 1000).toFixed(0);
        const clSizes = clusters.map((c, i) => 'c' + i + '=' + c.size).join(' ');
        if (TRIM_LOG) log('[Trim] [msg=' + this._msgId + '] Chunk ' + chunkIndex + textLabel + ': TRIMMED ' + trimmedMs + 'ms (fade=' + fadeMs + 'ms, threshold=' + ANOMALY_THRESHOLD + 'σ, vote=' + bestWindow + ' of [' + VOTE_WINDOWS + '], ' + clSizes + '). ' + originalMs + 'ms → ' + newMs + 'ms. Tail (newest→oldest):\n' + tailLog.join('\n'));

        // ── Rebuild WAV ──
        const int16 = new Int16Array(trimmedData.length);
        for (let i = 0; i < trimmedData.length; i++) {
            int16[i] = Math.max(-32768, Math.min(32767, Math.round(trimmedData[i] * 32767)));
        }
        const wavHeader = new ArrayBuffer(44);
        const view = new DataView(wavHeader);
        const dataSize = int16.length * 2;
        view.setUint32(0, 0x52494646, false); // "RIFF"
        view.setUint32(4, 36 + dataSize, true);
        view.setUint32(8, 0x57415645, false); // "WAVE"
        view.setUint32(12, 0x666d7420, false); // "fmt "
        view.setUint32(16, 16, true);
        view.setUint16(20, 1, true); // PCM
        view.setUint16(22, 1, true); // mono
        view.setUint32(24, sampleRate, true);
        view.setUint32(28, sampleRate * 2, true);
        view.setUint16(32, 2, true);
        view.setUint16(34, 16, true);
        view.setUint32(36, 0x64617461, false); // "data"
        view.setUint32(40, dataSize, true);
        const wavBlob = new Blob([wavHeader, int16.buffer], { type: 'audio/wav' });

        return { trimmed: trimmedBuffer, trimmedMs: parseFloat(trimmedMs), originalMs: parseFloat(originalMs), wavBlob };
    }

    async scheduleChunk(wavBytes, chunkIndex) {
        if (!this._ctx) return;
        // Keep a copy of raw bytes for EC2 upload after playback
        this._chunkBytes[chunkIndex] = wavBytes.slice(0);
        try {
            const audioBuffer = await this._ctx.decodeAudioData(wavBytes.slice(0));

            // Browser-side trailing silence trim (with logging)
            const chunkText = this._chunkTexts ? this._chunkTexts[chunkIndex] || '' : '';
            const trimResult = this._trimTrailingAudio(audioBuffer, chunkIndex, chunkText);
            const finalBuffer = trimResult.trimmed;
            // If trimmed, update the stored WAV bytes for upload
            if (trimResult.wavBlob) {
                this._chunkBytes[chunkIndex] = await trimResult.wavBlob.arrayBuffer();
            }

            if (!this._playbackStarted) {
                this._pendingBuffers.push({ audioBuffer: finalBuffer, chunkIndex });
                log('[GaplessStream] [msg=' + this._msgId + '] Buffered chunk ' + chunkIndex + ' (' + this._pendingBuffers.length + '/' + this._bufferCount + ')');
                if (this._pendingBuffers.length >= this._bufferCount) await this._flushBuffer();
            } else {
                this._scheduleBuffer(finalBuffer, chunkIndex);
            }
        } catch (e) {
            log('[GaplessStream] [msg=' + this._msgId + '] Decode error chunk ' + chunkIndex + ': ' + e.message);
            _handleTtsLost(this._msgId, '(chunk ' + chunkIndex + ')', 'Audio decode failed: ' + e.message, 'decode_error');
        }
    }

    async _flushBuffer() {
        if (this._playbackStarted) return;
        this._playbackStarted = true;

        // iOS Safari/Chrome: await AudioContext resume before scheduling.
        // _ensureUnlocked() is idempotent — toast may already be showing from start().
        await this._ensureUnlocked();
        log('[GaplessStream] [msg=' + this._msgId + '] AudioContext state after unlock: ' + this._ctx.state);

        // VAD Gate: hold playback while user is speaking
        if (HC._VAD_GATE_ENABLED && HC.isRecording()) {
            const isUserSpeaking = () => {
                if (isSpeaking) return true;
                const micActive = currentMicLevel > SPEECH_THRESHOLD;
                const recentTranscription = (Date.now() - lastTranscriptionTime) < VAD_GATE_SILENCE_WINDOW;
                return micActive || recentTranscription;
            };
            if (isUserSpeaking()) {
                const startWait = Date.now();
                log('[GaplessStream] [msg=' + this._msgId + '] VAD gate: user speaking, holding playback...');
                while (isUserSpeaking()) {
                    if (Date.now() - startWait >= VAD_GATE_MAX_DELAY) {
                        log('[GaplessStream] [msg=' + this._msgId + '] VAD gate: max delay, playing anyway');
                        break;
                    }
                    await new Promise(r => setTimeout(r, 300));
                }
                log('[GaplessStream] [msg=' + this._msgId + '] VAD gate: silence after ' + (Date.now() - startWait) + 'ms');
            }
        }

        this._nextStartTime = this._ctx ? this._ctx.currentTime + 0.02 : 0;
        this._playbackStartTime = this._nextStartTime;
        this._htmlAudioStartTime = performance.now() / 1000;
        log('[GaplessStream] [msg=' + this._msgId + '] Flushing ' + this._pendingBuffers.length + ' chunks (ctxState=' + (this._ctx ? this._ctx.state : 'N/A') + ')');
        for (const { audioBuffer, chunkIndex } of this._pendingBuffers) {
            this._scheduleBuffer(audioBuffer, chunkIndex);
        }
        this._pendingBuffers = [];
    }

    _scheduleBuffer(audioBuffer, chunkIndex) {
        // Always use HTML <audio> — more reliable across platforms.
        // Web Audio AudioBufferSourceNode is silently broken on iOS Chrome
        // (reports "running" but no sound). HTML <audio> works everywhere.
        this._scheduleBufferHtml(audioBuffer, chunkIndex);
        return;

        // Legacy Web Audio path (kept for reference, not reached)
        const source = this._ctx.createBufferSource();
        source.buffer = audioBuffer;
        source.connect(this._gainNode);
        const startAt = this._nextStartTime;
        const headroom = startAt - this._ctx.currentTime;
        const actualStart = headroom < -0.01 ? this._ctx.currentTime : startAt;
        source.start(actualStart);
        this._nextStartTime = actualStart + audioBuffer.duration;
        this._totalDuration = this._nextStartTime - this._playbackStartTime;
        this._totalScheduled++;
        this._chunkTimeline.push({ startAt: actualStart, duration: audioBuffer.duration });
        // Start rAF time updater on first scheduled chunk
        if (!this._rafId) this._startTimeUpdater();
        const gapSec = headroom < -0.01 ? -headroom : 0;
        this._chunkMetrics.push({
            chunk_index: chunkIndex,
            duration_sec: parseFloat(audioBuffer.duration.toFixed(2)),
            headroom_sec: parseFloat(headroom.toFixed(3)),
            gap_sec: parseFloat(gapSec.toFixed(3)),
        });
        const gen = this._generation;
        source.onended = () => {
            if (gen !== this._generation) return;
            this._totalEnded++;
            reportVoiceEvent(this._msgId, chunkIndex, 'finished');
            if (this._totalEnded >= this._totalScheduled && this._onAllDone) {
                const cb = this._onAllDone; this._onAllDone = null; cb();
            }
        };
        this._sources.push(source);
        reportVoiceEvent(this._msgId, chunkIndex, 'started');
        log('[GaplessStream] [msg=' + this._msgId + '] Chunk ' + chunkIndex + ' scheduled dur=' + audioBuffer.duration.toFixed(2) + 's headroom=' + headroom.toFixed(3) + 's');
    }

    // iOS fallback: play WAV chunks sequentially via HTML <audio> elements.
    // AudioBufferSourceNode is silently broken on iOS Chrome (context reports
    // "running" but no sound). HTML <audio> works reliably.
    _scheduleBufferHtml(audioBuffer, chunkIndex) {
        // Build a WAV blob from the raw bytes we saved earlier
        const wavBytes = this._chunkBytes[chunkIndex];
        if (!wavBytes) {
            log('[GaplessStream-HTML] [msg=' + this._msgId + '] No WAV bytes for chunk ' + chunkIndex);
            return;
        }
        const blob = new Blob([wavBytes], { type: 'audio/wav' });
        const url = URL.createObjectURL(blob);

        const dur = audioBuffer.duration;
        this._totalScheduled++;
        this._totalDuration += dur;
        const now = performance.now() / 1000;
        this._chunkTimeline.push({ startAt: now, duration: dur });
        this._chunkMetrics.push({
            chunk_index: chunkIndex,
            duration_sec: parseFloat(dur.toFixed(2)),
            headroom_sec: 0,
            gap_sec: 0,
        });

        if (!this._rafId) this._startTimeUpdaterHtml();

        const entry = { chunkIndex, duration: dur, url };
        this._htmlAudioQueue.push(entry);

        reportVoiceEvent(this._msgId, chunkIndex, 'started');
        log('[GaplessStream-HTML] [msg=' + this._msgId + '] Chunk ' + chunkIndex + ' queued dur=' + dur.toFixed(2) + 's');

        // If nothing is currently playing, start immediately
        if (!this._htmlAudioCurrent) {
            this._playNextHtmlAudio();
        }
    }

    _playNextHtmlAudio() {
        if (this._htmlAudioQueue.length === 0) return;
        const entry = this._htmlAudioQueue.shift();
        this._htmlAudioCurrent = entry;
        this._htmlAudioStartTime = performance.now() / 1000;
        const gen = this._generation;
        const msgId = this._msgId;

        // Native audio path: play through AVAudioEngine for echo cancellation
        if (window.NativeBridge && window.NativeBridge.capabilities &&
            window.NativeBridge.capabilities.indexOf('nativeAudio') !== -1 &&
            this._chunkBytes && this._chunkBytes[entry.chunkIndex]) {
            // Convert raw WAV bytes to base64 for the native bridge
            var wavBytes = this._chunkBytes[entry.chunkIndex];
            var binary = '';
            var bytes = new Uint8Array(wavBytes);
            for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
            var b64 = btoa(binary);

            log('[GaplessStream-HTML] [msg=' + msgId + '] Playing chunk ' + entry.chunkIndex + ' (native audio)');

            window.NativeBridge.playAudio(b64, () => {
                URL.revokeObjectURL(entry.url);
                if (gen !== this._generation) return;
                this._totalEnded++;
                reportVoiceEvent(msgId, entry.chunkIndex, 'finished');
                this._htmlAudioCurrent = null;
                this._playNextHtmlAudio();
                if (this._totalEnded >= this._totalScheduled && this._onAllDone) {
                    const cb = this._onAllDone; this._onAllDone = null; cb();
                }
            });
            return;
        }

        // Web audio path: HTML <audio> element
        const el = this._getAudioEl();

        // Route through Web Audio gain node for per-page volume control
        if (this._ctx && this._gainNode && !this._mediaElSource) {
            try {
                this._mediaElSource = this._ctx.createMediaElementSource(el);
                this._mediaElSource.connect(this._gainNode);
                log('[GaplessStream-HTML] [msg=' + msgId + '] Routed audio element through gain node');
            } catch (e) {
                log('[GaplessStream-HTML] [msg=' + msgId + '] MediaElementSource failed: ' + e.message);
            }
        }
        const volPref = HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100;
        if (this._gainNode) this._gainNode.gain.value = volPref / 100;

        el.onended = null;
        el.onerror = null;

        el.onended = () => {
            URL.revokeObjectURL(entry.url);
            if (gen !== this._generation) return;
            this._totalEnded++;
            reportVoiceEvent(msgId, entry.chunkIndex, 'finished');
            this._htmlAudioCurrent = null;
            this._playNextHtmlAudio();
            if (this._totalEnded >= this._totalScheduled && this._onAllDone) {
                const cb = this._onAllDone; this._onAllDone = null; cb();
            }
        };
        el.onerror = () => {
            URL.revokeObjectURL(entry.url);
            log('[GaplessStream-HTML] [msg=' + msgId + '] Error playing chunk ' + entry.chunkIndex);
            if (gen !== this._generation) return;
            this._totalEnded++;
            this._htmlAudioCurrent = null;
            this._playNextHtmlAudio();
            if (this._totalEnded >= this._totalScheduled && this._onAllDone) {
                const cb = this._onAllDone; this._onAllDone = null; cb();
            }
        };

        el.src = entry.url;
        el.play().catch(e => {
            log('[GaplessStream-HTML] [msg=' + msgId + '] Play error: ' + e.message);
            if (el.onended) el.onended();
        });
        log('[GaplessStream-HTML] [msg=' + msgId + '] Playing chunk ' + entry.chunkIndex);
    }

    _startTimeUpdaterHtml() {
        const gen = this._generation;
        const tick = () => {
            if (gen !== this._generation) return;
            const current = this._htmlAudioCurrent;
            const el = this._htmlAudioEl;
            const timeline = this._chunkTimeline;
            const estTotal = Math.max(this._estimatedTotalChunks, timeline.length);
            let chunkIdx = this._totalEnded;
            let fraction = 0;
            if (current && el && !isNaN(el.currentTime) && !isNaN(el.duration) && el.duration > 0) {
                fraction = Math.min(1, Math.max(0, el.currentTime / el.duration));
            }
            const seekPct = Math.min(100, ((chunkIdx + fraction) / estTotal) * 100);
            const elapsed = (current && el && !isNaN(el.currentTime)) ? el.currentTime : 0;
            let totalElapsed = 0;
            for (let i = 0; i < this._totalEnded && i < timeline.length; i++) {
                totalElapsed += timeline[i].duration;
            }
            totalElapsed += elapsed;
            updateMiniPlayer(true, 'Playing ' + (chunkIdx + 1) + '/' + estTotal, formatTime(totalElapsed));
            updatePlaybackStrip(seekPct);
            this._rafId = requestAnimationFrame(tick);
        };
        this._rafId = requestAnimationFrame(tick);
    }

    _startTimeUpdater() {
        const gen = this._generation;
        const tick = () => {
            if (gen !== this._generation || !this._ctx) return;
            const now = this._ctx.currentTime;
            const elapsed = Math.max(0, now - this._playbackStartTime);
            const timeline = this._chunkTimeline;
            const estTotal = Math.max(this._estimatedTotalChunks, timeline.length);
            // Find which chunk is currently playing and fraction within it
            let chunkIdx = timeline.length - 1;
            let fraction = 1;
            for (let i = 0; i < timeline.length; i++) {
                const c = timeline[i];
                if (now < c.startAt + c.duration) {
                    chunkIdx = i;
                    fraction = c.duration > 0 ? (now - c.startAt) / c.duration : 0;
                    break;
                }
            }
            // Seek position: segment-based — each chunk owns 1/estTotal of the bar
            const seekPct = Math.min(100, ((chunkIdx + Math.max(0, fraction)) / estTotal) * 100);
            // Estimate total duration from average chunk duration
            const avgDur = timeline.length > 0
                ? timeline.reduce((s, c) => s + c.duration, 0) / timeline.length : 0;
            const estDuration = avgDur * estTotal;
            // Update mini player + progress strip
            updateMiniPlayer(true, 'Playing ' + (chunkIdx + 1) + '/' + estTotal, formatTime(elapsed));
            updatePlaybackStrip(seekPct);
            this._rafId = requestAnimationFrame(tick);
        };
        this._rafId = requestAnimationFrame(tick);
    }

    cleanup() {
        // Cancel onAllDone FIRST — prevents aborted audio from triggering auto-advance
        this._onAllDone = null;
        if (this._rafId) { cancelAnimationFrame(this._rafId); this._rafId = null; }
        // Stop Web Audio sources
        for (const src of this._sources) {
            try { src.stop(); } catch (e) {}
        }
        this._sources = [];
        // Stop HTML audio
        if (this._htmlAudioEl) {
            try { this._htmlAudioEl.pause(); this._htmlAudioEl.onended = null; this._htmlAudioEl.onerror = null; } catch(e) {}
        }
        for (const entry of this._htmlAudioQueue) {
            if (entry.url) URL.revokeObjectURL(entry.url);
        }
        this._htmlAudioQueue = [];
        this._htmlAudioCurrent = null;
        this._pendingBuffers = [];
        this._paused = false;
        if (this._ctx && this._ctx.state === 'suspended') {
            this._ctx.resume().catch(() => {});
        }
        if (this._onAllDone) { const cb = this._onAllDone; this._onAllDone = null; cb(); }
    }

    pause() {
        // HTML audio pause
        if (this._htmlAudioCurrent && this._htmlAudioEl) {
            this._htmlAudioEl.pause();
            this._paused = true;
            log('[GaplessStream] [msg=' + this._msgId + '] Paused');
            return true;
        }
        if (this._ctx && this._ctx.state === 'running') {
            this._paused = true;
            this._ctx.suspend();
            log('[GaplessStream] [msg=' + this._msgId + '] Paused');
            return true;
        }
        return false;
    }

    resume() {
        // HTML audio resume
        if (this._htmlAudioCurrent && this._htmlAudioEl && this._paused) {
            this._paused = false;
            this._htmlAudioEl.play().catch(() => {});
            log('[GaplessStream] [msg=' + this._msgId + '] Resumed');
            return true;
        }
        if (this._ctx && this._paused) {
            this._paused = false;
            this._ctx.resume();
            log('[GaplessStream] [msg=' + this._msgId + '] Resumed');
            return true;
        }
        return false;
    }

    isPaused() {
        return !!this._paused;
    }

    async flushRemaining() {
        if (!this._playbackStarted && this._pendingBuffers.length > 0) {
            log('[GaplessStream] [msg=' + this._msgId + '] Flushing remaining ' + this._pendingBuffers.length + ' chunks');
            await this._flushBuffer();
        }
    }

    // Resample mono 16-bit PCM bytes from srcRate to 24000Hz using linear interpolation.
    // For exact 2:1 downsampling (48kHz→24kHz) this keeps every other sample.
    _resamplePcm(pcmBuffer, srcRate) {
        const TARGET = 24000;
        if (srcRate === TARGET) return pcmBuffer;
        const src = new Int16Array(pcmBuffer);
        const dstLen = Math.round(src.length * TARGET / srcRate);
        const dst = new Int16Array(dstLen);
        for (var i = 0; i < dstLen; i++) {
            var srcPos = i * srcRate / TARGET;
            var s0 = Math.floor(srcPos);
            var frac = srcPos - s0;
            var a = s0 < src.length ? src[s0] : 0;
            var b = (s0 + 1) < src.length ? src[s0 + 1] : a;
            dst[i] = Math.round(a + frac * (b - a));
        }
        return dst.buffer;
    }

    // Stitch WAV chunks into a single 24kHz buffer.
    // Reads each chunk's declared sample rate from WAV header bytes 24-27.
    // Chunks at a different rate are resampled to 24kHz before concatenation,
    // handling mixed-rate output from dual-backend synthesis (local GPU vs RunPod).
    _stitchWavChunks(chunkBytes) {
        const TARGET_RATE = 24000;
        const HEADER_SIZE = 44;
        const ordered = (chunkBytes || this._chunkBytes).filter(Boolean);
        if (ordered.length === 0) return null;

        // Resample and collect PCM from each chunk.
        var pcmParts = [];
        for (var i = 0; i < ordered.length; i++) {
            var buf = ordered[i];
            if (buf.byteLength <= HEADER_SIZE) continue;
            var dv = new DataView(buf);
            var srcRate = dv.getUint32(24, true); // bytes 24-27: SampleRate
            var rawPcm = buf.slice(HEADER_SIZE);
            pcmParts.push(srcRate !== TARGET_RATE ? this._resamplePcm(rawPcm, srcRate) : rawPcm);
        }

        if (pcmParts.length === 0) return null;
        if (pcmParts.length === 1 && ordered.length === 1) {
            // Single chunk already at target rate — return as-is.
            if (new DataView(ordered[0]).getUint32(24, true) === TARGET_RATE) return ordered[0];
        }

        var totalPcm = pcmParts.reduce(function(s, p) { return s + p.byteLength; }, 0);

        // Build combined WAV: first chunk's header (patched to 24kHz) + all PCM.
        var combined = new ArrayBuffer(HEADER_SIZE + totalPcm);
        var view = new Uint8Array(combined);
        view.set(new Uint8Array(ordered[0].slice(0, HEADER_SIZE)), 0);

        // Patch header to declare TARGET_RATE.
        var hdv = new DataView(combined);
        hdv.setUint32(24, TARGET_RATE, true);               // SampleRate
        hdv.setUint32(28, TARGET_RATE * 2, true);           // ByteRate = 24000 * 1ch * 2B
        hdv.setUint32(4, 36 + totalPcm, true);              // RIFF chunk size
        hdv.setUint32(40, totalPcm, true);                   // data sub-chunk size

        var offset = HEADER_SIZE;
        for (var j = 0; j < pcmParts.length; j++) {
            view.set(new Uint8Array(pcmParts[j]), offset);
            offset += pcmParts[j].byteLength;
        }
        return combined;
    }

    // Store stitched audio as browser-local Blob URL for replay,
    // AND upload to daemon archive for persistence across refreshes.
    // chunkBytes is passed explicitly (not read from this._chunkBytes) to avoid
    // race conditions when a new message starts.
    async _uploadAndRecord(msgId, textPreview, chunkBytes, skipUpload, voiceJobId) {
        if (!msgId || !chunkBytes || chunkBytes.length === 0) return;
        // Skip history recording for replays — they shouldn't re-add themselves
        if (skipUpload) return;
        const stitched = this._stitchWavChunks(chunkBytes);
        if (stitched) {
            const blob = new Blob([stitched], { type: 'audio/wav' });
            const audioUrl = URL.createObjectURL(blob);
            log('[GaplessStream] [msg=' + msgId + '] Stored TTS audio locally (' + stitched.byteLength + 'B)');
            // Add to local history (daemon archive is source of truth, but keep
            // local entry for immediate replay before next daemon sync)
            var isDup = false;
            for (var k = 0; k < HC._ttsHistory.length; k++) {
                if (HC._ttsHistory[k].msgId === msgId) { isDup = true; break; }
            }
            if (!isDup) {
                HC._ttsHistory.push({ msgId, textPreview: textPreview || '', emotion: this._emotion || '', audioUrl });
            }
            HC._updateHistoryControls();

            // Upload to daemon archive for persistence.
            // Send JSON with base64-encoded audio instead of FormData — daemonFetch
            // calls String(body) on the request body, which would stringify FormData
            // to "[object FormData]" on the EC2 proxy path and cause a 400.
            try {
                log('[GaplessStream] [msg=' + msgId + '] Uploading to archive (' + stitched.byteLength + 'B)');
                var self = this;
                new Promise(function(resolve, reject) {
                    var reader = new FileReader();
                    reader.onload = function() { resolve(reader.result.split(',')[1]); };
                    reader.onerror = reject;
                    reader.readAsDataURL(blob);
                }).then(function(audioB64) {
                    return daemonFetch('/api/tts-upload', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({
                            audio_b64: audioB64,
                            msg_id: msgId,
                            text: textPreview || '',
                            original_text: self._originalText || textPreview || '',
                            voice: self._voice || '',
                            emotion: self._emotion || '',
                            replying_to: self._replyingTo || '',
                            duration_sec: stitched.byteLength / 48000,
                            voice_job_id: voiceJobId || '',
                        }),
                    });
                }).then(function(r) {
                    log('[GaplessStream] [msg=' + msgId + '] Archive upload: HTTP ' + r.status);
                }).catch(function(e) {
                    log('[GaplessStream] [msg=' + msgId + '] Archive upload failed: ' + e.message);
                });
            } catch(e) {
                log('[GaplessStream] [msg=' + msgId + '] Archive upload error: ' + e.message);
            }
        }
    }

    async finalize(totalChunks, callback, textPreview, skipUpload) {
        await this.flushRemaining();
        const savedMsgId = this._msgId;
        // Snapshot chunk bytes now — passed directly to avoid touching instance
        // state during the async upload (fixes race with next message start).
        const savedChunkBytes = this._chunkBytes.slice();
        // Return a promise that resolves when all audio finishes playing.
        // This is critical for the TTS queue — without it, the next message
        // starts while the current one is still audible.
        return new Promise((resolve) => {
            this._onAllDone = () => {
                const chunksPlayed = this._totalEnded;
                const chunksScheduled = this._totalScheduled;
                log('[GaplessStream] [msg=' + savedMsgId + '] All ' + totalChunks + ' chunks finished');
                // Report playback complete to daemon (always POST, injection gated daemon-side)
                daemonFetch('/api/voice-report', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        msg_id: savedMsgId,
                        status: 'complete',
                        chunks_scheduled: chunksScheduled,
                        chunks_played: chunksPlayed,
                        session_id: clientId,
                        volume: HC._prefs && HC._prefs.volume !== undefined ? HC._prefs.volume : 100,
                        inject: !!HC._voiceReportEnabled,
                    }),
                }).catch(() => {});
                this.cleanup();
                updateMiniPlayer(false, '', '');
                updatePlaybackStrip(0);
                // Auto-advance: if navigating history forward, play next entry.
                // Only auto-advance when user pressed next (not prev).
                if (HC._autoAdvanceHistory && HC._prevCursor >= 0 && HC._prevCursor < (HC._ttsHistory || []).length - 1) {
                    HC._prevCursor++;
                    var nextEntry = HC._ttsHistory[HC._prevCursor];
                    if (nextEntry && HC._replayHistoryEntry) {
                        HC._replayHistoryEntry(nextEntry);
                    }
                }
                if (callback) callback();
                // Upload uses the snapshot — never touches this._chunkBytes again
                this._uploadAndRecord(savedMsgId, textPreview, savedChunkBytes, !!skipUpload, this._voiceJobId);
                spendCredit(); // instant local decrement + background server sync
                resolve();
            };
            if (this._totalEnded >= this._totalScheduled && this._totalScheduled > 0) {
                const cb = this._onAllDone; this._onAllDone = null; cb();
            }
        });
    }

    isActive() { return this._ctx !== null && this._ctx.state !== 'closed'; }

    getMetrics() {
        if (!this._chunkMetrics.length) return null;
        const totalDuration = this._chunkMetrics.reduce((s, m) => s + m.duration_sec, 0);
        const totalGaps = this._chunkMetrics.reduce((s, m) => s + m.gap_sec, 0);
        const gapCount = this._chunkMetrics.filter(m => m.gap_sec > 0.01).length;
        return { total_duration_sec: parseFloat(totalDuration.toFixed(2)), total_gaps_sec: parseFloat(totalGaps.toFixed(2)), gap_count: gapCount };
    }
}
