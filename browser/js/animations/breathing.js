/**
 * Breathing — all LEDs pulse in unison with a sinusoidal brightness envelope.
 * Cycles between 30% and 100% brightness, ~4.2s period.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function breathing(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    strip.patternState.phase += delta * 0.0015;
    var brightness = (Math.sin(strip.patternState.phase) + 1) / 2;
    strip.setAllPixels(
        Math.floor(r * (0.3 + brightness * 0.7)),
        Math.floor(g * (0.3 + brightness * 0.7)),
        Math.floor(b * (0.3 + brightness * 0.7))
    );
}

HC.registerAnimation('breathing', breathing);
