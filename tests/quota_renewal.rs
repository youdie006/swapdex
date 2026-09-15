//! Quota reads must never renew or rewrite a saved credential.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

struct ReapedChild(Option<Child>);

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    root: tempfile::TempDir,
    slot: PathBuf,
    curl: PathBuf,
}

impl Fixture {
    fn new(with_saved_profile: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join(".local/share/swapdex");
        let slot = store.join("slots/current");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(
            slot.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": "current-user",
                    "organizationUuid": "current-org",
                    "emailAddress": "current@example.com"
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            slot.join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "OLD-ACCESS",
                    "refreshToken": "OLD-REFRESH",
                    "expiresAt": 1,
                    "refreshTokenExpiresAt": 9_000_000_000_000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            store.join("slots.json"),
            serde_json::json!([{
                "name": "work",
                "id": "current",
                "tool": "claude-code",
                "adopted": false,
                "config_dir": slot
            }])
            .to_string(),
        )
        .unwrap();
        std::fs::write(store.join("active-claude"), slot.to_str().unwrap()).unwrap();

        if with_saved_profile {
            let saved = store.join("accounts/work/claude-code");
            std::fs::create_dir_all(&saved).unwrap();
            std::fs::write(
                saved.join("oauth_account"),
                serde_json::json!({
                    "accountUuid": "old-snapshot-user",
                    "emailAddress": "old-snapshot@example.com"
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                saved.join("credentials"),
                serde_json::json!({
                    "claudeAiOauth": {
                        "accessToken": "OLD-SNAPSHOT-ACCESS",
                        "expiresAt": 9_000_000_000_000_i64
                    }
                })
                .to_string(),
            )
            .unwrap();
        }

        let curl = root.path().join("fake-curl");
        std::fs::write(
            &curl,
            r#"#!/bin/sh
cfg=$(cat)
case "$cfg" in
  *oauth.test*)
    printf x >> "$HOME/oauth-calls"
    case "$cfg" in *OLD-REFRESH*) ;; *) exit 92 ;; esac
    printf '{"access_token":"NEW-ACCESS","refresh_token":"NEW-REFRESH","expires_in":3600}\n200'
    ;;
  *OLD-ACCESS*)
    printf x >> "$HOME/old-usage-calls"
    printf '{"error":"old access rejected"}\n401'
    ;;
  *VALID-ACCESS*)
    printf x >> "$HOME/valid-usage-calls"
    printf '{"five_hour":{"utilization":12}}\n200'
    ;;
  *OLD-SNAPSHOT-ACCESS*)
    printf x >> "$HOME/snapshot-usage-calls"
    printf '{"five_hour":{"utilization":99}}\n200'
    ;;
  *NEW-ACCESS*)
    printf x >> "$HOME/new-usage-calls"
    printf '{"five_hour":{"utilization":12}}\n200'
    ;;
  *NATIVE-ACCESS*)
    printf x >> "$HOME/native-usage-calls"
    printf '{"five_hour":{"utilization":23}}\n200'
    ;;
  *LIVE-EXPIRED-ACCESS*)
    printf x >> "$HOME/live-expired-usage-calls"
    printf '{"five_hour":{"utilization":77}}\n200'
    ;;
  *SAVED-ACCESS*)
    printf x >> "$HOME/saved-usage-calls"
    printf '{"five_hour":{"utilization":99}}\n200'
    ;;
  *) exit 91 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();

        Self { root, slot, curl }
    }

    fn quota_command_with_json(&self, json: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_swapdex"));
        command
            .arg("quota")
            .env("SWAPDEX_ROOT", self.root.path())
            .env("HOME", self.root.path())
            .env("SWAPDEX_CURL", &self.curl)
            .env("SWAPDEX_OAUTH_URL", "https://oauth.test/token")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .env_remove("SWAPDEX_TEST_REFRESH_COUNT")
            .env_remove("SWAPDEX_TEST_REFRESH_STARTED")
            .env_remove("SWAPDEX_TEST_REFRESH_RELEASE");
        if json {
            command.arg("--json");
        }
        command
    }

    fn quota_command(&self) -> Command {
        self.quota_command_with_json(true)
    }

    fn quota(&self) -> serde_json::Value {
        let out = self.quota_command().output().unwrap();
        self.output(out)
    }

    fn human_quota(&self) -> Output {
        self.quota_command_with_json(false).output().unwrap()
    }

    fn credential_bytes(&self) -> Vec<u8> {
        std::fs::read(self.slot.join(".credentials.json")).unwrap()
    }

    fn write_credential(&self, access_token: &str, expires_at: i64) {
        std::fs::write(
            self.slot.join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": access_token,
                    "refreshToken": "OLD-REFRESH",
                    "expiresAt": expires_at,
                    "refreshTokenExpiresAt": 9_000_000_000_000_i64
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn output(&self, out: Output) -> serde_json::Value {
        assert!(
            out.status.success(),
            "quota exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
            panic!(
                "quota returned invalid JSON ({error}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    fn calls(&self, name: &str) -> usize {
        std::fs::read(self.root.path().join(name))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    fn row<'a>(&self, value: &'a serde_json::Value) -> &'a serde_json::Value {
        value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "work")
            .expect("work quota row")
    }

    fn assert_expired_read_was_read_only(
        &self,
        value: &serde_json::Value,
        credential_before: &[u8],
    ) {
        let row = self.row(value);
        assert_eq!(row["status"], "offline", "{row}");
        assert_eq!(row["email"], "current@example.com", "{row}");
        assert!(
            row["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("expired")),
            "expired state was hidden: {row}"
        );
        assert_eq!(self.calls("oauth-calls"), 0, "quota invoked OAuth");
        assert_eq!(self.calls("new-usage-calls"), 0, "renewed token was used");
        assert_eq!(self.calls("old-usage-calls"), 0, "expired token was used");
        assert_eq!(
            self.calls("snapshot-usage-calls"),
            0,
            "saved profile substituted for its backing slot"
        );
        assert_eq!(
            std::fs::read(self.slot.join(".credentials.json")).unwrap(),
            credential_before,
            "quota rewrote the saved credential"
        );
    }
}

#[test]
fn slot_only_quota_json_reports_expired_without_oauth_or_a_credential_write() {
    let fixture = Fixture::new(false);
    let credential_before = fixture.credential_bytes();

    let value = fixture.quota();

    fixture.assert_expired_read_was_read_only(&value, &credential_before);
}

#[test]
fn slot_only_human_quota_reports_expired_without_oauth_or_a_credential_write() {
    let fixture = Fixture::new(false);
    let credential_before = fixture.credential_bytes();

    let out = fixture.human_quota();
    assert!(
        out.status.success(),
        "quota exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("work (active)"),
        "account missing: {stdout}"
    );
    assert!(stdout.contains("expired"), "expired state hidden: {stdout}");
    assert_eq!(fixture.calls("oauth-calls"), 0, "quota invoked OAuth");
    assert_eq!(
        fixture.calls("old-usage-calls"),
        0,
        "expired token was used"
    );
    assert_eq!(fixture.credential_bytes(), credential_before);
}

#[test]
#[cfg(target_os = "linux")]
fn an_expired_slot_owned_by_native_claude_is_still_only_reported() {
    let fixture = Fixture::new(false);
    let credential_before = fixture.credential_bytes();
    let claude = fixture.root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &claude).unwrap();
    let mut held = ReapedChild(Some(
        Command::new(&claude)
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", &fixture.slot)
            .spawn()
            .unwrap(),
    ));
    wait_for(Path::new(&format!(
        "/proc/{}/environ",
        held.0.as_ref().unwrap().id()
    )));

    let value = fixture.quota();
    let row = fixture.row(&value);

    assert_eq!(row["status"], "offline", "{row}");
    assert!(
        row["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("expired")),
        "expired state was hidden: {row}"
    );
    assert_eq!(fixture.calls("oauth-calls"), 0, "quota invoked OAuth");
    assert_eq!(
        fixture.calls("old-usage-calls"),
        0,
        "expired token was used"
    );
    assert_eq!(fixture.credential_bytes(), credential_before);
    held.0.as_mut().unwrap().kill().unwrap();
    held.0.as_mut().unwrap().wait().unwrap();
    held.0 = None;
}

#[test]
#[cfg(target_os = "linux")]
fn a_current_native_owner_supplies_only_the_same_accounts_credential() {
    let fixture = Fixture::new(false);
    let credential_before = fixture.credential_bytes();
    let native = fixture.root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::write(
        fixture.root.path().join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "current-user",
                "organizationUuid": "current-org",
                "emailAddress": "current@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        native.join(".credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "NATIVE-ACCESS",
                "refreshToken": "NATIVE-REFRESH",
                "expiresAt": 9_000_000_000_000_i64
            }
        })
        .to_string(),
    )
    .unwrap();
    let claude = fixture.root.path().join("native-bin/claude");
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", &claude).unwrap();
    let mut held = ReapedChild(Some(
        Command::new(&claude)
            .arg("30")
            .env_clear()
            .env("HOME", fixture.root.path())
            .spawn()
            .unwrap(),
    ));
    let pid = held.0.as_ref().unwrap().id();
    wait_for(Path::new(&format!("/proc/{pid}/environ")));

    let value = fixture.quota();
    let row = fixture.row(&value);

    assert_eq!(row["status"], "ok", "{row}");
    assert_eq!(row["email"], "current@example.com", "{row}");
    assert_eq!(row["five_hour"]["used_pct"], 23.0, "{row}");
    assert_eq!(fixture.calls("native-usage-calls"), 1);
    assert_eq!(fixture.calls("oauth-calls"), 0, "native token was rotated");
    assert_eq!(
        fixture.calls("old-usage-calls"),
        0,
        "slot token substituted"
    );
    assert_eq!(fixture.credential_bytes(), credential_before);
    held.0.as_mut().unwrap().kill().unwrap();
    held.0.as_mut().unwrap().wait().unwrap();
    held.0 = None;
}

#[test]
fn a_valid_slot_token_is_read_without_oauth_or_a_credential_write() {
    let fixture = Fixture::new(false);
    fixture.write_credential("VALID-ACCESS", 9_000_000_000_000_i64);
    let credential_before = fixture.credential_bytes();

    let value = fixture.quota();
    let row = fixture.row(&value);

    assert_eq!(row["status"], "ok", "{row}");
    assert_eq!(row["email"], "current@example.com", "{row}");
    assert_eq!(row["five_hour"]["used_pct"], 12.0, "{row}");
    assert_eq!(fixture.calls("oauth-calls"), 0, "quota invoked OAuth");
    assert_eq!(fixture.calls("valid-usage-calls"), 1, "usage was not read");
    assert_eq!(fixture.credential_bytes(), credential_before);
}

#[test]
fn saved_profile_quota_reads_its_expired_authoritative_slot_without_oauth_or_a_write() {
    let fixture = Fixture::new(true);
    let credential_before = fixture.credential_bytes();
    let snapshot = fixture
        .root
        .path()
        .join(".local/share/swapdex/accounts/work/claude-code/credentials");
    let snapshot_before = std::fs::read(&snapshot).unwrap();

    let value = fixture.quota();

    fixture.assert_expired_read_was_read_only(&value, &credential_before);
    assert_eq!(
        std::fs::read(snapshot).unwrap(),
        snapshot_before,
        "quota rewrote the saved profile credential"
    );
}

#[test]
fn matching_unslotted_profile_does_not_send_an_expired_live_credential() {
    let fixture = Fixture::new(true);
    let store = fixture.root.path().join(".local/share/swapdex");
    std::fs::remove_file(store.join("slots.json")).unwrap();
    std::fs::remove_file(store.join("active-claude")).unwrap();

    let profile = store.join("accounts/work/claude-code");
    std::fs::write(
        profile.join("oauth_account"),
        serde_json::json!({
            "accountUuid": "current-user",
            "emailAddress": "current@example.com"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        profile.join("credentials"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "SAVED-ACCESS",
                "expiresAt": 9_000_000_000_000_i64
            }
        })
        .to_string(),
    )
    .unwrap();

    let native = fixture.root.path().join(".claude");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::write(
        fixture.root.path().join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "current-user",
                "organizationUuid": "current-org",
                "emailAddress": "current@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();
    let live_credential = native.join(".credentials.json");
    std::fs::write(
        &live_credential,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "LIVE-EXPIRED-ACCESS",
                "refreshToken": "LIVE-EXPIRED-REFRESH",
                "expiresAt": 1,
                "refreshTokenExpiresAt": 9_000_000_000_000_i64
            }
        })
        .to_string(),
    )
    .unwrap();
    let live_before = std::fs::read(&live_credential).unwrap();
    let saved_before = std::fs::read(profile.join("credentials")).unwrap();

    let value = fixture.quota();
    let row = fixture.row(&value);

    assert_eq!(row["status"], "offline", "{row}");
    assert!(
        row["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("expired")),
        "expired live credential was hidden: {row}"
    );
    assert_eq!(fixture.calls("oauth-calls"), 0, "quota invoked OAuth");
    assert_eq!(
        fixture.calls("live-expired-usage-calls"),
        0,
        "expired live token reached the usage endpoint"
    );
    assert_eq!(
        fixture.calls("saved-usage-calls"),
        0,
        "saved snapshot substituted for the matching live login"
    );
    assert_eq!(std::fs::read(live_credential).unwrap(), live_before);
    assert_eq!(
        std::fs::read(profile.join("credentials")).unwrap(),
        saved_before
    );
}
