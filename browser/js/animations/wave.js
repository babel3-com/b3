/**
 * Wave — a sinusoidal brightness wave propagates along the strip.
 * Two complete cycles visible, creating a flowing motion effect.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function wave(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    var n = strip.numLEDs;
    strip.patternState.phase += delta * 0.003;
    for (var i = 0; i < n; i++) {
        var w = (Math.sin(strip.patternState.phase + (i / n) * Math.PI * 4) + 1) / 2;
        var br = 0.4 + w * 0.6;
        strip.setPixel(i, Math.floor(r * br), Math.floor(g * br), Math.floor(b * br));
    }
}

HC.registerAnimation('wave', wave);
