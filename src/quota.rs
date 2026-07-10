//! `swapdex quota` - the ONE network path in swapdex.
//!
//! Reads each Claude account's REMAINING quota from Anthropic's official OAuth
//! usage endpoint, using that account's own access token. The call is:
//! read-only, spends zero message quota (documented by Anthropic), and runs
//! ONLY when the user invokes `quota`. Every other swapdex command is 100%
//! local - this file is the single, opt-in exception, kept isolated on purpose.
//!
//! The request shells out to `curl` with its config on STDIN (never argv), so
//! the token stays off `ps` - the same discipline the Keychain writer uses -
//! and swapdex's dependency graph keeps no HTTP client (still CI-asserted).
//!
//! No token refresh, no proxying, no client impersonation: swapdex sends an
//! honest `User-Agent: swapdex` and only ever READS. An account whose saved
//! access token has expired simply reports "expired" - switch to it (which lets
//! the official CLI refresh) to see its numbers. That is the deliberate line
//! between this and a rotator/proxy like teamclaude or claude-swap.

use serde_json::Value;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// One rate-limit window (5h or 7d): how much is used and when it resets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// Fraction used, normalized to 0..100.
    pub used_pct: f64,
    /// When the window resets, unix seconds (None if the response omits it).
    pub resets_at: Option<i64>,
}

impl Window {
    pub fn remaining_pct(&self) -> f64 {
        (100.0 - self.used_pct).clamp(0.0, 100.0)
    }
}

/// Parsed usage across the windows Anthropic reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Per-model weekly windows (e.g. Opus), label -> window.
    pub scoped: Vec<(String, Window)>,
}

/// The outcome of one account's quota fetch.
#[derive(Debug)]
pub enum Fetch {
    Ok(Quota),
    /// 401/403: the account's access token is expired/rejected.
    Unauthorized,
    /// A 2xx (or other) response whose body we could not map to a Quota;
    /// carries the status and raw body so `--json` can surface ground truth.
    Unexpected(u32, String),
    /// curl could not run or the network was unreachable.
    Offline(String),
}

/// Pull the OAuth access token out of a Claude credentials blob
/// (`{"claudeAiOauth":{"accessToken":...}}`).
pub fn token_from_credentials(bytes: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    v["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(str::to_string)
}

/// Interpret one window object leniently: the exact field names of the usage
/// endpoint are treated as best-effort (fraction fields first, then percentage
/// fields), so a small schema drift degrades to "unexpected" rather than lying.
fn window_from(v: &Value) -> Option<Window> {
    Some(Window {
        used_pct: pct_used(v)?,
        resets_at: reset_secs(v),
    })
}

fn pct_used(v: &Value) -> Option<f64> {
    // A 0..1 fraction (utilization).
    for k in ["utilization", "used_fraction", "fraction_used"] {
        if let Some(f) = v.get(k).and_then(Value::as_f64) {
            return Some((f * 100.0).clamp(0.0, 100.0));
        }
    }
    // An explicit 0..100 percentage.
    for k in [
        "used_percentage",
        "used_pct",
        "utilization_percentage",
        "percent_used",
        "percent",
    ] {
        if let Some(f) = v.get(k).and_then(Value::as_f64) {
            return Some(f.clamp(0.0, 100.0));
        }
    }
    None
}

fn reset_secs(v: &Value) -> Option<i64> {
    for k in ["resets_at", "reset_at", "resets", "reset"] {
        match v.get(k) {
            Some(Value::Number(n)) => return n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Some(Value::String(s)) => {
                if let Some(t) = crate::session_link::rfc3339_to_secs(s) {
                    return Some(t);
                }
                if let Ok(n) = s.parse::<i64>() {
                    return Some(n);
                }
            }
            _ => {}
        }
    }
    None
}

/// Map a usage-endpoint JSON body to a `Quota`. `None` when nothing recognizable
/// is present (the caller then treats it as an unexpected shape).
pub fn parse(body: &str) -> Option<Quota> {
    let v: Value = serde_json::from_str(body).ok()?;
    let five_hour = v.get("five_hour").and_then(window_from);
    let seven_day = v.get("seven_day").and_then(window_from);
    let mut scoped = Vec::new();
    // Named per-model weekly windows, if the endpoint splits them out.
    for (k, label) in [
        ("seven_day_opus", "opus 7d"),
        ("seven_day_sonnet", "sonnet 7d"),
        ("seven_day_oi", "opus 7d"),
    ] {
        if let Some(w) = v.get(k).and_then(window_from) {
            if !scoped.iter().any(|(n, _): &(String, Window)| n == label) {
                scoped.push((label.to_string(), w));
            }
        }
    }
    // A generic limits[] array of scoped weekly entries.
    if let Some(limits) = v.get("limits").and_then(Value::as_array) {
        for l in limits {
            let name = l
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str)
                .or_else(|| l.get("name").and_then(Value::as_str));
            if let (Some(name), Some(w)) = (name, window_from(l)) {
                if !scoped.iter().any(|(n, _)| n == name) {
                    scoped.push((name.to_string(), w));
                }
            }
        }
    }
    if five_hour.is_none() && seven_day.is_none() && scoped.is_empty() {
        return None;
    }
    Some(Quota {
        five_hour,
        seven_day,
        scoped,
    })
}

/// Turn an HTTP status + body into a `Fetch`.
pub fn classify(code: u32, body: String) -> Fetch {
    match code {
        401 | 403 => Fetch::Unauthorized,
        200..=299 => match parse(&body) {
            Some(q) => Fetch::Ok(q),
            None => Fetch::Unexpected(code, body),
        },
        0 => Fetch::Offline("no response from api.anthropic.com".into()),
        c => Fetch::Unexpected(c, body),
    }
}

/// The live, opt-in network call. curl reads its config (including the bearer
/// token) from stdin so the token never appears in argv.
pub fn fetch(token: &str) -> Fetch {
    if token.is_empty() || token.contains(['"', '\n', '\r', '\\']) {
        return Fetch::Offline("no usable access token for this account".into());
    }
    let cfg = format!(
        "url = \"{USAGE_URL}\"\n\
         header = \"Authorization: Bearer {token}\"\n\
         header = \"anthropic-beta: {OAUTH_BETA}\"\n\
         header = \"Accept: application/json\"\n\
         header = \"User-Agent: swapdex\"\n\
         silent\n\
         show-error\n\
         connect-timeout = 6\n\
         max-time = 15\n\
         write-out = \"\\n%{{http_code}}\"\n"
    );
    match run_curl(&cfg) {
        Ok((body, code)) => classify(code, body),
        Err(e) => Fetch::Offline(e),
    }
}

/// Run `curl --config -`, feeding the config on stdin. Returns (body, status).
fn run_curl(cfg: &str) -> std::result::Result<(String, u32), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("curl")
        .arg("--config")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("run curl: {e} (curl is required only for `swapdex quota`)"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "curl stdin unavailable".to_string())?
        .write_all(cfg.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    // curl exits non-zero on transport failure; with no body that is offline.
    if out.stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.trim();
        return Err(if msg.is_empty() {
            "no response from api.anthropic.com".to_string()
        } else {
            msg.to_string()
        });
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // The last line is the http_code emitted by write-out; everything before
    // it is the response body.
    let (body, code) = match text.rfind('\n') {
        Some(i) => (
            text[..i].to_string(),
            text[i + 1..].trim().parse::<u32>().unwrap_or(0),
        ),
        None => (String::new(), text.trim().parse::<u32>().unwrap_or(0)),
    };
    Ok((body, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_extracted_from_credentials() {
        let cred = br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-XYZ","refreshToken":"r"}}"#;
        assert_eq!(
            token_from_credentials(cred).as_deref(),
            Some("sk-ant-oat01-XYZ")
        );
        assert_eq!(token_from_credentials(b"{}"), None);
        assert_eq!(token_from_credentials(b"not json"), None);
    }

    #[test]
    fn parses_fraction_windows_with_reset() {
        // utilization as a 0..1 fraction, reset as unix seconds.
        let body = r#"{"five_hour":{"utilization":0.61,"resets_at":1700000000},
                       "seven_day":{"utilization":0.22,"resets_at":1700500000}}"#;
        let q = parse(body).unwrap();
        let f = q.five_hour.unwrap();
        assert!((f.used_pct - 61.0).abs() < 1e-6);
        assert!((f.remaining_pct() - 39.0).abs() < 1e-6);
        assert_eq!(f.resets_at, Some(1_700_000_000));
        assert!((q.seven_day.unwrap().remaining_pct() - 78.0).abs() < 1e-6);
    }

    #[test]
    fn parses_percentage_fields_and_rfc3339_reset() {
        // An alternate shape: an explicit percentage and an RFC3339 reset.
        let body = r#"{"five_hour":{"used_percentage":90,"resets_at":"2026-07-10T12:00:00Z"}}"#;
        let q = parse(body).unwrap();
        let f = q.five_hour.unwrap();
        assert!((f.used_pct - 90.0).abs() < 1e-6);
        assert_eq!(
            f.resets_at,
            crate::session_link::rfc3339_to_secs("2026-07-10T12:00:00Z")
        );
    }

    #[test]
    fn parses_scoped_weekly_limits_array() {
        let body = r#"{"seven_day":{"utilization":0.5},
                       "limits":[{"scope":{"model":{"display_name":"Opus"}},"utilization":0.8}]}"#;
        let q = parse(body).unwrap();
        assert_eq!(q.scoped.len(), 1);
        assert_eq!(q.scoped[0].0, "Opus");
        assert!((q.scoped[0].1.used_pct - 80.0).abs() < 1e-6);
    }

    #[test]
    fn unrecognized_shape_is_none() {
        assert!(parse(r#"{"something":"else"}"#).is_none());
        assert!(parse("not json").is_none());
    }

    #[test]
    fn classify_maps_status_codes() {
        assert!(matches!(classify(401, String::new()), Fetch::Unauthorized));
        assert!(matches!(classify(403, String::new()), Fetch::Unauthorized));
        assert!(matches!(
            classify(200, r#"{"five_hour":{"utilization":0.1}}"#.into()),
            Fetch::Ok(_)
        ));
        assert!(matches!(
            classify(200, "{}".into()),
            Fetch::Unexpected(200, _)
        ));
        assert!(matches!(
            classify(500, "oops".into()),
            Fetch::Unexpected(500, _)
        ));
        assert!(matches!(classify(0, String::new()), Fetch::Offline(_)));
    }

    #[test]
    fn fetch_rejects_a_token_with_shell_metacharacters() {
        // A token that could break out of the curl config quoting must never
        // reach curl; it fails closed as "no usable token".
        assert!(matches!(fetch("bad\"token"), Fetch::Offline(_)));
        assert!(matches!(fetch(""), Fetch::Offline(_)));
    }
}
