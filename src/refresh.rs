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
            Self::Refused(why) => format!("'{name}' could not be renewed: {why}"),
            Self::Offline(why) => format!("could not reach the login server: {why}"),
        }
    }
}

/// Does this credential need renewing? True when the access token has lapsed or
/// is about to - a token that expires mid-flight is already useless.
pub fn needs_refresh(blob: &[u8], now_ms: i64) -> bool {
    const SLACK_MS: i64 = 5 * 60_000;
    serde_json::from_slice::<serde_json::Value>(blob)
        .ok()
        .and_then(|v| v["claudeAiOauth"]["expiresAt"].as_i64())
        .is_some_and(|exp| exp - now_ms <= SLACK_MS)
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
pub fn refresh_slot(dir: &Path, now_ms: i64) -> Result<(), RefreshError> {
    // A slot the tool is using is never touched - see the module note.
    if slot_in_use(dir) {
        return Err(RefreshError::InUse);
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
fn slot_in_use(dir: &Path) -> bool {
    crate::proc::config_dir_in_use(dir)
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
    fn renewal_is_due_before_the_token_actually_lapses() {
        let now = 1_800_000_000_000i64;
        let at = |exp: i64| format!(r#"{{"claudeAiOauth":{{"expiresAt":{exp}}}}}"#);
        assert!(needs_refresh(at(now - 1).as_bytes(), now), "already lapsed");
        assert!(
            needs_refresh(at(now + 60_000).as_bytes(), now),
            "a minute left would lapse mid-flight"
        );
        assert!(
            !needs_refresh(at(now + 3_600_000).as_bytes(), now),
            "an hour"
        );
        // No expiry recorded is not a reason to renew.
        assert!(!needs_refresh(br#"{"claudeAiOauth":{}}"#, now));
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
        let msg = RefreshError::InUse.remedy("work");
        assert!(
            msg.contains("its own session will renew it"),
            "an in-use slot is fine, not broken: {msg}"
        );
    }
}
