/**
 * Fire — each LED flickers independently with randomized brightness.
 * Biased toward reds/oranges (green ×0.6, blue ×0.2).
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function fire(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    var n = strip.numLEDs;
    for (var i = 0; i < n; i++) {
        var f = 0.5 + Math.random() * 0.5;
        strip.setPixel(i, Math.floor(r * f), Math.floor(g * f * 0.6), Math.floor(b * f * 0.2));
    }
}

HC.registerAnimation('fire', fire);
