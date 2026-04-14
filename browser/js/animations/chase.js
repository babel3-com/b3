/**
 * Chase — segments of color cycle around the strip using palette colors.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function chase(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    var n = strip.numLEDs;
    strip.patternState.offset += delta * 0.1;
    var pal = strip.palette.length > 0 ? strip.palette : [[r, g, b]];
    for (var i = 0; i < n; i++) {
        var ci = Math.floor((i + strip.patternState.offset) / (n / pal.length)) % pal.length;
        var c = pal[ci] || [r, g, b];
        strip.setPixel(i, c[0], c[1], c[2]);
    }
}

HC.registerAnimation('chase', chase);
