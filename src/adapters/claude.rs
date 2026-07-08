use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Claude;

/// The macOS login Keychain service Claude Code stores its OAuth token under.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Read the Claude token JSON from the macOS Keychain (`{"claudeAiOauth":...}`).
/// None on non-macOS or when there is no such item.
fn keychain_read() -> Option<Vec<u8>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut v = out.stdout;
    while v.last() == Some(&b'\n') {
        v.pop();
    }
    (!v.is_empty()).then_some(v)
}

/// The `acct` attribute of the existing Keychain item, so a write updates the
/// SAME item Claude Code reads rather than creating a sibling.
fn keychain_account() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .output()
        .ok()?;
    parse_keychain_acct(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the `acct` attribute out of `security find-generic-password` output,
/// which prints it as a line like `    "acct"<blob>="bsgong"`.
fn parse_keychain_acct(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.split("\"acct\"").nth(1) {
            if let Some(after) = rest.split("=\"").nth(1) {
                if let Some(end) = after.find('"') {
                    return Some(after[..end].to_string());
                }
            }
        }
    }
    None
}

/// Write the Claude token JSON into the macOS Keychain (updating the existing
/// item). `-U` updates in place; the account is preserved when known.
fn keychain_write(value: &[u8]) -> Result<()> {
    let val = std::str::from_utf8(value).context("keychain value is not UTF-8")?;
    let acct = keychain_account()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "swapdex".into());
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            &acct,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            val,
        ])
        .status()
        .context("run `security add-generic-password`")?;
    if !status.success() {
        bail!("`security add-generic-password` failed (Keychain write)");
    }
    Ok(())
}

/// Remove the macOS Keychain item so `claude` prompts a FRESH sign-in during
/// the add-a-new-account flow. No-op on non-macOS.
pub(crate) fn keychain_delete() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", KEYCHAIN_SERVICE])
        .output();
}

/// The Claude token JSON from wherever it lives: the file when present,
/// otherwise the macOS Keychain.
fn cred_read(paths: &Paths) -> Option<Vec<u8>> {
    let f = paths.claude_credentials();
    if f.exists() {
        crate::atomic::read_regular(&f).ok()
    } else {
        keychain_read()
    }
}

/// True if a Claude login exists at all (file or Keychain).
fn cred_present(paths: &Paths) -> bool {
    paths.claude_credentials().exists() || keychain_read().is_some()
}

impl AuthTool for Claude {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn present(&self, paths: &Paths) -> bool {
        cred_present(paths)
    }

    fn capture(&self, paths: &Paths) -> Result<Snapshot> {
        let Some(cred_bytes) = cred_read(paths) else {
            bail!("not logged in to Claude Code");
        };
        serde_json::from_slice::<Value>(&cred_bytes)
            .context("the Claude credential is not valid JSON")?;
        // Extract ONLY the oauthAccount block from .claude.json, never the file.
        // Right after a fresh CLI login .claude.json may not exist yet; treat a
        // missing config as an absent oauthAccount rather than failing capture.
        let cfg_path = paths.claude_config_json();
        let cfg: Value = if cfg_path.exists() {
            serde_json::from_slice(&crate::atomic::read_regular(&cfg_path)?).context(
                "your LIVE ~/.claude.json is corrupt (not the profile snapshot) - \
                     repair or remove that file, then retry; removing loses local \
                     settings like project trust",
            )?
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
            serde_json::from_slice(&crate::atomic::read_regular(&cfg_path)?).context(
                "your LIVE ~/.claude.json is corrupt (not the profile snapshot) - \
                     repair or remove that file, then retry; removing loses local \
                     settings like project trust",
            )?
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
        // Three writes, both-or-neither: the credential FILE, the macOS
        // Keychain (Claude Code reads its token from there), and the config
        // file's oauthAccount. Snapshot the previous state of each so any
        // failure rolls ALL of them back - the login is never half-swapped.
        let cred_path = paths.claude_credentials();
        let macos = cfg!(target_os = "macos");
        let prev_file = if cred_path.exists() {
            crate::atomic::read_regular(&cred_path).ok()
        } else {
            None
        };
        let prev_kc = if macos { keychain_read() } else { None };

        let restore_file = |prev: &Option<Vec<u8>>| match prev {
            Some(p) => crate::atomic::write_secret(&cred_path, p).is_ok(),
            None => std::fs::remove_file(&cred_path).is_ok() || !cred_path.exists(),
        };

        // 1) credential file (keeps Claude working on Linux, and on macOS
        //    installs that also read the file).
        crate::atomic::write_secret(&cred_path, cred.expose())?;
        // 2) macOS Keychain - the source of truth for Claude on macOS.
        if macos {
            if let Err(e) = keychain_write(cred.expose()) {
                restore_file(&prev_file);
                return Err(e.context("apply aborted; credential file rolled back"));
            }
        }
        // 3) config oauthAccount.
        if let Err(e) = crate::atomic::write_secret(&cfg_path, &new_cfg) {
            let f_ok = restore_file(&prev_file);
            let k_ok = if macos {
                match &prev_kc {
                    Some(p) => keychain_write(p).is_ok(),
                    None => true, // nothing prior; leave the new token
                }
            } else {
                true
            };
            let msg = if f_ok && k_ok {
                "apply aborted; the credential change was rolled back"
            } else {
                "apply aborted and the rollback FAILED - the login may be half-swapped; \
                 run `swapdex restore --tool claude` once the underlying problem is fixed"
            };
            return Err(e.context(msg));
        }
        Ok(())
    }

    fn identity(&self, paths: &Paths) -> Result<Option<Account>> {
        // The token comes from the file or the macOS Keychain; the identity
        // (email/uuid) is always in .claude.json.
        let Some(cred_bytes) = cred_read(paths) else {
            return Ok(None);
        };
        let creds: Value = serde_json::from_slice(&cred_bytes)
            .context("the Claude credential is not valid JSON")?;
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

    #[test]
    fn keychain_acct_parser_reads_the_security_output() {
        // A realistic `security find-generic-password` dump.
        let sample = r#"keychain: "/Users/bsgong/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    0x00000007 <blob>="Claude Code-credentials"
    "acct"<blob>="bsgong"
    "svce"<blob>="Claude Code-credentials"
"#;
        assert_eq!(
            super::parse_keychain_acct(sample).as_deref(),
            Some("bsgong")
        );
        assert_eq!(super::parse_keychain_acct("no acct here"), None);
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
