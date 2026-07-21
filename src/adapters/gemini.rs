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

/// A roll-back journal for Gemini's two-file apply (`oauth_creds.json` +
/// `google_accounts.json`) - the same crash-atomicity fix as the Claude
/// adapter's WAL. A SIGKILL / power loss between the two writes would leave one
/// account's token with another's identity, which a later `use` would silently
/// apply. apply writes+fsyncs this (both files' prior bytes) BEFORE the first
/// write and removes it once the state is consistent; a surviving journal means
/// the run was interrupted and is rolled back to the pre-switch state on the
/// next apply/capture.
#[derive(serde::Serialize, serde::Deserialize)]
struct GeminiWal {
    oauth_path: String,
    /// prior `oauth_creds.json` bytes as hex; None = the file was absent.
    oauth_prior_hex: Option<String>,
    accounts_path: String,
    /// prior `google_accounts.json` bytes as hex; None = the file was absent.
    accounts_prior_hex: Option<String>,
}

fn gemini_wal_path(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("apply-gemini.wal")
}

fn write_gemini_wal(paths: &Paths, wal: &GeminiWal) -> Result<()> {
    let p = gemini_wal_path(paths);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let bytes = serde_json::to_vec(wal).context("serialize gemini WAL")?;
    crate::atomic::write_secret(&p, &bytes)
}

fn remove_gemini_wal(paths: &Paths) {
    let _ = std::fs::remove_file(gemini_wal_path(paths));
}

/// Restore one file to the prior bytes a WAL records (or delete it if it was
/// absent). Returns whether the restore succeeded.
fn restore_gemini_file(path: &str, prior_hex: &Option<String>) -> bool {
    let p = std::path::PathBuf::from(path);
    match prior_hex {
        Some(hex) => match crate::adapters::claude::from_hex(hex) {
            Some(b) => crate::atomic::write_secret(&p, &b).is_ok(),
            None => false,
        },
        None => std::fs::remove_file(&p).is_ok() || !p.exists(),
    }
}

/// Roll both Gemini files back to the pre-switch state a surviving WAL records,
/// so a crash mid-apply never leaves a mixed token/identity. The WAL is removed
/// only once every restore succeeded, so an unrecoverable slice retries on the
/// next call rather than silently leaving a mix. No-op when there is no WAL.
pub(crate) fn recover_interrupted_gemini_apply(paths: &Paths) {
    let p = gemini_wal_path(paths);
    let Ok(bytes) = std::fs::read(&p) else {
        return;
    };
    let Ok(wal) = serde_json::from_slice::<GeminiWal>(&bytes) else {
        return; // unparseable: leave for inspection, never guess
    };
    let mut ok = restore_gemini_file(&wal.oauth_path, &wal.oauth_prior_hex);
    ok &= restore_gemini_file(&wal.accounts_path, &wal.accounts_prior_hex);
    if ok {
        remove_gemini_wal(paths);
    }
}

impl AuthTool for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn present(&self, paths: &Paths) -> bool {
        paths.gemini_oauth().exists()
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        // Heal a crashed apply before reading, so we never capture a mixed
        // (A-token + B-identity) live state into a profile.
        recover_interrupted_gemini_apply(paths);
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
        // Heal a previously-interrupted apply (roll it back) before starting.
        recover_interrupted_gemini_apply(paths);
        let oauth = snap.part("oauth").context("snapshot missing oauth")?;
        let accounts = snap.part("accounts").context("snapshot missing accounts")?;
        // Validate BOTH blobs before touching any live file.
        serde_json::from_slice::<Value>(oauth.expose())
            .context("saved oauth_creds are not valid JSON; refusing to apply")?;
        serde_json::from_slice::<Value>(accounts.expose())
            .context("saved google_accounts are not valid JSON; refusing to apply")?;
        let oauth_path = paths.gemini_oauth();
        let accounts_path = paths.gemini_accounts();
        let prev_oauth = if oauth_path.exists() {
            crate::atomic::read_regular(&oauth_path).ok()
        } else {
            None
        };
        let prev_accounts = if accounts_path.exists() {
            crate::atomic::read_regular(&accounts_path).ok()
        } else {
            None
        };
        // Journal BOTH files' prior bytes and fsync BEFORE the first write, so a
        // crash between the two writes is rolled back on the next apply/capture
        // (recover_interrupted_gemini_apply). Removed once the state is consistent.
        write_gemini_wal(
            paths,
            &GeminiWal {
                oauth_path: oauth_path.to_string_lossy().into_owned(),
                oauth_prior_hex: prev_oauth.as_deref().map(crate::adapters::claude::to_hex),
                accounts_path: accounts_path.to_string_lossy().into_owned(),
                accounts_prior_hex: prev_accounts
                    .as_deref()
                    .map(crate::adapters::claude::to_hex),
            },
        )?;
        // Both-or-neither: oauth first, then accounts, rolling oauth back (in
        // process) if the second write fails; a CRASH is healed by the WAL above.
        crate::atomic::write_secret(&oauth_path, oauth.expose())?;
        if let Err(e) = crate::atomic::write_secret(&accounts_path, accounts.expose()) {
            let (msg, rolled_back) = match &prev_oauth {
                Some(prev) => match crate::atomic::write_secret(&oauth_path, prev) {
                    Ok(()) => ("apply aborted; oauth_creds rolled back", true),
                    Err(_) => (
                        "apply aborted and the rollback FAILED - the login may be \
                         half-swapped; run `swapdex restore --tool gemini` once the \
                         underlying problem (e.g. disk space) is fixed",
                        false,
                    ),
                },
                None => match std::fs::remove_file(&oauth_path) {
                    // Fresh install: remove the just-written file rather than
                    // leave a half-swap (new oauth_creds, no accounts file).
                    Ok(()) => (
                        "apply aborted; the just-written oauth_creds were removed",
                        true,
                    ),
                    Err(_) => (
                        "apply aborted and cleanup FAILED - oauth_creds was written without \
                         google_accounts; run `swapdex restore --tool gemini` once the \
                         underlying problem is fixed",
                        false,
                    ),
                },
            };
            // Rolled back to a consistent (pre-switch) state -> retire the WAL, so
            // a later apply can't revert a legitimate token rotation. A FAILED
            // rollback left a mix -> keep the WAL so recovery heals it next time.
            if rolled_back {
                remove_gemini_wal(paths);
            }
            return Err(e.context(msg));
        }
        // Both files written: consistent - retire the WAL.
        remove_gemini_wal(paths);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(p: &Paths, oauth: &[u8], accounts: &[u8]) {
        std::fs::create_dir_all(p.gemini_oauth().parent().unwrap()).unwrap();
        std::fs::write(p.gemini_oauth(), oauth).unwrap();
        std::fs::write(p.gemini_accounts(), accounts).unwrap();
    }

    // #2-class: a crash mid-apply (WAL survives beside a half-written login - A's
    // oauth token but B's accounts) is rolled back to the pre-switch state, never
    // a mixed A-token + B-identity. The credential-file path (no Keychain).
    #[test]
    fn recover_rolls_back_a_crashed_gemini_apply_to_prior() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::rooted(d.path());
        let oauth_b = br#"{"refresh_token":"RT-B"}"#;
        let accounts_b = br#"{"active":"b@x.com"}"#;
        seed(&p, oauth_b, accounts_b);
        // Journal B's prior, as apply does before its first write.
        write_gemini_wal(
            &p,
            &GeminiWal {
                oauth_path: p.gemini_oauth().to_string_lossy().into_owned(),
                oauth_prior_hex: Some(crate::adapters::claude::to_hex(oauth_b)),
                accounts_path: p.gemini_accounts().to_string_lossy().into_owned(),
                accounts_prior_hex: Some(crate::adapters::claude::to_hex(accounts_b)),
            },
        )
        .unwrap();
        // Simulate the crash: A's oauth written, accounts NOT yet (still B) = a mix.
        std::fs::write(p.gemini_oauth(), br#"{"refresh_token":"RT-A"}"#).unwrap();

        recover_interrupted_gemini_apply(&p);

        assert_eq!(
            std::fs::read(p.gemini_oauth()).unwrap(),
            oauth_b,
            "oauth rolled back to B"
        );
        assert_eq!(
            std::fs::read(p.gemini_accounts()).unwrap(),
            accounts_b,
            "accounts stays B"
        );
        assert!(
            !gemini_wal_path(&p).exists(),
            "WAL removed after a successful recovery"
        );
    }

    // A clean apply retires its WAL (no leftover journal to recover from).
    #[test]
    fn gemini_apply_leaves_no_wal_on_success() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::rooted(d.path());
        seed(
            &p,
            br#"{"refresh_token":"RT-B"}"#,
            br#"{"active":"b@x.com"}"#,
        );
        let snap = Gemini.capture(&p).unwrap();
        Gemini.apply(&p, &snap).unwrap();
        assert!(
            !gemini_wal_path(&p).exists(),
            "WAL retired once the apply is consistent"
        );
    }
}
