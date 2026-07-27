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
    fn header_names_are_matched_case_insensitively() {
        let q = from_headers(&h(&[("Anthropic-RateLimit-Unified-Status", "REJECTED")]))
            .expect("quota seen");
        assert!(q.rejected);
    }
}
