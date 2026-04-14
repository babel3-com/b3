/* boot.js — Babel3 dashboard boot sequence (open-source)
 *
 * EC2 signaling, agent settings persistence, voice picker,
 * version fetch, account state, boot initialization.
 *
 * Depends on: core.js (HC namespace), all other modules
 * Must be loaded last.
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// ── Signaling: Ask EC2 for daemon's tunnel URL ──
var _hostedResumeRequested = false;
async function resolveAndConnect() {
    // Fast path: if we already have the tunnel URL, reconnect directly.
    // Avoids EC2 fetch on cellular (which can fail/timeout during tunnel flaps).
    // After 3 consecutive rapid failures, clear cache to force fresh EC2 lookup
    // (handles daemon restart with new relay URL).
    if (HC._daemonBase && _wsConsecutiveFailures < 3) {
        HC.connectWS();
        return;
    }
    if (_wsConsecutiveFailures >= 3) {
        log('[WS] 3+ consecutive failures — clearing cached tunnel URL, re-fetching from EC2');
        HC._daemonBase = null;
    }
    HC.setTerminalStatus('Looking up agent...');
    document.getElementById('status-dot').className = 'status-dot connecting';

    try {
        var resp = await fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentName + '/connect');
        if (!resp.ok) {
            HC.setTerminalStatus('Agent not found');
            document.getElementById('status-dot').className = 'status-dot offline';
            setTimeout(resolveAndConnect, 10000);
            return;
        }

        var info = await resp.json();
        if (!info.online) {
            if (HC.config.hostedSessionId) {
                // Hosted session — auto-resume the daemon
                HC.setTerminalStatus('Resuming session...');
                document.getElementById('status-dot').className = 'status-dot connecting';
                if (!_hostedResumeRequested) {
                    _hostedResumeRequested = true;
                    HC.term.reset();
                    HC.term.write('\r\n');
                    HC.term.write('  \x1b[1;36mReconnecting to your session...\x1b[0m\r\n');
                    HC.term.write('  \x1b[90mYour files are still here. Starting daemon...\x1b[0m\r\n');
                    try {
                        await fetch(HC.EC2_BASE + '/api/hosted/' + HC.config.hostedSessionId + '/resume', { method: 'POST' });
                    } catch(e) { log('[Resume] Failed: ' + e.message); }
                }
                setTimeout(resolveAndConnect, 5000);
                return;
            }
            HC.setTerminalStatus('Agent offline — will auto-connect when it starts');
            document.getElementById('status-dot').className = 'status-dot offline';
            HC.term.reset();
            HC.term.write('\r\n');
            HC.term.write('  \x1b[1;33mAgent is registered but not running.\x1b[0m\r\n');
            HC.term.write('\r\n');
            HC.term.write('  To start this agent, run on your machine:\r\n');
            HC.term.write('\r\n');
            HC.term.write('    \x1b[1;36mb3 start\x1b[0m\r\n');
            HC.term.write('\r\n');
            HC.term.write('  \x1b[90mThis page will auto-connect once the agent is running.\x1b[0m\r\n');
            HC.term.write('  \x1b[90mRetrying every 5 seconds...\x1b[0m\r\n');
            setTimeout(resolveAndConnect, 5000);
            return;
        }

        // Agent is online. EC2 encrypted proxy is primary; direct WebSocket is fallback.
        if (HC.DaemonWS && HC.config.agentId) {
            // EC2 proxy path (primary) — E2E encrypted EC2 proxy.
            // Direct WebSocket stored as fallback: ws.js switches to it if EC2 fails to open.
            // Requires Web Crypto x25519: Chrome 113+, Safari 17.4+, Firefox 130+.
            log('[Signaling] EC2 encrypted proxy (primary)');
            HC.setTerminalStatus('Connecting to daemon...');
            // Initialize session ID before constructing DaemonWS — it reads _daemonSessionId
            // at construction time and embeds it in the handshake JSON.
            if (!HC._daemonSessionId) {
                HC._daemonSessionId = sessionStorage.getItem('b3-daemon-session-id');
                if (!HC._daemonSessionId) {
                    HC._daemonSessionId = String(Math.floor(Math.random() * 0x7FFFFFFF));
                    sessionStorage.setItem('b3-daemon-session-id', HC._daemonSessionId);
                }
            }
            HC._pendingWs = new HC.DaemonWS(
                HC.config.agentId, HC._daemonSessionId, HC.EC2_BASE, HC.config.daemonToken);
            HC.connectWS();
        } else if (info.tunnel_url) {
            // Legacy tunnel path — no DaemonWS support. Should not happen on current daemons.
            log('[Signaling] No DaemonWS — cannot connect (tunnel path removed)');
            HC.setTerminalStatus('Daemon too old — please run b3 update');
            if (HC.daemonRtcConnect) {
                HC.daemonRtcConnect().catch(function(e) {
                    log('[BOOT] Daemon WebRTC failed (using tunnel): ' + (e.message || e));
                });
            }
            HC.daemonFetch('/api/auth-cookie').catch(function() {});
        } else {
            // Online but no connection path yet — retry shortly
            HC.setTerminalStatus('Agent starting...');
            setTimeout(resolveAndConnect, 3000);
            return;
        }
        // These go to EC2 server directly — safe on both paths
        if (HC.fetchCredits) HC.fetchCredits();
        HC.fetchGpuConfig();
        HC.fetchGarbageEmbeddings();
    } catch(e) {
        HC.setTerminalStatus('Network error — retrying...');
        setTimeout(resolveAndConnect, 5000);
    }
}

// ── Toast Notification ──

var _fsToastTimer = null;
function flashFsToast(text) {
    if (!document.body.classList.contains('hc-fullscreen') || !text) return;
    var toast = document.getElementById('fs-toast');
    if (!toast) return;
    toast.textContent = text;
    toast.style.display = 'block';
    toast.style.opacity = '1';
    clearTimeout(_fsToastTimer);
    _fsToastTimer = setTimeout(function() {
        toast.style.opacity = '0';
        setTimeout(function() { toast.style.display = 'none'; }, 300);
    }, 2000);
}
HC.flashFsToast = flashFsToast;

HC.setTerminalStatus = function(text, silent) {
    var el = document.getElementById('terminal-status-text');
    if (el) el.textContent = text;
    var dst = document.getElementById('drawer-status-text');
    if (dst) dst.textContent = text;
    // Only show toast for non-routine updates (connection changes, errors)
    if (!silent && !/^\d+x\d+/.test(text)) flashFsToast(text);
};
// Silent status bar update — no toast
HC.refreshStatusBar = function() {
    if (HC.fullStatus) HC.setTerminalStatus(HC.fullStatus(), true);
};

// ── Agent Settings persistence (EC2 as source of truth) ──
var sttSelect = document.getElementById('stt-preset');
var diarizeCheckbox = document.getElementById('stt-diarize');
var voiceReportCheckbox = document.getElementById('voice-report-toggle');
HC._voiceReportEnabled = false; // default off
var forceLowercaseCheckbox = document.getElementById('force-lowercase');
HC._forceLowercase = true; // default on (backward compat)
var agentSettingsHeaders = { 'Authorization': 'Bearer ' + HC.config.daemonToken, 'Content-Type': 'application/json' };

// Load settings from EC2
fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/settings', {
    headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
}).then(function(r) { return r.ok ? r.json() : ({}); }).then(function(settings) {
    if (settings.stt_preset && sttSelect) sttSelect.value = settings.stt_preset;
    if (diarizeCheckbox) diarizeCheckbox.checked = !!settings.diarize;
    HC._voiceReportEnabled = !!settings.voice_report;
    if (voiceReportCheckbox) voiceReportCheckbox.checked = HC._voiceReportEnabled;
    HC._forceLowercase = settings.force_lowercase !== false; // default true
    if (forceLowercaseCheckbox) forceLowercaseCheckbox.checked = HC._forceLowercase;
    log('[Settings] Loaded from EC2: ' + JSON.stringify(settings));
}).catch(function(e) {
    log('[Settings] Failed to load from EC2, using defaults: ' + e.message);
});

// Save on change (event delegation on #drawer-settings for robustness)
function saveAgentSettings(patch) {
    log('[Settings] Saving: ' + JSON.stringify(patch));
    fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/settings', {
        method: 'PUT',
        headers: agentSettingsHeaders,
        body: JSON.stringify(patch),
    }).then(function(r) { if (r.ok) log('[Settings] Saved OK'); else log('[Settings] Save HTTP ' + r.status); })
      .catch(function(e) { log('[Settings] Save failed: ' + e.message); });
}
var drawerSettings = document.getElementById('drawer-settings');
if (drawerSettings) {
    drawerSettings.addEventListener('change', function(e) {
        if (e.target.id === 'stt-preset') saveAgentSettings({ stt_preset: e.target.value });
        else if (e.target.id === 'stt-diarize') saveAgentSettings({ diarize: e.target.checked });
        else if (e.target.id === 'voice-report-toggle') {
            HC._voiceReportEnabled = e.target.checked;
            saveAgentSettings({ voice_report: e.target.checked });
        }
        else if (e.target.id === 'force-lowercase') {
            HC._forceLowercase = e.target.checked;
            saveAgentSettings({ force_lowercase: e.target.checked });
            // Notify daemon to refresh its cached setting
            if (HC.DAEMON_BASE) {
                fetch(HC.DAEMON_BASE + '/api/settings/refresh', {
                    method: 'POST',
                    headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
                }).catch(function() {});
            }
        }
    });
}

// Sync settings from EC2 when drawer opens (cross-browser sync)
window.onDrawerOpen = function() {
    var agentHeaders = { 'Authorization': 'Bearer ' + HC.config.daemonToken };
    fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/settings', {
        headers: agentHeaders,
    }).then(function(r) { return r.ok ? r.json() : {}; }).then(function(settings) {
        if (settings.stt_preset && sttSelect) sttSelect.value = settings.stt_preset;
        if (diarizeCheckbox) diarizeCheckbox.checked = !!settings.diarize;
        HC._voiceReportEnabled = !!settings.voice_report;
        if (voiceReportCheckbox) voiceReportCheckbox.checked = HC._voiceReportEnabled;
        HC._forceLowercase = settings.force_lowercase !== false;
        if (forceLowercaseCheckbox) forceLowercaseCheckbox.checked = HC._forceLowercase;
    }).catch(function() {});
    // Sync voice preference
    fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/voice-preference', {
        headers: agentHeaders,
    }).then(function(r) { return r.ok ? r.json() : {}; }).then(function(pref) {
        var vs = document.getElementById('voice-select');
        if (pref.voice && vs && vs.value !== pref.voice) {
            vs.value = pref.voice;
            log('[Settings] Voice synced from EC2: ' + pref.voice);
        }
    }).catch(function() {});
    // Sync layout toggles from cookie-based prefs (EC2 fetch removed — layout.js owns state)
    if (HC.syncLayoutToggles && HC._prefs) HC.syncLayoutToggles(HC._prefs);
};

// ── Drawer (shared) ──
window.CURRENT_AGENT = HC.config.agentName;
window.DRAWER_LOGOUT_URL = '/';

// DEPRECATED: Regular view is deprecated. Full-screen configurable view is the only supported mode.
// toggleFocusMode kept as no-op for any remaining callers.
window.toggleFocusMode = function() {};

// ── Voice Picker (fetches default + user voices from EC2) ──
async function loadVoices() {
    var select = document.getElementById('voice-select');
    if (!select) return;

    // Show a default immediately so the selector is never empty on page load.
    // loadVoices replaces this once the full list arrives from the server.
    if (select.options.length === 0) {
        var saved = localStorage.getItem('b3-voice-' + HC.config.agentId) || 'arabella-chase';
        var opt = document.createElement('option');
        opt.value = saved; opt.textContent = saved; opt.selected = true;
        select.appendChild(opt);
    }

    function populateSelect(voices, savedVoice) {
        select.innerHTML = '';
        voices.forEach(function(v) {
            var opt = document.createElement('option');
            opt.value = v; opt.textContent = v;
            if (v === savedVoice) opt.selected = true;
            select.appendChild(opt);
        });
    }

    try {
        // Fetch in parallel: default voices, user's custom voices, agent's saved preference
        var agentHeaders = { 'Authorization': 'Bearer ' + HC.config.daemonToken };
        var results = await Promise.all([
            fetch(HC.EC2_BASE + '/api/default-voices').catch(function() { return null; }),
            fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/user-voices', {
                headers: agentHeaders,
            }).catch(function() { return null; }),
            fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/voice-preference', {
                headers: agentHeaders,
            }).catch(function() { return null; }),
        ]);
        var defaultResp = results[0], userResp = results[1], prefResp = results[2];

        // Get saved voice preference (DB > localStorage > 'arabella-chase')
        // DB is authoritative — sync to localStorage so tts.js and other
        // code paths that read localStorage get the correct voice.
        var savedVoice = 'arabella-chase';
        if (prefResp && prefResp.ok) {
            var prefData = await prefResp.json();
            if (prefData.voice) {
                savedVoice = prefData.voice;
                localStorage.setItem('b3-voice-' + HC.config.agentId, savedVoice);
            }
        } else {
            savedVoice = localStorage.getItem('b3-voice-' + HC.config.agentId) || 'arabella-chase';
        }

        var defaults = [];
        if (defaultResp && defaultResp.ok) {
            var data = await defaultResp.json();
            defaults = data.voices || [];
        }
        if (defaults.length === 0) defaults = ['arabella-chase'];

        // Collect user's custom voices
        var custom = [];
        if (userResp && userResp.ok) {
            var udata = await userResp.json();
            custom = (udata.voices || []).map(function(v) { return v.name || v; });
        }

        // Merge: defaults first, then custom (deduped)
        var all = defaults.slice();
        custom.forEach(function(v) { if (all.indexOf(v) === -1) all.push(v); });
        populateSelect(all, savedVoice);
        log('[Voices] Loaded ' + defaults.length + ' default + ' + custom.length + ' custom voices');
    } catch(e) {
        log('[Voices] Failed to load voices: ' + e.message);
        select.innerHTML = '<option value="arabella-chase" selected>arabella-chase</option>';
    }
}
var voiceSelectEl = document.getElementById('voice-select');
if (voiceSelectEl) {
    voiceSelectEl.addEventListener('change', function(e) {
        var voice = e.target.value;
        localStorage.setItem('b3-voice-' + HC.config.agentId, voice);
        HC.fetchVoiceConditionals(voice);
        // Persist to DB
        fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/voice-preference', {
            method: 'PUT',
            credentials: 'include',
            headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken, 'Content-Type': 'application/json' },
            body: JSON.stringify({ voice: voice }),
        }).catch(function(err) { log('[Voices] Failed to save preference: ' + err.message); });
    });
}

// ── Fetch and log versions (JS, server, daemon) ──
HC._serverVersion = '';
HC._daemonVersion = '';

// JS version: read from our own script tag's ?v= parameter.
// This is the version the browser actually loaded — if the cache is stale,
// this will differ from the live server version.
HC._jsVersion = (function() {
    var scripts = document.querySelectorAll('script[src*="boot.js"]');
    for (var i = 0; i < scripts.length; i++) {
        var m = scripts[i].src.match(/[?&]v=([^&]+)/);
        if (m) return m[1];
    }
    return HC.config.version || '';
})();

// Server version (EC2 — live, not cached)
fetch('/version')
    .then(function(r) { return r.text(); })
    .then(function(version) {
        HC._serverVersion = version;
        console.log('%c Babel3 Server v' + version + ' ', 'background: #238636; color: white; font-weight: bold; padding: 4px 8px; border-radius: 3px;');
        console.log('%c Babel3 JS v' + HC._jsVersion + ' ', 'background: #8b5cf6; color: white; font-weight: bold; padding: 4px 8px; border-radius: 3px;');
        if (version !== HC._jsVersion) {
            console.warn('[Version] JS files are stale! JS=' + HC._jsVersion + ' Server=' + version + ' — hard refresh needed');
        }
        // Refresh status bar with server version (silent — no toast)
        HC.refreshStatusBar();
    })
    .catch(function(err) { console.error('[Version] Failed to fetch:', err); });

// Daemon version (local)
HC._fetchDaemonVersion = function() {
    if (!HC.hasDaemonConnection()) return;
    HC.daemonFetch('/api/diagnostics')
        .then(function(r) { return r.ok ? r.json() : null; })
        .then(function(data) {
            if (data && data.daemon_version) {
                HC._daemonVersion = data.daemon_version;
                console.log('%c Babel3 Daemon v' + data.daemon_version + ' ', 'background: #2f81f7; color: white; font-weight: bold; padding: 4px 8px; border-radius: 3px;');
                HC.refreshStatusBar();
            }
        })
        .catch(function() {});
};

// GPU version — populated from pool worker health responses
HC._gpuVersion = '';

// Status bar items — configurable via layout settings
// Default: show browser + geometry. User can toggle others.
HC._statusBarItems = null; // null = use layout settings

// Build version status string from configured items
HC.versionStatus = function() {
    var layout = typeof _currentLayout !== 'undefined' ? _currentLayout : {};
    var parts = [];

    // Individual version items controlled by layout
    var isIOS = window.NativeBridge && window.NativeBridge.isAvailable;

    if (layout.status_browser !== false) {
        parts.push('browser v' + (HC._jsVersion || '?'));
    }
    if (isIOS && layout.status_ios !== false) {
        parts.push('ios v' + (window.NativeBridge.version || '?'));
    }
    if (layout.status_server !== false) {
        parts.push('server v' + (HC._serverVersion || '?'));
    }
    if (layout.status_daemon !== false) {
        parts.push('daemon v' + (HC._daemonVersion || '?'));
    }
    if (layout.status_gpu === true && HC._gpuVersion) {
        parts.push('gpu v' + HC._gpuVersion.replace(/^v/, ''));
    }
    if (layout.status_credits === true && HC._creditBalance !== undefined) {
        parts.push(HC._creditBalance + ' credits');
    }

    return parts.join(' · ');
};

// Build full status bar string (geometry + versions + credits)
HC.fullStatus = function() {
    var layout = typeof _currentLayout !== 'undefined' ? _currentLayout : {};
    var geom = (layout.status_geometry !== false && HC.term) ? HC.term.cols + 'x' + HC.term.rows : '';
    var ver = HC.versionStatus ? HC.versionStatus() : '';
    if (geom && ver) return geom + ' ' + ver;
    return geom || ver;
};

// Credit balance tracking for status bar
HC._creditBalance = undefined;
HC._fetchCredits = function() {
    fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentName + '/credits', { credentials: 'include' })
        .then(function(r) { return r.ok ? r.json() : null; })
        .then(function(data) {
            if (data && data.balance !== undefined) {
                HC._creditBalance = data.balance;
                HC.refreshStatusBar();
            }
        })
        .catch(function() {});
};
// Fetch credits on load and every 30s
HC._fetchCredits();
setInterval(HC._fetchCredits, 30000);

// ── Account state (plan info for credit checks) ──
// Auth state UI (logout/signup) handled by shared window.loadDrawerAgents()
fetch(HC.EC2_BASE + '/api/auth/me', { credentials: 'include' })
    .then(function(r) { return r.ok ? r.json() : null; })
    .then(function(data) { if (data && HC._userState) HC._userState.plan = data.plan || 'free'; })
    .catch(function() {});

// ── Signup handler ──
window.doSignup = async function() {
    var emailEl = document.getElementById('signup-email');
    var passEl = document.getElementById('signup-password');
    var msgEl = document.getElementById('signup-msg');
    var email = (emailEl.value || '').trim();
    var password = passEl.value || '';
    if (!email || !email.includes('@')) { msgEl.textContent = 'Enter a valid email'; msgEl.style.color = 'var(--red)'; return; }
    if (password.length < 8) { msgEl.textContent = 'Password must be 8+ characters'; msgEl.style.color = 'var(--red)'; return; }
    msgEl.textContent = 'Creating account...'; msgEl.style.color = 'var(--text-dim)';
    try {
        var resp = await fetch(HC.EC2_BASE + '/api/auth/signup', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            credentials: 'include',
            body: JSON.stringify({ email: email, password: password }),
        });
        var data = await resp.json();
        if (data.ok) {
            msgEl.textContent = 'Account created!'; msgEl.style.color = 'var(--green)';
            window.loadDrawerAgents();
        } else {
            msgEl.textContent = data.error || 'Signup failed'; msgEl.style.color = 'var(--red)';
        }
    } catch(e) { msgEl.textContent = 'Network error'; msgEl.style.color = 'var(--red)'; }
};

// ── Boot ──
// Always fullscreen — regular view is deprecated
document.body.classList.add('hc-fullscreen');
requestAnimationFrame(function() { HC.fitTerminal(); });
setTimeout(function() { HC.fitTerminal(); }, 300);
try { if (HC.ensureSession) HC.ensureSession(); } catch(e) { console.error('[BOOT] ensureSession error:', e); }
try { loadVoices(); } catch(e) { console.error('[BOOT] loadVoices error:', e); }
try { resolveAndConnect(); } catch(e) { console.error('[BOOT] resolveAndConnect error:', e); }
