/**
 * Sparkle — dim base with random LEDs flashing to full brightness and fading.
 * 30% base brightness, sparkles decay over ~300ms.
 * @param {LEDStrip} strip
 * @param {number} delta — ms since last frame
 */
function sparkle(strip, delta) {
    var r = strip.baseColor[0], g = strip.baseColor[1], b = strip.baseColor[2];
    var n = strip.numLEDs;
    strip.setAllPixels(Math.floor(r * 0.3), Math.floor(g * 0.3), Math.floor(b * 0.3));
    if (Math.random() < 0.3 && strip.patternState.sparkles.length < 20) {
        strip.patternState.sparkles.push({ idx: Math.floor(Math.random() * n), life: 1 });
    }
    strip.patternState.sparkles = strip.patternState.sparkles.filter(function(s) {
        s.life -= delta * 0.003;
        if (s.life > 0) {
            strip.setPixel(s.idx, Math.floor(r * s.life), Math.floor(g * s.life), Math.floor(b * s.life));
            return true;
        }
        return false;
    });
}

HC.registerAnimation('sparkle', sparkle);
