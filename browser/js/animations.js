/* animations.js — Babel3 animations manager (open-source)
 *
 * Lists all LED animations (built-in + custom), allows enable/disable toggles,
 * and links to source files in the file editor.
 *
 * Depends on: core.js (HC namespace, HC.daemonFetch), files.js (openFile)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

(function() {

    var log = function(msg) { if (HC.log) HC.log(msg); else console.log(msg); };

    // Built-in animations shipped with the browser
    var BUILTIN_ANIMATIONS = [
        { name: 'solid',     description: 'All LEDs display the same color, static',                        file: 'js/animations/solid.js' },
        { name: 'breathing', description: 'Sinusoidal brightness pulse, 30%-100%, ~4.2s period',            file: 'js/animations/breathing.js' },
        { name: 'chase',     description: 'Color segments cycle around the strip using palette',             file: 'js/animations/chase.js' },
        { name: 'wave',      description: 'Sinusoidal brightness wave propagates along the strip',          file: 'js/animations/wave.js' },
        { name: 'sparkle',   description: 'Dim base with random LEDs flashing and fading',                  file: 'js/animations/sparkle.js' },
        { name: 'fire',      description: 'Independent per-LED flicker, biased toward reds and oranges',    file: 'js/animations/fire.js' },
        { name: 'gradient',  description: 'Static linear interpolation between palette colors',              file: 'js/animations/gradient.js' },
        { name: 'aurora',    description: 'Overlapping sinusoidal waves modulate palette color selection',   file: 'js/animations/aurora.js' }
    ];

    var _panel = null;
    var _listEl = null;
    var _customAnimations = [];
    var _disabledDefaults = [];

    function getPanel() {
        if (!_panel) _panel = document.getElementById('animations-panel');
        return _panel;
    }

    function openAnimationsPanel() {
        var panel = getPanel();
        if (!panel) return;
        panel.classList.add('open');
        loadAnimations();
    }

    function closeAnimationsPanel() {
        var panel = getPanel();
        if (panel) panel.classList.remove('open');
    }

    async function loadAnimations() {
        _listEl = document.getElementById('animations-list');
        if (!_listEl) return;

        // Fetch user's custom animations + disabled defaults from server
        try {
            var resp = await fetch(HC.EC2_BASE + '/api/animations', {
                credentials: 'include'
            });
            if (resp.ok) {
                var data = await resp.json();
                _customAnimations = data.custom || [];
                _disabledDefaults = (data.disabled_defaults || []).map(function(d) { return d.pattern_name; });
            }
        } catch(e) {
            log('[Animations] Failed to load: ' + e.message);
        }

        renderList();
    }

    function renderList() {
        if (!_listEl) return;
        _listEl.innerHTML = '';

        // Section: Built-in animations
        var builtinLabel = document.createElement('div');
        builtinLabel.className = 'anim-section-label';
        builtinLabel.textContent = 'Built-in Animations';
        _listEl.appendChild(builtinLabel);

        BUILTIN_ANIMATIONS.forEach(function(anim) {
            var isDisabled = _disabledDefaults.indexOf(anim.name) !== -1;
            _listEl.appendChild(createAnimRow(anim.name, anim.description, anim.file, !isDisabled, true));
        });

        // Section: Custom animations
        if (_customAnimations.length > 0) {
            var customLabel = document.createElement('div');
            customLabel.className = 'anim-section-label';
            customLabel.style.marginTop = '16px';
            customLabel.textContent = 'Custom Animations';
            _listEl.appendChild(customLabel);

            _customAnimations.forEach(function(anim) {
                _listEl.appendChild(createAnimRow(anim.name, anim.description, null, true, false, anim.id));
            });
        }

        // Add animation hint
        var hint = document.createElement('div');
        hint.className = 'anim-hint';
        hint.textContent = 'Ask your agent to create new animations using the animation_add tool.';
        _listEl.appendChild(hint);
    }

    function createAnimRow(name, description, filePath, enabled, isBuiltin, customId) {
        var row = document.createElement('div');
        row.className = 'anim-row' + (enabled ? '' : ' disabled');

        // Toggle
        var toggle = document.createElement('input');
        toggle.type = 'checkbox';
        toggle.checked = enabled;
        toggle.className = 'anim-toggle';
        toggle.title = enabled ? 'Disable this animation' : 'Enable this animation';
        toggle.addEventListener('change', function() {
            if (isBuiltin) {
                toggleDefault(name, toggle.checked);
            } else if (customId) {
                // Custom animations: delete to disable
                if (!toggle.checked) deleteCustom(customId, name);
            }
            row.classList.toggle('disabled', !toggle.checked);
        });

        // Info
        var info = document.createElement('div');
        info.className = 'anim-info';

        var nameEl = document.createElement('span');
        nameEl.className = 'anim-name';
        nameEl.textContent = name;

        var descEl = document.createElement('span');
        descEl.className = 'anim-desc';
        descEl.textContent = description;

        info.appendChild(nameEl);
        info.appendChild(descEl);

        row.appendChild(toggle);
        row.appendChild(info);

        // File link (opens in file editor)
        if (filePath) {
            var link = document.createElement('button');
            link.className = 'anim-file-link';
            link.textContent = '</>';
            link.title = 'View source: ' + filePath;
            link.addEventListener('click', function() {
                closeAnimationsPanel();
                if (typeof window.openFileByPath === 'function') {
                    window.openFileByPath(filePath);
                } else if (typeof HC.openFilesPanel === 'function') {
                    HC.openFilesPanel();
                }
            });
            row.appendChild(link);
        }

        // Delete button for custom animations
        if (!isBuiltin && customId) {
            var del = document.createElement('button');
            del.className = 'anim-delete-btn';
            del.textContent = '\u00D7';
            del.title = 'Delete this animation';
            del.addEventListener('click', function() {
                deleteCustom(customId, name);
            });
            row.appendChild(del);
        }

        return row;
    }

    async function toggleDefault(patternName, enable) {
        try {
            if (enable) {
                await fetch(HC.EC2_BASE + '/api/animations/disable/' + patternName, {
                    method: 'DELETE',
                    credentials: 'include'
                });
                var idx = _disabledDefaults.indexOf(patternName);
                if (idx !== -1) _disabledDefaults.splice(idx, 1);
                log('[Animations] Enabled: ' + patternName);
            } else {
                await fetch(HC.EC2_BASE + '/api/animations/disable', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    credentials: 'include',
                    body: JSON.stringify({ pattern_name: patternName })
                });
                _disabledDefaults.push(patternName);
                log('[Animations] Disabled: ' + patternName);
            }
        } catch(e) {
            log('[Animations] Toggle error: ' + e.message);
        }
    }

    async function deleteCustom(id, name) {
        if (!confirm('Delete custom animation "' + name + '"?')) return;
        try {
            await fetch(HC.EC2_BASE + '/api/animations/' + id, {
                method: 'DELETE',
                credentials: 'include'
            });
            _customAnimations = _customAnimations.filter(function(a) { return a.id !== id; });
            renderList();
            log('[Animations] Deleted: ' + name);
        } catch(e) {
            log('[Animations] Delete error: ' + e.message);
        }
    }

    // Expose on HC namespace
    HC.openAnimationsPanel = openAnimationsPanel;
    HC.closeAnimationsPanel = closeAnimationsPanel;

})();
