#!/bin/bash
# GPU Reliability Test Suite
# Run: bash tests/gpu-reliability-test.sh [tunnel_url]
# Tests every leg of the GPU pipeline and reports results.

set -euo pipefail

RELAY_LOCAL="http://localhost:5131"
WORKER_LOCAL="http://localhost:5125"
TUNNEL_URL="${1:-}"
PASS=0
FAIL=0
WARN=0

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

pass() { ((PASS++)); echo -e "  ${GREEN}✓${NC} $1"; }
fail() { ((FAIL++)); echo -e "  ${RED}✗${NC} $1"; }
warn() { ((WARN++)); echo -e "  ${YELLOW}⚠${NC} $1"; }
header() { echo -e "\n${CYAN}── $1 ──${NC}"; }

# Auto-detect tunnel URL from Docker logs if not provided
if [ -z "$TUNNEL_URL" ]; then
    TUNNEL_URL=$(docker logs gpu-worker 2>&1 | grep "Quick tunnel ready" | tail -1 | awk '{print $NF}' 2>/dev/null || true)
    if [ -z "$TUNNEL_URL" ]; then
        echo "Usage: $0 [tunnel_url]"
        echo "Or ensure gpu-worker Docker container is running (auto-detects quick tunnel)"
        exit 1
    fi
fi

echo "GPU Reliability Test Suite"
echo "=========================="
echo "Relay:  $RELAY_LOCAL"
echo "Worker: $WORKER_LOCAL"
echo "Tunnel: $TUNNEL_URL"
echo "Time:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ── Test 1: Docker container ──
header "Docker Container"

if docker ps --format '{{.Names}}' | grep -q gpu-worker; then
    UPTIME=$(docker inspect --format='{{.State.StartedAt}}' gpu-worker 2>/dev/null)
    pass "Container running (started: $UPTIME)"
else
    fail "Container not running"
fi

# ── Test 2: GPU Worker Health ──
header "GPU Worker (localhost:5125)"

WORKER_HEALTH=$(curl -s --max-time 5 "$WORKER_LOCAL/health" 2>/dev/null || echo "UNREACHABLE")
if echo "$WORKER_HEALTH" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['status']=='ok'" 2>/dev/null; then
    GPU_NAME=$(echo "$WORKER_HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('gpu','?'))" 2>/dev/null)
    VRAM=$(echo "$WORKER_HEALTH" | python3 -c "import sys,json; d=json.load(sys.stdin); print(f\"{d.get('vram_used_gb','?')}/{d.get('vram_total_gb','?')} GB\")" 2>/dev/null)
    VERSION=$(echo "$WORKER_HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('version','?'))" 2>/dev/null)
    pass "Worker healthy ($GPU_NAME, VRAM $VRAM, $VERSION)"
else
    fail "Worker unreachable or unhealthy: $WORKER_HEALTH"
fi

# ── Test 3: GPU Relay Health ──
header "GPU Relay (localhost:5131)"

RELAY_HEALTH=$(curl -s --max-time 5 "$RELAY_LOCAL/health" 2>/dev/null || echo "UNREACHABLE")
if echo "$RELAY_HEALTH" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['status']=='ok'" 2>/dev/null; then
    SESSIONS=$(echo "$RELAY_HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('sessions',0))" 2>/dev/null)
    VERSION=$(echo "$RELAY_HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('version','?'))" 2>/dev/null)
    pass "Relay healthy ($SESSIONS sessions, $VERSION)"
else
    fail "Relay unreachable or unhealthy: $RELAY_HEALTH"
fi

# ── Test 4: Relay Health via URL ──
header "Relay Health (via URL)"

TUNNEL_HEALTH=$(curl -s --max-time 10 "$TUNNEL_URL/health" 2>/dev/null || echo "UNREACHABLE")
if echo "$TUNNEL_HEALTH" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['status']=='ok'" 2>/dev/null; then
    pass "Relay reachable and healthy"
else
    TUNNEL_STATUS=$(curl -s --max-time 10 -o /dev/null -w "%{http_code}" "$TUNNEL_URL/health" 2>/dev/null || echo "000")
    if [ "$TUNNEL_STATUS" = "000" ]; then
        fail "Relay unreachable"
    elif [ "$TUNNEL_STATUS" = "404" ]; then
        fail "Relay 404 — ingress mismatch or stale config"
    else
        fail "Relay HTTP $TUNNEL_STATUS: $TUNNEL_HEALTH"
    fi
fi

# ── Test 5: Relay ↔ Worker Internal WS ──
header "Relay → Worker Internal WS"

# Check relay logs for recent ping activity
PING_LOG=$(docker logs gpu-worker --tail 100 2>&1 | grep "GPU worker WS alive" | tail -1)
if [ -n "$PING_LOG" ]; then
    pass "Worker pings flowing: $PING_LOG"
else
    # Check if any recent relay activity at all
    RELAY_LOG=$(docker logs gpu-worker --tail 50 2>&1 | grep "b3_gpu_relay" | tail -1)
    if [ -n "$RELAY_LOG" ]; then
        warn "No ping count logged yet (relay active: $(echo "$RELAY_LOG" | awk '{print $1}')"
    else
        warn "No relay log entries found"
    fi
fi

# Check for zombie detection warnings
ZOMBIE=$(docker logs gpu-worker --tail 200 2>&1 | grep "zombie connection" | tail -1)
if [ -n "$ZOMBIE" ]; then
    warn "Zombie detected recently: $ZOMBIE"
fi

# Check for reconnects
RECONNECTS=$(docker logs gpu-worker 2>&1 | grep -c "GPU worker WS reconnected" || true)
if [ "$RECONNECTS" -gt 0 ]; then
    warn "WS reconnected $RECONNECTS time(s) since container start"
else
    pass "No WS reconnects needed (stable connection)"
fi

# ── Test 6: WebSocket Connectivity ──
header "WebSocket Connectivity"

# Test WS to relay via tunnel
WS_URL=$(echo "$TUNNEL_URL" | sed 's|^https://|wss://|;s|^http://|ws://|')/ws
# We can't easily test WS from bash, but we can check if the upgrade endpoint exists
WS_CHECK=$(curl -s --max-time 5 -o /dev/null -w "%{http_code}" -H "Upgrade: websocket" -H "Connection: Upgrade" "$TUNNEL_URL/ws" 2>/dev/null || echo "000")
if [ "$WS_CHECK" = "101" ] || [ "$WS_CHECK" = "426" ] || [ "$WS_CHECK" = "400" ]; then
    pass "WS endpoint responsive (HTTP $WS_CHECK)"
else
    fail "WS endpoint not responsive (HTTP $WS_CHECK)"
fi

# ── Test 7: WebRTC Offer Endpoint ──
header "WebRTC Offer Endpoint"

RTC_CHECK=$(curl -s --max-time 5 -X OPTIONS "$TUNNEL_URL/webrtc/offer" -o /dev/null -w "%{http_code}" 2>/dev/null || echo "000")
if [ "$RTC_CHECK" = "204" ]; then
    pass "CORS preflight OK (HTTP 204)"
else
    warn "CORS preflight returned HTTP $RTC_CHECK"
fi

# ── Test 8: GPU Worker WS Endpoint ──
header "Worker WS Endpoint (direct)"

WORKER_WS_CHECK=$(curl -s --max-time 5 -o /dev/null -w "%{http_code}" -H "Upgrade: websocket" -H "Connection: Upgrade" "$WORKER_LOCAL/ws" 2>/dev/null || echo "000")
if [ "$WORKER_WS_CHECK" = "101" ] || [ "$WORKER_WS_CHECK" = "426" ] || [ "$WORKER_WS_CHECK" = "400" ] || [ "$WORKER_WS_CHECK" = "403" ]; then
    pass "Worker WS endpoint responsive (HTTP $WORKER_WS_CHECK)"
else
    fail "Worker WS endpoint not responsive (HTTP $WORKER_WS_CHECK)"
fi

# ── Test 10: GPU Memory ──
header "GPU Memory"

if command -v nvidia-smi &>/dev/null; then
    GPU_MEM=$(nvidia-smi --query-gpu=memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null | head -1)
    if [ -n "$GPU_MEM" ]; then
        USED=$(echo "$GPU_MEM" | cut -d, -f1 | tr -d ' ')
        TOTAL=$(echo "$GPU_MEM" | cut -d, -f2 | tr -d ' ')
        PCT=$((USED * 100 / TOTAL))
        if [ "$PCT" -gt 90 ]; then
            warn "GPU memory ${PCT}% used ($USED/$TOTAL MiB) — near capacity"
        else
            pass "GPU memory ${PCT}% used ($USED/$TOTAL MiB)"
        fi
    fi
else
    warn "nvidia-smi not available"
fi

# ── Test 11: Error Log Scan ──
header "Recent Errors"

ERROR_COUNT=$(docker logs gpu-worker --since 1h 2>&1 | grep -ciE "error|traceback|panic|fatal|killed|OOM" 2>/dev/null || true)
if [ "$ERROR_COUNT" -eq 0 ]; then
    pass "No errors in last hour"
elif [ "$ERROR_COUNT" -lt 5 ]; then
    warn "$ERROR_COUNT error(s) in last hour"
else
    fail "$ERROR_COUNT errors in last hour"
fi

# ── Test 12: End-to-End TTS → Transcription ──
header "End-to-End: TTS → Transcription"

# Generate TTS audio via GPU worker, then transcribe it back
E2E_TEXT="The quick brown fox jumps over the lazy dog"
TTS_RESULT=$(curl -s --max-time 60 "$WORKER_LOCAL/run" \
    -H "Content-Type: application/json" \
    -d "{\"input\":{\"action\":\"synthesize\",\"text\":\"$E2E_TEXT\",\"agent_id\":\"test\"}}" 2>/dev/null || echo "FAILED")

if echo "$TTS_RESULT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'audio_b64' in d or 'output' in d" 2>/dev/null; then
    # Extract audio and transcribe it
    AUDIO_B64=$(echo "$TTS_RESULT" | python3 -c "
import sys, json
d = json.load(sys.stdin)
out = d.get('output', d)
if isinstance(out, list): out = out[-1] if out else {}
print(out.get('audio_b64', ''))
" 2>/dev/null)

    if [ -n "$AUDIO_B64" ] && [ "$AUDIO_B64" != "" ]; then
        TTS_DUR=$(echo "$TTS_RESULT" | python3 -c "
import sys, json
d = json.load(sys.stdin)
out = d.get('output', d)
if isinstance(out, list): out = out[-1] if out else {}
print(f\"{out.get('duration_sec', '?')}s\")
" 2>/dev/null)
        pass "TTS generated ($TTS_DUR)"

        # Transcribe the TTS audio
        STT_RESULT=$(curl -s --max-time 60 "$WORKER_LOCAL/run" \
            -H "Content-Type: application/json" \
            -d "{\"input\":{\"action\":\"transcribe\",\"audio_b64\":\"$AUDIO_B64\",\"language\":\"en\",\"preset\":\"english-fast\",\"agent_id\":\"test\"}}" 2>/dev/null || echo "FAILED")

        STT_TEXT=$(echo "$STT_RESULT" | python3 -c "
import sys, json
d = json.load(sys.stdin)
out = d.get('output', d)
if isinstance(out, list): out = out[-1] if out else {}
print(out.get('text', out.get('whisperx_text', '')).strip().lower())
" 2>/dev/null)

        if [ -n "$STT_TEXT" ]; then
            # Check if transcription roughly matches
            MATCH=$(python3 -c "
orig = '$E2E_TEXT'.lower().split()
got = '$STT_TEXT'.lower().split()
common = set(orig) & set(got)
pct = len(common) / max(len(orig), 1) * 100
print(f'{pct:.0f}')
" 2>/dev/null || echo "0")
            if [ "$MATCH" -gt 50 ]; then
                pass "Transcription matched ${MATCH}%: \"$STT_TEXT\""
            else
                warn "Transcription ${MATCH}% match: \"$STT_TEXT\" (expected: \"$E2E_TEXT\")"
            fi
        else
            fail "Transcription returned empty"
        fi
    else
        fail "TTS returned no audio data"
    fi
else
    fail "TTS failed: $(echo "$TTS_RESULT" | head -c 100)"
fi

# ── Summary ──
echo ""
echo "=========================="
echo -e "Results: ${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC}, ${YELLOW}$WARN warnings${NC}"
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}GPU pipeline has failures — investigate above.${NC}"
    exit 1
elif [ "$WARN" -gt 0 ]; then
    echo -e "${YELLOW}GPU pipeline operational with warnings.${NC}"
    exit 0
else
    echo -e "${GREEN}GPU pipeline fully healthy.${NC}"
    exit 0
fi
