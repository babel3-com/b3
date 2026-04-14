//! GPU streaming client for daemon-side TTS generation.
//!
//! Implements the same protocol as the browser's `gpuRun()` + stream polling:
//! 1. POST `/run` to local GPU first (2s timeout), fallback to RunPod
//! 2. Poll `/stream/{jobId}` every 400ms for audio chunks
//! 3. Stitch WAV chunks via `b3_common::tts::stitch_wav_chunks()`

use std::collections::HashMap;
use std::time::Duration;

use b3_common::tts;
use tokio::sync::RwLock;

/// GPU client for submitting TTS jobs and collecting audio.
///
/// Reads GPU configuration from env vars lazily (on each call) because
/// the daemon loads `~/.b3/runpod.env` AFTER WebState construction.
pub struct GpuClient {
    client: reqwest::Client,
    /// Cached voice conditionals: voice name → base64 .pt data.
    conditionals_cache: RwLock<HashMap<String, CachedConditional>>,
}

struct CachedConditional {
    data: String,
    fetched_at: std::time::Instant,
}

/// GPU backend configuration, read from env vars on demand.
struct GpuConfig {
    local_gpu_url: Option<String>,
    local_gpu_token: String,
    runpod_base_url: Option<String>,
    runpod_token: String,
}

impl GpuConfig {
    fn from_env() -> Self {
        let local_gpu_url = std::env::var("LOCAL_GPU_URL").ok().filter(|s| !s.is_empty());
        let local_gpu_token = std::env::var("LOCAL_GPU_TOKEN").unwrap_or_default();
        let runpod_gpu_id = std::env::var("RUNPOD_GPU_ID").ok().filter(|s| !s.is_empty());
        let runpod_api_key = std::env::var("RUNPOD_API_KEY").unwrap_or_default();
        let runpod_base_url = runpod_gpu_id
            .map(|id| format!("https://api.runpod.ai/v2/{id}"));
        Self { local_gpu_url, local_gpu_token, runpod_base_url, runpod_token: runpod_api_key }
    }
}

const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const LOCAL_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(400);
const MAX_POLL_TIME: Duration = Duration::from_secs(300); // 5 minute max generation

impl GpuClient {
    /// Create a new GPU client. Env vars are read lazily on each call.
    pub fn from_env() -> Self {
        Self {
            // Disable connection pooling for the Cloudflare tunnel path.
            // Default reqwest pooling keeps idle connections alive, but Cloudflare
            // quick tunnels silently drop idle HTTP connections without FIN/RST.
            // The next request reuses the dead pooled connection → broken pipe.
            // pool_max_idle_per_host(0) forces a fresh TCP connection per request.
            // Cost: ~5ms extra per TTS job over localhost tunnel — negligible.
            client: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("reqwest client"),
            conditionals_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Submit an async job to the GPU — local-first with RunPod fallback.
    ///
    /// Returns `(job_id, base_url)` for subsequent polling.
    async fn submit_job(
        &self,
        input: &serde_json::Value,
    ) -> anyhow::Result<(String, String)> {
        let cfg = GpuConfig::from_env();
        let body = serde_json::json!({"input": input});

        // Try local GPU first
        if let Some(ref local_url) = cfg.local_gpu_url {
            let run_url = format!("{local_url}/run");
            let mut req = self.client.post(&run_url)
                .json(&body)
                .timeout(LOCAL_TIMEOUT);

            if !cfg.local_gpu_token.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", cfg.local_gpu_token));
            }

            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let data: serde_json::Value = resp.json().await?;
                    if let Some(id) = data.get("id").and_then(|i| i.as_str()) {
                        tracing::info!(job_id = %id, "GPU job submitted to local-gpu");
                        return Ok((id.to_string(), local_url.clone()));
                    }
                }
                Ok(resp) => {
                    tracing::warn!(status = %resp.status(), "Local GPU /run failed, falling back to RunPod");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Local GPU unreachable, falling back to RunPod");
                }
            }
        }

        // Fallback to RunPod
        let runpod_url = cfg.runpod_base_url.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No GPU backend available (no local GPU, no RUNPOD_GPU_ID)"))?;

        let run_url = format!("{runpod_url}/run");
        let mut req = self.client.post(&run_url).json(&body);

        if !cfg.runpod_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.runpod_token));
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("RunPod /run failed: HTTP {status} — {body}");
        }

        let data: serde_json::Value = resp.json().await?;
        let id = data.get("id").and_then(|i| i.as_str())
            .ok_or_else(|| anyhow::anyhow!("RunPod /run response missing 'id'"))?;

        tracing::info!(job_id = %id, "GPU job submitted to runpod-cloud");
        Ok((id.to_string(), runpod_url.clone()))
    }

    /// Poll `/stream/{job_id}` until all chunks are received or timeout.
    ///
    /// Returns ordered audio chunks (raw WAV bytes, decoded from base64).
    async fn poll_stream(
        &self,
        job_id: &str,
        base_url: &str,
    ) -> anyhow::Result<Vec<Vec<u8>>> {
        let cfg = GpuConfig::from_env();
        let stream_url = format!("{base_url}/stream/{job_id}");
        let status_url = format!("{base_url}/status/{job_id}");

        let is_runpod = base_url.contains("runpod.ai");
        let auth_header = if is_runpod && !cfg.runpod_token.is_empty() {
            Some(format!("Bearer {}", cfg.runpod_token))
        } else if !is_runpod && !cfg.local_gpu_token.is_empty() {
            Some(format!("Bearer {}", cfg.local_gpu_token))
        } else {
            None
        };

        let mut all_chunks: Vec<Option<Vec<u8>>> = Vec::new();
        let start = std::time::Instant::now();
        let mut done = false;
        let mut consecutive_empty = 0u32;

        while !done && start.elapsed() < MAX_POLL_TIME {
            tokio::time::sleep(POLL_INTERVAL).await;

            // Poll stream endpoint
            let mut req = self.client.get(&stream_url);
            if let Some(ref auth) = auth_header {
                req = req.header("Authorization", auth);
            }

            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await?;
                    let result = tts::parse_stream_response(&body);

                    if !result.chunks.is_empty() {
                        consecutive_empty = 0;
                        for chunk in &result.chunks {
                            // Ensure vec is large enough
                            while all_chunks.len() <= chunk.chunk_index {
                                all_chunks.push(None);
                            }
                            all_chunks[chunk.chunk_index] = Some(chunk.audio_bytes.clone());
                        }
                    } else {
                        consecutive_empty += 1;
                    }

                    if result.done {
                        done = true;
                    }
                }
                Ok(resp) if resp.status().as_u16() == 404 => {
                    // Job not ready yet (cold start) — keep polling
                    consecutive_empty += 1;
                }
                Ok(resp) => {
                    tracing::warn!(status = %resp.status(), "Unexpected stream poll response");
                    consecutive_empty += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Stream poll error");
                    consecutive_empty += 1;
                }
            }

            // After many empty polls, check job status
            if consecutive_empty >= 10 && consecutive_empty % 10 == 0 {
                let mut req = self.client.get(&status_url);
                if let Some(ref auth) = auth_header {
                    req = req.header("Authorization", auth);
                }
                if let Ok(resp) = req.send().await {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        let status = body.get("status").and_then(|s| s.as_str()).unwrap_or("");
                        if status == "COMPLETED" || status == "FAILED" || status == "CANCELLED" {
                            tracing::info!(status, "Job ended via status check");
                            done = true;
                        }
                    }
                }
            }
        }

        if !done {
            anyhow::bail!("TTS generation timed out after {:?}", MAX_POLL_TIME);
        }

        // Collect ordered chunks, skip any gaps
        let ordered: Vec<Vec<u8>> = all_chunks.into_iter().flatten().collect();
        if ordered.is_empty() {
            anyhow::bail!("No audio chunks received");
        }

        Ok(ordered)
    }

    /// Fetch voice conditionals from EC2, with caching.
    pub async fn fetch_conditionals(
        &self,
        voice: &str,
        api_url: &str,
        agent_id: &str,
        api_key: &str,
    ) -> String {
        // Check cache first
        {
            let cache = self.conditionals_cache.read().await;
            if let Some(cached) = cache.get(voice) {
                if cached.fetched_at.elapsed() < CACHE_TTL {
                    return cached.data.clone();
                }
            }
        }

        // Fetch from EC2 — system voice first, then user voice
        let result = self.fetch_conditionals_uncached(voice, api_url, agent_id, api_key).await;

        // Cache the result
        if !result.is_empty() {
            let mut cache = self.conditionals_cache.write().await;
            cache.insert(voice.to_string(), CachedConditional {
                data: result.clone(),
                fetched_at: std::time::Instant::now(),
            });
        }

        result
    }

    /// Get cached conditionals for a voice without fetching.
    /// Returns None if not cached or expired.
    pub async fn get_cached_conditionals(&self, voice: &str) -> Option<String> {
        let cache = self.conditionals_cache.read().await;
        cache.get(voice)
            .filter(|c| c.fetched_at.elapsed() < CACHE_TTL)
            .map(|c| c.data.clone())
    }

    async fn fetch_conditionals_uncached(
        &self,
        voice: &str,
        api_url: &str,
        agent_id: &str,
        api_key: &str,
    ) -> String {
        // Try system default voice (no auth)
        let url = format!("{api_url}/api/default-voices/{voice}");
        if let Ok(resp) = self.client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    if let Some(b64) = data.get("conditionals_b64").and_then(|v| v.as_str()) {
                        if !b64.is_empty() {
                            return b64.to_string();
                        }
                    }
                }
            }
        }

        // Try user custom voice
        let url = format!("{api_url}/api/agents/{agent_id}/user-voices/{voice}");
        if let Ok(resp) = self.client.get(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    if let Some(b64) = data.get("conditionals_b64").and_then(|v| v.as_str()) {
                        if !b64.is_empty() {
                            return b64.to_string();
                        }
                    }
                }
            }
        }

        String::new()
    }

    /// Full TTS generation pipeline: build payload → submit → poll → stitch.
    ///
    /// Returns the complete stitched WAV bytes and duration.
    pub async fn generate_tts(
        &self,
        text: &str,
        voice: &str,
        msg_id: &str,
        api_url: &str,
        agent_id: &str,
        api_key: &str,
    ) -> anyhow::Result<(Vec<u8>, f64)> {
        // 1. Fetch voice conditionals
        let conditionals = self.fetch_conditionals(voice, api_url, agent_id, api_key).await;

        // 2. Build payload (same format as browser)
        let mut payload = tts::build_tts_payload(
            text,
            voice,
            msg_id,
            &conditionals,
            tts::DEFAULT_SPLIT_CHARS,
            tts::DEFAULT_MIN_CHUNK_CHARS,
        );
        // Inject agent_id for GPU worker billing
        payload["agent_id"] = serde_json::Value::String(agent_id.to_string());

        // 3. Submit job
        let (job_id, base_url) = self.submit_job(&payload).await?;

        // 4. Poll for chunks
        let wav_chunks = self.poll_stream(&job_id, &base_url).await?;

        // 5. Calculate total duration from PCM data
        // 24kHz, 16-bit mono = 48000 bytes per second
        let total_pcm_bytes: usize = wav_chunks.iter()
            .map(|c| if c.len() > 44 { c.len() - 44 } else { 0 })
            .sum();
        let duration_sec = total_pcm_bytes as f64 / 48000.0;

        // 6. Stitch WAV chunks
        let wav = tts::stitch_wav_chunks(&wav_chunks)
            .ok_or_else(|| anyhow::anyhow!("Failed to stitch WAV chunks"))?;

        tracing::info!(
            msg_id,
            chunks = wav_chunks.len(),
            duration_sec = format!("{:.1}", duration_sec),
            size_bytes = wav.len(),
            "TTS generation complete"
        );

        Ok((wav, duration_sec))
    }
}
