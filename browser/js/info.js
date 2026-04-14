/* info.js — Babel3 info panel (open-source)
 *
 * Displays HTML content pushed by the agent via voice_share_info().
 * History is persisted by the daemon (not localStorage) — works across
 * devices and survives browser refreshes. Receives live updates via
 * WebSocket "info" messages, fetches archive from daemon API on load.
 *
 * Depends on: core.js (HC namespace, HC.daemonFetch)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

var _infoHistory = []; // Array of {ts, title, index} summaries (newest first)
var _infoPanel = document.getElementById('info-panel');
var _infoContent = document.getElementById('info-content');
var _infoSelect = document.getElementById('info-history-select');
var _infoCloseBtn = document.getElementById('info-close-btn');

function _loadInfoArchive() {
    if (!HC.hasDaemonConnection()) return;

    HC.daemonFetch('/api/info')
        .then(function(r) { return r.ok ? r.json() : []; })
        .then(function(entries) {
            if (!entries || !entries.length) return;
            _infoHistory = entries; // [{ts, title, index}] newest first
            _renderInfoHistory();
            // Load the latest entry's full HTML
            _fetchInfoEntry(_infoHistory[0].index);
        })
        .catch(function(e) {
            log('[Info] Failed to load archive: ' + e);
        });
}

function _renderInfoHistory() {
    if (_infoHistory.length < 2) {
        _infoSelect.classList.remove('has-items');
        return;
    }
    _infoSelect.classList.add('has-items');
    _infoSelect.innerHTML = _infoHistory.map(function(entry, i) {
        var d = new Date(entry.ts);
        var label = d.toLocaleDateString('en-US', {month: 'short', day: 'numeric'})
            + ' ' + d.toLocaleTimeString('en-US', {hour: 'numeric', minute: '2-digit'});
        var title = entry.title || 'Info';
        return '<option value="' + entry.index + '"' + (i === 0 ? ' selected' : '') + '>'
            + label + ' — ' + title.replace(/</g, '&lt;').substring(0, 40)
            + '</option>';
    }).join('');
}

function _fetchInfoEntry(archiveIndex) {
    if (!HC.hasDaemonConnection()) return;

    HC.daemonFetch('/api/info/' + archiveIndex)
        .then(function(r) { return r.ok ? r.json() : null; })
        .then(function(entry) {
            if (entry && entry.html) {
                _infoContent.innerHTML = entry.html;
            }
        })
        .catch(function(e) {
            log('[Info] Failed to fetch entry ' + archiveIndex + ': ' + e);
        });
}

function showInfoContent(html) {
    // Live update from WebSocket — show immediately.
    // The daemon has already archived it server-side.
    _infoContent.innerHTML = html;
    log('[Info] New content received via WebSocket');

    // Refresh the history list from daemon to pick up the new entry
    if (HC.hasDaemonConnection()) {
        HC.daemonFetch('/api/info')
            .then(function(r) { return r.ok ? r.json() : []; })
            .then(function(entries) {
                if (entries && entries.length) {
                    _infoHistory = entries;
                    _renderInfoHistory();
                }
            })
            .catch(function() {});
    }
}

function toggleInfoPanel() {
    _infoPanel.classList.toggle('open');
}

function closeInfoPanel() {
    _infoPanel.classList.remove('open');
}

// History selector change — fetch full HTML from daemon
_infoSelect.addEventListener('change', function() {
    var archiveIndex = parseInt(this.value);
    if (isNaN(archiveIndex)) return;
    _fetchInfoEntry(archiveIndex);
});

// Close button
_infoCloseBtn.addEventListener('click', closeInfoPanel);

// Expose to HC namespace
HC.showInfoContent = showInfoContent;
HC.toggleInfoPanel = toggleInfoPanel;
HC.closeInfoPanel = closeInfoPanel;
HC.loadInfoArchive = _loadInfoArchive;
