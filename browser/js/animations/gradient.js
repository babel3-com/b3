/**
 * Gradient — static linear interpolation between palette colors along the strip.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function gradient(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    var n = strip.numLEDs;
    var pal = strip.palette.length >= 2 ? strip.palette : [[r, g, b], [b, r, g]];
    for (var i = 0; i < n; i++) {
        var t = i / n;
        var idx = t * (pal.length - 1);
        var i1 = Math.floor(idx);
        var i2 = Math.min(i1 + 1, pal.length - 1);
        var bl = idx - i1;
        strip.setPixel(i,
            Math.floor(pal[i1][0] + (pal[i2][0] - pal[i1][0]) * bl),
            Math.floor(pal[i1][1] + (pal[i2][1] - pal[i1][1]) * bl),
            Math.floor(pal[i1][2] + (pal[i2][2] - pal[i1][2]) * bl)
        );
    }
}

HC.registerAnimation('gradient', gradient);
