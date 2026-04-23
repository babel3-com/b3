/* files.js — Babel3 file browser (open-source)
 *
 * Directory listing, file viewing (Shiki syntax highlighting),
 * CodeMirror 6 editing, markdown preview, context menu (rename/copy/delete).
 *
 * Depends on: core.js (HC namespace), daemonFetch (HC.daemonFetch)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

var log = function(msg) { if (HC.log) HC.log(msg); else console.log('[Babel3] ' + msg); };

// ── State ──
var _filesCurrentPath = '.';
var _filesAbsolutePath = '';
var _filesHistory = [];
var _filesViewingFile = false;
var _filesCurrentFilePath = '';
var _filesOriginalContent = '';
var _filesLanguage = null;
var _filesEditing = false;
var _filesWrapLines = true;
var _shikiHighlighter = null;
var _shikiLoading = false;
var _cmEditor = null;
var _cmLoaded = false;
var _cmLoading = false;
var _cmModules = null;
var _filesShowLineNums = true;

function daemonFetch(path, opts) {
    if (HC.daemonFetch) return HC.daemonFetch(path, opts);
    return fetch(path, opts);
}

// openFilesPanel(path?) — optional path opens directly to a file or directory.
// If path ends with a filename (not '/'), the parent dir is loaded and the file opened.
function openFilesPanel(path) {
    var panel = document.getElementById('files-panel');
    if (!panel) return;
    panel.classList.add('open');
    _filesCurrentPath = '.';
    _filesHistory = [];
    _filesViewingFile = false;
    _filesEditing = false;
    showFileTree();
    loadShiki();
    loadCodeMirror();
    if (path && path !== '.') {
        // Navigate to the path — if it looks like a file (has extension or no trailing slash),
        // load the parent dir then open the file.
        // Heuristic: treat as file if path contains '.' and doesn't end with '/'.
        // Good enough for the known callers (e.g. ~/.b3/term-keys.json).
        var isFile = path.indexOf('.') !== -1 && !path.endsWith('/');
        if (isFile) {
            var lastSlash = path.lastIndexOf('/');
            var dir = lastSlash > 0 ? path.slice(0, lastSlash) : '.';
            var file = path.slice(lastSlash + 1);
            loadFiles(dir, function() { openFile(file); });
        } else {
            loadFiles(path);
        }
    } else {
        loadFiles('.');
    }
}

function closeFilesPanel() {
    var panel = document.getElementById('files-panel');
    if (panel) panel.classList.remove('open');
    exitEditMode();
}

function showFileTree() {
    _filesViewingFile = false;
    _filesEditing = false;
    document.getElementById('files-tree').style.display = '';
    document.getElementById('files-viewer').style.display = 'none';
    document.getElementById('files-path-input').style.display = '';
}

function showFileViewer() {
    _filesViewingFile = true;
    document.getElementById('files-tree').style.display = 'none';
    document.getElementById('files-viewer').style.display = 'flex';
}

function hideAllViewerPanes() {
    document.getElementById('files-viewer-content').style.display = 'none';
    document.getElementById('files-editor-wrap').style.display = 'none';
    document.getElementById('files-image-wrap').style.display = 'none';
    document.getElementById('files-md-wrap').style.display = 'none';
    document.getElementById('files-edit-btn').style.display = 'none';
    var pdfWrap = document.getElementById('files-pdf-wrap');
    if (pdfWrap) { pdfWrap.style.display = 'none'; pdfWrap.innerHTML = ''; }
    document.getElementById('files-save-btn').style.display = 'none';
}

function loadFiles(path, callback) {
    var tree = document.getElementById('files-tree');
    if (!HC.hasDaemonConnection()) {
        tree.innerHTML = '<div class="files-loading">Waiting for agent connection...</div>';
        log('[Files] No daemon connection');
        return;
    }
    _filesCurrentPath = path;
    tree.innerHTML = '<div class="files-loading">Loading...</div>';

    var url = '/api/files?path=' + encodeURIComponent(path) + '&hidden=true';
    daemonFetch(url)
    .then(function(r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
    })
    .then(function(data) {
        if (data.error) { tree.innerHTML = '<div class="files-loading">' + data.error + '</div>'; return; }
        if (data.absolute) {
            _filesAbsolutePath = data.absolute;
            document.getElementById('files-path-input').value = data.absolute;
        }
        renderFileList(data.entries || []);
        if (typeof callback === 'function') callback();
    })
    .catch(function(e) { tree.innerHTML = '<div class="files-loading">Error: ' + (e.message || e) + '</div>'; });
}


function renderFileList(entries) {
    var tree = document.getElementById('files-tree');
    if (!entries.length) { tree.innerHTML = '<div class="files-loading">Empty directory</div>'; return; }
    var html = '';
    for (var i = 0; i < entries.length; i++) {
        var e = entries[i];
        var icon = e.type === 'dir' ? '📁' : fileIcon(e.name);
        var size = e.type === 'dir' ? '' : formatSize(e.size);
        var nameClass = 'file-entry-name' + (e.type === 'dir' ? ' is-dir' : '');
        html += '<div class="file-entry" data-name="' + escHtml(e.name) + '" data-type="' + e.type + '">'
            + '<span class="file-entry-icon">' + icon + '</span>'
            + '<span class="' + nameClass + '">' + escHtml(e.name) + '</span>'
            + '<span class="file-entry-size">' + size + '</span>'
            + '</div>';
    }
    tree.innerHTML = html;
}

function fileIcon(name) {
    var ext = name.split('.').pop().toLowerCase();
    var icons = {rs:'🦀',py:'🐍',js:'📜',ts:'📘',json:'📋',md:'📝',html:'🌐',css:'🎨',sh:'⚙️',toml:'⚙️',yaml:'⚙️',yml:'⚙️',sql:'🗃️',go:'🔵',java:'☕',svg:'🖼️',png:'🖼️',jpg:'🖼️',gif:'🖼️'};
    return icons[ext] || '📄';
}

function formatSize(bytes) {
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1048576) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / 1048576).toFixed(1) + ' MB';
}

function escHtml(s) { var d = document.createElement('div'); d.textContent = s; return d.innerHTML; }

function navigateToDir(name) {
    _filesHistory.push(_filesAbsolutePath || _filesCurrentPath);
    var newPath = _filesAbsolutePath ? _filesAbsolutePath + '/' + name : (_filesCurrentPath === '.' ? name : _filesCurrentPath + '/' + name);
    showFileTree();
    loadFiles(newPath);
}

function navigateBack() {
    if (_filesEditing) { exitEditMode(); return; }
    if (_filesViewingFile) { showFileTree(); return; }
    if (_filesHistory.length > 0) {
        var prev = _filesHistory.pop();
        loadFiles(prev);
    } else {
        closeFilesPanel();
    }
}

function openFile(name) {
    if (!HC.hasDaemonConnection()) return;
    var viewerName = document.getElementById('files-viewer-name');
    var viewerMeta = document.getElementById('files-viewer-meta');
    if (!viewerName || !viewerMeta) return;
    var filePath = _filesAbsolutePath ? _filesAbsolutePath + '/' + name : (_filesCurrentPath === '.' ? name : _filesCurrentPath + '/' + name);
    _filesCurrentFilePath = filePath;
    // Track file visit during recording
    if (HC.isRecording && HC.isRecording() && filePath) {
        HC.trackFileVisit(filePath);
    }

    viewerName.textContent = name;
    viewerMeta.textContent = 'Loading...';
    hideAllViewerPanes();
    showFileViewer();

    daemonFetch('/api/file?path=' + encodeURIComponent(filePath))
    .then(function(r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
    })
    .then(function(data) {
        if (data.error) {
            document.getElementById('files-viewer-meta').textContent = data.error;
            document.getElementById('files-viewer-content').style.display = '';
            document.getElementById('files-code').textContent = data.error;
            return;
        }
        var fileType = data.type || 'text';

        if (fileType === 'image') {
            document.getElementById('files-viewer-meta').textContent = formatSize(data.size);
            var imgWrap = document.getElementById('files-image-wrap');
            imgWrap.innerHTML = '<img src="' + escHtml(data.data_url) + '" alt="' + escHtml(name) + '" />';
            imgWrap.style.display = 'flex';
        } else if (fileType === 'binary' && name.toLowerCase().endsWith('.pdf')) {
            // PDF: render in iframe with a "Open Full Screen" button for pinch-to-zoom
            document.getElementById('files-viewer-meta').textContent = formatSize(data.size) + ' · PDF';
            var pdfWrap = document.getElementById('files-pdf-wrap');
            if (pdfWrap) {
                pdfWrap.innerHTML = '<div style="text-align:center;padding:20px;color:var(--text-dim);">Loading PDF...</div>';
                pdfWrap.style.display = 'flex';
                daemonFetch('/api/file-raw?path=' + encodeURIComponent(filePath))
                .then(function(r) { return r.blob(); })
                .then(function(blob) {
                    var url = URL.createObjectURL(blob);
                    pdfWrap.innerHTML =
                        '<div style="text-align:center;padding:8px;">' +
                            '<a href="' + url + '" target="_blank" style="display:inline-block;padding:8px 20px;border-radius:6px;background:var(--accent,#2f81f7);color:#fff;font-size:0.85rem;text-decoration:none;">Open PDF</a>' +
                        '</div>' +
                        '<iframe src="' + url + '" style="width:100%;flex:1;border:none;border-radius:8px;background:#fff;min-height:400px;"></iframe>';
                })
                .catch(function(e) {
                    pdfWrap.innerHTML = '<div style="text-align:center;padding:20px;color:#f87171;">Failed to load PDF: ' + e.message + '</div>';
                });
            }
        } else if (fileType === 'binary') {
            document.getElementById('files-viewer-meta').textContent = formatSize(data.size);
            document.getElementById('files-viewer-content').style.display = '';
            document.getElementById('files-code').textContent = '(Binary file, ' + formatSize(data.size) + ')';
        } else {
            var ext = name.split('.').pop().toLowerCase();
            _filesOriginalContent = data.content;
            _filesLanguage = data.language || null;
            document.getElementById('files-viewer-meta').textContent = data.lines + ' lines · ' + formatSize(data.size);
            document.getElementById('files-edit-btn').style.display = '';

            if (ext === 'md' || ext === 'mdx') {
                renderMarkdown(data.content);
            } else {
                document.getElementById('files-viewer-content').style.display = '';
                highlightCode(data.content, data.language);
            }
        }
    })
    .catch(function(e) {
        var m = document.getElementById('files-viewer-meta');
        var c = document.getElementById('files-viewer-content');
        var code = document.getElementById('files-code');
        if (m) m.textContent = 'Error';
        if (c) c.style.display = '';
        if (code) code.textContent = e.message || String(e);
    });
}

function renderMarkdown(src) {
    var mdWrap = document.getElementById('files-md-wrap');
    var html = src
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        .replace(/```(\w*)\n([\s\S]*?)```/g, function(m, lang, code) {
            return '<pre><code>' + code + '</code></pre>';
        })
        .replace(/^### (.+)$/gm, '<h3>$1</h3>')
        .replace(/^## (.+)$/gm, '<h2>$1</h2>')
        .replace(/^# (.+)$/gm, '<h1>$1</h1>')
        .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
        .replace(/\*(.+?)\*/g, '<em>$1</em>')
        .replace(/`([^`]+)`/g, '<code>$1</code>')
        .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank">$1</a>')
        .replace(/^&gt; (.+)$/gm, '<blockquote>$1</blockquote>')
        .replace(/^- (.+)$/gm, '<li>$1</li>')
        .replace(/^---$/gm, '<hr>')
        .replace(/\n\n/g, '</p><p>')
        .replace(/\n/g, '<br>');
    html = html.replace(/(<li>.*<\/li>)/gs, '<ul>$1</ul>');
    html = html.replace(/<\/ul>\s*<ul>/g, '');
    mdWrap.innerHTML = '<p>' + html + '</p>';
    mdWrap.style.display = '';
}

function addLineNumbers(html) {
    var lines = html.split('\n');
    if (lines[lines.length - 1] === '') lines.pop();
    return lines.map(function(line, i) {
        var num = _filesShowLineNums ? '<span class="line-no">' + (i + 1) + '</span>' : '';
        return '<div class="code-line">' + num + '<span class="line-code">' + (line || ' ') + '</span></div>';
    }).join('');
}

function highlightCode(code, language) {
    var viewerContent = document.getElementById('files-viewer-content');
    if (_shikiHighlighter && language && language !== 'text') {
        try {
            var html = _shikiHighlighter.codeToHtml(code, { lang: language, theme: 'github-dark' });
            var tmp = document.createElement('div');
            tmp.innerHTML = html;
            var codeTag = tmp.querySelector('code');
            if (codeTag) {
                var inner = addLineNumbers(codeTag.innerHTML);
                viewerContent.innerHTML = '<pre style="margin:0;padding:12px 16px;font-family:\'JetBrains Mono\',monospace;line-height:1.5;tab-size:4;"><code>' + inner + '</code></pre>';
            } else {
                viewerContent.innerHTML = html;
            }
            var shikiEl = viewerContent.querySelector('.shiki');
            if (shikiEl) shikiEl.style.background = 'transparent';
            return;
        } catch(e) { /* fall through to plain text */ }
    }
    var escaped = code.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    var inner = addLineNumbers(escaped);
    viewerContent.innerHTML = '<pre style="margin:0;padding:12px 16px;font-family:\'JetBrains Mono\',monospace;line-height:1.5;tab-size:4;"><code>' + inner + '</code></pre>';
}

function loadShiki() {
    if (_shikiHighlighter || _shikiLoading) return;
    _shikiLoading = true;
    import('https://esm.sh/shiki@3.0.0').then(function(shiki) {
        return shiki.createHighlighter({
            themes: ['github-dark'],
            langs: ['rust','python','javascript','typescript','json','toml','yaml','markdown','html','css','bash','sql','go','java','c','cpp','tsx','jsx','svelte','xml']
        });
    }).then(function(h) {
        _shikiHighlighter = h;
        _shikiLoading = false;
        log('[Files] Shiki loaded');
    }).catch(function(e) { _shikiLoading = false; log('[Files] Shiki load failed: ' + e); });
}

// ── CodeMirror 6 ──
// Only ONE ?bundle-deps import — multiple create duplicate @codemirror/state instances.
function loadCodeMirror() {
    if (_cmLoaded || _cmLoading) return;
    _cmLoading = true;
    import('https://esm.sh/@codemirror/basic-setup@0.20.0?bundle-deps').then(function(mod) {
        _cmModules = {
            EditorView: mod.EditorView,
            EditorState: mod.EditorState,
            basicSetup: mod.basicSetup
        };
        _cmLoaded = true;
        _cmLoading = false;
        log('[Files] CodeMirror loaded');
    }).catch(function(e) { _cmLoading = false; log('[Files] CodeMirror load failed: ' + e); });
}

function enterEditMode() {
    if (!_cmLoaded || !_cmModules) {
        log('[Files] CodeMirror not loaded yet');
        return;
    }
    _filesEditing = true;
    document.getElementById('files-viewer-content').style.display = 'none';
    document.getElementById('files-md-wrap').style.display = 'none';
    var editorWrap = document.getElementById('files-editor-wrap');
    editorWrap.style.display = '';
    editorWrap.innerHTML = '';
    document.getElementById('files-edit-btn').style.display = 'none';
    document.getElementById('files-save-btn').style.display = '';

    var state = _cmModules.EditorState.create({
        doc: _filesOriginalContent,
        extensions: [_cmModules.basicSetup]
    });
    _cmEditor = new _cmModules.EditorView({
        state: state,
        parent: editorWrap
    });
}

function exitEditMode() {
    _filesEditing = false;
    if (_cmEditor) {
        _cmEditor.destroy();
        _cmEditor = null;
    }
    document.getElementById('files-editor-wrap').style.display = 'none';
    document.getElementById('files-save-btn').style.display = 'none';
    document.getElementById('files-edit-btn').style.display = '';

    var ext = _filesCurrentFilePath.split('.').pop().toLowerCase();
    if (ext === 'md' || ext === 'mdx') {
        renderMarkdown(_filesOriginalContent);
    } else {
        document.getElementById('files-viewer-content').style.display = '';
        highlightCode(_filesOriginalContent, _filesLanguage);
    }
}

function saveFile() {
    if (!_cmEditor || !HC.hasDaemonConnection()) return;
    var content = _cmEditor.state.doc.toString();
    var saveBtn = document.getElementById('files-save-btn');
    saveBtn.textContent = 'Saving...';
    saveBtn.disabled = true;

    daemonFetch('/api/file', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: _filesCurrentFilePath, content: content })
    })
    .then(function(r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
    })
    .then(function(data) {
        if (data.error) throw new Error(data.error);
        saveBtn.textContent = 'Saved!';
        _filesOriginalContent = content;
        setTimeout(function() {
            saveBtn.textContent = 'Save';
            saveBtn.disabled = false;
            exitEditMode();
        }, 800);
    })
    .catch(function(e) {
        saveBtn.textContent = 'Error!';
        saveBtn.disabled = false;
        log('[Files] Save failed: ' + e.message);
        setTimeout(function() { saveBtn.textContent = 'Save'; }, 2000);
    });
}

// ── Path input navigation ──
document.getElementById('files-path-input').addEventListener('keydown', function(e) {
    if (e.key === 'Enter') {
        e.preventDefault();
        var val = this.value.trim();
        if (val) {
            _filesHistory.push(_filesAbsolutePath || _filesCurrentPath);
            loadFiles(val);
        }
        this.blur();
    }
});

// Event delegation for file tree clicks
document.getElementById('files-tree').addEventListener('click', function(e) {
    var entry = e.target.closest('.file-entry');
    if (!entry) return;
    var name = entry.dataset.name;
    var type = entry.dataset.type;
    if (type === 'dir') navigateToDir(name);
    else openFile(name);
});

// Back, close, edit, save buttons
document.getElementById('files-back-btn').addEventListener('click', navigateBack);
document.getElementById('files-close-btn').addEventListener('click', closeFilesPanel);
document.getElementById('files-edit-btn').addEventListener('click', enterEditMode);
document.getElementById('files-save-btn').addEventListener('click', saveFile);

// Wrap toggle
document.getElementById('files-wrap-btn').addEventListener('click', function() {
    _filesWrapLines = !_filesWrapLines;
    this.classList.toggle('active', _filesWrapLines);
    document.getElementById('files-viewer-content').classList.toggle('wrap-lines', _filesWrapLines);
    document.getElementById('files-editor-wrap').classList.toggle('wrap-lines', _filesWrapLines);
});
document.getElementById('files-viewer-content').classList.add('wrap-lines');

// Line number toggle
document.getElementById('files-linenum-btn').addEventListener('click', function() {
    _filesShowLineNums = !_filesShowLineNums;
    this.classList.toggle('active', _filesShowLineNums);
    var gutters = document.querySelector('.files-editor-wrap .cm-gutters');
    if (gutters) gutters.style.display = _filesShowLineNums ? '' : 'none';
    if (_filesViewingFile && !_filesEditing && _filesOriginalContent) {
        var ext = _filesCurrentFilePath.split('.').pop().toLowerCase();
        if (ext !== 'md' && ext !== 'mdx') {
            highlightCode(_filesOriginalContent, _filesLanguage);
        }
    }
});

// ── File context menu (long-press) ──
(function() {
    var contextMenu = document.getElementById('files-context-menu');
    var overlay = document.getElementById('files-context-overlay');
    var pasteBtn = document.getElementById('files-paste-btn');
    var _contextTarget = null;
    var _longPressTimer = null;
    var _longPressFired = false;
    var _clipboard = null;

    var tree = document.getElementById('files-tree');
    tree.addEventListener('contextmenu', function(e) { e.preventDefault(); });

    function showContextMenu(entry, x, y) {
        _contextTarget = {
            name: entry.dataset.name,
            type: entry.dataset.type,
            path: _filesAbsolutePath + '/' + entry.dataset.name
        };
        contextMenu.style.left = Math.min(x, window.innerWidth - 180) + 'px';
        contextMenu.style.top = Math.min(y, window.innerHeight - 200) + 'px';
        contextMenu.classList.add('visible');
        overlay.classList.add('visible');
    }

    function hideContextMenu() {
        contextMenu.classList.remove('visible');
        overlay.classList.remove('visible');
        _contextTarget = null;
    }

    function updatePasteBtn() {
        pasteBtn.style.display = _clipboard ? '' : 'none';
    }

    overlay.addEventListener('click', hideContextMenu);
    overlay.addEventListener('touchend', function(e) { e.preventDefault(); hideContextMenu(); });

    tree.addEventListener('touchstart', function(e) {
        var entry = e.target.closest('.file-entry');
        if (!entry) return;
        _longPressFired = false;
        var touch = e.touches[0];
        _longPressTimer = setTimeout(function() {
            _longPressFired = true;
            showContextMenu(entry, touch.clientX, touch.clientY);
        }, 500);
    }, { passive: false });

    tree.addEventListener('touchmove', function() {
        if (_longPressTimer) { clearTimeout(_longPressTimer); _longPressTimer = null; }
    }, { passive: true });

    tree.addEventListener('touchend', function(e) {
        if (_longPressTimer) { clearTimeout(_longPressTimer); _longPressTimer = null; }
        if (_longPressFired) { e.preventDefault(); _longPressFired = false; }
    });

    pasteBtn.addEventListener('click', function() {
        if (!_clipboard) return;
        pasteBtn.textContent = 'Pasting...';
        daemonFetch('/api/file-copy', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path: _clipboard.path, new_name: _clipboard.name, dest_dir: _filesAbsolutePath })
        }).then(function(r) { return r.json(); })
        .then(function(data) {
            pasteBtn.textContent = '\uD83D\uDCCB Paste';
            if (data.error) { alert('Paste failed: ' + data.error); return; }
            _clipboard = null;
            updatePasteBtn();
            loadFiles(_filesCurrentPath);
        }).catch(function(err) {
            pasteBtn.textContent = '\uD83D\uDCCB Paste';
            alert('Paste failed: ' + err.message);
        });
    });

    contextMenu.addEventListener('click', function(e) {
        var item = e.target.closest('.files-context-item');
        if (!item || !_contextTarget) return;
        var action = item.dataset.action;
        var target = _contextTarget;
        hideContextMenu();

        if (action === 'rename') {
            var newName = prompt('Rename to:', target.name);
            if (!newName || newName === target.name) return;
            daemonFetch('/api/file-rename', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ path: target.path, new_name: newName })
            }).then(function(r) { return r.json(); })
            .then(function(data) {
                if (data.error) { alert('Rename failed: ' + data.error); return; }
                loadFiles(_filesCurrentPath);
            }).catch(function(err) { alert('Rename failed: ' + err.message); });
        }
        else if (action === 'copy') {
            _clipboard = { name: target.name, path: target.path, type: target.type };
            updatePasteBtn();
            log('[Files] Copied: ' + target.path);
        }
        else if (action === 'delete') {
            if (!confirm('Delete "' + target.name + '"?')) return;
            daemonFetch('/api/file-delete', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ path: target.path })
            }).then(function(r) { return r.json(); })
            .then(function(data) {
                if (data.error) { alert('Delete failed: ' + data.error); return; }
                loadFiles(_filesCurrentPath);
            }).catch(function(err) { alert('Delete failed: ' + err.message); });
        }
    });
})();

// Swipe right to go back
(function() {
    var startX = 0, startY = 0;
    var panel = document.getElementById('files-panel');
    panel.addEventListener('touchstart', function(e) {
        startX = e.touches[0].clientX;
        startY = e.touches[0].clientY;
    }, { passive: true });
    panel.addEventListener('touchend', function(e) {
        var dx = e.changedTouches[0].clientX - startX;
        var dy = e.changedTouches[0].clientY - startY;
        if (dx > 80 && Math.abs(dy) < 60) navigateBack();
    }, { passive: true });
})();

// Expose to HC namespace
HC.openFilesPanel = openFilesPanel;
HC.closeFilesPanel = closeFilesPanel;
HC.navigateBack = navigateBack;
HC.loadFiles = loadFiles;
HC.getCurrentFilePath = function() { return _filesCurrentFilePath; };
HC.getFilesAbsolutePath = function() { return _filesAbsolutePath; };
HC.isViewingFile = function() { return _filesViewingFile; };
HC.getFilesCurrentPath = function() { return _filesCurrentPath; };
