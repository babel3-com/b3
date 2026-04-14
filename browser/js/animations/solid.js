/**
 * Solid — all LEDs display the same color, static.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function solid(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    strip.setAllPixels(r, g, b);
}

HC.registerAnimation('solid', solid);
