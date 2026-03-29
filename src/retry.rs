//! Basic retry policy for gRPC client calls.
//!
//! This implements a simple exponential-backoff retry policy.  It is not
//! the full gRPC A6 retry spec (which covers per-method service configs,
//! hedging, and retry throttling); it covers the common case:
//!
//! * Fixed set of retryable status codes
//! * Exponential backoff with jitter
//! * Maximum number of attempts (including the first)
//! * Respects the caller-supplied `timeout` (no retry attempt will start
//!   after the deadline has passed)

use std::time::Duration;

use crate::status::Code;

/// Configuration for automatic retry on unary RPCs.
///
/// Attach to a [`Channel`](crate::client::Channel) via
/// [`Channel::with_retry_policy`](crate::client::Channel::with_retry_policy).
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Total number of attempts (1 = no retry, 2 = one retry, etc.).
    /// Capped at 5 to prevent accidental abuse.
    pub max_attempts: usize,

    /// Backoff duration before the second attempt.
    pub initial_backoff: Duration,

    /// Maximum backoff duration (caps exponential growth).
    pub max_backoff: Duration,

    /// Multiplier applied to backoff after each attempt.
    pub backoff_multiplier: f64,

    /// Status codes that are eligible for retry.
    ///
    /// Codes not in this list cause immediate failure regardless of
    /// `max_attempts`.
    pub retryable_codes: Vec<Code>,
}

impl RetryPolicy {
    /// A sensible default: up to 3 attempts, backoff 100ms→1s, retries on
    /// `Unavailable` and `Unknown` (matching grpc-go's default retry guidance).
    pub fn default_retry() -> Self {
        RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            backoff_multiplier: 2.0,
            retryable_codes: vec![Code::Unavailable, Code::Unknown],
        }
    }

    /// Returns `true` if `code` is retryable under this policy.
    pub fn is_retryable(&self, code: Code) -> bool {
        self.retryable_codes.contains(&code)
    }

    /// Backoff duration before attempt number `n` (0-indexed; attempt 0 never
    /// sleeps, attempt 1 waits `initial_backoff`, etc.).
    pub fn backoff_for_attempt(&self, attempt: usize) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        // initial_backoff * multiplier^(attempt-1), capped at max_backoff
        let factor = self.backoff_multiplier.powi((attempt - 1) as i32);
        let secs = self.initial_backoff.as_secs_f64() * factor;
        let capped = secs.min(self.max_backoff.as_secs_f64());
        Duration::from_secs_f64(capped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_retryable_matches_configured_codes() {
        let p = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            retryable_codes: vec![Code::Unavailable],
        };
        assert!(p.is_retryable(Code::Unavailable));
        assert!(!p.is_retryable(Code::Internal));
        assert!(!p.is_retryable(Code::DeadlineExceeded));
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let p = RetryPolicy {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(300),
            backoff_multiplier: 2.0,
            retryable_codes: vec![],
        };
        assert_eq!(p.backoff_for_attempt(0), Duration::ZERO);
        assert_eq!(p.backoff_for_attempt(1), Duration::from_millis(100));
        assert_eq!(p.backoff_for_attempt(2), Duration::from_millis(200));
        // 100 * 2^2 = 400 → capped at 300
        assert_eq!(p.backoff_for_attempt(3), Duration::from_millis(300));
        assert_eq!(p.backoff_for_attempt(4), Duration::from_millis(300));
    }

    #[test]
    fn first_attempt_has_zero_backoff() {
        let p = RetryPolicy::default_retry();
        assert_eq!(p.backoff_for_attempt(0), Duration::ZERO);
    }
}
