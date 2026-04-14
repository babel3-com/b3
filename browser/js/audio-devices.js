/* audio-devices.js — iOS audio device picker panel (open-source)
 *
 * Adds a floating button to #fs-btn-stack that opens a device selection
 * panel for audio input (microphone) and output routing.
 *
 * Only active when NativeBridge.capabilities includes 'audioDeviceSelection'.
 * Hooks into NativeBridge.onDevicesChanged() and onPreferredInputUnavailable()
 * for live updates.
 *
 * Depends on: layout.js (HC._prefs, HC.saveLayoutSetting)
 * Loads after: layout.js, boot.js (NativeBridge detection)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

(function() {

var _popup = null;
var _btn = null;
var _isOpen = false;
var _initialized = false;

function _hasAudioDeviceSelection() {
    return window.NativeBridge
        && window.NativeBridge.isAvailable
        && Array.isArray(window.NativeBridge.capabilities)
        && window.NativeBridge.capabilities.indexOf('audioDeviceSelection') !== -1;
}

// ── Panel open/close ──

function _openPanel() {
    if (!_popup || !_hasAudioDeviceSelection()) return;
    _isOpen = true;
    _popup.classList.add('open');
    _refreshDevices();
}

function _closePanel() {
    if (!_popup) return;
    _isOpen = false;
    _popup.classList.remove('open');
}

function _togglePanel() {
    if (_isOpen) _closePanel(); else _openPanel();
}

// ── Device list rendering ──

function _refreshDevices() {
    if (!_hasAudioDeviceSelection()) return;

    // getCurrentRoute → then listAudioDevices (async bridge calls, callback-based)
    NativeBridge.getCurrentRoute(function(route) {
        var currentInputUid = route && route.inputs && route.inputs[0] && route.inputs[0].uid;
        NativeBridge.listAudioDevices(function(devices) {
            _renderInputs(devices.inputs || [], currentInputUid);
            _renderOutput(route);
        });
    });
}

function _renderInputs(inputs, currentInputUid) {
    var inputList = _popup.querySelector('.audio-devices-input-list');
    inputList.innerHTML = '';
    if (inputs.length === 0) {
        var empty = document.createElement('div');
        empty.className = 'audio-devices-empty';
        empty.textContent = 'No inputs available';
        inputList.appendChild(empty);
        return;
    }
    inputs.forEach(function(input) {
        var row = document.createElement('button');
        row.className = 'audio-devices-row' + (input.uid === currentInputUid ? ' active' : '');
        row.title = input.uid;
        var icon = document.createElement('span');
        icon.className = 'audio-devices-row-icon';
        icon.textContent = _inputIcon(input);
        var label = document.createElement('span');
        label.className = 'audio-devices-row-label';
        label.textContent = input.portName || input.uid;
        row.appendChild(icon);
        row.appendChild(label);
        if (input.uid !== currentInputUid) {
            row.addEventListener('click', function() {
                NativeBridge.selectAudioInput(input.uid, function(selectedUid, success) {
                    if (success) _refreshDevices();
                });
            });
        }
        inputList.appendChild(row);
    });
}

function _renderOutput(route) {
    var routeLabel = _popup.querySelector('.audio-devices-route');
    var output = route && route.outputs && route.outputs[0];
    routeLabel.textContent = output ? (output.portName || output.portType || 'Speaker') : 'Speaker';
}

function _inputIcon(input) {
    var pt = (input.portType || '').toLowerCase();
    if (pt.indexOf('bluetooth') !== -1) return '🎧';
    if (pt.indexOf('headset') !== -1 || pt.indexOf('headphone') !== -1) return '🎧';
    if (pt.indexOf('built') !== -1 || pt.indexOf('micro') !== -1) return '🎙';
    if (pt.indexOf('usb') !== -1) return '🔌';
    return '🎤';
}

// ── Build DOM ──

function _buildUI() {
    var stack = document.getElementById('fs-btn-stack');
    if (!stack) return;

    // Wrapper (for CSS show/hide via body class)
    var wrap = document.createElement('div');
    wrap.className = 'stack-wrap-audio-devices';
    wrap.style.position = 'relative';

    // Button
    _btn = document.createElement('button');
    _btn.className = 'stack-btn';
    _btn.id = 'fs-audio-devices-btn';
    _btn.title = 'Audio devices';
    _btn.innerHTML = '&#x1F3A7;'; // 🎧
    _btn.addEventListener('click', _togglePanel);

    // Popup panel
    _popup = document.createElement('div');
    _popup.className = 'stack-audio-devices-popup';
    _popup.id = 'fs-audio-devices-popup';
    _popup.innerHTML = [
        '<div class="audio-devices-section-label">Microphone Input</div>',
        '<div class="audio-devices-input-list"></div>',
        '<div class="audio-devices-divider"></div>',
        '<div class="audio-devices-section-label">Output</div>',
        '<div class="audio-devices-output-row">',
            '<span class="audio-devices-route-icon">&#x1F50A;</span>',
            '<span class="audio-devices-route"></span>',
            '<button class="audio-devices-output-btn" id="fs-audio-output-picker" title="Select output">&#x25B6;&#xFE0E;</button>',
        '</div>',
    ].join('');

    _popup.querySelector('#fs-audio-output-picker').addEventListener('click', function() {
        NativeBridge.showOutputPicker();
    });

    wrap.appendChild(_btn);
    wrap.appendChild(_popup);

    // Insert before the scroll-to-bottom button (last child)
    var scrollBtn = stack.querySelector('#fs-scroll-bottom');
    if (scrollBtn) {
        stack.insertBefore(wrap, scrollBtn);
    } else {
        stack.appendChild(wrap);
    }

    // Close panel when tapping outside.
    // Use touchstart (capture phase) because iOS doesn't fire 'click' on
    // non-interactive elements (no cursor:pointer / onclick) — taps on the
    // terminal background would otherwise not dismiss the panel.
    document.addEventListener('touchstart', function(e) {
        if (_isOpen && _popup && !wrap.contains(e.target)) {
            _closePanel();
        }
    }, { passive: true, capture: true });
}

// ── NativeBridge callbacks ──

function _registerCallbacks() {
    if (!_hasAudioDeviceSelection()) return;

    // _onDevicesChanged fires with updated devices payload from pushDeviceUpdateToWebApp().
    // When open, re-query route (to get current input uid) then render with new device list.
    NativeBridge.onDevicesChanged(function() {
        if (_isOpen) _refreshDevices();
    });

    NativeBridge.onPreferredInputUnavailable(function(uid) {
        if (HC.log) HC.log('[AudioDevices] Preferred input unavailable: ' + uid);
    });
}

// ── Init ──

function init() {
    if (!_hasAudioDeviceSelection()) return;
    if (_initialized) return;
    _initialized = true;
    _buildUI();
    _registerCallbacks();

    // Reveal the drawer toggle (hidden by default; only shown on iOS)
    var drawerToggleWrap = document.getElementById('layout-show-audio-devices-wrapper');
    if (drawerToggleWrap) drawerToggleWrap.style.display = '';

    if (HC.log) HC.log('[AudioDevices] Audio device picker ready');
}

// Run after DOM ready (this file loads at end of body)
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
} else {
    init();
}

// Expose for boot.js re-init (NativeBridge may not be ready at first load on iOS)
HC.initAudioDevices = init;

})();
