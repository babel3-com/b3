//! Shared TTS types and pure logic — WASM-safe (no I/O, no network deps).
//!
//! Used by `b3-cli` (daemon-side TTS generation) and eventually
//! compiled to WASM for browser use (replacing the JS implementation).

use serde::{Deserialize, Serialize};

// ── GPU Request Payload ─────────────────────────────────────────────

/// Default TTS generation parameters (same as browser localStorage defaults).
pub const DEFAULT_TEMPERATURE: f64 = 0.8;
pub const DEFAULT_REPETITION_PENALTY: f64 = 1.2;
pub const DEFAULT_MIN_P: f64 = 0.05;
pub const DEFAULT_TOP_P: f64 = 1.0;
pub const DEFAULT_EXAGGERATION: f64 = 0.5;
pub const DEFAULT_CFG_WEIGHT: f64 = 0.5;
pub const DEFAULT_SPLIT_CHARS: &str = ".!?";
pub const DEFAULT_MIN_CHUNK_CHARS: usize = 80;

/// Build the GPU request payload — same format as browser's `streamFromGpu()`.
///
/// Returns a JSON value suitable for posting to `{gpu_url}/run` as `{"input": <payload>}`.
pub fn build_tts_payload(
    text: &str,
    voice: &str,
    msg_id: &str,
    conditionals_b64: &str,
    split_chars: &str,
    min_chunk_chars: usize,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "action": "synthesize_stream",
        "text": text,
        "voice": voice,
        "msg_id": msg_id,
        "exaggeration": DEFAULT_EXAGGERATION,
        "cfg_weight": DEFAULT_CFG_WEIGHT,
        "temperature": DEFAULT_TEMPERATURE,
        "repetition_penalty": DEFAULT_REPETITION_PENALTY,
        "min_p": DEFAULT_MIN_P,
        "top_p": DEFAULT_TOP_P,
    });

    if !split_chars.is_empty() {
        payload["split_chars"] = serde_json::Value::String(split_chars.to_string());
    }
    if min_chunk_chars > 0 {
        payload["min_chunk_chars"] = serde_json::Value::Number(min_chunk_chars.into());
    }
    if !conditionals_b64.is_empty() {
        payload["conditionals_b64"] = serde_json::Value::String(conditionals_b64.to_string());
    }

    payload
}

// ── Stream Response Parsing ─────────────────────────────────────────

/// A decoded audio chunk from the GPU stream.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub chunk_index: usize,
    pub audio_bytes: Vec<u8>,
    pub sample_rate: u32,
    pub duration_sec: f64,
}

/// Result of parsing a stream poll response.
#[derive(Debug)]
pub struct StreamParseResult {
    /// New audio chunks found in this poll.
    pub chunks: Vec<AudioChunk>,
    /// Whether the stream is complete.
    pub done: bool,
    /// Total chunks expected (set when done).
    pub total_chunks: Option<usize>,
}

/// Parse a GPU stream poll response body.
///
/// The response format from RunPod `/stream/{jobId}`:
/// ```json
/// {"stream": [{"output": {"chunk_index": 0, "audio_b64": "...", ...}}, ...]}
/// ```
/// Or from local GPU:
/// ```json
/// [{"output": {"chunk_index": 0, "audio_b64": "...", ...}}, ...]
/// ```
pub fn parse_stream_response(body: &serde_json::Value) -> StreamParseResult {
    let mut chunks = Vec::new();
    let mut done = false;
    let mut total_chunks = None;

    // Extract the outputs array — RunPod wraps in {"stream": [...]}, local returns [...]
    let outputs = body
        .get("stream")
        .and_then(|s| s.as_array())
        .or_else(|| body.as_array());

    let items = match outputs {
        Some(arr) => arr.clone(),
        None => return StreamParseResult { chunks, done, total_chunks },
    };

    for item in &items {
        // RunPod wraps each chunk in {"output": {...}}, local may not
        let chunk = item.get("output").unwrap_or(item);

        // Check for done marker
        if chunk.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            done = true;
            total_chunks = chunk.get("total_chunks").and_then(|t| t.as_u64()).map(|t| t as usize);
            continue;
        }

        // Skip progress events
        if chunk.get("progress").is_some() {
            continue;
        }

        // Parse audio chunk
        let audio_b64 = match chunk.get("audio_b64").and_then(|a| a.as_str()) {
            Some(b64) => b64,
            None => continue,
        };

        let chunk_index = chunk
            .get("chunk_index")
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as usize;

        let sample_rate = chunk
            .get("sample_rate")
            .and_then(|s| s.as_u64())
            .unwrap_or(24000) as u32;

        let duration_sec = chunk
            .get("duration_sec")
            .and_then(|d| d.as_f64())
            .unwrap_or(0.0);

        // Decode base64 audio
        match base64_decode(audio_b64) {
            Ok(bytes) => {
                chunks.push(AudioChunk {
                    chunk_index,
                    audio_bytes: bytes,
                    sample_rate,
                    duration_sec,
                });
            }
            Err(_) => {
                // Skip malformed chunks
                continue;
            }
        }
    }

    StreamParseResult { chunks, done, total_chunks }
}

/// Simple base64 decoder — avoids adding the `base64` crate dependency.
/// Handles standard base64 with optional padding.
fn base64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    const TABLE: [u8; 128] = {
        let mut t = [255u8; 128];
        let mut i = 0u8;
        while i < 26 {
            t[(b'A' + i) as usize] = i;
            t[(b'a' + i) as usize] = i + 26;
            i += 1;
        }
        let mut d = 0u8;
        while d < 10 {
            t[(b'0' + d) as usize] = d + 52;
            d += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let input = input.trim_end_matches('=');
    let len = input.len();
    let mut out = Vec::with_capacity(len * 3 / 4);

    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < len {
        let a = *TABLE.get(bytes[i] as usize).ok_or("invalid")?;
        let b = *TABLE.get(bytes[i + 1] as usize).ok_or("invalid")?;
        let c = *TABLE.get(bytes[i + 2] as usize).ok_or("invalid")?;
        let d = *TABLE.get(bytes[i + 3] as usize).ok_or("invalid")?;
        if a == 255 || b == 255 || c == 255 || d == 255 {
            return Err("invalid base64 character");
        }
        let n = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | (d as u32);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
        i += 4;
    }

    let remaining = len - i;
    if remaining == 2 {
        let a = *TABLE.get(bytes[i] as usize).ok_or("invalid")?;
        let b = *TABLE.get(bytes[i + 1] as usize).ok_or("invalid")?;
        if a == 255 || b == 255 {
            return Err("invalid base64 character");
        }
        out.push(((a as u32) << 2 | (b as u32) >> 4) as u8);
    } else if remaining == 3 {
        let a = *TABLE.get(bytes[i] as usize).ok_or("invalid")?;
        let b = *TABLE.get(bytes[i + 1] as usize).ok_or("invalid")?;
        let c = *TABLE.get(bytes[i + 2] as usize).ok_or("invalid")?;
        if a == 255 || b == 255 || c == 255 {
            return Err("invalid base64 character");
        }
        out.push(((a as u32) << 2 | (b as u32) >> 4) as u8);
        out.push(((b as u32) << 4 | (c as u32) >> 2) as u8);
    }

    Ok(out)
}

// ── WAV Stitching ───────────────────────────────────────────────────

const TARGET_SAMPLE_RATE: u32 = 24_000;
const HEADER_SIZE: usize = 44;

/// Read the sample rate from bytes 24-27 of a WAV header (little-endian u32).
fn wav_sample_rate(chunk: &[u8]) -> u32 {
    if chunk.len() < 28 {
        return TARGET_SAMPLE_RATE;
    }
    u32::from_le_bytes([chunk[24], chunk[25], chunk[26], chunk[27]])
}

/// Resample mono 16-bit PCM to TARGET_SAMPLE_RATE using nearest-neighbour
/// decimation (for integer downsampling ratios) or linear interpolation
/// (for upsampling). Sufficient quality for speech.
fn resample_pcm(pcm: &[u8], src_rate: u32, dst_rate: u32) -> Vec<u8> {
    if src_rate == dst_rate || pcm.len() < 2 {
        return pcm.to_vec();
    }
    // PCM is 16-bit little-endian samples.
    let sample_count = pcm.len() / 2;
    let dst_samples = (sample_count as u64 * dst_rate as u64 / src_rate as u64) as usize;
    let mut out = Vec::with_capacity(dst_samples * 2);
    for i in 0..dst_samples {
        // Map output sample index back to input index (fixed-point).
        let src_pos = i as f64 * src_rate as f64 / dst_rate as f64;
        let src_i = src_pos as usize;
        let frac = src_pos - src_i as f64;
        let s0 = if src_i < sample_count {
            i16::from_le_bytes([pcm[src_i * 2], pcm[src_i * 2 + 1]]) as f64
        } else {
            0.0
        };
        let s1 = if src_i + 1 < sample_count {
            i16::from_le_bytes([pcm[(src_i + 1) * 2], pcm[(src_i + 1) * 2 + 1]]) as f64
        } else {
            s0
        };
        let interpolated = (s0 + frac * (s1 - s0)).round() as i16;
        out.extend_from_slice(&interpolated.to_le_bytes());
    }
    out
}

/// Stitch multiple WAV chunks into a single WAV file at TARGET_SAMPLE_RATE (24kHz).
///
/// Each chunk's sample rate is read from its WAV header (bytes 24-27). Chunks
/// at a different rate are resampled to 24kHz before concatenation. This handles
/// mixed-rate output from the dual-backend daemon (local GPU=24kHz, RunPod may vary).
///
/// Output header: 24kHz, mono, 16-bit PCM, 44-byte.
pub fn stitch_wav_chunks(chunks: &[Vec<u8>]) -> Option<Vec<u8>> {
    if chunks.is_empty() {
        return None;
    }
    if chunks.len() == 1 {
        // Single chunk: resample if needed, return as-is otherwise.
        let src_rate = wav_sample_rate(&chunks[0]);
        if src_rate == TARGET_SAMPLE_RATE || chunks[0].len() <= HEADER_SIZE {
            return Some(chunks[0].clone());
        }
        let pcm = resample_pcm(&chunks[0][HEADER_SIZE..], src_rate, TARGET_SAMPLE_RATE);
        let mut out = Vec::with_capacity(HEADER_SIZE + pcm.len());
        out.extend_from_slice(&chunks[0][..HEADER_SIZE]);
        patch_wav_header(&mut out, pcm.len());
        out.extend_from_slice(&pcm);
        return Some(out);
    }

    // Collect resampled PCM from each chunk.
    let mut pcm_parts: Vec<Vec<u8>> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        if chunk.len() <= HEADER_SIZE {
            continue;
        }
        let src_rate = wav_sample_rate(chunk);
        let raw_pcm = &chunk[HEADER_SIZE..];
        if src_rate == TARGET_SAMPLE_RATE {
            pcm_parts.push(raw_pcm.to_vec());
        } else {
            pcm_parts.push(resample_pcm(raw_pcm, src_rate, TARGET_SAMPLE_RATE));
        }
    }

    if pcm_parts.is_empty() {
        return None;
    }

    let total_pcm: usize = pcm_parts.iter().map(|p| p.len()).sum();

    // Build combined WAV: first chunk's header (patched to 24kHz) + all PCM.
    if chunks[0].len() < HEADER_SIZE {
        return None;
    }
    let mut combined = Vec::with_capacity(HEADER_SIZE + total_pcm);
    combined.extend_from_slice(&chunks[0][..HEADER_SIZE]);

    // Ensure header declares TARGET_SAMPLE_RATE and correct byte rates.
    combined[24..28].copy_from_slice(&TARGET_SAMPLE_RATE.to_le_bytes());
    // ByteRate = SampleRate * NumChannels * BitsPerSample/8 = 24000 * 1 * 2 = 48000
    combined[28..32].copy_from_slice(&(TARGET_SAMPLE_RATE * 2).to_le_bytes());

    patch_wav_header(&mut combined, total_pcm);
    for pcm in &pcm_parts {
        combined.extend_from_slice(pcm);
    }

    Some(combined)
}

/// Patch RIFF and data sub-chunk size fields for a given PCM byte count.
fn patch_wav_header(wav: &mut Vec<u8>, pcm_len: usize) {
    let riff_size = (36 + pcm_len) as u32;
    wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
    let data_size = pcm_len as u32;
    wav[40..44].copy_from_slice(&data_size.to_le_bytes());
}

// ── Archive Entry Type ──────────────────────────────────────────────

/// TTS archive entry — metadata for a stored voice message.
/// Used by both the daemon (disk storage) and browser (history hydration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsEntry {
    pub msg_id: String,
    /// Cleaned message text (post TTS processing).
    pub text: String,
    /// Original text before TTS cleaning — for matching in browser overlay.
    #[serde(default)]
    pub original_text: String,
    pub voice: String,
    pub emotion: String,
    /// What the agent was responding to (user transcription or context summary).
    #[serde(default)]
    pub replying_to: String,
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
    /// Audio duration in seconds.
    pub duration_sec: f64,
    /// WAV file size in bytes.
    pub size_bytes: u64,
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tts_payload() {
        let payload = build_tts_payload("Hello world", "arabella", "msg-123", "", ".!?", 80);
        assert_eq!(payload["action"], "synthesize_stream");
        assert_eq!(payload["text"], "Hello world");
        assert_eq!(payload["voice"], "arabella");
        assert_eq!(payload["msg_id"], "msg-123");
        assert_eq!(payload["temperature"], DEFAULT_TEMPERATURE);
        assert!(payload.get("conditionals_b64").is_none());
    }

    #[test]
    fn test_build_tts_payload_with_conditionals() {
        let payload = build_tts_payload("Hi", "custom", "m1", "abc123base64", ".!?", 80);
        assert_eq!(payload["conditionals_b64"], "abc123base64");
    }

    #[test]
    fn test_stitch_wav_single_chunk() {
        let chunk = vec![0u8; 100]; // 44 header + 56 PCM
        let result = stitch_wav_chunks(&[chunk.clone()]);
        assert_eq!(result, Some(chunk));
    }

    #[test]
    fn test_stitch_wav_empty() {
        let result = stitch_wav_chunks(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_stitch_wav_two_chunks() {
        // Create two minimal WAV chunks with known PCM data
        let mut chunk1 = vec![0u8; 44]; // header
        chunk1[0..4].copy_from_slice(b"RIFF");
        chunk1.extend_from_slice(&[1, 2, 3, 4]); // 4 bytes PCM

        let mut chunk2 = vec![0u8; 44]; // header
        chunk2[0..4].copy_from_slice(b"RIFF");
        chunk2.extend_from_slice(&[5, 6, 7, 8]); // 4 bytes PCM

        let result = stitch_wav_chunks(&[chunk1, chunk2]).unwrap();

        // Should have 44 header + 8 PCM bytes
        assert_eq!(result.len(), 52);

        // RIFF size at bytes 4-7: 36 + 8 = 44
        let riff_size = u32::from_le_bytes([result[4], result[5], result[6], result[7]]);
        assert_eq!(riff_size, 44);

        // Data size at bytes 40-43: 8
        let data_size = u32::from_le_bytes([result[40], result[41], result[42], result[43]]);
        assert_eq!(data_size, 8);

        // PCM data should be concatenated
        assert_eq!(&result[44..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_base64_decode() {
        assert_eq!(base64_decode("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(base64_decode("SGVsbG8").unwrap(), b"Hello"); // no padding
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
    }

    #[test]
    fn test_parse_stream_response_runpod_format() {
        let body = serde_json::json!({
            "stream": [
                {"output": {"chunk_index": 0, "audio_b64": "AAAA", "sample_rate": 24000, "duration_sec": 1.5}},
                {"output": {"progress": {"stage": "generating"}}},
                {"output": {"done": true, "total_chunks": 1, "msg_id": "test"}}
            ]
        });

        let result = parse_stream_response(&body);
        assert_eq!(result.chunks.len(), 1);
        assert_eq!(result.chunks[0].chunk_index, 0);
        assert_eq!(result.chunks[0].sample_rate, 24000);
        assert!(result.done);
        assert_eq!(result.total_chunks, Some(1));
    }

    #[test]
    fn test_tts_entry_serialize() {
        let entry = TtsEntry {
            msg_id: "abc".to_string(),
            text: "Hello world".to_string(),
            original_text: "**Hello** world".to_string(),
            voice: "arabella".to_string(),
            emotion: "warm".to_string(),
            replying_to: "user said something".to_string(),
            timestamp: 1741654321,
            duration_sec: 4.2,
            size_bytes: 100000,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""msg_id":"abc""#));
        assert!(json.contains(r#""text":"Hello world""#));

        let parsed: TtsEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.msg_id, "abc");
    }
}
