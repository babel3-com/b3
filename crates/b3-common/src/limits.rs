//! System limits — shared between CLI and server.
//!
//! When changing any value, check its documented dependencies and the
//! compile-time assertions at the bottom of this file. If an assertion
//! fails, it means the new value violates a cross-boundary constraint.

use std::time::Duration;

/// Maximum terminal buffer size (raw bytes). The on-wire representation
/// is base64 (+⅓) wrapped in JSON (+~100 bytes), so this value must
/// satisfy: MAX_BUFFER_BYTES * 4/3 + 200 < SERVER_BODY_LIMIT.
///
/// Used by: pusher.rs (remote), web.rs (local), cap_buffer().
pub const MAX_BUFFER_BYTES: usize = 256 * 1024; // 256 KB

/// Server body size limit (bytes). Applied via DefaultBodyLimit layer.
/// Must be > MAX_BUFFER_BYTES * 4/3 (base64 expansion) to allow session pushes.
/// Must be < nginx client_max_body_size (25MB in deploy/nginx.conf).
///
/// Used by: lib.rs (DefaultBodyLimit layer).
pub const SERVER_BODY_LIMIT: usize = 15 * 1024 * 1024; // 15 MB

/// SSE keepalive interval — server sends a ping comment this often.
/// Must be < SSE_TIMEOUT / 3 so the client can miss up to 3 keepalives
/// before deciding the connection is dead.
///
/// Used by: sessions.rs (Sse::keep_alive), admin.rs.
pub const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// SSE timeout — client considers the connection dead after this long
/// without any data (including keepalives).
/// Must be >= SSE_KEEPALIVE_INTERVAL * 3.
///
/// Used by: puller.rs (tokio::time::timeout around stream.next()).
pub const SSE_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum push interval — rate limit for terminal data pushes.
/// Prevents hot loops when PTY output is streaming fast.
///
/// Used by: pusher.rs (base_interval floor).
pub const MIN_PUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Number of consecutive failures before logging a warning.
/// Prevents log spam on prolonged outages — only logs at failure #1
/// and then every Nth failure.
///
/// Used by: pusher.rs, puller.rs, backoff.rs.
pub const FAILURE_LOG_INTERVAL: u32 = 50;

/// Maximum delta size per push (raw bytes, before base64 encoding).
/// Safety cap to prevent a single push from dominating server memory.
/// Normal deltas are small (100 bytes–10 KB). Full resyncs (offset=0)
/// can be up to MAX_BUFFER_BYTES. This limit applies per-push, not per-agent.
///
/// Used by: sessions.rs (push handler validation).
pub const MAX_DELTA_BYTES: usize = MAX_BUFFER_BYTES;

// ── Compile-time assertions ─────────────────────────────────────────
//
// These fail the build if cross-boundary constraints are violated.
// If you hit one, read the doc comment on the constant you changed.

// base64 expansion of MAX_BUFFER_BYTES + JSON overhead must fit in SERVER_BODY_LIMIT
const _: () = assert!(
    MAX_BUFFER_BYTES * 4 / 3 + 1024 < SERVER_BODY_LIMIT,
    "MAX_BUFFER_BYTES base64 expansion must fit under SERVER_BODY_LIMIT"
);

// SERVER_BODY_LIMIT must fit under nginx 25MB limit
const _: () = assert!(
    SERVER_BODY_LIMIT < 25 * 1024 * 1024,
    "SERVER_BODY_LIMIT must be under nginx 25MB client_max_body_size"
);

// SSE timeout must allow at least 3 missed keepalives
const _: () = assert!(
    SSE_TIMEOUT.as_secs() >= SSE_KEEPALIVE_INTERVAL.as_secs() * 3,
    "SSE_TIMEOUT must allow at least 3 missed keepalives"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_sane() {
        assert!(MAX_BUFFER_BYTES > 0);
        assert!(SERVER_BODY_LIMIT > MAX_BUFFER_BYTES);
        assert!(SSE_TIMEOUT > SSE_KEEPALIVE_INTERVAL);
        assert!(MIN_PUSH_INTERVAL.as_millis() >= 50);
    }

    #[test]
    fn base64_expansion_fits() {
        // Worst-case: MAX_BUFFER_BYTES of data, base64-encoded, plus JSON wrapper
        let base64_size = (MAX_BUFFER_BYTES * 4 + 2) / 3;
        let json_overhead = 1024; // agent_id, rows, cols, etc.
        assert!(
            base64_size + json_overhead < SERVER_BODY_LIMIT,
            "base64 {} + overhead {} = {} > limit {}",
            base64_size, json_overhead, base64_size + json_overhead, SERVER_BODY_LIMIT
        );
    }
}
