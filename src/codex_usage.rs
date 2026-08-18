//! Codex usage, asked of the account itself.
//!
//! `codex_limits` reads what Codex happened to write into a home's transcripts.
//! This asks the account, with that account's own token, and gets back an
//! answer that names itself: `email`, `plan_type`, the windows, the per-model
//! limits, the credit balance, and why a refusal happened.
//!
//! That difference matters for one reason above the others. A transcript can
//! only be attributed by the home it sits in, and a home with no transcripts
//! yields nothing at all - a saved account that has not been used through this
//! machine is a permanent blank. The endpoint answers per CREDENTIAL, so it
//! reports that account too, and says whose the numbers are rather than leaving
//! it to be inferred.
//!
//! Same discipline as `quota`, for the same reasons: read-only, run only when
//! the user asks, curl with its config on stdin so the token never reaches
//! `ps`, an honest `User-Agent`, and no HTTP client in the dependency graph.

use serde_json::Value;

pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// What one account says about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Account {
    /// The account this token belongs to, as the endpoint names it.
    pub email: Option<String>,
    pub plan: Option<String>,
    /// The plan windows, shortest first - the same shape `codex_limits`
    /// produces, so both sources feed one row builder.
    pub limits: crate::codex_limits::Limits,
    /// Per-model windows the response lists separately, label -> window.
    pub scoped: Vec<(String, crate::codex_limits::Window)>,
    pub credits: Option<Credits>,
    /// Why the account is refusing, when it is. `None` means the response said
    /// nothing, which is not the same as "it is fine".
    pub refused: Option<String>,
}

/// Pre-purchased credits, the way past a full window - and the reason a window
/// at 100% is not the end of an account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    /// The spend cap is hit: there was a way through and it is closed.
    pub overage_limit_reached: bool,
    /// Reported as a string, and kept as one: it is a decimal balance and
    /// rounding it into an f64 to print it back is a loss for nothing.
    pub balance: Option<String>,
}

/// One window, converted to swapdex's units.
///
/// The response measures a window in SECONDS and every other window in swapdex
/// is minutes; converting at the boundary is what lets one row builder place a
/// window from either source by its length.
fn window_from(v: &Value) -> Option<crate::codex_limits::Window> {
    let used_pct = v.get("used_percent")?.as_f64()?;
    let window_minutes = v
        .get("limit_window_seconds")
        .and_then(Value::as_i64)
        .map(|s| s / 60)
        .unwrap_or(0);
    Some(crate::codex_limits::Window {
        used_pct,
        window_minutes,
        resets_at: v.get("reset_at").and_then(Value::as_i64),
    })
}

/// Both windows of one `rate_limit` block, shortest first.
fn windows_of(
    v: &Value,
) -> (
    Option<crate::codex_limits::Window>,
    Option<crate::codex_limits::Window>,
) {
    let a = v.get("primary_window").and_then(window_from);
    let b = v.get("secondary_window").and_then(window_from);
    match (a, b) {
        (Some(x), Some(y)) if y.window_minutes < x.window_minutes => (Some(y), Some(x)),
        pair => pair,
    }
}

/// Read one account's answer.
///
/// `None` when the body is not this endpoint's - an error page, a login wall, a
/// truncated read. A response that cannot be recognised is reported as nothing
/// rather than as an account with no usage, which would read as "all clear".
pub fn parse(body: &str) -> Option<Account> {
    let v: Value = serde_json::from_str(body).ok()?;
    let rate_limit = v.get("rate_limit")?;
    let (short, long) = windows_of(rate_limit);

    let scoped = v
        .get("additional_rate_limits")
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(|x| {
                    let name = x.get("limit_name")?.as_str()?.to_string();
                    let (w, _) = windows_of(x.get("rate_limit")?);
                    Some((name, w?))
                })
                .collect()
        })
        .unwrap_or_default();

    let credits = v.get("credits").filter(|c| c.is_object()).map(|c| {
        let flag = |k: &str| c.get(k).and_then(Value::as_bool).unwrap_or(false);
        Credits {
            has_credits: flag("has_credits"),
            unlimited: flag("unlimited"),
            overage_limit_reached: flag("overage_limit_reached"),
            balance: c.get("balance").and_then(Value::as_str).map(str::to_string),
        }
    });

    Some(Account {
        email: v.get("email").and_then(Value::as_str).map(str::to_string),
        plan: v
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        limits: crate::codex_limits::Limits {
            short,
            long,
            observed_at: None,
        },
        scoped,
        credits,
        refused: v
            .get("rate_limit_reached_type")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The same reading, taken off a response the proxy was already carrying.
///
/// Free and fresh: no extra request, and bound to the credential that actually
/// served the turn, so there is nothing to attribute. It only appears when the
/// user is working, which is why it supplements the endpoint rather than
/// replacing it - an idle account never sends one.
///
/// `None` when the response carried none of these headers, so an unrelated
/// response never overwrites a known reading with a blank one. Only the plan
/// windows are read; per-model limits are documented elsewhere as arriving under
/// their own `x-<limit-id>-*` names, but none has been observed here and a
/// parser for a shape nobody has seen is a guess.
pub fn from_headers(headers: &[(String, String)]) -> Option<Account> {
    let get = |want: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(want))
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    let window = |kind: &str| {
        let used_pct: f64 = get(&format!("x-codex-{kind}-used-percent"))?.parse().ok()?;
        // A percent with no window length cannot be placed in a column, and
        // picking one for it is how a weekly number lands in the session gauge.
        let window_minutes: i64 = get(&format!("x-codex-{kind}-window-minutes"))?
            .parse()
            .ok()?;
        Some(crate::codex_limits::Window {
            used_pct,
            window_minutes,
            resets_at: get(&format!("x-codex-{kind}-reset-at")).and_then(reset_at),
        })
    };
    let (short, long) = match (window("primary"), window("secondary")) {
        (Some(x), Some(y)) if y.window_minutes < x.window_minutes => (Some(y), Some(x)),
        pair => pair,
    };
    let refused = get("x-codex-rate-limit-reached-type").map(str::to_string);
    if short.is_none() && long.is_none() {
        return None;
    }
    Some(Account {
        limits: crate::codex_limits::Limits {
            short,
            long,
            observed_at: None,
        },
        refused,
        ..Default::default()
    })
}

/// Plain words for a refusal, and - where the reason distinguishes them - who
/// can clear it.
///
/// The workspace reasons come in owner and member forms, and that difference is
/// the whole value of showing them: one of you can top the workspace up, the
/// other can only go and ask.
///
/// A reason nobody has words for is returned as it arrived. Dropping it would
/// leave an account refusing with no explanation, which is the exact failure
/// this field exists to prevent.
pub fn refusal_words(kind: &str) -> String {
    match kind.trim().to_ascii_lowercase().as_str() {
        "rate_limit_reached" => "out of quota".into(),
        "workspace_owner_credits_depleted" => "credits spent - top them up in the workspace".into(),
        "workspace_member_credits_depleted" => {
            "workspace credits spent - its owner has to top them up".into()
        }
        "workspace_owner_usage_limit_reached" => {
            "spend limit reached - raise it in the workspace".into()
        }
        "workspace_member_usage_limit_reached" => {
            "workspace spend limit reached - its owner has to raise it".into()
        }
        _ => kind.trim().to_string(),
    }
}

/// Record what a Codex response said about the account that served it.
///
/// Returns whether anything was recorded. A response carrying none of these
/// headers is not a reading of zero - it is no reading - so it leaves what is
/// remembered alone. Kept per tool: slot names are unique only within one, and
/// a Codex `work` must not overwrite a Claude `work`.
pub fn remember(
    paths: &crate::paths::Paths,
    serving: &str,
    headers: &[(String, String)],
    at: i64,
) -> bool {
    let Some(a) = from_headers(headers) else {
        return false;
    };
    let p = crate::codex_limits::place(&a.limits);
    let entry = crate::quota_cache::Entry {
        five_h: p.five_h.map(|w| w.used_pct),
        five_h_reset: p.five_h.and_then(|w| w.resets_at),
        seven_d: p.seven_d.map(|w| w.used_pct),
        seven_d_reset: p.seven_d.and_then(|w| w.resets_at),
        at,
        on_credits: false,
    };
    crate::quota_cache::update_for(paths, "codex", &[(serving.to_string(), entry)]);
    true
}

/// A reset time as either form it arrives in: unix seconds, or a timestamp.
fn reset_at(v: &str) -> Option<i64> {
    match v.parse::<i64>() {
        Ok(n) if n > 0 => Some(n),
        _ => crate::session_link::rfc3339_to_secs(v),
    }
}

/// What one read produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetch {
    Ok(Box<Account>),
    /// The token was rejected. Also what a home that has never been signed into
    /// gets, since there is no token to send.
    Unauthorized,
    /// The endpoint itself is rate-limited - which says nothing whatever about
    /// the account's own quota, and must never be shown as though it did.
    Throttled,
    /// A reply we could not read. Kept whole so `--json` can show what came
    /// back rather than asserting a shape it did not have.
    Unexpected(u32, String),
    Offline(String),
}

impl Fetch {
    /// Why this read produced no number, in the words a log should use.
    pub fn why_no_number(&self) -> Option<&'static str> {
        match self {
            Self::Ok(_) => None,
            Self::Throttled => Some("usage endpoint throttled"),
            Self::Unauthorized => Some("token rejected"),
            Self::Offline(_) => Some("could not reach the endpoint"),
            Self::Unexpected(_, _) => Some("unexpected reply"),
        }
    }
}

pub fn classify(code: u32, body: String) -> Fetch {
    match code {
        401 | 403 => Fetch::Unauthorized,
        429 => Fetch::Throttled,
        200..=299 => match parse(&body) {
            Some(a) => Fetch::Ok(Box::new(a)),
            None => Fetch::Unexpected(code, body),
        },
        0 => Fetch::Offline("no response from chatgpt.com".into()),
        c => Fetch::Unexpected(c, body),
    }
}

/// Ask one account, using its own login.
///
/// `chatgpt-account-id` rides along because the token alone does not say which
/// workspace to answer for. It is omitted when the saved id is a placeholder
/// rather than a real one - Codex writes `email_`/`local_` forms for logins that
/// have no workspace, and sending those gets the whole request rejected.
pub fn fetch(auth: &crate::proxy::codex::Auth) -> Fetch {
    let Ok(token) = std::str::from_utf8(auth.token.expose()) else {
        return Fetch::Unauthorized;
    };
    if !crate::quota::token_usable(token) {
        return Fetch::Unauthorized;
    }
    let mut cfg = format!(
        "url = \"{USAGE_URL}\"\n\
         header = \"Authorization: Bearer {token}\"\n\
         header = \"Accept: application/json\"\n\
         header = \"User-Agent: swapdex\"\n"
    );
    if workspace_id(&auth.account_id) {
        cfg.push_str(&format!(
            "header = \"chatgpt-account-id: {}\"\n",
            auth.account_id
        ));
    }
    cfg.push_str(
        "silent\n\
         show-error\n\
         connect-timeout = 6\n\
         max-time = 15\n\
         write-out = \"\\n%{http_code}\"\n",
    );
    match crate::quota::run_curl_cfg(&cfg) {
        Ok((body, code)) => classify(code, body),
        Err(e) => Fetch::Offline(e),
    }
}

/// Is this a real workspace id, or one of Codex's placeholders?
fn workspace_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && !id.starts_with("email_")
        && !id.starts_with("local_")
        && crate::quota::token_usable(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded response, with the identifiers replaced. Every field this
    /// module reads appears here, including the ones a healthy account leaves
    /// null - `secondary_window` among them, which is why the 5h gauge is
    /// empty and not broken.
    const BODY: &str = r#"{
      "user_id": "user-EXAMPLE",
      "account_id": "00000000-0000-0000-0000-000000000000",
      "email": "someone@example.com",
      "plan_type": "pro",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": {
          "used_percent": 84,
          "limit_window_seconds": 604800,
          "reset_after_seconds": 501720,
          "reset_at": 1787196620
        },
        "secondary_window": null
      },
      "code_review_rate_limit": null,
      "additional_rate_limits": [
        {
          "limit_name": "GPT-5.3-Codex-Spark",
          "metered_feature": "codex_bengalfox",
          "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
              "used_percent": 0,
              "limit_window_seconds": 604800,
              "reset_after_seconds": 604800,
              "reset_at": 1787299682
            },
            "secondary_window": null
          }
        }
      ],
      "credits": {
        "has_credits": false,
        "unlimited": false,
        "overage_limit_reached": false,
        "balance": "0"
      },
      "spend_control": { "reached": false, "individual_limit": null },
      "rate_limit_reached_type": null
    }"#;

    #[test]
    fn a_reading_names_the_account_it_came_from() {
        let a = parse(BODY).expect("a recorded response parses");
        assert_eq!(a.email.as_deref(), Some("someone@example.com"));
        assert_eq!(a.plan.as_deref(), Some("pro"));
    }

    #[test]
    fn windows_carry_their_length_in_minutes() {
        let a = parse(BODY).expect("parses");
        // The response states seconds; every other window in swapdex is
        // minutes, and the row builder places a window by its length.
        let w = a.limits.short.expect("the plan window");
        assert_eq!(w.used_pct, 84.0);
        assert_eq!(w.window_minutes, 10_080);
        assert_eq!(w.resets_at, Some(1_787_196_620));
        // Reported as null, so absent - not zero, and not invented.
        assert_eq!(a.limits.long, None);
    }

    #[test]
    fn per_model_limits_are_kept_under_their_own_names() {
        let a = parse(BODY).expect("parses");
        assert_eq!(a.scoped.len(), 1);
        assert_eq!(a.scoped[0].0, "GPT-5.3-Codex-Spark");
        assert_eq!(a.scoped[0].1.used_pct, 0.0);
    }

    #[test]
    fn credits_are_read_whole() {
        let a = parse(BODY).expect("parses");
        let c = a.credits.expect("the response describes credits");
        assert!(!c.has_credits);
        assert!(!c.overage_limit_reached);
        assert_eq!(c.balance.as_deref(), Some("0"));
        // Nothing refused this account, and the response saying so is not the
        // same as the response being silent.
        assert_eq!(a.refused, None);
    }

    #[test]
    fn a_refusal_is_carried_verbatim() {
        let body = BODY.replace(
            "\"rate_limit_reached_type\": null",
            "\"rate_limit_reached_type\": \"usage_limit\"",
        );
        assert_eq!(
            parse(&body).unwrap().refused.as_deref(),
            Some("usage_limit")
        );
    }

    #[test]
    fn a_body_that_is_not_this_endpoint_is_not_guessed_at() {
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"error":"unauthorized"}"#), None);
    }

    /// A throttled endpoint and a rejected token are different news, and a 200
    /// carrying something else is neither. Collapsing any of them into "no
    /// usage" is how an account that is fine reads as an account that is spent.
    #[test]
    fn each_failure_keeps_its_own_name() {
        assert_eq!(classify(429, String::new()), Fetch::Throttled);
        assert_eq!(classify(401, String::new()), Fetch::Unauthorized);
        assert_eq!(classify(403, String::new()), Fetch::Unauthorized);
        assert!(matches!(
            classify(200, "<html>login</html>".into()),
            Fetch::Unexpected(200, _)
        ));
        assert!(classify(200, BODY.into()).why_no_number().is_none());
        assert_eq!(
            classify(429, String::new()).why_no_number(),
            Some("usage endpoint throttled")
        );
    }

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// The proxy already carries every Codex response. When one names the
    /// account's windows, that reading is free, fresh, and bound to the
    /// credential that served the turn.
    #[test]
    fn windows_are_read_off_a_response_the_proxy_already_has() {
        let got = from_headers(&h(&[
            ("content-type", "text/event-stream"),
            ("X-Codex-Primary-Used-Percent", "42.5"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", "1787196620"),
            ("x-codex-secondary-used-percent", "7"),
            ("x-codex-secondary-window-minutes", "300"),
            ("x-codex-secondary-reset-at", "1786600000"),
        ]))
        .expect("these headers are a reading");
        // Shortest first, whichever label carried it - the 300-minute window is
        // the session one even though it arrived as `secondary`.
        let short = got.limits.short.expect("the session window");
        assert_eq!(short.window_minutes, 300);
        assert_eq!(short.used_pct, 7.0);
        assert_eq!(short.resets_at, Some(1_786_600_000));
        let long = got.limits.long.expect("the weekly window");
        assert_eq!(long.window_minutes, 10080);
        assert_eq!(long.used_pct, 42.5);
    }

    /// A reset can arrive as a unix integer or as a timestamp string.
    #[test]
    fn a_reset_time_is_read_in_either_form_it_arrives_in() {
        let iso = from_headers(&h(&[
            ("x-codex-primary-used-percent", "10"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", "2026-08-18T04:30:20Z"),
        ]))
        .expect("a reading");
        assert_eq!(iso.limits.short.unwrap().resets_at, Some(1_787_027_420));
    }

    /// Anything that is not this - an error page, a plain SSE response, a
    /// health check - is no reading at all. Returning an empty one would
    /// overwrite a good number with a blank on every unrelated response.
    #[test]
    fn a_response_without_them_is_not_a_reading() {
        assert!(from_headers(&h(&[("content-type", "application/json")])).is_none());
        // A percent with no window length cannot be placed in a column, and
        // guessing which column it belongs to is how a weekly number lands in
        // the session gauge.
        assert!(from_headers(&h(&[("x-codex-primary-used-percent", "42")])).is_none());
    }

    /// The refusal reason rides along on the same response, and it is the
    /// difference between "out of quota" and "something else stopped this".
    #[test]
    fn a_refusal_reason_on_the_response_is_kept() {
        let got = from_headers(&h(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-rate-limit-reached-type", "usage_limit"),
        ]))
        .expect("a reading");
        assert_eq!(got.refused.as_deref(), Some("usage_limit"));
    }

    /// A reading taken off a response is remembered under the account that
    /// SERVED it - the credential the turn actually went out on, which is what
    /// makes this the one Codex source needing no attribution at all.
    #[test]
    fn a_response_reading_is_remembered_under_the_serving_account() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(root.path());
        let headers = h(&[
            ("x-codex-primary-used-percent", "42.5"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", "1787196620"),
        ]);
        assert!(remember(&paths, "work", &headers, 1_786_600_000));

        let c = crate::quota_cache::load_for(&paths, "codex");
        let e = c.get("work").expect("the serving account was recorded");
        // A weekly window fills the weekly column, whatever label carried it.
        assert_eq!(e.seven_d, Some(42.5));
        assert_eq!(e.seven_d_reset, Some(1_787_196_620));
        assert_eq!(e.five_h, None);
        assert_eq!(e.at, 1_786_600_000);

        // A response carrying none of these headers leaves the reading alone
        // rather than overwriting it with a blank.
        assert!(!remember(
            &paths,
            "work",
            &h(&[("content-type", "text/plain")]),
            1_786_600_900
        ));
        let c = crate::quota_cache::load_for(&paths, "codex");
        assert_eq!(c.get("work").map(|e| e.at), Some(1_786_600_000));
    }

    /// A refusal reason is worth showing only if it says something a person can
    /// act on, and the owner/member split is exactly that: one of you can top up
    /// the workspace and the other can only ask.
    #[test]
    fn a_refusal_says_what_happened_and_who_can_clear_it() {
        assert_eq!(refusal_words("rate_limit_reached"), "out of quota");
        assert_eq!(
            refusal_words("workspace_owner_credits_depleted"),
            "credits spent - top them up in the workspace"
        );
        assert_eq!(
            refusal_words("workspace_member_credits_depleted"),
            "workspace credits spent - its owner has to top them up"
        );
        assert_eq!(
            refusal_words("workspace_owner_usage_limit_reached"),
            "spend limit reached - raise it in the workspace"
        );
        assert_eq!(
            refusal_words("workspace_member_usage_limit_reached"),
            "workspace spend limit reached - its owner has to raise it"
        );
        // Case and padding are the server's business, not the reader's.
        assert_eq!(refusal_words("  RATE_LIMIT_REACHED "), "out of quota");
    }

    /// A reason nobody has words for is still the reason. Dropping it leaves an
    /// account refusing with no explanation at all, which is the failure this
    /// whole field exists to prevent.
    #[test]
    fn an_unknown_refusal_is_shown_rather_than_swallowed() {
        assert_eq!(refusal_words("some_new_thing"), "some_new_thing");
    }

    /// Codex writes a placeholder where an account has no workspace. Sending
    /// one as `chatgpt-account-id` gets the request rejected outright, so the
    /// header is left off instead - the token alone still answers.
    #[test]
    fn placeholder_account_ids_are_not_sent_as_a_workspace() {
        assert!(workspace_id("0ed4911f-efae-43fd-a2a7-b5fcbec47e10"));
        assert!(!workspace_id("email_someone@example.com"));
        assert!(!workspace_id("local_abc"));
        assert!(!workspace_id("   "));
        // A value that could break out of the curl config is refused for the
        // same reason the token is.
        assert!(!workspace_id("id\"\nheader = \"X: y"));
    }
}
