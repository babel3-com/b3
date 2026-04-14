/* led.js — Babel3 LED strip simulator (open-source)
 *
 * Renders a 300-LED strip around the terminal container using canvas.
 * Animation patterns are loaded from standalone files in js/animations/.
 * Colors and patterns driven by emotion embeddings (server-side matching).
 *
 * Depends on: core.js (HC namespace), js/animations/*.js (pattern functions)
 *
 * https://github.com/babel3-ai/babel3
 */

'use strict';

/**
 * LED strip renderer — draws addressable LEDs around a canvas perimeter.
 * @param {HTMLCanvasElement} canvas
 * @param {number} [numLEDs=300]
 */
HC.LEDStrip = class LEDStrip {
    constructor(canvas, numLEDs = 300) {
        this.canvas = canvas;
        this.ctx = canvas.getContext('2d', { alpha: true });
        this.numLEDs = numLEDs;
        this.leds = new Uint8Array(numLEDs * 3);
        this.positions = [];
        this.ledSize = 6;
        this.ledGap = 2;
        this.lastFrame = performance.now();
        this.animationId = null;
        this.pattern = 'solid';
        this.patternState = { offset: 0, phase: 0, sparkles: [], drops: [] };
        this.baseColor = [51, 51, 51];
        this.palette = [];
        this.resizeObserver = new ResizeObserver(() => this.resize());
        this.resizeObserver.observe(canvas.parentElement);
        this.resize();
        this.startAnimation();
    }

    resize() {
        var container = this.canvas.parentElement;
        var rect = container.getBoundingClientRect();
        var dpr = window.devicePixelRatio || 1;
        this.canvas.width = rect.width * dpr;
        this.canvas.height = rect.height * dpr;
        this.canvas.style.width = rect.width + 'px';
        this.canvas.style.height = rect.height + 'px';
        this.ctx.scale(dpr, dpr);
        this.width = rect.width;
        this.height = rect.height;
        this.calculatePositions();
    }

    calculatePositions() {
        this.positions = [];
        var margin = 4, ledSize = this.ledSize;
        var w = this.width, h = this.height;
        var perimeter = 2 * (w + h) - 4 * margin;
        var actualStep = perimeter / this.numLEDs;
        for (var i = 0; i < this.numLEDs; i++) {
            var p = i * actualStep;
            var x, y;
            if (p < w - 2 * margin) {
                x = margin + p; y = margin;
            } else if (p < (w - 2 * margin) + (h - 2 * margin)) {
                var rP = p - (w - 2 * margin); x = w - margin - ledSize; y = margin + rP;
            } else if (p < 2 * (w - 2 * margin) + (h - 2 * margin)) {
                var bP = p - (w - 2 * margin) - (h - 2 * margin); x = w - margin - ledSize - bP; y = h - margin - ledSize;
            } else {
                var lP = p - 2 * (w - 2 * margin) - (h - 2 * margin); x = margin; y = h - margin - ledSize - lP;
            }
            this.positions.push({ x: Math.max(0, x), y: Math.max(0, y) });
        }
    }

    setPixel(index, r, g, b) {
        if (index < 0 || index >= this.numLEDs) return;
        var off = index * 3;
        this.leds[off] = r; this.leds[off + 1] = g; this.leds[off + 2] = b;
    }

    setAllPixels(r, g, b) {
        for (var i = 0; i < this.numLEDs; i++) this.setPixel(i, r, g, b);
    }

    setColor(r, g, b) {
        this.baseColor = [r, g, b];
        if (this.pattern === 'solid') this.setAllPixels(r, g, b);
    }

    setPalette(palette) { this.palette = palette || []; }

    setPattern(pattern) {
        this.pattern = pattern;
        this.patternState = { offset: 0, phase: 0, sparkles: [], drops: [] };
    }

    /**
     * Register a pattern function. Used by standalone animation files
     * and by custom user-submitted patterns.
     * @param {string} name — pattern name (e.g. "rainfall")
     * @param {Function} fn — function(strip, delta)
     */
    registerPattern(name, fn) {
        if (!this._patterns) this._patterns = {};
        this._patterns[name] = fn;
    }

    /**
     * Load all globally registered patterns from HC._animationRegistry.
     * Called after construction to pick up patterns from standalone files.
     */
    loadRegisteredPatterns() {
        var reg = HC._animationRegistry || {};
        for (var name in reg) {
            if (reg.hasOwnProperty(name)) this.registerPattern(name, reg[name]);
        }
    }

    applyPattern(delta) {
        var fn = this._patterns && this._patterns[this.pattern];
        if (fn) {
            try { fn(this, delta); } catch(e) {
                this.setAllPixels(this.baseColor[0], this.baseColor[1], this.baseColor[2]);
            }
            return;
        }
        // Fallback for unknown patterns
        this.setAllPixels(this.baseColor[0], this.baseColor[1], this.baseColor[2]);
    }

    render() {
        this.ctx.clearRect(0, 0, this.width, this.height);
        for (var i = 0; i < this.positions.length; i++) {
            var pos = this.positions[i];
            var off = i * 3;
            var r = this.leds[off], g = this.leds[off + 1], b = this.leds[off + 2];
            this.ctx.fillStyle = 'rgba(' + r + ',' + g + ',' + b + ',0.3)';
            this.ctx.fillRect(pos.x - 2, pos.y - 2, this.ledSize + 4, this.ledSize + 4);
            this.ctx.fillStyle = 'rgb(' + r + ',' + g + ',' + b + ')';
            this.ctx.fillRect(pos.x, pos.y, this.ledSize, this.ledSize);
        }
    }

    startAnimation() {
        var self = this;
        var loop = function(ts) {
            var delta = ts - self.lastFrame;
            if (delta < 33) { self.animationId = requestAnimationFrame(loop); return; }
            self.lastFrame = ts;
            self.applyPattern(delta);
            self.render();
            self.animationId = requestAnimationFrame(loop);
        };
        this.animationId = requestAnimationFrame(loop);
    }

    stopAnimation() {
        if (this.animationId) {
            cancelAnimationFrame(this.animationId);
            this.animationId = null;
        }
    }
};

// ── Animation Registry — standalone files register here ──
// Animation files (js/animations/*.js) call HC.registerAnimation(name, fn)
// to add themselves. LEDStrip.loadRegisteredPatterns() picks them up.
if (!HC._animationRegistry) HC._animationRegistry = {};
HC.registerAnimation = function(name, fn) { HC._animationRegistry[name] = fn; };

// ── LED Chromatophore — Browser-side emotion matching ──
// Fetches reference embeddings once, embeds emotion text through the encrypted
// GPU pool channel, runs cosine similarity locally. Emotion text never touches EC2.
// Requires: HC.LEDStrip (above), HC.gpuSubmit, HC.EC2_BASE, HC.log

(function() {
    'use strict';

    /**
     * HC._ledStrip — the active LEDStrip instance.
     * Set by the main page after DOM ready: HC._ledStrip = new HC.LEDStrip(canvas, 300);
     */
    HC._ledStrip = null;

    function log(msg) { if (HC.log) HC.log(msg); else console.log(msg); }

    // ── Caches ──
    HC._emotionRefs = null;           // Fetched once from /api/emotion-embeddings
    HC._userAnimations = null;        // Fetched per-session from /api/user-animations
    HC._userAnimationsFetchedAt = 0;  // Timestamp of last user-animations fetch
    var USER_ANIMS_TTL = 60000;       // 60s TTL

    // ── Cosine similarity ──
    function cosineSim(a, b) {
        var dot = 0, na = 0, nb = 0;
        for (var i = 0; i < a.length; i++) { dot += a[i]*b[i]; na += a[i]*a[i]; nb += b[i]*b[i]; }
        return na && nb ? dot / (Math.sqrt(na) * Math.sqrt(nb)) : 0;
    }

    // ── Fetch emotion reference embeddings (cached) ──
    async function fetchEmotionRefs() {
        if (HC._emotionRefs) return HC._emotionRefs;
        var resp = await fetch(HC.EC2_BASE + '/api/emotion-refs', {
            signal: AbortSignal.timeout(10000),
        });
        if (!resp.ok) throw new Error('emotion-embeddings HTTP ' + resp.status);
        HC._emotionRefs = await resp.json();
        return HC._emotionRefs;
    }

    // ── Fetch user animations with embeddings (TTL-cached) ──
    // Response shape: {animations: [{name, embedding, pattern_code}], disabled_defaults: [patternName]}
    async function fetchUserAnimations() {
        var now = Date.now();
        if (HC._userAnimations && (now - HC._userAnimationsFetchedAt) < USER_ANIMS_TTL) {
            return HC._userAnimations;
        }
        try {
            var resp = await fetch(HC.EC2_BASE + '/api/user-animations', {
                signal: AbortSignal.timeout(5000),
            });
            if (resp.status === 401) { HC._userAnimations = { animations: [], disabled_defaults: [] }; return HC._userAnimations; }
            if (!resp.ok) return HC._userAnimations || { animations: [], disabled_defaults: [] };
            HC._userAnimations = await resp.json();
            HC._userAnimationsFetchedAt = now;
            return HC._userAnimations;
        } catch(e) {
            return HC._userAnimations || { animations: [], disabled_defaults: [] };
        }
    }

    // ── Embed emotion text via GPU pool channel ──
    function embedEmotion(text) {
        return new Promise(function(resolve, reject) {
            var ctrl = HC.gpuSubmit ? HC.gpuSubmit({
                action: 'embed',
                texts: ['feeling ' + text],
                task_type: 'search_query',
            }) : null;
            if (!ctrl) { reject(new Error('No GPU channel')); return; }

            var timer = setTimeout(function() {
                ctrl.cancel && ctrl.cancel();
                reject(new Error('embed timeout'));
            }, 10000);

            ctrl.onResult = function(result) {
                clearTimeout(timer);
                // Unwrap RunPod output: may be {embeddings:...} or [{worker_id:...}, {embeddings:...}]
                var output = result.output || result;
                if (Array.isArray(output)) {
                    output = output.find(function(el) { return el && el.embeddings; }) || output[0] || output;
                }
                var emb = output && output.embeddings && output.embeddings[0];
                if (!emb || !emb.length) { reject(new Error('No embedding in result')); return; }
                resolve(emb);
            };
            ctrl.onError = function(err) {
                clearTimeout(timer);
                reject(new Error(err || 'embed error'));
            };
        });
    }

    // ── Apply matched result to strip ──
    function applyToStrip(strip, rgb, topColors, pattern, patternCode) {
        strip.setColor(rgb[0], rgb[1], rgb[2]);

        if (topColors && topColors.length > 1) {
            strip.setPalette(topColors.map(function(c) { return c.rgb || c; }));
        } else {
            strip.setPalette([]);
        }

        // SECURITY: pattern_code is user-submitted JS that runs in the browser context.
        // This is safe because only the authenticated user's OWN pattern code is returned.
        // Custom pattern code MUST NOT be served to any user other than the one who submitted it.
        // If animation sharing is ever added, replace with a declarative animation DSL.
        if (patternCode && pattern) {
            try {
                var customFn = new Function('strip', 'delta', patternCode);
                strip.registerPattern(pattern, customFn);
                log('[LED] Registered custom pattern: ' + pattern);
            } catch(e) {
                log('[LED] ⚠ Custom pattern code error: ' + e);
            }
        }

        strip.setPattern(pattern);
    }

    // ── Match embedding against refs, return {rgb, topColors, pattern, patternCode, sim} ──
    // userAnims shape: {animations: [{name, embedding, pattern_code}], disabled_defaults: [patternName]}
    function matchEmbedding(emb, refs, userAnims) {
        // Refs may be a dict ({name: {rgb, embedding}} or {name: {embedding}}) or array.
        // Normalize to array form for consistent iteration.
        function toArray(val, nameKey) {
            if (!val) return [];
            if (Array.isArray(val)) return val;
            return Object.keys(val).map(function(k) {
                var obj = val[k]; obj[nameKey] = obj[nameKey] || k; return obj;
            });
        }
        var colors = toArray(refs.colors, 'name');
        var patterns = toArray(refs.patterns, 'name');
        var disabledDefaults = {};

        // Collect disabled defaults from user animations response
        var userAnimList = (userAnims && userAnims.animations) || [];
        var disabledList = (userAnims && userAnims.disabled_defaults) || [];
        disabledList.forEach(function(d) { disabledDefaults[d] = true; });

        // Score all color refs
        var colorScores = colors.map(function(c) {
            return { name: c.name, rgb: c.rgb, sim: cosineSim(emb, c.embedding) };
        }).sort(function(a, b) { return b.sim - a.sim; });

        // Blend top-3 colors by similarity weight
        var top3 = colorScores.slice(0, 3);
        var totalWeight = top3.reduce(function(s, c) { return s + c.sim; }, 0);
        var rgb = [0, 0, 0];
        if (totalWeight > 0) {
            top3.forEach(function(c) {
                var w = c.sim / totalWeight;
                rgb[0] += c.rgb[0] * w; rgb[1] += c.rgb[1] * w; rgb[2] += c.rgb[2] * w;
            });
        }
        rgb = rgb.map(function(v) { return Math.round(Math.max(0, Math.min(255, v))); });

        // Score pattern refs (skip disabled defaults)
        var patternScores = patterns
            .filter(function(p) { return !disabledDefaults[p.name]; })
            .map(function(p) { return { name: p.name, sim: cosineSim(emb, p.embedding), code: null }; });

        // Score user custom animations
        userAnimList.forEach(function(a) {
            if (a.embedding && a.embedding.length) {
                patternScores.push({ name: a.name, sim: cosineSim(emb, a.embedding), code: a.pattern_code || null });
            }
        });

        patternScores.sort(function(a, b) { return b.sim - a.sim; });
        var bestPattern = patternScores[0] || { name: 'breathing', sim: 0, code: null };

        return {
            rgb: rgb,
            topColors: top3,
            pattern: bestPattern.name,
            patternCode: bestPattern.code,
            sim: bestPattern.sim,
        };
    }

    /**
     * Set LED strip color and pattern based on an emotion string.
     * Embeds via GPU pool channel, matches against cached refs locally.
     * @param {string} emotion — e.g. "warm joy", "excited curiosity", "off"
     */
    async function setLedEmotion(emotion) {
        var strip = HC._ledStrip;
        if (!strip) { log('[LED] ⚠ No ledStrip instance'); return; }
        var key = (emotion || '').toLowerCase().trim();
        if (key === 'off' || !key) {
            log('[LED] Off');
            strip.setColor(51, 51, 51); strip.setPattern('solid'); return;
        }

        log('[LED] ── setLedEmotion("' + emotion + '") ──');

        try {
            // Fetch refs and user animations concurrently
            var refs, userAnims;
            try {
                var results = await Promise.all([fetchEmotionRefs(), fetchUserAnimations()]);
                refs = results[0]; userAnims = results[1];
            } catch(e) {
                log('[LED] ⚠ Failed to load refs: ' + e);
                strip.setColor(150, 180, 200); strip.setPattern('breathing'); return;
            }

            // Embed via GPU pool channel
            var emb;
            try {
                emb = await embedEmotion(key);
            } catch(e) {
                log('[LED] ⚠ Embed failed: ' + e + ' — using default color');
                strip.setColor(150, 180, 200); strip.setPattern('breathing');
                return;
            }

            // Match locally
            var match = matchEmbedding(emb, refs, userAnims);
            applyToStrip(strip, match.rgb, match.topColors, match.pattern, match.patternCode);

            log('[LED] ✓ "' + emotion + '" → rgb(' + match.rgb.join(',') + ') pattern=' + match.pattern +
                ' (sim=' + (match.sim * 100).toFixed(1) + '%)');
            if (match.topColors.length > 0) {
                log('[LED]   Colors: ' + match.topColors.map(function(c) {
                    return c.name + '=' + (c.sim * 100).toFixed(1) + '%';
                }).join(', '));
            }
        } catch(e) {
            log('[LED] ⚠ setLedEmotion error: ' + e);
            strip.setColor(150, 180, 200); strip.setPattern('breathing');
        }
    }

    // Expose on HC namespace
    HC.setLedEmotion = setLedEmotion;
})();
