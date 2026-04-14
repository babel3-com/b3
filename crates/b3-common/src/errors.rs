//! Typed error results for I/O operations.
//!
//! `OpResult` generalizes the pusher's `PushResult` pattern for any subsystem
//! that performs network I/O. The caller always knows whether to retry, back off,
//! or give up — no more `()` returns that discard failure information.
//!
//! Usage pattern:
//! ```ignore
//! match do_operation().await {
//!     OpResult::Ok => { backoff.reset(); }
//!     OpResult::PermanentFailure { .. } => { reduce_payload(); backoff.increase(); }
//!     OpResult::TransientFailure { .. } => { backoff.increase(); }
//!     OpResult::NetworkError { .. } => { backoff.increase(); }
//! }
//! ```

/// Classified result of a network I/O operation.
///
/// Every subsystem that does I/O (pusher, puller, tunnel registration,
/// credit deduction, voice event forwarding) returns `OpResult`.
#[derive(Debug)]
pub enum OpResult<T = ()> {
    /// Operation succeeded.
    Ok(T),
    /// Permanent failure — the request will never succeed as-is.
    /// Examples: HTTP 413 (payload too large), 422 (validation), 401 (bad key).
    /// Caller should modify the request (truncate, fix auth) before retrying.
    PermanentFailure {
        reason: String,
        status: Option<u16>,
    },
    /// Transient failure — server is temporarily unavailable.
    /// Examples: HTTP 502, 503, 429.
    /// Caller should back off and retry unchanged.
    TransientFailure {
        reason: String,
        status: Option<u16>,
    },
    /// Network error — connection failed, timed out, DNS error.
    /// Caller should back off and retry.
    NetworkError {
        reason: String,
    },
}

impl<T> OpResult<T> {
    /// Returns true if the operation succeeded.
    pub fn is_ok(&self) -> bool {
        matches!(self, OpResult::Ok(_))
    }

    /// Returns true if the failure is permanent (caller should not retry unchanged).
    pub fn is_permanent(&self) -> bool {
        matches!(self, OpResult::PermanentFailure { .. })
    }

    /// Classify an HTTP status code into success, permanent, or transient failure.
    pub fn from_http_status(status: u16) -> OpResult<()> {
        match status {
            200..=299 => OpResult::Ok(()),
            400 | 401 | 403 | 404 | 413 | 422 => OpResult::PermanentFailure {
                reason: format!("HTTP {status}"),
                status: Some(status),
            },
            _ => OpResult::TransientFailure {
                reason: format!("HTTP {status}"),
                status: Some(status),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_op_result_is_ok() {
        assert!(OpResult::Ok(()).is_ok());
        assert!(!OpResult::<()>::PermanentFailure {
            reason: "test".into(),
            status: Some(413),
        }.is_ok());
    }

    #[test]
    fn test_op_result_is_permanent() {
        assert!(OpResult::<()>::PermanentFailure {
            reason: "test".into(),
            status: Some(413),
        }.is_permanent());
        assert!(!OpResult::<()>::TransientFailure {
            reason: "test".into(),
            status: Some(503),
        }.is_permanent());
        assert!(!OpResult::Ok(()).is_permanent());
    }

    #[test]
    fn test_from_http_status() {
        assert!(OpResult::<()>::from_http_status(200).is_ok());
        assert!(OpResult::<()>::from_http_status(204).is_ok());
        assert!(OpResult::<()>::from_http_status(400).is_permanent());
        assert!(OpResult::<()>::from_http_status(401).is_permanent());
        assert!(OpResult::<()>::from_http_status(404).is_permanent());
        assert!(OpResult::<()>::from_http_status(413).is_permanent());
        assert!(!OpResult::<()>::from_http_status(502).is_permanent());
        assert!(!OpResult::<()>::from_http_status(503).is_ok());
    }

    #[test]
    fn test_op_result_with_value() {
        let result: OpResult<String> = OpResult::Ok("hello".to_string());
        assert!(result.is_ok());
        match result {
            OpResult::Ok(v) => assert_eq!(v, "hello"),
            _ => panic!("expected Ok"),
        }
    }
}
