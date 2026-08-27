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

/// How long a refusal holds an account out when the response named no reset.
/// A rate limit is a window, not a verdict, so something has to end it.
pub const SPENT_FOR_SECS: i64 = 900;

impl Quota {
    /// Is this account still out of the rotation at `now` (epoch seconds)?
    ///
    /// `rejected` used to mean "forever": it is set in two places and cleared in
    /// none, so one 429 benched an account for the life of the proxy. The
    /// response says when the window resets; past that, the account is usable
    /// again and holding it out only strands it.
    pub fn still_spent(&self, now: i64) -> bool {
        self.rejected && self.reset_secs.is_none_or(|r| now < r)
    }

    /// The same for a refusal that named no reset: it lapses on a fixed window
    /// measured from when it was recorded.
    pub fn still_spent_since(&self, marked_at: i64, now: i64) -> bool {
        match self.reset_secs {
            Some(_) => self.still_spent(now),
            None => self.rejected && now < marked_at + SPENT_FOR_SECS,
        }
    }

    /// Which windows reported `rejected` (e.g. `5h-status`, `7d-status`). Named in
    /// the log so "SPENT" on a successful response is explainable rather than
    /// mysterious.
    pub fn rejected_windows(&self) -> Vec<&str> {
        self.statuses
            .iter()
            .filter(|(_, v)| v.trim().eq_ignore_ascii_case("rejected"))
            .map(|(k, _)| k.as_str())
            .collect()
    }
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

/// Should this turn be tried once more with the body the CLIENT wrote?
///
/// swapdex rewrites a request body in two places: it aligns the account
/// identity in it, and past the wall it may ask for a fallback model instead of
/// the one requested. Both are guesses about what the server will accept, and
/// when the server answers "this request is malformed" the rewrite is the first
/// suspect - the client's own body is known-good by construction, since it is
/// what would have been sent with no proxy at all.
///
/// Only for a refusal ABOUT THE REQUEST. A 429 is about quota and a 5xx is the
/// server's own trouble; re-sending either changes nothing and hides what
/// happened. Once only: if the original is refused too, the request is the
/// problem and repeating it just doubles the wait.
pub fn retry_unrewritten(status: u16, body_was_rewritten: bool, already_retried: u32) -> bool {
    body_was_rewritten && already_retried == 0 && matches!(status, 400 | 422)
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
    fn rejected_windows_names_only_the_closed_ones() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
        ]))
        .expect("quota seen");
        assert_eq!(q.rejected_windows(), vec!["7d-status"]);
        let none = from_headers(&h(&[("anthropic-ratelimit-unified-status", "allowed")]))
            .expect("quota seen");
        assert!(none.rejected_windows().is_empty());
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

#[cfg(test)]
mod spent_expiry_tests {
    use super::*;

    /// A 429 benched an account for the life of the proxy: `rejected` is set in
    /// two places and cleared in none. But a rate limit is a WINDOW - the account
    /// is spent until it resets, and the response says when. Holding it out past
    /// that leaves a usable account sitting idle until someone restarts a
    /// background process they were never told about.
    #[test]
    fn a_spent_account_comes_back_when_its_window_resets() {
        let q = Quota {
            rejected: true,
            statuses: Vec::new(),
            reset_secs: Some(1_000),
        };
        assert!(q.still_spent(999), "before the reset it is out");
        assert!(!q.still_spent(1_000), "at the reset it is back");
        assert!(!q.still_spent(5_000), "and stays back");
    }

    /// With no reset reported there is nothing to wait for, so it lapses on a
    /// fixed window instead - long enough not to walk into the same wall, short
    /// enough that the account is not stranded.
    #[test]
    fn with_no_reset_reported_it_lapses_on_a_window() {
        let q = Quota {
            rejected: true,
            statuses: Vec::new(),
            reset_secs: None,
        };
        assert!(q.still_spent_since(100, 100), "just now");
        assert!(q.still_spent_since(100, 100 + SPENT_FOR_SECS - 1));
        assert!(!q.still_spent_since(100, 100 + SPENT_FOR_SECS));
    }

    #[test]
    fn an_account_that_was_never_refused_is_never_held_out() {
        let q = Quota::default();
        assert!(!q.still_spent(0));
        assert!(!q.still_spent_since(0, 0));
    }
}

/// How long a `Retry-After` may claim before Claude Code stops sleeping and
/// starts a THIRTY MINUTE cooldown instead. Measured from its own retry logic:
/// `429 -> Retry-After (<=20s: sleep, >20s: 30min cooldown)`.
pub const CLIENT_SLEEPS_UP_TO_SECS: u64 = 20;

/// Rewrite the `Retry-After` on a 429 we are handing back, so the client does
/// not sit out half an hour over a wall the user can step around in seconds.
///
/// Only when there IS somewhere to step: another account with room. Then the
/// honest thing is "wait a moment", because a moment is all it takes to press
/// Enter. With nothing to switch to, the real wait stands - telling a client to
/// retry in 20 seconds against a window that reopens in three hours would just
/// walk it into the wall over and over.
pub fn cap_retry_after(
    headers: &[(String, String)],
    somewhere_to_go: bool,
) -> Vec<(String, String)> {
    if !somewhere_to_go {
        return headers.to_vec();
    }
    headers
        .iter()
        .map(|(n, v)| {
            if n.eq_ignore_ascii_case("retry-after") {
                let secs = v
                    .trim()
                    .parse::<u64>()
                    .map(|s| s.min(CLIENT_SLEEPS_UP_TO_SECS))
                    .unwrap_or(CLIENT_SLEEPS_UP_TO_SECS);
                (n.clone(), secs.to_string())
            } else {
                (n.clone(), v.clone())
            }
        })
        .collect()
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }
    fn get<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        hs.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_long_wait_is_capped_when_another_account_could_serve() {
        let out = cap_retry_after(&h(&[("Retry-After", "3600"), ("x-other", "keep")]), true);
        assert_eq!(
            get(&out, "retry-after"),
            Some("20"),
            "the client sleeps instead of cooling down for 30 minutes"
        );
        assert_eq!(
            get(&out, "x-other"),
            Some("keep"),
            "nothing else is touched"
        );
    }

    #[test]
    fn a_short_wait_is_left_alone() {
        let out = cap_retry_after(&h(&[("retry-after", "5")]), true);
        assert_eq!(get(&out, "retry-after"), Some("5"));
    }

    /// With nowhere to switch, the real wait is the useful one: a capped value
    /// would send the client back into the same wall every twenty seconds.
    #[test]
    fn the_real_wait_stands_when_there_is_nowhere_to_go() {
        let out = cap_retry_after(&h(&[("retry-after", "3600")]), false);
        assert_eq!(get(&out, "retry-after"), Some("3600"));
    }

    #[test]
    fn a_response_without_the_header_is_unchanged() {
        let out = cap_retry_after(&h(&[("content-type", "application/json")]), true);
        assert_eq!(out.len(), 1);
        assert_eq!(get(&out, "content-type"), Some("application/json"));
    }
}

/// Does this status mean "this ACCOUNT cannot serve the turn" - as opposed to
/// something wrong with the request itself?
///
/// 403 was missing, and it is the one that says a subscription lapsed. One
/// unentitled account then answered for the whole fleet: every turn hit it, got
/// a 403, and stopped, while accounts with quota sat unused. The upstream
/// project fixed the same hole on 2026-08-04 ("한 계정의 구독 만료가 전체
/// 트래픽을 막던 문제").
///
/// 400 and 404 are deliberately NOT here: those are the request's fault, and
/// retrying them on another account just spends a second account's quota to get
/// the same answer.
pub fn account_cannot_serve(status: u16) -> bool {
    matches!(status, 401 | 403 | 429)
}

#[cfg(test)]
mod failover_status_tests {
    use super::*;

    #[test]
    fn a_lapsed_subscription_moves_the_turn_along() {
        assert!(
            account_cannot_serve(403),
            "403: this account is not entitled"
        );
        assert!(account_cannot_serve(401), "401: its login is not accepted");
        assert!(account_cannot_serve(429), "429: it is out of quota");
    }

    /// A bad request is bad on every account. Retrying it elsewhere spends a
    /// second account's quota to be told the same thing.
    #[test]
    fn a_broken_request_is_not_an_account_problem() {
        for s in [200, 400, 404, 500, 529] {
            assert!(!account_cannot_serve(s), "{s} is not the account's fault");
        }
    }
}

/// Is there enough evidence to bench this account for the next quarter of an
/// hour, or only enough to move THIS turn along?
///
/// Two different verdicts have been sharing one signal. A 429 is reason enough
/// to serve the turn elsewhere - the account said no, and arguing costs the
/// user a turn. But writing "spent" against the account holds it out of the
/// rotation for fifteen minutes, and a bare 429 with no rate-limit headers is a
/// throttle as often as a wall. Benching on that is how an account with quota
/// left sits idle.
///
/// The response's own headers are the proof: `*-status: rejected` says the
/// window is gone. Repeated refusals are the other proof - an account that keeps
/// saying no past the retries is out, whatever it declined to explain.
/// The retry count a turn carries into a DIFFERENT account: none.
///
/// `proven_spent` treats three attempts as proof a window is spent, which is
/// true of the account those attempts were made against and of no other. The
/// counter lived outside the account loop and survived rotation, so a 529 spell
/// on one account handed the next a counter already reading "spent" - and its
/// first bare throttle, carrying no rate-limit headers at all, benched it on no
/// evidence of its own. One transient overload took every account out at once.
pub fn attempts_against_next_account() -> u32 {
    0
}

pub fn proven_spent(headers: &[(String, String)], attempt: u32) -> bool {
    from_headers(headers).is_some_and(|q| q.rejected) || attempt >= 3
}

#[cfg(test)]
mod evidence_tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn the_response_saying_rejected_is_proof() {
        assert!(proven_spent(
            &h(&[("anthropic-ratelimit-unified-status", "rejected")]),
            0
        ));
    }

    /// A bare 429 explains nothing. Move the turn along by all means, but do not
    /// hold the account out of the rotation for a quarter of an hour on it.
    #[test]
    fn a_bare_refusal_is_not_proof() {
        assert!(!proven_spent(&h(&[]), 0));
        assert!(!proven_spent(&h(&[("retry-after", "5")]), 0));
    }

    /// Saying no over and over IS the explanation.
    #[test]
    fn refusing_past_the_retries_is_proof_enough() {
        assert!(proven_spent(&h(&[]), 3));
    }

    /// A window the response calls fine is not spent, however many times it is
    /// asked - that would bench an account for something else's fault.
    #[test]
    fn a_window_reported_healthy_is_never_called_spent_early() {
        let ok = h(&[("anthropic-ratelimit-unified-status", "allowed")]);
        assert!(!proven_spent(&ok, 0));
        assert!(!proven_spent(&ok, 2));
    }
}

#[cfg(test)]
mod rewrite_retry_tests {
    use super::*;

    /// swapdex rewrites a request body in two places - it aligns the account
    /// identity, and past the wall it may ask for a fallback model. If the
    /// server then refuses the request as malformed, the rewrite is the first
    /// suspect, and what the client actually asked for is still available.
    #[test]
    fn a_rejected_rewrite_is_worth_one_try_as_the_client_wrote_it() {
        assert!(retry_unrewritten(400, true, 0));
        // Nothing was rewritten - there is nothing to fall back to.
        assert!(!retry_unrewritten(400, false, 0));
        // Once only: if the original is refused too, the request is the
        // problem and repeating it just doubles the wait.
        assert!(!retry_unrewritten(400, true, 1));
    }

    /// Only a request-shaped refusal. A 429 is about quota and a 500 is the
    /// server's own trouble; re-sending either as the client wrote it would
    /// change nothing and hide what happened.
    #[test]
    fn other_failures_are_not_blamed_on_the_rewrite() {
        for status in [200, 401, 403, 429, 500, 529] {
            assert!(!retry_unrewritten(status, true, 0), "{status}");
        }
        // 422 is the other shape-of-request refusal this API uses.
        assert!(retry_unrewritten(422, true, 0));
    }
}

#[cfg(test)]
mod attempt_scope_tests {
    use super::*;

    /// Retries are evidence about ONE account, and do not carry to the next.
    ///
    /// `proven_spent` treats `attempt >= 3` as proof a window is spent, and the
    /// counter lived outside the account loop and was never reset on rotation.
    /// A 529 spell - the log here recorded eighteen inside one minute - drives
    /// it to 3 on the first account. Rotating then hands the next account a
    /// counter that already says "spent", so its very first bare throttle, with
    /// no rate-limit headers at all, benches it on no evidence of its own. One
    /// transient overload takes the whole fleet out for fifteen minutes.
    #[test]
    fn a_retry_count_does_not_follow_the_turn_to_another_account() {
        // A bare throttle - no unified headers - is not proof on its own.
        assert!(!proven_spent(&[], 0), "no headers, no attempts: not proven");
        assert!(
            proven_spent(&[], 3),
            "three tries at one account is the rule"
        );

        // Rotating resets the evidence, because it is about the account.
        assert_eq!(attempts_against_next_account(), 0);
    }
}
