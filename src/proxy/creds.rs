//! Reading a slot's own credential. The slot is the single source of truth: the
//! token is held in memory for one request and never written anywhere else.

use crate::secret::Secret;
use std::path::Path;

/// The access token stored in this slot, or `None` when the slot has no readable
/// login. Linux/WSL: the slot's `.credentials.json`. macOS: the slot's own
/// Keychain item, whose service name is derived from the config dir the way
/// Claude Code derives it.
pub fn slot_token(dir: &Path) -> Option<Secret> {
    if let Ok(bytes) = std::fs::read(dir.join(".credentials.json")) {
        if let Some(t) = access_token(&bytes) {
            return Some(t);
        }
    }
    let bytes = crate::adapters::claude::slot_keychain_read(dir)?;
    access_token(&bytes)
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
    fn slot_token_is_none_for_an_unparseable_credential() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"not json").unwrap();
        assert!(slot_token(dir.path()).is_none());
    }
}
