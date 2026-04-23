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
    // Ordered list of term-key seq names shown in the key row
    term_keys: ['esc','ctrl-c','ctrl-b','ctrl-o','left','right','up','down','enter'],
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

// ── Term Keys ──
function applyTermKeys(keys) {
    var container = document.querySelector('.term-keys');
    if (!container) return;
    var catalog = _fullCatalog();
    if (!catalog.length) return;
    // Inject user catalog raw bytes into TERM_KEY_MAP so _bindTermKey can send them
    if (HC.TERM_KEY_MAP) {
        (HC._userCatalog || []).forEach(function(entry) {
            if (entry.raw && !HC.TERM_KEY_MAP[entry.seq]) {
                HC.TERM_KEY_MAP[entry.seq] = entry.raw;
            }
        });
    }
    var bySeq = {};
    catalog.forEach(function(k) { bySeq[k.seq] = k; });
    container.innerHTML = '';
    (keys || []).forEach(function(seq) {
        var def = bySeq[seq];
        if (!def) return;
        var btn = document.createElement('button');
        btn.className = 'term-key' + (def.wide ? ' term-key-enter' : '');
        btn.dataset.seq = seq;
        btn.title = seq;
        btn.textContent = def.label;
        container.appendChild(btn);
        if (HC.bindTermKey) HC.bindTermKey(btn);
    });
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
    if (layout.term_keys && HC.TERM_KEY_CATALOG) applyTermKeys(layout.term_keys);
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
HC.applyTermKeys = applyTermKeys;
HC.syncLayoutToggles = syncLayoutToggles;
HC.saveLayoutSetting = saveLayoutSetting;
HC.updateBottomOffset = updateBottomOffset;
HC.getCurrentLayout = function() { return _currentLayout; };
HC.setCurrentLayout = function(l) { _currentLayout = l; };
HC.saveTermKeys = function(keys) {
    _currentLayout.term_keys = keys;
    _savePref('term_keys', keys);
    applyTermKeys(keys);
};
// loadLayout kept as no-op for callers that still invoke it
HC.loadLayout = function() {};

// ── User Catalog (EC2 user-settings, persists across devices) ──
// HC._userCatalog: array of {seq, label} objects added by the user.
// Merged with HC.TERM_KEY_CATALOG (built-in) for the pool.
HC._userCatalog = [];

function _saveUserCatalog(catalog) {
    HC._userCatalog = catalog;
    if (!HC.EC2_BASE || !HC.config) return;
    fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/user-settings', {
        method: 'PUT',
        headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken, 'Content-Type': 'application/json' },
        credentials: 'include',
        body: JSON.stringify({ term_key_catalog: catalog }),
    }).catch(function(e) { log('[Layout] Failed to save catalog: ' + e.message); });
}

HC.loadUserCatalog = function() {
    if (!HC.EC2_BASE || !HC.config) return Promise.resolve([]);
    return fetch(HC.EC2_BASE + '/api/agents/' + HC.config.agentId + '/user-settings', {
        headers: { 'Authorization': 'Bearer ' + HC.config.daemonToken },
        credentials: 'include',
    }).then(function(r) { return r.ok ? r.json() : {}; })
    .then(function(s) {
        if (Array.isArray(s.term_key_catalog)) {
            HC._userCatalog = s.term_key_catalog;
            // Inject raw bytes into TERM_KEY_MAP so buttons can send them
            if (HC.TERM_KEY_MAP) {
                HC._userCatalog.forEach(function(entry) {
                    if (entry.raw && !HC.TERM_KEY_MAP[entry.seq]) {
                        HC.TERM_KEY_MAP[entry.seq] = entry.raw;
                    }
                });
            }
        }
        return HC._userCatalog;
    }).catch(function() { return []; });
};

// Returns merged catalog: built-in + user additions (deduped by seq)
function _fullCatalog() {
    var catalog = (HC.TERM_KEY_CATALOG || []).slice();
    (HC._userCatalog || []).forEach(function(entry) {
        if (!catalog.find(function(k) { return k.seq === entry.seq; })) {
            catalog.push({ seq: entry.seq, label: entry.label, wide: false, _user: true });
        }
    });
    return catalog;
}

// ── Term Key Picker (drawer UI) ──
function _renderTermKeyPicker() {
    var picker = document.getElementById('term-key-picker');
    if (!picker) return;

    var catalog = _fullCatalog();
    var active = (_currentLayout.term_keys || LAYOUT_DEFAULTS.term_keys).slice();

    function saveActive() {
        HC.saveTermKeys(active.slice());
        _renderTermKeyPicker();
    }

    picker.innerHTML = '';

    // ── Active row ──
    var activeWrap = document.createElement('div');
    activeWrap.className = 'tkp-active';
    activeWrap.title = 'Tap a key to remove it from the row';
    active.forEach(function(seq) {
        var def = catalog.find(function(k) { return k.seq === seq; });
        if (!def) return;
        var chip = document.createElement('button');
        chip.className = 'tkp-chip tkp-chip-active';
        chip.textContent = def.label;
        chip.title = 'Remove ' + seq;
        chip.addEventListener('click', function(e) {
            e.preventDefault();
            active = active.filter(function(s) { return s !== seq; });
            saveActive();
        });
        activeWrap.appendChild(chip);
    });
    if (active.length === 0) {
        var empty = document.createElement('span');
        empty.className = 'tkp-empty';
        empty.textContent = 'No keys — add from below';
        activeWrap.appendChild(empty);
    }

    // ── Pool (user-added entries only — built-ins are not shown) ──
    var poolWrap = document.createElement('div');
    poolWrap.className = 'tkp-pool';
    poolWrap.title = 'Tap a key to add it to the row';
    catalog.forEach(function(def) {
        if (!def._user) return;
        if (active.indexOf(def.seq) !== -1) return;
        var chip = document.createElement('button');
        chip.className = 'tkp-chip tkp-chip-pool' + (def._user ? ' tkp-chip-user' : '');
        chip.textContent = def.label;
        chip.title = 'Add ' + def.seq;

        if (def._user) {
            // User-added keys: long-press triggers inline "Remove?" confirmation chip.
            // Two-tap pattern — safe on iOS WKWebView where confirm() returns true immediately.
            var _pressTimer = null;
            var _confirming = false;
            chip.addEventListener('pointerdown', function() {
                _pressTimer = setTimeout(function() {
                    _pressTimer = null;
                    _confirming = true;
                    chip.textContent = 'Remove?';
                    chip.classList.add('tkp-chip-danger');
                    setTimeout(function() {
                        if (_confirming && document.body.contains(chip)) {
                            _confirming = false;
                            chip.textContent = def.label;
                            chip.classList.remove('tkp-chip-danger');
                        }
                    }, 2500);
                }, 700);
            });
            chip.addEventListener('pointerup', function() { clearTimeout(_pressTimer); });
            chip.addEventListener('pointerleave', function() { clearTimeout(_pressTimer); });
            chip.addEventListener('click', function(e) {
                if (_confirming) {
                    e.stopImmediatePropagation();
                    _confirming = false;
                    HC._userCatalog = HC._userCatalog.filter(function(k) { return k.seq !== def.seq; });
                    _saveUserCatalog(HC._userCatalog);
                    active = active.filter(function(s) { return s !== def.seq; });
                    saveActive();
                }
            });
        }

        chip.addEventListener('click', function(e) {
            e.preventDefault();
            active.push(def.seq);
            saveActive();
        });
        poolWrap.appendChild(chip);
    });

    // ── Add custom key button ──
    var addBtn = document.createElement('button');
    addBtn.className = 'tkp-add-btn';
    addBtn.textContent = '+ Add key';
    addBtn.title = 'Capture a new key combination';
    addBtn.addEventListener('click', function(e) {
        e.preventDefault();
        _openKeyCapture();
    });

    var bottomRow = document.createElement('div');
    bottomRow.className = 'tkp-bottom-row';
    bottomRow.appendChild(addBtn);

    var resetBtn = document.createElement('button');
    resetBtn.className = 'tkp-reset-btn';
    resetBtn.textContent = 'Reset to defaults';
    resetBtn.addEventListener('click', function(e) {
        e.preventDefault();
        HC.saveTermKeys(LAYOUT_DEFAULTS.term_keys.slice());
        renderTermKeyPicker();
    });
    bottomRow.appendChild(resetBtn);

    picker.appendChild(activeWrap);
    picker.appendChild(poolWrap);
    picker.appendChild(bottomRow);
}

// ── Key combination → {seq, label, raw} ──
// BASE_KEYS: all selectable base keys with their plain (no-modifier) raw bytes.
// modifier flags (ctrl, shift, alt) transform raw per xterm modifier math.
var _BASE_KEYS = [
    // Special
    { id: 'esc',       name: 'Escape',    group: 'Special',    plain: '\x1b' },
    { id: 'enter',     name: 'Enter',     group: 'Special',    plain: '\r' },
    { id: 'tab',       name: 'Tab',       group: 'Special',    plain: '\t' },
    { id: 'backspace', name: 'Backspace', group: 'Special',    plain: '\x7f' },
    { id: 'delete',    name: 'Delete',    group: 'Special',    plain: '\x1b[3~' },
    { id: 'space',     name: 'Space',     group: 'Special',    plain: ' ' },
    // Navigation
    { id: 'up',        name: '▲ Up',      group: 'Navigation', plain: '\x1b[A',  csiLetter: 'A' },
    { id: 'down',      name: '▼ Down',    group: 'Navigation', plain: '\x1b[B',  csiLetter: 'B' },
    { id: 'right',     name: '▶ Right',   group: 'Navigation', plain: '\x1b[C',  csiLetter: 'C' },
    { id: 'left',      name: '◀ Left',    group: 'Navigation', plain: '\x1b[D',  csiLetter: 'D' },
    { id: 'home',      name: 'Home',      group: 'Navigation', plain: '\x1b[H',  csiLetter: 'H' },
    { id: 'end',       name: 'End',       group: 'Navigation', plain: '\x1b[F',  csiLetter: 'F' },
    { id: 'pageup',    name: 'Page Up',   group: 'Navigation', plain: '\x1b[5~', csiTilde: '5' },
    { id: 'pagedown',  name: 'Page Down', group: 'Navigation', plain: '\x1b[6~', csiTilde: '6' },
    // Function
    { id: 'f1',  name: 'F1',  group: 'Function', plain: '\x1bOP',   csiSS3: 'P' },
    { id: 'f2',  name: 'F2',  group: 'Function', plain: '\x1bOQ',   csiSS3: 'Q' },
    { id: 'f3',  name: 'F3',  group: 'Function', plain: '\x1bOR',   csiSS3: 'R' },
    { id: 'f4',  name: 'F4',  group: 'Function', plain: '\x1bOS',   csiSS3: 'S' },
    { id: 'f5',  name: 'F5',  group: 'Function', plain: '\x1b[15~', csiTilde: '15' },
    { id: 'f6',  name: 'F6',  group: 'Function', plain: '\x1b[17~', csiTilde: '17' },
    { id: 'f7',  name: 'F7',  group: 'Function', plain: '\x1b[18~', csiTilde: '18' },
    { id: 'f8',  name: 'F8',  group: 'Function', plain: '\x1b[19~', csiTilde: '19' },
    { id: 'f9',  name: 'F9',  group: 'Function', plain: '\x1b[20~', csiTilde: '20' },
    { id: 'f10', name: 'F10', group: 'Function', plain: '\x1b[21~', csiTilde: '21' },
    { id: 'f11', name: 'F11', group: 'Function', plain: '\x1b[23~', csiTilde: '23' },
    { id: 'f12', name: 'F12', group: 'Function', plain: '\x1b[24~', csiTilde: '24' },
];
// Letters a-z
'abcdefghijklmnopqrstuvwxyz'.split('').forEach(function(c) {
    _BASE_KEYS.push({ id: 'key-' + c, name: c.toUpperCase(), group: 'Letter', plain: c });
});
// Digits 0-9
'0123456789'.split('').forEach(function(c) {
    _BASE_KEYS.push({ id: 'key-' + c, name: c, group: 'Digit', plain: c });
});
// Symbols (unshifted variants only — use Shift toggle for shifted form, e.g. Shift+/ = ?)
'`-=[]\\;\',./'.split('').forEach(function(c) {
    _BASE_KEYS.push({ id: 'key-' + c, name: c, group: 'Symbol', plain: c });
});

// xterm modifier parameter: Ctrl=5, Shift=2, Alt=3, Ctrl+Shift=6, Ctrl+Alt=7, Shift+Alt=4, Ctrl+Shift+Alt=8
function _modParam(ctrl, shift, alt) {
    return (shift ? 1 : 0) + (alt ? 2 : 0) + (ctrl ? 4 : 0) + 1;
}

// Build {seq, label, raw} from base key + modifier flags
function _buildCombo(baseId, ctrl, shift, alt) {
    var base = null;
    for (var i = 0; i < _BASE_KEYS.length; i++) {
        if (_BASE_KEYS[i].id === baseId) { base = _BASE_KEYS[i]; break; }
    }
    if (!base) return null;

    var noMod = !ctrl && !shift && !alt;

    // Ctrl+Alt+letter — non-standard, not representable reliably; signal unsupported
    if (ctrl && alt && base.group === 'Letter') return null;

    // Ctrl+letter (a-z) → ASCII 0x01–0x1a
    if (ctrl && !alt && base.group === 'Letter') {
        var code = base.plain.charCodeAt(0) - 96;
        var lbl = (shift ? '⇧' : '') + '^' + base.plain;
        var seqId = (shift ? 'shift-' : '') + 'ctrl-' + base.id.replace('key-', '');
        return { seq: seqId, label: lbl, raw: String.fromCharCode(code) };
    }

    // Ctrl+symbol: five terminals-standard codes (ASCII 0x1b–0x1f)
    // [ = ESC(0x1b), \ = FS(0x1c), ] = GS(0x1d), ^ = RS(0x1e), - or _ = US(0x1f)
    var _ctrlSymbolMap = { '[': 0x1b, '\\': 0x1c, ']': 0x1d, '^': 0x1e, '-': 0x1f, '_': 0x1f };
    if (ctrl && !alt && base.group === 'Symbol' && _ctrlSymbolMap[base.plain] !== undefined) {
        var raw = String.fromCharCode(_ctrlSymbolMap[base.plain]);
        return { seq: 'ctrl-' + base.id.replace('key-', ''), label: '^' + base.plain, raw: raw };
    }

    // Shift+Tab → \x1b[Z
    if (shift && !ctrl && !alt && baseId === 'tab') {
        return { seq: 'shift-tab', label: '⇧Tab', raw: '\x1b[Z' };
    }

    // Alt+anything → ESC prefix
    if (alt && !ctrl && !shift && base.group === 'Letter') {
        return { seq: 'alt-' + base.id.replace('key-', ''), label: 'M-' + base.plain, raw: '\x1b' + base.plain };
    }

    // No modifiers → plain (base.name already contains arrow chars for nav keys)
    if (noMod) {
        return { seq: base.id, label: base.name.replace(/\s+(Up|Down|Left|Right)$/, '').trim(), raw: base.plain };
    }

    // CSI modifier sequences for navigation/function keys
    var mod = _modParam(ctrl, shift, alt);
    var modPfx = (ctrl ? (shift ? '⇧^' : '^') : (shift ? '⇧' : '')) + (alt ? 'M-' : '');
    var seqPfx = (ctrl ? (shift ? 'ctrl-shift-' : 'ctrl-') : (shift ? 'shift-' : '')) + (alt ? 'alt-' : '');

    if (base.csiLetter) {
        return { seq: seqPfx + base.id, label: modPfx + base.name.trim(), raw: '\x1b[1;' + mod + base.csiLetter };
    }
    if (base.csiTilde) {
        return { seq: seqPfx + base.id, label: modPfx + base.name.trim(), raw: '\x1b[' + base.csiTilde + ';' + mod + '~' };
    }
    if (base.csiSS3) {
        return { seq: seqPfx + base.id, label: modPfx + base.name.trim(), raw: '\x1b[1;' + mod + base.csiSS3 };
    }

    // Fallback: modifier + printable char (e.g. Ctrl+Shift+symbol — non-standard, send as-is)
    return { seq: seqPfx + base.id, label: modPfx + base.plain, raw: base.plain };
}

// ── Key Picker UI (mobile-first dropdown + desktop keyboard capture) ──
function _openKeyCapture() {
    var existing = document.getElementById('tkp-capture-overlay');
    if (existing) { existing.remove(); return; }

    // Build grouped <select> options
    var groups = {};
    _BASE_KEYS.forEach(function(k) {
        if (!groups[k.group]) groups[k.group] = [];
        groups[k.group].push(k);
    });
    var groupOrder = ['Special', 'Navigation', 'Function', 'Letter', 'Digit', 'Symbol'];
    var optionsHtml = '<option value="">— base key —</option>';
    groupOrder.forEach(function(g) {
        if (!groups[g]) return;
        optionsHtml += '<optgroup label="' + g + '">';
        groups[g].forEach(function(k) {
            optionsHtml += '<option value="' + _escapeHtml(k.id) + '">' + _escapeHtml(k.name) + '</option>';
        });
        optionsHtml += '</optgroup>';
    });

    var overlay = document.createElement('div');
    overlay.id = 'tkp-capture-overlay';
    overlay.innerHTML = [
        '<div class="tkp-capture-panel">',
        '  <div class="tkp-capture-title">Add key combination</div>',
        '  <div class="tkp-mod-row">',
        '    <button class="tkp-mod-btn" data-mod="ctrl">Ctrl</button>',
        '    <button class="tkp-mod-btn" data-mod="shift">Shift</button>',
        '    <button class="tkp-mod-btn" data-mod="alt">Alt</button>',
        '  </div>',
        '  <select class="tkp-key-select" id="tkp-key-select">' + optionsHtml + '</select>',
        '  <div class="tkp-capture-display" id="tkp-cap-display">',
        '    <span class="tkp-capture-hint">Choose modifiers + key above</span>',
        '  </div>',
        '  <input class="tkp-capture-label" id="tkp-cap-label" placeholder="Label (auto-filled)" maxlength="8">',
        '  <div class="tkp-capture-actions">',
        '    <button class="tkp-cap-btn tkp-cap-cancel" id="tkp-cap-cancel">Cancel</button>',
        '    <button class="tkp-cap-btn tkp-cap-add" id="tkp-cap-add" disabled>Add</button>',
        '  </div>',
        '  <div class="tkp-capture-hint-kb">On desktop: press the combo directly</div>',
        '</div>',
    ].join('');

    document.body.appendChild(overlay);

    var display   = document.getElementById('tkp-cap-display');
    var keySelect = document.getElementById('tkp-key-select');
    var labelInput = document.getElementById('tkp-cap-label');
    var addBtn    = document.getElementById('tkp-cap-add');
    var cancelBtn = document.getElementById('tkp-cap-cancel');
    var modBtns   = overlay.querySelectorAll('.tkp-mod-btn');

    var _mods = { ctrl: false, shift: false, alt: false };
    var _captured = null;
    var _labelAuto = true; // true while label still tracks auto-generated value

    labelInput.addEventListener('input', function() { _labelAuto = false; });

    function _update() {
        var baseId = keySelect.value;
        if (!baseId) { display.innerHTML = '<span class="tkp-capture-hint">Choose modifiers + key above</span>'; addBtn.disabled = true; _captured = null; return; }
        var result = _buildCombo(baseId, _mods.ctrl, _mods.shift, _mods.alt);
        _captured = result;
        display.innerHTML = result ? '<span class="tkp-capture-key">' + _escapeHtml(result.label) + '</span>' : '<span class="tkp-capture-hint">Combo not supported</span>';
        if (result && _labelAuto) labelInput.value = result.label;
        addBtn.disabled = !result;
    }

    modBtns.forEach(function(btn) {
        btn.addEventListener('click', function(e) {
            e.preventDefault();
            var mod = btn.dataset.mod;
            _mods[mod] = !_mods[mod];
            btn.classList.toggle('tkp-mod-active', _mods[mod]);
            _update();
        });
    });

    keySelect.addEventListener('change', function() { _update(); });

    function _close() {
        document.removeEventListener('keydown', _onKeyDown, true);
        overlay.remove();
    }

    // Desktop: live keyboard capture auto-fills the picker
    function _onKeyDown(e) {
        if (!document.body.contains(overlay)) { document.removeEventListener('keydown', _onKeyDown, true); return; }
        if (document.activeElement === labelInput && !e.ctrlKey && !e.altKey && !e.metaKey && e.key.length === 1) return;
        if (e.key === 'Escape') { e.preventDefault(); _close(); return; }
        if (['Shift','Control','Alt','Meta','CapsLock','NumLock','ScrollLock'].indexOf(e.key) !== -1) return;

        // Map event modifiers → toggle buttons
        var newMods = { ctrl: e.ctrlKey, shift: e.shiftKey, alt: e.altKey };
        _mods = newMods;
        modBtns.forEach(function(btn) { btn.classList.toggle('tkp-mod-active', !!_mods[btn.dataset.mod]); });

        // Map event key → base key id
        var keyMap = {
            'Escape':'esc','Enter':'enter','Tab':'tab','Backspace':'backspace','Delete':'delete',' ':'space',
            'ArrowUp':'up','ArrowDown':'down','ArrowRight':'right','ArrowLeft':'left',
            'Home':'home','End':'end','PageUp':'pageup','PageDown':'pagedown',
            'F1':'f1','F2':'f2','F3':'f3','F4':'f4','F5':'f5','F6':'f6',
            'F7':'f7','F8':'f8','F9':'f9','F10':'f10','F11':'f11','F12':'f12',
        };
        var baseId = keyMap[e.key] || (e.key.length === 1 ? 'key-' + e.key.toLowerCase() : null);
        if (!baseId) return;

        e.preventDefault();
        keySelect.value = baseId;
        _labelAuto = true; // keyboard capture resets to auto-label
        _update();
        labelInput.focus();
    }

    document.addEventListener('keydown', _onKeyDown, true);
    cancelBtn.addEventListener('click', _close);
    overlay.addEventListener('click', function(e) { if (e.target === overlay) _close(); });

    addBtn.addEventListener('click', function() {
        if (!_captured) return;
        var label = (labelInput.value.trim() || _captured.label).substring(0, 8);
        var exists = _fullCatalog().find(function(k) { return k.seq === _captured.seq; });
        if (!exists) {
            HC._userCatalog.push({ seq: _captured.seq, label: label, raw: _captured.raw });
            if (HC.TERM_KEY_MAP) HC.TERM_KEY_MAP[_captured.seq] = _captured.raw;
            _saveUserCatalog(HC._userCatalog);
        }
        _close();
        renderTermKeyPicker();
    });
}

function _escapeHtml(s) {
    return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');
}

function renderTermKeyPicker() {
    _renderTermKeyPicker();
}

HC.renderTermKeyPicker = renderTermKeyPicker;
