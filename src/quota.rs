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

/// Pay-as-you-go usage past the plan's windows, as the endpoint reports it.
///
/// This is why a window at 100% is not the end of an account: with it enabled,
/// Anthropic keeps serving and bills credits. Missing it read a working account
/// as spent - on screen, and worse, in the proxy, which rotated away from one
/// that could serve perfectly well.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Extra {
    pub enabled: bool,
    /// The spend cap has been hit: enabled, but no longer a way through.
    pub limit_reached: bool,
    /// How much of the cap is used, 0..100, when the response says.
    pub used_pct: Option<f64>,
}

/// Parsed usage across the windows Anthropic reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Per-model weekly windows (e.g. Opus), label -> window.
    pub scoped: Vec<(String, Window)>,
    /// Extra usage, when the response describes it. `None` means the response
    /// said nothing - which is not permission to assume there is any.
    pub extra: Option<Extra>,
}

impl Quota {
    /// Can this account still take a turn once its windows are full?
    ///
    /// Only when the response actually says so: extra usage enabled and its
    /// spend cap not yet reached. Silence is not permission.
    pub fn can_serve_past_windows(&self) -> bool {
        self.extra.is_some_and(|x| x.enabled && !x.limit_reached)
    }
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
    /// 429: the usage endpoint itself is rate-limited. Reading several accounts
    /// in a row trips this, and it says nothing about the account's own quota -
    /// so it must not be shown as "no data" next to accounts that answered.
    Throttled,
}

impl Fetch {
    /// Why this read produced no number, in the words the log should use.
    /// `None` when it succeeded.
    ///
    /// Every failure used to be dropped by the same `if let Fetch::Ok` and the
    /// account simply vanished from the usage line - so a throttled endpoint, an
    /// account with no login, and an unreachable network all read as the same
    /// silence. That silence matters: an account with no measurement cannot be
    /// held to the threshold, so the account it hides is exactly the one that
    /// stops rotating before it hits a wall.
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

/// Pull the OAuth access token out of a Claude credentials blob
/// (`{"claudeAiOauth":{"accessToken":...}}`).
pub fn token_from_credentials(bytes: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    v["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(str::to_string)
}

/// Has this saved credential's access token already lapsed? A snapshot holds the
/// token as it was at capture, and refresh tokens rotate, so an old snapshot's
/// token is dead - firing it at the endpoint earns a refusal that looks like the
/// endpoint being busy, which sends the user to wait for something that will
/// never come. `false` when the blob carries no expiry: unknown is not expired.
pub fn credentials_expired(bytes: &[u8], now_ms: i64) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v["claudeAiOauth"]["expiresAt"].as_i64())
        .is_some_and(|exp| exp <= now_ms)
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
    // `utilization` is a PERCENTAGE. It was read as a 0..1 fraction and multiplied
    // by 100, which clamped every account above 1% to "100% used" - so a session
    // 4% in displayed as spent, and a week of recorded readings held only 0.0 and
    // 100.0 with nothing in between. Confirmed against the endpoint: it answers
    // `"utilization": 4.0` beside `"limits":[{"kind":"session","percent":4}]`.
    for k in [
        "utilization",
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
    // Fields that name themselves a fraction still mean one.
    for k in ["used_fraction", "fraction_used"] {
        if let Some(f) = v.get(k).and_then(Value::as_f64) {
            return Some((f * 100.0).clamp(0.0, 100.0));
        }
    }
    None
}

/// Normalize an epoch that might be in milliseconds. Unix SECONDS stay under
/// ~10 digits until the year 2286; anything past 1e11 (year ~5138 in seconds)
/// is really milliseconds, so a drift to ms - or a per-window field that uses
/// ms - shows a sane countdown instead of "resets in 21970092d".
fn normalize_epoch(n: i64) -> i64 {
    if n > 100_000_000_000 {
        n / 1000
    } else {
        n
    }
}

fn reset_secs(v: &Value) -> Option<i64> {
    for k in ["resets_at", "reset_at", "resets", "reset"] {
        match v.get(k) {
            Some(Value::Number(n)) => {
                return n
                    .as_i64()
                    .or_else(|| n.as_f64().map(|f| f as i64))
                    .map(normalize_epoch)
            }
            Some(Value::String(s)) => {
                if let Some(t) = crate::session_link::rfc3339_to_secs(s) {
                    return Some(t);
                }
                if let Ok(n) = s.parse::<i64>() {
                    return Some(normalize_epoch(n));
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
    // Extra usage sits beside the windows, not inside them.
    let extra = v.get("extra_usage").and_then(|x| {
        let enabled = x.get("is_enabled").and_then(Value::as_bool)?;
        Some(Extra {
            enabled,
            limit_reached: x
                .get("spend_limit_reached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            used_pct: x
                .get("utilization")
                .and_then(Value::as_f64)
                .map(|p| p.clamp(0.0, 100.0)),
        })
    });
    if five_hour.is_none() && seven_day.is_none() && scoped.is_empty() {
        return None;
    }
    Some(Quota {
        five_hour,
        seven_day,
        scoped,
        extra,
    })
}

/// Turn an HTTP status + body into a `Fetch`.
pub fn classify(code: u32, body: String) -> Fetch {
    match code {
        401 | 403 => Fetch::Unauthorized,
        429 => Fetch::Throttled,
        200..=299 => match parse(&body) {
            Some(q) => Fetch::Ok(q),
            None => Fetch::Unexpected(code, body),
        },
        0 => Fetch::Offline("no response from api.anthropic.com".into()),
        c => Fetch::Unexpected(c, body),
    }
}

/// Whether a token can be embedded in the curl config at all: quotes,
/// backslashes, and newlines could break out of the quoted config value, so
/// they fail closed. Callers use this to distinguish "this snapshot's token is
/// unusable" (a per-account problem) from "the network is down" (global).
pub fn token_usable(token: &str) -> bool {
    !token.is_empty() && !token.contains(['"', '\n', '\r', '\\'])
}

/// The live, opt-in network call. curl reads its config (including the bearer
/// token) from stdin so the token never appears in argv.
/// Read an account's usage, retrying once when the ENDPOINT (not the account) is
/// rate-limited. Asking for several accounts in a row trips that regularly, and a
/// single short pause is enough to get an answer rather than a blank.
pub fn fetch_with_retry(token: &str) -> Fetch {
    // Back off rather than give up: the endpoint throttles a burst of accounts,
    // and a blank reading is indistinguishable from an account with nothing left.
    for wait_ms in [400u64, 900, 1800] {
        match fetch(token) {
            Fetch::Throttled => std::thread::sleep(std::time::Duration::from_millis(wait_ms)),
            other => return other,
        }
    }
    fetch(token)
}

/// Space out reads of several accounts. The throttling is per burst, so a small
/// gap between accounts is what keeps the LAST account from always losing.
pub fn pace_between_accounts() {
    std::thread::sleep(std::time::Duration::from_millis(PACE_MS));
}

/// How long to wait between accounts. Small enough that a handful of accounts
/// does not add a second of its own, large enough to stay under the burst the
/// endpoint objects to - the backoff inside each read covers the rest.
const PACE_MS: u64 = 120;

/// The same gap, for a caller that staggers concurrent reads instead of
/// sleeping between sequential ones.
pub fn pace_ms() -> u64 {
    PACE_MS
}

/// Read several accounts at once, keeping each result with its caller's index.
///
/// Serially this cost the pacing gap plus a full round trip PER ACCOUNT, which
/// on four accounts was most of six seconds. The requests overlap now, staggered
/// by the same small gap so they do not arrive as one burst, and each still backs
/// off on its own if the endpoint objects.
pub fn fetch_many(tokens: Vec<(usize, String)>) -> Vec<(usize, Fetch)> {
    let mut handles = Vec::with_capacity(tokens.len());
    for (n, (idx, token)) in tokens.into_iter().enumerate() {
        handles.push(std::thread::spawn(move || {
            // Stagger the starts rather than the finishes: arriving together is
            // what the endpoint objects to, not being in flight together.
            std::thread::sleep(std::time::Duration::from_millis(PACE_MS * n as u64));
            (idx, fetch_with_retry(&token))
        }));
    }
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

pub fn fetch(token: &str) -> Fetch {
    if !token_usable(token) {
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

/// The curl binary: the system one when it exists (macOS/most Linux ship
/// /usr/bin/curl), so a PATH-shadowing wrapper never receives the token -
/// the same discipline as the pinned /usr/bin/security. PATH is the fallback
/// for distros that install curl elsewhere. SWAPDEX_CURL is a test-fixture
/// hook, honored ONLY under SWAPDEX_ROOT (test/dev mode) so a production
/// environment can never redirect the token-bearing curl to another binary.
fn curl_bin() -> String {
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        if let Some(t) = std::env::var_os("SWAPDEX_CURL") {
            return t.to_string_lossy().into_owned();
        }
    }
    if std::path::Path::new("/usr/bin/curl").exists() {
        "/usr/bin/curl".into()
    } else {
        "curl".into()
    }
}

/// The same, for callers outside this module: one place owns talking to the
/// network, and it is the place that keeps the token off argv.
pub fn run_curl_cfg(cfg: &str) -> std::result::Result<(String, u32), String> {
    run_curl(cfg)
}

/// Run `curl --config -`, feeding the config on stdin. Returns (body, status).
fn run_curl(cfg: &str) -> std::result::Result<(String, u32), String> {
    use std::io::Write;
    let mut child = std::process::Command::new(curl_bin())
        // `-q` MUST be first: it disables ~/.curlrc, which could otherwise turn
        // on `verbose`/`trace-ascii` and log the Authorization: Bearer header
        // (the account token) to a file. curl reads the default config even
        // with `--config -` unless -q/--disable comes first.
        .arg("-q")
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
    // curl exits non-zero on transport failure (it exits 0 on HTTP error
    // statuses - no --fail here). A partial body from an aborted transfer
    // must not be parsed as a response.
    if !out.status.success() || out.stdout.is_empty() {
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
mod why_tests {
    use super::*;

    /// The distinction that was being thrown away: a throttled endpoint says
    /// nothing about the account, and an account nobody can read is not an
    /// account at 0%. Collapsing them made the serving account vanish from the
    /// usage line with no trace of why.
    #[test]
    fn every_failure_says_what_it_was() {
        assert_eq!(Fetch::Ok(Quota::default()).why_no_number(), None);
        let throttled = Fetch::Throttled.why_no_number().unwrap();
        let unauth = Fetch::Unauthorized.why_no_number().unwrap();
        let offline = Fetch::Offline("dns".into()).why_no_number().unwrap();
        assert!(throttled.contains("throttled"), "{throttled}");
        assert!(
            unauth != throttled,
            "a rejected token is not a busy endpoint"
        );
        assert!(offline != throttled && offline != unauth, "{offline}");
    }
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
    fn reset_in_milliseconds_is_normalized_to_seconds() {
        // A ms epoch (13 digits) must not render "resets in 21970092d".
        let body = r#"{"five_hour":{"utilization":0.5,"resets_at":1900000000000}}"#;
        let w = parse(body).unwrap().five_hour.unwrap();
        assert_eq!(w.resets_at, Some(1_900_000_000), "ms divided to seconds");
        // A real seconds epoch is left untouched.
        let body = r#"{"five_hour":{"utilization":0.5,"resets_at":1900000000}}"#;
        assert_eq!(
            parse(body).unwrap().five_hour.unwrap().resets_at,
            Some(1_900_000_000)
        );
    }

    // These asserted a 0..1 fraction, which was an assumption about the endpoint
    // rather than an observation of it - and it was wrong, so the test held the
    // bug in place. The numbers here are the endpoint's own.
    #[test]
    fn parses_percentage_windows_with_reset() {
        let body = r#"{"five_hour":{"utilization":61.0,"resets_at":1700000000},
                       "seven_day":{"utilization":22.0,"resets_at":1700500000}}"#;
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
        let body = r#"{"seven_day":{"utilization":50.0},
                       "limits":[{"scope":{"model":{"display_name":"Opus"}},"utilization":80.0}]}"#;
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

    // The endpoint reports `utilization` as a PERCENTAGE. Reading it as a 0..1
    // fraction and multiplying by 100 clamped everything above 1% to 100, so every
    // account displayed as spent no matter how little had been used - a week of
    // recorded readings held only 0.0 and 100.0, never a value between.
    #[test]
    fn utilization_is_a_percentage_not_a_fraction() {
        // The real shape, as the endpoint returns it.
        let body = r#"{"five_hour":{"utilization":4.0,"resets_at":"2026-07-31T04:19:59+00:00"},
                       "seven_day":{"utilization":59.0,"resets_at":"2026-08-03T06:59:59+00:00"},
                       "limits":[{"kind":"session","percent":4},{"kind":"weekly_all","percent":59}]}"#;
        let q = parse(body).expect("parsed");
        assert_eq!(q.five_hour.unwrap().used_pct, 4.0, "4% is four percent");
        assert_eq!(q.seven_day.unwrap().used_pct, 59.0);
        assert_eq!(q.five_hour.unwrap().remaining_pct(), 96.0);

        // A genuinely spent window still reads as spent, and an untouched one as
        // zero - the fix must not trade one wrong answer for another.
        let spent = parse(r#"{"five_hour":{"utilization":100.0}}"#).expect("parsed");
        assert_eq!(spent.five_hour.unwrap().used_pct, 100.0);
        let fresh = parse(r#"{"five_hour":{"utilization":0.0}}"#).expect("parsed");
        assert_eq!(fresh.five_hour.unwrap().used_pct, 0.0);
    }

    // A saved snapshot's token dies when the refresh token rotates, and the
    // endpoint answers a dead token the same way it answers a burst - so without
    // this check a rotted profile reads as "busy, try again", forever.
    #[test]
    fn a_lapsed_snapshot_is_recognised_before_it_is_sent() {
        let now = 1_800_000_000_000i64;
        let blob =
            |exp: i64| format!(r#"{{"claudeAiOauth":{{"accessToken":"A","expiresAt":{exp}}}}}"#);
        assert!(credentials_expired(blob(now - 1).as_bytes(), now));
        assert!(!credentials_expired(blob(now + 3_600_000).as_bytes(), now));
        // No expiry recorded, or nothing parseable: unknown is not expired, and
        // refusing to ask would hide an account that may answer perfectly well.
        assert!(!credentials_expired(
            br#"{"claudeAiOauth":{"accessToken":"A"}}"#,
            now
        ));
        assert!(!credentials_expired(b"not json", now));
    }

    // A 429 from the usage endpoint says the ENDPOINT is busy, not that the
    // account has no quota - reading several accounts in a row trips it, and
    // conflating the two blanked out accounts that were perfectly fine.
    #[test]
    fn a_429_is_endpoint_throttling_not_an_account_verdict() {
        assert!(matches!(classify(429, String::new()), Fetch::Throttled));
        assert!(matches!(classify(401, String::new()), Fetch::Unauthorized));
        assert!(matches!(classify(403, String::new()), Fetch::Unauthorized));
    }

    #[test]
    fn fetch_rejects_a_token_with_shell_metacharacters() {
        // A token that could break out of the curl config quoting must never
        // reach curl; it fails closed as "no usable token".
        assert!(matches!(fetch("bad\"token"), Fetch::Offline(_)));
        assert!(matches!(fetch(""), Fetch::Offline(_)));
    }
}

/// The newest version published to crates.io, or `None` when the answer cannot
/// be had (offline, rate-limited, anything).
///
/// Diagnostics only. `swapdex doctor` asks; nothing else does, because a version
/// check that runs on every command is a network call the user did not ask for.
pub fn latest_published() -> Option<String> {
    let url = std::env::var("SWAPDEX_INDEX_URL")
        .unwrap_or_else(|_| "https://index.crates.io/sw/ap/swapdex".to_string());
    // `write-out` is not optional here: run_curl reads the LAST line as the
    // status code, so a config without it reports 0 and every answer is thrown
    // away as a failure.
    let (body, status) = run_curl_cfg(&format!(
        "url = \"{url}\"\nmax-time = 8\nsilent\nwrite-out = \"\\n%{{http_code}}\"\n"
    ))
    .ok()?;
    if status != 200 {
        return None;
    }
    // The sparse index is one JSON object per version, oldest first, with yanked
    // ones marked. The newest usable one is the last that is not yanked.
    body.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| !v["yanked"].as_bool().unwrap_or(false))
        .and_then(|v| v["vers"].as_str().map(str::to_string))
}

/// Is `running` behind `latest`? Compares dotted numbers, so 0.9.0 is not read as
/// newer than 0.35.0 the way a string comparison would have it.
pub fn is_behind(running: &str, latest: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|p| p.parse::<u64>().ok())
            .collect()
    }
    let (a, b) = (parts(running), parts(latest));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn a_lower_version_is_behind_and_an_equal_one_is_not() {
        assert!(is_behind("0.34.1", "0.35.0"));
        assert!(is_behind("0.35.0", "0.35.1"));
        assert!(!is_behind("0.35.0", "0.35.0"));
        assert!(
            !is_behind("0.36.0", "0.35.0"),
            "ahead of the registry is fine"
        );
    }

    /// String comparison would call 0.9.0 newer than 0.35.0, which is exactly the
    /// kind of wrong that tells someone they are up to date when they are not.
    #[test]
    fn numbers_are_compared_as_numbers() {
        assert!(is_behind("0.9.0", "0.35.0"));
        assert!(!is_behind("0.35.0", "0.9.0"));
    }

    #[test]
    fn an_unreadable_version_is_never_reported_as_behind() {
        assert!(!is_behind("", "0.35.0"));
        assert!(!is_behind("0.35.0", "unknown"));
    }
}

#[cfg(test)]
mod extra_usage_tests {
    use super::*;

    /// A spent window does not mean a spent account. With extra usage enabled,
    /// Anthropic keeps serving past the session cap and bills credits - which is
    /// why an account reading "0% left" was answering turns all afternoon. Read
    /// as spent, swapdex marked it so on screen AND rotated the proxy away from a
    /// perfectly usable account.
    const REAL: &str = r#"{
        "five_hour": {"utilization": 100.0, "resets_at": 1785900000},
        "seven_day": {"utilization": 55.0, "resets_at": 1786300000},
        "extra_usage": {"is_enabled": true, "used_credits": 1121.0,
                        "monthly_limit": 50000, "utilization": 2.242,
                        "spend_limit_reached": false}
    }"#;

    #[test]
    fn a_capped_window_with_credits_left_can_still_serve() {
        let q = parse(REAL).expect("parsed");
        let x = q.extra.expect("extra usage read");
        assert!(x.enabled);
        assert!(!x.limit_reached);
        assert!(
            q.can_serve_past_windows(),
            "credits are available, so the account is not out"
        );
    }

    #[test]
    fn without_extra_usage_a_capped_window_really_is_the_end() {
        let body = r#"{"five_hour": {"utilization": 100.0},
                       "extra_usage": {"is_enabled": false}}"#;
        let q = parse(body).expect("parsed");
        assert!(!q.can_serve_past_windows());
    }

    /// Enabled but already at the spend cap is the same as not having it.
    #[test]
    fn a_reached_spend_limit_is_not_a_way_through() {
        let body = r#"{"five_hour": {"utilization": 100.0},
                       "extra_usage": {"is_enabled": true, "spend_limit_reached": true}}"#;
        let q = parse(body).expect("parsed");
        assert!(!q.can_serve_past_windows());
    }

    /// A response that says nothing about extra usage claims nothing about it.
    #[test]
    fn silence_is_not_permission() {
        let q = parse(r#"{"five_hour": {"utilization": 100.0}}"#).expect("parsed");
        assert!(q.extra.is_none());
        assert!(!q.can_serve_past_windows());
    }
}
