//! Antigravity CLI (Google's agentic CLI, binary `agy`): a single token file
//! at `~/.gemini/antigravity-cli/antigravity-oauth-token` -
//! `{token: {access_token, refresh_token, expiry, token_type}, auth_method}`.
//! No email or account id is stored anywhere readable, so identity is derived:
//! a one-way hash of the refresh token is the stable account id (it changes on
//! a fresh re-login, which honestly degrades the profile match to "not saved"),
//! and `auth_method` fills the tier column.

use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct Antigravity;

/// One-way 12-hex fingerprint of the refresh token: a stable per-account id
/// that never exposes the token (sha256 of a high-entropy secret, truncated).
pub(crate) fn token_fingerprint(v: &Value) -> String {
    let rt = v["token"]["refresh_token"].as_str().unwrap_or("");
    if rt.is_empty() {
        return String::new();
    }
    let d = Sha256::digest(format!("swapdex-antigravity:{rt}").as_bytes());
    let mut hex = String::with_capacity(12);
    for b in &d[..6] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

impl AuthTool for Antigravity {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn present(&self, paths: &Paths) -> bool {
        paths.antigravity_token().exists()
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        let path = paths.antigravity_token();
        if !path.exists() {
            bail!("not logged in to Antigravity (no {})", path.display());
        }
        let bytes = crate::atomic::read_regular(&path)?;
        serde_json::from_slice::<Value>(&bytes)
            .context("antigravity-oauth-token is not valid JSON")?;
        Ok(Snapshot {
            tool: "antigravity",
            blobs: vec![("token".into(), Secret::new(bytes))],
        })
    }

    fn apply(&self, paths: &Paths, snap: &Snapshot) -> Result<()> {
        let token = snap.part("token").context("snapshot missing token")?;
        serde_json::from_slice::<Value>(token.expose())
            .context("saved antigravity token is not valid JSON; refusing to apply")?;
        crate::atomic::write_secret(&paths.antigravity_token(), token.expose())
    }

    fn identity(&self, paths: &Paths) -> Result<Option<Account>> {
        let path = paths.antigravity_token();
        if !path.exists() {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&crate::atomic::read_regular(&path)?)
            .context("parse antigravity-oauth-token")?;
        Ok(Some(Account {
            tool: "antigravity",
            account_id: token_fingerprint(&v),
            display: "Google account (Antigravity)".into(),
            email: None,
            tier: v["auth_method"].as_str().map(|s| s.to_string()),
            // The ~1h access-token expiry is refreshed silently by the CLI, so
            // it is NOT a meaningful "login expired" signal - never flag it.
            expires_at: None,
        }))
    }
}
