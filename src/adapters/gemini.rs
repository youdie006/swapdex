//! Gemini CLI login: `~/.gemini/oauth_creds.json` (OAuth tokens; the id_token
//! JWT carries the email and the stable Google `sub`) plus
//! `~/.gemini/google_accounts.json` ({"active": email, "old": [...]}). Both
//! files are swapped together with the same both-or-neither rollback the
//! Claude adapter uses - a half-swap would show one account and send as
//! another.

use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Gemini;

/// A claim out of the id_token JWT payload (no verification needed - this is
/// the user's own file, read for display/matching only).
pub(crate) fn jwt_claim(id_token: Option<&str>, claim: &str) -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let payload = id_token?.split('.').nth(1)?;
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: Value = serde_json::from_slice(&json).ok()?;
    v[claim].as_str().map(|s| s.to_string())
}

impl AuthTool for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn present(&self, paths: &Paths) -> bool {
        paths.gemini_oauth().exists()
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        let oauth_path = paths.gemini_oauth();
        if !oauth_path.exists() {
            bail!("not logged in to Gemini CLI (no {})", oauth_path.display());
        }
        let oauth = crate::atomic::read_regular(&oauth_path)?;
        serde_json::from_slice::<Value>(&oauth).context("oauth_creds.json is not valid JSON")?;
        // google_accounts.json may not exist right after a fresh login; treat
        // a missing file as an empty object rather than failing the capture.
        let accounts_path = paths.gemini_accounts();
        let accounts = if accounts_path.exists() {
            let bytes = crate::atomic::read_regular(&accounts_path)?;
            serde_json::from_slice::<Value>(&bytes)
                .context("google_accounts.json is not valid JSON")?;
            bytes
        } else {
            b"{}".to_vec()
        };
        Ok(Snapshot {
            tool: "gemini",
            blobs: vec![
                ("oauth".into(), Secret::new(oauth)),
                ("accounts".into(), Secret::new(accounts)),
            ],
        })
    }

    fn apply(&self, paths: &Paths, snap: &Snapshot) -> Result<()> {
        let oauth = snap.part("oauth").context("snapshot missing oauth")?;
        let accounts = snap.part("accounts").context("snapshot missing accounts")?;
        // Validate BOTH blobs before touching any live file.
        serde_json::from_slice::<Value>(oauth.expose())
            .context("saved oauth_creds are not valid JSON; refusing to apply")?;
        serde_json::from_slice::<Value>(accounts.expose())
            .context("saved google_accounts are not valid JSON; refusing to apply")?;
        // Both-or-neither: oauth first, then accounts, rolling oauth back if
        // the second write fails (mirrors the Claude adapter, including honest
        // reporting when the rollback itself fails).
        let oauth_path = paths.gemini_oauth();
        let prev_oauth = if oauth_path.exists() {
            crate::atomic::read_regular(&oauth_path).ok()
        } else {
            None
        };
        crate::atomic::write_secret(&oauth_path, oauth.expose())?;
        if let Err(e) = crate::atomic::write_secret(&paths.gemini_accounts(), accounts.expose()) {
            let msg = match prev_oauth {
                Some(prev) => match crate::atomic::write_secret(&oauth_path, &prev) {
                    Ok(()) => "apply aborted; oauth_creds rolled back",
                    Err(_) => {
                        "apply aborted and the rollback FAILED - the login may be \
                         half-swapped; run `swapdex restore --tool gemini` once the \
                         underlying problem (e.g. disk space) is fixed"
                    }
                },
                None => {
                    // Fresh install: remove the just-written file rather than
                    // leave a half-swap (new oauth_creds, no accounts file).
                    match std::fs::remove_file(&oauth_path) {
                        Ok(()) => "apply aborted; the just-written oauth_creds were removed",
                        Err(_) => {
                            "apply aborted and cleanup FAILED - oauth_creds was written                              without google_accounts; run `swapdex restore --tool gemini`                              once the underlying problem is fixed"
                        }
                    }
                }
            };
            return Err(e.context(msg));
        }
        Ok(())
    }

    fn identity(&self, paths: &Paths) -> Result<Option<Account>> {
        let oauth_path = paths.gemini_oauth();
        if !oauth_path.exists() {
            return Ok(None);
        }
        let oauth: Value = serde_json::from_slice(&crate::atomic::read_regular(&oauth_path)?)
            .context("parse oauth_creds.json")?;
        let id_token = oauth["id_token"].as_str();
        // The stable Google subject is the account id; the active email from
        // google_accounts.json is the friendlier display (id_token email as
        // fallback).
        let sub = jwt_claim(id_token, "sub").unwrap_or_default();
        let email = std::fs::read(paths.gemini_accounts())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["active"].as_str().map(|s| s.to_string()))
            .or_else(|| jwt_claim(id_token, "email"));
        Ok(Some(Account {
            tool: "gemini",
            account_id: sub,
            display: email.clone().unwrap_or_else(|| "Google account".into()),
            email,
            tier: None,
            expires_at: oauth["expiry_date"].as_i64(),
        }))
    }
}
