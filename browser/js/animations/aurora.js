/**
 * Aurora — multiple overlapping sinusoidal waves modulate palette color selection.
 * Organic, aurora borealis-like effect. Irrational frequency ratio (1.0:0.7)
 * ensures the pattern never exactly repeats.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function aurora(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    var n = strip.numLEDs;
    strip.patternState.phase += delta * 0.001;
    var pal = strip.palette.length > 0 ? strip.palette : [[r, g, b], [g, b, r], [b, r, g]];
    for (var i = 0; i < n; i++) {
        var w1 = (Math.sin(strip.patternState.phase + i * 0.1) + 1) / 2;
        var w2 = (Math.sin(strip.patternState.phase * 0.7 + i * 0.15) + 1) / 2;
        var ci = Math.floor((w1 + w2) * (pal.length - 1) / 2);
        var pc = pal[Math.min(ci, pal.length - 1)];
        var br = 0.5 + w1 * 0.5;
        strip.setPixel(i, Math.floor(pc[0] * br), Math.floor(pc[1] * br), Math.floor(pc[2] * br));
    }
}

HC.registerAnimation('aurora', aurora);
