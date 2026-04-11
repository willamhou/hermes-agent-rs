//! Retry policy for transient provider errors.

use std::time::Duration;

use hermes_core::error::{HermesError, ProviderError};

/// Decides whether and when to retry a failed provider request.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (not counting the initial request).
    pub max_retries: u32,
    /// Backoff duration for the first retry.
    pub initial_backoff: Duration,
    /// Upper bound on computed backoff.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Action returned by [`RetryPolicy::should_retry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryAction {
    /// Wait for the given duration then retry.
    RetryAfter(Duration),
    /// Do not retry; surface the error to the caller.
    DoNotRetry,
}

impl RetryPolicy {
    /// Create a new policy with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Determine whether `error` should be retried after `attempt` failures.
    ///
    /// * `attempt` is 0-based: 0 means the initial request just failed.
    /// * `retry_after_header` is the parsed value of the `Retry-After` HTTP
    ///   header in seconds, if present.
    pub fn should_retry(
        &self,
        error: &HermesError,
        attempt: u32,
        retry_after_header: Option<f64>,
    ) -> RetryAction {
        if attempt >= self.max_retries {
            return RetryAction::DoNotRetry;
        }

        match error {
            HermesError::Provider(provider_err) => {
                self.handle_provider_error(provider_err, attempt, retry_after_header)
            }
            _ => RetryAction::DoNotRetry,
        }
    }

    fn handle_provider_error(
        &self,
        error: &ProviderError,
        attempt: u32,
        retry_after_header: Option<f64>,
    ) -> RetryAction {
        match error {
            ProviderError::RateLimited { retry_after } => {
                // Priority: header > error field > computed backoff
                let secs = retry_after_header
                    .or(*retry_after)
                    .map(Duration::from_secs_f64)
                    .unwrap_or_else(|| self.compute_backoff(attempt));
                RetryAction::RetryAfter(secs)
            }

            ProviderError::Network(_) | ProviderError::Timeout(_) => {
                RetryAction::RetryAfter(self.compute_backoff(attempt))
            }

            ProviderError::ApiError { status, .. } => match status {
                429 | 500 | 502 | 503 | 529 => {
                    RetryAction::RetryAfter(self.compute_backoff(attempt))
                }
                _ => RetryAction::DoNotRetry,
            },

            ProviderError::AuthError
            | ProviderError::ModelNotFound(_)
            | ProviderError::ContextLengthExceeded { .. }
            | ProviderError::SseParse(_) => RetryAction::DoNotRetry,
        }
    }

    /// Exponential backoff with ±25% deterministic jitter based on `attempt % 4`.
    ///
    /// The jitter factor cycles through `[0.75, 0.875, 1.0, 1.125, 1.25]` for
    /// attempts 0–4 so results are fully deterministic (no randomness).
    pub fn compute_backoff(&self, attempt: u32) -> Duration {
        // Base: initial_backoff * 2^attempt
        let base_ms = self.initial_backoff.as_millis() as f64 * 2f64.powi(attempt as i32);

        // Deterministic jitter: ±25% in 4 steps cycling with attempt % 4
        let jitter = match attempt % 4 {
            0 => 1.0,
            1 => 1.25,
            2 => 0.75,
            _ => 0.875,
        };

        let ms = (base_ms * jitter) as u64;
        let clamped = ms.min(self.max_backoff.as_millis() as u64);
        Duration::from_millis(clamped)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::error::{HermesError, ProviderError};

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    fn provider(e: ProviderError) -> HermesError {
        HermesError::Provider(e)
    }

    // ── Rate-limited ──────────────────────────────────────────────────────────

    #[test]
    fn test_rate_limited_uses_retry_after_header() {
        let err = provider(ProviderError::RateLimited {
            retry_after: Some(5.0),
        });
        let action = policy().should_retry(&err, 0, Some(10.0));
        // Header (10s) wins over error field (5s)
        assert_eq!(action, RetryAction::RetryAfter(Duration::from_secs(10)));
    }

    #[test]
    fn test_rate_limited_uses_error_field_if_no_header() {
        let err = provider(ProviderError::RateLimited {
            retry_after: Some(7.0),
        });
        let action = policy().should_retry(&err, 0, None);
        assert_eq!(
            action,
            RetryAction::RetryAfter(Duration::from_secs_f64(7.0))
        );
    }

    #[test]
    fn test_rate_limited_falls_back_to_backoff() {
        let err = provider(ProviderError::RateLimited { retry_after: None });
        let action = policy().should_retry(&err, 0, None);
        // Should be a RetryAfter (backoff), not DoNotRetry
        assert!(matches!(action, RetryAction::RetryAfter(_)));
    }

    // ── Network / Timeout ─────────────────────────────────────────────────────

    #[test]
    fn test_network_error_retries() {
        let err = provider(ProviderError::Network("connection refused".into()));
        let action = policy().should_retry(&err, 0, None);
        assert!(matches!(action, RetryAction::RetryAfter(_)));

        let timeout_err = provider(ProviderError::Timeout(30));
        let action2 = policy().should_retry(&timeout_err, 0, None);
        assert!(matches!(action2, RetryAction::RetryAfter(_)));
    }

    // ── Status codes ──────────────────────────────────────────────────────────

    #[test]
    fn test_500_retries() {
        let retryable = [429u16, 500, 502, 503, 529];
        for status in retryable {
            let err = provider(ProviderError::ApiError {
                status,
                message: "server error".into(),
            });
            let action = policy().should_retry(&err, 0, None);
            assert!(
                matches!(action, RetryAction::RetryAfter(_)),
                "status {status} should be retried"
            );
        }
    }

    #[test]
    fn test_400_does_not_retry() {
        let err = provider(ProviderError::ApiError {
            status: 400,
            message: "bad request".into(),
        });
        assert_eq!(
            policy().should_retry(&err, 0, None),
            RetryAction::DoNotRetry
        );
    }

    // ── Non-retryable errors ──────────────────────────────────────────────────

    #[test]
    fn test_auth_error_does_not_retry() {
        let err = provider(ProviderError::AuthError);
        assert_eq!(
            policy().should_retry(&err, 0, None),
            RetryAction::DoNotRetry
        );

        let err2 = provider(ProviderError::ModelNotFound("gpt-99".into()));
        assert_eq!(
            policy().should_retry(&err2, 0, None),
            RetryAction::DoNotRetry
        );

        let err3 = provider(ProviderError::ContextLengthExceeded {
            used: 200_000,
            max: 128_000,
        });
        assert_eq!(
            policy().should_retry(&err3, 0, None),
            RetryAction::DoNotRetry
        );

        let err4 = provider(ProviderError::SseParse("bad sse".into()));
        assert_eq!(
            policy().should_retry(&err4, 0, None),
            RetryAction::DoNotRetry
        );
    }

    // ── Max retries ───────────────────────────────────────────────────────────

    #[test]
    fn test_max_retries_exceeded() {
        let err = provider(ProviderError::Network("oops".into()));
        let p = policy(); // max_retries = 3
        // attempt 2 should still retry
        assert!(matches!(
            p.should_retry(&err, 2, None),
            RetryAction::RetryAfter(_)
        ));
        // attempt 3 equals max_retries → DoNotRetry
        assert_eq!(p.should_retry(&err, 3, None), RetryAction::DoNotRetry);
    }

    // ── Backoff growth ────────────────────────────────────────────────────────

    #[test]
    fn test_backoff_increases_with_attempts() {
        let p = policy();
        let b0 = p.compute_backoff(0);
        let b1 = p.compute_backoff(1);
        let b2 = p.compute_backoff(2);
        // Each step should be larger than the previous (modulo jitter direction)
        // At minimum, b2 (4x base * 0.75) > b0 (1x base * 1.0) when initial=500ms
        // b0 = 500 * 1.0 = 500ms
        // b1 = 1000 * 1.25 = 1250ms
        // b2 = 2000 * 0.75 = 1500ms
        assert!(b1 > b0, "b1 ({b1:?}) should exceed b0 ({b0:?})");
        assert!(b2 > b0, "b2 ({b2:?}) should exceed b0 ({b0:?})");
    }

    // ── Non-provider error ────────────────────────────────────────────────────

    #[test]
    fn test_non_provider_error_does_not_retry() {
        let err = HermesError::Config("bad config".into());
        assert_eq!(
            policy().should_retry(&err, 0, None),
            RetryAction::DoNotRetry
        );

        let err2 = HermesError::Tool {
            name: "foo".into(),
            message: "fail".into(),
        };
        assert_eq!(
            policy().should_retry(&err2, 0, None),
            RetryAction::DoNotRetry
        );
    }
}
