//! Renewing a slot's access token.
//!
//! An access token lives about an hour, and only the account's own refresh token
//! can renew it. Until now swapdex would not do that - it read credentials and
//! never wrote them - so any account idle for an hour looked expired: the proxy
//! stepped over it and `quota` reported it dead. Those are exactly the accounts
//! with quota left, which made the tool least useful precisely when it was needed.
//!
//! Refreshing means writing a credential, and that is a line worth naming. Two
//! properties make it safe to cross:
//!
//! 1. **Never while the tool is running there.** A refresh token ROTATES: the
//!    server issues a new one and retires the old. A Claude running in that slot
//!    holds the old one in memory, and when it later refreshes with a token that
//!    has already been spent, the server can revoke the whole chain - which is
//!    the logout this project exists to prevent. So a slot in use is never
//!    touched.
//! 2. **The new credential replaces the old one in place**, in whichever store
//!    the account already keeps it, so the tool's next run reads what swapdex
//!    wrote rather than a second, competing copy.
//!
//! No token value is ever logged, and the request carries it on curl's stdin -
//! the same discipline `quota` uses, so it never reaches `ps`.

use crate::secret::Secret;
use std::path::Path;

/// Where an OAuth refresh is exchanged. `SWAPDEX_OAUTH_URL` redirects it for
/// tests, honored ONLY under `SWAPDEX_ROOT` so a production run can never be
/// pointed at another host with a live refresh token.
pub fn token_url() -> String {
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        if let Some(u) = std::env::var_os("SWAPDEX_OAUTH_URL") {
            return u.to_string_lossy().into_owned();
        }
    }
    "https://console.anthropic.com/v1/oauth/token".to_string()
}

/// Claude Code's public OAuth client, as its own authorize URL carries it.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Why a refresh did not happen. Each is a different thing to tell the user, and
/// none of them should read as "your account is gone".
#[derive(Debug, PartialEq)]
pub enum RefreshError {
    /// The slot has no readable credential to renew.
    NoCredential,
    /// The tool is running in this slot right now. Refreshing would retire the
    /// refresh token it holds in memory, and its next renewal would fail.
    InUse,
    /// The refresh token itself has expired - only a fresh sign-in fixes that.
    Expired,
    /// The login server is rate-limiting; the account itself is fine.
    Busy,
    /// Another caller in this process is already renewing this slot. Spending
    /// the same refresh token twice is what logs an account out, so the second
    /// caller stands down and uses the credential the first is about to write.
    AlreadyRefreshing,
    /// The server refused the exchange.
    Refused(String),
    /// The request could not be made at all.
    Offline(String),
}

impl RefreshError {
    /// What the user should do about it, in one line.
    pub fn remedy(&self, name: &str) -> String {
        match self {
            Self::NoCredential => {
                format!("'{name}' has no login yet - `swapdex run {name}` signs it in")
            }
            Self::InUse => format!(
                "'{name}' is in use right now - its own session will renew it; \
                 renewing from here would retire the token that session is holding"
            ),
            Self::Expired => format!(
                "'{name}' has been idle too long to renew - `swapdex run {name}` signs it in again"
            ),
            Self::Busy => format!(
                "the login server is busy - '{name}' is fine, renewing again shortly will work"
            ),
            Self::AlreadyRefreshing => format!(
                "'{name}' is already being renewed by another turn - nothing to do; \
                 spending its refresh token twice is what logs an account out"
            ),
            Self::Refused(why) => format!("'{name}' could not be renewed: {why}"),
            Self::Offline(why) => format!("could not reach the login server: {why}"),
        }
    }
}

/// Has the refresh token itself expired? Then nothing here can help and only a
/// sign-in will - saying so is the difference between a fixable state and a
/// mysterious one.
fn refresh_token_expired(blob: &[u8], now_ms: i64) -> bool {
    serde_json::from_slice::<serde_json::Value>(blob)
        .ok()
        .and_then(|v| v["claudeAiOauth"]["refreshTokenExpiresAt"].as_i64())
        .is_some_and(|exp| exp <= now_ms)
}

/// Build the renewed credential blob by merging the server's answer into the one
/// on disk, so fields swapdex does not understand survive untouched.
///
/// The new refresh token REPLACES the old one when the server sends it. Keeping
/// the old one would leave a spent token on disk and the account unusable on the
/// renewal after this one.
pub fn merge_response(old: &[u8], response: &str, now_ms: i64) -> Option<Vec<u8>> {
    let mut blob: serde_json::Value = serde_json::from_slice(old).ok()?;
    let r: serde_json::Value = serde_json::from_str(response).ok()?;
    let access = r["access_token"].as_str().filter(|s| !s.is_empty())?;
    let o = blob.get_mut("claudeAiOauth")?.as_object_mut()?;
    o.insert("accessToken".into(), access.into());
    if let Some(rt) = r["refresh_token"].as_str().filter(|s| !s.is_empty()) {
        o.insert("refreshToken".into(), rt.into());
        // The deadline on disk belonged to the token this one replaces. The
        // server retired that one, so the recorded moment now describes
        // something that does not exist - and it is exactly what decides
        // whether renewing is worth trying. Left in place it made every
        // renewal leave the account one day nearer a sign-in swapdex would
        // demand while holding a refresh token the server had just issued.
        //
        // Record the new lifetime when the server states it. When it does not,
        // drop the stale one rather than apply it to a different token: the
        // server is then the one that says no, which is the only party that
        // knows.
        match r["refresh_token_expires_in"].as_i64() {
            Some(secs) => {
                o.insert(
                    "refreshTokenExpiresAt".into(),
                    (now_ms + secs * 1000).into(),
                );
            }
            None => {
                o.remove("refreshTokenExpiresAt");
            }
        }
    }
    // `expires_in` is seconds from now; the file records an absolute moment.
    if let Some(secs) = r["expires_in"].as_i64() {
        o.insert("expiresAt".into(), (now_ms + secs * 1000).into());
    }
    serde_json::to_vec(&blob).ok()
}

/// The request body for an exchange. Kept separate so a test can assert its
/// shape without a network call.
pub fn request_body(refresh_token: &str) -> String {
    serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    })
    .to_string()
}

/// Renew this slot's credential in place. `Ok` carries nothing: the point is the
/// side effect, and returning the token would invite logging it.
/// How close together two refreshes count as the same burst.
pub const BURST_SECS: i64 = 30;

/// Lets ONE refresh through per slot per burst.
///
/// Refresh tokens rotate: each use mints a new one and retires the old. So N
/// concurrent turns each refreshing the same slot spend the same token N times,
/// and every result but one is already invalid when it lands - the account ends
/// up logged out by its own renewal. teamclaude hit this as "don't rotate the
/// token family once per 401 in a burst".
///
/// Per-slot, so one account's burst never blocks another's genuine refresh, and
/// time-bounded, so a refresh minutes later is a new event rather than the same
/// burst still being suppressed.
#[derive(Default)]
pub struct RefreshGate {
    last: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, i64>>,
}

impl RefreshGate {
    /// True when this caller should go ahead; false when another already is.
    pub fn claim(&self, dir: &Path, now_secs: i64) -> bool {
        let mut m = self.last.lock().unwrap_or_else(|e| e.into_inner());
        match m.get(dir) {
            Some(&t) if now_secs - t <= BURST_SECS => false,
            _ => {
                m.insert(dir.to_path_buf(), now_secs);
                true
            }
        }
    }
}

/// The one gate every refresh passes through.
///
/// `RefreshGate` was created for this and then applied at a single call site,
/// while two other paths - the keep-alive sweep and `has_usable_login` - reached
/// `refresh_slot` unguarded. A rule enforced at one caller is a rule the next
/// caller does not know exists, so it lives here now, where the token is
/// actually spent.
fn gate() -> &'static RefreshGate {
    static GATE: std::sync::OnceLock<RefreshGate> = std::sync::OnceLock::new();
    GATE.get_or_init(RefreshGate::default)
}

/// The account a slot holds, however its tool records it.
/// The provider is part of the identity, not just the id under it.
///
/// A Claude `accountUuid` and a ChatGPT `account_id` are drawn from different
/// namespaces, and one person's email can hold a subscription to both. Keying
/// on the bare id would let two unrelated accounts share one claim - the same
/// correction KarpelesLab/teamclaude made in its own pool (#349).
fn account_of(dir: &Path) -> Option<String> {
    if let Some(u) = crate::proxy::creds::slot_account_uuid(dir) {
        return Some(format!("claude:{u}"));
    }
    let bytes = std::fs::read(dir.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|id| format!("codex:{id}"))
}

/// The key a claim is held under: the ACCOUNT, not the directory.
///
/// Two directories can hold one login - `doctor` reports exactly that - and
/// these tokens are single-use, so one claim per directory let a sweep spend
/// one account's token once per copy. That is the double-spend this gate
/// exists to stop, reached from the other side: the gate was made per-caller
/// once already, and the note above records why that failed.
///
/// An identity that cannot be read falls back to the path. Two unknowns are
/// not one account, and per-path is the stricter answer anyway.
fn claim_key(dir: &Path) -> std::path::PathBuf {
    match account_of(dir) {
        Some(a) => std::path::PathBuf::from(format!("account:{a}")),
        None => dir.to_path_buf(),
    }
}

/// Claim the right to renew this account now. False when another caller has it.
fn claim_refresh_at(dir: &Path, now_secs: i64) -> bool {
    gate().claim(&claim_key(dir), now_secs)
}

pub fn refresh_slot(dir: &Path, now_ms: i64) -> Result<(), RefreshError> {
    // A slot the tool is using is never touched - see the module note.
    if slot_in_use(dir, "claude-code") {
        return Err(RefreshError::InUse);
    }
    // Claimed HERE, not at a caller: every path that spends this token passes
    // through this line, and spending it twice is what logs an account out.
    if !claim_refresh_at(dir, now_ms / 1000) {
        return Err(RefreshError::AlreadyRefreshing);
    }
    let blob = read_credential(dir).ok_or(RefreshError::NoCredential)?;
    if refresh_token_expired(blob.expose(), now_ms) {
        return Err(RefreshError::Expired);
    }
    let token = serde_json::from_slice::<serde_json::Value>(blob.expose())
        .ok()
        .and_then(|v| {
            v["claudeAiOauth"]["refreshToken"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .ok_or(RefreshError::NoCredential)?;

    let (body, status) = post(&token)?;
    // 429 is the login server asking for quiet, not a verdict on this account.
    // Reporting it as a refusal reads as "sign in again" - a login nobody needed.
    if status == 429 {
        return Err(RefreshError::Busy);
    }
    if status == 401 || status == 400 {
        return Err(RefreshError::Refused(short_reason(&body)));
    }
    if !(200..300).contains(&status) {
        return Err(RefreshError::Refused(format!("HTTP {status}")));
    }
    let merged = merge_response(blob.expose(), &body, now_ms)
        .ok_or_else(|| RefreshError::Refused("the server's answer had no access token".into()))?;
    write_credential(dir, &merged).map_err(|e| RefreshError::Refused(e.to_string()))
}

/// One short clause from an error body, for a message a person reads. Never the
/// whole body: it can be long, and it is not the user's problem to parse.
fn short_reason(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error_description"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["error"]["message"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "the login server refused it".into())
}

/// Is a tool currently running with this slot as its home? Checked by the
/// environment of the running processes, since that is what actually decides
/// which credential a process holds.
fn slot_in_use(dir: &Path, tool: &str) -> bool {
    crate::proc::config_dir_in_use(dir, tool)
}

/// The credential blob wherever this slot keeps it.
fn read_credential(dir: &Path) -> Option<Secret> {
    if let Ok(bytes) = std::fs::read(dir.join(".credentials.json")) {
        if !bytes.is_empty() {
            return Some(Secret::new(bytes));
        }
    }
    crate::adapters::claude::slot_keychain_read_detail(dir)
        .ok()
        .map(Secret::new)
}

/// Put the renewed blob back where the old one was, so the tool's next run reads
/// what was written rather than a second, competing copy.
fn write_credential(dir: &Path, blob: &[u8]) -> anyhow::Result<()> {
    let file = dir.join(".credentials.json");
    if file.exists() {
        return crate::atomic::write_secret(&file, blob);
    }
    crate::adapters::claude::slot_keychain_write(dir, blob)
}

/// POST the exchange with the token on stdin, never in argv.
fn post(refresh_token: &str) -> Result<(String, u32), RefreshError> {
    let body = request_body(refresh_token);
    // The same config shape `quota` uses, including how the status is reported:
    // run_curl reads the LAST line as the code, so decorating it broke the parse
    // and every renewal came back as HTTP 0.
    let cfg = format!(
        "url = \"{}\"\n\
         request = POST\n\
         header = \"content-type: application/json\"\n\
         header = \"Accept: application/json\"\n\
         header = \"User-Agent: swapdex\"\n\
         data = \"{}\"\n\
         silent\n\
         show-error\n\
         connect-timeout = 6\n\
         max-time = 15\n\
         write-out = \"\\n%{{http_code}}\"\n",
        token_url(),
        body.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let out = crate::quota::run_curl_cfg(&cfg).map_err(RefreshError::Offline)?;
    Ok(out)
}

/// Codex's public OAuth client id, and where its exchange happens.
///
/// Not guessed: `icoretech/codex-pooler`, an Elixir gateway that keeps a pool of
/// Codex accounts alive, carries both as constants, and the issuer matches the
/// `iss` claim on every access token in a real slot here
/// (`https://auth.openai.com`).
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Where a Codex refresh is exchanged. Redirected only under `SWAPDEX_ROOT`,
/// the same rule the Claude URL follows, so a production run can never be
/// pointed at another host holding a live refresh token.
pub fn codex_token_url() -> String {
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        if let Some(u) = std::env::var_os("SWAPDEX_CODEX_OAUTH_URL") {
            return u.to_string_lossy().into_owned();
        }
    }
    "https://auth.openai.com/oauth/token".to_string()
}

/// The form body for a Codex exchange. Form-encoded, not JSON: that is what
/// RFC 6749 specifies and what the working implementation sends.
///
/// Probed against the real endpoint with a deliberately invalid token, which
/// spends nothing: it answers a clean JSON 401 to a bare `User-Agent: swapdex`,
/// so the URL, the form shape and the client id are all accepted and no
/// browser fingerprint is needed.
pub fn codex_request_body(refresh_token: &str) -> String {
    // A JWT is unreserved characters only, but encode anyway rather than depend
    // on the shape of someone else's token.
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        enc(refresh_token),
        enc(CODEX_CLIENT_ID)
    )
}

/// Fold a Codex exchange's answer back into the slot's `auth.json`, keeping
/// every field it already had.
///
/// The rotated refresh token is written when the server sends one. Dropping it
/// would leave the slot holding a token the server has already retired - the
/// logout this project exists to prevent, arrived at from the other direction.
pub fn merge_codex_response(old: &[u8], response: &str, now_rfc3339: &str) -> Option<Vec<u8>> {
    let mut blob: serde_json::Value = serde_json::from_slice(old).ok()?;
    let r: serde_json::Value = serde_json::from_str(response).ok()?;
    let access = r["access_token"].as_str().filter(|s| !s.is_empty())?;
    let t = blob.get_mut("tokens")?.as_object_mut()?;
    t.insert("access_token".into(), access.into());
    for (from, to) in [("refresh_token", "refresh_token"), ("id_token", "id_token")] {
        if let Some(v) = r[from].as_str().filter(|s| !s.is_empty()) {
            t.insert(to.into(), v.into());
        }
    }
    blob.as_object_mut()?
        .insert("last_refresh".into(), now_rfc3339.into());
    serde_json::to_vec_pretty(&blob).ok()
}

/// `last_refresh` the way Codex writes it: RFC 3339, UTC. Only this field needs
/// it, so the civil-date arithmetic lives here rather than pulling in a clock
/// crate the rest of the tool does not use.
pub fn rfc3339_utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // days-from-civil, inverted (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Renew a Codex slot's access token in place.
///
/// The guards are the Claude path's, for the same reasons: never while Codex is
/// running in that slot (its session holds the refresh token, and retiring it
/// would break the session's own next renewal), and the claim is taken here -
/// where the token is SPENT - so two callers cannot spend it twice.
pub fn refresh_codex_slot(dir: &Path, now_ms: i64) -> Result<(), RefreshError> {
    if slot_in_use(dir, "codex") {
        return Err(RefreshError::InUse);
    }
    if !claim_refresh_at(dir, now_ms / 1000) {
        return Err(RefreshError::AlreadyRefreshing);
    }
    let path = dir.join("auth.json");
    let blob = std::fs::read(&path)
        .ok()
        .filter(|b| !b.is_empty())
        .map(Secret::new)
        .ok_or(RefreshError::NoCredential)?;
    let token = serde_json::from_slice::<serde_json::Value>(blob.expose())
        .ok()
        .and_then(|v| {
            v["tokens"]["refresh_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .ok_or(RefreshError::NoCredential)?;

    let (body, status) = post_codex(&token)?;
    if status == 429 {
        return Err(RefreshError::Busy);
    }
    if matches!(status, 400 | 401 | 403) {
        return Err(RefreshError::Expired);
    }
    if !(200..300).contains(&status) {
        return Err(RefreshError::Refused(format!("HTTP {status}")));
    }
    let merged = merge_codex_response(blob.expose(), &body, &rfc3339_utc(now_ms / 1000))
        .ok_or_else(|| RefreshError::Refused("the server's answer had no access token".into()))?;
    crate::atomic::write_secret(&path, &merged)
        .map_err(|e| RefreshError::Refused(e.to_string()))?;
    Ok(())
}

fn post_codex(refresh_token: &str) -> Result<(String, u32), RefreshError> {
    let body = codex_request_body(refresh_token);
    let cfg = format!(
        "url = \"{}\"\n\
         request = POST\n\
         header = \"content-type: application/x-www-form-urlencoded\"\n\
         header = \"Accept: application/json\"\n\
         header = \"User-Agent: swapdex\"\n\
         data = \"{}\"\n\
         silent\n\
         show-error\n\
         connect-timeout = 6\n\
         max-time = 15\n\
         write-out = \"\\n%{{http_code}}\"\n",
        codex_token_url(),
        body.replace('\\', "\\\\").replace('"', "\\\"")
    );
    crate::quota::run_curl_cfg(&cfg).map_err(RefreshError::Offline)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &str = r#"{"claudeAiOauth":{"accessToken":"OLD-AT","refreshToken":"OLD-RT",
        "expiresAt":1000,"refreshTokenExpiresAt":9999999999999,"subscriptionType":"max"},
        "mcpOAuth":{"keep":"me"}}"#;

    #[test]
    fn a_renewal_replaces_the_refresh_token_the_server_retired() {
        let now = 1_800_000_000_000i64;
        let resp = r#"{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}"#;
        let merged = merge_response(BLOB.as_bytes(), resp, now).expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&merged).unwrap();
        let o = &v["claudeAiOauth"];
        assert_eq!(o["accessToken"], "NEW-AT");
        assert_eq!(
            o["refreshToken"], "NEW-RT",
            "keeping the old one would leave a spent token on disk"
        );
        assert_eq!(o["expiresAt"], now + 3_600_000, "an absolute moment");
        // Everything swapdex does not understand survives.
        assert_eq!(o["subscriptionType"], "max");
        assert_eq!(v["mcpOAuth"]["keep"], "me");
    }

    // Some servers renew the access token without issuing a new refresh token.
    // Dropping the old one then would leave the account unable to renew again.
    #[test]
    fn a_response_without_a_new_refresh_token_keeps_the_old_one() {
        let resp = r#"{"access_token":"NEW-AT","expires_in":3600}"#;
        let merged = merge_response(BLOB.as_bytes(), resp, 0).expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "OLD-RT");
    }

    #[test]
    fn an_answer_with_no_access_token_is_not_merged() {
        assert!(merge_response(BLOB.as_bytes(), r#"{"error":"invalid_grant"}"#, 0).is_none());
        assert!(merge_response(BLOB.as_bytes(), "not json", 0).is_none());
        assert!(merge_response(b"not json", r#"{"access_token":"X"}"#, 0).is_none());
    }

    #[test]
    fn an_expired_refresh_token_is_named_rather_than_retried() {
        let now = 1_800_000_000_000i64;
        let blob = format!(
            r#"{{"claudeAiOauth":{{"refreshToken":"R","refreshTokenExpiresAt":{}}}}}"#,
            now - 1
        );
        assert!(refresh_token_expired(blob.as_bytes(), now));
        let msg = RefreshError::Expired.remedy("work");
        assert!(msg.contains("swapdex run work"), "names the way out: {msg}");
        // Unknown is not expired.
        assert!(!refresh_token_expired(br#"{"claudeAiOauth":{}}"#, now));
    }

    #[test]
    fn the_request_names_the_grant_and_the_client() {
        let b = request_body("RT-1");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["grant_type"], "refresh_token");
        assert_eq!(v["refresh_token"], "RT-1");
        assert_eq!(v["client_id"], CLIENT_ID);
    }

    // A refusal has to read as something the user can act on, never as data loss.
    #[test]
    fn a_refusal_is_reported_in_the_users_terms() {
        assert_eq!(
            short_reason(r#"{"error":"invalid_grant","error_description":"token revoked"}"#),
            "token revoked"
        );
        assert_eq!(
            short_reason(r#"{"error":{"type":"rate_limit_error","message":"Rate limited."}}"#),
            "Rate limited."
        );
        assert_eq!(
            short_reason("<html>502</html>"),
            "the login server refused it"
        );
        // Rate limiting is a wait, not a verdict: telling someone to sign in
        // again over it would cost them a login they did not need.
        let busy = RefreshError::Busy.remedy("work");
        assert!(busy.contains("is fine"), "{busy}");
        assert!(!busy.contains("swapdex run"), "not a sign-in: {busy}");
        let msg = RefreshError::InUse.remedy("work");
        assert!(
            msg.contains("its own session will renew it"),
            "an in-use slot is fine, not broken: {msg}"
        );
    }
}

/// How long before an access token lapses a keep-alive sweep renews it.
///
/// Deliberately wide. "Is this token unusable right NOW" is a different
/// question, asked when a turn is waiting, and answered on the serving path by
/// `slot_token_expired` and `has_usable_login`. This one answers "will this
/// account still work tomorrow", and the sweep runs whether or not anybody is
/// using the account.
pub const KEEP_ALIVE_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;

/// Should a keep-alive sweep renew this credential now?
///
/// An OAuth refresh token is not a key that sits still - it is exercised, and it
/// rotates. Leave an account idle long enough and its refresh token goes stale,
/// and then only a browser sign-in brings it back. Three of this machine's
/// accounts died exactly that way and stayed dead for a week.
///
/// So the sweep renews ahead of expiry rather than at it. Nothing to renew, or a
/// refresh token already gone, is not this function's problem: renewing needs a
/// refresh token, and a dead one needs a human.
pub fn wants_keep_alive(blob: &[u8], now_ms: i64) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(blob) else {
        return false;
    };
    let oauth = &v["claudeAiOauth"];
    if oauth["refreshToken"].as_str().is_none_or(str::is_empty) {
        return false;
    }
    if refresh_token_expired(blob, now_ms) {
        return false;
    }
    oauth["expiresAt"]
        .as_i64()
        .is_some_and(|exp| exp - now_ms <= KEEP_ALIVE_WINDOW_MS)
}

/// How long before a Codex access token lapses the sweep renews it.
///
/// Claude's token lives an hour, so any daily use renews it and the sweep is a
/// safety net. Codex's lives ten days and ONLY a Codex run renews it, so for an
/// account nobody has opened the sweep is the only thing standing between it and
/// a re-login. Two days is late enough that a daily sweep renews about once a
/// week rather than every pass - each renewal rotates the refresh token, and
/// rotating one more often than needed is its own risk.
pub const KEEP_ALIVE_CODEX_WINDOW_SECS: i64 = 2 * 24 * 60 * 60;

/// Should a keep-alive sweep renew this Codex credential now?
///
/// The deadline is in the access token's own `exp` claim. A credential with no
/// refresh token cannot be renewed from here, and a deadline that could not be
/// read is not an expired one - neither is grounds for spending a refresh token.
pub fn wants_keep_alive_codex(blob: &[u8], now_secs: i64) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(blob) else {
        return false;
    };
    if v["tokens"]["refresh_token"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return false;
    }
    v["tokens"]["access_token"]
        .as_str()
        .and_then(crate::proxy::codex::jwt_expiry)
        .is_some_and(|exp| exp - now_secs <= KEEP_ALIVE_CODEX_WINDOW_SECS)
}

/// The Codex half of the keep-alive sweep.
///
/// The sweep was written because an idle account's refresh token goes stale, and
/// it looked at Claude alone. A Codex account is idle BY DESIGN between runs -
/// which is the case the sweep exists for, and the one it did not cover.
pub fn keep_alive_sweep_codex(
    slots: &[(String, std::path::PathBuf)],
    now_ms: i64,
) -> (Vec<String>, Vec<(String, RefreshError)>) {
    let (mut renewed, mut failed) = (Vec::new(), Vec::new());
    for (name, dir) in slots {
        let Ok(blob) = std::fs::read(dir.join("auth.json")) else {
            continue;
        };
        if !wants_keep_alive_codex(&blob, now_ms / 1000) {
            continue;
        }
        match refresh_codex_slot(dir, now_ms) {
            Ok(()) => renewed.push(name.clone()),
            // Being in use is the guard doing its job, not a failure worth
            // reporting: that account is alive by definition.
            Err(RefreshError::InUse) => {}
            Err(e) => failed.push((name.clone(), e)),
        }
    }
    (renewed, failed)
}

#[cfg(test)]
mod codex_keep_alive_tests {
    use super::*;

    fn jwt(exp: i64) -> String {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.sig",
            b64(br#"{"alg":"none"}"#),
            b64(format!(r#"{{"exp":{exp}}}"#).as_bytes())
        )
    }

    fn blob(exp: i64, refresh: &str) -> Vec<u8> {
        format!(
            r#"{{"tokens":{{"access_token":"{}","account_id":"a","refresh_token":"{refresh}"}}}}"#,
            jwt(exp)
        )
        .into_bytes()
    }

    /// The sweep exists because an idle account's refresh token goes stale. A
    /// Codex account is idle BY DESIGN between runs - Codex renews only when
    /// Codex runs, so the token dies exactly ten days later - which makes it the
    /// case the sweep was written for, and it was the one tool the sweep did not
    /// look at.
    #[test]
    fn a_codex_token_near_its_deadline_wants_renewing() {
        let now = 1_788_743_000i64;
        assert!(
            wants_keep_alive_codex(&blob(now + 3600, "R"), now),
            "an hour left"
        );
        assert!(
            wants_keep_alive_codex(&blob(now - 86_400, "R"), now),
            "already lapsed"
        );
        assert!(
            !wants_keep_alive_codex(&blob(now + 9 * 86_400, "R"), now),
            "nine days left is not the sweep's business yet"
        );
    }

    /// Renewing needs a refresh token, and a deadline swapdex could not read is
    /// not a deadline: neither is grounds for spending one.
    #[test]
    fn the_sweep_leaves_alone_what_it_cannot_renew() {
        let now = 1_788_743_000i64;
        assert!(
            !wants_keep_alive_codex(&blob(now + 60, ""), now),
            "no refresh token"
        );
        assert!(
            !wants_keep_alive_codex(
                br#"{"tokens":{"access_token":"not-a-jwt","refresh_token":"R"}}"#,
                now
            ),
            "an unreadable deadline is not an expired one"
        );
        assert!(!wants_keep_alive_codex(b"not json", now));
    }
}

#[cfg(test)]
mod keep_alive_tests {
    use super::*;

    fn blob(expires_in_ms: i64, refresh: &str, refresh_expiry: Option<i64>) -> Vec<u8> {
        let now = 1_700_000_000_000i64;
        let mut o = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "a",
                "refreshToken": refresh,
                "expiresAt": now + expires_in_ms,
            }
        });
        if let Some(r) = refresh_expiry {
            o["claudeAiOauth"]["refreshTokenExpiresAt"] = (now + r).into();
        }
        serde_json::to_vec(&o).unwrap()
    }
    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn an_idle_account_is_renewed_before_it_lapses() {
        let hour = 60 * 60 * 1000;
        assert!(
            wants_keep_alive(&blob(2 * hour, "r", None), NOW),
            "two hours left is inside the window"
        );
        assert!(
            !wants_keep_alive(&blob(12 * hour, "r", None), NOW),
            "half a day left needs nothing yet"
        );
    }

    /// Renewing takes a refresh token. Without one - or with one already gone -
    /// the sweep has nothing to do, and only a sign-in helps.
    #[test]
    fn there_is_nothing_to_sweep_without_a_live_refresh_token() {
        let hour = 60 * 60 * 1000;
        assert!(
            !wants_keep_alive(&blob(hour, "", None), NOW),
            "no refresh token"
        );
        assert!(
            !wants_keep_alive(&blob(hour, "r", Some(-1)), NOW),
            "the refresh token itself has expired"
        );
    }

    #[test]
    fn nonsense_is_never_swept() {
        assert!(!wants_keep_alive(b"not json", NOW));
        assert!(!wants_keep_alive(br#"{"claudeAiOauth":{}}"#, NOW));
    }
}

/// Renew every idle account whose token is heading for expiry. Returns the names
/// it renewed and the ones it could not, so a caller can say what happened.
///
/// Deliberately per-account and forgiving: one account's dead refresh token must
/// not stop the sweep reaching the next. `refresh_slot` refuses a slot the tool
/// is running in, which is the guard that keeps this from logging anyone out.
pub fn keep_alive_sweep(
    slots: &[(String, std::path::PathBuf)],
    now_ms: i64,
) -> (Vec<String>, Vec<(String, RefreshError)>) {
    let (mut renewed, mut failed) = (Vec::new(), Vec::new());
    for (name, dir) in slots {
        let Some(blob) = read_credential(dir) else {
            continue;
        };
        if !wants_keep_alive(blob.expose(), now_ms) {
            continue;
        }
        match refresh_slot(dir, now_ms) {
            Ok(()) => renewed.push(name.clone()),
            // Being in use is the guard doing its job, not a failure worth
            // reporting: that account is alive by definition.
            Err(RefreshError::InUse) => {}
            Err(e) => failed.push((name.clone(), e)),
        }
    }
    (renewed, failed)
}

#[cfg(test)]
mod one_refresh_per_burst_tests {
    use super::*;

    /// A burst of 401s must produce ONE refresh, not one per request.
    ///
    /// Refresh tokens rotate: each use mints a new one and retires the old. So
    /// N concurrent turns each refreshing the same slot spend the same token N
    /// times, and every result but one is already invalid when it lands - the
    /// account ends up logged out by its own renewal. teamclaude hit this as
    /// "don't rotate the token family once per 401 in a burst".
    ///
    /// The gate is per-slot and time-bounded: a second refresh moments later is
    /// the burst, a refresh minutes later is a new event.
    #[test]
    fn a_burst_of_refreshes_collapses_to_one() {
        let g = RefreshGate::default();
        let dir = std::path::Path::new("/s/a");
        // First caller in: proceeds.
        assert!(g.claim(dir, 1_000));
        // Everyone else in the same burst: stands down.
        assert!(!g.claim(dir, 1_000));
        assert!(!g.claim(dir, 1_002));
        // A different slot is unaffected - one account's burst must not block
        // another's genuine refresh.
        assert!(g.claim(std::path::Path::new("/s/b"), 1_000));
        // Long enough later, it is a new event rather than the same burst.
        assert!(g.claim(dir, 1_000 + BURST_SECS + 1));
    }
}

#[cfg(test)]
mod point_of_effect_tests {
    use super::*;

    /// The gate has to sit at the point of effect, not at one caller.
    ///
    /// `RefreshGate`'s own doc names the outcome: N concurrent renewals of one
    /// slot spend the same refresh token N times, every result but one is dead
    /// when it lands, and the account logs itself out. Only one of the three
    /// paths reaching `refresh_slot` claimed it - the keep-alive sweep and
    /// `has_usable_login` went straight through. A rule enforced at one caller
    /// is a rule the next caller does not know exists.
    #[test]
    fn a_second_refresh_of_the_same_slot_in_a_burst_stands_down() {
        let a = std::path::Path::new("/tmp/swapdex-gate-a");
        let b = std::path::Path::new("/tmp/swapdex-gate-b");
        assert!(claim_refresh_at(a, 10_000), "the first caller goes ahead");
        assert!(
            !claim_refresh_at(a, 10_000),
            "a second in the same burst stands down"
        );
        assert!(
            !claim_refresh_at(a, 10_000 + BURST_SECS),
            "still inside the window"
        );
        assert!(
            claim_refresh_at(a, 10_001 + BURST_SECS),
            "a later refresh is a new event, not the same burst"
        );
        assert!(
            claim_refresh_at(b, 10_000),
            "another account is never blocked"
        );
    }

    /// Standing down is not the login server refusing, and must not read as it.
    #[test]
    fn standing_down_is_its_own_answer() {
        let e = RefreshError::AlreadyRefreshing;
        let r = e.remedy("rnd");
        assert!(
            !r.to_lowercase().contains("sign in"),
            "nobody needs to sign in: {r}"
        );
        assert!(r.contains("rnd"), "name the account: {r}");
    }
}

#[cfg(test)]
mod codex_in_use_tests {
    use super::*;

    /// The Codex renewal refuses a slot a live session is on.
    ///
    /// This is the guard `refresh_codex_slot` documents, and the reason the
    /// keep-alive sweep cannot log an account out: the session in that slot
    /// holds the refresh token the renewal retires, so its own next renewal
    /// would fail. The slot deliberately has no `auth.json`, so a renewal that
    /// gets past the guard stops at `NoCredential` - what this asserts against -
    /// without reaching the network.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_session_refuses_the_renewal() {
        let root = std::env::temp_dir().join(format!("swapdex_cx_ref_{}", std::process::id()));
        let slot = root.join("slot");
        std::fs::create_dir_all(&slot).unwrap();
        let bin = root.join("codex");
        std::fs::copy("/bin/sleep", &bin)
            .or_else(|_| std::fs::copy("/usr/bin/sleep", &bin))
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env("CODEX_HOME", &slot)
            .spawn()
            .unwrap();
        // Wait for the exec, not for the code under test: a process is only on
        // its slot once its environment exists.
        let environ = format!("/proc/{}/environ", child.id());
        for _ in 0..300 {
            if std::fs::read(&environ).is_ok_and(|b| !b.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let verdict = refresh_codex_slot(&slot, 1_700_000_000_000);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            matches!(verdict, Err(RefreshError::InUse)),
            "renewed a slot a live session holds: {verdict:?}"
        );
    }
}

#[cfg(test)]
mod refresh_deadline_tests {
    use super::*;

    fn blob(refresh_exp: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"OLD-AT","refreshToken":"OLD-RT",
               "expiresAt":1000,"refreshTokenExpiresAt":{refresh_exp},
               "subscriptionType":"max"}}}}"#
        )
    }

    /// A renewal that mints a NEW refresh token must not keep the old one's
    /// deadline.
    ///
    /// The deadline describes the token it was issued with. Rotation retires
    /// that token, so the recorded moment now describes something that no
    /// longer exists - and `refresh_token_expired` reads it to decide whether
    /// renewing is even worth trying. Kept, it made every renewal leave the
    /// account one day closer to a sign-in swapdex would demand while holding
    /// a refresh token the server had just issued.
    #[test]
    fn a_rotated_refresh_token_does_not_inherit_the_old_deadline() {
        let now = 1_800_000_000_000i64;
        let dead_soon = now + 86_400_000; // tomorrow
        let out = merge_response(
            blob(dead_soon).as_bytes(),
            r#"{"access_token":"NEW-AT","refresh_token":"NEW-RT","expires_in":3600}"#,
            now,
        )
        .expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let o = &v["claudeAiOauth"];
        assert_eq!(o["refreshToken"], "NEW-RT", "the token rotated");
        assert!(
            !refresh_token_expired(&out, now + 2 * 86_400_000),
            "the new token inherited the retired one's deadline: {o}"
        );
    }

    /// When the server states the new token's lifetime, record it.
    #[test]
    fn a_stated_refresh_lifetime_is_recorded() {
        let now = 1_800_000_000_000i64;
        let out = merge_response(
            blob(now + 1000).as_bytes(),
            r#"{"access_token":"A","refresh_token":"R","expires_in":3600,
                "refresh_token_expires_in":604800}"#,
            now,
        )
        .expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["claudeAiOauth"]["refreshTokenExpiresAt"].as_i64(),
            Some(now + 604_800 * 1000),
            "the stated lifetime was not written"
        );
    }

    /// A renewal that does NOT rotate keeps the deadline it had.
    ///
    /// The old token is still the live one, so its deadline still describes
    /// something real. Dropping it there would throw away the one signal that
    /// says a sign-in is genuinely needed.
    #[test]
    fn a_renewal_without_rotation_keeps_the_deadline() {
        let now = 1_800_000_000_000i64;
        let deadline = now + 86_400_000;
        let out = merge_response(
            blob(deadline).as_bytes(),
            r#"{"access_token":"NEW-AT","expires_in":3600}"#,
            now,
        )
        .expect("merged");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["claudeAiOauth"]["refreshTokenExpiresAt"].as_i64(),
            Some(deadline),
            "the live token's deadline was discarded"
        );
    }
}
