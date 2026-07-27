//! Quota state read off the response headers the API already sends. Header NAMES
//! are matched by prefix rather than an exact list, so an added window (5h, 7d,
//! or another) is picked up without a code change - the exact names are not
//! documented. Nothing here probes the API: this is telemetry that arrives on
//! traffic the user was making anyway.

const PREFIX: &str = "anthropic-ratelimit-unified-";

#[derive(Debug, Default, Clone)]
pub struct Quota {
    /// Any window reported `rejected` - this account is spent.
    pub rejected: bool,
    /// Every `*-status` header seen, for display and diagnosis.
    pub statuses: Vec<(String, String)>,
    /// Soonest reported reset, epoch seconds.
    pub reset_secs: Option<i64>,
}

/// `None` when the response carried no unified rate-limit headers at all, so a
/// non-API response (a probe, an error page) never overwrites a known-good state
/// with an empty one.
pub fn from_headers(headers: &[(String, String)]) -> Option<Quota> {
    let mut q = Quota::default();
    let mut seen = false;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix(PREFIX) else {
            continue;
        };
        seen = true;
        if rest == "status" || rest.ends_with("-status") {
            if value.trim().eq_ignore_ascii_case("rejected") {
                q.rejected = true;
            }
            q.statuses.push((rest.to_string(), value.clone()));
        } else if rest == "reset" || rest.ends_with("-reset") {
            if let Ok(n) = value.trim().parse::<i64>() {
                q.reset_secs = Some(q.reset_secs.map_or(n, |cur| cur.min(n)));
            }
        }
    }
    seen.then_some(q)
}

/// How a 429 should be handled. Observed 2026-07-27 against the real API: a
/// throttle 429 carries `x-should-retry: true`, a `rate_limit_error` body, and NO
/// `anthropic-ratelimit-unified-*` headers at all - so "the account is spent" and
/// "slow down for a moment" are different events that look similar. Treating a
/// throttle as exhaustion would abandon a perfectly good account; treating
/// exhaustion as a throttle would retry a wall forever.
#[derive(Debug, PartialEq)]
pub enum Throttle {
    /// Retry the SAME account after this long.
    RetryAfter(std::time::Duration),
    /// Do not retry here - the account is out of quota (rotate instead).
    Exhausted,
}

/// Classify a 429. `attempt` is how many retries have already been spent, so a
/// throttled account cannot loop forever. Backoff is 1s, 2s, 4s.
pub fn classify_429(headers: &[(String, String)], attempt: u32) -> Throttle {
    const MAX_RETRIES: u32 = 3;
    let quota_says_spent = from_headers(headers).is_some_and(|q| q.rejected);
    let retryable = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("x-should-retry") && v.trim().eq_ignore_ascii_case("true")
    });
    if quota_says_spent || !retryable || attempt >= MAX_RETRIES {
        return Throttle::Exhausted;
    }
    // Honor an explicit retry-after when the server sends one.
    let after = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, v)| v.trim().parse::<u64>().ok())
        .unwrap_or(1u64 << attempt);
    Throttle::RetryAfter(std::time::Duration::from_secs(after.min(8)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn allowed_status_is_not_rejected() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ]))
        .expect("quota seen");
        assert!(!q.rejected);
    }

    #[test]
    fn any_rejected_window_marks_the_account_spent() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ]))
        .expect("quota seen");
        assert!(q.rejected, "a rejected window exhausts the account");
    }

    #[test]
    fn reset_is_the_soonest_and_absent_headers_yield_none() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-7d-reset", "1900000000"),
            ("anthropic-ratelimit-unified-5h-reset", "1800000000"),
        ]))
        .expect("quota seen");
        assert_eq!(q.reset_secs, Some(1_800_000_000), "soonest reset wins");
        assert!(
            from_headers(&h(&[("content-type", "application/json")])).is_none(),
            "a response with no unified headers must not overwrite known state"
        );
    }

    #[test]
    fn a_retryable_429_is_a_throttle_and_a_spent_one_is_not() {
        // The real shape: x-should-retry, no unified headers at all.
        let throttle = h(&[("x-should-retry", "true")]);
        assert_eq!(
            classify_429(&throttle, 0),
            Throttle::RetryAfter(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            classify_429(&throttle, 2),
            Throttle::RetryAfter(std::time::Duration::from_secs(4)),
            "backoff grows with the attempt"
        );
        assert_eq!(
            classify_429(&throttle, 3),
            Throttle::Exhausted,
            "retries are bounded so a throttled account cannot loop forever"
        );
        // An explicit retry-after wins over the backoff, capped.
        assert_eq!(
            classify_429(&h(&[("x-should-retry", "true"), ("retry-after", "5")]), 0),
            Throttle::RetryAfter(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            classify_429(&h(&[("x-should-retry", "true"), ("retry-after", "600")]), 0),
            Throttle::RetryAfter(std::time::Duration::from_secs(8)),
            "a huge retry-after is capped - rotating beats sleeping for minutes"
        );
        // Quota actually spent, or no retry hint: do not retry here.
        assert_eq!(
            classify_429(
                &h(&[
                    ("x-should-retry", "true"),
                    ("anthropic-ratelimit-unified-status", "rejected")
                ]),
                0
            ),
            Throttle::Exhausted,
            "a rejected window means the wall, not a throttle"
        );
        assert_eq!(classify_429(&h(&[]), 0), Throttle::Exhausted);
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let q = from_headers(&h(&[("Anthropic-RateLimit-Unified-Status", "REJECTED")]))
            .expect("quota seen");
        assert!(q.rejected);
    }
}
