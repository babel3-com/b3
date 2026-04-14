# Investigation: RTC Terminal Garbling

**Opened:** 2026-03-29 UTC
**Symptom:** Terminal fills with garbled binary data (0xB3 reliable frame bytes rendered as text) when switching from WS to RTC/AUTO mode.
**Impact:** RTC terminal delivery completely broken. Blocks testing MultiTransport failover. WS mode unaffected.
**Timeline:** Pre-existing — observed on v1195, v1196 (reverted), and v1197. Not introduced by MultiTransport changes.

## Error Signatures

```
Terminal renders raw bytes: ³ (0xB3), followed by binary data that looks like
b3-reliable frame headers (seq numbers, timestamps, CRC32, payloads).
The 0xB3 magic byte is clearly visible as the Latin-1 ³ character.
```

## Initial Hypotheses (Pre-Evidence)

| # | Hypothesis | Initial P | Category | Reasoning |
|---|-----------|-----------|----------|-----------|
| H1 | **Missing RTC framing** — daemon sends raw reliable frames on RTC data channel without the `[type:u8][len:u32LE]` wrapper | 25% | Framing | If `send_binary()` sends raw bytes instead of framed, browser sees 0xB3 as first byte, doesn't match type 0x01/0x02/0x05, falls through |
| H2 | **Init bundle too large** — terminal init exceeds 16KB data channel MTU, gets chunked by datachannel-rs internally, browser doesn't reassemble | 20% | Size/MTU | Init was capped at 256KB. Even with framing, one reliable frame wrapping 256KB of JSON would be enormous |
| H3 | **Type byte collision** — 0xB3 as first byte of raw reliable frame is read as "type" by the browser's framing parser, `len` is garbage, `payload` overflows | 20% | Framing | If data arrives without RTC framing, DataView reads wrong fields |
| H4 | **Terminal channel sends as string** — daemon sends reliable frame bytes as string instead of binary, browser's string path tries JSON.parse, fails silently, but xterm gets the raw string | 10% | Encoding | `ChannelSender::send_binary` should send as binary, but if there's a path that sends as text... |
| H5 | **Chunk reassembly corruption** — large messages get chunked (type 0x05), reassembly has an off-by-one or type mismatch, reassembled data written directly to terminal | 10% | Chunking | The old code had chunk reassembly for terminal. If reassembly fails, raw bytes hit xterm |
| H6 | **WS and RTC both delivering** — MultiTransport sends to RTC but old WS event loop also sends unreliable data, creating garbled interleaving | 10% | Delivery | With rtc_active skip removed, WS event loop still runs. If MultiTransport routes to RTC but something else sends on WS... |
| H7 | **Other/unknown** | 5% | Unknown | Something not yet considered |

## Evidence Collection Plan

1. **Browser console during RTC transition** — filter for "DAEMON-RTC", "terminal", "reliable", errors
2. **Source code: ChannelSender::send_binary** — verify it adds `[0x02][len][payload]` framing
3. **Source code: rtc.rs terminal channel** — what exactly is sent when terminal opens
4. **Source code: rtc.js terminalDc.onmessage** — trace all code paths for incoming data
5. **Daemon logs during RTC transition** — check what the daemon reports sending
6. **Size of init bundle** — how large is a typical terminal init? Does it exceed chunking thresholds?
7. **WS path during RTC** — is handleWsMessage still receiving data on WS simultaneously?

### Web Research Queries (MANDATORY)

1. "datachannel-rs send_binary framing" — does the library add its own framing?
2. "WebRTC data channel binary ArrayBuffer onmessage" — browser receiving behavior
3. "datachannel-rs message size limit chunking" — does the library auto-chunk?
