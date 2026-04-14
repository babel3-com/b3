/* layout.js — Babel3 configurable layout system (open-source)
 *
 * Manages fullscreen layout toggles and UI preferences.
 * Persists to a single agent+device-scoped cookie: 'b3-{agentId}' (JSON, 1yr).
 * Replaces EC2 user-settings load/save and individual sessionStorage keys.
 *
 * Depends on: core.js (HC namespace, window.__HC__)
 * Must load BEFORE: terminal.js, gpu.js, ws.js, voice-record.js, voice-play.js, rtc.js
 * (those files read HC._prefs at load time)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// ── Preference Cookie ──
// Key uses window.__HC__.agentId (HC.config not set yet at file load time).
function _prefCookieKey() {
    var agentId = (window.__HC__ && window.__HC__.agentId) || '';
    return 'b3-' + agentId;
}

function _readPrefCookie() {
    var key = _prefCookieKey();
    var escaped = key.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    var match = document.cookie.match(new RegExp('(?:^|;\\s*)' + escaped + '=([^;]*)'));
    if (!match) return {};
    try { return JSON.parse(decodeURIComponent(match[1])); } catch(e) { return {}; }
}

function _writePrefCookie(prefs) {
    var key = _prefCookieKey();
    var val = encodeURIComponent(JSON.stringify(prefs));
    document.cookie = key + '=' + val + '; path=/; max-age=31536000; SameSite=Lax';
}

// ── Defaults ──
var LAYOUT_DEFAULTS = {
    // Visibility toggles
    show_mic: true,
    show_text_input: false,
    show_upload: false,
    show_status_bar: false,
    show_badge: true,
    show_led: true,
    show_term_keys: true,
    show_scroll_btn: true,
    show_vol_btn: false,
    show_font_btn: false,
    show_player: false,
    show_files: false,
    show_info: false,
    show_toast: true,
    show_mic_level: true,
    show_transport_btn: false,
    show_gpu_toggle: false,
    show_reflow_toggle: false,
    show_refit: true,
    show_audio_devices: false,
    // Status bar content toggles
    status_geometry: false,
    status_browser: true,
    status_ios: false,
    status_server: false,
    status_daemon: true,
    status_gpu: false,
    status_credits: true,
    // UI preferences (shared with terminal.js, gpu.js, ws.js, etc.)
    volume: 100,
    gpu_mode: 'local+runpod',
    reflow_enabled: false,
    font_size: 13,
    speech_threshold: 20,
    // Reserved for future drag-and-drop
    positions: {},
};

// HC._prefs is populated synchronously from cookie at file load time.
// All files that load after layout.js read from HC._prefs directly.
HC._prefs = Object.assign({}, LAYOUT_DEFAULTS, _readPrefCookie());

var _currentLayout = HC._prefs;

// ── Pref Write ──
var _prefSaveTimer = null;
function _savePref(key, value) {
    HC._prefs[key] = value;
    clearTimeout(_prefSaveTimer);
    _prefSaveTimer = setTimeout(function() {
        _writePrefCookie(HC._prefs);
        log('[Layout] Prefs saved');
    }, 300);
}

// ── Layout Application ──
function applyLayout(layout) {
    var body = document.body;
    for (var key in layout) {
        if (typeof layout[key] === 'boolean') {
            var cls = 'fs-' + key.replace(/_/g, '-');
            body.classList.toggle(cls, layout[key]);
        }
    }
    body.classList.toggle('hide-led', !layout.show_led);
    var iosToggle = document.getElementById('layout-status-ios-wrapper');
    if (iosToggle && window.NativeBridge && window.NativeBridge.isAvailable) {
        iosToggle.style.display = '';
    }
    requestAnimationFrame(updateBottomOffset);
}

function updateBottomOffset() {
    var h = 0;
    var tk = document.querySelector('.term-keys');
    var sb = document.querySelector('.terminal-status');
    var ml = document.getElementById('mic-level-strip');
    if (tk && getComputedStyle(tk).display !== 'none') h += tk.offsetHeight + 8;
    if (sb && getComputedStyle(sb).display !== 'none' && sb.offsetHeight > 0) h += sb.offsetHeight;
    if (ml && getComputedStyle(ml).display !== 'none' && ml.offsetHeight > 0) h += ml.offsetHeight;
    h += 15;
    h = Math.max(h, 15);
    document.documentElement.style.setProperty('--bottom-offset', h + 'px');
}

function syncLayoutToggles(layout) {
    var section = document.getElementById('drawer-layout');
    if (!section) return;
    section.querySelectorAll('[data-layout]').forEach(function(el) {
        var key = el.dataset.layout;
        if (el.type === 'checkbox') el.checked = !!layout[key];
        else if (el.tagName === 'SELECT') el.value = layout[key] || '';
    });
}

function saveLayoutSetting(key, value) {
    _currentLayout[key] = value;
    _savePref(key, value);
    applyLayout(_currentLayout);
    if (key.indexOf('status_') === 0 && HC.refreshStatusBar) {
        HC.refreshStatusBar();
    }
}

// Apply layout from cookie immediately (synchronous — no network round-trip).
applyLayout(_currentLayout);
syncLayoutToggles(_currentLayout);

// Event delegation on layout section
var layoutSection = document.getElementById('drawer-layout');
if (layoutSection) {
    layoutSection.addEventListener('change', function(e) {
        var key = e.target.dataset && e.target.dataset.layout;
        if (!key) return;
        var value = e.target.type === 'checkbox' ? e.target.checked : e.target.value;
        saveLayoutSetting(key, value);
    });
}

// Expose to HC namespace
HC.LAYOUT_DEFAULTS = LAYOUT_DEFAULTS;
HC.applyLayout = applyLayout;
HC.syncLayoutToggles = syncLayoutToggles;
HC.saveLayoutSetting = saveLayoutSetting;
HC.updateBottomOffset = updateBottomOffset;
HC.getCurrentLayout = function() { return _currentLayout; };
HC.setCurrentLayout = function(l) { _currentLayout = l; };
// loadLayout kept as no-op for callers that still invoke it
HC.loadLayout = function() {};
