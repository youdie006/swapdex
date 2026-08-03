//! Reading a slot's own credential. The slot is the single source of truth: the
//! token is held in memory for one request and never written anywhere else.

use crate::secret::Secret;
use std::path::Path;

/// The access token stored in this slot, or `None` when the slot has no readable
/// login. Linux/WSL: the slot's `.credentials.json`. macOS: the slot's own
/// Keychain item, whose service name is derived from the config dir the way
/// Claude Code derives it.
pub fn slot_token(dir: &Path) -> Option<Secret> {
    slot_token_detail(dir).ok()
}

/// Why a slot's token could not be read, phrased as the thing the user should do.
/// A LOCKED Keychain is the case worth separating: the account is signed in and
/// the environment is what is wrong (a non-interactive ssh session cannot read a
/// secret from the login keychain), so telling the user to sign in again would
/// send them to fix something that is not broken.
pub fn slot_token_detail(dir: &Path) -> Result<Secret, TokenUnavailable> {
    if let Ok(bytes) = std::fs::read(dir.join(".credentials.json")) {
        if let Some(t) = access_token(&bytes) {
            return Ok(t);
        }
    }
    use crate::adapters::claude::KeychainReadError as K;
    match crate::adapters::claude::slot_keychain_read_detail(dir) {
        Ok(bytes) => access_token(&bytes).ok_or(TokenUnavailable::NoLogin),
        Err(K::Locked) => Err(TokenUnavailable::KeychainLocked),
        Err(K::Missing | K::NotApplicable) => Err(TokenUnavailable::NoLogin),
    }
}

pub enum TokenUnavailable {
    /// The slot has never been signed into (or its login is unreadable).
    NoLogin,
    /// macOS: the login exists but the keychain will not release it here.
    KeychainLocked,
}

impl TokenUnavailable {
    /// The one next step, for the account named `name`.
    pub fn remedy(&self, name: &str) -> String {
        match self {
            Self::NoLogin => format!(
                "account '{name}' has no usable login - `swapdex run {name}` once signs it in"
            ),
            Self::KeychainLocked => format!(
                "account '{name}' is signed in, but macOS will not release its login here: \
                 reading a Keychain secret needs an unlocked login keychain, which a remote \
                 or non-interactive shell does not have. Run the proxy from a terminal on \
                 the Mac itself (or unlock with `security unlock-keychain`)."
            ),
        }
    }
}

/// Has this slot's access token already expired? Sending an expired token just
/// earns a 401, and nothing here can refresh it - Claude refreshes its OWN
/// token, not one the proxy injected - so an expired slot must be stepped over
/// rather than tried. `false` when the expiry cannot be read (macOS keeps the
/// credential in the Keychain): unknown is not the same as expired.
pub fn slot_token_expired(dir: &Path, now_ms: i64) -> bool {
    // A minute of slack: a token about to lapse mid-flight is already useless.
    const SLACK_MS: i64 = 60_000;
    // The expiry lives inside the credential blob, wherever that blob is kept -
    // a file on Linux/WSL, the Keychain on macOS. Reading only the file meant
    // every macOS slot looked fresh and its lapsed token was sent anyway.
    let blob = std::fs::read(dir.join(".credentials.json"))
        .ok()
        .or_else(|| crate::adapters::claude::slot_keychain_read_detail(dir).ok());
    blob.and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["claudeAiOauth"]["expiresAt"].as_i64())
        .is_some_and(|exp| exp - now_ms <= SLACK_MS)
}

/// This slot's own account UUID, from its `.claude.json` `oauthAccount` - the
/// identity Claude reports as "connected". Used to keep a forwarded request's
/// `metadata.user_id` consistent with the token serving it. An identifier, not a
/// secret, and never logged.
pub fn slot_account_uuid(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["oauthAccount"]["accountUuid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// This slot's connected email, from its `.claude.json` - a label, not a secret.
pub fn slot_email(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["oauthAccount"]["emailAddress"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Does the identity recorded for this slot disagree with the login it holds?
///
/// The name beside an account comes from `.claude.json`, the numbers come from
/// the credential, and nothing keeps them in step - so a config whose identity
/// was overwritten shows one person's name above another person's usage. Only a
/// contradiction is reported, never a guess: an identity that names an
/// organisation beside a personal subscription cannot both be true.
pub fn identity_contradicts_login(dir: &Path) -> Option<String> {
    let id = std::fs::read(dir.join(".claude.json")).ok()?;
    let id: serde_json::Value = serde_json::from_slice(&id).ok()?;
    let email = id["oauthAccount"]["emailAddress"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    let org = id["oauthAccount"]["organizationName"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    // A personal account still carries an organisation - Anthropic names one
    // after the address itself. Treating any organisation as proof of a team
    // account made every personal login look like a contradiction.
    if org.contains(email) {
        return None;
    }
    let blob = slot_token_blob(dir)?;
    let cred: serde_json::Value = serde_json::from_slice(&blob).ok()?;
    let sub = cred["claudeAiOauth"]["subscriptionType"].as_str()?;
    // A personal plan cannot be the login of an organisation account.
    if matches!(sub, "max" | "pro") {
        return Some(format!(
            "recorded as {email} ({org}) but signed in on a '{sub}' plan -              the name and the login belong to different accounts"
        ));
    }
    None
}

/// The credential blob for this slot, wherever it lives.
fn slot_token_blob(dir: &Path) -> Option<Vec<u8>> {
    if let Ok(b) = std::fs::read(dir.join(".credentials.json")) {
        if !b.is_empty() {
            return Some(b);
        }
    }
    crate::adapters::claude::slot_keychain_read_detail(dir).ok()
}

/// Pull `claudeAiOauth.accessToken` out of a Claude credential blob.
fn access_token(bytes: &[u8]) -> Option<Secret> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let t = v["claudeAiOauth"]["accessToken"].as_str()?;
    (!t.is_empty()).then(|| Secret::new(t.as_bytes().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_token_reads_the_access_token_from_the_slot_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(slot_token(dir.path()).is_none(), "no login yet");
        std::fs::write(
            dir.path().join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"AT-1","refreshToken":"RT-1","expiresAt":1}}"#,
        )
        .unwrap();
        let t = slot_token(dir.path()).expect("token");
        assert_eq!(t.expose(), b"AT-1");
    }

    #[test]
    fn slot_account_uuid_comes_from_the_slots_oauth_account() {
        let dir = tempfile::tempdir().unwrap();
        assert!(slot_account_uuid(dir.path()).is_none(), "no config yet");
        std::fs::write(
            dir.path().join(".claude.json"),
            br#"{"oauthAccount":{"accountUuid":"u-1","emailAddress":"a@x.com"}}"#,
        )
        .unwrap();
        assert_eq!(slot_account_uuid(dir.path()).as_deref(), Some("u-1"));
        // An empty or absent uuid must not become an identity.
        std::fs::write(
            dir.path().join(".claude.json"),
            br#"{"oauthAccount":{"accountUuid":""}}"#,
        )
        .unwrap();
        assert!(slot_account_uuid(dir.path()).is_none());
    }

    #[test]
    fn an_expired_slot_is_recognised_and_an_unknown_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000_000i64;
        // Unknown expiry (no file, or a Keychain-backed slot) is NOT "expired":
        // stepping over an account we simply cannot read would be a guess.
        assert!(!slot_token_expired(dir.path(), now));
        let write = |exp: i64| {
            std::fs::write(
                dir.path().join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"accessToken":"A","expiresAt":{exp}}}}}"#),
            )
            .unwrap()
        };
        write(now + 3_600_000);
        assert!(!slot_token_expired(dir.path(), now), "an hour left is fine");
        write(now - 1);
        assert!(slot_token_expired(dir.path(), now), "already lapsed");
        write(now + 30_000);
        assert!(
            slot_token_expired(dir.path(), now),
            "about to lapse mid-flight counts as expired"
        );
    }

    // The name comes from one file and the numbers from another, and nothing
    // keeps them in step - so a config whose identity was overwritten shows one
    // person's name above another person's usage.
    #[test]
    fn an_identity_that_cannot_match_its_login_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let id = |email: &str, org: &str| {
            std::fs::write(
                dir.path().join(".claude.json"),
                format!(
                    r#"{{"oauthAccount":{{"emailAddress":"{email}","organizationName":"{org}"}}}}"#
                ),
            )
            .unwrap()
        };
        let cred = |sub: &str| {
            std::fs::write(
                dir.path().join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"accessToken":"A","subscriptionType":"{sub}"}}}}"#),
            )
            .unwrap()
        };
        // An organisation account signed in on a personal plan cannot be one
        // account: say so, and name both halves.
        id("a@company.com", "Acme RnD");
        cred("max");
        let msg = identity_contradicts_login(dir.path()).expect("contradiction");
        assert!(msg.contains("a@company.com"), "{msg}");
        assert!(msg.contains("Acme RnD"), "{msg}");
        assert!(msg.contains("max"), "{msg}");

        // The same account consistently: nothing to report.
        cred("team");
        assert!(identity_contradicts_login(dir.path()).is_none());
        // A personal identity on a personal plan is not a contradiction either -
        // including the organisation Anthropic names after the address itself,
        // which every personal account has and which is not a team.
        cred("max");
        id("me@gmail.com", "");
        assert!(identity_contradicts_login(dir.path()).is_none());
        id("me@gmail.com", "me@gmail.com's Organization");
        assert!(
            identity_contradicts_login(dir.path()).is_none(),
            "an account named after its own address is not an organisation"
        );
        // And nothing is claimed when either half is missing.
        std::fs::remove_file(dir.path().join(".credentials.json")).unwrap();
        assert!(identity_contradicts_login(dir.path()).is_none());
    }

    #[test]
    fn slot_token_is_none_for_an_unparseable_credential() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"not json").unwrap();
        assert!(slot_token(dir.path()).is_none());
    }
}
