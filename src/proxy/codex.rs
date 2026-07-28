//! The Codex leg of proxy mode.
//!
//! Codex authenticates a ChatGPT subscription with an OAuth bearer plus a
//! `ChatGPT-Account-ID` header, and it attaches both itself when its model
//! provider declares no `env_key`. That is what makes a proxy possible at all:
//! the request arrives already shaped, and changing accounts is a rewrite of
//! those two values with the pair held by the slot serving this turn.
//!
//! The two must move together. A token from one account with another's
//! account-id is not a half-switch, it is a request the backend refuses - so
//! they are read as one thing and never applied separately.

use crate::secret::Secret;
use std::path::Path;

/// Where Codex's ChatGPT backend lives. `SWAPDEX_UPSTREAM_CODEX` redirects it
/// for hermetic tests, the same fixture pattern the Claude leg uses, so no test
/// ever reaches the real backend.
pub fn base_url() -> String {
    std::env::var("SWAPDEX_UPSTREAM_CODEX")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".to_string())
}

/// The upstream URL for a path the client asked for.
///
/// Codex talks to its provider in OpenAI's shape (`/v1/responses`) while the
/// ChatGPT backend that serves a subscription mounts the same endpoint without
/// that prefix. Joining the two naively produced `/backend-api/codex/v1/responses`,
/// which is not an endpoint - so the version prefix is dropped here rather than
/// left for the backend to reject.
pub fn upstream_url(base: &str, path: &str) -> String {
    let rest = path.strip_prefix("/v1").unwrap_or(path);
    format!("{}{}", base.trim_end_matches('/'), rest)
}

/// One account's ChatGPT credentials, as the backend wants them.
pub struct Auth {
    pub token: Secret,
    pub account_id: String,
}

/// Read the ChatGPT login held in this slot's `auth.json`. `None` when the slot
/// has never been signed into, or when either half is missing - a token without
/// its account-id is not a usable login here.
pub fn slot_auth(dir: &Path) -> Option<Auth> {
    let bytes = std::fs::read(dir.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let t = v["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    let id = v["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    Some(Auth {
        token: Secret::new(t.as_bytes().to_vec()),
        account_id: id.to_string(),
    })
}

/// The account this slot is signed into, for display and for deciding whether a
/// switch would change anything. An identifier, not a secret.
pub fn slot_account_id(dir: &Path) -> Option<String> {
    slot_auth(dir).map(|a| a.account_id)
}

/// Replace the caller's credentials with the serving account's.
///
/// Both headers are dropped first and re-added, so a client that sent them in
/// any casing cannot leave a stale copy behind: two `authorization` headers
/// would make which account serves the turn depend on the backend's choice.
pub fn apply_auth(headers: &mut Vec<(String, String)>, auth: &Auth) {
    headers.retain(|(n, _)| {
        !matches!(
            n.to_ascii_lowercase().as_str(),
            "authorization" | "chatgpt-account-id"
        )
    });
    headers.push((
        "authorization".into(),
        format!("Bearer {}", String::from_utf8_lossy(auth.token.expose())),
    ));
    headers.push(("chatgpt-account-id".into(), auth.account_id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_auth(dir: &Path, token: &str, account: &str) {
        std::fs::write(
            dir.join("auth.json"),
            format!(
                r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{token}",
                   "refresh_token":"RT","account_id":"{account}"}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn the_version_prefix_is_dropped_for_the_chatgpt_backend() {
        let base = "https://chatgpt.com/backend-api/codex";
        assert_eq!(
            upstream_url(base, "/v1/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // A trailing slash on the base must not double up.
        assert_eq!(
            upstream_url("http://127.0.0.1:1/", "/v1/responses"),
            "http://127.0.0.1:1/responses"
        );
        // A path that never carried the prefix is passed through untouched.
        assert_eq!(
            upstream_url(base, "/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn a_slots_login_is_read_as_a_token_and_account_id_together() {
        let d = tempfile::tempdir().unwrap();
        assert!(slot_auth(d.path()).is_none(), "no login yet");
        write_auth(d.path(), "AT-1", "acct-1");
        let a = slot_auth(d.path()).expect("login");
        assert_eq!(a.token.expose(), b"AT-1");
        assert_eq!(a.account_id, "acct-1");
    }

    // Half a login is not a login: sending a token without its account-id earns a
    // refusal from the backend, and pretending the slot can serve would spend a
    // turn discovering that.
    #[test]
    fn half_a_login_is_not_a_login() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("auth.json"),
            br#"{"tokens":{"access_token":"AT-1"}}"#,
        )
        .unwrap();
        assert!(slot_auth(d.path()).is_none(), "no account id");
        std::fs::write(
            d.path().join("auth.json"),
            br#"{"tokens":{"account_id":"acct-1"}}"#,
        )
        .unwrap();
        assert!(slot_auth(d.path()).is_none(), "no token");
        std::fs::write(d.path().join("auth.json"), b"not json").unwrap();
        assert!(slot_auth(d.path()).is_none(), "unreadable");
    }

    // The client always sends its OWN credentials - that is how Codex is built -
    // so switching accounts means replacing both, with no trace of the old pair.
    #[test]
    fn applying_a_slots_login_replaces_the_clients_own_pair() {
        let d = tempfile::tempdir().unwrap();
        write_auth(d.path(), "AT-NEW", "acct-new");
        let auth = slot_auth(d.path()).unwrap();
        let mut headers = vec![
            ("Authorization".into(), "Bearer AT-OLD".to_string()),
            ("ChatGPT-Account-ID".into(), "acct-old".to_string()),
            ("originator".into(), "codex_cli_rs".to_string()),
        ];
        apply_auth(&mut headers, &auth);
        let get = |k: &str| {
            headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("authorization"), vec!["Bearer AT-NEW".to_string()]);
        assert_eq!(get("chatgpt-account-id"), vec!["acct-new".to_string()]);
        assert_eq!(
            get("originator"),
            vec!["codex_cli_rs".to_string()],
            "everything else the client sent is left alone"
        );
    }
}
