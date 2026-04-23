//! Session Memory MCP server.
//!
//! Surfaces moments from Claude Code session JSONL files via five tools:
//! - `find_active_session` — most recently modified session file
//! - `get_session_info`    — metadata (timestamps, line count, files written)
//! - `list_sessions`       — all sessions sorted by size
//! - `resurface`           — extract/filter session content with sampling
//! - `add_session_dir`     — persist a directory to ~/.b3/session-dirs.json
//!
//! Extra session directories are loaded from ~/.b3/session-dirs.json (JSON array
//! of path strings), then appended with SESSION_EXTRA_DIRS env var if set.
//!
//! Runs as `b3 mcp session-memory` (stdio transport, spawned by Claude Code).
//! Mirrors the Python `session-memory.py` server — same tool signatures,
//! same filter semantics, no subprocess dependencies.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::DateTime;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// JSON-RPC types (identical pattern to voice.rs)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct JsonRpcRequest {
    method: String,
    id: Option<Value>,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Session directory discovery
// ---------------------------------------------------------------------------

fn discover_session_dirs() -> Vec<PathBuf> {
    // Explicit SESSION_DIRS override
    if let Ok(explicit) = std::env::var("SESSION_DIRS") {
        if !explicit.is_empty() {
            return explicit
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect();
        }
    }

    let mut dirs = Vec::new();

    // Auto-discover from ~/.claude/projects/
    if let Some(home) = dirs::home_dir() {
        let claude_projects = home.join(".claude").join("projects");
        if claude_projects.is_dir() {
            if let Ok(entries) = fs::read_dir(&claude_projects) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        // Include if it contains any .jsonl files
                        if fs::read_dir(&p)
                            .ok()
                            .map(|mut e| e.any(|f| {
                                f.ok().map(|f| {
                                    f.path().extension()
                                        .map(|ex| ex == "jsonl")
                                        .unwrap_or(false)
                                }).unwrap_or(false)
                            }))
                            .unwrap_or(false)
                        {
                            let canonical = fs::canonicalize(&p).unwrap_or(p);
                            dirs.push(canonical);
                        }
                    }
                }
            }
        }
    }

    dirs
}

/// Path to the persistent extra-dirs config: ~/.b3/session-dirs.json
fn session_dirs_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".b3").join("session-dirs.json"))
}

/// Load extra dirs from ~/.b3/session-dirs.json (JSON array of path strings).
fn load_session_dirs_config() -> Vec<PathBuf> {
    let path = match session_dirs_config_path() {
        Some(p) => p,
        None => return vec![],
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let entries: Vec<String> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[b3-mcp-sm] warning: could not parse {}: {}", path.display(), e);
            return vec![];
        }
    };
    entries.into_iter().map(PathBuf::from).collect()
}

/// Save extra dirs to ~/.b3/session-dirs.json.
fn save_session_dirs_config(dirs: &[PathBuf]) -> Result<(), String> {
    let path = session_dirs_config_path().ok_or("could not determine home directory")?;
    // Ensure ~/.b3/ exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let strings: Vec<String> = dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let json = serde_json::to_string_pretty(&strings).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())
}

fn get_all_session_dirs() -> Vec<PathBuf> {
    let mut dirs = discover_session_dirs();

    // Load from ~/.b3/session-dirs.json
    for p in load_session_dirs_config() {
        if p.exists() && !dirs.contains(&p) {
            dirs.push(p);
        }
    }

    // Also append SESSION_EXTRA_DIRS env var
    if let Ok(extra) = std::env::var("SESSION_EXTRA_DIRS") {
        for d in extra.split(':') {
            let d = d.trim();
            if !d.is_empty() {
                let p = PathBuf::from(d);
                if p.exists() && !dirs.contains(&p) {
                    dirs.push(p);
                }
            }
        }
    }

    dirs
}

/// Resolve a session_id (UUID stem) to its full file path by searching all
/// session directories. Returns None if no matching `.jsonl` file is found.
fn resolve_session_id(session_id: &str) -> Option<PathBuf> {
    let filename = if session_id.ends_with(".jsonl") {
        session_id.to_string()
    } else {
        format!("{}.jsonl", session_id)
    };
    for dir in get_all_session_dirs() {
        let candidate = dir.join(&filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve session path from args: try session_id first, then session_file,
/// then fall back to active (most recently modified) session.
fn resolve_session_path(args: &Value) -> Result<PathBuf, String> {
    // 1. Try session_id (UUID lookup)
    if let Some(id) = args.get("session_id").and_then(|v| v.as_str()) {
        if !id.is_empty() {
            return resolve_session_id(id)
                .ok_or_else(|| format!("No session file found for session_id: {}", id));
        }
    }

    // 2. Try session_file (explicit path)
    if let Some(file) = args.get("session_file").and_then(|v| v.as_str()) {
        if !file.is_empty() {
            let p = PathBuf::from(file);
            return if p.exists() {
                Ok(p)
            } else {
                Err(format!("Session file not found: {}", p.display()))
            };
        }
    }

    // 3. Fall back to active session
    let dirs = get_all_session_dirs();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in &dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    if let Ok(meta) = fs::metadata(&p) {
                        if let Ok(mtime) = meta.modified() {
                            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                                best = Some((mtime, p));
                            }
                        }
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| "No session file provided and no active session found".to_string())
}

fn get_agent_name() -> String {
    if let Ok(name) = std::env::var("SESSION_AGENT_NAME") {
        if !name.is_empty() {
            return name;
        }
    }
    if let Ok(user) = std::env::var("USER") {
        if let Some(stripped) = user.strip_prefix("agent-") {
            let mut c = stripped.chars();
            return c.next().map(|ch| ch.to_uppercase().collect::<String>()).unwrap_or_default()
                + c.as_str();
        }
    }
    "Assistant".to_string()
}

fn classify_source(path: &Path) -> &'static str {
    if path.to_string_lossy().contains(".claude-remote") {
        "remote"
    } else {
        "local"
    }
}

// ---------------------------------------------------------------------------
// Efficient line counter — reads in 64 KB chunks, counts newlines
// ---------------------------------------------------------------------------

fn count_lines_in_file(path: &Path) -> io::Result<usize> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(65536, file);
    let mut count = 0usize;
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Session metadata extractor
// ---------------------------------------------------------------------------

struct SessionData {
    first_timestamp: Option<String>,
    last_timestamp: Option<String>,
    line_count: usize,
    size_bytes: u64,
    files_written: Vec<String>,
    duration_hours: Option<f64>,
}

fn extract_session_data(path: &Path) -> Result<SessionData, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let stat = fs::metadata(path).map_err(|e| e.to_string())?;
    let reader = BufReader::new(file);

    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut line_count = 0usize;
    let mut files_written: HashSet<String> = HashSet::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        line_count += 1;

        let data: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Extract timestamp
        let ts = data.get("timestamp")
            .or_else(|| data.get("createdAt"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(ref t) = ts {
            if first_ts.is_none() {
                first_ts = Some(t.clone());
            }
            last_ts = Some(t.clone());
        }

        // Collect Write tool calls
        if let Some(msg) = data.get("message") {
            if let Some(content) = msg.get("content") {
                if let Some(items) = content.as_array() {
                    for item in items {
                        if item.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                            && item.get("name").and_then(|v| v.as_str()) == Some("Write")
                        {
                            if let Some(fp) = item
                                .get("input")
                                .and_then(|i| i.get("file_path"))
                                .and_then(|v| v.as_str())
                            {
                                files_written.insert(fp.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Compute duration
    let duration_hours = if let (Some(ref f), Some(ref l)) = (&first_ts, &last_ts) {
        if f != l {
            parse_iso_seconds(f).and_then(|s| {
                parse_iso_seconds(l).map(|e| {
                    let diff = (e as i64 - s as i64).max(0) as f64;
                    (diff / 3600.0 * 100.0).round() / 100.0
                })
            })
        } else {
            None
        }
    } else {
        None
    };

    let mut fw_sorted: Vec<String> = files_written.into_iter().collect();
    fw_sorted.sort();

    Ok(SessionData {
        first_timestamp: first_ts,
        last_timestamp: last_ts,
        line_count,
        size_bytes: stat.len(),
        files_written: fw_sorted,
        duration_hours,
    })
}

fn parse_iso_seconds(s: &str) -> Option<u64> {
    // Try chrono parse first
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp() as u64)
        .or_else(|| {
            // Fallback: try with Z → +00:00
            let normalized = s.replace('Z', "+00:00");
            DateTime::parse_from_rfc3339(&normalized)
                .ok()
                .map(|dt| dt.timestamp() as u64)
        })
}

// ---------------------------------------------------------------------------
// Filter functions — ported from session_parser.py
// ---------------------------------------------------------------------------

/// Skip content that is a task-notification or a compaction summary.
fn should_skip_content(text: &str) -> bool {
    text.contains("<task-notification>")
        || (text.contains("Compacted") && text.contains("ctrl+o"))
}

/// Is this record a queue-operation enqueue?
fn is_queue_operation(data: &Value) -> bool {
    data.get("type").and_then(|v| v.as_str()) == Some("queue-operation")
        && data.get("operation").and_then(|v| v.as_str()) == Some("enqueue")
}

/// Extract content string from a queue-operation record.
fn get_queue_content(data: &Value) -> Option<String> {
    data.get("content").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Extract message role ("user" | "assistant") if present.
fn message_role(data: &Value) -> Option<&str> {
    data.get("message")
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
}

// ---------------------------------------------------------------------------
// Tool: find_active_session
// ---------------------------------------------------------------------------

pub fn handle_find_active_session(_args: &Value) -> Value {
    let dirs = get_all_session_dirs();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;

    for dir in &dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    if let Ok(meta) = fs::metadata(&p) {
                        if let Ok(mtime) = meta.modified() {
                            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                                best = Some((mtime, p));
                            }
                        }
                    }
                }
            }
        }
    }

    match best {
        Some((_, path)) => {
            let size_mb = fs::metadata(&path)
                .map(|m| (m.len() as f64) / (1024.0 * 1024.0))
                .unwrap_or(0.0);
            json!({
                "file": path.to_string_lossy(),
                "size_mb": (size_mb * 100.0).round() / 100.0,
            })
        }
        None => json!({"error": "No active session found"}),
    }
}

// ---------------------------------------------------------------------------
// Tool: get_session_info
// ---------------------------------------------------------------------------

pub fn handle_get_session_info(args: &Value) -> Value {
    let path = match resolve_session_path(args) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };

    match extract_session_data(&path) {
        Err(e) => json!({"error": e}),
        Ok(data) => {
            json!({
                "file": path.to_string_lossy(),
                "line_count": data.line_count,
                "size_bytes": data.size_bytes,
                "size_mb": ((data.size_bytes as f64 / (1024.0 * 1024.0)) * 100.0).round() / 100.0,
                "first_timestamp": data.first_timestamp,
                "last_timestamp": data.last_timestamp,
                "duration_hours": data.duration_hours,
                "files_written": data.files_written,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: list_sessions
// ---------------------------------------------------------------------------

pub fn handle_list_sessions(args: &Value) -> Value {
    let min_size_mb = args.get("min_size_mb").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let after = args.get("after").and_then(|v| v.as_str()).unwrap_or("");
    let before = args.get("before").and_then(|v| v.as_str()).unwrap_or("");

    let dirs = get_all_session_dirs();
    let mut sessions: Vec<Value> = Vec::new();

    for dir in &dirs {
        if !dir.exists() {
            continue;
        }
        let source = classify_source(dir);

        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Ok(meta) = fs::metadata(&p) {
                    let size_mb = meta.len() as f64 / (1024.0 * 1024.0);

                    if size_mb < min_size_mb {
                        continue;
                    }

                    // Read first line for first_timestamp
                    let first_ts = read_first_timestamp(&p);
                    // Read last non-empty line for last_timestamp
                    let last_ts = read_last_timestamp(&p, &meta);

                    // Apply time filters on last_timestamp
                    if !after.is_empty() {
                        if let Some(ref lt) = last_ts {
                            if lt.as_str() < after {
                                continue;
                            }
                        }
                    }
                    if !before.is_empty() {
                        if let Some(ref lt) = last_ts {
                            if lt.as_str() > before {
                                continue;
                            }
                        }
                    }

                    // Line count: exact for <10MB, heuristic for larger
                    let line_count = if size_mb < 10.0 {
                        count_lines_in_file(&p).unwrap_or(0)
                    } else {
                        (meta.len() / 3400) as usize
                    };

                    let session_id = p.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();

                    sessions.push(json!({
                        "session_id": session_id,
                        "source": source,
                        "size_mb": (size_mb * 100.0).round() / 100.0,
                        "line_count": line_count,
                        "first_timestamp": first_ts,
                        "last_timestamp": last_ts,
                        "file_path": p.to_string_lossy(),
                    }));
                }
            }
        }
    }

    // Sort by size_mb descending
    sessions.sort_by(|a, b| {
        let sa = a.get("size_mb").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let sb = b.get("size_mb").and_then(|v| v.as_f64()).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = sessions.len();
    if limit > 0 && sessions.len() > limit {
        sessions.truncate(limit);
    }

    json!({
        "total": total,
        "returned": sessions.len(),
        "sessions": sessions,
    })
}

fn read_first_timestamp(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let data: Value = serde_json::from_str(line.trim()).ok()?;
    data.get("timestamp")
        .or_else(|| data.get("createdAt"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn read_last_timestamp(path: &Path, meta: &fs::Metadata) -> Option<String> {
    // Seek to last 8KB to find last non-empty line
    let tail_size: u64 = 8192;
    let file_size = meta.len();

    use std::io::Seek;
    let mut file = File::open(path).ok()?;

    if file_size > tail_size {
        file.seek(std::io::SeekFrom::End(-(tail_size as i64))).ok()?;
    }

    let reader = BufReader::new(file);
    let mut last_ts: Option<String> = None;

    for line in reader.lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(data) = serde_json::from_str::<Value>(&line) {
            if let Some(ts) = data.get("timestamp")
                .or_else(|| data.get("createdAt"))
                .and_then(|v| v.as_str())
            {
                last_ts = Some(ts.to_string());
            }
        }
    }

    last_ts
}

// ---------------------------------------------------------------------------
// Tool: resurface
// ---------------------------------------------------------------------------

pub fn handle_resurface(args: &Value) -> Value {
    let start_line = args.get("start_line").and_then(|v| v.as_i64()).unwrap_or(1);
    let end_line = args.get("end_line").and_then(|v| v.as_i64()).unwrap_or(-1);
    let include_timestamps = args.get("include_timestamps").and_then(|v| v.as_bool()).unwrap_or(true);
    let frac_sample = args.get("frac_sample").and_then(|v| v.as_f64());
    let max_word_length = args.get("max_word_length").and_then(|v| v.as_u64()).map(|n| n as usize);
    let seed = args.get("seed").and_then(|v| v.as_u64());
    let output_file = args.get("output_file").and_then(|v| v.as_str()).unwrap_or("");
    let record_type = args.get("record_type").and_then(|v| v.as_str()).unwrap_or("conversation");
    let tool_result_limit = args.get("tool_result_limit").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(100);

    // Resolve session file path (session_id > session_file > active)
    let session_path = match resolve_session_path(args) {
        Ok(p) => p,
        Err(e) => return json!(format!("Error: {}", e)),
    };

    // Count total lines and get file size for byte-offset seek optimization
    let file_len = fs::metadata(&session_path).map(|m| m.len()).unwrap_or(0);
    let total_lines = count_lines_in_file(&session_path).unwrap_or(100_000) as i64;

    // Resolve negative line numbers
    let abs_start = if start_line > 0 {
        start_line as usize
    } else {
        (total_lines + start_line + 1).max(1) as usize
    };
    let abs_end = if end_line > 0 {
        end_line as usize
    } else if end_line == -1 {
        total_lines as usize
    } else {
        (total_lines + end_line).max(1) as usize
    };

    // Read the requested line range with BufReader
    let range_result = match read_line_range(
        &session_path,
        abs_start,
        abs_end,
        total_lines as usize,
        file_len,
    ) {
        Ok(r) => r,
        Err(e) => return json!(format!("Error: Failed to read lines: {}", e)),
    };
    let raw_lines = range_result.lines;
    let range_truncated = range_result.truncated;
    let range_truncated_at = range_result.truncated_at_line;

    // Pre-filter: keep compaction markers, skip tool_result lines (except for "all" mode)
    let mut filtered_lines: Vec<String> = Vec::new();
    let mut original_indices: Vec<usize> = Vec::new();

    for (i, line) in raw_lines.iter().enumerate() {
        if line.contains("Compacted") && line.contains("ctrl+o") {
            // Always keep compaction markers
            filtered_lines.push(line.clone());
            original_indices.push(i);
        } else if !line.trim().is_empty() {
            if line.contains("tool_use_id") {
                // Tool result lines: only keep for "all" mode
                if record_type == "all" {
                    filtered_lines.push(line.clone());
                    original_indices.push(i);
                }
                // For all other modes, skip (continue to next line)
                continue;
            }
            filtered_lines.push(line.clone());
            original_indices.push(i);
        }
    }

    // Random sampling
    let (sampled_lines, sampled_indices) = if let Some(frac) = frac_sample {
        if frac > 0.0 && frac < 1.0 {
            // Find compaction line indices first
            let compaction_idxs: Vec<usize> = filtered_lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains("Compacted") && l.contains("ctrl+o"))
                .map(|(i, _)| i)
                .collect();

            let total_count = filtered_lines.len();
            let num_to_keep = ((total_count as f64 * frac) as usize)
                .saturating_sub(compaction_idxs.len())
                .max(1);

            let available: Vec<usize> = (0..total_count)
                .filter(|i| !compaction_idxs.contains(i))
                .collect();

            let sampled: Vec<usize> = if available.len() > num_to_keep {
                // Seeded sampling
                let mut rng = if let Some(s) = seed {
                    StdRng::seed_from_u64(s)
                } else {
                    // Use a time-based seed if none provided
                    let t = std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(42);
                    StdRng::seed_from_u64(t)
                };
                // Reservoir sampling
                reservoir_sample(&available, num_to_keep, &mut rng)
            } else {
                available
            };

            let mut keep_indices: Vec<usize> = compaction_idxs.iter().chain(sampled.iter()).cloned().collect();
            keep_indices.sort_unstable();
            keep_indices.dedup();

            let new_orig: Vec<usize> = keep_indices.iter().map(|&i| original_indices[i]).collect();
            let new_lines: Vec<String> = keep_indices.iter().map(|&i| filtered_lines[i].clone()).collect();
            (new_lines, new_orig)
        } else {
            (filtered_lines, original_indices)
        }
    } else {
        (filtered_lines, original_indices)
    };

    let agent_name = get_agent_name();
    // Track tool_use id → name for tool_result resolution
    let mut tool_id_to_name: HashMap<String, String> = HashMap::new();
    let mut result: Vec<String> = Vec::new();

    for (i, line) in sampled_lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }

        // Absolute line number for display
        let line_num = abs_start as i64 + sampled_indices[i] as i64;

        let data: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                let preview: String = line.chars().take(50).collect();
                eprintln!("[b3-mcp-sm] skipping malformed JSONL at line {}: `{}`", line_num, preview);
                continue;
            }
        };

        let timestamp = data.get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let ts_prefix = if include_timestamps && !timestamp.is_empty() {
            format!("{} | ", timestamp)
        } else {
            String::new()
        };

        // Queue-operation (voice input mid-turn)
        if is_queue_operation(&data) {
            if let Some(content) = get_queue_content(&data) {
                if !content.is_empty() && !should_skip_content(&content) {
                    if record_type == "conversation" || record_type == "all" {
                        result.push(format!("L{}: {}User: {}", line_num, ts_prefix, content));
                    }
                }
            }
            continue;
        }

        // User/assistant messages
        let role = match message_role(&data) {
            Some(r) => r.to_string(),
            None => continue,
        };

        let content_list = match data.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };

        // Handle string content
        if let Some(text) = content_list.as_str() {
            if should_skip_content(text) {
                continue;
            }
            if record_type == "conversation" || record_type == "all" {
                let text_flat = text.replace('\n', " ");
                let speaker = if role == "user" { "User" } else { &agent_name };
                result.push(format!("L{}: {}{}: {}", line_num, ts_prefix, speaker, text_flat));
            }
            continue;
        }

        let items = match content_list.as_array() {
            Some(arr) => arr,
            None => continue,
        };

        for item in items {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match item_type {
                "tool_use" => {
                    let tool_name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_input = item.get("input").cloned().unwrap_or(json!({}));
                    let tool_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if !tool_id.is_empty() {
                        tool_id_to_name.insert(tool_id.to_string(), tool_name.to_string());
                    }

                    match record_type {
                        "conversation" => {
                            if tool_name.contains("voice_say") {
                                let voice_text = tool_input
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .replace('\n', " ");
                                if !voice_text.is_empty() {
                                    result.push(format!(
                                        "L{}: {}{} [voice]: {}",
                                        line_num, ts_prefix, agent_name, voice_text
                                    ));
                                }
                            }
                        }
                        "tool_use" => {
                            let input_str = if tool_input.is_null() {
                                "{}".to_string()
                            } else {
                                tool_input.to_string()
                            };
                            result.push(format!("L{}: {}{}: {}", line_num, ts_prefix, tool_name, input_str));
                        }
                        "bash" => {
                            if tool_name == "Bash" {
                                let command = tool_input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                                let desc = tool_input.get("description").and_then(|v| v.as_str()).unwrap_or("");
                                let desc_suffix = if !desc.is_empty() { format!(" # {}", desc) } else { String::new() };
                                result.push(format!("L{}: {}$ {}{}", line_num, ts_prefix, command, desc_suffix));
                            }
                        }
                        "grep" => {
                            if tool_name == "Bash" {
                                let command = tool_input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                                if command.trim().starts_with("grep ") || command.contains(" grep ") {
                                    result.push(format!("L{}: {}$ {}", line_num, ts_prefix, command));
                                }
                            } else if tool_name == "Grep" {
                                let pattern = tool_input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                                let path = tool_input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                                result.push(format!("L{}: {}Grep: {} in {}", line_num, ts_prefix, pattern, path));
                            }
                        }
                        "find" => {
                            if tool_name == "Bash" {
                                let command = tool_input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                                if command.trim().starts_with("find ") || command.contains(" find ") {
                                    result.push(format!("L{}: {}$ {}", line_num, ts_prefix, command));
                                }
                            } else if tool_name == "Glob" {
                                let pattern = tool_input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                                let path = tool_input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                                result.push(format!("L{}: {}Glob: {} in {}", line_num, ts_prefix, pattern, path));
                            }
                        }
                        "write" => {
                            if tool_name == "Write" {
                                let file_path = tool_input.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
                                result.push(format!("L{}: {}Write: {}", line_num, ts_prefix, file_path));
                            }
                        }
                        "read" => {
                            if tool_name == "Read" {
                                let file_path = tool_input.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
                                result.push(format!("L{}: {}Read: {}", line_num, ts_prefix, file_path));
                            }
                        }
                        "all" => {
                            let input_str = if tool_input.is_null() { "{}".to_string() } else { tool_input.to_string() };
                            result.push(format!("L{}: {}{}: {}", line_num, ts_prefix, tool_name, input_str));
                        }
                        _ => {}
                    }
                }

                "text" => {
                    if record_type == "conversation" || record_type == "all" {
                        let text_content = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text_content.is_empty() || text_content.trim().is_empty() {
                            continue;
                        }
                        if should_skip_content(text_content) {
                            continue;
                        }
                        let text_flat = text_content.replace('\n', " ");
                        let speaker = if role == "user" { "User" } else { &agent_name };
                        result.push(format!("L{}: {}{}: {}", line_num, ts_prefix, speaker, text_flat));

                        // Compaction marker inside text content
                        if text_content.contains("Compacted") && text_content.contains("ctrl+o") {
                            // Already captured above, but note as compaction
                            result.push(format!("L{}: {}COMPACTION", line_num, ts_prefix));
                        }
                    }
                }

                "tool_result" => {
                    if record_type == "all" {
                        let tool_use_id = item.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        let content = item.get("content").map(|v| v.to_string()).unwrap_or_default();

                        let limited_content = if tool_result_limit > 0 && content.len() > tool_result_limit {
                            format!("{}...", &content[..tool_result_limit])
                        } else {
                            content
                        };

                        let short_id = if tool_use_id.len() >= 8 { &tool_use_id[..8] } else { tool_use_id };
                        result.push(format!("L{}: {}[result {}]: {}", line_num, ts_prefix, short_id, limited_content));
                    }
                }

                _ => {}
            }
        }
    }

    // Word truncation
    let result = if let Some(max_words) = max_word_length {
        result.into_iter().map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            if words.len() > max_words {
                words[..max_words].join(" ") + "..."
            } else {
                line
            }
        }).collect::<Vec<_>>()
    } else {
        result
    };

    let content = result.join("\n");
    let truncation_notice = if range_truncated {
        format!(
            " | TRUNCATED at line {} (I/O error — partial result)",
            range_truncated_at.unwrap_or(0)
        )
    } else {
        String::new()
    };
    let header = format!(
        "[Session: {} total lines | Showing L{}-L{}{}]\n\n",
        total_lines, abs_start, abs_end, truncation_notice
    );

    if !output_file.is_empty() {
        let output_path = Path::new(output_file);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::write(output_path, format!("{}{}", header, content)) {
            Ok(_) => {
                let mut resp = json!({
                    "resurfaced": output_file,
                    "moments": result.len(),
                    "total_lines": total_lines,
                });
                if range_truncated {
                    resp["truncated"] = json!(true);
                    resp["truncated_at_line"] = json!(range_truncated_at);
                }
                resp
            }
            Err(e) => json!(format!("Error writing output file: {}", e)),
        }
    } else {
        Value::String(format!("{}{}", header, content))
    }
}

/// Reservoir sampling: select k items from slice without replacement.
fn reservoir_sample<R: Rng + ?Sized>(items: &[usize], k: usize, rng: &mut R) -> Vec<usize> {
    if k >= items.len() {
        return items.to_vec();
    }
    let mut reservoir: Vec<usize> = items[..k].to_vec();
    for i in k..items.len() {
        let j = rng.gen_range(0..=i);
        if j < k {
            reservoir[j] = items[i];
        }
    }
    reservoir.sort_unstable();
    reservoir
}

struct LineRangeResult {
    lines: Vec<String>,
    truncated: bool,
    truncated_at_line: Option<usize>,
}

/// Read lines abs_start..=abs_end (1-indexed) from a file.
///
/// For large `abs_start` values, uses a byte-offset seek approximation to skip
/// most of the prefix scan. `total_lines` and `file_len` enable proportional
/// seek calculation; pass 0 for either to disable the optimization.
///
/// On I/O error mid-read, returns a partial result with `truncated = true`
/// rather than aborting the entire range read.
fn read_line_range(
    path: &Path,
    abs_start: usize,
    abs_end: usize,
    total_lines: usize,
    file_len: u64,
) -> io::Result<LineRangeResult> {
    use std::io::{Read, Seek};

    const SEEK_THRESHOLD: usize = 5_000;
    const SAFETY_MARGIN: usize = 500;

    let mut file = File::open(path)?;
    let mut scan_from_line = 1usize;

    // Byte-offset seek optimization: for large start offsets, seek to approximate
    // byte position rather than iterating from the beginning.
    // Safety margin of 500 lines ensures we land before abs_start even with
    // line-length variance. Handles seek-past-EOF by capping at file_len - 1.
    if abs_start > SEEK_THRESHOLD + SAFETY_MARGIN
        && total_lines > SAFETY_MARGIN
        && file_len > 1024
    {
        let scan_start = abs_start.saturating_sub(SAFETY_MARGIN);
        let bytes_per_line = file_len / total_lines as u64;
        let approx_offset = (scan_start as u64).saturating_mul(bytes_per_line);
        let seek_to = approx_offset.min(file_len.saturating_sub(1024));

        if seek_to > 0 {
            file.seek(std::io::SeekFrom::Start(seek_to))?;
            // Discard partial line to align to next line boundary
            let mut byte = [0u8; 1];
            loop {
                let n = file.read(&mut byte)?;
                if n == 0 || byte[0] == b'\n' {
                    break;
                }
            }
            scan_from_line = scan_start;
        }
    }

    let reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut truncated = false;
    let mut truncated_at_line = None;

    for (i, line) in reader.lines().enumerate() {
        let line_num = scan_from_line + i;
        if line_num < abs_start {
            continue;
        }
        if line_num > abs_end {
            break;
        }
        match line {
            Ok(l) => lines.push(l),
            Err(e) => {
                eprintln!("[b3-mcp-sm] read_line_range I/O error at line {}: {}", line_num, e);
                truncated = true;
                truncated_at_line = Some(line_num);
                break;
            }
        }
    }

    Ok(LineRangeResult { lines, truncated, truncated_at_line })
}

// ---------------------------------------------------------------------------
// Tool definitions (MCP schema)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tool: add_session_dir
// ---------------------------------------------------------------------------

pub fn handle_add_session_dir(args: &Value) -> Value {
    let dir = match args.get("dir").and_then(|v| v.as_str()) {
        Some(d) if !d.is_empty() => d,
        _ => return tool_error("'dir' parameter is required"),
    };

    let path = PathBuf::from(dir);
    if !path.exists() {
        return tool_error(&format!("Directory does not exist: {}", dir));
    }
    if !path.is_dir() {
        return tool_error(&format!("Path is not a directory: {}", dir));
    }

    let mut existing = load_session_dirs_config();

    // Normalise to canonical path to avoid duplicates from symlinks / relative paths
    let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

    if existing.iter().any(|p| {
        fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == canonical
    }) {
        let config_path = session_dirs_config_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        return json!({
            "status": "already_present",
            "dir": dir,
            "config": config_path,
        });
    }

    existing.push(canonical.clone());

    match save_session_dirs_config(&existing) {
        Ok(()) => {
            let config_path = session_dirs_config_path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            json!({
                "status": "added",
                "dir": canonical.to_string_lossy(),
                "config": config_path,
                "total_extra_dirs": existing.len(),
            })
        }
        Err(e) => tool_error(&format!("Failed to write config: {}", e)),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "find_active_session",
            "description": "Find the currently active Claude session file (most recently modified .jsonl).",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "get_session_info",
            "description": "Get metadata about a session: timestamps, line count, size, files written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session UUID (from list_sessions). Resolved to file path automatically."
                    },
                    "session_file": {
                        "type": "string",
                        "description": "Path to session JSONL. Optional — uses active session if omitted."
                    }
                },
                "required": []
            }
        },
        {
            "name": "list_sessions",
            "description": "List all Claude session files with metadata, sorted by size descending.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_size_mb": {
                        "type": "number",
                        "description": "Minimum file size in MB (default 0 = all). Use 1.0 to skip tiny sub-sessions."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions to return (default 50, 0 = unlimited)."
                    },
                    "after": {
                        "type": "string",
                        "description": "Only sessions with last_timestamp after this ISO date."
                    },
                    "before": {
                        "type": "string",
                        "description": "Only sessions with last_timestamp before this ISO date."
                    }
                },
                "required": []
            }
        },
        {
            "name": "add_session_dir",
            "description": "Add a directory to the persistent session search list (~/.b3/session-dirs.json). The directory will be included in all future list_sessions and resurface calls without requiring env vars or config restarts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": {
                        "type": "string",
                        "description": "Absolute path to the directory containing session JSONL files."
                    }
                },
                "required": ["dir"]
            }
        },
        {
            "name": "resurface",
            "description": "Resurface moments from a session — extract and filter content by type with optional sampling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_line": {
                        "type": "integer",
                        "description": "Starting line number. Negative = from end (e.g. -1000 = last 1000 lines)."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Ending line number. -1 = end of file."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session UUID (from list_sessions). Resolved to file path automatically."
                    },
                    "session_file": {
                        "type": "string",
                        "description": "Path to session JSONL. Optional — uses active session if omitted."
                    },
                    "include_timestamps": {
                        "type": "boolean",
                        "description": "Include timestamps in output (default true)."
                    },
                    "frac_sample": {
                        "type": "number",
                        "description": "Random sample fraction 0.0-1.0. Omit or 1.0 for all lines."
                    },
                    "max_word_length": {
                        "type": "integer",
                        "description": "Truncate output lines to this many words."
                    },
                    "seed": {
                        "type": "integer",
                        "description": "Random seed for reproducible sampling."
                    },
                    "output_file": {
                        "type": "string",
                        "description": "Write output to this file path instead of returning (saves tokens)."
                    },
                    "record_type": {
                        "type": "string",
                        "description": "What to extract: conversation (default), tool_use, bash, grep, find, write, read, all.",
                        "enum": ["conversation", "tool_use", "bash", "grep", "find", "write", "read", "all"]
                    },
                    "tool_result_limit": {
                        "type": "integer",
                        "description": "Max chars for tool_result content in 'all' mode (default 100, 0 = unlimited)."
                    }
                },
                "required": ["start_line", "end_line"]
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// JSON-RPC helpers
// ---------------------------------------------------------------------------

fn make_response(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

fn make_error_response(id: Value, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
    }
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true,
    })
}

// ---------------------------------------------------------------------------
// Main loop — stdio JSON-RPC server
// ---------------------------------------------------------------------------

pub async fn run() -> anyhow::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();

    let death_log = dirs::home_dir()
        .map(|h| h.join(".b3").join("mcp-session-memory-death.log"))
        .unwrap_or_else(|| PathBuf::from("/tmp/b3-mcp-session-memory-death.log"));

    // Signal handlers (Unix only)
    #[cfg(unix)]
    {
        unsafe extern "C" fn signal_handler(sig: libc::c_int) {
            let msg: &[u8] = match sig {
                libc::SIGTERM => b"[b3-mcp-sm] SIGTERM received\n",
                libc::SIGINT  => b"[b3-mcp-sm] SIGINT received\n",
                libc::SIGHUP  => b"[b3-mcp-sm] SIGHUP received\n",
                libc::SIGPIPE => b"[b3-mcp-sm] SIGPIPE received\n",
                _             => b"[b3-mcp-sm] unknown signal\n",
            };
            unsafe {
                libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            }
            // SIGPIPE MUST exit — returning causes infinite spin loop (voice.rs 2026-03-24)
            unsafe { libc::_exit(128 + sig); }
        }

        unsafe {
            let handler = signal_handler as *const () as libc::sighandler_t;
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGINT,  handler);
            libc::signal(libc::SIGHUP,  handler);
            libc::signal(libc::SIGPIPE, handler);
        }
    }

    // Panic hook
    let death_log_clone = death_log.clone();
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("[b3-mcp-sm] PANIC: {} (PID {})\n", info, std::process::id());
        eprintln!("{}", msg.trim());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log_clone) {
            let _ = write!(f, "{}", msg);
        }
    }));

    let my_pid = std::process::id();
    eprintln!("[b3-mcp-sm] Starting session-memory MCP server (PID {})", my_pid);

    // Orphan watchdog
    #[cfg(unix)]
    {
        let parent_pid = unsafe { libc::getppid() };
        eprintln!("[b3-mcp-sm] Parent PID: {}", parent_pid);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let ppid = unsafe { libc::getppid() };
                if ppid == 1 {
                    eprintln!("[b3-mcp-sm] Parent died (ppid=1), self-terminating (PID {})", my_pid);
                    std::process::exit(0);
                }
            }
        });
    }

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[b3-mcp-sm] stdin read error: {}", e);
                break;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[b3-mcp-sm] parse error: {} | line: {}", e, line);
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                };
                let _ = writeln!(stdout_lock, "{}", serde_json::to_string(&resp)?);
                let _ = stdout_lock.flush();
                continue;
            }
        };

        // Notifications have no id — don't respond
        if request.id.is_none() {
            eprintln!("[b3-mcp-sm] notification: {}", request.method);
            continue;
        }

        let id = request.id.unwrap();
        let params = request.params.as_ref().cloned().unwrap_or(json!({}));

        let response = match request.method.as_str() {
            "initialize" => make_response(id, json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": "session-memory",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Session memory tools — resurface, list, and inspect Claude Code session JSONL files."
            })),

            "tools/list" => make_response(id, json!({ "tools": tool_definitions() })),

            "resources/list" => make_response(id, json!({ "resources": [] })),

            "prompts/list" => make_response(id, json!({ "prompts": [] })),

            "tools/call" => {
                let tool_name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));

                eprintln!("[b3-mcp-sm] tools/call: {}", tool_name);

                let result = match tool_name {
                    "find_active_session" => handle_find_active_session(&arguments),
                    "get_session_info"    => handle_get_session_info(&arguments),
                    "list_sessions"       => handle_list_sessions(&arguments),
                    "resurface"           => handle_resurface(&arguments),
                    "add_session_dir"     => handle_add_session_dir(&arguments),
                    _ => tool_error(&format!("Unknown tool: {}", tool_name)),
                };

                // MCP spec: tools/call result must be {"content": [{"type": "text", "text": ...}]}
                // tool_error already returns this shape; wrap successful results here.
                let wrapped = if result.get("content").is_some() {
                    // Already wrapped (tool_error path)
                    result
                } else {
                    let text = serde_json::to_string(&result).unwrap_or_default();
                    json!({ "content": [{ "type": "text", "text": text }] })
                };

                make_response(id, wrapped)
            }

            "ping" => make_response(id, json!({})),

            _ => {
                eprintln!("[b3-mcp-sm] unknown method: {}", request.method);
                make_error_response(id, -32601, &format!("Method not found: {}", request.method))
            }
        };

        let json_str = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[b3-mcp-sm] FATAL: JSON serialization failed: {} (PID {})", e, my_pid);
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log) {
                    let _ = writeln!(f, "[b3-mcp-sm] exit: JSON serialization error: {}", e);
                }
                return Err(e.into());
            }
        };

        if let Err(e) = writeln!(stdout_lock, "{}", json_str) {
            eprintln!("[b3-mcp-sm] FATAL: stdout write failed: {} (PID {})", e, my_pid);
            return Err(e.into());
        }
        if let Err(e) = stdout_lock.flush() {
            eprintln!("[b3-mcp-sm] FATAL: stdout flush failed: {} (PID {})", e, my_pid);
            return Err(e.into());
        }
    }

    let exit_reason = "stdin closed (EOF)";
    eprintln!("[b3-mcp-sm] {} — shutting down (PID {})", exit_reason, my_pid);

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&death_log) {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}] PID {} exit: {}", now, my_pid, exit_reason);
    }

    Ok(())
}
