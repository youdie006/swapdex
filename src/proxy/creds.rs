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
    fn slot_token_is_none_for_an_unparseable_credential() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"not json").unwrap();
        assert!(slot_token(dir.path()).is_none());
    }
}
