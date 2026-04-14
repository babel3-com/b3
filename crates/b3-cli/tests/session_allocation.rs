//! Memory allocation tests for SessionStore → delta_msg path.
//!
//! Isolates the allocation churn that causes monotonic RSS growth in the daemon.
//! The daemon stores terminal data as base64 String (~10.7MB for 8MB buffer).
//! Each PTY write triggers re-encode + N consumers each clone/decode/delta/encode.
//!
//! These tests prove:
//! 1. Whether the update→delta_msg cycle leaks vs. pure allocator fragmentation
//! 2. Broadcast channel lag under realistic load
//! 3. Arc<Vec<u8>> eliminates the clone+decode churn
//! 4. Debounce interval impact on allocation rate

use base64::Engine;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex, RwLock};

// ── Inline SessionStore (avoid importing daemon internals) ──────────

#[derive(Debug, Clone)]
struct SessionBuffer {
    terminal_data: String, // base64-encoded
    rows: u16,
    cols: u16,
    write_offset: u64,
}

#[derive(Clone)]
struct SessionStore {
    buffer: Arc<RwLock<Option<SessionBuffer>>>,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            buffer: Arc::new(RwLock::new(None)),
        }
    }

    async fn update(&self, terminal_data: String, rows: u16, cols: u16, write_offset: u64) {
        let mut buf = self.buffer.write().await;
        *buf = Some(SessionBuffer {
            terminal_data,
            rows,
            cols,
            write_offset,
        });
    }

    async fn get(&self) -> Option<SessionBuffer> {
        self.buffer.read().await.clone()
    }

    async fn delta_msg(&self, prev_offset: u64) -> Option<(serde_json::Value, u64)> {
        let buf = self.get().await?;
        let full_bytes = base64::engine::general_purpose::STANDARD
            .decode(&buf.terminal_data)
            .ok()?;
        let cur_offset = buf.write_offset;
        let buf_len = full_bytes.len() as u64;

        if cur_offset <= prev_offset {
            return None;
        }

        let new_bytes = cur_offset - prev_offset;
        let msg = if new_bytes <= buf_len {
            let start = (buf_len - new_bytes) as usize;
            let delta = &full_bytes[start..];
            let delta_b64 = base64::engine::general_purpose::STANDARD.encode(delta);
            serde_json::json!({ "type": "delta", "data": delta_b64 })
        } else {
            serde_json::json!({
                "type": "full",
                "terminal_data": buf.terminal_data,
                "rows": buf.rows,
                "cols": buf.cols,
            })
        };
        Some((msg, cur_offset))
    }
}

// ── Proposed fix: Arc-based store ───────────────────────────────────

#[derive(Clone)]
struct SessionStoreArc {
    buffer: Arc<RwLock<Option<Arc<Vec<u8>>>>>,
    write_offset: Arc<RwLock<u64>>,
}

impl SessionStoreArc {
    fn new() -> Self {
        Self {
            buffer: Arc::new(RwLock::new(None)),
            write_offset: Arc::new(RwLock::new(0)),
        }
    }

    async fn update(&self, raw_bytes: Vec<u8>, write_offset: u64) {
        *self.buffer.write().await = Some(Arc::new(raw_bytes));
        *self.write_offset.write().await = write_offset;
    }

    async fn delta_msg(&self, prev_offset: u64) -> Option<(serde_json::Value, u64)> {
        let data = self.buffer.read().await.clone()?;
        let cur_offset = *self.write_offset.read().await;
        let buf_len = data.len() as u64;

        if cur_offset <= prev_offset {
            return None;
        }

        let new_bytes = cur_offset - prev_offset;
        let msg = if new_bytes <= buf_len {
            let start = (buf_len - new_bytes) as usize;
            let delta = &data[start..];
            let delta_b64 = base64::engine::general_purpose::STANDARD.encode(delta);
            serde_json::json!({ "type": "delta", "data": delta_b64 })
        } else {
            let full_b64 = base64::engine::general_purpose::STANDARD.encode(&**data);
            serde_json::json!({
                "type": "full",
                "terminal_data": full_b64,
            })
        };
        Some((msg, cur_offset))
    }
}

// ── Helper: read RSS from /proc/self/status ─────────────────────────

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| {
                    l.split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse::<u64>().ok())
                })
        })
        .unwrap_or(0)
}

// ════════════════════════════════════════════════════════════════════
// Test 1: SessionStore allocation churn
// ════════════════════════════════════════════════════════════════════
//
// Isolates whether update→delta_msg cycle itself leaks, or if it's
// purely glibc allocator fragmentation (allocations freed but RSS
// doesn't shrink).

#[tokio::test]
async fn test_session_store_allocation_churn() {
    let store = SessionStore::new();
    let raw_buffer = vec![0x41u8; 8 * 1024 * 1024]; // 8MB of 'A'
    let num_consumers = 2;
    let iterations = 1000;

    // Warm up — first iteration primes allocator
    {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_buffer);
        store.update(b64, 24, 80, raw_buffer.len() as u64).await;
        for _ in 0..num_consumers {
            let _ = store.delta_msg(0).await;
        }
    }

    let rss_start = rss_kb();
    let mut peak_rss: u64 = rss_start;
    let start_time = Instant::now();

    // Checkpoint at 10%, 50%, 100%
    let checkpoints = [100, 500, 1000];
    let mut checkpoint_rss = Vec::new();

    for i in 1..=iterations as u64 {
        // Simulate PTY pusher: encode 8MB → ~10.7MB base64 String
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_buffer);
        let offset = i * raw_buffer.len() as u64;
        store.update(b64, 24, 80, offset).await;

        // Simulate N consumers each calling delta_msg
        // Each call: clone SessionBuffer (10.7MB String), decode base64 (8MB alloc),
        // slice delta, re-encode delta to base64
        let prev_offset = (i - 1) * raw_buffer.len() as u64;
        for _ in 0..num_consumers {
            let result = store.delta_msg(prev_offset).await;
            assert!(result.is_some(), "delta_msg should return Some");
            // Drop result — this is where glibc should free the allocations
            // (but may not return pages to OS due to fragmentation)
        }

        let current_rss = rss_kb();
        if current_rss > peak_rss {
            peak_rss = current_rss;
        }

        if checkpoints.contains(&(i as usize)) {
            checkpoint_rss.push((i, current_rss));
        }
    }

    let elapsed = start_time.elapsed();
    let rss_end = rss_kb();
    let rss_growth = rss_end.saturating_sub(rss_start);

    println!("\n═══ Test 1: SessionStore Allocation Churn ═══");
    println!("Iterations:     {iterations}");
    println!("Consumers:      {num_consumers}");
    println!("Buffer size:    8 MB raw → ~10.7 MB base64");
    println!("Elapsed:        {:.2}s", elapsed.as_secs_f64());
    println!("RSS start:      {} KB ({:.1} MB)", rss_start, rss_start as f64 / 1024.0);
    println!("RSS end:        {} KB ({:.1} MB)", rss_end, rss_end as f64 / 1024.0);
    println!("RSS peak:       {} KB ({:.1} MB)", peak_rss, peak_rss as f64 / 1024.0);
    println!("RSS growth:     {} KB ({:.1} MB)", rss_growth, rss_growth as f64 / 1024.0);
    println!("Growth/iter:    {:.2} KB", rss_growth as f64 / iterations as f64);
    println!();
    println!("Checkpoints:");
    for (iter, rss) in &checkpoint_rss {
        let growth = rss.saturating_sub(rss_start);
        println!(
            "  iter {:>5}: RSS = {} KB ({:.1} MB), growth = {} KB",
            iter,
            rss,
            *rss as f64 / 1024.0,
            growth
        );
    }
    println!();

    // Analysis: if growth is > 50MB over 1000 iterations, it's either a leak
    // or severe fragmentation. The allocations SHOULD be freed each iteration.
    // glibc malloc with MMAP_THRESHOLD at 128KB should mmap these large allocs
    // and munmap on free — but the base64 String may be just under the threshold
    // depending on delta size.
    let growth_mb = rss_growth as f64 / 1024.0;
    if growth_mb > 50.0 {
        println!(
            "⚠ RSS grew {:.1} MB over {} iterations — likely fragmentation or leak",
            growth_mb, iterations
        );
    } else {
        println!(
            "✓ RSS growth {:.1} MB is within acceptable range (allocations being freed)",
            growth_mb
        );
    }
}

// ════════════════════════════════════════════════════════════════════
// Test 2: Broadcast channel lag under load
// ════════════════════════════════════════════════════════════════════
//
// Proves whether 5ms debounce is too aggressive for the delta
// computation cost. Sends SessionUpdated events at various intervals
// and measures how quickly receivers fall behind.
//
// NOTE: This test runs for 10 seconds real-time with 2000 events ×
// 2 receivers × full delta_msg work. Expected runtime: 30-60s
// depending on machine (ARM will be slower).

#[tokio::test]
async fn test_broadcast_channel_lag() {
    // Simplified event — we only care about SessionUpdated
    #[derive(Debug, Clone)]
    enum TestEvent {
        SessionUpdated { seq: u64 },
    }

    let (tx, _) = broadcast::channel::<TestEvent>(1024);
    let store = SessionStore::new();
    let raw_buffer = vec![0x42u8; 8 * 1024 * 1024]; // 8MB

    let num_receivers = 2;
    let test_duration = Duration::from_secs(10);
    let send_interval = Duration::from_millis(5); // Match daemon's 5ms debounce

    // Pre-populate store
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_buffer);
    store.update(b64, 24, 80, raw_buffer.len() as u64).await;

    // Spawn receivers — each does the delta_msg work
    let lag_counts: Vec<Arc<std::sync::atomic::AtomicU64>> = (0..num_receivers)
        .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0)))
        .collect();
    let event_counts: Vec<Arc<std::sync::atomic::AtomicU64>> = (0..num_receivers)
        .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0)))
        .collect();

    let mut handles = Vec::new();

    for i in 0..num_receivers {
        let mut rx = tx.subscribe();
        let store = store.clone();
        let lag = lag_counts[i].clone();
        let events = event_counts[i].clone();
        let offset = Arc::new(Mutex::new(raw_buffer.len() as u64));

        handles.push(tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(TestEvent::SessionUpdated { .. }) => {
                        events.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Simulate the real consumer work: delta_msg
                        let mut prev = offset.lock().await;
                        if let Some((_msg, new_offset)) = store.delta_msg(*prev).await {
                            *prev = new_offset;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        lag.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                        // Catch up with full delta
                        let mut prev = offset.lock().await;
                        if let Some((_msg, new_offset)) = store.delta_msg(*prev).await {
                            *prev = new_offset;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

    // Producer: send events at 5ms intervals, updating store each time
    let start = Instant::now();
    let mut seq = 1u64;
    while start.elapsed() < test_duration {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_buffer);
        let offset = (seq + 1) * raw_buffer.len() as u64;
        store.update(b64, 24, 80, offset).await;
        let _ = tx.send(TestEvent::SessionUpdated { seq });
        seq += 1;
        tokio::time::sleep(send_interval).await;
    }
    drop(tx); // Signal receivers to stop

    for h in handles {
        let _ = h.await;
    }

    let total_sent = seq - 1;

    println!("\n═══ Test 2: Broadcast Channel Lag ═══");
    println!("Duration:       {}s", test_duration.as_secs());
    println!("Send interval:  {}ms", send_interval.as_millis());
    println!("Events sent:    {}", total_sent);
    println!("Receivers:      {}", num_receivers);
    println!("Events/sec:     {:.1}", total_sent as f64 / test_duration.as_secs_f64());
    println!();

    for i in 0..num_receivers {
        let processed = event_counts[i].load(std::sync::atomic::Ordering::Relaxed);
        let lagged = lag_counts[i].load(std::sync::atomic::Ordering::Relaxed);
        let miss_rate = if total_sent > 0 {
            (total_sent - processed) as f64 / total_sent as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  Receiver {}: processed={}, lagged={} events, miss_rate={:.1}%",
            i, processed, lagged, miss_rate
        );
    }
    println!();

    let total_lagged: u64 = lag_counts
        .iter()
        .map(|l| l.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    if total_lagged > 0 {
        println!(
            "⚠ {} total lagged events — 5ms debounce is too fast for delta_msg cost",
            total_lagged
        );
    } else {
        println!("✓ No lag — receivers kept up with 5ms event rate");
    }
}

// ════════════════════════════════════════════════════════════════════
// Test 3: Arc<Vec<u8>> vs String allocation comparison
// ════════════════════════════════════════════════════════════════════
//
// Benchmarks the proposed fix: store raw bytes in Arc<Vec<u8>> instead
// of base64 String. Consumers Arc::clone (refcount bump) instead of
// String::clone (10.7MB memcpy), skip decode entirely.

#[tokio::test]
async fn test_arc_vs_string_allocation() {
    let raw_buffer = vec![0x43u8; 8 * 1024 * 1024]; // 8MB
    let num_consumers = 2;
    let iterations = 500;

    // ── Current path: base64 String ──
    let store_string = SessionStore::new();

    let rss_before_string = rss_kb();
    let start_string = Instant::now();

    for i in 1..=iterations as u64 {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_buffer);
        let offset = i * raw_buffer.len() as u64;
        store_string.update(b64, 24, 80, offset).await;

        let prev_offset = (i - 1) * raw_buffer.len() as u64;
        for _ in 0..num_consumers {
            let _ = store_string.delta_msg(prev_offset).await;
        }
    }

    let elapsed_string = start_string.elapsed();
    let rss_after_string = rss_kb();

    // ── Proposed path: Arc<Vec<u8>> ──
    let store_arc = SessionStoreArc::new();

    let rss_before_arc = rss_kb();
    let start_arc = Instant::now();

    for i in 1..=iterations as u64 {
        // No base64 encode on update — store raw bytes
        let offset = i * raw_buffer.len() as u64;
        store_arc.update(raw_buffer.clone(), offset).await;

        let prev_offset = (i - 1) * raw_buffer.len() as u64;
        for _ in 0..num_consumers {
            let _ = store_arc.delta_msg(prev_offset).await;
        }
    }

    let elapsed_arc = start_arc.elapsed();
    let rss_after_arc = rss_kb();

    // ── Per-cycle allocation estimate (theoretical, not measured) ──
    // These are calculated from known data sizes, not from allocator stats.
    // The speedup ratio and RSS growth numbers above are the measured values.
    // String path: encode(8MB→10.7MB) + N * [clone(10.7MB) + decode(10.7MB→8MB) + encode_delta]
    // Arc path:    clone(8MB into Vec) + N * [Arc::clone(16 bytes) + encode_delta]
    let string_alloc_per_cycle = 10_700_000 + num_consumers * (10_700_000 + 8_000_000);
    let arc_alloc_per_cycle = 8_000_000 + num_consumers * 100; // Arc clone is ~free

    println!("\n═══ Test 3: Arc<Vec<u8>> vs String Allocation ═══");
    println!("Iterations:     {iterations}");
    println!("Consumers:      {num_consumers}");
    println!();
    println!("── String path (current) ──");
    println!("  Elapsed:      {:.2}s", elapsed_string.as_secs_f64());
    println!("  Time/iter:    {:.2}ms", elapsed_string.as_millis() as f64 / iterations as f64);
    println!(
        "  RSS growth:   {} KB ({:.1} MB)",
        rss_after_string.saturating_sub(rss_before_string),
        rss_after_string.saturating_sub(rss_before_string) as f64 / 1024.0
    );
    println!(
        "  Alloc/cycle:  ~{:.1} MB (encode + {} × [clone+decode])",
        string_alloc_per_cycle as f64 / 1_000_000.0,
        num_consumers
    );
    println!();
    println!("── Arc path (proposed) ──");
    println!("  Elapsed:      {:.2}s", elapsed_arc.as_secs_f64());
    println!("  Time/iter:    {:.2}ms", elapsed_arc.as_millis() as f64 / iterations as f64);
    println!(
        "  RSS growth:   {} KB ({:.1} MB)",
        rss_after_arc.saturating_sub(rss_before_arc),
        rss_after_arc.saturating_sub(rss_before_arc) as f64 / 1024.0
    );
    println!(
        "  Alloc/cycle:  ~{:.1} MB (Vec::clone + {} × Arc::clone)",
        arc_alloc_per_cycle as f64 / 1_000_000.0,
        num_consumers
    );
    println!();

    let speedup = elapsed_string.as_secs_f64() / elapsed_arc.as_secs_f64();
    let alloc_reduction = 1.0 - (arc_alloc_per_cycle as f64 / string_alloc_per_cycle as f64);
    println!("Speedup:        {:.1}x", speedup);
    println!("Alloc reduction: {:.1}%", alloc_reduction * 100.0);
}

// ════════════════════════════════════════════════════════════════════
// Test 4: Debounce interval impact
// ════════════════════════════════════════════════════════════════════
//
// Simulates PTY output at Claude streaming rates (~500 bytes every
// 5-20ms) and measures allocation churn at different debounce intervals.

#[tokio::test]
async fn test_debounce_interval_impact() {
    let debounce_intervals = [5u64, 50, 100]; // milliseconds
    let test_duration = Duration::from_secs(5);
    // Simulate Claude streaming: ~500 bytes appended per PTY write
    let pty_write_size = 500usize;
    let pty_interval = Duration::from_millis(10); // PTY writes every ~10ms
    let buffer_capacity = 8 * 1024 * 1024usize; // 8MB ring buffer

    println!("\n═══ Test 4: Debounce Interval Impact ═══");
    println!(
        "PTY write:      {} bytes every {}ms",
        pty_write_size,
        pty_interval.as_millis()
    );
    println!("Buffer:         {} MB", buffer_capacity / (1024 * 1024));
    println!("Test duration:  {}s", test_duration.as_secs());
    println!();

    for &debounce_ms in &debounce_intervals {
        let store = SessionStore::new();
        let debounce = Duration::from_millis(debounce_ms);

        // Simulate: PTY writes accumulate, debounce timer fires, full buffer re-encoded
        let mut buffer = vec![0u8; buffer_capacity];
        let mut write_offset = buffer_capacity as u64;
        let mut last_debounce = Instant::now();
        let mut events_fired = 0u64;
        let mut total_alloc_bytes = 0u64;

        let start = Instant::now();
        let rss_start = rss_kb();

        // Run simulation
        let mut pty_tick = Instant::now();
        while start.elapsed() < test_duration {
            // Simulate PTY write: append bytes to ring buffer
            if pty_tick.elapsed() >= pty_interval {
                // Rotate buffer (ring buffer simulation)
                buffer.rotate_left(pty_write_size);
                for b in buffer[buffer_capacity - pty_write_size..].iter_mut() {
                    *b = 0x44; // Fill new bytes
                }
                write_offset += pty_write_size as u64;
                pty_tick = Instant::now();
            }

            // Debounce timer fires: encode full buffer, update store, consumers read
            if last_debounce.elapsed() >= debounce {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&buffer);
                let b64_len = b64.len() as u64;
                store.update(b64, 24, 80, write_offset).await;

                // 2 consumers each call delta_msg
                let prev_offset = write_offset - pty_write_size as u64;
                for _ in 0..2 {
                    let _ = store.delta_msg(prev_offset).await;
                }

                // Estimate allocations this cycle:
                // encode: b64_len bytes
                // 2x delta_msg: each clones SessionBuffer (b64_len) + decodes (buf_cap) + encodes delta
                total_alloc_bytes += b64_len + 2 * (b64_len + buffer_capacity as u64);
                events_fired += 1;
                last_debounce = Instant::now();
            }

            // Yield to tokio
            tokio::task::yield_now().await;
        }

        let rss_end = rss_kb();
        let events_per_sec = events_fired as f64 / test_duration.as_secs_f64();
        let alloc_per_sec_mb = (total_alloc_bytes as f64 / test_duration.as_secs_f64()) / 1_000_000.0;

        println!("── Debounce: {}ms ──", debounce_ms);
        println!("  Events fired: {} ({:.1}/sec)", events_fired, events_per_sec);
        println!("  Alloc churn:  {:.1} MB/sec", alloc_per_sec_mb);
        println!(
            "  RSS growth:   {} KB ({:.1} MB)",
            rss_end.saturating_sub(rss_start),
            rss_end.saturating_sub(rss_start) as f64 / 1024.0
        );
        println!(
            "  Total alloc:  {:.1} MB",
            total_alloc_bytes as f64 / 1_000_000.0
        );
        println!();
    }
}
