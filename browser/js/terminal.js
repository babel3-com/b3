/* terminal.js — Babel3 terminal setup and UI controls (open-source)
 *
 * xterm.js initialization, keyboard input, text input, copy/paste popup,
 * click-to-cursor, touch scroll with momentum, virtual keyboard resize,
 * volume/font controls, terminal key strip, file upload.
 *
 * Depends on: core.js (HC namespace), xterm.js + FitAddon (loaded via CDN)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// ── Terminal Setup ──
var _savedFontSize = 13;
try { var _fs = HC._prefs && HC._prefs.font_size > 0 ? HC._prefs.font_size : 0; if (_fs > 0) _savedFontSize = _fs; } catch(e) {}
const term = new Terminal({
    theme: {
        background: '#0d1117',
        foreground: '#c9d1d9',
        cursor: '#58a6ff',
        selectionBackground: 'rgba(88, 166, 255, 0.3)',
    },
    fontSize: _savedFontSize,
    fontFamily: "'SF Mono', Monaco, 'Cascadia Code', 'Courier New', monospace",
    cursorBlink: true,
    scrollback: 5000,
});

var _fitAddon;
const fitAddon = new FitAddon.FitAddon();
_fitAddon = fitAddon;
term.loadAddon(fitAddon);
// Make single-line URLs clickable via WebLinksAddon (works for short URLs).
if (typeof WebLinksAddon !== 'undefined') {
    term.loadAddon(new WebLinksAddon.WebLinksAddon());
}
term.open(document.getElementById('xterm-wrap'));
window._term = term;  // expose for debugging/testing

// ── URL overlay ──
// URLs that wrap across terminal rows can't be clicked or copy-pasted cleanly
// (iOS Safari inserts %0A at line wraps). Scan the buffer for URLs spanning
// wrapped rows and overlay a floating "Open" button next to each one.
// Overlay is on #xterm-wrap (not inside .xterm) to avoid breaking xterm layout.
(function() {
    var wrap = document.getElementById('xterm-wrap');
    var urlOverlay = document.createElement('div');
    urlOverlay.id = 'url-overlay-container';
    urlOverlay.style.cssText = 'position:absolute;top:0;left:0;right:0;bottom:0;pointer-events:none;z-index:50;overflow:hidden;';
    wrap.style.position = 'relative';
    wrap.appendChild(urlOverlay);

    function detectUrls() {
        var buf = term.buffer.active;
        var cols = term.cols;
        var viewportY = buf.viewportY;
        var rows = term.rows;
        urlOverlay.innerHTML = '';

        // Read all visible rows
        var rawRows = [];
        for (var i = 0; i < rows; i++) {
            var line = buf.getLine(viewportY + i);
            if (!line) { rawRows.push({text: '', trimmed: '', wrapped: false}); continue; }
            var text = '';
            for (var j = 0; j < cols; j++) {
                var cell = line.getCell(j);
                text += cell ? cell.getChars() || ' ' : ' ';
            }
            var trimmed = text.replace(/\s+$/, '');
            rawRows.push({text: text, trimmed: trimmed, wrapped: line.isWrapped});
        }

        // Join lines into segments. A line continues the previous segment if:
        // (a) isWrapped is true, OR
        // (b) previous line was full (trimmed length >= cols-1) AND this line
        //     starts with a non-space URL-like char (letter, digit, %, &, =, /, etc.)
        //     This handles programs that explicitly write \n at column boundaries.
        var segments = [];
        var currentText = '';
        var currentStartRow = 0;
        for (var i = 0; i < rawRows.length; i++) {
            var r = rawRows[i];
            var shouldJoin = r.wrapped;
            if (!shouldJoin && i > 0 && currentText.length > 0) {
                var prevTrimLen = rawRows[i - 1].trimmed.length;
                var firstChar = r.trimmed.charAt(0);
                // Previous line full AND this line starts with URL-continuation char
                if (prevTrimLen >= cols - 1 && firstChar && firstChar !== ' ' && /[a-zA-Z0-9%&=\/?._\-+~#]/.test(firstChar)) {
                    shouldJoin = true;
                }
            }
            if (shouldJoin) {
                currentText += r.trimmed;
            } else {
                if (currentText) segments.push({text: currentText, startRow: currentStartRow});
                currentText = r.trimmed;
                currentStartRow = i;
            }
        }
        if (currentText) segments.push({text: currentText, startRow: currentStartRow});

        var urlRegex = /https?:\/\/[^\s]+/g;
        var xtermScreen = document.querySelector('.xterm-screen');
        if (!xtermScreen) return;
        var rowH = xtermScreen.offsetHeight / rows;
        var charW = xtermScreen.offsetWidth / cols;

        segments.forEach(function(seg) {
            var m;
            while ((m = urlRegex.exec(seg.text)) !== null) {
                var url = m[0].replace(/[\s)>\],.;:]+$/, ''); // trim trailing punct + spaces

                var urlEndOffset = m.index + url.length;
                var endRow = seg.startRow + Math.floor((urlEndOffset - 1) / cols);
                var endCol = (urlEndOffset - 1) % cols + 1; // column after last URL char

                var btn = document.createElement('a');
                btn.href = url;
                btn.target = '_blank';
                btn.rel = 'noopener';
                btn.textContent = '\u2197 Open';
                // Position right after the URL's last character
                var btnLeft = Math.min(endCol * charW + 4, xtermScreen.offsetWidth - 60);
                btn.style.cssText = 'position:absolute;left:' + btnLeft + 'px;top:' + (endRow * rowH + 2) + 'px;' +
                    'background:#1a7f37;color:#fff;padding:2px 8px;border-radius:4px;font-size:11px;' +
                    'pointer-events:auto;text-decoration:none;z-index:51;font-family:system-ui,sans-serif;' +
                    'line-height:1.4;box-shadow:0 1px 3px rgba(0,0,0,0.3);';
                urlOverlay.appendChild(btn);
            }
        });
    }

    term.onRender(detectUrls);
    term.onScroll(detectUrls);
})();

// ── Transcription overlay ──
// Detects voice transcription lines in the terminal (prefixed by "← b3 · voice:")
// and overlays a styled "b3 · voice" button over the prefix text. Hover (desktop)
// or tap (mobile) expands to show full transcription details. Popup is positioned
// inside the overlay container (position:absolute) so it scrolls with the terminal.
(function() {
    var wrap = document.getElementById('xterm-wrap');
    var txOverlay = document.createElement('div');
    txOverlay.id = 'tx-overlay-container';
    txOverlay.style.cssText = 'position:absolute;top:0;left:0;right:0;bottom:0;pointer-events:none;z-index:52;overflow:hidden;';
    wrap.appendChild(txOverlay);

    var _activePopup = null;     // { el, entry, btnRow } — currently open popup
    var _autoCloseTimer = null;
    var _lastAutoOpenTs = 0;
    var _pinnedEntry = null;     // entry that was clicked (stays open across renders)
    var _prevFingerprint = '';   // content hash of visible rows — skip rebuild if unchanged
    var _archiveAudio = null;    // active archive Audio element — survives rebuilds

    function findTranscriptionMatch(text) {
        if (!HC._transcriptionHistory || HC._transcriptionHistory.length === 0) return null;
        var clean = text.replace(/\u2026$/, '').trim();
        if (!clean) return null;
        for (var i = HC._transcriptionHistory.length - 1; i >= 0; i--) {
            var entry = HC._transcriptionHistory[i];
            if (entry.injection && entry.injection.indexOf(clean) === 0) return entry;
            if (entry.textModel && entry.textModel.indexOf(clean) === 0) return entry;
        }
        return null;
    }

    function escHtml(s) {
        return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
                .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    }

    function buildPopupHtml(entry) {
        var html = '';
        if (entry.textModel) {
            html += '<div style="margin-bottom:8px"><b style="color:#89b4fa">Text model</b>';
            if (entry.elapsedM) html += ' <span style="color:#6c7086">(' + entry.elapsedM.toFixed(1) + 's)</span>';
            html += '<br>' + escHtml(entry.textModel) + '</div>';
        }
        if (entry.voicesModel) {
            html += '<div style="margin-bottom:8px"><b style="color:#a6e3a1">Voices model</b>';
            if (entry.elapsedX) html += ' <span style="color:#6c7086">(' + entry.elapsedX.toFixed(1) + 's)</span>';
            html += '<br>' + escHtml(entry.voicesModel) + '</div>';
        }
        var meta = [];
        if (entry.audioDur) meta.push('Audio: ' + entry.audioDur + 's');
        if (entry.similarity) meta.push('Similarity: ' + entry.similarity);
        if (meta.length) {
            html += '<div style="color:#6c7086;font-size:11px;border-top:1px solid #585b70;padding-top:6px;margin-top:4px">' +
                meta.join(' \u00b7 ') + '</div>';
        }
        return html;
    }

    function createPopupEl(entry, btnLeft, btnTop, rowH, overlayW) {
        var el = document.createElement('div');
        el.style.cssText = 'position:absolute;z-index:200;background:#1e1e2e;color:#cdd6f4;' +
            'border:1px solid #585b70;border-radius:8px;padding:12px 16px;' +
            'max-width:min(500px,90vw);font-family:system-ui,sans-serif;font-size:13px;' +
            'line-height:1.5;box-shadow:0 4px 16px rgba(0,0,0,0.5);' +
            'word-break:break-word;white-space:pre-wrap;pointer-events:auto;';
        el.innerHTML = buildPopupHtml(entry);
        // Position below button first, flip above if no room
        var popLeft = Math.max(4, btnLeft);
        if (popLeft + 500 > overlayW) popLeft = Math.max(4, overlayW - 504);
        el.style.left = popLeft + 'px';
        el.style.top = (btnTop + rowH + 4) + 'px';
        return el;
    }

    function flipPopupIfNeeded(el, btnTop, rowH, overlayH) {
        // After adding to DOM, check if popup overflows bottom
        var popBottom = parseInt(el.style.top) + el.offsetHeight;
        if (popBottom > overlayH && btnTop > el.offsetHeight + 4) {
            // Flip above the button
            el.style.top = (btnTop - el.offsetHeight - 4) + 'px';
        }
    }

    // Close popup on click outside
    document.addEventListener('click', function(e) {
        if (_activePopup && !_activePopup.el.contains(e.target)) {
            _pinnedEntry = null;
            _activePopup = null;
        }
    });

    function detectTranscriptions() {
        var buf = term.buffer.active;
        var cols = term.cols;
        var viewportY = buf.viewportY;
        var rows = term.rows;

        var xtermScreen = document.querySelector('.xterm-screen');
        if (!xtermScreen) return;
        var rowH = xtermScreen.offsetHeight / rows;
        var charW = xtermScreen.offsetWidth / cols;
        var overlayW = txOverlay.offsetWidth || xtermScreen.offsetWidth;
        var overlayH = txOverlay.offsetHeight || xtermScreen.offsetHeight;

        var PREFIX = '\u2190 b3 \u00b7 voice: ';

        // Build fingerprint of visible rows — skip full rebuild if nothing changed.
        // This eliminates flicker from destroying/recreating DOM on every render tick.
        var fp = viewportY + ':' + rows + ':' + cols;
        for (var fi = 0; fi < rows; fi++) {
            var fLine = buf.getLine(viewportY + fi);
            if (fLine) {
                // Just sample first 40 chars + last 10 for a fast fingerprint
                var fText = '';
                for (var fj = 0; fj < Math.min(cols, 40); fj++) {
                    var fc = fLine.getCell(fj);
                    fText += fc ? fc.getChars() || ' ' : ' ';
                }
                fp += '|' + fText.trimEnd();
            }
        }
        // Also include transcription history length (new transcription = new auto-open)
        fp += '|h' + (HC._transcriptionHistory ? HC._transcriptionHistory.length : 0);
        // Include gapless player state — rebuild when playback starts/stops/pauses
        // so live→archive transition happens without waiting for scroll/output
        var _gp = HC.gaplessPlayer;
        var _gpPaused = _gp && _gp._htmlAudioEl ? (_gp._htmlAudioEl.paused ? 'P' : 'R') : '';
        fp += '|gp' + (_gp ? (_gp.isActive() ? '1' : '0') + (_gp._playbackStarted ? 'p' : '') + _gpPaused + (_gp._msgId || '') : 'x');
        // Include TTS history length — archive may have appeared
        fp += '|th' + (HC._ttsHistory ? HC._ttsHistory.length : 0);
        // Include archive audio state — rebuild when play/pause/stop changes
        fp += '|aa' + (_archiveAudio ? (_archiveAudio.paused ? 'P' : 'R') + (_archiveAudio._entryMsgId || '') : 'x');
        // Include hive history length for overlay
        fp += '|hv' + (HC._hiveHistory ? HC._hiveHistory.length : 0);
        if (fp === _prevFingerprint) return; // nothing changed — skip rebuild
        _prevFingerprint = fp;

        // Rebuild buttons + reposition popup
        txOverlay.innerHTML = '';
        var foundPinned = false;

        var SAY_PREFIX = '- say (MCP)';

        for (var i = 0; i < rows; i++) {
            var line = buf.getLine(viewportY + i);
            if (!line) continue;
            var text = '';
            for (var j = 0; j < cols; j++) {
                var cell = line.getCell(j);
                text += cell ? cell.getChars() || ' ' : ' ';
            }
            text = text.replace(/\s+$/, '');

            // ── Say play/pause/stop button ──
            var sayIdx = text.indexOf(SAY_PREFIX);
            if (sayIdx !== -1 && text.indexOf('b3:b3') !== -1) {
                var textMatch = text.match(/text:\s*"([^"]*)/);
                var sayText = textMatch ? textMatch[1] : '';

                // Check if this is the currently-streaming message.
                // Live mode = gapless player active + playback started + audio not paused.
                // If paused and archive exists → fall through to archive mode.
                var gp = HC.gaplessPlayer;
                // Match using original_text (pre-TTS-cleaning) — exact match, no normalization needed.
                var sayText30 = sayText ? sayText.substring(0, 30) : '';
                var isThisMessage = gp && gp.isActive() && gp._playbackStarted &&
                    gp._originalText && sayText30 &&
                    gp._originalText.indexOf(sayText30) === 0;
                var isPaused = isThisMessage && gp._htmlAudioEl && gp._htmlAudioEl.paused;
                var isLiveStreaming = isThisMessage && !isPaused;

                // Find archived entry — match on originalText first, fall back to textPreview
                var ttsEntry = null;
                if (sayText30 && HC._ttsHistory) {
                    for (var ti = HC._ttsHistory.length - 1; ti >= 0; ti--) {
                        var te = HC._ttsHistory[ti];
                        var matchText = te.originalText || te.textPreview || '';
                        if (matchText && matchText.indexOf(sayText30) === 0) {
                            ttsEntry = te;
                            break;
                        }
                    }
                }

                if (isLiveStreaming || ttsEntry) {
                    var playLeft = sayIdx * charW + charW * 0.25;  // nudge right by 1/4 char
                    var playW = SAY_PREFIX.length * charW;
                    var playBtn = document.createElement('button');
                    var stopBtn = document.createElement('button');
                    stopBtn.textContent = '\u25A0';
                    stopBtn.style.cssText = 'position:absolute;left:' + (playLeft + playW + 2) + 'px;top:' + (i * rowH + rowH * 0.15) + 'px;' +
                        'width:' + rowH + 'px;height:' + rowH + 'px;' +
                        'background:#da3633;color:#fff;border:none;border-radius:3px;font-size:11px;' +
                        'pointer-events:auto;cursor:pointer;font-family:system-ui,sans-serif;' +
                        'line-height:' + rowH + 'px;text-align:center;box-shadow:0 1px 3px rgba(0,0,0,0.3);';

                    if (isLiveStreaming) {
                        // Live streaming — pause only, no stop.
                        playBtn.textContent = '\u23F8 pause';
                        playBtn.style.cssText = 'position:absolute;left:' + playLeft + 'px;top:' + (i * rowH + rowH * 0.15) + 'px;' +
                            'width:' + playW + 'px;height:' + rowH + 'px;' +
                            'background:#1a7f37;color:#fff;border:none;border-radius:3px;font-size:11px;' +
                            'pointer-events:auto;cursor:pointer;font-family:system-ui,sans-serif;' +
                            'line-height:' + rowH + 'px;text-align:center;box-shadow:0 1px 3px rgba(0,0,0,0.3);';
                        // No stop button for live streaming (gapless player has no rewind)

                        (function(btn) {
                            btn.onclick = function(ev) {
                                ev.stopPropagation();
                                var el = HC.gaplessPlayer._htmlAudioEl;
                                if (el && !el.paused) {
                                    el.pause();
                                    btn.textContent = '\u25B6 say';
                                } else if (el && el.paused) {
                                    el.play();
                                    btn.textContent = '\u23F8 pause';
                                }
                            };
                        })(playBtn);
                    } else {
                        // Archived — play from daemon archive
                        // Check if archive audio is currently playing for this entry
                        var isArchivePlaying = _archiveAudio && !_archiveAudio.paused && _archiveAudio._entryMsgId === ttsEntry.msgId;
                        var isArchivePaused = _archiveAudio && _archiveAudio.paused && _archiveAudio._entryMsgId === ttsEntry.msgId;
                        playBtn.textContent = isArchivePlaying ? '\u23F8 pause' : '\u25B6 say';
                        playBtn.style.cssText = 'position:absolute;left:' + playLeft + 'px;top:' + (i * rowH + rowH * 0.15) + 'px;' +
                            'width:' + playW + 'px;height:' + rowH + 'px;' +
                            'background:#1a7f37;color:#fff;border:none;border-radius:3px;font-size:11px;' +
                            'pointer-events:auto;cursor:pointer;font-family:system-ui,sans-serif;' +
                            'line-height:' + rowH + 'px;text-align:center;box-shadow:0 1px 3px rgba(0,0,0,0.3);';
                        stopBtn.style.display = (isArchivePlaying || isArchivePaused) ? '' : 'none';
                        txOverlay.appendChild(stopBtn);

                        (function(entry, btn, sBtn) {
                            btn.onclick = function(ev) {
                                ev.stopPropagation();
                                // Playing → pause
                                if (_archiveAudio && !_archiveAudio.paused && _archiveAudio._entryMsgId === entry.msgId) {
                                    _archiveAudio.pause();
                                    btn.textContent = '\u25B6 say';
                                    return;
                                }
                                // Paused → resume
                                if (_archiveAudio && _archiveAudio.paused && _archiveAudio._entryMsgId === entry.msgId) {
                                    _archiveAudio.play();
                                    btn.textContent = '\u23F8 pause';
                                    return;
                                }
                                // Start fresh from archive
                                btn.textContent = '\u23F3 ...';
                                HC.daemonFetch('/api/tts-archive/' + entry.msgId).then(function(resp) {
                                    if (!resp.ok) throw new Error('HTTP ' + resp.status);
                                    return resp.blob();
                                }).then(function(blob) {
                                    // Stop any previous archive audio
                                    if (_archiveAudio) { try { _archiveAudio.pause(); } catch(e) {} if (_archiveAudio._blobUrl) URL.revokeObjectURL(_archiveAudio._blobUrl); }
                                    var blobUrl = URL.createObjectURL(blob);
                                    var audio = new Audio(blobUrl);
                                    audio._blobUrl = blobUrl;
                                    audio._entryMsgId = entry.msgId;
                                    _archiveAudio = audio;
                                    btn.textContent = '\u23F8 pause';
                                    sBtn.style.display = '';
                                    audio.onended = function() {
                                        URL.revokeObjectURL(blobUrl);
                                        _archiveAudio = null;
                                        _prevFingerprint = ''; // trigger rebuild → shows play button
                                    };
                                    audio.onerror = function() {
                                        URL.revokeObjectURL(blobUrl);
                                        _archiveAudio = null;
                                        _prevFingerprint = '';
                                    };
                                    audio.play();
                                }).catch(function() { btn.textContent = '\u25B6 say'; sBtn.style.display = 'none'; });
                            };
                            sBtn.onclick = function(ev) {
                                ev.stopPropagation();
                                if (_archiveAudio) { _archiveAudio.pause(); _archiveAudio.currentTime = 0; if (_archiveAudio._blobUrl) URL.revokeObjectURL(_archiveAudio._blobUrl); _archiveAudio = null; }
                                btn.textContent = '\u25B6 say';
                                btn.style.background = '#1a7f37';
                                sBtn.style.display = 'none';
                                _prevFingerprint = ''; // trigger rebuild
                            };
                            btn.onmouseenter = function() { if (!_archiveAudio) btn.style.background = '#2ea043'; };
                            btn.onmouseleave = function() { if (!_archiveAudio) btn.style.background = '#1a7f37'; };
                        })(ttsEntry, playBtn, stopBtn);
                    }
                    txOverlay.appendChild(playBtn);
                }
                continue;
            }

            // ── Hive message overlay ──
            // Claude Code renders channel tags as "← b3 · {user}: {content}"
            // where {user} is the sender name (e.g., "Claude", "Elena", "system").
            // Match by checking known senders from HC._hiveHistory.
            var B3_PREFIX = '\u2190 b3 \u00b7 ';
            var hiveEntry = null;
            var hiveIdx = -1;
            var hivePrefixLen = 0;
            if (HC._hiveHistory && HC._hiveHistory.length > 0 && text.indexOf(B3_PREFIX) !== -1) {
                // Check each known sender from history
                for (var hi = HC._hiveHistory.length - 1; hi >= 0; hi--) {
                    var he = HC._hiveHistory[hi];
                    var senderPrefix = B3_PREFIX + he.sender + ': ';
                    var idx = text.indexOf(senderPrefix);
                    if (idx !== -1) {
                        var hiveContent = text.substring(idx + senderPrefix.length).trim().replace(/\u2026$/, '').trim();
                        if (hiveContent && he.text && he.text.indexOf(hiveContent) === 0) {
                            hiveEntry = he;
                            hiveIdx = idx;
                            hivePrefixLen = senderPrefix.length;
                            break;
                        }
                    }
                }
            }
            if (hiveEntry) {
                    var hBtnLeft = hiveIdx * charW + charW * 0.25;
                    var hBtnLabel = 'b3 \u00b7 ' + hiveEntry.sender;
                    var hBtnW = (hivePrefixLen - 1) * charW;
                    var hBtn = document.createElement('button');
                    hBtn.textContent = hBtnLabel;
                    hBtn.style.cssText = 'position:absolute;left:' + hBtnLeft + 'px;top:' + (i * rowH + rowH * 0.15) + 'px;' +
                        'width:' + hBtnW + 'px;height:' + rowH + 'px;' +
                        'background:#1a7f37;color:#fff;border:none;border-radius:3px;font-size:11px;' +
                        'pointer-events:auto;cursor:pointer;font-family:system-ui,sans-serif;' +
                        'line-height:' + rowH + 'px;text-align:center;box-shadow:0 1px 3px rgba(0,0,0,0.3);';
                    (function(entry, btn, bLeft, bTop) {
                        var hoverTimer = null;
                        btn.onmouseenter = function() {
                            btn.style.background = '#2ea043';
                            if (!_pinnedEntry) {
                                hoverTimer = setTimeout(function() {
                                    var typeLabel = entry.type === 'hive_room' ? 'Room message' : 'Direct message';
                                    var html = '<div style="margin-bottom:4px"><b style="color:#89b4fa">' + escHtml(entry.sender) + '</b> <span style="color:#6c7086;font-size:11px">' + typeLabel + '</span></div>' +
                                        '<div>' + escHtml(entry.text) + '</div>';
                                    if (entry.roomTopic) html += '<div style="color:#6c7086;font-size:11px;margin-top:6px">Room: ' + escHtml(entry.roomTopic) + '</div>';
                                    if (entry.members && entry.members.length > 0) html += '<div style="color:#6c7086;font-size:11px;margin-top:2px">Members: ' + entry.members.map(escHtml).join(', ') + '</div>';
                                    var pop = document.createElement('div');
                                    pop.style.cssText = 'position:absolute;z-index:200;background:#1e1e2e;color:#cdd6f4;' +
                                        'border:1px solid #585b70;border-radius:8px;padding:12px 16px;' +
                                        'max-width:min(500px,90vw);font-family:system-ui,sans-serif;font-size:13px;' +
                                        'line-height:1.5;box-shadow:0 4px 16px rgba(0,0,0,0.5);word-break:break-word;white-space:pre-wrap;pointer-events:auto;';
                                    pop.innerHTML = html;
                                    pop.style.left = Math.max(4, bLeft) + 'px';
                                    pop.style.top = (bTop + rowH + 4) + 'px';
                                    txOverlay.appendChild(pop);
                                    _activePopup = { el: pop, entry: entry, btnRow: i };
                                }, 200);
                            }
                        };
                        btn.onmouseleave = function() {
                            btn.style.background = '#1a7f37';
                            if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null; }
                            if (_activePopup && !_pinnedEntry && _activePopup.entry === entry) {
                                if (_activePopup.el.parentNode) _activePopup.el.parentNode.removeChild(_activePopup.el);
                                _activePopup = null;
                            }
                        };
                        btn.onclick = function(ev) {
                            ev.stopPropagation();
                            if (_pinnedEntry === entry) { _pinnedEntry = null; _activePopup = null; }
                            else { _pinnedEntry = entry; btn.onmouseenter(); }
                        };
                    })(hiveEntry, hBtn, hBtnLeft, i * rowH + rowH * 0.15);
                    txOverlay.appendChild(hBtn);
                    continue;
            }

            var prefixIdx = text.indexOf(PREFIX);
            if (prefixIdx === -1) continue;

            var content = text.substring(prefixIdx + PREFIX.length).trim();
            var entry = findTranscriptionMatch(content);
            if (!entry) continue;

            var btnLeft = prefixIdx * charW;
            var btnW = PREFIX.length * charW;
            var btnTop = i * rowH;

            // Button
            var btn = document.createElement('button');
            btn.textContent = 'b3 \u00b7 voice';
            btn.style.cssText = 'position:absolute;left:' + btnLeft + 'px;top:' + btnTop + 'px;' +
                'width:' + btnW + 'px;height:' + rowH + 'px;' +
                'background:#1a7f37;color:#fff;border:none;border-radius:3px;font-size:11px;' +
                'pointer-events:auto;cursor:pointer;font-family:system-ui,sans-serif;' +
                'line-height:' + rowH + 'px;text-align:center;box-shadow:0 1px 3px rgba(0,0,0,0.3);';
            txOverlay.appendChild(btn);

            // Hover (desktop)
            (function(e, b, bLeft, bTop) {
                var hoverTimer = null;
                b.onmouseenter = function() {
                    b.style.background = '#2ea043';
                    if (!_pinnedEntry) {
                        hoverTimer = setTimeout(function() {
                            var pop = createPopupEl(e, bLeft, bTop, rowH, overlayW);
                            txOverlay.appendChild(pop);
                            flipPopupIfNeeded(pop, bTop, rowH, overlayH);
                            _activePopup = { el: pop, entry: e, btnRow: i };
                        }, 200);
                    }
                };
                b.onmouseleave = function() {
                    b.style.background = '#1a7f37';
                    if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null; }
                    // Close hover popup (not pinned ones)
                    if (_activePopup && !_pinnedEntry && _activePopup.entry === e) {
                        if (_activePopup.el.parentNode) _activePopup.el.parentNode.removeChild(_activePopup.el);
                        _activePopup = null;
                    }
                };
                b.onclick = function(ev) {
                    ev.stopPropagation();
                    if (_pinnedEntry === e) {
                        // Unpin
                        _pinnedEntry = null;
                        _activePopup = null;
                    } else {
                        // Pin this entry
                        _pinnedEntry = e;
                        if (_activePopup && _activePopup.el.parentNode) {
                            _activePopup.el.parentNode.removeChild(_activePopup.el);
                        }
                        var pop = createPopupEl(e, bLeft, bTop, rowH, overlayW);
                        txOverlay.appendChild(pop);
                        flipPopupIfNeeded(pop, bTop, rowH, overlayH);
                        _activePopup = { el: pop, entry: e, btnRow: i };
                    }
                };
            })(entry, btn, btnLeft, btnTop);

            // Re-attach pinned popup at new position after rebuild
            if (_pinnedEntry && _pinnedEntry === entry) {
                foundPinned = true;
                var pop = createPopupEl(entry, btnLeft, btnTop, rowH, overlayW);
                txOverlay.appendChild(pop);
                flipPopupIfNeeded(pop, btnTop, rowH, overlayH);
                _activePopup = { el: pop, entry: entry, btnRow: i };
            }

            // Auto-open for new transcriptions (first 3 seconds)
            if (!_pinnedEntry && entry.ts && entry.ts > _lastAutoOpenTs && (Date.now() - entry.ts < 3000)) {
                _lastAutoOpenTs = entry.ts;
                if (_autoCloseTimer) clearTimeout(_autoCloseTimer);
                var autoPop = createPopupEl(entry, btnLeft, btnTop, rowH, overlayW);
                txOverlay.appendChild(autoPop);
                flipPopupIfNeeded(autoPop, btnTop, rowH, overlayH);
                _activePopup = { el: autoPop, entry: entry, btnRow: i };
                (function(ap) {
                    _autoCloseTimer = setTimeout(function() {
                        if (ap.parentNode) ap.parentNode.removeChild(ap);
                        if (_activePopup && _activePopup.el === ap) _activePopup = null;
                    }, 3000);
                })(autoPop);
            }
        }

        // If pinned entry scrolled off screen, dismiss
        if (_pinnedEntry && !foundPinned) {
            _pinnedEntry = null;
            _activePopup = null;
        }
    }

    term.onRender(detectTranscriptions);
    term.onScroll(detectTranscriptions);
})();

// ── iOS scroll-into-view fix ──
// iOS Safari scrolls overflow:hidden containers when focusing an input
// (the xterm helper textarea). This shifts the entire terminal off-screen.
// Fix: always reset #xterm-wrap scrollTop to 0.
const _wrapEl = document.getElementById('xterm-wrap');
_wrapEl.addEventListener('scroll', () => {
    if (_wrapEl.scrollTop !== 0) _wrapEl.scrollTop = 0;
}, { passive: false });

// ── Scroll-to-bottom button ──
function scrollToBottom() {
    term.scrollToBottom();
    document.getElementById('fs-scroll-bottom').style.display = 'none';
}
// Use xterm buffer position (not viewport scrollTop — we set overflow-y:hidden)
setInterval(function() {
    const buf = term.buffer.active;
    const atBottom = buf.viewportY >= buf.baseY;
    var sb = document.getElementById('fs-scroll-bottom');
    if (sb) sb.style.display = atBottom ? 'none' : 'flex';
}, 500);

// ── Floating volume button ──
function toggleFsVolPopup(e) {
    if (e) { e.stopPropagation(); e.preventDefault(); }
    var popup = document.getElementById('fs-vol-popup');
    var fontPopup = document.getElementById('fs-font-popup');
    if (fontPopup) fontPopup.classList.remove('open');
    popup.classList.toggle('open');
}
function toggleFsFontPopup(e) {
    if (e) { e.stopPropagation(); e.preventDefault(); }
    var popup = document.getElementById('fs-font-popup');
    var volPopup = document.getElementById('fs-vol-popup');
    if (volPopup) volPopup.classList.remove('open');
    popup.classList.toggle('open');
    // Sync slider position to current font size
    _updateFontSliderUI();
}
var _playerPopupOpen = false;
function toggleFsPlayerPopup(e) {
    if (e) { e.stopPropagation(); e.preventDefault(); }
    var popup = document.getElementById('fs-player-popup');
    var volPopup = document.getElementById('fs-vol-popup');
    var fontPopup = document.getElementById('fs-font-popup');
    if (volPopup) volPopup.classList.remove('open');
    if (fontPopup) fontPopup.classList.remove('open');
    _playerPopupOpen = !_playerPopupOpen;
    popup.classList.toggle('open', _playerPopupOpen);
}
// Player popup buttons — touch handlers for mobile
(function() {
    var btns = ['fs-player-prev', 'fs-player-pause', 'fs-player-stop', 'fs-player-next'];
    btns.forEach(function(id) {
        var el = document.getElementById(id);
        if (!el) return;
        var touched = false;
        el.addEventListener('touchstart', function(e) {
            e.stopImmediatePropagation();
            e.preventDefault();
            touched = true;
            el.style.transform = 'scale(0.9)';
        }, { passive: false, capture: true });
        el.addEventListener('touchend', function(e) {
            if (!touched) return;
            e.stopImmediatePropagation();
            e.preventDefault();
            touched = false;
            el.style.transform = '';
            el.dispatchEvent(new MouseEvent('click', { bubbles: false }));
        }, { passive: false, capture: true });
        el.addEventListener('touchcancel', function() {
            touched = false;
            el.style.transform = '';
        }, { passive: true, capture: true });
    });
})();
// Update player popup text when mini player updates
var _origUpdateMiniPlayer = window.updateMiniPlayer;

// Sync floating volume with status bar volume
function syncFsVolume(pct) {
    var fill = document.getElementById('fs-vol-fill');
    var thumb = document.getElementById('fs-vol-thumb');
    var label = document.getElementById('fs-vol-pct');
    if (fill) fill.style.height = pct + '%';
    if (thumb) thumb.style.bottom = pct + '%';
    if (label) label.textContent = Math.round(pct) + '%';
}

// ── Font size slider ──
var _fontMin = 6, _fontMax = 24;
var _currentFontSize = _savedFontSize || 14;
function _applyFontSize(size) {
    size = Math.max(_fontMin, Math.min(_fontMax, Math.round(size)));
    _currentFontSize = size;
    // Apply to terminal
    term.options.fontSize = size;
    if (typeof _fitAddon !== 'undefined') _fitAddon.fit();
    // Apply to all file browser views via CSS custom property
    var fp = document.getElementById('files-panel');
    if (fp) {
        fp.style.setProperty('--files-font-size', size + 'px');
    }
    try { if (HC.saveLayoutSetting) HC.saveLayoutSetting('font_size', size); } catch(e) {}
    _updateFontSliderUI();
}
function _updateFontSliderUI() {
    var pct = ((_currentFontSize - _fontMin) / (_fontMax - _fontMin)) * 100;
    var fill = document.getElementById('fs-font-fill');
    var thumb = document.getElementById('fs-font-thumb');
    var label = document.getElementById('fs-font-pct');
    if (fill) fill.style.height = pct + '%';
    if (thumb) thumb.style.bottom = pct + '%';
    if (label) label.textContent = _currentFontSize + 'px';
}
_updateFontSliderUI();

// ── Mobile touch scroll fix ──
// xterm.js has no momentum touch scrolling (GitHub issue #594).
// Its native touch handler in Viewport.ts fights with custom handlers,
// so we suppress xterm's native touch scroll entirely and handle it ourselves.
setTimeout(function() {
    const viewport = document.querySelector('.xterm-viewport');
    if (viewport) {
        viewport.style.touchAction = 'none';
        // overflow-y: scroll is set via CSS so the scrollbar renders;
        // touch-action: none already suppresses xterm's scrollTop-based touch handler
    }
    const xtermTextarea = document.querySelector('.xterm-helper-textarea');
    if (xtermTextarea) xtermTextarea.style.pointerEvents = 'none';

    // Compute actual row height from rendered xterm rows
    function getRowHeight() {
        const rows = document.querySelector('.xterm-rows');
        if (rows && rows.children.length > 0) {
            return rows.children[0].getBoundingClientRect().height || 14;
        }
        return 14;
    }

    let _touchY = null;
    let _touchAccum = 0;
    let _velocity = 0;
    let _momentumId = null;
    let _lastDeltas = [];
    const LINE_PX = getRowHeight();
    const FRICTION = 0.95;   // Slower decay for longer iOS-style coasting
    const MIN_VEL = 0.3;
    const container = document.getElementById('xterm-wrap');
    if (!container) return;

    function stopMomentum() {
        if (_momentumId) { cancelAnimationFrame(_momentumId); _momentumId = null; }
    }

    function doMomentum() {
        _velocity *= FRICTION;
        if (Math.abs(_velocity) < MIN_VEL) { _momentumId = null; return; }
        _touchAccum += _velocity;
        const lines = Math.trunc(_touchAccum / LINE_PX);
        if (lines !== 0) {
            _touchAccum -= lines * LINE_PX;
            term.scrollLines(lines);
        }
        _momentumId = requestAnimationFrame(doMomentum);
    }

    // Register touch capture handlers for ALL stack buttons
    // Mobile touch events need stopImmediatePropagation to prevent
    // the scroll handler from eating them
    function registerStackBtn(id, action) {
        var btn = document.getElementById(id);
        if (!btn) return;
        var touched = false;
        document.addEventListener('touchstart', function(e) {
            if (e.target === btn || btn.contains(e.target)) {
                e.stopImmediatePropagation();
                e.preventDefault();
                touched = true;
                btn.style.transform = 'scale(0.9)';
            }
        }, { passive: false, capture: true });
        document.addEventListener('touchend', function(e) {
            if (touched) {
                e.stopImmediatePropagation();
                e.preventDefault();
                touched = false;
                btn.style.transform = '';
                action();
            }
        }, { passive: false, capture: true });
        document.addEventListener('touchcancel', function() {
            touched = false;
            btn.style.transform = '';
        }, { passive: true, capture: true });
        btn.addEventListener('click', function(e) {
            e.preventDefault();
            e.stopPropagation();
            action();
        });
    }
    registerStackBtn('fs-scroll-bottom', scrollToBottom);
    registerStackBtn('fs-vol-btn', function() { toggleFsVolPopup(); });
    registerStackBtn('fs-font-btn', function() { toggleFsFontPopup(); });
    registerStackBtn('fs-upload', function() { document.getElementById('file-upload-input').click(); });
    registerStackBtn('fs-files-btn', function() { HC.openFilesPanel(); });
    registerStackBtn('fs-info-btn', function() { HC.toggleInfoPanel(); });
    // Player button — custom handler to avoid popup controls triggering toggle
    (function() {
        var btn = document.getElementById('fs-player-btn');
        if (!btn) return;
        btn.addEventListener('touchstart', function(e) {
            e.stopPropagation();
            e.preventDefault();
            btn.style.transform = 'scale(0.9)';
        }, { passive: false });
        btn.addEventListener('touchend', function(e) {
            e.stopPropagation();
            e.preventDefault();
            btn.style.transform = '';
            toggleFsPlayerPopup();
        }, { passive: false });
        btn.addEventListener('click', function(e) {
            e.stopPropagation();
            e.preventDefault();
            toggleFsPlayerPopup();
        });
    })();

    // Now register the scroll touch handlers (after button handler)
    // IMPORTANT: touchstart must be passive:true — calling preventDefault()
    // on touchstart in iOS Chrome kills subsequent touchmove events,
    // causing swipes to produce only 1 move event instead of continuous.
    // We suppress browser scroll in touchmove instead.
    let _touchMoveCount = 0;
    document.addEventListener('touchstart', function(e) {
        if (!container.contains(e.target)) return;
        if (e.target.closest('#fs-scroll-bottom, .term-key, button, a, input')) return;
        if (e.touches.length === 1) {
            stopMomentum();
            _touchY = e.touches[0].clientY;
            _touchAccum = 0;
            _velocity = 0;
            _lastDeltas = [];
            _touchMoveCount = 0;
        }
    }, { passive: true, capture: true });

    document.addEventListener('touchmove', function(e) {
        if (_touchY === null || e.touches.length !== 1) return;
        if (!container.contains(e.target)) return;
        if (e.target.closest('#fs-scroll-bottom, input, .sel-handle')) return;
        e.preventDefault();
        e.stopPropagation();
        const curY = e.touches[0].clientY;
        const delta = _touchY - curY;  // positive = scroll down
        _touchY = curY;
        _lastDeltas.push(delta);
        if (_lastDeltas.length > 4) _lastDeltas.shift();
        _velocity = _lastDeltas.reduce((a, b) => a + b, 0) / _lastDeltas.length;
        _touchAccum += delta;
        const lines = Math.trunc(_touchAccum / LINE_PX);
        _touchMoveCount++;
        if (lines !== 0) {
            _touchAccum -= lines * LINE_PX;
            term.scrollLines(lines);
        }
    }, { passive: false, capture: true });

    document.addEventListener('touchend', function(e) {
        if (_touchY === null) return;
        _touchY = null;
        if (Math.abs(_velocity) > MIN_VEL) {
            _momentumId = requestAnimationFrame(doMomentum);
        }
    }, { passive: true, capture: true });

    document.addEventListener('touchcancel', function() {
        _touchY = null;
        _touchAccum = 0;
        stopMomentum();
    }, { passive: true, capture: true });

    log('[xterm] Touch scroll: term.scrollLines() with momentum, LINE_PX=' + LINE_PX.toFixed(1));
}, 200);

// Initialize LED strip around terminal
const ledCanvas = document.getElementById('led-canvas');
if (ledCanvas) { HC._ledStrip = new HC.LEDStrip(ledCanvas, 300); HC._ledStrip.loadRegisteredPatterns(); }

function fitTerminal() {
    // Save scroll offset from bottom (survives reflow from resize)
    const buf = term.buffer.active;
    const wasBack = buf.viewportY < buf.baseY;
    const offsetFromBottom = buf.baseY - buf.viewportY;
    try { fitAddon.fit(); } catch(e) {}
    if (wasBack) {
        requestAnimationFrame(() => {
            const newBase = term.buffer.active.baseY;
            const targetY = Math.max(0, newBase - offsetFromBottom);
            term.scrollToLine(targetY);
        });
    }
    sendResize();
}
function sendResize() {
    if (HC.ws && HC.ws.readyState === WebSocket.OPEN && term.cols && term.rows) {
        HC.ws.send(JSON.stringify({ type: 'resize', rows: term.rows, cols: term.cols }));
        // Update status immediately — don't wait for server echo
        if (HC.refreshStatusBar) HC.refreshStatusBar();
        else HC.setTerminalStatus(term.cols + 'x' + term.rows, true);
    }
}
fitTerminal();
// Debounce resize to avoid scroll jumps from mobile address bar hide/show
let _resizeTimer = null;
window.addEventListener('resize', () => {
    if (_resizeTimer) clearTimeout(_resizeTimer);
    _resizeTimer = setTimeout(fitTerminal, 300);
});
// ResizeObserver on xterm-wrap — catches ANY container size change
// (fullscreen toggle, keyboard, CSS transitions) without relying on
// manual fitTerminal() calls at every code point that changes layout.
if (typeof ResizeObserver !== 'undefined') {
    let _roTimer = null;
    new ResizeObserver(() => {
        if (_roTimer) clearTimeout(_roTimer);
        _roTimer = setTimeout(fitTerminal, 50);
    }).observe(document.getElementById('xterm-wrap'));
}

// ── Virtual Keyboard Resize (mobile) ──
// When the on-screen keyboard opens, it covers the bottom half of the screen.
// We detect this via visualViewport.height shrinking, then:
//   1. Add body.terminal-keyboard OR body.terminal-keyboard-text
//      (terminal mode hides voice bar; text mode keeps it for text input)
//   2. Constrain body height to the visible area above the keyboard
//   3. Re-fit xterm to the now-smaller terminal container
// When the keyboard closes, we restore everything.
const KEYBOARD_THRESHOLD = 150; // px — keyboards are at least ~200px tall
const _isIOS = /iPad|iPhone|iPod/.test(navigator.userAgent);
let _keyboardVisible = false;
let _keyboardSource = 'terminal'; // 'terminal' or 'text'

// Track what the user tapped to open the keyboard
document.getElementById('text-input').addEventListener('focus', () => { _keyboardSource = 'text'; });
// Terminal taps go through xterm's helper textarea
const _xtermTA = document.querySelector('.xterm-helper-textarea');
if (_xtermTA) _xtermTA.addEventListener('focus', () => { _keyboardSource = 'terminal'; });

function _kbClass() {
    return _keyboardSource === 'text' ? 'terminal-keyboard-text' : 'terminal-keyboard';
}

function checkKeyboardState() {
    if (!window.visualViewport) return;
    const vvH = window.visualViewport.height;
    const winH = window.innerHeight;
    // iOS: innerHeight stays fixed, visualViewport.height shrinks.
    // Android: both change — diff is still the keyboard height.
    const diff = winH - vvH;
    const nowVisible = diff > KEYBOARD_THRESHOLD;

    if (nowVisible && _keyboardVisible) {
        // Keyboard still open but may have resized (e.g. suggestions row toggled)
        document.body.style.height = Math.round(vvH) + 'px';
        fitTerminal();
    }

    if (nowVisible !== _keyboardVisible) {
        _keyboardVisible = nowVisible;
        if (nowVisible) {
            // Keyboard opened: shrink body to visible area, collapse chrome
            document.body.classList.add(_kbClass());
            if (window.visualViewport) {
                document.body.style.height = Math.round(vvH) + 'px';
                document.body.style.overflow = 'hidden';
            }
        } else {
            // Keyboard closed: restore full-height layout
            document.body.classList.remove('terminal-keyboard', 'terminal-keyboard-text');
            document.body.style.height = '';
            document.body.style.overflow = '';
            // Prevent iOS scroll-jump when keyboard dismisses
            if (_isIOS) setTimeout(() => window.scrollTo(0, 0), 0);
        }
        // Re-fit terminal after layout settles — needs multiple passes on mobile
        requestAnimationFrame(() => fitTerminal());
        setTimeout(() => fitTerminal(), 200);
        setTimeout(() => fitTerminal(), 500);
    }
}

if (window.visualViewport) {
    window.visualViewport.addEventListener('resize', checkKeyboardState);
    // iOS sometimes scrolls the viewport instead of firing resize
    window.visualViewport.addEventListener('scroll', checkKeyboardState);
}
// iOS insurance: visualViewport.resize is unreliable on Safari — poll briefly on focus
if (_isIOS) {
    document.addEventListener('focusin', () => {
        const id = setInterval(checkKeyboardState, 100);
        setTimeout(() => clearInterval(id), 1000);
    });
    document.addEventListener('focusout', () => {
        setTimeout(checkKeyboardState, 300);
    });
}
// ── End Virtual Keyboard Resize ──

// ── Keyboard Input → Daemon PTY ──
term.onData((data) => {
    if (data === '\x1b[I' || data === '\x1b[O') return;
    // Filter out SGR mouse wheel sequences — xterm generates these from touch/scroll
    // but Claude Code's PTY doesn't have mouse reporting, so they echo as raw text.
    // SGR format: \x1b[<64;...M (scroll up) or \x1b[<65;...M (scroll down)
    if (data.startsWith('\x1b[<6') && (data.includes('M') || data.includes('m'))) return;
    if (HC.ws && HC.ws.readyState === WebSocket.OPEN) {
        HC.ws.send(data);
    }
});

// ── Text Input → Daemon PTY (multi-line with smart Enter) ──
const textInput = document.getElementById('text-input');
let _lastKeyTime = 0;
const PASTE_THRESHOLD_MS = 100; // Enter within 100ms of prev key = paste, not send

function sendTextInput() {
    const text = textInput.value.trim();
    if (!text) return;
    textInput.value = '';
    textInput.style.height = 'auto';
    if (HC.ws && HC.ws.readyState === WebSocket.OPEN) {
        // Send each line followed by \r (Enter), preserving multi-line structure
        const lines = text.split('\n');
        for (let i = 0; i < lines.length; i++) {
            if (i > 0) HC.ws.send('\r');
            HC.ws.send(lines[i]);
        }
        HC.ws.send('\r');
    }
}

textInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
        const now = Date.now();
        const elapsed = now - _lastKeyTime;
        const val = textInput.value;
        const cursorPos = textInput.selectionStart;
        const charBefore = cursorPos > 0 ? val[cursorPos - 1] : '';

        // Backslash + Enter = explicit multi-line (remove backslash, insert newline)
        if (charBefore === '\\') {
            e.preventDefault();
            textInput.value = val.slice(0, cursorPos - 1) + '\n' + val.slice(cursorPos);
            textInput.selectionStart = textInput.selectionEnd = cursorPos; // cursor after newline
            textInput.dispatchEvent(new Event('input')); // trigger auto-resize
            _lastKeyTime = now;
            return;
        }

        // Fast Enter (< threshold) = paste, insert newline
        if (elapsed < PASTE_THRESHOLD_MS) {
            // Let the default behavior insert the newline
            _lastKeyTime = now;
            return;
        }

        // Slow Enter = send
        e.preventDefault();
        sendTextInput();
        _lastKeyTime = now;
        return;
    }
    _lastKeyTime = Date.now();
});

// Auto-resize textarea to fit content (up to max-height)
textInput.addEventListener('input', () => {
    textInput.style.height = 'auto';
    textInput.style.height = Math.min(textInput.scrollHeight, parseFloat(getComputedStyle(textInput).maxHeight) || 96) + 'px';
});

// ── Fullscreen Text Input (collapsible) ──
const fsTextInput = document.getElementById('fs-text-input');
const fsTextWrap = document.getElementById('fs-text-input-wrap');

window.toggleFsTextInput = function() {
    if (!fsTextWrap) return;
    const isOpen = fsTextWrap.classList.toggle('open');
    if (isOpen && fsTextInput) {
        fsTextInput.focus();
    } else if (fsTextInput) {
        fsTextInput.blur();
    }
};

if (fsTextInput) {
    fsTextInput.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            const text = fsTextInput.value.trim();
            if (!text || !HC.ws || HC.ws.readyState !== WebSocket.OPEN) return;
            const lines = text.split('\n');
            for (let i = 0; i < lines.length; i++) {
                if (i > 0) HC.ws.send('\r');
                HC.ws.send(lines[i]);
            }
            HC.ws.send('\r');
            fsTextInput.value = '';
            fsTextInput.style.height = 'auto';
            if (fsTextWrap) fsTextWrap.classList.remove('open');
        }
        if (e.key === 'Escape') {
            if (fsTextWrap) fsTextWrap.classList.remove('open');
            fsTextInput.blur();
        }
    });
    fsTextInput.addEventListener('input', () => {
        fsTextInput.style.height = 'auto';
        fsTextInput.style.height = Math.min(fsTextInput.scrollHeight, 120) + 'px';
    });

    // iOS virtual keyboard fix: when our textarea gets focus,
    // iOS doesn't resize the layout viewport for position:fixed elements.
    // Use visualViewport to detect keyboard height and offset fixed elements.
    if (window.visualViewport) {
        let _fsTextFocused = false;
        const _fixedEls = function() {
            return [
                document.getElementById('fs-text-input-wrap'),
                document.getElementById('fs-mic'),
                document.getElementById('fs-upload'),
                document.getElementById('fs-toast'),
            ].filter(Boolean);
        };

        function applyKeyboardOffset() {
            if (!_fsTextFocused) return;
            const vv = window.visualViewport;
            // keyboard height = full viewport - visual viewport - visual viewport offset
            const kbHeight = window.innerHeight - vv.height - vv.offsetTop;
            if (kbHeight > 50) {
                // Shift fixed-bottom elements up by keyboard height
                document.documentElement.style.setProperty('--kb-offset', kbHeight + 'px');
                // Prevent page from scrolling behind keyboard
                window.scrollTo(0, 0);
            }
        }

        fsTextInput.addEventListener('focus', () => {
            _fsTextFocused = true;
            // Small delay to let iOS settle the viewport
            setTimeout(applyKeyboardOffset, 100);
        });
        fsTextInput.addEventListener('blur', () => {
            _fsTextFocused = false;
            document.documentElement.style.setProperty('--kb-offset', '0px');
            window.scrollTo(0, 0);
        });
        window.visualViewport.addEventListener('resize', applyKeyboardOffset);
        window.visualViewport.addEventListener('scroll', () => {
            if (_fsTextFocused) window.scrollTo(0, 0);
        });
    }
}

// ── File Upload ──
document.getElementById('file-upload-input').addEventListener('change', async function() {
    if (!this.files || !this.files[0]) return;
    const file = this.files[0];
    const sizeKB = (file.size / 1024).toFixed(0);
    const btn = document.getElementById('upload-btn');
    btn.disabled = true;
    log('Uploading: ' + file.name + ' (' + sizeKB + ' KB)');

    var fp = document.getElementById('files-panel');
    var destDir = (fp && fp.classList.contains('open') && HC.getFilesAbsolutePath && HC.getFilesAbsolutePath()) ? HC.getFilesAbsolutePath() : null;

    try {
        var resp;
        if (HC._daemonBase) {
            // Direct connection — use multipart FormData
            var formData = new FormData();
            formData.append('file', file);
            if (destDir) formData.append('dest_dir', destDir);
            resp = await HC.daemonFetch('/api/file-upload', { method: 'POST', body: formData });
        } else {
            // WS relay path — FormData can't serialize over text WS, use JSON+base64
            var buf = await file.arrayBuffer();
            var bytes = new Uint8Array(buf);
            var bin = '';
            for (var i = 0; i < bytes.length; i += 8192) {
                bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
            }
            var b64 = btoa(bin);
            var payload = { filename: file.name, data_b64: b64 };
            if (destDir) payload.dest_dir = destDir;
            resp = await HC.daemonFetch('/api/file-upload-json', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(payload),
            });
        }
        const result = await resp.json();
        if (result && result.status === 'ok') {
            log('Upload ok: ' + result.path);
            // Refresh file list if in file browser
            if (fp && fp.classList.contains('open') && HC.isViewingFile && !HC.isViewingFile()) {
                HC.loadFiles(HC.getFilesCurrentPath ? HC.getFilesCurrentPath() : '.');
            }
        } else {
            log('Upload failed: ' + JSON.stringify(result));
        }
    } catch(err) {
        log('Upload error: ' + err);
    }
    setTimeout(() => { btn.disabled = false; }, 1000);
    this.value = '';
});

// ── Terminal Key Strip ──
const TERM_KEY_MAP = {
    'up': '\x1b[A', 'down': '\x1b[B', 'left': '\x1b[D', 'right': '\x1b[C',
    'esc': '\x1b', 'ctrl-c': '\x03', 'ctrl-b': '\x02', 'ctrl-o': '\x0f',
    'enter': '\r',
};
document.querySelectorAll('.term-key[data-seq]').forEach(btn => {
    function _sendTermKey() {
        const data = TERM_KEY_MAP[btn.dataset.seq] || btn.dataset.seq;
        if (HC.ws && HC.ws.readyState === WebSocket.OPEN) HC.ws.send(data);
    }
    // On touch: preventDefault on touchstart prevents the button from stealing
    // focus from the xterm textarea, which would dismiss the iOS keyboard.
    // Key is sent on touchend. The click handler covers non-touch devices.
    btn.addEventListener('touchstart', (e) => {
        e.preventDefault();
        e.stopPropagation();
    }, { passive: false });
    btn.addEventListener('touchend', (e) => {
        e.preventDefault();
        e.stopPropagation();
        _sendTermKey();
    }, { passive: false });
    btn.addEventListener('click', (e) => {
        e.preventDefault();
        _sendTermKey();
    });
});

// ── Long-Press Copy/Paste (mobile-native feel) ──
// Long press (500ms, <10px movement) on terminal → selection at that word.
// Drag handles appear to extend selection. Popup with Copy/Paste/Select All.
// Long press on empty area → Paste popup.
(function() {
    const copyPopup = document.getElementById('copy-popup');
    const copyBtn = document.getElementById('copy-popup-copy');
    const selectAllBtn = document.getElementById('copy-popup-selectall');
    const pastePopupBtn = document.getElementById('copy-popup-paste');
    const handleStart = document.getElementById('sel-handle-start');
    const handleEnd = document.getElementById('sel-handle-end');
    const container = document.getElementById('xterm-wrap');
    if (!container || !copyPopup) return;

    const LONG_PRESS_MS = 500;
    const MOVE_THRESHOLD = 10; // px — more than this cancels long-press
    let _lpTimer = null;
    let _lpStartX = 0, _lpStartY = 0;
    let _lpCancelled = false;
    let _selecting = false; // true when selection is active with handles
    let _selAnchor = null;  // {col, row} — the long-press origin
    let _draggingHandle = null; // 'start' or 'end' or null
    let _selStartPos = null; // {col, row} of selection start
    let _selEndPos = null;   // {col, row} of selection end
    let _scrollInterval = null; // for auto-scroll when handle near edge

    function getRowHeight() {
        const rows = document.querySelector('.xterm-rows');
        if (rows && rows.children.length > 0) return rows.children[0].getBoundingClientRect().height || 14;
        return 14;
    }

    function getColWidth() {
        const rows = document.querySelector('.xterm-rows');
        if (rows && rows.children.length > 0) {
            const row = rows.children[0];
            const rect = row.getBoundingClientRect();
            return (rect.width / term.cols) || 8;
        }
        return 8;
    }

    function touchToBufferPos(clientX, clientY) {
        const xtermEl = document.querySelector('.xterm-screen');
        if (!xtermEl) return null;
        const rect = xtermEl.getBoundingClientRect();
        const x = clientX - rect.left;
        const y = clientY - rect.top;
        const rowH = getRowHeight();
        const colW = getColWidth();
        const col = Math.max(0, Math.min(term.cols - 1, Math.floor(x / colW)));
        const viewRow = Math.max(0, Math.floor(y / rowH));
        const bufRow = term.buffer.active.viewportY + viewRow;
        return { col, row: bufRow };
    }

    function bufferPosToScreen(pos) {
        const xtermEl = document.querySelector('.xterm-screen');
        if (!xtermEl) return { x: 0, y: 0 };
        const rect = xtermEl.getBoundingClientRect();
        const rowH = getRowHeight();
        const colW = getColWidth();
        const viewRow = pos.row - term.buffer.active.viewportY;
        return {
            x: rect.left + pos.col * colW,
            y: rect.top + viewRow * rowH
        };
    }

    // Get the word boundaries at a buffer position
    function getWordAt(pos) {
        const line = term.buffer.active.getLine(pos.row);
        if (!line) return { start: pos.col, end: pos.col + 1 };
        let start = pos.col, end = pos.col;
        // Expand left
        while (start > 0) {
            const ch = line.getCell(start - 1);
            if (!ch || ch.getChars() === '' || ch.getChars() === ' ') break;
            start--;
        }
        // Expand right
        while (end < term.cols - 1) {
            const ch = line.getCell(end + 1);
            if (!ch || ch.getChars() === '' || ch.getChars() === ' ') break;
            end++;
        }
        return { start, end: end + 1 }; // end is exclusive
    }

    function applySelection() {
        if (!_selStartPos || !_selEndPos) return;
        let sr = _selStartPos.row, sc = _selStartPos.col;
        let er = _selEndPos.row, ec = _selEndPos.col;
        // Ensure start <= end
        if (er < sr || (er === sr && ec < sc)) {
            [sr, sc, er, ec] = [er, ec, sr, sc];
        }
        if (sr === er) {
            term.select(sc, sr, Math.max(1, ec - sc));
        } else {
            const totalChars = (term.cols - sc) + (er - sr - 1) * term.cols + ec;
            term.select(sc, sr, totalChars);
        }
    }

    function positionHandles() {
        if (!_selStartPos || !_selEndPos) return;
        const rowH = getRowHeight();
        // Start handle: left edge of selection start
        let sp = _selStartPos, ep = _selEndPos;
        if (ep.row < sp.row || (ep.row === sp.row && ep.col < sp.col)) {
            [sp, ep] = [ep, sp];
        }
        const sScreen = bufferPosToScreen(sp);
        const eScreen = bufferPosToScreen(ep);
        handleStart.style.left = sScreen.x + 'px';
        handleStart.style.top = (sScreen.y - 2) + 'px'; // above the line
        handleStart.style.display = 'block';
        handleEnd.style.left = eScreen.x + 'px';
        handleEnd.style.top = (eScreen.y + rowH + 2) + 'px'; // below the line
        handleEnd.style.display = 'block';
    }

    function showPopup(x, y, hasSelection) {
        // Enable/disable buttons based on context
        copyBtn.disabled = !hasSelection;
        copyBtn.style.display = hasSelection ? 'block' : 'none';
        selectAllBtn.style.display = 'block';
        pastePopupBtn.style.display = 'block';

        const popup = copyPopup;
        popup.style.display = 'block';
        // Measure popup for positioning
        const pw = popup.offsetWidth || 120;
        const ph = popup.offsetHeight || 100;
        let left = Math.min(x - pw / 2, window.innerWidth - pw - 10);
        left = Math.max(10, left);
        let top = y - ph - 15;
        if (top < 10) top = y + 25;
        popup.style.left = left + 'px';
        popup.style.top = top + 'px';
    }

    function hidePopup() { copyPopup.style.display = 'none'; }

    function hideHandles() {
        handleStart.style.display = 'none';
        handleEnd.style.display = 'none';
    }

    function clearAll() {
        _selecting = false;
        _selAnchor = null;
        _selStartPos = null;
        _selEndPos = null;
        _draggingHandle = null;
        term.clearSelection();
        hideHandles();
        hidePopup();
        if (_scrollInterval) { clearInterval(_scrollInterval); _scrollInterval = null; }
    }

    // ── Long-press detection ──
    // Registered BEFORE the scroll handlers (which are in setTimeout 200ms).
    // On touchstart: start the timer. On touchmove >threshold or touchend <500ms: cancel.
    // On timer fire: we have a long press → begin selection.

    document.addEventListener('touchstart', function(e) {
        // Skip buttons, popups, handles, voice bar
        if (e.target.closest('.term-key, button, a, input, textarea, select, .copy-popup, .sel-handle, .voice-bar, .terminal-status')) return;
        if (!container.contains(e.target)) return;
        if (e.touches.length !== 1) return;

        // If already selecting and user taps outside handles, clear
        if (_selecting && !e.target.closest('.sel-handle')) {
            clearAll();
            // Don't start a new long-press on the clearing tap
            return;
        }

        const t = e.touches[0];
        _lpStartX = t.clientX;
        _lpStartY = t.clientY;
        _lpCancelled = false;

        _lpTimer = setTimeout(() => {
            if (_lpCancelled) return;
            // Long press triggered! Haptic feedback if available
            if (navigator.vibrate) navigator.vibrate(30);

            const pos = touchToBufferPos(_lpStartX, _lpStartY);
            if (!pos) return;

            // Check if there's a word at this position
            const word = getWordAt(pos);
            _selAnchor = pos;
            _selStartPos = { col: word.start, row: pos.row };
            _selEndPos = { col: word.end, row: pos.row };
            _selecting = true;

            applySelection();
            positionHandles();
            showPopup(_lpStartX, _lpStartY, true);

            log('[select] Long-press selection at row=' + pos.row + ' col=' + pos.col);
        }, LONG_PRESS_MS);
    }, { passive: true, capture: true });

    document.addEventListener('touchmove', function(e) {
        if (_lpTimer && !_lpCancelled) {
            const t = e.touches[0];
            const dx = t.clientX - _lpStartX;
            const dy = t.clientY - _lpStartY;
            if (Math.sqrt(dx * dx + dy * dy) > MOVE_THRESHOLD) {
                _lpCancelled = true;
                clearTimeout(_lpTimer);
                _lpTimer = null;
            }
        }
    }, { passive: true, capture: true });

    document.addEventListener('touchend', function() {
        if (_lpTimer) {
            clearTimeout(_lpTimer);
            _lpTimer = null;
        }
    }, { passive: true, capture: true });

    document.addEventListener('touchcancel', function() {
        if (_lpTimer) {
            clearTimeout(_lpTimer);
            _lpTimer = null;
        }
        _lpCancelled = true;
    }, { passive: true, capture: true });

    // ── Handle dragging ──
    handleStart.addEventListener('touchstart', function(e) {
        e.preventDefault();
        e.stopImmediatePropagation();
        _draggingHandle = 'start';
        hidePopup();
    }, { passive: false, capture: true });

    handleEnd.addEventListener('touchstart', function(e) {
        e.preventDefault();
        e.stopImmediatePropagation();
        _draggingHandle = 'end';
        hidePopup();
    }, { passive: false, capture: true });

    document.addEventListener('touchmove', function(e) {
        if (!_draggingHandle) return;
        e.preventDefault();
        e.stopImmediatePropagation();

        const t = e.touches[0];
        const pos = touchToBufferPos(t.clientX, t.clientY);
        if (!pos) return;

        if (_draggingHandle === 'start') {
            _selStartPos = pos;
        } else {
            _selEndPos = pos;
        }

        applySelection();
        positionHandles();

        // Auto-scroll when handle is near viewport edge
        const xtermEl = document.querySelector('.xterm-screen');
        if (xtermEl) {
            const rect = xtermEl.getBoundingClientRect();
            const edgeZone = 30;
            if (_scrollInterval) { clearInterval(_scrollInterval); _scrollInterval = null; }
            if (t.clientY < rect.top + edgeZone) {
                _scrollInterval = setInterval(() => {
                    term.scrollLines(-1);
                    if (_draggingHandle === 'start') _selStartPos.row--;
                    else _selEndPos.row--;
                    applySelection();
                    positionHandles();
                }, 80);
            } else if (t.clientY > rect.bottom - edgeZone) {
                _scrollInterval = setInterval(() => {
                    term.scrollLines(1);
                    if (_draggingHandle === 'start') _selStartPos.row++;
                    else _selEndPos.row++;
                    applySelection();
                    positionHandles();
                }, 80);
            }
        }
    }, { passive: false, capture: true });

    document.addEventListener('touchend', function(e) {
        if (!_draggingHandle) return;
        e.stopImmediatePropagation();
        if (_scrollInterval) { clearInterval(_scrollInterval); _scrollInterval = null; }
        _draggingHandle = null;

        // Show popup after handle drag
        const touch = e.changedTouches[0];
        const sel = term.getSelection();
        if (touch && sel) {
            showPopup(touch.clientX, touch.clientY, true);
        }
    }, { passive: false, capture: true });

    // ── Popup button handlers ──
    copyBtn.addEventListener('click', (e) => {
        e.preventDefault();
        e.stopPropagation();
        const sel = term.getSelection();
        if (sel) {
            navigator.clipboard.writeText(sel).then(() => {
                log('[select] Copied ' + sel.length + ' chars');
                copyBtn.textContent = 'Copied!';
                setTimeout(() => { copyBtn.textContent = 'Copy'; }, 800);
            }).catch(err => log('[select] Copy failed: ' + err));
        }
        setTimeout(clearAll, 400);
    });

    selectAllBtn.addEventListener('click', (e) => {
        e.preventDefault();
        e.stopPropagation();
        term.selectAll();
        const sel = term.getSelection();
        if (sel) {
            navigator.clipboard.writeText(sel).then(() => {
                log('[select] Copied all: ' + sel.length + ' chars');
                selectAllBtn.textContent = 'Copied!';
                setTimeout(() => { selectAllBtn.textContent = 'Select All'; }, 800);
            }).catch(err => log('[select] Copy all failed: ' + err));
        }
        setTimeout(clearAll, 400);
    });

    pastePopupBtn.addEventListener('click', async (e) => {
        e.preventDefault();
        e.stopPropagation();
        try {
            const text = await navigator.clipboard.readText();
            if (text && HC.ws && HC.ws.readyState === WebSocket.OPEN) {
                HC.ws.send(text);
                log('[paste] Pasted ' + text.length + ' chars');
                pastePopupBtn.textContent = 'Pasted!';
                setTimeout(() => { pastePopupBtn.textContent = 'Paste'; }, 800);
            }
        } catch(err) {
            log('[paste] Clipboard API denied: ' + err);
            pastePopupBtn.textContent = 'Denied';
            setTimeout(() => { pastePopupBtn.textContent = 'Paste'; }, 1000);
        }
        setTimeout(clearAll, 400);
    });

    // Dismiss when tapping outside popup+handles while selecting
    document.addEventListener('click', function(e) {
        if (copyPopup.style.display === 'block' &&
            !copyPopup.contains(e.target) &&
            !handleStart.contains(e.target) &&
            !handleEnd.contains(e.target)) {
            clearAll();
        }
    });

    log('[select] Long-press copy/paste initialized');
})();

// ── Click-to-cursor: tap/click → move terminal cursor via arrow keys ──
// DISABLED: needs iteration — xterm.js mouse reporting interferes, only 1 arrow fires
// TODO: research existing xterm.js click-to-cursor implementations
(function() {
    return; // disabled — see TODO above
    const container = document.getElementById('xterm-wrap');
    if (!container) return;

    function getRowHeight() {
        const rows = document.querySelector('.xterm-rows');
        if (rows && rows.children.length > 0) return rows.children[0].getBoundingClientRect().height || 14;
        return 14;
    }

    function getColWidth() {
        const rows = document.querySelector('.xterm-rows');
        if (rows && rows.children.length > 0) {
            const row = rows.children[0];
            const rect = row.getBoundingClientRect();
            return (rect.width / term.cols) || 8;
        }
        return 8;
    }

    function clickToBufferPos(clientX, clientY) {
        const xtermEl = document.querySelector('.xterm-screen');
        if (!xtermEl) return null;
        const rect = xtermEl.getBoundingClientRect();
        const x = clientX - rect.left;
        const y = clientY - rect.top;
        const rowH = getRowHeight();
        const colW = getColWidth();
        const col = Math.max(0, Math.min(term.cols - 1, Math.floor(x / colW)));
        const viewRow = Math.max(0, Math.floor(y / rowH));
        const bufRow = term.buffer.active.viewportY + viewRow;
        return { col, row: bufRow };
    }

    function showJumpIndicator(clientX, clientY) {
        const rowH = getRowHeight();
        const colW = getColWidth();
        const div = document.createElement('div');
        div.className = 'cursor-jump-indicator';
        div.style.width = colW + 'px';
        div.style.height = rowH + 'px';
        div.style.left = (clientX - colW / 2) + 'px';
        div.style.top = (clientY - rowH / 2) + 'px';
        document.body.appendChild(div);
        div.addEventListener('animationend', () => div.remove());
    }

    function moveCursorTo(targetCol, targetRow) {
        const buf = term.buffer.active;
        // Safety: viewport scrolled back in history?
        if (buf.viewportY < buf.baseY) return;
        // Safety: target is in scrollback (above active area)?
        if (targetRow < buf.baseY) return;
        // Current cursor in absolute buffer coords
        const cursorAbsRow = buf.baseY + buf.cursorY;
        const deltaRows = targetRow - cursorAbsRow;
        const deltaCols = targetCol - buf.cursorX;
        if (deltaRows === 0 && deltaCols === 0) return;
        // Build arrow key sequence — vertical first, then horizontal
        let seq = '';
        // Check for application cursor mode (vim, tmux, etc.)
        const appMode = term.modes && term.modes.applicationCursorKeysMode;
        const up    = appMode ? '\x1bOA' : '\x1b[A';
        const down  = appMode ? '\x1bOB' : '\x1b[B';
        const right = appMode ? '\x1bOC' : '\x1b[C';
        const left  = appMode ? '\x1bOD' : '\x1b[D';
        const upDown = deltaRows < 0 ? up : down;
        for (let i = 0; i < Math.abs(deltaRows); i++) seq += upDown;
        const leftRight = deltaCols < 0 ? left : right;
        for (let i = 0; i < Math.abs(deltaCols); i++) seq += leftRight;
        if (seq && HC.ws && HC.ws.readyState === WebSocket.OPEN) {
            HC.ws.send(seq);
            log('[cursor-jump] moved ' + deltaRows + ' rows, ' + deltaCols + ' cols');
        }
    }

    // ── Desktop: click handler ──
    container.addEventListener('click', function(e) {
        // Ignore clicks on interactive elements
        if (e.target.closest('.term-key, button, a, input, textarea, select, .copy-popup, .sel-handle, .voice-bar, .terminal-status')) return;
        // Ignore modified clicks (Shift+click for selection, Ctrl+click for links, etc.)
        if (e.shiftKey || e.ctrlKey || e.metaKey) return;
        // Ignore if text is selected in terminal
        if (term.hasSelection && term.hasSelection()) return;
        // Only on mobile we use the tap handler — skip click on touch devices
        if ('ontouchstart' in window) return;
        const pos = clickToBufferPos(e.clientX, e.clientY);
        if (!pos) return;
        showJumpIndicator(e.clientX, e.clientY);
        moveCursorTo(pos.col, pos.row);
    });

    // ── Mobile: tap detection (distinguishes tap from scroll/long-press) ──
    let _tapStartX = 0, _tapStartY = 0, _tapStartTime = 0;
    let _tapMoved = false;
    const TAP_MOVE_THRESHOLD = 10; // px
    const TAP_MAX_DURATION = 300;  // ms

    container.addEventListener('touchstart', function(e) {
        if (e.touches.length !== 1) return;
        if (e.target.closest('.term-key, button, a, input, textarea, select, .copy-popup, .sel-handle, .voice-bar, .terminal-status')) return;
        const t = e.touches[0];
        _tapStartX = t.clientX;
        _tapStartY = t.clientY;
        _tapStartTime = Date.now();
        _tapMoved = false;
    }, { passive: true });

    container.addEventListener('touchmove', function(e) {
        if (_tapMoved) return;
        if (e.touches.length !== 1) { _tapMoved = true; return; }
        const t = e.touches[0];
        const dx = t.clientX - _tapStartX;
        const dy = t.clientY - _tapStartY;
        if (dx * dx + dy * dy > TAP_MOVE_THRESHOLD * TAP_MOVE_THRESHOLD) {
            _tapMoved = true;
        }
    }, { passive: true });

    container.addEventListener('touchend', function(e) {
        if (_tapMoved) return;
        if (e.changedTouches.length !== 1) return;
        const elapsed = Date.now() - _tapStartTime;
        if (elapsed > TAP_MAX_DURATION) return; // long-press, not a tap
        // Ignore if selection is active
        if (term.hasSelection && term.hasSelection()) return;
        const t = e.changedTouches[0];
        const pos = clickToBufferPos(t.clientX, t.clientY);
        if (!pos) return;
        showJumpIndicator(t.clientX, t.clientY);
        moveCursorTo(pos.col, pos.row);
    }, { passive: true });

    log('[cursor-jump] Click-to-cursor initialized');
})();

// ── TTS control button handlers (in status bar) ──
const _prevBtn = document.getElementById('fs-player-prev');
const _pauseBtn = document.getElementById('fs-player-pause');
const _nextBtn = document.getElementById('fs-player-next');
const _stopBtn = document.getElementById('fs-player-stop');
let _prevDebounceTimer = null;
HC._prevCursor = -1; // index into HC._ttsHistory, set on first press
HC._autoAdvanceHistory = false; // only auto-advance on next, not prev
if (_prevBtn) {
    _prevBtn.addEventListener('click', async () => {
        HC._autoAdvanceHistory = false;
        // Refresh from daemon to pick up daemon-generated messages
        if (HC._refreshHistoryFromDaemon) await HC._refreshHistoryFromDaemon();
        if (HC._ttsHistory.length === 0) return;
        HC.gaplessPlayer.cleanup();
        if (HC._prevCursor < 0) {
            HC._prevCursor = HC._ttsHistory.length - 1;
        } else {
            HC._prevCursor = Math.max(0, HC._prevCursor - 1);
        }
        const entry = HC._ttsHistory[HC._prevCursor];
        HC.setVoiceStatus('info', 'History ' + (HC._prevCursor + 1) + '/' + HC._ttsHistory.length + ': ' + (entry.textPreview || '').substring(0, 30) + '...');
        HC._updateHistoryControls();
        if (_prevDebounceTimer) clearTimeout(_prevDebounceTimer);
        _prevDebounceTimer = setTimeout(() => {
            _prevDebounceTimer = null;
            HC._replayHistoryEntry(entry);
        }, 600);
    });
}
let _nextDebounceTimer = null;
if (_nextBtn) {
    _nextBtn.addEventListener('click', async () => {
        HC._autoAdvanceHistory = true;
        if (HC._ttsHistory.length === 0) return;
        HC.gaplessPlayer.cleanup();
        if (HC._prevCursor < 0) return;
        HC._prevCursor = Math.min(HC._ttsHistory.length - 1, HC._prevCursor + 1);
        const entry = HC._ttsHistory[HC._prevCursor];
        HC.setVoiceStatus('info', 'History ' + (HC._prevCursor + 1) + '/' + HC._ttsHistory.length + ': ' + (entry.textPreview || '').substring(0, 30) + '...');
        HC._updateHistoryControls();
        if (_nextDebounceTimer) clearTimeout(_nextDebounceTimer);
        _nextDebounceTimer = setTimeout(() => {
            _nextDebounceTimer = null;
            HC._replayHistoryEntry(entry);
        }, 600);
    });
}
if (_stopBtn) {
    _stopBtn.addEventListener('click', () => {
        HC.stopTts();
        HC._prevCursor = -1;
        HC._autoAdvanceHistory = false;
        if (_prevDebounceTimer) { clearTimeout(_prevDebounceTimer); _prevDebounceTimer = null; }
        HC.setVoiceStatus('', '');
        HC.updateMiniPlayer(false, '', '', { collapse: true });
        HC.updatePlaybackStrip(0);
        HC._updateHistoryControls();
        log('[History] Stopped all playback');
    });
}
if (_pauseBtn) {
    _pauseBtn.addEventListener('click', () => {
        if (HC.gaplessPlayer.isPaused()) {
            HC.gaplessPlayer.resume();
            _pauseBtn.innerHTML = '&#x23F8;&#xFE0E;';
            _pauseBtn.title = 'Pause';
            log('[History] Resumed playback');
        } else {
            HC.gaplessPlayer.pause();
            _pauseBtn.innerHTML = '&#x23F5;&#xFE0E;';
            _pauseBtn.title = 'Resume';
            log('[History] Paused playback');
        }
    });
}

// ── Volume Control (custom touch slider popup) ──────────────────────
const _volTrack = document.getElementById('vol-track');
const _volFill = document.getElementById('vol-fill');
const _volThumb = document.getElementById('vol-thumb');
const _volIcon = document.getElementById('vol-icon');
const _volPopup = document.getElementById('vol-popup');
const _volPct = document.getElementById('vol-pct');
let _volValue = 100;
let _savedVolume = 100;
let _volPopupTimer = null;
let _volDragging = false;
// Restore from prefs cookie
const storedVol = HC._prefs && HC._prefs.volume !== undefined ? String(HC._prefs.volume) : null;
if (storedVol !== null) {
    const v = parseInt(storedVol, 10);
    if (v >= 0 && v <= 100) {
        _volValue = v;
        _savedVolume = v > 0 ? v : _savedVolume;
    }
}
function _setVolUI(pct) {
    pct = Math.max(0, Math.min(100, Math.round(pct)));
    _volValue = pct;
    // Status bar volume
    if (_volFill) _volFill.style.height = pct + '%';
    if (_volThumb) _volThumb.style.bottom = pct + '%';
    if (_volPct) _volPct.textContent = pct + '%';
    if (_volIcon) _volIcon.textContent = pct === 0 ? '\uD83D\uDD07' : pct < 50 ? '\uD83D\uDD09' : '\uD83D\uDD0A';
    // Floating volume (sync)
    var fsFill = document.getElementById('fs-vol-fill');
    var fsThumb = document.getElementById('fs-vol-thumb');
    var fsPct = document.getElementById('fs-vol-pct');
    var fsBtn = document.getElementById('fs-vol-btn');
    if (fsFill) fsFill.style.height = pct + '%';
    if (fsThumb) fsThumb.style.bottom = pct + '%';
    if (fsPct) fsPct.textContent = pct + '%';
    if (fsBtn) fsBtn.textContent = pct === 0 ? '\uD83D\uDD07' : pct < 50 ? '\uD83D\uDD09' : '\uD83D\uDD0A';
}
function _applyVolume(pct) {
    _setVolUI(pct);
    // Exponential curve: human hearing is logarithmic, so linear gain sounds wrong.
    // (pct/100)^3 maps slider to perceptual loudness — 50% slider ≈ 12.5% gain.
    var gain = pct === 0 ? 0 : Math.pow(pct / 100, 3);
    try {
        if (gaplessPlayer && HC.gaplessPlayer._gainNode) {
            const g = HC.gaplessPlayer._gainNode.gain;
            if (HC.gaplessPlayer._ctx && g.setValueAtTime) {
                g.setValueAtTime(gain, HC.gaplessPlayer._ctx.currentTime);
            } else {
                g.value = gain;
            }
        }
    } catch(e) {}
    if (HC.saveLayoutSetting) HC.saveLayoutSetting('volume', pct);
    // Notify daemon of volume change
    if (HC.ws && HC.ws.readyState === WebSocket.OPEN) {
        HC.ws.send(JSON.stringify({ type: 'volume_change', volume: pct }));
    }
}
function _volFromTouch(clientY) {
    if (!_volTrack) return _volValue;
    const rect = _volTrack.getBoundingClientRect();
    const pct = ((rect.bottom - clientY) / rect.height) * 100;
    return Math.max(0, Math.min(100, Math.round(pct)));
}
function _showVolPopup() {
    if (_volPopup) _volPopup.classList.add('open');
    if (_volPopupTimer) clearTimeout(_volPopupTimer);
    _volPopupTimer = setTimeout(() => {
        if (!_volDragging && _volPopup) _volPopup.classList.remove('open');
    }, 3000);
}
function _hideVolPopup() {
    if (_volPopupTimer) clearTimeout(_volPopupTimer);
    if (_volPopup) _volPopup.classList.remove('open');
}
// Custom touch slider on the track/thumb
if (_volTrack) {
    _volTrack.addEventListener('touchstart', (e) => {
        e.preventDefault();
        e.stopPropagation();
        _volDragging = true;
        if (_volPopupTimer) clearTimeout(_volPopupTimer);
        const v = _volFromTouch(e.touches[0].clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    }, { passive: false });
    document.addEventListener('touchmove', (e) => {
        if (!_volDragging) return;
        e.preventDefault();
        const v = _volFromTouch(e.touches[0].clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    }, { passive: false });
    document.addEventListener('touchend', () => {
        if (!_volDragging) return;
        _volDragging = false;
        _volPopupTimer = setTimeout(() => _hideVolPopup(), 2000);
    });
    // Desktop mouse support
    _volTrack.addEventListener('mousedown', (e) => {
        _volDragging = true;
        const v = _volFromTouch(e.clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    });
    document.addEventListener('mousemove', (e) => {
        if (!_volDragging) return;
        const v = _volFromTouch(e.clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    });
    document.addEventListener('mouseup', () => {
        if (!_volDragging) return;
        _volDragging = false;
    });
}
// Set initial UI
_setVolUI(_volValue);
if (_volIcon) {
    _volIcon.addEventListener('click', (e) => {
        e.stopPropagation();
        if (_volPopup && _volPopup.classList.contains('open')) {
            // Popup open — toggle mute
            if (_volValue > 0) {
                _savedVolume = _volValue;
                _applyVolume(0);
            } else {
                _applyVolume(_savedVolume || 100);
            }
            if (_volPopupTimer) clearTimeout(_volPopupTimer);
            _volPopupTimer = setTimeout(() => _hideVolPopup(), 3000);
        } else {
            _showVolPopup();
        }
    });
}
// Close popups when tapping elsewhere
document.addEventListener('click', (e) => {
    if (_volPopup && _volPopup.classList.contains('open')) {
        if (!e.target.closest('.volume-control')) {
            _hideVolPopup();
        }
    }
    var fsVolPopup = document.getElementById('fs-vol-popup');
    if (fsVolPopup && fsVolPopup.classList.contains('open')) {
        if (!e.target.closest('#fs-vol-btn') && !e.target.closest('#fs-vol-popup')) {
            fsVolPopup.classList.remove('open');
        }
    }
    var fsFontPopup = document.getElementById('fs-font-popup');
    if (fsFontPopup && fsFontPopup.classList.contains('open')) {
        if (!e.target.closest('#fs-font-btn') && !e.target.closest('#fs-font-popup')) {
            fsFontPopup.classList.remove('open');
        }
    }
    var fsPlayerPopup = document.getElementById('fs-player-popup');
    if (fsPlayerPopup && fsPlayerPopup.classList.contains('open')) {
        if (!e.target.closest('#fs-player-btn') && !e.target.closest('#fs-player-popup')) {
            _playerPopupOpen = false;
            fsPlayerPopup.classList.remove('open');
        }
    }
});

// ── Floating volume slider (same logic as status bar) ──
(function() {
    var fsTrack = document.getElementById('fs-vol-track');
    if (!fsTrack) return;
    var fsDragging = false;
    function fsVolFromTouch(clientY) {
        var rect = fsTrack.getBoundingClientRect();
        var pct = ((rect.bottom - clientY) / rect.height) * 100;
        return Math.max(0, Math.min(100, Math.round(pct)));
    }
    fsTrack.addEventListener('touchstart', function(e) {
        e.preventDefault(); e.stopPropagation();
        fsDragging = true;
        var v = fsVolFromTouch(e.touches[0].clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    }, { passive: false });
    document.addEventListener('touchmove', function(e) {
        if (!fsDragging) return;
        e.preventDefault();
        var v = fsVolFromTouch(e.touches[0].clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    }, { passive: false });
    document.addEventListener('touchend', function() {
        fsDragging = false;
    });
    fsTrack.addEventListener('mousedown', function(e) {
        fsDragging = true;
        var v = fsVolFromTouch(e.clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    });
    document.addEventListener('mousemove', function(e) {
        if (!fsDragging) return;
        var v = fsVolFromTouch(e.clientY);
        _savedVolume = v > 0 ? v : _savedVolume;
        _applyVolume(v);
    });
    document.addEventListener('mouseup', function() { fsDragging = false; });
})();

// ── Font size slider (same pattern as volume) ──
(function() {
    var fsTrack = document.getElementById('fs-font-track');
    if (!fsTrack) return;
    var fsDragging = false;
    function fsFontFromTouch(clientY) {
        var rect = fsTrack.getBoundingClientRect();
        var pct = ((rect.bottom - clientY) / rect.height) * 100;
        pct = Math.max(0, Math.min(100, pct));
        return Math.round(_fontMin + (pct / 100) * (_fontMax - _fontMin));
    }
    fsTrack.addEventListener('touchstart', function(e) {
        e.preventDefault(); e.stopPropagation();
        fsDragging = true;
        _applyFontSize(fsFontFromTouch(e.touches[0].clientY));
    }, { passive: false });
    document.addEventListener('touchmove', function(e) {
        if (!fsDragging) return;
        e.preventDefault();
        _applyFontSize(fsFontFromTouch(e.touches[0].clientY));
    }, { passive: false });
    document.addEventListener('touchend', function() { fsDragging = false; });
    fsTrack.addEventListener('mousedown', function(e) {
        fsDragging = true;
        _applyFontSize(fsFontFromTouch(e.clientY));
    });
    document.addEventListener('mousemove', function(e) {
        if (!fsDragging) return;
        _applyFontSize(fsFontFromTouch(e.clientY));
    });
    document.addEventListener('mouseup', function() { fsDragging = false; });
})();


// Expose to HC namespace
HC.term = term;
HC.fitTerminal = fitTerminal;
HC.sendResize = sendResize;
HC.scrollToBottom = scrollToBottom;
HC.fitAddon = fitAddon;
HC.textInput = textInput;
