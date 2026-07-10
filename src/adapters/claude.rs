use super::{Account, AuthTool, Snapshot};
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Claude;

/// Absolute path to `security`: Claude Code creates its Keychain item by
/// shelling out to `/usr/bin/security`, so the item's ACL trusts THAT binary.
/// Using the same absolute path means swapdex is the same trusted app - no
/// "allow / Always Allow" prompt - and a PATH-injected `security` can't steal
/// tokens.
const SECURITY: &str = "/usr/bin/security";

/// The prefix of the Keychain service Claude Code stores its OAuth token under.
const KEYCHAIN_PREFIX: &str = "Claude Code-credentials";

/// The Keychain `acct` attribute Claude Code uses: `$USER`, else the OS
/// username, else a fixed fallback - matching Claude Code's own account fn.
fn keychain_account_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "claude-code-user".into())
}

/// Pull an attribute value out of a `security` output line, e.g. the `svce`
/// (service) or `acct` printed as `    "svce"<blob>="<value>"`.
fn parse_kc_attr(line: &str, attr: &str) -> Option<String> {
    let needle = format!("\"{attr}\"");
    let rest = line.split(&needle).nth(1)?;
    let after = rest.split("=\"").nth(1)?;
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// sha256 as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The exact Keychain service Claude Code uses, computed the SAME way Claude
/// Code does: "Claude Code-credentials" plus, when CLAUDE_SECURESTORAGE_CONFIG_DIR
/// / CLAUDE_CONFIG_DIR is set, "-" + first 8 hex of sha256(that dir). Correct
/// even when the config dir is set (the case hardcoding tools get wrong).
/// Existence is verified; a dump-keychain scan is the fallback.
fn keychain_service() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    // DISCOVERY FIRST. swapdex may not see the same CLAUDE_CONFIG_DIR the user
    // launches `claude` with (e.g. it's set only in a shell alias), so the
    // computed name could be the bare prefix while Claude's real item is
    // suffixed. Discovery scans the keychain and prefers the suffixed item -
    // Claude's real credential - over a bare-prefix stray. Only if discovery
    // finds nothing do we fall back to the computed name.
    if let Some(svc) = discover_keychain_service() {
        return Some(svc);
    }
    let computed = match std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        Ok(t) if t.is_empty() => KEYCHAIN_PREFIX.to_string(),
        Ok(t) => format!("{KEYCHAIN_PREFIX}-{}", &sha256_hex(t.as_bytes())[..8]),
        Err(_) => match std::env::var("CLAUDE_CONFIG_DIR") {
            Ok(d) if !d.is_empty() => {
                format!("{KEYCHAIN_PREFIX}-{}", &sha256_hex(d.as_bytes())[..8])
            }
            _ => KEYCHAIN_PREFIX.to_string(),
        },
    };
    keychain_item_exists(&computed).then_some(computed)
}

/// True if a Keychain item with this service (+ account) exists, via an
/// attribute-only lookup (no `-w`, so no ACL prompt).
fn keychain_item_exists(service: &str) -> bool {
    std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            &keychain_account_name(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fallback discovery: dump the login keychain's ATTRIBUTES (no `-d`, no
/// prompt) and find a service starting with the prefix, preferring a suffixed
/// entry over the bare prefix.
fn discover_keychain_service() -> Option<String> {
    let out = std::process::Command::new(SECURITY)
        .arg("dump-keychain")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut best: Option<String> = None;
    for line in text.lines() {
        if let Some(svc) = parse_kc_attr(line, "svce") {
            if svc.starts_with(KEYCHAIN_PREFIX) {
                let suffixed = svc != KEYCHAIN_PREFIX;
                let best_bare = best.as_deref() == Some(KEYCHAIN_PREFIX);
                if best.is_none() || (suffixed && best_bare) {
                    best = Some(svc);
                }
            }
        }
    }
    best
}

/// Every Keychain service name starting with the Claude prefix (attribute dump
/// only - no secret, no prompt). For `doctor`: reveals strays and the real
/// item so a service-name mismatch (switch writes A, Claude reads B) is caught.
#[cfg(target_os = "macos")]
fn all_claude_services() -> Vec<String> {
    let Ok(out) = std::process::Command::new(SECURITY)
        .arg("dump-keychain")
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(svc) = parse_kc_attr(line, "svce") {
            if svc.starts_with(KEYCHAIN_PREFIX) && !v.contains(&svc) {
                v.push(svc);
            }
        }
    }
    v
}

/// macOS Keychain reality for `doctor`: which Claude items exist vs the one
/// swapdex reads/writes. `None` off macOS (Claude is file-based there).
pub(crate) struct KeychainDiag {
    pub found: Vec<String>,
    pub target: Option<String>,
    pub config_dir: Option<String>,
}

pub(crate) fn keychain_diagnostic() -> Option<KeychainDiag> {
    #[cfg(target_os = "macos")]
    {
        let config_dir = std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("CLAUDE_CONFIG_DIR")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
        Some(KeychainDiag {
            found: all_claude_services(),
            target: keychain_service(),
            config_dir,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Read the Claude token JSON from the macOS Keychain (`{"claudeAiOauth":...}`).
fn keychain_read() -> Option<Vec<u8>> {
    let service = keychain_service()?;
    let out = std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &keychain_account_name(),
            "-w",
        ])
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

/// Write the Claude token into the Keychain, updating Claude's own item. The
/// token is passed as HEX over `security -i` stdin (via `-X`), never in argv,
/// so it can't be read from `ps`.
fn keychain_write(value: &[u8]) -> Result<()> {
    use std::io::Write;
    let service = keychain_service().unwrap_or_else(|| KEYCHAIN_PREFIX.to_string());
    let acct = keychain_account_name();
    let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
    let cmd = format!("add-generic-password -U -a \"{acct}\" -s \"{service}\" -X {hex}\n");
    let mut child = std::process::Command::new(SECURITY)
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("run `/usr/bin/security -i`")?;
    child
        .stdin
        .as_mut()
        .context("security stdin")?
        .write_all(cmd.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "Keychain write failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Remove Claude's Keychain item so `claude` prompts a FRESH sign-in during the
/// add-a-new-account flow. No-op off macOS or when there is no item.
pub(crate) fn keychain_delete() {
    let Some(service) = keychain_service() else {
        return;
    };
    let acct = keychain_account_name();
    let _ = std::process::Command::new(SECURITY)
        .args(["delete-generic-password", "-s", &service, "-a", &acct])
        .output();
    if service != KEYCHAIN_PREFIX {
        let _ = std::process::Command::new(SECURITY)
            .args([
                "delete-generic-password",
                "-s",
                KEYCHAIN_PREFIX,
                "-a",
                &acct,
            ])
            .output();
    }
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

/// The LIVE Claude credential JSON (file or Keychain) - the active account's
/// token, kept fresh by Claude Code. Used only by `swapdex quota` to read the
/// active account's remaining quota; never leaves the machine except as that
/// account's own bearer token to Anthropic's usage endpoint.
pub(crate) fn live_credentials(paths: &Paths) -> Option<Vec<u8>> {
    cred_read(paths)
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
    fn sha256_hex_matches_known_vector() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(&super::sha256_hex(b"abc")[..8], "ba7816bf");
    }

    #[test]
    fn keychain_attr_parser_reads_svce_and_acct() {
        assert_eq!(
            super::parse_kc_attr("    \"acct\"<blob>=\"bsgong\"", "acct").as_deref(),
            Some("bsgong")
        );
        assert_eq!(
            super::parse_kc_attr(
                "    \"svce\"<blob>=\"Claude Code-credentials-5953ba74\"",
                "svce"
            )
            .as_deref(),
            Some("Claude Code-credentials-5953ba74")
        );
        assert_eq!(super::parse_kc_attr("no attr here", "acct"), None);
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
