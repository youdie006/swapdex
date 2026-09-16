//! Read a usable access-token snapshot from the store owned by a running native
//! Claude Code or Codex process.
//!
//! This module never refreshes or copies a native credential. A candidate is
//! returned only when its stable account identity exactly matches the selected
//! slot, its files remain unchanged across the credential read, and its access
//! token has a known usable deadline.

use crate::paths::Paths;
use crate::secret::Secret;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Non-secret identity used to prove that a native login is the selected one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginIdentity {
    Claude {
        account_uuid: String,
        organization_uuid: String,
    },
    Codex {
        subject: String,
        workspace_id: String,
    },
}

/// One immutable, usable access-token snapshot from an actual native owner.
///
/// Deliberately has no `Debug`: the token's [`Secret`] wrapper is redacted, but
/// keeping the aggregate non-debuggable makes accidental diagnostic expansion
/// harder. No refresh token leaves the resolver.
pub struct LiveLogin {
    pub access_token: Secret,
    pub expires_at_ms: i64,
    pub provider_account_id: Option<String>,
    pub source_dir: PathBuf,
    pub identity_path: PathBuf,
    pub identity: LoginIdentity,
    /// A rejection recorded for this exact Codex refresh-token generation.
    /// The access token may remain usable; callers can surface the warning
    /// without claiming that native ownership renewed it successfully.
    pub refresh_rejected_at_ms: Option<i64>,
}

fn regular_file(path: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    crate::atomic::read_regular(path).ok()
}

fn jwt_claim(token: &str, claim: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get(claim)?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Parse the stable provider identity from one already-read credential blob.
///
/// Callers that also select a bearer from this blob can bind both decisions to
/// the same filesystem generation instead of racing a second path read.
pub(crate) fn identity_from_credential(bytes: &[u8], tool: &str) -> Option<LoginIdentity> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    match tool {
        "claude-code" => {
            let oauth = value.get("oauthAccount")?;
            let account_uuid = oauth
                .get("accountUuid")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            let organization_uuid = oauth
                .get("organizationUuid")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            Some(LoginIdentity::Claude {
                account_uuid: account_uuid.to_string(),
                organization_uuid: organization_uuid.to_string(),
            })
        }
        "codex" => {
            let tokens = value.get("tokens")?;
            let subject = jwt_claim(tokens.get("id_token")?.as_str()?, "sub")?;
            let workspace_id = tokens
                .get("account_id")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            Some(LoginIdentity::Codex {
                subject,
                workspace_id: workspace_id.to_string(),
            })
        }
        _ => None,
    }
}

fn selected_identity_path(dir: &Path, tool: &str) -> Option<PathBuf> {
    match tool {
        "claude-code" => Some(dir.join(".claude.json")),
        "codex" => Some(dir.join("auth.json")),
        _ => None,
    }
}

fn login_from_credential(
    credential: &[u8],
    identity: LoginIdentity,
    source_dir: PathBuf,
    identity_path: PathBuf,
    tool: &str,
    now_ms: i64,
) -> Option<LiveLogin> {
    let value: Value = serde_json::from_slice(credential).ok()?;
    match tool {
        "claude-code" => {
            let oauth = value.get("claudeAiOauth")?;
            let access = oauth
                .get("accessToken")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            let expires_at_ms = oauth.get("expiresAt")?.as_i64()?;
            if expires_at_ms <= now_ms.checked_add(60_000)? {
                return None;
            }
            let provider_account_id = match &identity {
                LoginIdentity::Claude { account_uuid, .. } => account_uuid.clone(),
                LoginIdentity::Codex { .. } => return None,
            };
            Some(LiveLogin {
                access_token: Secret::new(access.as_bytes().to_vec()),
                expires_at_ms,
                provider_account_id: Some(provider_account_id),
                source_dir,
                identity_path,
                identity,
                refresh_rejected_at_ms: None,
            })
        }
        "codex" => {
            let tokens = value.get("tokens")?;
            let access = tokens
                .get("access_token")?
                .as_str()
                .filter(|value| !value.is_empty())?;
            let expires_at_ms = crate::proxy::codex::jwt_expiry(access)?.checked_mul(1000)?;
            if expires_at_ms <= now_ms {
                return None;
            }
            let provider_account_id = tokens
                .get("account_id")?
                .as_str()
                .filter(|value| !value.is_empty())?
                .to_string();
            let fingerprint =
                crate::refresh_health::codex_credential_fingerprint_from_blob(credential);
            let refresh_rejected_at_ms = fingerprint.as_deref().and_then(|fingerprint| {
                crate::refresh_health::codex_rejection_for_fingerprint(&source_dir, fingerprint)
            });
            Some(LiveLogin {
                access_token: Secret::new(access.as_bytes().to_vec()),
                expires_at_ms,
                provider_account_id: Some(provider_account_id),
                source_dir,
                identity_path,
                identity,
                refresh_rejected_at_ms,
            })
        }
        _ => None,
    }
}

fn same_generation(left: &LiveLogin, right: &LiveLogin) -> bool {
    left.identity == right.identity
        && left.access_token.expose() == right.access_token.expose()
        && left.expires_at_ms == right.expires_at_ms
        && left.provider_account_id == right.provider_account_id
}

/// Resolve a usable access token owned by an actual running native process.
///
/// `None` is deliberately broad and conservative: no native process, invalid
/// or changing files, identity mismatch, unknown/insufficient lifetime, or
/// multiple differing usable generations. Existing refresh ownership guards
/// remain responsible for blocking refresh when a native holder exists but no
/// usable snapshot can be established.
pub fn resolve(paths: &Paths, dir: &Path, tool: &str, now_ms: i64) -> Option<LiveLogin> {
    let selected_path = selected_identity_path(dir, tool)?;
    if !crate::proc::native_path_allowed(paths, &selected_path) {
        return None;
    }
    let selected_before = regular_file(&selected_path)?;
    let selected_identity = identity_from_credential(&selected_before, tool)?;

    let mut processes = crate::proc::running_native_login_processes(paths, tool);
    processes.sort_by(|left, right| {
        left.source_dir
            .cmp(&right.source_dir)
            .then_with(|| left.identity_path.cmp(&right.identity_path))
    });
    processes.dedup();

    let mut winner: Option<LiveLogin> = None;
    for process in processes {
        let Some(source_before) = regular_file(&process.identity_path) else {
            continue;
        };
        let Some(source_identity) = identity_from_credential(&source_before, tool) else {
            continue;
        };
        if source_identity != selected_identity {
            continue;
        }

        let credential = match tool {
            "claude-code" => crate::adapters::claude::native_credentials(
                paths,
                &process.source_dir,
                process.claude_keychain_key.as_deref(),
            ),
            "codex" => regular_file(&process.source_dir.join("auth.json")),
            _ => return None,
        };
        let Some(credential) = credential else {
            continue;
        };

        // Identity names the credential owner. Both the selected and source
        // identity must remain byte-for-byte stable across the credential read.
        if regular_file(&selected_path).as_deref() != Some(selected_before.as_slice()) {
            return None;
        }
        if regular_file(&process.identity_path).as_deref() != Some(source_before.as_slice()) {
            continue;
        }
        // Codex keeps identity and credential in one file, so the captured blob
        // itself must be the same generation bracketed by those two reads.
        if tool == "codex" && credential != source_before {
            continue;
        }

        let Some(candidate) = login_from_credential(
            &credential,
            source_identity,
            process.source_dir,
            process.identity_path,
            tool,
            now_ms,
        ) else {
            continue;
        };
        match &mut winner {
            None => winner = Some(candidate),
            Some(current) if same_generation(current, &candidate) => {
                let refresh_rejected_at_ms = current
                    .refresh_rejected_at_ms
                    .max(candidate.refresh_rejected_at_ms);
                if candidate.source_dir == dir && current.source_dir != dir {
                    *current = candidate;
                }
                current.refresh_rejected_at_ms = refresh_rejected_at_ms;
            }
            Some(_) => return None,
        }
    }
    winner
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use crate::paths::Paths;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};

    const NOW_MS: i64 = 1_800_000_000_000;

    struct NativeChild(Child);

    impl NativeChild {
        fn spawn(
            root: &Path,
            instance: &str,
            comm: &str,
            home: &Path,
            env: &[(&str, &Path)],
            text_env: &[(&str, &str)],
        ) -> Self {
            let bin_dir = root.join(instance);
            std::fs::create_dir_all(&bin_dir).unwrap();
            let bin = bin_dir.join(comm);
            let sleep = if Path::new("/bin/sleep").exists() {
                "/bin/sleep"
            } else {
                "/usr/bin/sleep"
            };
            std::os::unix::fs::symlink(sleep, &bin).unwrap();
            let mut command = Command::new(&bin);
            command.arg("30").env_clear().env("HOME", home);
            for (key, value) in env {
                command.env(key, value);
            }
            for (key, value) in text_env {
                command.env(key, value);
            }
            let mut child = command.spawn().unwrap();
            let proc_dir = PathBuf::from(format!("/proc/{}", child.id()));
            for _ in 0..200 {
                let ready = std::fs::read_to_string(proc_dir.join("comm"))
                    .is_ok_and(|name| name.trim() == comm)
                    && std::fs::read(proc_dir.join("environ")).is_ok_and(|b| !b.is_empty());
                if ready {
                    return Self(child);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("native test process {comm} did not become readable");
        }
    }

    impl Drop for NativeChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn claude_identity(dir: &Path, account: &str, organization: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            serde_json::to_vec(&serde_json::json!({
                "oauthAccount": {
                    "accountUuid": account,
                    "organizationUuid": organization,
                    "emailAddress": "owner@example.com"
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn claude_default_identity(home: &Path, account: &str, organization: &str) {
        std::fs::write(
            home.join(".claude.json"),
            serde_json::to_vec(&serde_json::json!({
                "oauthAccount": {
                    "accountUuid": account,
                    "organizationUuid": organization,
                    "emailAddress": "owner@example.com"
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn claude_credential(dir: &Path, access: &str, expires_at: i64) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(".credentials.json"),
            serde_json::to_vec(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": access,
                    "refreshToken": "never-return-this",
                    "expiresAt": expires_at
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn jwt(claims: serde_json::Value) -> String {
        format!(
            "{}.{}.sig",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    fn codex_auth(dir: &Path, subject: &str, workspace: &str, access: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            serde_json::to_vec(&serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": jwt(serde_json::json!({"sub": subject})),
                    "access_token": access,
                    "refresh_token": "never-return-this",
                    "account_id": workspace
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn codex_access(exp_secs: i64) -> String {
        jwt(serde_json::json!({"exp": exp_secs}))
    }

    #[test]
    fn stale_selected_claude_uses_fresh_running_default_login() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        claude_identity(&selected, "account-a", "org-a");
        claude_credential(&selected, "STALE", 1);

        let native_dir = root.path().join(".claude");
        claude_default_identity(root.path(), "account-a", "org-a");
        claude_credential(&native_dir, "FRESH", NOW_MS + 3_600_000);
        // A default Claude reads HOME/.claude.json. This similarly named file
        // under its config directory must never choose the identity.
        claude_identity(&native_dir, "wrong-nested-account", "wrong-nested-org");
        let _child =
            NativeChild::spawn(root.path(), "bin-default", "claude", root.path(), &[], &[]);

        let login = super::resolve(&paths, &selected, "claude-code", NOW_MS)
            .expect("the native login is usable and is the selected account");
        assert_eq!(login.access_token.expose(), b"FRESH");
        assert_eq!(login.expires_at_ms, NOW_MS + 3_600_000);
        assert_eq!(
            login.provider_account_id.as_deref(),
            Some("account-a"),
            "request metadata uses the captured native identity"
        );
        assert_eq!(login.source_dir, native_dir);
        assert_eq!(login.identity_path, root.path().join(".claude.json"));
        assert_eq!(
            login.identity,
            super::LoginIdentity::Claude {
                account_uuid: "account-a".into(),
                organization_uuid: "org-a".into()
            }
        );
        assert!(!format!("{}", login.access_token).contains("FRESH"));
    }

    #[test]
    fn claude_requires_both_exact_account_and_nonempty_organization() {
        for (account, organization) in [
            ("account-b", "org-a"),
            ("account-a", "org-b"),
            ("account-a", ""),
        ] {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::rooted(root.path());
            let selected = root.path().join("selected");
            claude_identity(&selected, "account-a", "org-a");
            claude_default_identity(root.path(), account, organization);
            claude_credential(&root.path().join(".claude"), "OTHER", NOW_MS + 3_600_000);
            let _child =
                NativeChild::spawn(root.path(), "bin-mismatch", "claude", root.path(), &[], &[]);
            assert!(
                super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none(),
                "{account:?}/{organization:?} is not the selected identity"
            );
        }
    }

    #[test]
    fn codex_workspace_without_the_same_subject_is_not_the_same_login() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        codex_auth(
            &selected,
            "user-a",
            "workspace-shared",
            &codex_access(NOW_MS / 1000 + 3_600),
        );
        codex_auth(
            &root.path().join(".codex"),
            "user-b",
            "workspace-shared",
            &codex_access(NOW_MS / 1000 + 3_600),
        );
        let _child = NativeChild::spawn(
            root.path(),
            "bin-codex-other-user",
            "codex",
            root.path(),
            &[],
            &[],
        );

        assert!(super::resolve(&paths, &selected, "codex", NOW_MS).is_none());
    }

    #[test]
    fn codex_returns_the_live_token_and_provider_account_id_together() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        let access = codex_access(NOW_MS / 1000 + 3_600);
        codex_auth(&selected, "user-a", "workspace-a", "ignored.selected.token");
        codex_auth(
            &root.path().join(".codex"),
            "user-a",
            "workspace-a",
            &access,
        );
        let _child = NativeChild::spawn(root.path(), "bin-codex", "codex", root.path(), &[], &[]);

        let login = super::resolve(&paths, &selected, "codex", NOW_MS).expect("live codex");
        assert_eq!(login.access_token.expose(), access.as_bytes());
        assert_eq!(login.expires_at_ms, NOW_MS + 3_600_000);
        assert_eq!(login.provider_account_id.as_deref(), Some("workspace-a"));
        assert_eq!(
            login.identity,
            super::LoginIdentity::Codex {
                subject: "user-a".into(),
                workspace_id: "workspace-a".into()
            }
        );
    }

    #[test]
    fn codex_rejection_is_bound_to_the_captured_native_generation() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        let native = root.path().join(".codex");
        let access = codex_access(NOW_MS / 1000 + 3_600);
        codex_auth(&selected, "user-a", "workspace-a", "unused");
        codex_auth(&native, "user-a", "workspace-a", &access);
        let fingerprint = crate::refresh_health::codex_credential_fingerprint(&native).unwrap();
        crate::refresh_health::record_codex_rejection(&native, &fingerprint, NOW_MS - 1).unwrap();
        let _child = NativeChild::spawn(
            root.path(),
            "bin-codex-rejected",
            "codex",
            root.path(),
            &[],
            &[],
        );

        let login = super::resolve(&paths, &selected, "codex", NOW_MS)
            .expect("a valid access token remains usable");
        assert_eq!(login.refresh_rejected_at_ms, Some(NOW_MS - 1));
    }

    #[test]
    fn inherited_slot_environment_without_native_comm_is_not_authority() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        let inherited = root.path().join("inherited");
        claude_identity(&selected, "account-a", "org-a");
        claude_identity(&inherited, "account-a", "org-a");
        claude_credential(&inherited, "CHILD", NOW_MS + 3_600_000);
        let _child = NativeChild::spawn(
            root.path(),
            "bin-child",
            "session-child",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", inherited.as_path())],
            &[],
        );

        assert!(super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none());
    }

    #[test]
    fn alternate_environment_credential_disqualifies_a_native_process() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        claude_identity(&selected, "account-a", "org-a");
        claude_default_identity(root.path(), "account-a", "org-a");
        claude_credential(
            &root.path().join(".claude"),
            "OAUTH-ON-DISK",
            NOW_MS + 3_600_000,
        );
        let _child = NativeChild::spawn(
            root.path(),
            "bin-env-auth",
            "claude",
            root.path(),
            &[],
            &[("ANTHROPIC_API_KEY", "environment-wins")],
        );

        assert!(super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none());
    }

    #[test]
    fn differing_valid_native_generations_are_ambiguous() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        let first = root.path().join("native-a");
        let second = root.path().join("native-b");
        claude_identity(&selected, "account-a", "org-a");
        for (dir, token) in [(&first, "FIRST"), (&second, "SECOND")] {
            claude_identity(dir, "account-a", "org-a");
            claude_credential(dir, token, NOW_MS + 3_600_000);
        }
        let _first = NativeChild::spawn(
            root.path(),
            "bin-ambiguous-a",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", first.as_path())],
            &[],
        );
        let _second = NativeChild::spawn(
            root.path(),
            "bin-ambiguous-b",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", second.as_path())],
            &[],
        );

        assert!(super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none());
    }

    #[test]
    fn unrelated_unreadable_and_expired_processes_do_not_hide_a_verified_source() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        let unreadable = root.path().join("a-unreadable");
        let expired = root.path().join("b-expired");
        let current = root.path().join("c-current");
        claude_identity(&selected, "account-a", "org-a");
        std::fs::create_dir_all(&unreadable).unwrap();
        claude_identity(&expired, "account-a", "org-a");
        claude_credential(&expired, "EXPIRED", 1);
        claude_identity(&current, "account-a", "org-a");
        claude_credential(&current, "CURRENT", NOW_MS + 3_600_000);
        let _unreadable = NativeChild::spawn(
            root.path(),
            "bin-unreadable",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", unreadable.as_path())],
            &[],
        );
        let _expired = NativeChild::spawn(
            root.path(),
            "bin-expired-peer",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", expired.as_path())],
            &[],
        );
        let _current = NativeChild::spawn(
            root.path(),
            "bin-current",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", current.as_path())],
            &[],
        );

        let login = super::resolve(&paths, &selected, "claude-code", NOW_MS)
            .expect("only valid native candidates participate in ambiguity");
        assert_eq!(login.access_token.expose(), b"CURRENT");
    }

    #[test]
    fn duplicate_processes_with_the_same_access_and_identity_are_not_ambiguous() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        let first = root.path().join("native-a");
        let second = root.path().join("native-b");
        claude_identity(&selected, "account-a", "org-a");
        for dir in [&first, &second] {
            claude_identity(dir, "account-a", "org-a");
            claude_credential(dir, "SAME", NOW_MS + 3_600_000);
        }
        let _first = NativeChild::spawn(
            root.path(),
            "bin-same-a",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", first.as_path())],
            &[],
        );
        let _second = NativeChild::spawn(
            root.path(),
            "bin-same-b",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", second.as_path())],
            &[],
        );

        let login = super::resolve(&paths, &selected, "claude-code", NOW_MS)
            .expect("identical native snapshots agree");
        assert_eq!(login.access_token.expose(), b"SAME");
    }

    #[test]
    fn preferred_identical_codex_source_keeps_peer_rejection_timestamp() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let peer = root.path().join("a-native");
        let selected = root.path().join("z-selected");
        let access = codex_access(NOW_MS / 1000 + 3_600);
        for dir in [&peer, &selected] {
            codex_auth(dir, "user-a", "workspace-a", &access);
        }
        let fingerprint = crate::refresh_health::codex_credential_fingerprint(&peer).unwrap();
        crate::refresh_health::record_codex_rejection(&peer, &fingerprint, NOW_MS - 1).unwrap();
        let _peer = NativeChild::spawn(
            root.path(),
            "bin-rejected-peer",
            "codex",
            root.path(),
            &[("CODEX_HOME", peer.as_path())],
            &[],
        );
        let _selected = NativeChild::spawn(
            root.path(),
            "bin-preferred-selected",
            "codex",
            root.path(),
            &[("CODEX_HOME", selected.as_path())],
            &[],
        );

        let login = super::resolve(&paths, &selected, "codex", NOW_MS)
            .expect("identical Codex generations agree");
        assert_eq!(
            login.source_dir, selected,
            "the selected source is preferred"
        );
        assert_eq!(
            login.refresh_rejected_at_ms,
            Some(NOW_MS - 1),
            "the shared generation retains its newest rejection"
        );
    }

    #[test]
    fn the_selected_slot_itself_can_be_native_authority() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        claude_identity(&selected, "account-a", "org-a");
        claude_credential(&selected, "OWN", NOW_MS + 3_600_000);
        let _child = NativeChild::spawn(
            root.path(),
            "bin-own",
            "claude",
            root.path(),
            &[("CLAUDE_CONFIG_DIR", selected.as_path())],
            &[],
        );

        let login = super::resolve(&paths, &selected, "claude-code", NOW_MS).expect("own slot");
        assert_eq!(login.access_token.expose(), b"OWN");
        assert_eq!(login.identity_path, selected.join(".claude.json"));
    }

    #[test]
    fn expired_or_unknown_deadlines_are_never_returned() {
        for expiry in [NOW_MS + 60_000, NOW_MS + 59_999] {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::rooted(root.path());
            let selected = root.path().join("selected");
            claude_identity(&selected, "account-a", "org-a");
            claude_default_identity(root.path(), "account-a", "org-a");
            claude_credential(&root.path().join(".claude"), "LATE", expiry);
            let _child =
                NativeChild::spawn(root.path(), "bin-expired", "claude", root.path(), &[], &[]);
            assert!(super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none());
        }

        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let selected = root.path().join("selected");
        codex_auth(&selected, "user-a", "workspace-a", "unused");
        codex_auth(
            &root.path().join(".codex"),
            "user-a",
            "workspace-a",
            "not-a-jwt",
        );
        let _child = NativeChild::spawn(
            root.path(),
            "bin-unknown-expiry",
            "codex",
            root.path(),
            &[],
            &[],
        );
        assert!(super::resolve(&paths, &selected, "codex", NOW_MS).is_none());
    }

    #[test]
    fn identity_and_credential_symlinks_are_refused() {
        for link_identity in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::rooted(root.path());
            let selected = root.path().join("selected");
            let native_dir = root.path().join("native");
            claude_identity(&selected, "account-a", "org-a");
            std::fs::create_dir_all(&native_dir).unwrap();
            if link_identity {
                let real = root.path().join("real-identity.json");
                std::fs::write(
                    &real,
                    br#"{"oauthAccount":{"accountUuid":"account-a","organizationUuid":"org-a"}}"#,
                )
                .unwrap();
                std::os::unix::fs::symlink(&real, native_dir.join(".claude.json")).unwrap();
                claude_credential(&native_dir, "LINKED", NOW_MS + 3_600_000);
            } else {
                claude_identity(&native_dir, "account-a", "org-a");
                let real = root.path().join("real-credential.json");
                std::fs::write(
                    &real,
                    format!(
                        r#"{{"claudeAiOauth":{{"accessToken":"LINKED","expiresAt":{}}}}}"#,
                        NOW_MS + 3_600_000
                    ),
                )
                .unwrap();
                std::os::unix::fs::symlink(&real, native_dir.join(".credentials.json")).unwrap();
            }
            let _child = NativeChild::spawn(
                root.path(),
                "bin-symlink",
                "claude",
                root.path(),
                &[("CLAUDE_CONFIG_DIR", native_dir.as_path())],
                &[],
            );
            assert!(super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none());
        }
    }

    #[test]
    fn rooted_paths_never_follow_a_native_process_to_a_foreign_home() {
        let sandbox = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(sandbox.path());
        let selected = sandbox.path().join("selected");
        claude_identity(&selected, "account-a", "org-a");
        claude_default_identity(foreign.path(), "account-a", "org-a");
        claude_credential(
            &foreign.path().join(".claude"),
            "FOREIGN",
            NOW_MS + 3_600_000,
        );
        let _child = NativeChild::spawn(
            sandbox.path(),
            "bin-foreign",
            "claude",
            foreign.path(),
            &[],
            &[],
        );

        assert!(super::resolve(&paths, &selected, "claude-code", NOW_MS).is_none());
    }
}
