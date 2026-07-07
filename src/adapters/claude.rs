use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Claude;

/// Is this a macOS Keychain-mode install? True when there is no credentials
/// FILE but `.claude.json` proves a login exists (its oauthAccount block).
/// `cfg!` (not `#[cfg]`) keeps this type-checked on every platform.
fn keychain_mode(paths: &Paths) -> bool {
    cfg!(target_os = "macos")
        && !paths.claude_credentials().exists()
        && std::fs::read(paths.claude_config_json())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(|v| v["oauthAccount"].is_object())
            .unwrap_or(false)
}

impl AuthTool for Claude {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn present(&self, paths: &Paths) -> bool {
        paths.claude_credentials().exists()
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        let cred = paths.claude_credentials();
        if !cred.exists() {
            if keychain_mode(paths) {
                bail!(
                    "Claude Code on macOS keeps its login in the Keychain, which swapdex \
                     cannot snapshot yet (Codex switching works; Claude-on-macOS is on \
                     the roadmap)"
                );
            }
            bail!("not logged in to Claude Code (no {})", cred.display());
        }
        let cred_bytes = crate::atomic::read_regular(&cred)?;
        serde_json::from_slice::<Value>(&cred_bytes)
            .context(".credentials.json is not valid JSON")?;
        // Extract ONLY the oauthAccount block from .claude.json, never the file.
        // Right after a fresh CLI login .claude.json may not exist yet; treat a
        // missing config as an absent oauthAccount rather than failing capture.
        let cfg_path = paths.claude_config_json();
        let cfg: Value = if cfg_path.exists() {
            serde_json::from_slice(&crate::atomic::read_regular(&cfg_path)?)
                .context("parse .claude.json")?
        } else {
            Value::Null
        };
        let oauth = cfg.get("oauthAccount").cloned().unwrap_or(Value::Null);
        let oauth_bytes = serde_json::to_vec(&oauth)?;
        Ok(Snapshot {
            tool: "claude-code",
            blobs: vec![
                ("credentials".into(), Secret::new(cred_bytes)),
                ("oauth_account".into(), Secret::new(oauth_bytes)),
            ],
        })
    }

    fn apply(&self, paths: &Paths, snap: &Snapshot) -> Result<()> {
        // On a Keychain-mode macOS install, writing .credentials.json would be
        // ignored by Claude Code while the oauthAccount swap would still change
        // what swapdex REPORTS - a half-switch that lies about the live login.
        // Refuse up front instead.
        if keychain_mode(paths) {
            bail!(
                "Claude Code on macOS keeps its login in the Keychain, which swapdex \
                 cannot write yet - refusing to switch claude-code (Codex switching \
                 works; Claude-on-macOS is on the roadmap)"
            );
        }
        let cred = snap
            .part("credentials")
            .context("snapshot missing credentials")?;
        let oauth = snap
            .part("oauth_account")
            .context("snapshot missing oauth_account")?;
        // Validate BOTH blobs before touching any live file, so a corrupt
        // snapshot can never brick the login (never write unvalidated bytes).
        serde_json::from_slice::<Value>(cred.expose())
            .context("saved credentials are not valid JSON; refusing to apply")?;
        let oauth_val: Value = serde_json::from_slice(oauth.expose())
            .context("saved oauthAccount is not valid JSON; refusing to apply")?;
        // Build the new .claude.json bytes (read-modify-write: replace ONLY the
        // oauthAccount key, preserve projects/mcpServers/theme/... - the A1
        // guarantee) BEFORE writing anything, so both writes are prepared first.
        let cfg_path = paths.claude_config_json();
        let mut cfg: Value = if cfg_path.exists() {
            serde_json::from_slice(&crate::atomic::read_regular(&cfg_path)?)
                .context("parse .claude.json")?
        } else {
            Value::Object(Default::default())
        };
        match cfg.as_object_mut() {
            Some(obj) => {
                obj.insert("oauthAccount".into(), oauth_val);
            }
            None => bail!(".claude.json is not a JSON object"),
        }
        let new_cfg = serde_json::to_vec(&cfg)?;
        // Both-or-neither: write credentials, then config. If the config write
        // fails, roll the credentials back so the login is never left half-
        // swapped (new credentials + old identity, or vice versa).
        let cred_path = paths.claude_credentials();
        let prev_creds = if cred_path.exists() {
            crate::atomic::read_regular(&cred_path).ok()
        } else {
            None
        };
        crate::atomic::write_secret(&cred_path, cred.expose())?;
        if let Err(e) = crate::atomic::write_secret(&cfg_path, &new_cfg) {
            // Report what the rollback actually did - if it also failed (e.g.
            // disk full broke both writes), saying "rolled back" would be a lie
            // about a half-swapped login.
            let msg = match prev_creds {
                Some(prev) => match crate::atomic::write_secret(&cred_path, &prev) {
                    Ok(()) => "apply aborted; credentials rolled back",
                    Err(_) => {
                        "apply aborted and the rollback FAILED - the login may be \
                         half-swapped; run `swapdex restore --tool claude` once the \
                         underlying problem (e.g. disk space) is fixed"
                    }
                },
                None => {
                    // Fresh install: nothing to roll back TO - remove the file
                    // we just wrote, or the "aborted" apply leaves new
                    // credentials next to an un-updated identity (half-swap).
                    match std::fs::remove_file(&cred_path) {
                        Ok(()) => "apply aborted; the just-written credentials were removed",
                        Err(_) => {
                            "apply aborted and cleanup FAILED - a credentials file was                              written without its identity; run `swapdex restore --tool                              claude` once the underlying problem is fixed"
                        }
                    }
                }
            };
            return Err(e.context(msg));
        }
        Ok(())
    }

    fn identity(&self, paths: &Paths) -> Result<Option<Account>> {
        let cred = paths.claude_credentials();
        if !cred.exists() {
            return Ok(None);
        }
        let creds: Value = serde_json::from_slice(&crate::atomic::read_regular(&cred)?)
            .context("parse .credentials.json")?;
        let expires_at = creds["claudeAiOauth"]["expiresAt"].as_i64();
        let tier = creds["claudeAiOauth"]["subscriptionType"]
            .as_str()
            .map(|s| s.to_string());
        let cfg_path = paths.claude_config_json();
        let cfg: Value = if cfg_path.exists() {
            crate::atomic::read_regular(&cfg_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let oauth = &cfg["oauthAccount"];
        Ok(Some(Account {
            tool: "claude-code",
            account_id: oauth["accountUuid"].as_str().unwrap_or("").to_string(),
            display: oauth["displayName"]
                .as_str()
                .unwrap_or("Claude account")
                .to_string(),
            email: oauth["emailAddress"].as_str().map(|s| s.to_string()),
            tier,
            expires_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use serde_json::json;

    fn seed_claude(p: &Paths, acct: &str, email: &str) {
        std::fs::create_dir_all(p.claude_credentials().parent().unwrap()).unwrap();
        std::fs::write(
            p.claude_credentials(),
            serde_json::to_vec(&json!({"claudeAiOauth": {
                "accessToken": "AT-SENTINEL", "refreshToken": "RT-SENTINEL",
                "expiresAt": 9999999999999i64, "scopes": ["x"],
                "subscriptionType": "max", "rateLimitTier": "default"}}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            p.claude_config_json(),
            serde_json::to_vec(&json!({
                "projects": {"/home/x/proj": {"trust": true}},
                "mcpServers": {"prodex": {"command": "prodex"}},
                "theme": "dark",
                "oauthAccount": {"accountUuid": acct, "emailAddress": email,
                                 "displayName": "Work", "userRateLimitTier": "max"}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    // C1: if the .claude.json write fails after credentials are written, the
    // credentials must roll back so the login is never left half-swapped.
    #[test]
    fn apply_rolls_back_credentials_when_config_write_fails() {
        let a = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(a.path());
        seed_claude(&pa, "uuid-A", "a@x.com");
        let snap = Claude.capture(&pa).unwrap();

        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        seed_claude(&pb, "uuid-B", "b@y.com");
        let orig_creds = std::fs::read(pb.claude_credentials()).unwrap();

        // Block the config write: plant a directory at its atomic temp path so
        // the write fails AFTER the credentials have already been swapped.
        let cfg = pb.claude_config_json();
        let tmp = cfg.parent().unwrap().join(format!(
            ".{}.swapdex.tmp",
            cfg.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::create_dir(&tmp).unwrap();

        assert!(Claude.apply(&pb, &snap).is_err(), "config write must fail");
        assert_eq!(
            std::fs::read(pb.claude_credentials()).unwrap(),
            orig_creds,
            "credentials must roll back to B - never half-swapped"
        );
    }

    #[test]
    fn apply_swaps_only_oauthaccount_and_preserves_siblings() {
        let a = tempfile::tempdir().unwrap();
        let pa = Paths::rooted(a.path());
        seed_claude(&pa, "uuid-A", "a@x.com");
        let snap = Claude.capture(&pa).unwrap();

        // A DIFFERENT existing config on machine B with its own projects/mcp.
        let b = tempfile::tempdir().unwrap();
        let pb = Paths::rooted(b.path());
        seed_claude(&pb, "uuid-B", "b@y.com");
        std::fs::write(
            pb.claude_config_json(),
            serde_json::to_vec(&json!({
                "projects": {"/keep/me": {"trust": true}},
                "mcpServers": {"sessionwiki": {"command": "sessionwiki"}},
                "theme": "light",
                "oauthAccount": {"accountUuid": "uuid-B", "emailAddress": "b@y.com"}
            }))
            .unwrap(),
        )
        .unwrap();

        Claude.apply(&pb, &snap).unwrap();

        let after: Value =
            serde_json::from_slice(&std::fs::read(pb.claude_config_json()).unwrap()).unwrap();
        // oauthAccount switched to A...
        assert_eq!(after["oauthAccount"]["accountUuid"], "uuid-A");
        assert_eq!(after["oauthAccount"]["emailAddress"], "a@x.com");
        // ...but B's projects/mcp/theme are INTACT (the A1 guarantee).
        assert_eq!(after["projects"]["/keep/me"]["trust"], true);
        assert_eq!(after["mcpServers"]["sessionwiki"]["command"], "sessionwiki");
        assert_eq!(after["theme"], "light");
        let creds: Value =
            serde_json::from_slice(&std::fs::read(pb.claude_credentials()).unwrap()).unwrap();
        assert_eq!(creds["claudeAiOauth"]["subscriptionType"], "max");

        let id = Claude.identity(&pb).unwrap().unwrap();
        assert_eq!(id.account_id, "uuid-A");
        assert_eq!(id.email.as_deref(), Some("a@x.com"));
    }
}
